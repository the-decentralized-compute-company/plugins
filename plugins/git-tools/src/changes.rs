//! Diffs: the shared renderer, and the `diff` tool built on it.
//!
//! A diff is the one thing here that can be arbitrarily large — "what changed
//! between these two releases" is a completely reasonable question whose honest
//! answer is sometimes forty megabytes. Four separate bounds apply, and a
//! response says which one it hit:
//!
//! - **Per-file hunk work**, through `DiffOptions::max_size`: a blob above the
//!   threshold is treated as binary and never hunked at all.
//! - **Rename detection**, at the operator's `--max-rename-candidates`. This is
//!   the expensive one, and the reason it has its own limit is measured rather
//!   than assumed: on a 3065-file release range in the TDCC repository,
//!   `diff_tree_to_tree` took 19 ms and `find_similar` took **12 seconds**.
//!   Inexact rename detection compares every removed file against every added
//!   one. Above the limit it is skipped, exactly as `git`'s own
//!   `diff.renameLimit` does, and the response reports
//!   `renames: "skipped_too_many_files"` rather than silently presenting a
//!   rename as a delete plus an add with no explanation.
//! - **The file list**, at [`crate::settings::MAX_FILE_ENTRIES`].
//! - **The patch text**, at the operator's `--max-patch-bytes`.
//!
//! Totals are counted across the whole diff even when the file list was capped,
//! so `files_changed` is never quietly the same number as `files.len()` when
//! they differ.

use git2::{Commit, Diff, DiffDelta, DiffFormat, DiffOptions, Repository};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tdcc_plugin::{PluginError, PluginResult};

use crate::guard::{TreePath, parse_tree_paths};
use crate::render::{Budget, delta_status, short_oid};
use crate::repos::Registry;
use crate::resolve::{commit_tree, required_revision, resolve_commit, revision_or_head};
use crate::settings::{MAX_FILE_ENTRIES, MAX_PATHSPECS};

/// Blobs larger than this are reported as binary rather than hunked.
///
/// libgit2 defaults to 512 MiB, which is a bound in name only on a machine
/// somebody else is paying to run. Eight is generous for source and small
/// enough that one committed archive cannot turn a diff into a minutes-long
/// read.
pub const MAX_DIFFED_BLOB_BYTES: i64 = 8 * 1024 * 1024;

/// Unified-diff context lines a caller may ask for.
pub const MAX_CONTEXT_LINES: u32 = 25;
pub const DEFAULT_CONTEXT_LINES: u32 = 3;

/// One changed file.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FileChange {
    /// One of: added, deleted, modified, renamed, copied, typechange,
    /// conflicted, unmodified, ignored, untracked, unreadable.
    pub status: &'static str,
    /// Path after the change. Absent only for a deletion.
    pub path: Option<String>,
    /// Path before the change, present only for a rename or a copy.
    pub old_path: Option<String>,
    pub additions: usize,
    pub deletions: usize,
    /// True when the content was not diffed — genuinely binary, or larger than
    /// the blob threshold. `additions` and `deletions` are zero in that case.
    pub binary: bool,
}

/// What happened to rename and copy detection on one diff.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RenameDetection {
    /// Ran. A moved file appears once, as `renamed`.
    Detected,
    /// The caller passed `detect_renames: false`.
    Disabled,
    /// The diff touched more files than `--max-rename-candidates` allows, so
    /// detection was skipped to bound the cost. A moved file appears twice:
    /// once as `deleted` and once as `added`.
    SkippedTooManyFiles,
}

/// The rendered result of one diff.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChangeSet {
    /// Files changed across the whole diff, even when `files` was capped.
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub files: Vec<FileChange>,
    /// True when `files` holds fewer entries than `files_changed`.
    pub files_truncated: bool,
    /// Whether a moved file was recognised as one move or as two changes.
    pub renames: RenameDetection,
    /// Unified diff text, present only when the caller asked for it.
    pub patch: Option<String>,
    /// True when any cap shortened this result.
    pub truncated: bool,
    /// Which cap stopped the work first.
    pub truncated_reason: Option<String>,
}

