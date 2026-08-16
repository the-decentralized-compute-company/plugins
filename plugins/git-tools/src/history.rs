//! `log` and `show` — the two tools that answer "when did this happen".
//!
//! ### Why the text filters are substring, not regex
//!
//! `author` and `grep` match case-insensitive substrings. A regex would be more
//! expressive and would also hand a model the ability to spend unbounded CPU on
//! somebody else's machine: a pattern with nested quantifiers applied to every
//! commit message in a large repository is a denial of service that arrives
//! looking like a search. Substring matching is linear, is what most callers
//! meant anyway, and the walk is separately bounded by `--max-scan-commits`.
//!
//! ### What the path filter means on a merge
//!
//! A commit is considered to touch a path when its diff *against its first
//! parent* touches it. That is a simplification — git's own history
//! simplification is subtler — and it means a merge that resolved a conflict in
//! a file will be reported as touching that file, while a merge that took one
//! side wholesale usually will not. Stated here rather than left to be noticed.

use git2::{Commit, Repository, Sort};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tdcc_plugin::{PluginError, PluginResult};

use crate::changes::{
    ChangeSet, DEFAULT_CONTEXT_LINES, DiffBudget, MAX_CONTEXT_LINES, RevisionRef,
    diff_commit_against_parent, diff_options, render_changes,
};
use crate::guard::{TreePath, parse_tree_paths};
use crate::render::{Identity, identity, message_text, parse_time_bound, short_oid, truncate_text};
use crate::repos::Registry;
use crate::resolve::{commit_tree, required_revision, resolve_commit, revision_or_head};
use crate::settings::{
    Disclosure, MAX_FILE_ENTRIES, MAX_LOG_MESSAGE_BYTES, MAX_PATHSPECS, MAX_SHOW_MESSAGE_BYTES,
};

/// How merge commits are treated by `log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MergeFilter {
    /// Merges and ordinary commits alike. The default.
    Include,
    /// Only commits with a single parent — the ones that carry actual changes.
    Exclude,
    /// Only commits with more than one parent.
    Only,
}

