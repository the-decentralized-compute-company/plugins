//! `refs`, `repo_status`, and `status` — the three tools that describe a
//! repository rather than its history.
//!
//! ### Two tools called something like "status", on purpose
//!
//! The catalog convention is that `status` means *"what is this plugin
//! configured as, without touching anything expensive"*, and seven of the
//! eleven plugins in this repository have one. Git also has a `status`, and it
//! means something else entirely: the state of a working tree. Rather than
//! overload one name, this plugin ships both under distinct names:
//!
//! | Tool | Answers |
//! | --- | --- |
//! | `status` | Which repositories are configured, whether each one opens, what the limits are, and what this build can and cannot do |
//! | `repo_status` | What `git status` answers: staged, modified, and untracked files in one repository's working tree |
//!
//! ### `repo_status` never writes to the index
//!
//! libgit2 can refresh the index's stat cache while computing status, which is
//! a write to `.git/index`. That option is explicitly turned off here, because
//! "read-only" has to mean read-only on a machine somebody lent you. The
//! consequence is stated in the README: on a repository whose index is stale,
//! a file may be reported modified until some other tool refreshes it.

use git2::{BranchType, Repository, Status, StatusOptions, StatusShow};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tdcc_plugin::{PluginError, PluginResult};

use crate::guard::parse_tree_paths;
use crate::render::{format_timestamp, message_text, repository_state, short_oid, truncate_text};
use crate::repos::Registry;
use crate::settings::{
    MAX_PATHSPECS, MAX_REF_ENTRIES, MAX_REFS_SCANNED, MAX_STATUS_ENTRIES, PLUGIN_NAME,
    PLUGIN_VERSION,
};

pub const DEFAULT_REF_LIMIT: usize = 200;

/// Which refs `refs` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    /// Branches only.
    Branches,
    /// Tags only.
    Tags,
    /// Both, tags first. The default.
    All,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RefEntry {
    /// `branch` or `tag`.
    pub kind: &'static str,
    /// Short name: `main`, `origin/main`, `v1.4.0`.
    pub name: String,
    /// Full reference name, as `refs/heads/main`.
    pub full_name: String,
    /// True for a branch that lives on a remote rather than locally.
    pub remote: bool,
    /// True for the branch HEAD currently points at.
    pub head: bool,
    /// True for a tag object carrying its own message and tagger, as opposed
    /// to a lightweight tag that is just a name for a commit.
    pub annotated: bool,
    /// The commit this ref resolves to, after peeling any tag object.
    pub commit: String,
    pub short: String,
    /// Committer time of that commit, as an epoch second.
    pub timestamp: i64,
    /// ISO-8601 rendering of `timestamp` in UTC.
    pub date: Option<String>,
    /// First line of that commit's message.
    pub summary: Option<String>,
}

/// Arguments for the `refs` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RefsArgs {
    /// Which configured repository to read. Required only when the operator
    /// configured more than one; call `status` to list them.
    #[serde(default)]
    pub repo: Option<String>,
    /// Return branches, tags, or both. Defaults to both.
    #[serde(default)]
    pub kind: Option<RefKind>,
    /// Keep only refs whose name contains this text, matched
    /// case-insensitively. Plain substring, not a pattern.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Include branches that live on remotes, such as `origin/main`. On by
    /// default. Nothing here contacts a remote — these are the refs the last
    /// fetch left behind on disk.
    #[serde(default)]
    pub include_remote: Option<bool>,
    /// How many refs to return. Defaults to 200, capped at 1000. Results are
    /// ordered newest commit first, so a small limit still shows recent work.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefsResponse {
    pub repository: String,
    pub refs: Vec<RefEntry>,
    pub returned: usize,
    /// Refs examined before the scan cap stopped the search.
    pub refs_scanned: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<String>,
}