/// Run rename and copy detection, unless the diff is too wide to afford it.
///
/// `find_similar` is quadratic in the number of candidate files, so the gate is
/// the delta count *before* detection runs. `rename_limit` is set as well, so
/// libgit2 has the same ceiling internally even if this gate is ever widened.
pub fn detect_renames(
    diff: &mut Diff<'_>,
    enabled: bool,
    candidate_limit: usize,
) -> PluginResult<RenameDetection> {
    if !enabled {
        return Ok(RenameDetection::Disabled);
    }
    if diff.deltas().len() > candidate_limit {
        return Ok(RenameDetection::SkippedTooManyFiles);
    }
    let mut options = git2::DiffFindOptions::new();
    options
        .renames(true)
        .copies(true)
        .rename_limit(candidate_limit);
    diff.find_similar(Some(&mut options)).map_err(|error| {
        PluginError::internal(format!(
            "rename detection failed: {}. Retry with detect_renames=false",
            error.message()
        ))
    })?;
    Ok(RenameDetection::Detected)
}

fn path_of(delta_path: Option<&std::path::Path>) -> Option<String> {
    delta_path.map(|path| path.to_string_lossy().replace('\\', "/"))
}

fn file_change(delta: &DiffDelta<'_>) -> FileChange {
    let binary = delta.old_file().is_binary() || delta.new_file().is_binary();
    let status = delta.status();
    let old_path = path_of(delta.old_file().path());
    let new_path = path_of(delta.new_file().path());

    FileChange {
        status: delta_status(status),
        path: match status {
            git2::Delta::Deleted => None,
            _ => new_path.clone().or_else(|| old_path.clone()),
        },
        old_path: match status {
            git2::Delta::Renamed | git2::Delta::Copied => old_path,
            git2::Delta::Deleted => new_path.or(old_path),
            _ => None,
        },
        additions: 0,
        deletions: 0,
        binary,
    }
}

/// Build the diff options every diff in this plugin uses.
///
/// Pathspecs are passed as `&str`, which libgit2 matches with fnmatch
/// semantics. They have already been through [`crate::guard::parse_tree_path`],
/// so none of them is absolute, contains `..`, or begins with a magic prefix.
pub fn diff_options(paths: &[TreePath], context_lines: u32) -> DiffOptions {
    let mut options = DiffOptions::new();
    options
        .context_lines(context_lines)
        .max_size(MAX_DIFFED_BLOB_BYTES)
        // Submodule contents live in a repository this plugin was not
        // configured to read, so a submodule shows up as one changed entry
        // rather than as a diff of somebody else's history.
        .ignore_submodules(true);
    for path in paths {
        options.pathspec(path.as_str());
    }
    options
}

#[derive(Debug, Clone, Copy)]
pub struct DiffBudget {
    pub max_patch_bytes: usize,
    pub max_files: usize,
}

