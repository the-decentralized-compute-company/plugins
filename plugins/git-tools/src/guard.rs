//! Everything a caller can type, validated before it reaches libgit2.
//!
//! This plugin runs on hardware that may not belong to the person asking the
//! question, and every argument here arrives from a model. Two kinds of string
//! get validated:
//!
//! 1. **Revisions** — `HEAD`, `v1.4.0`, `refs/heads/main`, `a1b2c3d`,
//!    `HEAD~5`. [`parse_revision`] is the only way to build a [`Revision`], and
//!    a [`Revision`] is the only thing the rest of the crate passes to
//!    `revparse_single`. That makes "no unvalidated revision reaches git" a
//!    property of the type system rather than of somebody remembering.
//! 2. **Tree paths** — `src/main.rs`, `crates/*/Cargo.toml`. These address
//!    entries inside a git tree, not the filesystem, but they are sanitized on
//!    the same rules a filesystem path would get, because a path that cannot
//!    escape today is one refactor away from one that can.
//!
//! ### Why a leading `-` is refused even though nothing here builds a command
//!
//! This plugin uses libgit2 in-process; there is no `git` subprocess and no
//! argument vector, so `--upload-pack=…` in a ref name has nothing to inject
//! into. The rule is here anyway because it costs one comparison and it means
//! the guarantee survives the day somebody adds a backend that *does* shell
//! out. A validator that is only correct given the current backend is a trap.
//!
//! ### What is refused and why
//!
//! | Rejected | Reason |
//! | --- | --- |
//! | `:` anywhere | `HEAD:path` addresses a blob and `:/text` runs a message search — neither is something a `log` or `diff` argument needs |
//! | `{` and `}` | Removes `HEAD@{2}`, `@{upstream}`, and `HEAD^{/regex}` in one rule; the last is a caller-supplied regex over every commit message |
//! | `..` | Ranges are expressed as two separate arguments, so a single revision never needs one |
//! | leading `-` | See above |
//! | anything outside the allowed character set | Whitespace, control characters, NUL, and non-ASCII cannot appear in a git ref name |

use std::fmt;

/// The two kinds of value this module validates. Used in error text and to
/// pick the right advice for a shared rule.
pub const REVISION: &str = "a revision";
pub const PATH: &str = "a path";