fn describe_ref(
    repository: &Repository,
    reference: &git2::Reference<'_>,
    kind: &'static str,
    remote: bool,
    head: bool,
) -> Option<RefEntry> {
    let full_name = String::from_utf8_lossy(reference.name_bytes()).into_owned();
    let name = String::from_utf8_lossy(reference.shorthand_bytes()).into_owned();
    let annotated = reference
        .target()
        .is_some_and(|oid| repository.find_tag(oid).is_ok());
    // Peels a tag object through to the commit, so an annotated and a
    // lightweight tag on the same commit report the same id.
    let commit = reference.peel_to_commit().ok()?;
    let when = commit.committer().when();

    Some(RefEntry {
        kind,
        name,
        full_name,
        remote,
        head,
        annotated,
        commit: commit.id().to_string(),
        short: short_oid(commit.id()),
        timestamp: when.seconds(),
        date: format_timestamp(when.seconds(), 0),
        summary: commit
            .summary_bytes()
            .map(message_text)
            .map(|text| truncate_text(&text, 1_024).0),
    })
}

pub fn refs(registry: &Registry, args: RefsArgs) -> PluginResult<RefsResponse> {
    let selected = registry.select(args.repo.as_deref())?;
    let kind = args.kind.unwrap_or(RefKind::All);
    let include_remote = args.include_remote.unwrap_or(true);
    let pattern = args.pattern.as_deref().map(str::to_lowercase);
    let limit = match args.limit {
        None => DEFAULT_REF_LIMIT,
        Some(value) => {
            let value = value as usize;
            if value == 0 || value > MAX_REF_ENTRIES {
                return Err(PluginError::invalid_params(format!(
                    "limit must be between 1 and {MAX_REF_ENTRIES}, got {value}"
                )));
            }
            value
        }
    };

    let repository = registry.open(selected)?;
    let mut entries: Vec<RefEntry> = Vec::new();
    let mut scanned = 0usize;
    let mut scan_capped = false;

    if matches!(kind, RefKind::Tags | RefKind::All) {
        let names = repository.tag_names(None).map_err(|error| {
            PluginError::internal(format!("tags could not be listed: {}", error.message()))
        })?;
        for name in names.iter().flatten() {
            if scanned >= MAX_REFS_SCANNED {
                scan_capped = true;
                break;
            }
            scanned += 1;
            let Ok(reference) = repository.find_reference(&format!("refs/tags/{name}")) else {
                continue;
            };
            if let Some(entry) = describe_ref(&repository, &reference, "tag", false, false) {
                entries.push(entry);
            }
        }
    }

    if matches!(kind, RefKind::Branches | RefKind::All) && !scan_capped {
        let branches = repository.branches(None).map_err(|error| {
            PluginError::internal(format!("branches could not be listed: {}", error.message()))
        })?;
        for branch in branches {
            if scanned >= MAX_REFS_SCANNED {
                scan_capped = true;
                break;
            }
            scanned += 1;
            let Ok((branch, branch_type)) = branch else {
                continue;
            };
            let remote = branch_type == BranchType::Remote;
            if remote && !include_remote {
                continue;
            }
            let head = branch.is_head();
            if let Some(entry) = describe_ref(&repository, branch.get(), "branch", remote, head) {
                entries.push(entry);
            }
        }
    }

    if let Some(pattern) = &pattern {
        entries.retain(|entry| entry.name.to_lowercase().contains(pattern));
    }

    // Newest first: the question behind this tool is almost always "what has
    // been released or worked on lately", and an alphabetical list buries it.
    entries.sort_by(|left, right| {
        right
            .timestamp
            .cmp(&left.timestamp)
            .then_with(|| left.name.cmp(&right.name))
    });

    let matched = entries.len();
    entries.truncate(limit);

    let truncated_reason = if scan_capped {
        Some(format!(
            "the search stopped after examining {MAX_REFS_SCANNED} refs, so refs that sort later \
             may be missing. Narrow it with kind or pattern"
        ))
    } else if matched > entries.len() {
        Some(format!(
            "{matched} refs matched and the newest {} are returned; raise limit or narrow the \
             pattern to see the rest",
            entries.len()
        ))
    } else {
        None
    };

    Ok(RefsResponse {
        repository: selected.alias.clone(),
        returned: entries.len(),
        refs: entries,
        refs_scanned: scanned,
        truncated: truncated_reason.is_some(),
        truncated_reason,
    })
}

