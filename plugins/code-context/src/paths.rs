//! Root confinement.
//!
//! This is the security core of the plugin. It runs on hardware that may not
//! belong to the person asking the question, so every path that arrives in a
//! tool argument goes through [`resolve_within`] before anything touches the
//! filesystem, and the answer is only ever a path that is provably inside the
//! configured root.
//!
//! Two independent layers, because either one alone is bypassable:
//!
//! 1. [`sanitize_relative`] is *lexical*. It rejects absolute paths, roots,
//!    drive prefixes and `..` before a syscall happens, so a traversal attempt
//!    never even becomes a `stat`.
//! 2. [`resolve_within`] is *physical*. It canonicalizes the joined path —
//!    which resolves symlinks, junctions and `.` — and then re-checks
//!    containment. A symlink inside the root that points outside it fails here.
//!
//! The indexer adds a third: it walks with `follow_links(false)` and only
//! records regular files, so a symlink is never even a candidate.

use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// The path was absolute or rooted (`/etc/passwd`, `\\server\share`).
    Absolute,
    /// The path contained a `..` segment.
    Traversal,
    /// A segment contained `:` — a Windows drive letter or an NTFS alternate
    /// data stream. Rejected everywhere so behaviour does not fork per OS.
    Reserved,
    /// The path resolved, but outside the configured root. This is the symlink
    /// and junction case.
    Escaped,
    /// Nothing exists at that path inside the root.
    NotFound,
    /// The path exists but could not be resolved (permissions, I/O).
    Unresolvable(String),
}

impl fmt::Display for PathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absolute => write!(
                formatter,
                "path must be relative to the configured root, not absolute"
            ),
            Self::Traversal => write!(formatter, "path must not contain a '..' segment"),
            Self::Reserved => write!(
                formatter,
                "path must not contain ':' (drive letters and alternate data streams are refused)"
            ),
            Self::Escaped => write!(
                formatter,
                "path resolves outside the configured root and was refused"
            ),
            Self::NotFound => write!(formatter, "no such path inside the configured root"),
            Self::Unresolvable(reason) => write!(formatter, "path could not be resolved: {reason}"),
        }
    }
}

impl std::error::Error for PathError {}

/// Turn caller-supplied text into a relative path made only of plain segments.
///
/// Backslashes are treated as separators on every platform so a Windows-shaped
/// `src\main.rs` works; the cost is that a file whose name literally contains a
/// backslash is unreachable, which is the right trade for a code index.
///
/// `..` is refused outright rather than normalized away. `a/../b` stays inside
/// the root, but accepting it means the resolver has to agree with the
/// filesystem about what `..` means across symlinks, and that is exactly the
/// class of bug this function exists to avoid.
///
/// An empty result is legal and means "the root itself".
pub fn sanitize_relative(input: &str) -> Result<PathBuf, PathError> {
    let normalized = input.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(PathError::Absolute);
    }

    let mut relative = PathBuf::new();
    for segment in normalized.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return Err(PathError::Traversal),
            _ => {}
        }
        if segment.contains(':') {
            return Err(PathError::Reserved);
        }
        relative.push(segment);
    }
    Ok(relative)
}

/// [`sanitize_relative`] rendered back as the `/`-separated key the index uses.
///
/// An empty result means the root itself, which is legal for `tree` and not for
/// `read`; the caller decides.
pub fn normalize_relative(input: &str) -> Result<String, PathError> {
    Ok(join_components(&sanitize_relative(input)?))
}

fn join_components(path: &Path) -> String {
    let mut rendered = String::new();
    for component in path.components() {
        if !rendered.is_empty() {
            rendered.push('/');
        }
        rendered.push_str(&component.as_os_str().to_string_lossy());
    }
    rendered
}

/// Component-wise containment test.
///
/// Component-wise, not string-prefix: `/srv/repo-backup` must not count as
/// being inside `/srv/repo`, and a textual `starts_with` would say it is.
/// Both arguments must already be canonical for this to mean anything.
pub fn is_within(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

/// Resolve `input` against an already-canonical `root` and prove the result is
/// inside it.
///
/// The returned path is canonical, so callers can hand it straight to the
/// filesystem. Errors deliberately carry no absolute path: telling a caller
/// where the root lives on the contributor's disk is a disclosure this plugin
/// has no reason to make.
pub fn resolve_within(root: &Path, input: &str) -> Result<PathBuf, PathError> {
    let relative = sanitize_relative(input)?;
    let joined = root.join(&relative);
    let resolved = std::fs::canonicalize(&joined).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => PathError::NotFound,
        _ => PathError::Unresolvable(error.kind().to_string()),
    })?;
    if !is_within(root, &resolved) {
        return Err(PathError::Escaped);
    }
    Ok(resolved)
}

/// Render `absolute` as a root-relative, forward-slash path.
///
/// Forward slashes on every platform so a citation a model produces on a
/// Windows contributor's node reads the same as one from Linux.
pub fn relative_display(root: &Path, absolute: &Path) -> Option<String> {
    Some(join_components(absolute.strip_prefix(root).ok()?))
}