/// One commit as a response carries it.
#[derive(Debug, Clone, Serialize)]
pub struct CommitSummary {
    pub commit: String,
    pub short: String,
    pub parents: Vec<String>,
    pub author: Identity,
    pub committer: Identity,
    /// First line of the message, as git defines a summary.
    pub summary: Option<String>,
    /// The full message, shortened to the per-tool cap.
    pub message: String,
    /// True when `message` was shortened.
    pub message_truncated: bool,
    /// True when the commit has more than one parent.
    pub merge: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<CommitStats>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct CommitStats {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

pub fn commit_summary(
    commit: &Commit<'_>,
    disclosure: Disclosure,
    message_limit: usize,
) -> CommitSummary {
    let (message, message_truncated) =
        truncate_text(&message_text(commit.message_bytes()), message_limit);
    CommitSummary {
        commit: commit.id().to_string(),
        short: short_oid(commit.id()),
        parents: commit.parent_ids().map(|oid| oid.to_string()).collect(),
        author: identity(&commit.author(), disclosure),
        committer: identity(&commit.committer(), disclosure),
        summary: commit
            .summary_bytes()
            .map(message_text)
            .map(|text| truncate_text(&text, 1_024).0),
        message,
        message_truncated,
        merge: commit.parent_count() > 1,
        stats: None,
    }
}

/// Arguments for the `log` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogArgs {
    /// Which configured repository to read. Required only when the operator
    /// configured more than one; call `status` to list them.
    #[serde(default)]
    pub repo: Option<String>,
    /// Where the walk starts: a commit, branch, or tag. Defaults to HEAD.
    #[serde(default)]
    pub rev: Option<String>,
    /// Hide commits reachable from this revision. Setting `rev` to `v2.0.0`
    /// and `exclude_rev` to `v1.0.0` gives exactly what landed between the two
    /// releases.
    #[serde(default)]
    pub exclude_rev: Option<String>,
    /// Only commits that changed these paths, relative to the repository root.
    /// `*`, `?` and `[…]` match the way a shell glob does. Filtering by path
    /// reads a tree diff per commit, so it is the slowest option here.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// Only commits whose author name or email contains this text, matched
    /// case-insensitively. Plain substring, not a pattern.
    #[serde(default)]
    pub author: Option<String>,
    /// Only commits whose message contains this text, matched
    /// case-insensitively. Plain substring, not a pattern.
    #[serde(default)]
    pub grep: Option<String>,
    /// Only commits authored at or after this time. Accepts `YYYY-MM-DD`,
    /// `YYYY-MM-DDTHH:MM:SSZ`, or a plain epoch second. Always read as UTC.
    #[serde(default)]
    pub since: Option<String>,
    /// Only commits authored at or before this time. Same formats as `since`.
    #[serde(default)]
    pub until: Option<String>,
    /// Whether merge commits are included, excluded, or the only thing
    /// returned. Defaults to include.
    #[serde(default)]
    pub merges: Option<MergeFilter>,
    /// Follow only the first parent of each merge, which reads a merged branch
    /// as one entry instead of as everything it contained. Off by default.
    #[serde(default)]
    pub first_parent: Option<bool>,
    /// How many commits to return. Defaults to 30; the operator sets the
    /// ceiling with `--max-commits`.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Skip this many matching commits before returning any. Use it to page
    /// through a long history.
    #[serde(default)]
    pub skip: Option<u32>,
    /// Include per-commit files-changed, insertions, and deletions. Off by
    /// default because it costs one tree diff per returned commit.
    #[serde(default)]
    pub include_stats: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogResponse {
    pub repository: String,
    pub rev: RevisionRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_rev: Option<RevisionRef>,
    pub commits: Vec<CommitSummary>,
    pub returned: usize,
    /// Commits the walk examined, including those the filters rejected.
    pub commits_scanned: usize,
    pub limit: usize,
    pub skip: usize,
    /// True when the walk still had commits left after `limit` was filled, so
    /// paging further with `skip` will return more.
    pub more_available: bool,
    /// True when a cap stopped the walk before it ran out of commits, which
    /// means matching commits may exist that are not reported here.
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<String>,
}

fn touches_paths(
    repository: &Repository,
    commit: &Commit<'_>,
    paths: &[TreePath],
) -> PluginResult<bool> {
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
    // Zero context: only the delta list is consulted, so no hunk is ever built.
    let mut options = diff_options(paths, 0);
    let diff = repository
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut options))
        .map_err(|error| {
            PluginError::internal(format!("diff could not be computed: {}", error.message()))
        })?;
    Ok(diff.deltas().len() > 0)
}

fn matches_text(haystack: &str, needle_lowercase: &str) -> bool {
    haystack.to_lowercase().contains(needle_lowercase)
}