/// Decode a libgit2 status bitset into stable lowercase names.
///
/// These strings appear in responses, so a caller may match on them.
pub fn status_flags(status: Status) -> Vec<&'static str> {
    let mut flags = Vec::new();
    for (bit, name) in [
        (Status::INDEX_NEW, "staged_new"),
        (Status::INDEX_MODIFIED, "staged_modified"),
        (Status::INDEX_DELETED, "staged_deleted"),
        (Status::INDEX_RENAMED, "staged_renamed"),
        (Status::INDEX_TYPECHANGE, "staged_typechange"),
        (Status::WT_NEW, "untracked"),
        (Status::WT_MODIFIED, "modified"),
        (Status::WT_DELETED, "deleted"),
        (Status::WT_TYPECHANGE, "typechange"),
        (Status::WT_RENAMED, "renamed"),
        (Status::WT_UNREADABLE, "unreadable"),
        (Status::IGNORED, "ignored"),
        (Status::CONFLICTED, "conflicted"),
    ] {
        if status.contains(bit) {
            flags.push(name);
        }
    }
    flags
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: String,
    /// Every state this path is in at once — a file can be staged and modified
    /// again in the working tree.
    pub flags: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HeadInfo {
    /// Full reference name, absent when HEAD is detached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// Short branch name, absent when HEAD is detached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub commit: String,
    pub short: String,
    pub detached: bool,
    /// Commits this branch has that its configured upstream does not, and vice
    /// versa. Computed from refs already on disk; nothing contacts a remote,
    /// so these numbers are as fresh as the last fetch somebody else ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<UpstreamInfo>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpstreamInfo {
    pub name: String,
    pub ahead: usize,
    pub behind: usize,
}

fn head_info(repository: &Repository) -> Option<HeadInfo> {
    let reference = repository.head().ok()?;
    let commit = reference.peel_to_commit().ok()?;
    let detached = repository.head_detached().unwrap_or(false);
    let branch_name =
        (!detached).then(|| String::from_utf8_lossy(reference.shorthand_bytes()).into_owned());

    let upstream = branch_name.as_deref().and_then(|name| {
        let branch = repository.find_branch(name, BranchType::Local).ok()?;
        let upstream = branch.upstream().ok()?;
        let upstream_commit = upstream.get().peel_to_commit().ok()?;
        let upstream_name = upstream
            .name()
            .ok()
            .flatten()
            .map(str::to_string)
            .unwrap_or_else(|| "<unnamed>".to_string());
        let (ahead, behind) = repository
            .graph_ahead_behind(commit.id(), upstream_commit.id())
            .ok()?;
        Some(UpstreamInfo {
            name: upstream_name,
            ahead,
            behind,
        })
    });

    Some(HeadInfo {
        reference: (!detached)
            .then(|| String::from_utf8_lossy(reference.name_bytes()).into_owned()),
        branch: branch_name,
        commit: commit.id().to_string(),
        short: short_oid(commit.id()),
        detached,
        upstream,
    })
}

/// Arguments for the `repo_status` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoStatusArgs {
    /// Which configured repository to read. Required only when the operator
    /// configured more than one; call `status` to list them.
    #[serde(default)]
    pub repo: Option<String>,
    /// Include files git does not track. On by default.
    #[serde(default)]
    pub include_untracked: Option<bool>,
    /// Include files `.gitignore` excludes. Off by default, because on a
    /// working checkout this is mostly build output.
    #[serde(default)]
    pub include_ignored: Option<bool>,
    /// Limit the report to these paths, relative to the repository root.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// How many entries to return. Defaults to and is capped at 1000.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoStatusResponse {
    pub repository: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<HeadInfo>,
    /// `clean`, or the operation in progress: `merge`, `rebase`, `bisect`, and
    /// so on. A repository mid-rebase explains a great deal of odd output.
    pub state: &'static str,
    pub clean: bool,
    pub entries: Vec<StatusEntry>,
    pub returned: usize,
    /// Entries matching before the limit was applied.
    pub total: usize,
    pub include_untracked: bool,
    pub include_ignored: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<String>,
}

