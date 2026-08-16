//! Root confinement.
//!
//! This is the security core of the plugin. It runs on hardware that may not
//! belong to the person asking the question, so every path that arrives in a
//! tool argument goes through [`Roots::resolve`] before anything touches the
//! filesystem, and the answer is only ever a path that is provably inside one
//! configured root.
//!
//! Callers address files as `<label>/<relative path>`. The label is mandatory
//! even when a single root is configured, which removes the one ambiguity a
//! bare relative path would introduce — a first segment that happens to match a
//! label — and makes `list_documents` output directly usable as input.
//!
//! Two independent layers, because either one alone is bypassable:
//!
//! 1. [`sanitize_relative`] is *lexical*. It rejects absolute paths, rooted
//!    paths, drive prefixes, alternate-data-stream syntax and `..` before a
//!    syscall happens, so a traversal attempt never even becomes a `stat`.
//! 2. [`Roots::resolve`] is *physical*. It canonicalizes the joined path —
//!    which resolves symlinks, junctions and `.` — and then re-checks
//!    containment against the canonical root. A symlink inside a root that
//!    points outside it fails here.
//!
//! The directory walk in [`crate::listing`] adds a third: it never follows a
//! directory symlink, so a link is not a candidate in the first place.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::options::RootSpec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// The path did not start with a configured root label.
    NoLabel {
        available: Vec<String>,
    },
    /// The first segment was not one of the configured labels.
    UnknownLabel {
        given: String,
        available: Vec<String>,
    },
    /// The path was absolute or rooted (`/etc/passwd`, `\\server\share`).
    Absolute,
    /// The path contained a `..` segment.
    Traversal,
    /// A segment contained `:` — a Windows drive letter or an NTFS alternate
    /// data stream. Refused everywhere so behaviour does not fork per OS.
    Reserved,
    /// The path resolved, but outside the configured root. This is the symlink
    /// and junction case.
    Escaped,
    /// Nothing exists at that path inside the root.
    NotFound,
    /// A file was required and a directory was named, or the other way round.
    NotAFile,
    NotADirectory,
    /// The path exists but could not be resolved (permissions, I/O).
    Unresolvable(String),
}

fn render_labels(labels: &[String]) -> String {
    labels
        .iter()
        .map(|label| format!("`{label}/`"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl fmt::Display for PathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoLabel { available } => write!(
                formatter,
                "path must start with a configured root label. Available: {}. Call \
                 `list_documents` to see the files under each.",
                render_labels(available)
            ),
            Self::UnknownLabel { given, available } => write!(
                formatter,
                "`{given}` is not a configured root. Available: {}. Call `list_documents` to see \
                 the files under each.",
                render_labels(available)
            ),
            Self::Absolute => write!(
                formatter,
                "path must be `<root label>/<path inside that root>`, not an absolute path"
            ),
            Self::Traversal => write!(formatter, "path must not contain a '..' segment"),
            Self::Reserved => write!(
                formatter,
                "path must not contain ':' (drive letters and alternate data streams are refused)"
            ),
            Self::Escaped => write!(
                formatter,
                "path resolves outside its configured root and was refused"
            ),
            Self::NotFound => write!(formatter, "no such path inside the configured root"),
            Self::NotAFile => write!(formatter, "that path is a directory, not a file"),
            Self::NotADirectory => write!(formatter, "that path is a file, not a directory"),
            Self::Unresolvable(reason) => write!(formatter, "path could not be resolved: {reason}"),
        }
    }
}

impl std::error::Error for PathError {}

/// A path proven to be inside one configured root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The label of the root it was found under.
    pub label: String,
    /// The caller-facing path, `<label>/<relative>`, forward slashes on every
    /// platform. Equal to what `list_documents` returns for the same file.
    pub display: String,
    /// Canonical absolute path. Never returned to a caller.
    pub absolute: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub label: String,
    /// Canonical. Every resolved path is proven to sit under this.
    pub directory: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roots {
    roots: Vec<Root>,
}

#[derive(Debug)]
pub struct RootsError {
    pub label: String,
    pub directory: PathBuf,
    pub reason: String,
}

impl fmt::Display for RootsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "root `{}` ({}) is unusable: {}",
            self.label,
            self.directory.display(),
            self.reason
        )
    }
}

impl std::error::Error for RootsError {}