/// Render a canonical path for a human to read.
///
/// Windows canonicalization returns verbatim paths (`\\?\C:\src\repo`). The
/// prefix is meaningful to the OS and noise to an operator reading a startup
/// log, so it is stripped here and nowhere else — comparisons keep using the
/// real canonical path.
pub fn display_path(path: &Path) -> String {
    let rendered = path.display().to_string();
    rendered
        .strip_prefix(r"\\?\")
        .unwrap_or(rendered.as_str())
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{TempTree, link_directory};

    #[test]
    fn plain_relative_paths_survive_unchanged() {
        assert_eq!(
            sanitize_relative("src/main.rs").expect("plain path"),
            PathBuf::from("src").join("main.rs")
        );
    }

    #[test]
    fn redundant_segments_are_dropped() {
        assert_eq!(
            sanitize_relative("./src//main.rs").expect("plain path"),
            PathBuf::from("src").join("main.rs")
        );
    }

    #[test]
    fn backslashes_are_separators_everywhere() {
        assert_eq!(
            sanitize_relative(r"src\util\mod.rs").expect("windows-shaped path"),
            PathBuf::from("src").join("util").join("mod.rs")
        );
    }

    #[test]
    fn an_empty_path_means_the_root_itself() {
        assert_eq!(sanitize_relative("").expect("empty"), PathBuf::new());
        assert_eq!(sanitize_relative(".").expect("dot"), PathBuf::new());
    }

    #[test]
    fn traversal_is_refused_even_when_it_would_land_back_inside() {
        for input in ["..", "../etc/passwd", "src/../../secrets", "src/../lib.rs"] {
            assert_eq!(
                sanitize_relative(input),
                Err(PathError::Traversal),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn absolute_and_rooted_paths_are_refused() {
        for input in ["/etc/passwd", r"\Windows\System32", r"\\server\share\x"] {
            assert_eq!(
                sanitize_relative(input),
                Err(PathError::Absolute),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn drive_letters_and_alternate_data_streams_are_refused() {
        for input in [r"C:\Windows\System32", "C:/Windows", "notes.txt:secret"] {
            assert_eq!(
                sanitize_relative(input),
                Err(PathError::Reserved),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn containment_is_component_wise_not_textual() {
        let root = Path::new("/srv/repo");
        assert!(is_within(root, Path::new("/srv/repo")));
        assert!(is_within(root, Path::new("/srv/repo/src/main.rs")));
        // The bug a textual prefix check would have.
        assert!(!is_within(root, Path::new("/srv/repo-backup/src/main.rs")));
        assert!(!is_within(root, Path::new("/srv/other")));
        assert!(!is_within(root, Path::new("/")));
    }

    #[test]
    fn the_verbatim_prefix_is_stripped_only_for_display() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\src\repo")),
            r"C:\src\repo".to_string()
        );
        assert_eq!(
            display_path(Path::new("/srv/repo")),
            "/srv/repo".to_string()
        );
    }

    #[test]
    fn a_file_inside_the_root_resolves() {
        let tree = TempTree::new("resolve-inside");
        tree.write("src/main.rs", "fn main() {}\n");
        let root = tree.canonical_root();

        let resolved = resolve_within(&root, "src/main.rs").expect("inside the root");

        assert!(is_within(&root, &resolved));
        assert_eq!(
            relative_display(&root, &resolved).as_deref(),
            Some("src/main.rs")
        );
    }

    #[test]
    fn a_missing_file_is_not_found_rather_than_escaped() {
        let tree = TempTree::new("resolve-missing");
        tree.write("src/main.rs", "fn main() {}\n");
        let root = tree.canonical_root();

        assert_eq!(
            resolve_within(&root, "src/nope.rs"),
            Err(PathError::NotFound)
        );
    }

    /// The escape this plugin exists to refuse: a symlink (or, on Windows, a
    /// directory junction) that lives inside the root but points outside it.
    /// Lexical checks cannot catch this — only canonicalizing and re-testing
    /// containment can.
    #[test]
    fn a_symlink_pointing_outside_the_root_is_refused() {
        let tree = TempTree::new("escape-symlink");
        tree.write("root/src/main.rs", "fn main() {}\n");
        tree.write("outside/secret.txt", "a password lives here\n");

        let root = std::fs::canonicalize(tree.path().join("root")).expect("canonical root");
        let outside =
            std::fs::canonicalize(tree.path().join("outside")).expect("canonical outside");

        let Ok(()) = link_directory(&outside, &root.join("escape")) else {
            eprintln!(
                "skipping symlink escape assertion: this platform refused to create a directory \
                 link (Windows needs Developer Mode or junction support)"
            );
            return;
        };

        // The link really does reach the outside file...
        assert!(
            root.join("escape").join("secret.txt").exists(),
            "test setup is meaningless unless the link actually resolves"
        );
        // ...and the resolver still refuses to hand it back.
        assert_eq!(
            resolve_within(&root, "escape/secret.txt"),
            Err(PathError::Escaped)
        );
        assert_eq!(resolve_within(&root, "escape"), Err(PathError::Escaped));
        // The legitimate file next to it is unaffected.
        assert!(resolve_within(&root, "src/main.rs").is_ok());
    }
}