pub fn repo_status(registry: &Registry, args: RepoStatusArgs) -> PluginResult<RepoStatusResponse> {
    let selected = registry.select(args.repo.as_deref())?;
    if selected.bare {
        return Err(PluginError::invalid_request(format!(
            "repository {:?} is bare, so it has no working tree to report on. Its history is still \
             readable with log, show, diff, blame, and refs",
            selected.alias
        )));
    }

    let include_untracked = args.include_untracked.unwrap_or(true);
    let include_ignored = args.include_ignored.unwrap_or(false);
    let limit = match args.limit {
        None => MAX_STATUS_ENTRIES,
        Some(value) => {
            let value = value as usize;
            if value == 0 || value > MAX_STATUS_ENTRIES {
                return Err(PluginError::invalid_params(format!(
                    "limit must be between 1 and {MAX_STATUS_ENTRIES}, got {value}"
                )));
            }
            value
        }
    };
    let paths = parse_tree_paths(args.paths.as_deref().unwrap_or(&[]), MAX_PATHSPECS)
        .map_err(|error| PluginError::invalid_params(error.to_string()))?;

    let repository = registry.open(selected)?;

    let mut options = StatusOptions::new();
    options
        .show(StatusShow::IndexAndWorkdir)
        .include_untracked(include_untracked)
        .include_ignored(include_ignored)
        .include_unmodified(false)
        // One entry per untracked directory instead of one per file inside it,
        // so a freshly unpacked node_modules is a line rather than a flood.
        .recurse_untracked_dirs(false)
        .recurse_ignored_dirs(false)
        // A submodule's contents live in a repository this plugin was not
        // configured to read.
        .exclude_submodules(true)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true)
        // The one that matters: libgit2 will otherwise write the refreshed
        // stat cache back to .git/index. This plugin does not write.
        .update_index(false);
    for path in &paths {
        options.pathspec(path.as_str());
    }

    let statuses = repository.statuses(Some(&mut options)).map_err(|error| {
        PluginError::internal(format!("status could not be read: {}", error.message()))
    })?;

    let mut entries: Vec<StatusEntry> = Vec::new();
    let total = statuses.len();
    for entry in statuses.iter().take(limit) {
        let path = String::from_utf8_lossy(entry.path_bytes())
            .replace('\\', "/")
            .to_string();
        let old_path = entry
            .head_to_index()
            .and_then(|delta| delta.old_file().path().map(|path| path.to_path_buf()))
            .or_else(|| {
                entry
                    .index_to_workdir()
                    .and_then(|delta| delta.old_file().path().map(|path| path.to_path_buf()))
            })
            .map(|value| value.to_string_lossy().replace('\\', "/"))
            .filter(|value| *value != path);

        entries.push(StatusEntry {
            path,
            flags: status_flags(entry.status()),
            old_path,
        });
    }

    let truncated_reason = (total > entries.len()).then(|| {
        format!(
            "{total} entries matched and the first {} are returned; narrow it with paths",
            entries.len()
        )
    });

    Ok(RepoStatusResponse {
        repository: selected.alias.clone(),
        head: head_info(&repository),
        state: repository_state(repository.state()),
        clean: total == 0,
        returned: entries.len(),
        entries,
        total,
        include_untracked,
        include_ignored,
        truncated: truncated_reason.is_some(),
        truncated_reason,
    })
}

/// Arguments for the `status` tool. It takes none.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusArgs {}

#[derive(Debug, Clone, Serialize)]
pub struct BackendInfo {
    pub library: &'static str,
    /// The libgit2 version this binary is linked against.
    pub libgit2_version: String,
    /// The `git2` crate version providing the bindings.
    pub binding_version: &'static str,
    /// Whether an HTTPS transport is compiled in. Always false here: the
    /// dependency is built with `default-features = false`, so there is no TLS
    /// stack in the binary for a clone or a fetch to use.
    pub https_transport: bool,
    /// Whether an SSH transport is compiled in. Always false, for the same
    /// reason.
    pub ssh_transport: bool,
    /// True when a `git` executable is used. Always false: everything runs
    /// in-process, so there is no subprocess and no PATH lookup.
    pub uses_git_subprocess: bool,
}