impl Roots {
    /// Canonicalize every configured root and take ownership of it.
    ///
    /// A root that does not exist is a startup failure rather than a warning:
    /// an operator who mistyped a directory should find out immediately, not
    /// when a caller gets "no such path" for a file that is really there.
    pub fn open(specs: &[RootSpec]) -> Result<Self, RootsError> {
        let mut roots = Vec::with_capacity(specs.len());
        for spec in specs {
            let directory = std::fs::canonicalize(&spec.directory).map_err(|error| RootsError {
                label: spec.label.clone(),
                directory: spec.directory.clone(),
                reason: error.to_string(),
            })?;
            if !directory.is_dir() {
                return Err(RootsError {
                    label: spec.label.clone(),
                    directory: spec.directory.clone(),
                    reason: "not a directory".to_string(),
                });
            }
            roots.push(Root {
                label: spec.label.clone(),
                directory,
            });
        }
        Ok(Self { roots })
    }

    /// Roots with nothing in them, for `--print-package-manifest`.
    ///
    /// Building the manifest needs a value for the handlers to capture, but no
    /// handler runs on that path. Every resolve against an empty set fails, so
    /// a mistake here is loud rather than resolving against the process working
    /// directory.
    pub fn empty() -> Self {
        Self { roots: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Root> {
        self.roots.iter()
    }

    pub fn labels(&self) -> Vec<String> {
        self.roots.iter().map(|root| root.label.clone()).collect()
    }

    pub fn get(&self, label: &str) -> Option<&Root> {
        self.roots.iter().find(|root| root.label == label)
    }

    /// Resolve `<label>/<relative>` and prove the result is inside that root.
    ///
    /// The returned path is canonical, so callers can hand it straight to the
    /// filesystem. Errors deliberately carry no absolute path: telling a caller
    /// where a root lives on the contributor's disk is a disclosure this plugin
    /// has no reason to make.
    pub fn resolve(&self, input: &str) -> Result<Resolved, PathError> {
        let normalized = input.trim().replace('\\', "/");
        if normalized.starts_with('/') {
            return Err(PathError::Absolute);
        }

        let (label, rest) = match normalized.split_once('/') {
            Some((label, rest)) => (label, rest),
            None => (normalized.as_str(), ""),
        };
        if label.is_empty() {
            return Err(PathError::NoLabel {
                available: self.labels(),
            });
        }
        let Some(root) = self.get(label) else {
            // A drive-letter path (`C:/Users/...`) lands here as an unknown
            // label; the dedicated error is clearer than "unknown root C:".
            if label.contains(':') {
                return Err(PathError::Absolute);
            }
            return Err(PathError::UnknownLabel {
                given: label.to_string(),
                available: self.labels(),
            });
        };

        let relative = sanitize_relative(rest)?;
        let resolved = std::fs::canonicalize(root.directory.join(&relative)).map_err(|error| {
            match error.kind() {
                std::io::ErrorKind::NotFound => PathError::NotFound,
                _ => PathError::Unresolvable(error.kind().to_string()),
            }
        })?;
        if !is_within(&root.directory, &resolved) {
            return Err(PathError::Escaped);
        }

        Ok(Resolved {
            label: root.label.clone(),
            display: display_key(&root.label, &join_components(&relative)),
            absolute: resolved,
        })
    }

    /// [`Roots::resolve`] for a path that must be a regular file.
    pub fn resolve_file(&self, input: &str) -> Result<Resolved, PathError> {
        let resolved = self.resolve(input)?;
        if !resolved.absolute.is_file() {
            return Err(PathError::NotAFile);
        }
        Ok(resolved)
    }

    /// [`Roots::resolve`] for a path that must be a directory.
    pub fn resolve_directory(&self, input: &str) -> Result<Resolved, PathError> {
        let resolved = self.resolve(input)?;
        if !resolved.absolute.is_dir() {
            return Err(PathError::NotADirectory);
        }
        Ok(resolved)
    }
}

/// Turn caller-supplied text into a relative path made only of plain segments.
///
/// Backslashes are treated as separators on every platform so a Windows-shaped
/// `reports\q4.pdf` works; the cost is that a file whose name literally
/// contains a backslash is unreachable, which is the right trade here.
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

/// Component-wise containment test.
///
/// Component-wise, not string-prefix: `/srv/docs-backup` must not count as
/// being inside `/srv/docs`, and a textual `starts_with` would say it is. Both
/// arguments must already be canonical for this to mean anything.
pub fn is_within(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

/// Render a path made of plain components as a `/`-separated string.
///
/// Forward slashes on every platform, so a citation a model produces on a
/// Windows contributor's node reads the same as one from Linux.
pub fn join_components(path: &Path) -> String {
    let mut rendered = String::new();
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            continue;
        }
        if !rendered.is_empty() {
            rendered.push('/');
        }
        rendered.push_str(&component.as_os_str().to_string_lossy());
    }
    rendered
}

/// The caller-facing key for a file: `<label>/<relative>`, or bare `<label>`
/// for the root directory itself.
pub fn display_key(label: &str, relative: &str) -> String {
    if relative.is_empty() {
        label.to_string()
    } else {
        format!("{label}/{relative}")
    }
}

/// Render a canonical path for a human to read.
///
/// Windows canonicalization returns verbatim paths (`\\?\C:\docs`). The prefix
/// is meaningful to the OS and noise to an operator reading a startup log, so
/// it is stripped here and nowhere else — comparisons keep using the real
/// canonical path.
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

