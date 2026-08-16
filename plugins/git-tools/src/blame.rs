//! `blame` — who last touched these lines, and in which commit.
//!
//! Blame is by far the most expensive thing this plugin does: libgit2 walks
//! history backwards re-diffing the file at every step.
//!
//! ### A line range bounds the answer, not the walk
//!
//! Worth stating first, because the opposite is the natural assumption.
//! `min_line` and `max_line` *are* passed down to libgit2, and on a repository
//! with deep history they save almost nothing. Two repositories, same code,
//! measured on one laptop:
//!
//! | Repository | Commits | Request | Wall clock |
//! | --- | --- | --- | --- |
//! | `tdcc-plugins` | 13 | whole 938-line file | 0.23 s |
//! | `tdcc-plugins` | 13 | lines 1–5 | 0.01 s |
//! | `tdcc-mesh` | 1994 | whole 217-line file | 11.4 s |
//! | `tdcc-mesh` | 1994 | lines 1–5 | 9.7 s |
//!
//! Blame cost is roughly *commits walked × the cost of one tree comparison*.
//! The second repository is slower on both counts, and narrowing 217 lines to 5
//! removed about a seventh of it. Treat these as one machine's numbers, not a
//! benchmark; the shape is what matters, and the shape is that the walk
//! dominates.
//!
//! So the two bounds that actually shorten the *work* are:
//!
//! 1. **File size**, `--max-blame-file-bytes`, checked before any history is
//!    read. A 40 MB generated file is not a question worth minutes of somebody
//!    else's CPU.
//! 2. **`oldest_rev`**, which stops the walk at a revision the caller names.
//!    On the 9.3 s call above, an `oldest_rev` recent enough to cover the lines
//!    asked about brought it to 0.02 s. Lines whose real origin is older are
//!    attributed to the boundary and flagged `boundary: true`, so a bounded
//!    answer is visibly a bounded answer rather than a wrong one.
//!
//! `--max-blame-lines` and the response byte budget still apply; they bound the
//! size of the reply, which is a different problem from bounding the work.
//!
//! The result is deduplicated: each line names a commit id, and the commit
//! metadata appears once in a `commits` map. A 500-line blame of a file with
//! four authors is four commit records, not five hundred.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tdcc_plugin::{PluginError, PluginResult};

use crate::changes::RevisionRef;
use crate::guard::parse_exact_tree_path;
use crate::render::{Budget, Identity, identity, message_text, short_oid, truncate_text};
use crate::repos::Registry;
use crate::resolve::{blob_at, required_revision, resolve_commit, revision_or_head};

/// Bytes of one line of file content a response will carry.
///
/// A minified bundle is one line of two megabytes; without this a single line
/// would consume the whole text budget and the rest of the blame would come
/// back empty.
pub const MAX_LINE_BYTES: usize = 512;

/// Count the lines in a file the way git does.
///
/// A trailing newline terminates the last line rather than starting a new one,
/// and a file with no trailing newline still has a final line. An empty file
/// has no lines at all.
pub fn count_lines(content: &[u8]) -> usize {
    if content.is_empty() {
        return 0;
    }
    let newlines = content.iter().filter(|byte| **byte == b'\n').count();
    if content.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    }
}

/// Split file content into lines, without the terminators.
fn split_lines(content: &[u8]) -> Vec<&[u8]> {
    if content.is_empty() {
        return Vec::new();
    }
    let trimmed = if content.last() == Some(&b'\n') {
        &content[..content.len() - 1]
    } else {
        content
    };
    trimmed
        .split(|byte| *byte == b'\n')
        .map(|line| match line.last() {
            // Tolerate CRLF without depending on how the operator's
            // core.autocrlf happens to be set.
            Some(b'\r') => &line[..line.len() - 1],
            _ => line,
        })
        .collect()
}

/// Clamp a caller's line range against the file and the operator's cap.
///
/// Returns the range plus the reason it was shortened, if it was. Separated out
/// so the arithmetic — which is where an off-by-one would silently misattribute
/// a line to the wrong commit — is testable without a repository.
pub fn clamp_range(
    total_lines: usize,
    start: Option<u32>,
    end: Option<u32>,
    max_lines: usize,
) -> Result<(usize, usize, Option<String>), String> {
    if total_lines == 0 {
        return Err(
            "that file is empty at that revision, so there is nothing to attribute".to_string(),
        );
    }

    let start = start.unwrap_or(1).max(1) as usize;
    if start > total_lines {
        return Err(format!(
            "start_line {start} is past the end of the file, which has {total_lines} lines at that \
             revision"
        ));
    }
    let end = match end {
        Some(value) => (value as usize).min(total_lines),
        None => total_lines,
    };
    if end < start {
        return Err(format!("end_line {end} is before start_line {start}"));
    }

    let requested = end - start + 1;
    if requested > max_lines {
        let capped = start + max_lines - 1;
        return Ok((
            start,
            capped,
            Some(format!(
                "the range was cut to {max_lines} lines, ending at line {capped}. Ask for the next \
                 range with start_line {}",
                capped + 1
            )),
        ));
    }
    Ok((start, end, None))
}