pub fn backend_info() -> BackendInfo {
    let version = git2::Version::get();
    let (major, minor, patch) = version.libgit2_version();
    BackendInfo {
        library: "libgit2",
        libgit2_version: format!("{major}.{minor}.{patch}"),
        binding_version: version.crate_version(),
        https_transport: version.https(),
        ssh_transport: version.ssh(),
        uses_git_subprocess: false,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LimitsReport {
    pub max_commits_per_call: usize,
    pub max_commits_scanned: usize,
    pub max_patch_bytes: usize,
    /// Files a diff may touch before rename detection is skipped.
    pub max_rename_candidates: usize,
    pub max_blame_lines: usize,
    pub max_blame_file_bytes: u64,
    pub max_files_listed: usize,
    pub max_refs_listed: usize,
    pub max_status_entries: usize,
    pub max_paths_per_call: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepositoryReport {
    pub alias: String,
    /// `ok` when the repository opened, `unavailable` when it did not.
    pub state: &'static str,
    /// Present only when `state` is `unavailable`. Never contains a path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub bare: bool,
    /// True for a repository with no commits yet.
    pub empty: bool,
    /// True for a shallow clone, whose history stops at a grafted boundary —
    /// `log` and `blame` will both appear to end early on one.
    pub shallow: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<HeadInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub plugin: &'static str,
    pub version: &'static str,
    /// Always true. This plugin declares no tool that commits, checks out,
    /// fetches, pushes, or writes configuration.
    pub read_only: bool,
    pub backend: BackendInfo,
    /// False when the operator passed `--no-content`: no diff hunks and no
    /// blame line text will be returned.
    pub content_available: bool,
    /// True when the operator passed `--redact-emails`.
    pub emails_redacted: bool,
    pub limits: LimitsReport,
    pub repositories: Vec<RepositoryReport>,
    pub repositories_available: usize,
}

pub fn status(registry: &Registry, _args: StatusArgs) -> PluginResult<StatusResponse> {
    let limits = registry.limits();
    let disclosure = registry.disclosure();

    let mut repositories: Vec<RepositoryReport> = Vec::new();

    for resolved in registry.repositories() {
        // Opening and reading HEAD is two small file reads. Keeping `status`
        // answerable while everything else is failing is the whole point of
        // the tool, so it never walks history.
        match registry.open(resolved) {
            Ok(repository) => repositories.push(RepositoryReport {
                alias: resolved.alias.clone(),
                state: "ok",
                reason: None,
                bare: repository.is_bare(),
                empty: repository.is_empty().unwrap_or(false),
                shallow: repository.is_shallow(),
                head: head_info(&repository),
            }),
            Err(error) => repositories.push(RepositoryReport {
                alias: resolved.alias.clone(),
                state: "unavailable",
                reason: Some(format!("{error:?}")),
                bare: resolved.bare,
                empty: false,
                shallow: false,
                head: None,
            }),
        }
    }

    for problem in registry.problems() {
        repositories.push(RepositoryReport {
            alias: problem.alias.clone(),
            state: "unavailable",
            reason: Some(problem.error.to_string()),
            bare: false,
            empty: false,
            shallow: false,
            head: None,
        });
    }

    repositories.sort_by(|left, right| left.alias.cmp(&right.alias));
    let available = repositories
        .iter()
        .filter(|report| report.state == "ok")
        .count();

    Ok(StatusResponse {
        plugin: PLUGIN_NAME,
        version: PLUGIN_VERSION,
        read_only: true,
        backend: backend_info(),
        content_available: disclosure.content,
        emails_redacted: disclosure.redact_emails,
        limits: LimitsReport {
            max_commits_per_call: limits.commit_ceiling,
            max_commits_scanned: limits.max_scan_commits,
            max_patch_bytes: limits.max_patch_bytes,
            max_rename_candidates: limits.rename_candidate_limit,
            max_blame_lines: limits.max_blame_lines,
            max_blame_file_bytes: limits.max_blame_file_bytes,
            max_files_listed: crate::settings::MAX_FILE_ENTRIES,
            max_refs_listed: MAX_REF_ENTRIES,
            max_status_entries: MAX_STATUS_ENTRIES,
            max_paths_per_call: MAX_PATHSPECS,
        },
        repositories,
        repositories_available: available,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Disclosure, Limits, RepoSpec};
    use crate::testsupport::{RepoFixture, TempTree};

    fn registry_for(fixture: &RepoFixture) -> Registry {
        let registry = Registry::resolve(
            &[RepoSpec {
                alias: "repo".to_string(),
                path: fixture.root().to_path_buf(),
            }],
            Limits::default(),
            Disclosure::default(),
        );
        assert!(registry.problems().is_empty(), "{:?}", registry.problems());
        registry
    }

    fn refs_args() -> RefsArgs {
        RefsArgs {
            repo: None,
            kind: None,
            pattern: None,
            include_remote: None,
            limit: None,
        }
    }

    fn repo_status_args() -> RepoStatusArgs {
        RepoStatusArgs {
            repo: None,
            include_untracked: None,
            include_ignored: None,
            paths: None,
            limit: None,
        }
    }

    fn tagged_history(fixture: &RepoFixture) {
        fixture.write("a.txt", "one\n");
        fixture.commit("first");
        fixture.tag_light("v1.0.0");
        fixture.branch("release/1.x");

        fixture.write("b.txt", "two\n");
        fixture.commit("second");
        fixture.tag_annotated("v2.0.0", "the second release");
        fixture.branch("topic/experiment");
    }

    #[test]
    fn refs_lists_branches_and_tags_newest_first() {
        let tree = TempTree::new("refs-basic");
        let fixture = tree.repository("repo");
        tagged_history(&fixture);
        let registry = registry_for(&fixture);

        let response = refs(&registry, refs_args()).expect("lists");
        assert!(!response.truncated);
        assert!(response.returned >= 4);

        let tags: Vec<&RefEntry> = response
            .refs
            .iter()
            .filter(|entry| entry.kind == "tag")
            .collect();
        assert_eq!(tags.len(), 2);

        let annotated = tags.iter().find(|entry| entry.name == "v2.0.0").unwrap();
        assert!(annotated.annotated);
        assert_eq!(annotated.summary.as_deref(), Some("second"));
        assert_eq!(annotated.full_name, "refs/tags/v2.0.0");

        let lightweight = tags.iter().find(|entry| entry.name == "v1.0.0").unwrap();
        assert!(!lightweight.annotated);
        assert_eq!(lightweight.summary.as_deref(), Some("first"));

        // Newest commit first.
        let timestamps: Vec<i64> = response.refs.iter().map(|entry| entry.timestamp).collect();
        let mut sorted = timestamps.clone();
        sorted.sort_by(|left, right| right.cmp(left));
        assert_eq!(timestamps, sorted);

        // Exactly one branch is HEAD.
        assert_eq!(
            response
                .refs
                .iter()
                .filter(|entry| entry.kind == "branch" && entry.head)
                .count(),
            1
        );
    }

    #[test]
    fn refs_can_be_narrowed_by_kind_and_by_substring() {
        let tree = TempTree::new("refs-filter");
        let fixture = tree.repository("repo");
        tagged_history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = refs_args();
        args.kind = Some(RefKind::Tags);
        let tags = refs(&registry, args).expect("lists");
        assert_eq!(tags.returned, 2);
        assert!(tags.refs.iter().all(|entry| entry.kind == "tag"));

        let mut args = refs_args();
        args.kind = Some(RefKind::Branches);
        args.pattern = Some("RELEASE".to_string());
        let branches = refs(&registry, args).expect("lists");
        assert_eq!(branches.returned, 1);
        assert_eq!(branches.refs[0].name, "release/1.x");
    }

    #[test]
    fn a_ref_limit_reports_how_many_matched() {
        let tree = TempTree::new("refs-limit");
        let fixture = tree.repository("repo");
        tagged_history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = refs_args();
        args.limit = Some(1);
        let response = refs(&registry, args).expect("lists");
        assert_eq!(response.returned, 1);
        assert!(response.truncated);
        assert!(
            response
                .truncated_reason
                .expect("reason")
                .contains("matched")
        );

        let mut args = refs_args();
        args.limit = Some(0);
        assert!(refs(&registry, args).is_err());
    }

    #[test]
    fn repo_status_reports_staged_modified_and_untracked_separately() {
        let tree = TempTree::new("status-worktree");
        let fixture = tree.repository("repo");
        fixture.write("tracked.txt", "one\n");
        fixture.write("staged.txt", "committed\n");
        fixture.commit("initial");

        // Modify a tracked file, stage a change to another, add an untracked
        // one.
        fixture.write("tracked.txt", "changed\n");
        fixture.write("staged.txt", "staged change\n");
        {
            let mut index = fixture.repository().index().expect("index");
            index
                .add_path(std::path::Path::new("staged.txt"))
                .expect("stage");
            index.write().expect("write index");
        }
        fixture.write("untracked.txt", "new\n");

        let registry = registry_for(&fixture);
        let response = repo_status(&registry, repo_status_args()).expect("status");

        assert!(!response.clean);
        assert_eq!(response.state, "clean", "no operation is in progress");
        assert_eq!(response.total, 3);

        let by_path = |path: &str| {
            response
                .entries
                .iter()
                .find(|entry| entry.path == path)
                .unwrap_or_else(|| panic!("{path} missing from {:?}", response.entries))
        };
        assert!(by_path("tracked.txt").flags.contains(&"modified"));
        assert!(by_path("staged.txt").flags.contains(&"staged_modified"));
        assert!(by_path("untracked.txt").flags.contains(&"untracked"));

        let head = response.head.expect("a head");
        assert!(!head.detached);
        assert!(head.branch.is_some());
        assert_eq!(head.commit.len(), 40);
        assert!(head.upstream.is_none(), "no remote is configured");
    }

    #[test]
    fn a_clean_tree_says_so() {
        let tree = TempTree::new("status-clean");
        let fixture = tree.repository("repo");
        fixture.write("a.txt", "one\n");
        fixture.commit("initial");

        let registry = registry_for(&fixture);
        let response = repo_status(&registry, repo_status_args()).expect("status");
        assert!(response.clean);
        assert_eq!(response.total, 0);
        assert!(response.entries.is_empty());
        assert!(!response.truncated);
    }

    #[test]
    fn ignored_files_are_excluded_until_asked_for() {
        let tree = TempTree::new("status-ignored");
        let fixture = tree.repository("repo");
        fixture.write(".gitignore", "build/\n");
        fixture.commit("initial");
        fixture.write("build/out.o", "binary\n");

        let registry = registry_for(&fixture);
        let quiet = repo_status(&registry, repo_status_args()).expect("status");
        assert!(quiet.clean, "{:?}", quiet.entries);

        let mut args = repo_status_args();
        args.include_ignored = Some(true);
        let loud = repo_status(&registry, args).expect("status");
        assert!(!loud.clean);
        assert!(
            loud.entries
                .iter()
                .any(|entry| entry.flags.contains(&"ignored"))
        );
    }

    #[test]
    fn untracked_files_can_be_left_out() {
        let tree = TempTree::new("status-untracked");
        let fixture = tree.repository("repo");
        fixture.write("a.txt", "one\n");
        fixture.commit("initial");
        fixture.write("new.txt", "new\n");

        let registry = registry_for(&fixture);
        let mut args = repo_status_args();
        args.include_untracked = Some(false);
        let response = repo_status(&registry, args).expect("status");
        assert!(response.clean);
        assert!(!response.include_untracked);
    }

    #[test]
    fn repo_status_refuses_a_bare_repository_and_names_what_still_works() {
        let tree = TempTree::new("status-bare");
        let fixture = tree.repository("repo");
        fixture.write("a.txt", "one\n");
        fixture.commit("initial");
        let bare_path = tree.path().join("bare.git");
        fixture.clone_bare(&bare_path);

        let registry = Registry::resolve(
            &[RepoSpec {
                alias: "bare".to_string(),
                path: bare_path,
            }],
            Limits::default(),
            Disclosure::default(),
        );
        assert!(registry.problems().is_empty());

        let error = repo_status(&registry, repo_status_args()).expect_err("no working tree");
        let message = format!("{error:?}");
        assert!(message.contains("bare"), "{message}");
        assert!(message.contains("log"), "{message}");
    }

    #[test]
    fn status_reports_the_backend_the_limits_and_every_repository() {
        let tree = TempTree::new("plugin-status");
        let fixture = tree.repository("repo");
        fixture.write("a.txt", "one\n");
        fixture.commit("initial");
        let registry = registry_for(&fixture);

        let response = status(&registry, StatusArgs {}).expect("status");

        assert_eq!(response.plugin, "git-tools");
        assert!(response.read_only);
        assert!(response.content_available);
        assert!(!response.emails_redacted);

        // The claim the README makes about network access, asserted against
        // what the binary actually linked.
        assert_eq!(response.backend.library, "libgit2");
        assert!(!response.backend.https_transport);
        assert!(!response.backend.ssh_transport);
        assert!(!response.backend.uses_git_subprocess);
        assert!(!response.backend.libgit2_version.is_empty());

        assert_eq!(response.repositories.len(), 1);
        assert_eq!(response.repositories_available, 1);
        let report = &response.repositories[0];
        assert_eq!(report.alias, "repo");
        assert_eq!(report.state, "ok");
        assert!(!report.bare);
        assert!(!report.empty);
        assert!(!report.shallow);
        assert!(report.head.is_some());

        assert_eq!(
            response.limits.max_commits_per_call,
            Limits::default().commit_ceiling
        );
    }

    #[test]
    fn status_reports_a_repository_that_failed_to_resolve_instead_of_hiding_it() {
        let tree = TempTree::new("plugin-status-broken");
        let good = tree.repository("good");
        good.write("a.txt", "one\n");
        good.commit("initial");

        let registry = Registry::resolve(
            &[
                RepoSpec {
                    alias: "good".to_string(),
                    path: good.root().to_path_buf(),
                },
                RepoSpec {
                    alias: "gone".to_string(),
                    path: tree.path().join("nowhere"),
                },
            ],
            Limits::default(),
            Disclosure::default(),
        );

        let response = status(&registry, StatusArgs {}).expect("status still answers");
        assert_eq!(response.repositories.len(), 2);
        assert_eq!(response.repositories_available, 1);

        let broken = response
            .repositories
            .iter()
            .find(|report| report.alias == "gone")
            .expect("reported");
        assert_eq!(broken.state, "unavailable");
        let reason = broken.reason.as_deref().expect("a reason");
        assert!(reason.contains("could not be resolved"), "{reason}");
        assert!(!reason.contains("nowhere"), "{reason}");
    }

    #[test]
    fn status_mirrors_the_disclosure_policy_the_operator_chose() {
        let tree = TempTree::new("plugin-status-policy");
        let fixture = tree.repository("repo");
        fixture.write("a.txt", "one\n");
        fixture.commit("initial");

        let registry = Registry::resolve(
            &[RepoSpec {
                alias: "repo".to_string(),
                path: fixture.root().to_path_buf(),
            }],
            Limits::default(),
            Disclosure {
                content: false,
                redact_emails: true,
            },
        );

        let response = status(&registry, StatusArgs {}).expect("status");
        assert!(!response.content_available);
        assert!(response.emails_redacted);
    }

    #[test]
    fn status_answers_for_an_empty_repository() {
        let tree = TempTree::new("plugin-status-empty");
        let fixture = tree.repository("repo");
        let registry = registry_for(&fixture);

        let response = status(&registry, StatusArgs {}).expect("status");
        let report = &response.repositories[0];
        assert_eq!(report.state, "ok");
        assert!(report.empty);
        assert!(report.head.is_none());
    }

    #[test]
    fn status_flag_names_are_stable_and_combine() {
        assert_eq!(status_flags(Status::CURRENT), Vec::<&str>::new());
        assert_eq!(status_flags(Status::WT_NEW), vec!["untracked"]);
        assert_eq!(
            status_flags(Status::INDEX_MODIFIED | Status::WT_MODIFIED),
            vec!["staged_modified", "modified"]
        );
        assert_eq!(status_flags(Status::CONFLICTED), vec!["conflicted"]);
    }
}
