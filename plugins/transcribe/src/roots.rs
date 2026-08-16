//! Root confinement.
//!
//! This is the security core of the plugin. It runs on hardware that may not
//! belong to the person asking the question, and it reads audio — voice notes,
//! interviews, therapy recordings, meetings — which is about as private as
//! files on a disk get. Every path that arrives in a tool argument goes through
//! [`Roots::resolve`] before anything is opened, and the answer is only ever a
//! path that is provably inside a directory the operator named.
//!
//! Two independent layers, because either one alone is bypassable:
//!
//! 1. [`sanitize_relative`] is *lexical*. It rejects absolute paths, rooted
//!    paths, drive prefixes and `..` before a syscall happens, so a traversal
//!    attempt never even becomes a `stat`.
//! 2. [`Roots::resolve`] is *physical*. It canonicalizes the joined path —
//!    which resolves symlinks, junctions and `.` — and then re-checks
//!    containment. A symlink inside a root that points outside it fails here.
//!
//! With no root configured the resolver refuses everything, which is the state
//! an operator gets by doing nothing.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::config::RootSpec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// The operator configured no root, so nothing is readable.
    NoRoots,
    /// The path was absolute or rooted (`/srv/audio/x.wav`, `C:\x.wav`).
    Absolute { known: Vec<String> },
    /// The path contained a `..` segment.
    Traversal,
    /// A segment contained `:` — a Windows drive letter or an NTFS alternate
    /// data stream. Rejected everywhere so behaviour does not fork per OS.
    Reserved,
    /// The path named no file at all (empty, or only a root label).
    Empty { known: Vec<String> },
    /// A root label was named that does not exist.
    UnknownRoot { label: String, known: Vec<String> },
    /// The path resolved, but outside every configured root. This is the
    /// symlink and junction case.
    Escaped,
    /// Nothing exists at that path inside any configured root.
    NotFound { known: Vec<String> },
    /// Something exists there, but it is a directory or a device, not a file.
    NotAFile,
    /// The same relative path exists in more than one root.
    Ambiguous { candidates: Vec<String> },
    /// The path exists but could not be resolved (permissions, I/O).
    Unresolvable(String),
}

fn list(known: &[String]) -> String {
    if known.is_empty() {
        "none".to_string()
    } else {
        known.join(", ")
    }
}

impl fmt::Display for PathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRoots => write!(formatter, "{}", crate::config::Config::no_roots_message()),
            Self::Absolute { known } => write!(
                formatter,
                "give a path relative to one of this plugin's audio roots, not an absolute path. \
                 Configured roots: {}. Call `transcribe.list_audio` to see the exact paths that \
                 work.",
                list(known)
            ),
            Self::Traversal => write!(
                formatter,
                "a path must not contain a '..' segment; it would point outside the audio root."
            ),
            Self::Reserved => write!(
                formatter,
                "a path must not contain ':' (drive letters and alternate data streams are \
                 refused)."
            ),
            Self::Empty { known } => write!(
                formatter,
                "that path names no file. Write `<root>/<file>`, where root is one of: {}.",
                list(known)
            ),
            Self::UnknownRoot { label, known } => write!(
                formatter,
                "`{label}` is not one of this plugin's audio roots. Configured roots: {}.",
                list(known)
            ),
            Self::Escaped => write!(
                formatter,
                "that path resolves outside every configured audio root — it is a link pointing \
                 out of the root — and was refused."
            ),
            Self::NotFound { known } => write!(
                formatter,
                "no such file inside the configured audio roots ({}). Call \
                 `transcribe.list_audio` to see what is there.",
                list(known)
            ),
            Self::NotAFile => write!(
                formatter,
                "that path exists but is not a regular file, so there is no audio to read."
            ),
            Self::Ambiguous { candidates } => write!(
                formatter,
                "that path exists in more than one audio root ({}). Name the root explicitly, for \
                 example `{}`.",
                candidates.join(", "),
                candidates
                    .first()
                    .map(String::as_str)
                    .unwrap_or("<root>/<file>")
            ),
            Self::Unresolvable(reason) => {
                write!(formatter, "that path could not be resolved: {reason}")
            }
        }
    }
}

impl std::error::Error for PathError {}