    fn roots_for(tree: &TempTree, labels: &[(&str, &str)]) -> Roots {
        let specs: Vec<RootSpec> = labels
            .iter()
            .map(|(label, relative)| RootSpec {
                label: (*label).to_string(),
                directory: tree.path().join(relative),
            })
            .collect();
        Roots::open(&specs).expect("roots open")
    }

    #[test]
    fn plain_relative_paths_survive_unchanged() {
        assert_eq!(
            sanitize_relative("reports/q4.pdf").expect("plain path"),
            PathBuf::from("reports").join("q4.pdf")
        );
    }

    #[test]
    fn redundant_segments_are_dropped_and_backslashes_are_separators() {
        assert_eq!(
            sanitize_relative("./reports//q4.pdf").expect("plain path"),
            PathBuf::from("reports").join("q4.pdf")
        );
        assert_eq!(
            sanitize_relative(r"reports\2024\q4.pdf").expect("windows-shaped path"),
            PathBuf::from("reports").join("2024").join("q4.pdf")
        );
    }

    #[test]
    fn traversal_is_refused_even_when_it_would_land_back_inside() {
        for input in ["..", "../etc/passwd", "a/../../secrets", "a/../b.pdf"] {
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
        for input in [r"C:\Windows\System32", "C:/Windows", "notes.pdf:secret"] {
            assert_eq!(
                sanitize_relative(input),
                Err(PathError::Reserved),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn containment_is_component_wise_not_textual() {
        let root = Path::new("/srv/docs");
        assert!(is_within(root, Path::new("/srv/docs")));
        assert!(is_within(root, Path::new("/srv/docs/reports/q4.pdf")));
        // The bug a textual prefix check would have.
        assert!(!is_within(root, Path::new("/srv/docs-backup/q4.pdf")));
        assert!(!is_within(root, Path::new("/srv/other")));
        assert!(!is_within(root, Path::new("/")));
    }

    #[test]
    fn the_verbatim_prefix_is_stripped_only_for_display() {
        assert_eq!(display_path(Path::new(r"\\?\C:\docs")), r"C:\docs");
        assert_eq!(display_path(Path::new("/srv/docs")), "/srv/docs");
    }

    #[test]
    fn a_file_inside_a_labelled_root_resolves_to_the_key_a_listing_would_show() {
        let tree = TempTree::new("resolve-inside");
        tree.write("docs/reports/q4.pdf", "%PDF-1.4\n");
        let roots = roots_for(&tree, &[("docs", "docs")]);

        let resolved = roots.resolve_file("docs/reports/q4.pdf").expect("inside");

        assert_eq!(resolved.label, "docs");
        assert_eq!(resolved.display, "docs/reports/q4.pdf");
        assert!(resolved.absolute.is_file());
    }

    #[test]
    fn several_roots_are_addressed_by_their_labels() {
        let tree = TempTree::new("resolve-multi");
        tree.write("a/one.pdf", "%PDF-1.4\n");
        tree.write("b/two.pdf", "%PDF-1.4\n");
        let roots = roots_for(&tree, &[("first", "a"), ("second", "b")]);

        assert_eq!(
            roots.resolve_file("first/one.pdf").expect("first").display,
            "first/one.pdf"
        );
        assert_eq!(
            roots
                .resolve_file("second/two.pdf")
                .expect("second")
                .display,
            "second/two.pdf"
        );
        // A file that exists under the *other* root is not reachable from this
        // one, which is the whole point of the labels.
        assert_eq!(
            roots.resolve_file("first/two.pdf"),
            Err(PathError::NotFound)
        );
    }

    #[test]
    fn a_missing_label_lists_the_labels_that_do_exist() {
        let tree = TempTree::new("resolve-nolabel");
        tree.write("docs/q4.pdf", "%PDF-1.4\n");
        let roots = roots_for(&tree, &[("docs", "docs")]);

        // No label at all: a bare filename is not a path this plugin accepts.
        let error = roots.resolve_file("q4.pdf").expect_err("bare name");
        assert!(
            matches!(&error, PathError::UnknownLabel { given, .. } if given == "q4.pdf"),
            "{error}"
        );
        assert!(error.to_string().contains("`docs/`"), "{error}");

        let error = roots.resolve_file("").expect_err("empty path");
        assert!(matches!(error, PathError::NoLabel { .. }), "{error}");
    }

    #[test]
    fn an_absolute_path_is_refused_by_shape_rather_than_looked_up() {
        let tree = TempTree::new("resolve-absolute");
        tree.write("docs/q4.pdf", "%PDF-1.4\n");
        let roots = roots_for(&tree, &[("docs", "docs")]);

        assert_eq!(roots.resolve_file("/etc/passwd"), Err(PathError::Absolute));
        assert_eq!(
            roots.resolve_file(r"C:\Windows\win.ini"),
            Err(PathError::Absolute)
        );
        // Even one naming the real root: the caller must go through a label.
        let absolute = display_path(&tree.canonical_root().join("docs").join("q4.pdf"));
        assert!(
            matches!(
                roots.resolve_file(&absolute),
                Err(PathError::Absolute | PathError::UnknownLabel { .. })
            ),
            "an absolute path must never resolve"
        );
    }

    #[test]
    fn traversal_out_of_a_root_is_refused_before_any_syscall() {
        let tree = TempTree::new("resolve-traversal");
        tree.write("docs/q4.pdf", "%PDF-1.4\n");
        tree.write("secrets/passwords.pdf", "%PDF-1.4\n");
        let roots = roots_for(&tree, &[("docs", "docs")]);

        assert_eq!(
            roots.resolve_file("docs/../secrets/passwords.pdf"),
            Err(PathError::Traversal)
        );
    }

    #[test]
    fn a_directory_is_not_a_file_and_a_file_is_not_a_directory() {
        let tree = TempTree::new("resolve-kind");
        tree.write("docs/reports/q4.pdf", "%PDF-1.4\n");
        let roots = roots_for(&tree, &[("docs", "docs")]);

        assert_eq!(roots.resolve_file("docs/reports"), Err(PathError::NotAFile));
        assert_eq!(
            roots.resolve_directory("docs/reports/q4.pdf"),
            Err(PathError::NotADirectory)
        );
        assert_eq!(
            roots
                .resolve_directory("docs")
                .expect("root itself")
                .display,
            "docs"
        );
    }

    /// The escape this plugin exists to refuse: a symlink (or, on Windows, a
    /// directory junction) that lives inside a root but points outside it.
    /// Lexical checks cannot catch this — only canonicalizing and re-testing
    /// containment can.
    #[test]
    fn a_symlink_pointing_outside_a_root_is_refused() {
        let tree = TempTree::new("escape-symlink");
        tree.write("docs/q4.pdf", "%PDF-1.4\n");
        tree.write("outside/payroll.pdf", "%PDF-1.4\n");
        let roots = roots_for(&tree, &[("docs", "docs")]);

        let root = tree.canonical_root().join("docs");
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
            root.join("escape").join("payroll.pdf").exists(),
            "the test is meaningless unless the link actually resolves"
        );
        // ...and the resolver still refuses to hand it back.
        assert_eq!(
            roots.resolve_file("docs/escape/payroll.pdf"),
            Err(PathError::Escaped)
        );
        assert_eq!(
            roots.resolve_directory("docs/escape"),
            Err(PathError::Escaped)
        );
        // The legitimate file next to it is unaffected.
        assert!(roots.resolve_file("docs/q4.pdf").is_ok());
    }

    #[test]
    fn an_empty_root_set_refuses_everything_rather_than_using_the_working_directory() {
        let roots = Roots::empty();

        assert!(roots.is_empty());
        assert!(matches!(
            roots.resolve_file("docs/q4.pdf"),
            Err(PathError::UnknownLabel { .. })
        ));
    }

    #[test]
    fn a_root_that_does_not_exist_is_a_startup_error() {
        let tree = TempTree::new("root-missing");
        let error = Roots::open(&[RootSpec {
            label: "docs".to_string(),
            directory: tree.path().join("nope"),
        }])
        .expect_err("a mistyped root must not start");

        assert_eq!(error.label, "docs");
    }

    #[test]
    fn a_root_that_is_a_file_is_a_startup_error() {
        let tree = TempTree::new("root-file");
        tree.write("notes.pdf", "%PDF-1.4\n");
        let error = Roots::open(&[RootSpec {
            label: "docs".to_string(),
            directory: tree.path().join("notes.pdf"),
        }])
        .expect_err("a file is not a root");

        assert!(error.reason.contains("not a directory"), "{error}");
    }
}