/// Count every change, and optionally render the patch text, under a budget.
pub fn render_changes(
    diff: &Diff<'_>,
    want_patch: bool,
    budget: DiffBudget,
    renames: RenameDetection,
) -> PluginResult<ChangeSet> {
    // The two libgit2 callbacks are held mutably at the same time, so the
    // accumulator lives behind a `RefCell` rather than being captured by both.
    // Nothing here is shared across threads: `foreach` is synchronous and both
    // borrows are released before it returns.
    #[derive(Default)]
    struct Walk {
        files: Vec<FileChange>,
        files_changed: usize,
        insertions: usize,
        deletions: usize,
        current: Option<FileChange>,
    }

    let walk = std::cell::RefCell::new(Walk::default());
    let mut truncated = false;
    let mut truncated_reason: Option<String> = None;

    {
        // One pass over the whole diff for exact counts. `files` is capped
        // separately below so a diff touching 50 000 files still reports 50 000
        // in `files_changed` rather than the length of a clipped list.
        let mut on_file = |delta: DiffDelta<'_>, _progress: f32| -> bool {
            let mut state = walk.borrow_mut();
            if let Some(finished) = state.current.take()
                && state.files.len() < budget.max_files
            {
                state.files.push(finished);
            }
            state.files_changed += 1;
            state.current = Some(file_change(&delta));
            true
        };
        let mut on_line = |_delta: DiffDelta<'_>,
                           _hunk: Option<git2::DiffHunk<'_>>,
                           line: git2::DiffLine<'_>|
         -> bool {
            let mut state = walk.borrow_mut();
            let origin = line.origin();
            if let Some(entry) = state.current.as_mut() {
                match origin {
                    '+' => entry.additions += 1,
                    '-' => entry.deletions += 1,
                    _ => return true,
                }
            } else {
                return true;
            }
            match origin {
                '+' => state.insertions += 1,
                '-' => state.deletions += 1,
                _ => {}
            }
            true
        };

        diff.foreach(&mut on_file, None, None, Some(&mut on_line))
            .map_err(|error| {
                PluginError::internal(format!("diff could not be walked: {}", error.message()))
            })?;
    }

    let Walk {
        mut files,
        files_changed,
        insertions,
        deletions,
        current,
    } = walk.into_inner();
    if let Some(finished) = current
        && files.len() < budget.max_files
    {
        files.push(finished);
    }
    let files_truncated = files.len() < files_changed;
    if files_truncated {
        truncated = true;
        truncated_reason = Some(format!(
            "the file list stops at {} entries; files_changed is the real total",
            budget.max_files
        ));
    }

    let patch = if want_patch {
        let mut text = String::new();
        let mut patch_budget = Budget::new(budget.max_patch_bytes);
        let reason = format!(
            "the patch stops at {} bytes; narrow it with the paths argument or ask for one commit \
             at a time",
            budget.max_patch_bytes
        );
        let mut stopped = false;

        let result = diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
            if stopped {
                return false;
            }
            // Content for +/-/context lines excludes the origin marker, which
            // is exactly the character that makes a unified diff readable.
            if matches!(line.origin(), '+' | '-' | ' ') {
                let mut marker = [0u8; 4];
                let marker = line.origin().encode_utf8(&mut marker);
                if !patch_budget.push_str(&mut text, marker, &reason) {
                    stopped = true;
                    return false;
                }
            }
            let chunk = String::from_utf8_lossy(line.content());
            if !patch_budget.push_str(&mut text, &chunk, &reason) {
                stopped = true;
                return false;
            }
            true
        });

        // A callback that returns false aborts `git_diff_print` with an error.
        // That is this code deciding to stop, not a failure to read the diff,
        // so it is only an error when nothing asked for it.
        if let Err(error) = result
            && !stopped
        {
            return Err(PluginError::internal(format!(
                "diff could not be rendered: {}",
                error.message()
            )));
        }

        if patch_budget.truncated() {
            truncated = true;
            if truncated_reason.is_none() {
                truncated_reason = patch_budget.reason().map(str::to_string);
            }
        }
        Some(text)
    } else {
        None
    };

    Ok(ChangeSet {
        files_changed,
        insertions,
        deletions,
        files,
        files_truncated,
        renames,
        patch,
        truncated,
        truncated_reason,
    })
}

/// A revision as a response reports it: what the caller asked for, and what it
/// actually resolved to.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RevisionRef {
    pub requested: String,
    pub commit: String,
    pub short: String,
}

impl RevisionRef {
    pub fn new(requested: &str, commit: &Commit<'_>) -> Self {
        Self {
            requested: requested.to_string(),
            commit: commit.id().to_string(),
            short: short_oid(commit.id()),
        }
    }
}

/// Arguments for the `diff` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiffArgs {
    /// Which configured repository to read. Required only when the operator
    /// configured more than one; call `status` to list them.
    #[serde(default)]
    pub repo: Option<String>,
    /// The older side: a commit, branch, or tag. For "what changed in this
    /// release", this is the previous release tag.
    pub from_rev: String,
    /// The newer side: a commit, branch, or tag. Defaults to HEAD.
    #[serde(default)]
    pub to_rev: Option<String>,
    /// Limit the comparison to these paths, relative to the repository root.
    /// `*`, `?` and `[…]` match the way a shell glob does.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// Return the unified diff text as well as the file list. Off by default,
    /// because the file list plus line counts answers most questions and costs
    /// a fraction of the tokens.
    #[serde(default)]
    pub patch: Option<bool>,
    /// Context lines around each change in the patch text. 0–25, default 3.
    #[serde(default)]
    pub context_lines: Option<u32>,
    /// Compare `to_rev` against the merge base of the two revisions instead of
    /// against `from_rev` directly — the difference between `git diff a..b` and
    /// `git diff a...b`. Use this to see only what the newer branch added,
    /// excluding what the older one gained meanwhile. Off by default.
    #[serde(default)]
    pub use_merge_base: Option<bool>,
    /// Detect renames and copies, so a moved file reports as one rename rather
    /// than as a deletion plus an addition. On by default.
    #[serde(default)]
    pub detect_renames: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffResponse {
    pub repository: String,
    pub from: RevisionRef,
    pub to: RevisionRef,
    /// The commit actually used as the older side. Differs from `from.commit`
    /// only when `use_merge_base` was set.
    pub base: String,
    pub use_merge_base: bool,
    #[serde(flatten)]
    pub changes: ChangeSet,
}