pub fn log(registry: &Registry, args: LogArgs) -> PluginResult<LogResponse> {
    let selected = registry.select(args.repo.as_deref())?;
    let limits = registry.limits();
    let disclosure = registry.disclosure();

    let limit = match args.limit {
        None => crate::settings::DEFAULT_MAX_COMMITS.min(limits.commit_ceiling),
        Some(value) => {
            let value = value as usize;
            if value == 0 || value > limits.commit_ceiling {
                return Err(PluginError::invalid_params(format!(
                    "limit must be between 1 and {}, got {value}",
                    limits.commit_ceiling
                )));
            }
            value
        }
    };
    let skip = args.skip.unwrap_or(0) as usize;

    let paths = parse_tree_paths(args.paths.as_deref().unwrap_or(&[]), MAX_PATHSPECS)
        .map_err(|error| PluginError::invalid_params(error.to_string()))?;
    let since = args
        .since
        .as_deref()
        .map(parse_time_bound)
        .transpose()
        .map_err(|error| PluginError::invalid_params(format!("since: {error}")))?;
    let until = args
        .until
        .as_deref()
        .map(parse_time_bound)
        .transpose()
        .map_err(|error| PluginError::invalid_params(format!("until: {error}")))?;
    if let (Some(since), Some(until)) = (since, until)
        && since > until
    {
        return Err(PluginError::invalid_params(
            "since is later than until, so no commit could ever match",
        ));
    }

    let author_filter = args.author.as_deref().map(str::to_lowercase);
    let grep_filter = args.grep.as_deref().map(str::to_lowercase);
    let merges = args.merges.unwrap_or(MergeFilter::Include);
    let include_stats = args.include_stats.unwrap_or(false);

    let revision = revision_or_head(args.rev.as_deref())?;
    let exclude = args
        .exclude_rev
        .as_deref()
        .map(|value| required_revision(value, "exclude_rev"))
        .transpose()?;

    let repository = registry.open(selected)?;
    let start = resolve_commit(&repository, &revision)?;
    let exclude_commit = exclude
        .as_ref()
        .map(|value| resolve_commit(&repository, value))
        .transpose()?;

    let mut walk = repository.revwalk().map_err(|error| {
        PluginError::internal(format!("history walk could not be started: {error}"))
    })?;
    walk.set_sorting(Sort::TIME)
        .map_err(|error| PluginError::internal(format!("walk order could not be set: {error}")))?;
    walk.push(start.id())
        .map_err(|error| PluginError::internal(format!("walk start could not be set: {error}")))?;
    if let Some(exclude_commit) = &exclude_commit {
        walk.hide(exclude_commit.id()).map_err(|error| {
            PluginError::internal(format!("exclude_rev could not be applied: {error}"))
        })?;
    }
    if args.first_parent.unwrap_or(false) {
        walk.simplify_first_parent().map_err(|error| {
            PluginError::internal(format!("first_parent could not be applied: {error}"))
        })?;
    }

    let mut commits: Vec<CommitSummary> = Vec::new();
    let mut scanned = 0usize;
    let mut matched = 0usize;
    let mut truncated = false;
    let mut more_available = false;

    for step in walk {
        if scanned >= limits.max_scan_commits {
            truncated = true;
            break;
        }
        let oid = step.map_err(|error| {
            PluginError::internal(format!("history walk failed: {}", error.message()))
        })?;
        scanned += 1;

        let commit = match repository.find_commit(oid) {
            Ok(commit) => commit,
            // A ref can point at a missing object in a damaged or partially
            // fetched repository. Skipping it keeps the rest of the answer
            // useful; the count of scanned commits still reflects the work.
            Err(_) => continue,
        };

        let is_merge = commit.parent_count() > 1;
        match merges {
            MergeFilter::Include => {}
            MergeFilter::Exclude if is_merge => continue,
            MergeFilter::Only if !is_merge => continue,
            _ => {}
        }

        let when = commit.author().when().seconds();
        if since.is_some_and(|bound| when < bound) || until.is_some_and(|bound| when > bound) {
            continue;
        }

        if let Some(needle) = &author_filter {
            let author = commit.author();
            let name = String::from_utf8_lossy(author.name_bytes()).into_owned();
            let email = String::from_utf8_lossy(author.email_bytes()).into_owned();
            if !matches_text(&name, needle) && !matches_text(&email, needle) {
                continue;
            }
        }

        if let Some(needle) = &grep_filter
            && !matches_text(&message_text(commit.message_bytes()), needle)
        {
            continue;
        }

        if !paths.is_empty() && !touches_paths(&repository, &commit, &paths)? {
            continue;
        }

        matched += 1;
        if matched <= skip {
            continue;
        }
        if commits.len() == limit {
            more_available = true;
            break;
        }

        let mut summary = commit_summary(&commit, disclosure, MAX_LOG_MESSAGE_BYTES);
        if include_stats {
            let (diff, renames) =
                diff_commit_against_parent(&repository, &commit, &paths, 0, false, 0)?;
            let changes = render_changes(
                &diff,
                false,
                DiffBudget {
                    max_patch_bytes: 0,
                    max_files: MAX_FILE_ENTRIES,
                },
                renames,
            )?;
            summary.stats = Some(CommitStats {
                files_changed: changes.files_changed,
                insertions: changes.insertions,
                deletions: changes.deletions,
            });
        }
        commits.push(summary);
    }

    let truncated_reason = truncated.then(|| {
        format!(
            "the walk stopped after examining {} commits. Narrow it with exclude_rev, since, or \
             paths, or ask the operator to raise --max-scan-commits",
            limits.max_scan_commits
        )
    });

    Ok(LogResponse {
        repository: selected.alias.clone(),
        rev: RevisionRef::new(revision.as_str(), &start),
        exclude_rev: exclude
            .as_ref()
            .zip(exclude_commit.as_ref())
            .map(|(value, commit)| RevisionRef::new(value.as_str(), commit)),
        returned: commits.len(),
        commits,
        commits_scanned: scanned,
        limit,
        skip,
        more_available,
        truncated,
        truncated_reason,
    })
}