/// One configured directory, as the plugin holds it at runtime.
#[derive(Debug, Clone)]
pub struct Root {
    pub label: String,
    /// Exactly what the operator wrote, for reporting.
    pub configured: PathBuf,
    /// The canonical form, or `None` when the directory does not currently
    /// exist. A root on a drive that is not mounted is a real situation and a
    /// worse reason to refuse to start than to report honestly.
    pub canonical: Option<PathBuf>,
}

impl Root {
    pub fn is_available(&self) -> bool {
        self.canonical.is_some()
    }
}

/// A path that has been proven to live inside a configured root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The root it was found in.
    pub label: String,
    /// Forward-slash path below that root, on every platform, so a citation
    /// produced on a Windows node reads the same as one from Linux.
    pub relative: String,
    /// Canonical absolute path. Safe to hand straight to the filesystem.
    pub absolute: PathBuf,
}

impl Resolved {
    /// The string a caller passes back in to name this file again.
    pub fn addressed(&self) -> String {
        format!("{}/{}", self.label, self.relative)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Roots {
    entries: Vec<Root>,
}

impl Roots {
    /// Canonicalize the configured directories once, at startup.
    ///
    /// A root that does not exist is retained but marked unavailable rather
    /// than rejected, so `status` can name it and an operator can see which of
    /// their three roots is the one that is missing.
    pub fn open(specs: &[RootSpec]) -> Self {
        let entries = specs
            .iter()
            .map(|spec| Root {
                label: spec.label.clone(),
                configured: spec.path.clone(),
                canonical: std::fs::canonicalize(&spec.path)
                    .ok()
                    .filter(|path| path.is_dir()),
            })
            .collect();
        Self { entries }
    }

    pub fn entries(&self) -> &[Root] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn labels(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.label.clone())
            .collect()
    }

    fn find(&self, label: &str) -> Option<&Root> {
        self.entries.iter().find(|entry| entry.label == label)
    }

    /// Resolve a caller-supplied path to a real file inside a root.
    ///
    /// Two spellings are accepted, in this order:
    ///
    /// 1. `<root-label>/<relative path>` — the form `list_audio` returns, and
    ///    the only unambiguous one when several roots are configured.
    /// 2. A bare relative path, tried in every root. Exactly one match is used;
    ///    several is [`PathError::Ambiguous`] rather than an arbitrary pick.
    ///
    /// A file whose own name collides with a root label still works: the label
    /// reading is tried first, and a miss falls through to reading 2.
    pub fn resolve(&self, input: &str) -> Result<Resolved, PathError> {
        if self.entries.is_empty() {
            return Err(PathError::NoRoots);
        }
        let relative = sanitize_relative(input, &self.labels())?;
        let segments: Vec<&str> = relative
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        if segments.is_empty() {
            return Err(PathError::Empty {
                known: self.labels(),
            });
        }

        // Reading 1: an explicit root label.
        if let Some(root) = self.find(segments[0]) {
            let rest = segments[1..].join("/");
            if rest.is_empty() {
                return Err(PathError::Empty {
                    known: self.labels(),
                });
            }
            match resolve_in(root, &rest) {
                Ok(resolved) => return Ok(resolved),
                // A miss under an explicit label may still be a bare path whose
                // first segment happens to match a label; fall through and try.
                Err(PathError::NotFound { .. }) => {}
                Err(other) => return Err(other),
            }
        } else if segments.len() > 1 && self.entries.len() > 1 {
            // Reaching here means the leading segment is not a label. With
            // several roots configured that is usually a typo in the label
            // rather than a directory name, and saying so is far more useful
            // than "not found" — but only when no root actually holds the path
            // as written, which would make it a legitimate bare path.
            let nothing_holds_it = self
                .entries
                .iter()
                .all(|root| resolve_in(root, &relative).is_err());
            if nothing_holds_it {
                return Err(PathError::UnknownRoot {
                    label: segments[0].to_string(),
                    known: self.labels(),
                });
            }
        }

        // Reading 2: a bare relative path in whichever root holds it.
        self.resolve_bare(&relative)
    }

    fn resolve_bare(&self, relative: &str) -> Result<Resolved, PathError> {
        let mut found: Vec<Resolved> = Vec::new();
        let mut refusal: Option<PathError> = None;
        for root in &self.entries {
            match resolve_in(root, relative) {
                Ok(resolved) => found.push(resolved),
                Err(PathError::NotFound { .. }) => {}
                // An escape or a directory is worth reporting if nothing else
                // matched: it is more informative than a bare "not found".
                Err(other) => refusal = refusal.or(Some(other)),
            }
        }
        match found.len() {
            0 => Err(refusal.unwrap_or(PathError::NotFound {
                known: self.labels(),
            })),
            1 => Ok(found.remove(0)),
            _ => Err(PathError::Ambiguous {
                candidates: found.iter().map(Resolved::addressed).collect(),
            }),
        }
    }
}

/// Resolve `relative` inside one root and prove the result is contained by it.
fn resolve_in(root: &Root, relative: &str) -> Result<Resolved, PathError> {
    let Some(canonical_root) = root.canonical.as_ref() else {
        return Err(PathError::NotFound { known: Vec::new() });
    };
    let mut joined = canonical_root.clone();
    for segment in relative.split('/').filter(|part| !part.is_empty()) {
        joined.push(segment);
    }

    let resolved = std::fs::canonicalize(&joined).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => PathError::NotFound { known: Vec::new() },
        _ => PathError::Unresolvable(error.kind().to_string()),
    })?;
    if !is_within(canonical_root, &resolved) {
        return Err(PathError::Escaped);
    }
    // Checked after containment so a link out of the root is reported as an
    // escape rather than as "not a file".
    if !resolved.is_file() {
        return Err(PathError::NotAFile);
    }

    Ok(Resolved {
        label: root.label.clone(),
        relative: relative_display(canonical_root, &resolved)
            .unwrap_or_else(|| relative.to_string()),
        absolute: resolved,
    })
}