/// One attributed line.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BlameLine {
    /// Line number in the file at the requested revision.
    pub line: usize,
    /// Commit that last changed this line. Look it up in `commits`.
    pub commit: String,
    /// Line number this content had in the commit that introduced it.
    pub orig_line: usize,
    /// The file's path in that commit, when it differs from the one asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orig_path: Option<String>,
    /// True when the walk stopped here rather than proving this is where the
    /// line came from. Three things cause it: the named commit is the
    /// repository's root commit (which has no parent to look past), the caller
    /// set `oldest_rev`, or the repository is a shallow clone whose history is
    /// grafted. A boundary line means "no earlier than this", not "here".
    pub boundary: bool,
    /// The line itself, unless `include_line_text` was off or the node runs
    /// with `--no-content`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// A commit referenced by one or more attributed lines.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BlameCommit {
    pub short: String,
    pub summary: Option<String>,
    pub author: Identity,
}

/// Arguments for the `blame` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlameArgs {
    /// Which configured repository to read. Required only when the operator
    /// configured more than one; call `status` to list them.
    #[serde(default)]
    pub repo: Option<String>,
    /// The file to attribute, relative to the repository root. One exact
    /// path — globs are refused here.
    pub path: String,
    /// The revision the file is read at. Defaults to HEAD. Line numbers refer
    /// to the file as it exists at this revision.
    #[serde(default)]
    pub rev: Option<String>,
    /// Stop searching backwards at this revision instead of walking to the
    /// beginning of history. Lines whose real origin is older are attributed
    /// to this boundary and marked `boundary: true`. This is the only argument
    /// that makes a blame meaningfully cheaper on a file with deep history —
    /// a line range does not, because the walk is the same length either way.
    #[serde(default)]
    pub oldest_rev: Option<String>,
    /// First line to attribute, 1-based. Defaults to 1.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// Last line to attribute, inclusive. Defaults to the end of the file, or
    /// to whatever the operator's line cap allows.
    #[serde(default)]
    pub end_line: Option<u32>,
    /// Return the text of each attributed line alongside its commit. On by
    /// default; forced off when the node runs with `--no-content`.
    #[serde(default)]
    pub include_line_text: Option<bool>,
    /// Follow only the first parent of each merge, which attributes a line to
    /// the merge rather than to the branch it came from. Off by default.
    #[serde(default)]
    pub first_parent: Option<bool>,
    /// Ignore whitespace-only changes, so a reindentation does not take the
    /// blame for every line it touched. Off by default.
    #[serde(default)]
    pub ignore_whitespace: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlameResponse {
    pub repository: String,
    pub path: String,
    pub rev: RevisionRef,
    /// The boundary the search stopped at, when `oldest_rev` was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_rev: Option<RevisionRef>,
    /// Lines the file has at that revision.
    pub total_lines: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub lines: Vec<BlameLine>,
    /// Each commit named above, once.
    pub commits: BTreeMap<String, BlameCommit>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<String>,
}