/// Longest revision accepted. Real ref names are far shorter; a long one is
/// either a mistake or an attempt to find a buffer.
pub const MAX_REVISION_LEN: usize = 200;
/// Longest tree path accepted, matching the practical limit git itself imposes
/// on index entries.
pub const MAX_TREE_PATH_LEN: usize = 1_024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardError {
    /// The value was empty or only whitespace.
    Empty { what: &'static str },
    /// The value was longer than the limit for its kind.
    TooLong {
        what: &'static str,
        length: usize,
        limit: usize,
    },
    /// A revision starting with `-` would be an option to any program that
    /// took it as an argument.
    LeadingDash,
    /// `rev:path` and `:/message-search` syntax.
    ColonSyntax,
    /// `@{…}` reflog, upstream, and `^{/regex}` search syntax.
    BraceSyntax,
    /// A `..` segment. `what` distinguishes the two callers, because the
    /// advice differs: a revision wants a second argument, a path wants to
    /// stay inside the repository.
    Traversal { what: &'static str },
    /// An absolute or rooted path.
    Absolute,
    /// A path segment containing `:` — a Windows drive letter or an NTFS
    /// alternate data stream. Refused everywhere so behaviour does not fork
    /// per operating system.
    Reserved,
    /// A pathspec beginning with `:`, which is git's "magic pathspec" prefix
    /// (`:(exclude)`, `:!`, `:/`).
    PathspecMagic,
    /// A glob metacharacter where an exact path was required.
    GlobNotAllowed,
    /// A character outside the allowed set for this kind of value.
    DisallowedCharacter { what: &'static str, character: char },
    /// More list entries than the caller is allowed to supply.
    TooMany { what: &'static str, limit: usize },
}

impl fmt::Display for GuardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { what } => write!(formatter, "{what} must not be empty"),
            Self::TooLong {
                what,
                length,
                limit,
            } => write!(
                formatter,
                "{what} is {length} characters long, which is over the {limit} character limit"
            ),
            Self::LeadingDash => write!(
                formatter,
                "a revision must not start with '-'; that is an option, not a ref"
            ),
            Self::ColonSyntax => write!(
                formatter,
                "a revision must not contain ':'; 'rev:path' and ':/text' syntax are refused. \
                 Name a commit, branch, or tag, and use the path arguments to scope the result"
            ),
            Self::BraceSyntax => write!(
                formatter,
                "a revision must not contain '{{' or '}}'; reflog ('HEAD@{{1}}'), upstream \
                 ('@{{u}}'), and message-search ('HEAD^{{/text}}') syntax are refused"
            ),
            Self::Traversal { what } if *what == REVISION => write!(
                formatter,
                "a revision must not contain '..'; express a range as two separate arguments                  rather than one range string"
            ),
            Self::Traversal { .. } => {
                write!(formatter, "a path must not contain a '..' segment")
            }
            Self::Absolute => write!(
                formatter,
                "a path must be relative to the repository root, not absolute"
            ),
            Self::Reserved => write!(
                formatter,
                "a path must not contain ':' (drive letters and alternate data streams are \
                 refused)"
            ),
            Self::PathspecMagic => write!(
                formatter,
                "a path must not start with ':'; git's magic pathspec prefixes are refused"
            ),
            Self::GlobNotAllowed => write!(
                formatter,
                "this argument needs one exact path, not a glob: '*', '?', '[' and ']' are refused \
                 here"
            ),
            Self::DisallowedCharacter { what, character } => write!(
                formatter,
                "{what} contains {character:?}, which is not allowed here"
            ),
            Self::TooMany { what, limit } => {
                write!(formatter, "at most {limit} {what} may be supplied at once")
            }
        }
    }
}

impl std::error::Error for GuardError {}

/// A revision string that has passed [`parse_revision`].
///
/// Nothing else in this crate constructs one, and nothing in this crate calls
/// `revparse_single` with anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision(String);

impl Revision {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Characters a revision may contain.
///
/// Enough for every ref name git itself accepts plus the two navigation
/// operators worth keeping — `~` for "n generations back" and `^` for "this
/// parent". Deliberately excludes `:` and the braces; see the module docs.
fn is_revision_character(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || matches!(character, '.' | '_' | '-' | '/' | '+' | '~' | '^')
}

/// Validate a caller-supplied revision.
///
/// Surrounding whitespace is trimmed first, because a model that pastes a ref
/// out of a previous response often brings a newline with it. Whitespace
/// *inside* the value is still an error.
pub fn parse_revision(input: &str) -> Result<Revision, GuardError> {
    const WHAT: &str = REVISION;

    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(GuardError::Empty { what: WHAT });
    }
    if trimmed.len() > MAX_REVISION_LEN {
        return Err(GuardError::TooLong {
            what: WHAT,
            length: trimmed.len(),
            limit: MAX_REVISION_LEN,
        });
    }
    if trimmed.starts_with('-') {
        return Err(GuardError::LeadingDash);
    }
    if trimmed.contains(':') {
        return Err(GuardError::ColonSyntax);
    }
    if trimmed.contains('{') || trimmed.contains('}') {
        return Err(GuardError::BraceSyntax);
    }
    if trimmed.contains("..") {
        return Err(GuardError::Traversal { what: WHAT });
    }
    if let Some(character) = trimmed.chars().find(|c| !is_revision_character(*c)) {
        return Err(GuardError::DisallowedCharacter {
            what: WHAT,
            character,
        });
    }
    Ok(Revision(trimmed.to_string()))
}

/// A path inside a git tree that has passed [`parse_tree_path`].
///
/// Always `/`-separated, never absolute, never containing `..`, so it means the
/// same thing whichever platform the node runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreePath(String);

impl TreePath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TreePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn is_glob_character(character: char) -> bool {
    matches!(character, '*' | '?' | '[' | ']')
}

/// Validate a caller-supplied pathspec.
///
/// Backslashes are treated as separators so a Windows-shaped `src\main.rs`
/// works. `.` and empty segments are dropped; `..` is refused outright rather
/// than normalized away, on the same reasoning `code-context` gives: accepting
/// it means the resolver has to agree with something else about what `..`
/// means, and that is the bug class this function exists to avoid.
///
/// The result may still contain `*`, `?` and `[…]`, which libgit2 matches with
/// fnmatch semantics. Use [`parse_exact_tree_path`] where a glob would be
/// wrong.
pub fn parse_tree_path(input: &str) -> Result<TreePath, GuardError> {
    const WHAT: &str = PATH;

    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(GuardError::Empty { what: WHAT });
    }
    if trimmed.len() > MAX_TREE_PATH_LEN {
        return Err(GuardError::TooLong {
            what: WHAT,
            length: trimmed.len(),
            limit: MAX_TREE_PATH_LEN,
        });
    }
    // Checked before the separator rewrite so `:(exclude)src` is named
    // precisely rather than falling through to the drive-letter rule.
    if trimmed.starts_with(':') {
        return Err(GuardError::PathspecMagic);
    }
    if let Some(character) = trimmed
        .chars()
        .find(|c| c.is_control() || *c == '\0' || *c == '\n' || *c == '\r')
    {
        return Err(GuardError::DisallowedCharacter {
            what: WHAT,
            character,
        });
    }

    let normalized = trimmed.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(GuardError::Absolute);
    }

    let mut segments: Vec<&str> = Vec::new();
    for segment in normalized.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return Err(GuardError::Traversal { what: WHAT }),
            _ => {}
        }
        if segment.contains(':') {
            return Err(GuardError::Reserved);
        }
        segments.push(segment);
    }
    if segments.is_empty() {
        return Err(GuardError::Empty { what: WHAT });
    }
    Ok(TreePath(segments.join("/")))
}