/// Turn caller-supplied text into a relative, forward-slash path made only of
/// plain segments.
///
/// Backslashes are treated as separators on every platform so a Windows-shaped
/// `interviews\take-1.wav` works; the cost is that a file whose name literally
/// contains a backslash is unreachable, which is the right trade here.
///
/// `..` is refused outright rather than normalized away. `a/../b` stays inside
/// the root, but accepting it means the resolver has to agree with the
/// filesystem about what `..` means across symlinks, and that is exactly the
/// class of bug this function exists to avoid.
pub fn sanitize_relative(input: &str, known: &[String]) -> Result<String, PathError> {
    let normalized = input.trim().replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(PathError::Absolute {
            known: known.to_vec(),
        });
    }

    let mut segments: Vec<&str> = Vec::new();
    for segment in normalized.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return Err(PathError::Traversal),
            _ => {}
        }
        if segment.contains(':') {
            return Err(PathError::Reserved);
        }
        segments.push(segment);
    }
    Ok(segments.join("/"))
}

/// Component-wise containment test.
///
/// Component-wise, not string-prefix: `/srv/audio-backup` must not count as
/// being inside `/srv/audio`, and a textual `starts_with` would say it is.
/// Both arguments must already be canonical for this to mean anything.
pub fn is_within(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

/// Render `absolute` as a root-relative, forward-slash path.
pub fn relative_display(root: &Path, absolute: &Path) -> Option<String> {
    let stripped = absolute.strip_prefix(root).ok()?;
    let mut rendered = String::new();
    for component in stripped.components() {
        if !rendered.is_empty() {
            rendered.push('/');
        }
        rendered.push_str(&component.as_os_str().to_string_lossy());
    }
    Some(rendered)
}

/// Render a canonical path for a human to read.
///
/// Windows canonicalization returns verbatim paths (`\\?\C:\srv\audio`). The
/// prefix is meaningful to the OS and noise to an operator reading a status
/// payload, so it is stripped here and nowhere else — comparisons keep using
/// the real canonical path.
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
    use crate::testutil::{TempTree, link_directory};

    fn specs(tree: &TempTree, names: &[&str]) -> Vec<RootSpec> {
        names
            .iter()
            .map(|name| RootSpec {
                label: (*name).to_string(),
                path: tree.path().join(name),
            })
            .collect()
    }

    #[test]
    fn plain_relative_paths_survive_unchanged() {
        assert_eq!(
            sanitize_relative("takes/one.wav", &[]).unwrap(),
            "takes/one.wav"
        );
        assert_eq!(
            sanitize_relative("./takes//one.wav", &[]).unwrap(),
            "takes/one.wav"
        );
        assert_eq!(
            sanitize_relative(r"takes\one.wav", &[]).unwrap(),
            "takes/one.wav"
        );
    }

    #[test]
    fn traversal_is_refused_even_when_it_would_land_back_inside() {
        for input in [
            "..",
            "../secrets.wav",
            "takes/../../etc/passwd",
            "a/../b.wav",
        ] {
            assert_eq!(
                sanitize_relative(input, &[]),
                Err(PathError::Traversal),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn absolute_and_rooted_paths_are_refused() {
        for input in [
            "/srv/audio/x.wav",
            r"\Windows\x.wav",
            r"\\server\share\x.wav",
        ] {
            assert!(
                matches!(
                    sanitize_relative(input, &["audio".into()]),
                    Err(PathError::Absolute { .. })
                ),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn drive_letters_and_alternate_data_streams_are_refused() {
        for input in [r"C:\audio\x.wav", "C:/audio", "note.wav:secret"] {
            assert_eq!(
                sanitize_relative(input, &[]),
                Err(PathError::Reserved),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn containment_is_component_wise_not_textual() {
        let root = Path::new("/srv/audio");
        assert!(is_within(root, Path::new("/srv/audio/take.wav")));
        // The bug a textual prefix check would have.
        assert!(!is_within(root, Path::new("/srv/audio-backup/take.wav")));
        assert!(!is_within(root, Path::new("/srv/other")));
    }

    #[test]
    fn the_verbatim_prefix_is_stripped_only_for_display() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\srv\audio")),
            r"C:\srv\audio"
        );
        assert_eq!(display_path(Path::new("/srv/audio")), "/srv/audio");
    }

    #[test]
    fn with_no_roots_every_path_is_refused_and_the_setting_is_named() {
        let roots = Roots::open(&[]);
        let error = roots.resolve("take.wav").expect_err("nothing is readable");

        assert_eq!(error, PathError::NoRoots);
        assert!(error.to_string().contains("--root"), "{error}");
    }

    #[test]
    fn a_file_inside_a_root_resolves_by_label_and_by_bare_path() {
        let tree = TempTree::new("resolve-inside");
        tree.write("audio/takes/one.wav", b"RIFF");
        let roots = Roots::open(&specs(&tree, &["audio"]));

        let by_label = roots.resolve("audio/takes/one.wav").expect("labelled");
        assert_eq!(by_label.label, "audio");
        assert_eq!(by_label.relative, "takes/one.wav");
        assert_eq!(by_label.addressed(), "audio/takes/one.wav");
        assert!(by_label.absolute.is_file());

        let bare = roots.resolve("takes/one.wav").expect("bare");
        assert_eq!(bare, by_label);
    }

    #[test]
    fn a_windows_shaped_path_resolves_the_same_as_a_posix_one() {
        let tree = TempTree::new("resolve-backslash");
        tree.write("audio/takes/one.wav", b"RIFF");
        let roots = Roots::open(&specs(&tree, &["audio"]));

        assert_eq!(
            roots
                .resolve(r"audio\takes\one.wav")
                .expect("windows shaped"),
            roots.resolve("audio/takes/one.wav").expect("posix shaped")
        );
    }

    #[test]
    fn a_missing_file_is_not_found_and_the_message_points_at_list_audio() {
        let tree = TempTree::new("resolve-missing");
        tree.write("audio/one.wav", b"RIFF");
        let roots = Roots::open(&specs(&tree, &["audio"]));

        let error = roots.resolve("audio/two.wav").expect_err("no such file");
        assert!(matches!(error, PathError::NotFound { .. }), "{error:?}");
        assert!(error.to_string().contains("list_audio"), "{error}");
    }

    #[test]
    fn a_directory_is_refused_as_not_a_file() {
        let tree = TempTree::new("resolve-directory");
        tree.write("audio/takes/one.wav", b"RIFF");
        let roots = Roots::open(&specs(&tree, &["audio"]));

        assert_eq!(roots.resolve("audio/takes"), Err(PathError::NotAFile));
    }

    #[test]
    fn an_unknown_root_label_lists_the_ones_that_exist() {
        let tree = TempTree::new("resolve-unknown-root");
        tree.write("audio/one.wav", b"RIFF");
        tree.write("music/two.wav", b"RIFF");
        let roots = Roots::open(&specs(&tree, &["audio", "music"]));

        let error = roots.resolve("podcasts/one.wav").expect_err("no such root");
        let PathError::UnknownRoot { label, known } = &error else {
            panic!("expected UnknownRoot, got {error:?}");
        };
        assert_eq!(label, "podcasts");
        assert_eq!(known, &["audio".to_string(), "music".to_string()]);
    }

    #[test]
    fn the_same_relative_path_in_two_roots_is_ambiguous_rather_than_an_arbitrary_pick() {
        let tree = TempTree::new("resolve-ambiguous");
        tree.write("audio/take.wav", b"RIFF");
        tree.write("music/take.wav", b"RIFF");
        let roots = Roots::open(&specs(&tree, &["audio", "music"]));

        let error = roots.resolve("take.wav").expect_err("two roots hold it");
        let PathError::Ambiguous { candidates } = &error else {
            panic!("expected Ambiguous, got {error:?}");
        };
        assert_eq!(
            candidates,
            &["audio/take.wav".to_string(), "music/take.wav".to_string()]
        );

        // Naming the root resolves it.
        assert_eq!(roots.resolve("music/take.wav").unwrap().label, "music");
    }

    #[test]
    fn a_file_whose_name_matches_a_root_label_is_still_reachable() {
        let tree = TempTree::new("resolve-label-collision");
        // A directory inside the root that is named like the root itself.
        tree.write("audio/audio/take.wav", b"RIFF");
        let roots = Roots::open(&specs(&tree, &["audio"]));

        // Read as `<label>/audio/take.wav`.
        assert_eq!(
            roots.resolve("audio/audio/take.wav").unwrap().relative,
            "audio/take.wav"
        );
        // And the bare form still finds it through the fall-through.
        assert_eq!(
            roots.resolve("audio/take.wav").unwrap().relative,
            "audio/take.wav"
        );
    }

    #[test]
    fn a_root_that_does_not_exist_is_reported_rather_than_fatal() {
        let tree = TempTree::new("resolve-absent-root");
        tree.write("audio/one.wav", b"RIFF");
        let mut roots = specs(&tree, &["audio"]);
        roots.push(RootSpec {
            label: "removable".into(),
            path: tree.path().join("not-mounted"),
        });
        let roots = Roots::open(&roots);

        assert!(roots.entries()[0].is_available());
        assert!(!roots.entries()[1].is_available());
        // The available root still works.
        assert!(roots.resolve("audio/one.wav").is_ok());
    }

    /// The escape this resolver exists to refuse: a symlink (or, on Windows, a
    /// directory junction) that lives inside a root but points outside it.
    /// Lexical checks cannot catch this — only canonicalizing and re-testing
    /// containment can.
    #[test]
    fn a_symlink_pointing_outside_the_root_is_refused() {
        let tree = TempTree::new("escape-symlink");
        tree.write("audio/take.wav", b"RIFF");
        tree.write("private/confession.wav", b"RIFF");

        let outside = std::fs::canonicalize(tree.path().join("private")).expect("canonical");
        let inside = std::fs::canonicalize(tree.path().join("audio")).expect("canonical");

        let Ok(()) = link_directory(&outside, &inside.join("escape")) else {
            eprintln!(
                "skipping symlink escape assertion: this platform refused to create a directory \
                 link (Windows needs Developer Mode or junction support)"
            );
            return;
        };

        let roots = Roots::open(&[RootSpec {
            label: "audio".into(),
            path: inside.clone(),
        }]);

        // The link really does reach the outside file...
        assert!(
            inside.join("escape").join("confession.wav").exists(),
            "the test is meaningless unless the link actually resolves"
        );
        // ...and the resolver still refuses to hand it back.
        assert_eq!(
            roots.resolve("audio/escape/confession.wav"),
            Err(PathError::Escaped)
        );
        // The legitimate file next to it is unaffected.
        assert!(roots.resolve("audio/take.wav").is_ok());
    }

    #[test]
    fn error_messages_never_disclose_where_the_root_lives_on_disk() {
        let tree = TempTree::new("resolve-no-disclosure");
        tree.write("audio/one.wav", b"RIFF");
        let roots = Roots::open(&specs(&tree, &["audio"]));
        let secret = display_path(&tree.canonical_root());

        for input in ["two.wav", "..", "/etc/passwd", "audio"] {
            let message = roots
                .resolve(input)
                .map(|resolved| resolved.addressed())
                .unwrap_or_else(|error| error.to_string());
            assert!(
                !message.contains(&secret),
                "input {input:?} leaked the root path: {message}"
            );
        }
    }
}