pub fn blame(registry: &Registry, args: BlameArgs) -> PluginResult<BlameResponse> {
    let selected = registry.select(args.repo.as_deref())?;
    let limits = registry.limits();
    let disclosure = registry.disclosure();

    let path = parse_exact_tree_path(&args.path)
        .map_err(|error| PluginError::invalid_params(format!("path: {error}")))?;
    let revision = revision_or_head(args.rev.as_deref())?;
    let oldest = args
        .oldest_rev
        .as_deref()
        .map(|value| required_revision(value, "oldest_rev"))
        .transpose()?;

    let want_text = args.include_line_text.unwrap_or(true) && disclosure.content;
    if args.include_line_text == Some(true) && !disclosure.content {
        return Err(PluginError::invalid_request(
            "this node runs git-tools with --no-content, so line text is not available. Omit \
             include_line_text to get the attribution without it",
        ));
    }

    let repository = registry.open(selected)?;
    let commit = resolve_commit(&repository, &revision)?;
    let oldest_commit = oldest
        .as_ref()
        .map(|value| resolve_commit(&repository, value))
        .transpose()?;
    let blob = blob_at(&repository, &commit, &path)?;

    if blob.size() as u64 > limits.max_blame_file_bytes {
        return Err(PluginError::invalid_request(format!(
            "{} is {} bytes at that revision, over this node's {} byte blame limit. Blame cost \
             grows with file size and history depth, so the operator caps it with \
             --max-blame-file-bytes",
            path,
            blob.size(),
            limits.max_blame_file_bytes
        )));
    }
    if blob.is_binary() {
        return Err(PluginError::invalid_request(format!(
            "{path} is binary at that revision, and blame attributes text lines"
        )));
    }

    let content = blob.content().to_vec();
    let total_lines = count_lines(&content);
    let (start_line, end_line, range_reason) = clamp_range(
        total_lines,
        args.start_line,
        args.end_line,
        limits.max_blame_lines,
    )
    .map_err(PluginError::invalid_params)?;

    let mut options = git2::BlameOptions::new();
    options
        .newest_commit(commit.id())
        .min_line(start_line)
        .max_line(end_line)
        // Follow a line that moved within the same file, which is what makes a
        // blame survive a function being relocated.
        .track_copies_same_file(true)
        .first_parent(args.first_parent.unwrap_or(false))
        .ignore_whitespace(args.ignore_whitespace.unwrap_or(false));
    if let Some(oldest_commit) = &oldest_commit {
        options.oldest_commit(oldest_commit.id());
    }

    let attribution = repository
        .blame_file(std::path::Path::new(path.as_str()), Some(&mut options))
        .map_err(|error| PluginError::internal(format!("blame failed: {}", error.message())))?;

    let lines_text = want_text.then(|| split_lines(&content));
    let mut budget = Budget::new(limits.max_patch_bytes);
    let text_reason = format!(
        "line text stops at {} bytes; ask for a narrower line range",
        limits.max_patch_bytes
    );

    let mut lines = Vec::with_capacity(end_line - start_line + 1);
    let mut commits: BTreeMap<String, BlameCommit> = BTreeMap::new();
    let mut text_truncated = false;

    for line_number in start_line..=end_line {
        let Some(hunk) = attribution.get_line(line_number) else {
            // libgit2 produced no hunk for this line. Reporting the rest is
            // more useful than failing the whole call, and the gap is visible
            // because the line simply is not in the list.
            continue;
        };

        let final_id = hunk.final_commit_id();
        let key = final_id.to_string();
        if !commits.contains_key(&key)
            && let Ok(source) = repository.find_commit(final_id)
        {
            commits.insert(
                key.clone(),
                BlameCommit {
                    short: short_oid(final_id),
                    summary: source
                        .summary_bytes()
                        .map(message_text)
                        .map(|text| truncate_text(&text, 1_024).0),
                    author: identity(&source.author(), disclosure),
                },
            );
        }

        let orig_path = hunk
            .path()
            .map(|value| value.to_string_lossy().replace('\\', "/"))
            .filter(|value| value != path.as_str());
        let orig_line = hunk.orig_start_line() + (line_number - hunk.final_start_line());

        let text = lines_text.as_ref().and_then(|all| {
            let raw = all.get(line_number - 1)?;
            let rendered = String::from_utf8_lossy(raw);
            let (clipped, _) = truncate_text(&rendered, MAX_LINE_BYTES);
            if budget.is_exhausted() {
                text_truncated = true;
                return None;
            }
            let mut sink = String::new();
            if !budget.push_str(&mut sink, &clipped, &text_reason) {
                text_truncated = true;
            }
            Some(sink)
        });

        lines.push(BlameLine {
            line: line_number,
            commit: key,
            orig_line,
            orig_path,
            boundary: hunk.is_boundary(),
            text,
        });
    }

    let truncated_reason = range_reason.or_else(|| {
        text_truncated
            .then(|| budget.reason().map(str::to_string))
            .flatten()
    });

    Ok(BlameResponse {
        repository: selected.alias.clone(),
        path: path.as_str().to_string(),
        rev: RevisionRef::new(revision.as_str(), &commit),
        oldest_rev: oldest
            .as_ref()
            .zip(oldest_commit.as_ref())
            .map(|(value, commit)| RevisionRef::new(value.as_str(), commit)),
        total_lines,
        start_line,
        end_line,
        lines,
        commits,
        truncated: truncated_reason.is_some(),
        truncated_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Disclosure, Limits, RepoSpec};
    use crate::testsupport::{RepoFixture, TempTree};

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

    fn blame_args(path: &str) -> BlameArgs {
        BlameArgs {
            repo: None,
            path: path.to_string(),
            rev: None,
            oldest_rev: None,
            start_line: None,
            end_line: None,
            include_line_text: None,
            first_parent: None,
            ignore_whitespace: None,
        }
    }

    #[test]
    fn line_counting_matches_gits_definition() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"one\n"), 1);
        assert_eq!(count_lines(b"one"), 1);
        assert_eq!(count_lines(b"one\ntwo\n"), 2);
        assert_eq!(count_lines(b"one\ntwo"), 2);
        assert_eq!(count_lines(b"\n"), 1);
        assert_eq!(count_lines(b"\n\n"), 2);
    }

    #[test]
    fn splitting_lines_drops_terminators_including_crlf() {
        assert_eq!(split_lines(b"one\ntwo\n"), vec![&b"one"[..], &b"two"[..]]);
        assert_eq!(split_lines(b"one\r\ntwo"), vec![&b"one"[..], &b"two"[..]]);
        assert_eq!(split_lines(b""), Vec::<&[u8]>::new());
        assert_eq!(split_lines(b"solo"), vec![&b"solo"[..]]);
    }

    #[test]
    fn a_range_defaults_to_the_whole_file_and_clamps_to_it() {
        assert_eq!(
            clamp_range(10, None, None, 100).expect("whole file"),
            (1, 10, None)
        );
        assert_eq!(
            clamp_range(10, Some(3), Some(99), 100).expect("clamped to the end"),
            (3, 10, None)
        );
        assert_eq!(
            clamp_range(10, Some(0), None, 100).expect("zero means one"),
            (1, 10, None)
        );
    }

    #[test]
    fn a_range_over_the_line_cap_is_cut_and_points_at_the_next_page() {
        let (start, end, reason) = clamp_range(1_000, Some(1), Some(1_000), 10).expect("capped");
        assert_eq!((start, end), (1, 10));
        let reason = reason.expect("a reason");
        assert!(reason.contains("10 lines"), "{reason}");
        assert!(reason.contains("start_line 11"), "{reason}");
    }

    #[test]
    fn an_impossible_range_is_an_error_naming_the_real_length() {
        let error = clamp_range(5, Some(50), None, 100).expect_err("past the end");
        assert!(error.contains("has 5 lines"), "{error}");

        let error = clamp_range(5, Some(4), Some(2), 100).expect_err("backwards");
        assert!(error.contains("before start_line"), "{error}");

        let error = clamp_range(0, None, None, 100).expect_err("empty file");
        assert!(error.contains("empty"), "{error}");
    }

    #[test]
    fn each_line_is_attributed_to_the_commit_that_last_changed_it() {
        let tree = TempTree::new("blame-basic");
        let fixture = tree.repository("repo");
        fixture.write("src/lib.rs", "one\ntwo\nthree\n");
        let first = fixture.commit_as("Ada Lovelace", "ada@example.org", "initial");
        fixture.write("src/lib.rs", "one\nCHANGED\nthree\n");
        let second = fixture.commit_as("Grace Hopper", "grace@example.org", "change the middle");

        let registry = registry_for(&fixture);
        let response = blame(&registry, blame_args("src/lib.rs")).expect("blames");

        assert_eq!(response.total_lines, 3);
        assert_eq!((response.start_line, response.end_line), (1, 3));
        assert_eq!(response.lines.len(), 3);
        assert!(!response.truncated);

        assert_eq!(response.lines[0].commit, first.to_string());
        assert_eq!(response.lines[1].commit, second.to_string());
        assert_eq!(response.lines[2].commit, first.to_string());

        assert_eq!(response.lines[1].text.as_deref(), Some("CHANGED"));

        // Two commits, named once each, not once per line.
        assert_eq!(response.commits.len(), 2);
        assert_eq!(
            response.commits[&second.to_string()].author.name,
            "Grace Hopper"
        );
        assert_eq!(
            response.commits[&first.to_string()].summary.as_deref(),
            Some("initial")
        );
    }

    #[test]
    fn a_line_range_narrows_the_answer_to_exactly_that_range() {
        let tree = TempTree::new("blame-range");
        let fixture = tree.repository("repo");
        let body: String = (1..=50).map(|index| format!("line {index}\n")).collect();
        fixture.write("big.txt", &body);
        fixture.commit("initial");

        let registry = registry_for(&fixture);
        let mut args = blame_args("big.txt");
        args.start_line = Some(10);
        args.end_line = Some(14);
        let response = blame(&registry, args).expect("blames");

        assert_eq!(response.total_lines, 50);
        assert_eq!((response.start_line, response.end_line), (10, 14));
        let numbers: Vec<usize> = response.lines.iter().map(|line| line.line).collect();
        assert_eq!(numbers, vec![10, 11, 12, 13, 14]);
        assert_eq!(response.lines[0].text.as_deref(), Some("line 10"));
    }

    #[test]
    fn a_range_over_the_operators_cap_is_shortened_and_says_so() {
        let tree = TempTree::new("blame-cap");
        let fixture = tree.repository("repo");
        let body: String = (1..=50).map(|index| format!("line {index}\n")).collect();
        fixture.write("big.txt", &body);
        fixture.commit("initial");

        let registry = registry_with(
            &fixture,
            Limits {
                max_blame_lines: 5,
                ..Limits::default()
            },
            Disclosure::default(),
        );
        let response = blame(&registry, blame_args("big.txt")).expect("blames");

        assert_eq!(response.lines.len(), 5);
        assert_eq!(response.end_line, 5);
        assert!(response.truncated);
        assert!(
            response
                .truncated_reason
                .expect("reason")
                .contains("start_line 6")
        );
    }

    #[test]
    fn a_file_over_the_size_limit_is_refused_before_any_history_is_walked() {
        let tree = TempTree::new("blame-size");
        let fixture = tree.repository("repo");
        let body: String = (0..5_000).map(|index| format!("line {index}\n")).collect();
        fixture.write("big.txt", &body);
        fixture.commit("initial");

        let registry = registry_with(
            &fixture,
            Limits {
                max_blame_file_bytes: 1_024,
                ..Limits::default()
            },
            Disclosure::default(),
        );
        let error = blame(&registry, blame_args("big.txt")).expect_err("too big");
        let message = format!("{error:?}");
        assert!(message.contains("--max-blame-file-bytes"), "{message}");
    }

    #[test]
    fn a_binary_file_is_refused_rather_than_attributed_as_gibberish() {
        let tree = TempTree::new("blame-binary");
        let fixture = tree.repository("repo");
        std::fs::create_dir_all(fixture.root()).expect("root");
        std::fs::write(fixture.root().join("blob.bin"), [0u8, 1, 2, 0, 3]).expect("write");
        fixture.commit("initial");

        let registry = registry_for(&fixture);
        let error = blame(&registry, blame_args("blob.bin")).expect_err("binary");
        assert!(format!("{error:?}").contains("binary"));
    }

    #[test]
    fn line_text_is_withheld_on_request_and_refused_under_no_content() {
        let tree = TempTree::new("blame-content");
        let fixture = tree.repository("repo");
        fixture.write("src/lib.rs", "secret = 1\n");
        fixture.commit("initial");

        let registry = registry_for(&fixture);
        let mut args = blame_args("src/lib.rs");
        args.include_line_text = Some(false);
        let response = blame(&registry, args).expect("blames");
        assert_eq!(response.lines.len(), 1);
        assert!(response.lines[0].text.is_none());

        let closed = registry_with(
            &fixture,
            Limits::default(),
            Disclosure {
                content: false,
                redact_emails: false,
            },
        );
        // Attribution without text still works...
        let response = blame(&closed, blame_args("src/lib.rs")).expect("blames");
        assert!(response.lines[0].text.is_none());
        // ...and asking for text is an error rather than a silent omission.
        let mut args = blame_args("src/lib.rs");
        args.include_line_text = Some(true);
        let error = blame(&closed, args).expect_err("content is off");
        assert!(format!("{error:?}").contains("--no-content"));
    }

    #[test]
    fn a_glob_is_refused_where_blame_needs_one_file() {
        let tree = TempTree::new("blame-glob");
        let fixture = tree.repository("repo");
        fixture.write("src/lib.rs", "one\n");
        fixture.commit("initial");
        let registry = registry_for(&fixture);

        let error = blame(&registry, blame_args("src/*.rs")).expect_err("glob");
        let message = format!("{error:?}");
        assert!(message.contains("path:"), "{message}");
        assert!(message.contains("exact path"), "{message}");
    }

    #[test]
    fn a_traversal_path_never_reaches_the_repository() {
        let tree = TempTree::new("blame-traversal");
        let fixture = tree.repository("repo");
        fixture.write("src/lib.rs", "one\n");
        fixture.commit("initial");
        let registry = registry_for(&fixture);

        for hostile in ["../../etc/passwd", "/etc/passwd", r"C:\Windows\win.ini"] {
            let error = blame(&registry, blame_args(hostile)).expect_err("refused");
            assert!(format!("{error:?}").contains("path:"), "{hostile}");
        }
    }

    #[test]
    fn blaming_an_older_revision_uses_that_revisions_line_numbers() {
        let tree = TempTree::new("blame-historic");
        let fixture = tree.repository("repo");
        fixture.write("src/lib.rs", "one\ntwo\n");
        let first = fixture.commit("initial");
        fixture.write("src/lib.rs", "zero\none\ntwo\n");
        fixture.commit("prepend");

        let registry = registry_for(&fixture);
        let mut args = blame_args("src/lib.rs");
        args.rev = Some("HEAD~1".to_string());
        let response = blame(&registry, args).expect("blames");

        assert_eq!(response.total_lines, 2);
        assert_eq!(response.lines[0].text.as_deref(), Some("one"));
        assert!(
            response
                .lines
                .iter()
                .all(|line| line.commit == first.to_string())
        );
    }

    #[test]
    fn oldest_rev_stops_the_walk_and_flags_the_lines_it_could_not_reach() {
        let tree = TempTree::new("blame-oldest");
        let fixture = tree.repository("repo");
        fixture.write(
            "src/lib.rs",
            "a
",
        );
        let first = fixture.commit("first line");
        fixture.write(
            "src/lib.rs",
            "a
b
",
        );
        let second = fixture.commit("second line");
        fixture.write(
            "src/lib.rs",
            "a
b
c
",
        );
        let third = fixture.commit("third line");

        let registry = registry_for(&fixture);

        let open = blame(&registry, blame_args("src/lib.rs")).expect("blames");
        assert_eq!(open.lines.len(), 3);
        assert!(open.oldest_rev.is_none());
        assert_eq!(open.lines[0].commit, first.to_string());
        assert_eq!(open.lines[1].commit, second.to_string());
        assert_eq!(open.lines[2].commit, third.to_string());
        // The root commit is itself a boundary: there is no parent to look
        // past. Lines from later commits are not.
        assert!(open.lines[0].boundary);
        assert!(!open.lines[1].boundary);
        assert!(!open.lines[2].boundary);

        // Stopping at the second commit means line 1 cannot be traced to its
        // real origin. It is attributed to the boundary and flagged, rather
        // than being silently credited to the wrong author.
        let mut args = blame_args("src/lib.rs");
        args.oldest_rev = Some(second.to_string());
        let bounded = blame(&registry, args).expect("blames");

        let reported = bounded.oldest_rev.expect("the boundary is reported");
        assert_eq!(reported.commit, second.to_string());
        assert_eq!(bounded.lines[0].commit, second.to_string());
        assert!(bounded.lines[0].boundary);
        // The line that genuinely came later is unaffected by the boundary.
        assert_eq!(bounded.lines[2].commit, third.to_string());
        assert!(!bounded.lines[2].boundary);
    }

    #[test]
    fn a_hostile_oldest_rev_is_refused_and_names_its_field() {
        let tree = TempTree::new("blame-oldest-guard");
        let fixture = tree.repository("repo");
        fixture.write(
            "src/lib.rs",
            "one
",
        );
        fixture.commit("initial");
        let registry = registry_for(&fixture);

        let mut args = blame_args("src/lib.rs");
        args.oldest_rev = Some("--upload-pack=x".to_string());
        let error = blame(&registry, args).expect_err("refused");
        let message = format!("{error:?}");
        assert!(message.contains("oldest_rev"), "{message}");
    }

    #[test]
    fn a_missing_file_at_that_revision_says_which_tool_finds_out_why() {
        let tree = TempTree::new("blame-missing");
        let fixture = tree.repository("repo");
        fixture.write("src/lib.rs", "one\n");
        fixture.commit("initial");
        let registry = registry_for(&fixture);

        let error = blame(&registry, blame_args("src/gone.rs")).expect_err("not there");
        let message = format!("{error:?}");
        assert!(
            message.contains("does not exist at that revision"),
            "{message}"
        );
        assert!(message.contains("log"), "{message}");
    }
}