pub fn diff(registry: &Registry, args: DiffArgs) -> PluginResult<DiffResponse> {
    let selected = registry.select(args.repo.as_deref())?;
    let limits = registry.limits();
    let disclosure = registry.disclosure();

    let want_patch = args.patch.unwrap_or(false);
    if want_patch && !disclosure.content {
        return Err(PluginError::invalid_request(
            "this node runs git-tools with --no-content, so patch text is not available. The file \
             list and line counts are still returned when patch is omitted",
        ));
    }

    let context_lines = args.context_lines.unwrap_or(DEFAULT_CONTEXT_LINES);
    if context_lines > MAX_CONTEXT_LINES {
        return Err(PluginError::invalid_params(format!(
            "context_lines must be between 0 and {MAX_CONTEXT_LINES}, got {context_lines}"
        )));
    }

    let paths = parse_tree_paths(args.paths.as_deref().unwrap_or(&[]), MAX_PATHSPECS)
        .map_err(|error| PluginError::invalid_params(error.to_string()))?;

    let from_revision = required_revision(&args.from_rev, "from_rev")?;
    let to_revision = revision_or_head(args.to_rev.as_deref())?;

    let repository = registry.open(selected)?;
    let from_commit = resolve_commit(&repository, &from_revision)?;
    let to_commit = resolve_commit(&repository, &to_revision)?;

    let use_merge_base = args.use_merge_base.unwrap_or(false);
    let base_commit = if use_merge_base {
        let base = repository
            .merge_base(from_commit.id(), to_commit.id())
            .map_err(|_| {
                PluginError::invalid_params(format!(
                    "{} and {} have no common ancestor, so there is no merge base to compare \
                     against",
                    from_revision, to_revision
                ))
            })?;
        repository.find_commit(base).map_err(|error| {
            PluginError::internal(format!("merge base could not be read: {error}"))
        })?
    } else {
        from_commit.clone()
    };

    let from_tree = commit_tree(&base_commit)?;
    let to_tree = commit_tree(&to_commit)?;
    let mut options = diff_options(&paths, context_lines);
    let mut diff = repository
        .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), Some(&mut options))
        .map_err(|error| {
            PluginError::internal(format!("diff could not be computed: {}", error.message()))
        })?;

    let renames = detect_renames(
        &mut diff,
        args.detect_renames.unwrap_or(true),
        limits.rename_candidate_limit,
    )?;

    let changes = render_changes(
        &diff,
        want_patch,
        DiffBudget {
            max_patch_bytes: limits.max_patch_bytes,
            max_files: MAX_FILE_ENTRIES,
        },
        renames,
    )?;

    Ok(DiffResponse {
        repository: selected.alias.clone(),
        from: RevisionRef::new(from_revision.as_str(), &from_commit),
        to: RevisionRef::new(to_revision.as_str(), &to_commit),
        base: base_commit.id().to_string(),
        use_merge_base,
        changes,
    })
}