/// [`parse_tree_path`], plus a refusal of glob metacharacters.
///
/// `blame` addresses exactly one file, so a pattern there is a caller mistake
/// worth naming rather than something to resolve arbitrarily.
pub fn parse_exact_tree_path(input: &str) -> Result<TreePath, GuardError> {
    let path = parse_tree_path(input)?;
    if path.as_str().chars().any(is_glob_character) {
        return Err(GuardError::GlobNotAllowed);
    }
    Ok(path)
}

/// Validate a whole list of pathspecs, bounded in length.
pub fn parse_tree_paths(inputs: &[String], limit: usize) -> Result<Vec<TreePath>, GuardError> {
    if inputs.len() > limit {
        return Err(GuardError::TooMany {
            what: "paths",
            limit,
        });
    }
    inputs.iter().map(|input| parse_tree_path(input)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_revisions_survive_unchanged() {
        for input in [
            "HEAD",
            "main",
            "v1.4.0",
            "refs/heads/main",
            "refs/tags/v1.4.0",
            "origin/main",
            "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
            "HEAD~5",
            "HEAD^",
            "HEAD^2~3",
            "release/2024-06",
            "feature/thing+extra",
            "_internal",
        ] {
            let revision = parse_revision(input).unwrap_or_else(|error| {
                panic!("{input:?} is a legitimate revision but was refused: {error}")
            });
            assert_eq!(revision.as_str(), input);
        }
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_but_inner_whitespace_is_not_allowed() {
        assert_eq!(parse_revision("  HEAD\n").expect("trims").as_str(), "HEAD");
        assert_eq!(
            parse_revision("HEAD 5"),
            Err(GuardError::DisallowedCharacter {
                what: "a revision",
                character: ' '
            })
        );
    }

    /// The argument-injection shape. Nothing here builds a command line today,
    /// which is exactly why the refusal has to be pinned by a test: it must
    /// survive a backend change nobody remembers to re-audit.
    #[test]
    fn a_revision_starting_with_a_dash_is_refused() {
        for input in [
            "--upload-pack=touch /tmp/pwned",
            "-o/tmp/out",
            "--output=x",
            "-",
            "--",
        ] {
            assert_eq!(
                parse_revision(input),
                Err(GuardError::LeadingDash),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn blob_addressing_and_message_search_syntax_are_refused() {
        for input in [
            "HEAD:/etc/passwd",
            "HEAD:src/main.rs",
            ":/fix the bug",
            "v1:x",
        ] {
            assert_eq!(
                parse_revision(input),
                Err(GuardError::ColonSyntax),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn reflog_upstream_and_regex_search_syntax_are_refused() {
        for input in [
            "HEAD@{1}",
            "@{upstream}",
            "HEAD^{/leak}",
            "main@{2.days.ago}",
        ] {
            assert_eq!(
                parse_revision(input),
                Err(GuardError::BraceSyntax),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn range_syntax_in_a_single_revision_is_refused() {
        for input in ["v1.0.0..v2.0.0", "main...topic", ".."] {
            assert_eq!(
                parse_revision(input),
                Err(GuardError::Traversal { what: REVISION }),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn a_revision_may_not_be_empty_or_absurdly_long() {
        assert_eq!(
            parse_revision("   "),
            Err(GuardError::Empty { what: "a revision" })
        );
        let long = "a".repeat(MAX_REVISION_LEN + 1);
        assert_eq!(
            parse_revision(&long),
            Err(GuardError::TooLong {
                what: "a revision",
                length: MAX_REVISION_LEN + 1,
                limit: MAX_REVISION_LEN
            })
        );
    }

    #[test]
    fn control_characters_and_non_ascii_are_refused_in_a_revision() {
        for input in [
            "ma\0in", "ma\nin", "ma\tin", "brünn", "ma;in", "ma|in", "ma$in",
        ] {
            assert!(
                matches!(
                    parse_revision(input),
                    Err(GuardError::DisallowedCharacter { .. })
                ),
                "input {input:?} was not refused"
            );
        }
    }

    #[test]
    fn ordinary_paths_normalize_to_forward_slashes() {
        assert_eq!(
            parse_tree_path("src/main.rs").expect("plain").as_str(),
            "src/main.rs"
        );
        assert_eq!(
            parse_tree_path(r"src\util\mod.rs")
                .expect("windows")
                .as_str(),
            "src/util/mod.rs"
        );
        assert_eq!(
            parse_tree_path("./src//main.rs")
                .expect("redundant")
                .as_str(),
            "src/main.rs"
        );
        assert_eq!(
            parse_tree_path("crates/*/Cargo.toml")
                .expect("glob")
                .as_str(),
            "crates/*/Cargo.toml"
        );
    }

    #[test]
    fn path_traversal_is_refused_even_when_it_would_land_back_inside() {
        for input in [
            "..",
            "../../etc/passwd",
            "src/../../secrets",
            "src/../lib.rs",
        ] {
            assert_eq!(
                parse_tree_path(input),
                Err(GuardError::Traversal { what: PATH }),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn absolute_rooted_and_drive_prefixed_paths_are_refused() {
        assert_eq!(parse_tree_path("/etc/passwd"), Err(GuardError::Absolute));
        assert_eq!(
            parse_tree_path(r"\Windows\System32"),
            Err(GuardError::Absolute)
        );
        assert_eq!(parse_tree_path(r"C:\Windows"), Err(GuardError::Reserved));
        assert_eq!(
            parse_tree_path("notes.txt:stream"),
            Err(GuardError::Reserved)
        );
    }

    #[test]
    fn magic_pathspec_prefixes_are_refused() {
        for input in [":(exclude)src", ":!src", ":/", ":(top)"] {
            assert_eq!(
                parse_tree_path(input),
                Err(GuardError::PathspecMagic),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn a_path_that_is_only_separators_is_empty_rather_than_the_repository_root() {
        // An empty pathspec would silently mean "everything", which is the
        // opposite of what a caller passing a path filter asked for.
        for input in [".", "./", "././."] {
            assert_eq!(
                parse_tree_path(input),
                Err(GuardError::Empty { what: "a path" }),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn blame_refuses_a_glob_where_it_needs_one_file() {
        assert_eq!(
            parse_exact_tree_path("src/main.rs")
                .expect("exact")
                .as_str(),
            "src/main.rs"
        );
        for input in ["src/*.rs", "src/main.?s", "src/[abc].rs"] {
            assert_eq!(
                parse_exact_tree_path(input),
                Err(GuardError::GlobNotAllowed),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn a_path_list_is_bounded() {
        let many: Vec<String> = (0..5).map(|index| format!("src/{index}.rs")).collect();
        assert_eq!(
            parse_tree_paths(&many, 8).expect("under the limit").len(),
            5
        );
        assert_eq!(
            parse_tree_paths(&many, 4),
            Err(GuardError::TooMany {
                what: "paths",
                limit: 4
            })
        );
    }

    #[test]
    fn one_bad_entry_fails_the_whole_path_list() {
        let paths = vec!["src/main.rs".to_string(), "../../etc/passwd".to_string()];
        assert_eq!(
            parse_tree_paths(&paths, 8),
            Err(GuardError::Traversal { what: PATH })
        );
    }

    #[test]
    fn every_error_renders_a_sentence_naming_what_to_do() {
        // Error text is what a model reads before retrying, so it is part of
        // the contract rather than debug output.
        let rendered = [
            GuardError::LeadingDash.to_string(),
            GuardError::ColonSyntax.to_string(),
            GuardError::BraceSyntax.to_string(),
            GuardError::Traversal { what: REVISION }.to_string(),
            GuardError::Traversal { what: PATH }.to_string(),
            GuardError::PathspecMagic.to_string(),
            GuardError::GlobNotAllowed.to_string(),
        ];
        for message in rendered {
            assert!(message.len() > 20, "error text too terse: {message}");
            assert!(!message.contains("{}"), "unformatted braces in: {message}");
        }
    }
}