/// Arguments for the `show` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShowArgs {
    /// Which configured repository to read. Required only when the operator
    /// configured more than one; call `status` to list them.
    #[serde(default)]
    pub repo: Option<String>,
    /// The commit to show: a commit id, branch, or tag.
    pub rev: String,
    /// Return the unified diff text as well as the file list. Off by default.
    #[serde(default)]
    pub patch: Option<bool>,
    /// Limit the change list and the patch to these paths, relative to the
    /// repository root.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// Context lines around each change in the patch text. 0–25, default 3.
    #[serde(default)]
    pub context_lines: Option<u32>,
    /// Detect renames and copies. On by default.
    #[serde(default)]
    pub detect_renames: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShowResponse {
    pub repository: String,
    pub requested: String,
    pub commit: CommitSummary,
    /// What the change list is measured against: `first_parent` for an
    /// ordinary or merge commit, `empty_tree` for a repository's root commit.
    /// A merge is shown against its first parent only, so a change that came
    /// in through another parent will not appear here.
    pub diff_against: &'static str,
    #[serde(flatten)]
    pub changes: ChangeSet,
}

pub fn show(registry: &Registry, args: ShowArgs) -> PluginResult<ShowResponse> {
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
    let revision = required_revision(&args.rev, "rev")?;

    let repository = registry.open(selected)?;
    let commit = resolve_commit(&repository, &revision)?;

    let (diff, renames) = diff_commit_against_parent(
        &repository,
        &commit,
        &paths,
        context_lines,
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

    Ok(ShowResponse {
        repository: selected.alias.clone(),
        requested: revision.as_str().to_string(),
        diff_against: if commit.parent_count() == 0 {
            "empty_tree"
        } else {
            "first_parent"
        },
        commit: commit_summary(&commit, disclosure, MAX_SHOW_MESSAGE_BYTES),
        changes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Limits, RepoSpec};
    use crate::testsupport::{BASE_EPOCH, COMMIT_INTERVAL, RepoFixture, TempTree};

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

    fn registry_for(fixture: &RepoFixture) -> Registry {
        registry_with(fixture, Limits::default(), Disclosure::default())
    }

    fn log_args() -> LogArgs {
        LogArgs {
            repo: None,
            rev: None,
            exclude_rev: None,
            paths: None,
            author: None,
            grep: None,
            since: None,
            until: None,
            merges: None,
            first_parent: None,
            limit: None,
            skip: None,
            include_stats: None,
        }
    }

    fn show_args(rev: &str) -> ShowArgs {
        ShowArgs {
            repo: None,
            rev: rev.to_string(),
            patch: None,
            paths: None,
            context_lines: None,
            detect_renames: None,
        }
    }

    /// Four commits, two authors, two tags, one file touched twice.
    fn history(fixture: &RepoFixture) {
        fixture.write("src/main.rs", "fn main() {}\n");
        fixture.commit_as("Ada Lovelace", "ada@example.org", "feat: first cut");
        fixture.tag_light("v1.0.0");

        fixture.write("docs/guide.md", "hello\n");
        fixture.commit_as("Grace Hopper", "grace@example.org", "docs: add the guide");

        fixture.write("src/main.rs", "fn main() {\n    run();\n}\n");
        fixture.commit_as(
            "Ada Lovelace",
            "ada@example.org",
            "fix: call run\n\nlong body\n",
        );

        fixture.write("docs/guide.md", "hello\nworld\n");
        fixture.commit_as(
            "Grace Hopper",
            "grace@example.org",
            "docs: expand the guide",
        );
        fixture.tag_light("v2.0.0");
    }

    /// Builds the exact payload the README quotes, so the two cannot drift.
    fn readme_example_response() -> serde_json::Value {
        let tree = TempTree::new("readme-example");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = log_args();
        args.rev = Some("v2.0.0".to_string());
        args.exclude_rev = Some("v1.0.0".to_string());
        args.paths = Some(vec!["docs/*.md".to_string()]);
        args.limit = Some(2);
        args.include_stats = Some(true);

        serde_json::to_value(log(&registry, args).expect("logs")).expect("serializes")
    }

    /// The README quotes one `log` response in full. This pins it.
    ///
    /// The fixture is deterministic — fixed commit times, fixed content, fixed
    /// author signatures — so the commit ids below are stable on every machine.
    /// If this test fails, the README is now wrong and one of the two has to
    /// change.
    #[test]
    fn the_readme_example_is_exactly_what_log_returns() {
        let expected: serde_json::Value =
            serde_json::from_str(README_EXAMPLE).expect("the README payload is valid JSON");
        assert_eq!(readme_example_response(), expected);
    }

    const README_EXAMPLE: &str = r#"{
  "commits": [
    {
      "author": {
        "date": "2024-03-15T11:21:07+00:00",
        "email": "grace@example.org",
        "name": "Grace Hopper",
        "offset_minutes": 0,
        "timestamp": 1710501667
      },
      "commit": "d670d2ce756a82eb3f28fa69a59262a64fcd4865",
      "committer": {
        "date": "2024-03-15T11:21:07+00:00",
        "email": "grace@example.org",
        "name": "Grace Hopper",
        "offset_minutes": 0,
        "timestamp": 1710501667
      },
      "merge": false,
      "message": "docs: expand the guide",
      "message_truncated": false,
      "parents": [
        "242a3e8c3e84a2f5bf867bb19a5bdf7f75dfe834"
      ],
      "short": "d670d2ce756a",
      "stats": {
        "deletions": 0,
        "files_changed": 1,
        "insertions": 1
      },
      "summary": "docs: expand the guide"
    },
    {
      "author": {
        "date": "2024-03-15T09:21:07+00:00",
        "email": "grace@example.org",
        "name": "Grace Hopper",
        "offset_minutes": 0,
        "timestamp": 1710494467
      },
      "commit": "b82f45b17d3c72bb22717809b60137b059872df5",
      "committer": {
        "date": "2024-03-15T09:21:07+00:00",
        "email": "grace@example.org",
        "name": "Grace Hopper",
        "offset_minutes": 0,
        "timestamp": 1710494467
      },
      "merge": false,
      "message": "docs: add the guide",
      "message_truncated": false,
      "parents": [
        "74e6e637d75e5b9c9847cb8554d8d562efffe49b"
      ],
      "short": "b82f45b17d3c",
      "stats": {
        "deletions": 0,
        "files_changed": 1,
        "insertions": 1
      },
      "summary": "docs: add the guide"
    }
  ],
  "commits_scanned": 3,
  "exclude_rev": {
    "commit": "74e6e637d75e5b9c9847cb8554d8d562efffe49b",
    "requested": "v1.0.0",
    "short": "74e6e637d75e"
  },
  "limit": 2,
  "more_available": false,
  "repository": "repo",
  "returned": 2,
  "rev": {
    "commit": "d670d2ce756a82eb3f28fa69a59262a64fcd4865",
    "requested": "v2.0.0",
    "short": "d670d2ce756a"
  },
  "skip": 0,
  "truncated": false
}"#;

    #[test]
    fn log_returns_newest_first_with_the_metadata_a_citation_needs() {
        let tree = TempTree::new("log-basic");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_for(&fixture);

        let response = log(&registry, log_args()).expect("logs");

        assert_eq!(response.returned, 4);
        assert_eq!(response.commits_scanned, 4);
        assert!(!response.more_available);
        assert!(!response.truncated);

        let summaries: Vec<&str> = response
            .commits
            .iter()
            .map(|commit| commit.summary.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(
            summaries,
            [
                "docs: expand the guide",
                "fix: call run",
                "docs: add the guide",
                "feat: first cut"
            ]
        );

        let newest = &response.commits[0];
        assert_eq!(newest.commit.len(), 40);
        assert_eq!(newest.short.len(), 12);
        assert_eq!(newest.author.name, "Grace Hopper");
        assert_eq!(newest.author.email, "grace@example.org");
        assert!(!newest.merge);
        assert_eq!(newest.parents.len(), 1);
        assert!(newest.stats.is_none(), "stats are opt-in");
    }

    #[test]
    fn a_release_range_is_two_arguments_rather_than_one_range_string() {
        let tree = TempTree::new("log-range");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = log_args();
        args.rev = Some("v2.0.0".to_string());
        args.exclude_rev = Some("v1.0.0".to_string());
        let response = log(&registry, args).expect("logs");

        assert_eq!(response.returned, 3);
        assert!(response.exclude_rev.is_some());
        assert!(
            response
                .commits
                .iter()
                .all(|commit| commit.summary.as_deref() != Some("feat: first cut"))
        );
    }

    #[test]
    fn the_author_filter_matches_name_and_email_case_insensitively() {
        let tree = TempTree::new("log-author");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_for(&fixture);

        for needle in ["ADA", "ada@example.org", "lovelace"] {
            let mut args = log_args();
            args.author = Some(needle.to_string());
            let response = log(&registry, args).expect("logs");
            assert_eq!(response.returned, 2, "needle {needle:?}");
            assert!(
                response
                    .commits
                    .iter()
                    .all(|commit| commit.author.name == "Ada Lovelace")
            );
        }
    }

    #[test]
    fn the_message_filter_is_a_substring_and_searches_the_body_too() {
        let tree = TempTree::new("log-grep");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = log_args();
        args.grep = Some("DOCS:".to_string());
        assert_eq!(log(&registry, args).expect("logs").returned, 2);

        let mut args = log_args();
        args.grep = Some("long body".to_string());
        let response = log(&registry, args).expect("logs");
        assert_eq!(response.returned, 1);
        assert_eq!(
            response.commits[0].summary.as_deref(),
            Some("fix: call run")
        );

        // A regex is treated as literal text, which is the documented
        // behaviour and the reason this cannot be a CPU sink.
        let mut args = log_args();
        args.grep = Some("(docs|fix)".to_string());
        assert_eq!(log(&registry, args).expect("logs").returned, 0);
    }

    #[test]
    fn the_path_filter_returns_only_commits_that_touched_it() {
        let tree = TempTree::new("log-paths");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = log_args();
        args.paths = Some(vec!["src/main.rs".to_string()]);
        let response = log(&registry, args).expect("logs");
        assert_eq!(response.returned, 2);
        assert_eq!(response.commits_scanned, 4, "every commit was examined");

        let mut args = log_args();
        args.paths = Some(vec!["docs/*.md".to_string()]);
        assert_eq!(log(&registry, args).expect("logs").returned, 2);

        let mut args = log_args();
        args.paths = Some(vec!["nothing/here.txt".to_string()]);
        assert_eq!(log(&registry, args).expect("logs").returned, 0);
    }

    #[test]
    fn date_bounds_use_the_authored_time_in_utc() {
        let tree = TempTree::new("log-dates");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_for(&fixture);

        // The fixture clock ticks an hour per commit from BASE_EPOCH.
        let third = BASE_EPOCH + 2 * COMMIT_INTERVAL;
        let mut args = log_args();
        args.since = Some(third.to_string());
        let response = log(&registry, args).expect("logs");
        assert_eq!(response.returned, 2);

        let mut args = log_args();
        args.until = Some(BASE_EPOCH.to_string());
        assert_eq!(log(&registry, args).expect("logs").returned, 1);

        let mut args = log_args();
        args.since = Some("2030-01-01".to_string());
        args.until = Some("2020-01-01".to_string());
        let error = log(&registry, args).expect_err("impossible window");
        assert!(format!("{error:?}").contains("no commit could ever match"));
    }

    #[test]
    fn paging_reports_whether_more_is_available() {
        let tree = TempTree::new("log-paging");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = log_args();
        args.limit = Some(2);
        let first = log(&registry, args).expect("logs");
        assert_eq!(first.returned, 2);
        assert!(first.more_available);

        let mut args = log_args();
        args.limit = Some(2);
        args.skip = Some(2);
        let second = log(&registry, args).expect("logs");
        assert_eq!(second.returned, 2);
        assert!(!second.more_available);
        assert_ne!(first.commits[0].commit, second.commits[0].commit);

        let mut args = log_args();
        args.limit = Some(10);
        args.skip = Some(10);
        let empty = log(&registry, args).expect("logs");
        assert_eq!(empty.returned, 0);
    }

    #[test]
    fn a_limit_over_the_operators_ceiling_is_refused() {
        let tree = TempTree::new("log-ceiling");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_with(
            &fixture,
            Limits {
                commit_ceiling: 3,
                ..Limits::default()
            },
            Disclosure::default(),
        );

        let mut args = log_args();
        args.limit = Some(50);
        let error = log(&registry, args).expect_err("over the ceiling");
        assert!(format!("{error:?}").contains("between 1 and 3"));

        // And the default is clamped to the ceiling rather than exceeding it.
        assert_eq!(log(&registry, log_args()).expect("logs").limit, 3);
    }

    #[test]
    fn the_scan_limit_stops_the_walk_and_says_which_cap_did_it() {
        let tree = TempTree::new("log-scan-cap");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_with(
            &fixture,
            Limits {
                max_scan_commits: 2,
                ..Limits::default()
            },
            Disclosure::default(),
        );

        let mut args = log_args();
        // A filter nothing matches, so the walk runs to the scan cap.
        args.grep = Some("nothing matches this".to_string());
        let response = log(&registry, args).expect("logs");

        assert_eq!(response.returned, 0);
        assert_eq!(response.commits_scanned, 2);
        assert!(response.truncated);
        assert!(
            response
                .truncated_reason
                .as_deref()
                .expect("a reason")
                .contains("max-scan-commits")
        );
    }

    #[test]
    fn merges_can_be_included_excluded_or_isolated() {
        let tree = TempTree::new("log-merges");
        let fixture = tree.repository("repo");
        fixture.write("base.txt", "base\n");
        fixture.commit("base");
        fixture.branch("topic");
        fixture.write("main.txt", "main\n");
        let mainline = fixture.commit("mainline");

        let repository = fixture.repository();
        let topic = repository
            .find_branch("topic", git2::BranchType::Local)
            .expect("topic")
            .get()
            .peel_to_commit()
            .expect("topic commit");
        let head = repository.find_commit(mainline).expect("mainline commit");
        let signature = git2::Signature::new(
            "Ada Lovelace",
            "ada@example.org",
            &git2::Time::new(BASE_EPOCH + 10 * COMMIT_INTERVAL, 0),
        )
        .expect("signature");
        let tree_object = head.tree().expect("tree");
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "merge topic",
                &tree_object,
                &[&head, &topic],
            )
            .expect("merge commit");

        let registry = registry_for(&fixture);

        assert_eq!(log(&registry, log_args()).expect("logs").returned, 3);

        let mut args = log_args();
        args.merges = Some(MergeFilter::Only);
        let only = log(&registry, args).expect("logs");
        assert_eq!(only.returned, 1);
        assert!(only.commits[0].merge);
        assert_eq!(only.commits[0].parents.len(), 2);

        let mut args = log_args();
        args.merges = Some(MergeFilter::Exclude);
        let without = log(&registry, args).expect("logs");
        assert_eq!(without.returned, 2);
        assert!(without.commits.iter().all(|commit| !commit.merge));
    }

    #[test]
    fn stats_are_included_only_when_asked_for() {
        let tree = TempTree::new("log-stats");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = log_args();
        args.include_stats = Some(true);
        args.limit = Some(1);
        let response = log(&registry, args).expect("logs");

        let stats = response.commits[0].stats.expect("asked for stats");
        assert_eq!(stats.files_changed, 1);
        assert_eq!(stats.insertions, 1);
        assert_eq!(stats.deletions, 0);
    }

    #[test]
    fn redacting_emails_keeps_the_names_that_make_a_log_useful() {
        let tree = TempTree::new("log-redact");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_with(
            &fixture,
            Limits::default(),
            Disclosure {
                content: true,
                redact_emails: true,
            },
        );

        let response = log(&registry, log_args()).expect("logs");
        for commit in &response.commits {
            assert_eq!(commit.author.email, crate::render::REDACTED_EMAIL);
            assert_eq!(commit.committer.email, crate::render::REDACTED_EMAIL);
            assert!(!commit.author.name.is_empty());
        }

        // And the filter still works on the email, because it runs against the
        // commit rather than against the redacted response.
        let mut args = log_args();
        args.author = Some("ada@example.org".to_string());
        assert_eq!(log(&registry, args).expect("logs").returned, 2);
    }

    #[test]
    fn show_reports_one_commit_against_its_first_parent() {
        let tree = TempTree::new("show-basic");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_for(&fixture);

        let response = show(&registry, show_args("HEAD~1")).expect("shows");

        assert_eq!(response.diff_against, "first_parent");
        assert_eq!(response.commit.summary.as_deref(), Some("fix: call run"));
        assert_eq!(response.commit.message, "fix: call run\n\nlong body\n");
        assert!(!response.commit.message_truncated);
        assert_eq!(response.changes.files_changed, 1);
        assert_eq!(
            response.changes.files[0].path.as_deref(),
            Some("src/main.rs")
        );
        assert!(response.changes.patch.is_none());
    }

    #[test]
    fn showing_the_root_commit_diffs_against_the_empty_tree() {
        let tree = TempTree::new("show-root");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_for(&fixture);

        let response = show(&registry, show_args("v1.0.0")).expect("shows");
        assert_eq!(response.diff_against, "empty_tree");
        assert_eq!(response.changes.files_changed, 1);
        assert_eq!(response.changes.files[0].status, "added");
    }

    #[test]
    fn show_returns_patch_text_on_request() {
        let tree = TempTree::new("show-patch");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_for(&fixture);

        let mut args = show_args("HEAD~1");
        args.patch = Some(true);
        let response = show(&registry, args).expect("shows");
        let patch = response.changes.patch.expect("asked for it");
        assert!(patch.contains("+    run();"), "{patch}");
    }

    #[test]
    fn show_refuses_patch_text_under_no_content() {
        let tree = TempTree::new("show-no-content");
        let fixture = tree.repository("repo");
        history(&fixture);
        let registry = registry_with(
            &fixture,
            Limits::default(),
            Disclosure {
                content: false,
                redact_emails: false,
            },
        );

        let mut args = show_args("HEAD");
        args.patch = Some(true);
        let error = show(&registry, args).expect_err("content is off");
        assert!(format!("{error:?}").contains("--no-content"));
    }

    #[test]
    fn a_long_message_is_shortened_and_flagged_rather_than_silently_cut() {
        let tree = TempTree::new("show-long-message");
        let fixture = tree.repository("repo");
        fixture.write("a.txt", "a\n");
        let long = format!("subject\n\n{}", "body line\n".repeat(20_000));
        fixture.commit(&long);
        let registry = registry_for(&fixture);

        let response = show(&registry, show_args("HEAD")).expect("shows");
        assert!(response.commit.message_truncated);
        assert!(response.commit.message.len() <= MAX_SHOW_MESSAGE_BYTES);

        let logged = log(&registry, log_args()).expect("logs");
        assert!(logged.commits[0].message_truncated);
        assert!(logged.commits[0].message.len() <= MAX_LOG_MESSAGE_BYTES);
    }

    #[test]
    fn log_reads_a_bare_repository_the_same_way() {
        let tree = TempTree::new("log-bare");
        let fixture = tree.repository("repo");
        history(&fixture);
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
        assert!(registry.problems().is_empty(), "{:?}", registry.problems());
        assert!(registry.repositories()[0].bare);

        let response = log(&registry, log_args()).expect("logs");
        assert_eq!(response.returned, 4);
    }
}