/// Diff one commit against its first parent, as `show` presents it.
///
/// A root commit is diffed against the empty tree, which is what makes the
/// first commit in a repository show as a list of additions rather than as
/// nothing at all.
pub fn diff_commit_against_parent<'repo>(
    repository: &'repo Repository,
    commit: &Commit<'repo>,
    paths: &[TreePath],
    context_lines: u32,
    renames_enabled: bool,
    rename_candidate_limit: usize,
) -> PluginResult<(Diff<'repo>, RenameDetection)> {
    let parent_tree = match commit.parent_count() {
        0 => None,
        _ => {
            let parent = commit.parent(0).map_err(|error| {
                PluginError::internal(format!("parent commit could not be read: {error}"))
            })?;
            Some(commit_tree(&parent)?)
        }
    };
    let tree = commit_tree(commit)?;
    let mut options = diff_options(paths, context_lines);
    let mut diff = repository
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut options))
        .map_err(|error| {
            PluginError::internal(format!("diff could not be computed: {}", error.message()))
        })?;
    let renames = detect_renames(&mut diff, renames_enabled, rename_candidate_limit)?;
    Ok((diff, renames))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repos::Registry;
    use crate::settings::{Disclosure, Limits, RepoSpec};
    use crate::testsupport::{RepoFixture, TempTree};

    fn registry_for(fixture: &RepoFixture) -> Registry {
        registry_with(fixture, Limits::default(), Disclosure::default())
    }

    fn registry_with(fixture: &RepoFixture, limits: Limits, disclosure: Disclosure) -> Registry {
        let registry = Registry::resolve(
            &[RepoSpec {
                alias: "repo".to_string(),
                path: fixture.root().to_path_buf(),
            }],
            limits,
            disclosure,
        );
        assert!(registry.problems().is_empty(), "{:?}", registry.problems());
        registry
    }

    fn two_release_history(fixture: &RepoFixture) {
        fixture.write("src/main.rs", "fn main() {}\n");
        fixture.write("README.md", "old\n");
        fixture.commit("first");
        fixture.tag_light("v1.0.0");

        fixture.write("src/main.rs", "fn main() {\n    run();\n}\n");
        fixture.write("src/run.rs", "pub fn run() {}\n");
        fixture.remove("README.md");
        fixture.commit("second");
        fixture.tag_light("v2.0.0");
    }

    fn base_args(from: &str) -> DiffArgs {
        DiffArgs {
            repo: None,
            from_rev: from.to_string(),
            to_rev: None,
            paths: None,
            patch: None,
            context_lines: None,
            use_merge_base: None,
            detect_renames: None,
        }
    }

    #[test]
    fn a_release_range_reports_every_kind_of_change() {
        let tree = TempTree::new("diff-range");
        let fixture = tree.repository("repo");
        two_release_history(&fixture);
        let registry = registry_for(&fixture);

        let response = diff(&registry, base_args("v1.0.0")).expect("diffs");

        assert_eq!(response.from.requested, "v1.0.0");
        assert_eq!(response.to.requested, "HEAD");
        assert_eq!(response.changes.files_changed, 3);
        assert_eq!(response.changes.insertions, 4);
        assert_eq!(response.changes.deletions, 2);
        assert!(!response.changes.truncated);
        assert!(response.changes.patch.is_none(), "patch is opt-in");

        let mut described: Vec<(&str, String)> = response
            .changes
            .files
            .iter()
            .map(|file| {
                (
                    file.status,
                    file.path.clone().or_else(|| file.old_path.clone()).unwrap(),
                )
            })
            .collect();
        described.sort();
        assert_eq!(
            described,
            vec![
                ("added", "src/run.rs".to_string()),
                ("deleted", "README.md".to_string()),
                ("modified", "src/main.rs".to_string()),
            ]
        );
    }

    #[test]
    fn patch_text_is_returned_only_when_asked_for() {
        let tree = TempTree::new("diff-patch");
        let fixture = tree.repository("repo");
        two_release_history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = base_args("v1.0.0");
        args.patch = Some(true);
        args.paths = Some(vec!["src/main.rs".to_string()]);
        let response = diff(&registry, args).expect("diffs");

        let patch = response.changes.patch.expect("asked for it");
        assert!(patch.contains("+    run();"), "{patch}");
        assert!(patch.contains("-fn main() {}"), "{patch}");
        assert!(
            !patch.contains("run.rs"),
            "the pathspec should have scoped it"
        );
        assert_eq!(response.changes.files_changed, 1);
    }

    #[test]
    fn no_content_refuses_a_patch_rather_than_returning_an_empty_one() {
        let tree = TempTree::new("diff-no-content");
        let fixture = tree.repository("repo");
        two_release_history(&fixture);
        let registry = registry_with(
            &fixture,
            Limits::default(),
            Disclosure {
                content: false,
                redact_emails: false,
            },
        );

        let mut args = base_args("v1.0.0");
        args.patch = Some(true);
        let error = diff(&registry, args).expect_err("content is off");
        let message = format!("{error:?}");
        assert!(message.contains("--no-content"), "{message}");

        // The metadata answer still works, which is the point of the flag.
        let response = diff(&registry, base_args("v1.0.0")).expect("metadata is still allowed");
        assert_eq!(response.changes.files_changed, 3);
    }

    #[test]
    fn a_patch_over_the_byte_cap_is_cut_and_says_so() {
        let tree = TempTree::new("diff-patch-cap");
        let fixture = tree.repository("repo");
        fixture.write("big.txt", "");
        fixture.commit("empty");
        fixture.tag_light("before");
        let body: String = (0..2_000).map(|index| format!("line {index}\n")).collect();
        fixture.write("big.txt", &body);
        fixture.commit("filled");

        let registry = registry_with(
            &fixture,
            Limits {
                max_patch_bytes: 2_048,
                ..Limits::default()
            },
            Disclosure::default(),
        );

        let mut args = base_args("before");
        args.patch = Some(true);
        let response = diff(&registry, args).expect("diffs");

        let patch = response.changes.patch.expect("asked for it");
        assert!(patch.len() <= 2_048, "patch was {} bytes", patch.len());
        assert!(response.changes.truncated);
        assert!(
            response
                .changes
                .truncated_reason
                .as_deref()
                .expect("a reason")
                .contains("2048"),
            "{:?}",
            response.changes.truncated_reason
        );
        // The counts are still exact: truncating the text does not truncate
        // the arithmetic.
        assert_eq!(response.changes.insertions, 2_000);
    }

    #[test]
    fn a_rename_is_one_entry_rather_than_a_deletion_and_an_addition() {
        let tree = TempTree::new("diff-rename");
        let fixture = tree.repository("repo");
        let body: String = (0..40).map(|index| format!("line {index}\n")).collect();
        fixture.write("old/name.rs", &body);
        fixture.commit("first");
        fixture.tag_light("before");
        fixture.remove("old/name.rs");
        fixture.write("new/name.rs", &body);
        fixture.commit("moved");

        let registry = registry_for(&fixture);
        let response = diff(&registry, base_args("before")).expect("diffs");

        assert_eq!(response.changes.files_changed, 1);
        let file = &response.changes.files[0];
        assert_eq!(file.status, "renamed");
        assert_eq!(file.path.as_deref(), Some("new/name.rs"));
        assert_eq!(file.old_path.as_deref(), Some("old/name.rs"));

        let mut args = base_args("before");
        args.detect_renames = Some(false);
        let plain = diff(&registry, args).expect("diffs");
        assert_eq!(plain.changes.files_changed, 2);
    }

    #[test]
    fn rename_detection_is_skipped_above_the_candidate_limit_and_says_so() {
        let tree = TempTree::new("diff-rename-limit");
        let fixture = tree.repository("repo");
        let body: String = (0..40)
            .map(|index| {
                format!(
                    "line {index}
"
                )
            })
            .collect();
        for index in 0..6 {
            fixture.write(&format!("old/file{index}.rs"), &body);
        }
        fixture.commit("first");
        fixture.tag_light("before");
        for index in 0..6 {
            fixture.remove(&format!("old/file{index}.rs"));
            fixture.write(&format!("new/file{index}.rs"), &body);
        }
        fixture.commit("moved");

        // Twelve deltas before detection, so a limit of four skips it.
        let registry = registry_with(
            &fixture,
            Limits {
                rename_candidate_limit: 4,
                ..Limits::default()
            },
            Disclosure::default(),
        );
        let skipped = diff(&registry, base_args("before")).expect("diffs");
        assert_eq!(
            skipped.changes.renames,
            RenameDetection::SkippedTooManyFiles
        );
        assert_eq!(
            skipped.changes.files_changed, 12,
            "each move shows as a delete and an add when detection is skipped"
        );

        // The same diff under a limit that fits reports six renames.
        let registry = registry_with(
            &fixture,
            Limits {
                rename_candidate_limit: 400,
                ..Limits::default()
            },
            Disclosure::default(),
        );
        let detected = diff(&registry, base_args("before")).expect("diffs");
        assert_eq!(detected.changes.renames, RenameDetection::Detected);
        assert_eq!(detected.changes.files_changed, 6);
        assert!(
            detected
                .changes
                .files
                .iter()
                .all(|file| file.status == "renamed")
        );
    }

    #[test]
    fn turning_rename_detection_off_is_reported_differently_from_skipping_it() {
        let tree = TempTree::new("diff-rename-disabled");
        let fixture = tree.repository("repo");
        two_release_history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = base_args("v1.0.0");
        args.detect_renames = Some(false);
        let response = diff(&registry, args).expect("diffs");
        assert_eq!(response.changes.renames, RenameDetection::Disabled);

        let response = diff(&registry, base_args("v1.0.0")).expect("diffs");
        assert_eq!(response.changes.renames, RenameDetection::Detected);
    }

    #[test]
    fn a_binary_file_is_flagged_rather_than_counted_as_lines() {
        let tree = TempTree::new("diff-binary");
        let fixture = tree.repository("repo");
        fixture.write("readme.txt", "text\n");
        fixture.commit("first");
        fixture.tag_light("before");
        std::fs::write(fixture.root().join("blob.bin"), [0u8, 1, 2, 0, 3, 4, 0]).expect("write");
        fixture.commit("added a binary");

        let registry = registry_for(&fixture);
        let response = diff(&registry, base_args("before")).expect("diffs");

        let binary = response
            .changes
            .files
            .iter()
            .find(|file| file.path.as_deref() == Some("blob.bin"))
            .expect("the binary file");
        assert!(binary.binary);
        assert_eq!(binary.additions, 0);
        assert_eq!(binary.deletions, 0);
    }

    #[test]
    fn merge_base_mode_excludes_what_the_other_side_gained() {
        let tree = TempTree::new("diff-merge-base");
        let fixture = tree.repository("repo");
        fixture.write("shared.txt", "base\n");
        fixture.commit("base");
        fixture.branch("topic");

        // main gains a file the topic branch never saw.
        fixture.write("only-on-main.txt", "main\n");
        fixture.commit("main moves on");
        fixture.branch("mainline");

        // Move onto the topic branch and add a file there.
        let repository = fixture.repository();
        repository
            .set_head("refs/heads/topic")
            .expect("checkout topic");
        repository
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .expect("materialise the topic tree");
        fixture.write("only-on-topic.txt", "topic\n");
        fixture.commit("topic moves on");

        let registry = registry_for(&fixture);

        let mut two_dot = base_args("mainline");
        two_dot.to_rev = Some("topic".to_string());
        let two_dot = diff(&registry, two_dot).expect("diffs");
        // Straight comparison: main's file looks deleted from topic's side.
        assert_eq!(two_dot.changes.files_changed, 2);
        assert!(!two_dot.use_merge_base);

        let mut three_dot = base_args("mainline");
        three_dot.to_rev = Some("topic".to_string());
        three_dot.use_merge_base = Some(true);
        let three_dot = diff(&registry, three_dot).expect("diffs");
        assert!(three_dot.use_merge_base);
        assert_eq!(three_dot.changes.files_changed, 1);
        assert_eq!(
            three_dot.changes.files[0].path.as_deref(),
            Some("only-on-topic.txt")
        );
        assert_ne!(three_dot.base, three_dot.from.commit);
    }

    #[test]
    fn an_out_of_range_context_setting_is_refused() {
        let tree = TempTree::new("diff-context");
        let fixture = tree.repository("repo");
        two_release_history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = base_args("v1.0.0");
        args.context_lines = Some(999);
        let error = diff(&registry, args).expect_err("out of range");
        assert!(format!("{error:?}").contains("context_lines"));
    }

    #[test]
    fn a_hostile_revision_never_reaches_the_repository() {
        let tree = TempTree::new("diff-hostile");
        let fixture = tree.repository("repo");
        two_release_history(&fixture);
        let registry = registry_for(&fixture);

        for hostile in ["--output=/tmp/x", "HEAD:../../etc/passwd", "HEAD^{/leak}"] {
            let error = diff(&registry, base_args(hostile)).expect_err("refused");
            let message = format!("{error:?}");
            assert!(message.contains("from_rev"), "{hostile}: {message}");
        }
    }

    #[test]
    fn a_traversal_path_filter_is_refused() {
        let tree = TempTree::new("diff-path-guard");
        let fixture = tree.repository("repo");
        two_release_history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = base_args("v1.0.0");
        args.paths = Some(vec!["../../etc/passwd".to_string()]);
        let error = diff(&registry, args).expect_err("refused");
        assert!(format!("{error:?}").contains("must not contain a '..' segment"));
    }

    #[test]
    fn more_pathspecs_than_the_limit_are_refused() {
        let tree = TempTree::new("diff-path-count");
        let fixture = tree.repository("repo");
        two_release_history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = base_args("v1.0.0");
        args.paths = Some((0..MAX_PATHSPECS + 1).map(|i| format!("f{i}.rs")).collect());
        let error = diff(&registry, args).expect_err("too many");
        assert!(format!("{error:?}").contains("at most"));
    }
}
