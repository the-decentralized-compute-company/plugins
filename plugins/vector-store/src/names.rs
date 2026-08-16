//! Collection names, and how one becomes a filename without becoming a
//! traversal.
//!
//! A collection name arrives in a tool argument, which on an MCP surface means
//! it is frequently chosen by a language model reading a stranger's document.
//! It then has to name a file in the plugin's data directory. That is the
//! shape of every path-traversal bug ever written, so the rule here is not
//! "reject the bad ones" but **"only accept a shape that cannot be a path"**:
//! lowercase ASCII letters, digits, `-` and `_`, between 1 and 64 characters,
//! starting with a letter or digit. No dot, no slash, no backslash, no colon,
//! no `..`, nothing to escape with.
//!
//! Two more rules that are not about traversal:
//!
//! - **Names are case-folded.** `Docs` and `docs` are one collection on every
//!   platform. Accepting both as distinct would create two collections on
//!   Linux and one on Windows and macOS, and the day someone copies a data
//!   directory between machines, one of them silently wins.
//! - **Windows device names are refused.** `con`, `nul`, `com1` and friends are
//!   not filenames on Windows no matter what extension follows them. Refusing
//!   them everywhere keeps a collection that works on Linux from being
//!   impossible on Windows.
//!
//! [`collection_path`] then re-checks containment against the canonical data
//! root, so a symlinked or junctioned collections directory cannot be used to
//! write outside it either.

use std::fmt;
use std::path::{Path, PathBuf};

/// Longest accepted collection name.
pub const MAX_COLLECTION_NAME_CHARS: usize = 64;

/// The subdirectory of the data root where collection logs live.
pub const COLLECTIONS_DIR: &str = "collections";

/// File extension for a collection's append-only log.
pub const COLLECTION_EXTENSION: &str = "jsonl";

/// Reserved device names on Windows. Case-insensitive, and reserved with any
/// extension, so `con.jsonl` is still the console device.
const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    Empty,
    TooLong {
        chars: usize,
    },
    /// Contains something that is not a letter, digit, `-` or `_`.
    IllegalCharacter {
        character: char,
    },
    /// Starts with `-` or `_`, which makes a filename that argument parsers
    /// and shells both mishandle.
    BadFirstCharacter,
    /// A Windows device name.
    ReservedName,
    /// The resolved file landed outside the data root.
    Escaped,
}

impl fmt::Display for NameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(formatter, "collection name must not be empty"),
            Self::TooLong { chars } => write!(
                formatter,
                "collection name is {chars} characters; the maximum is {MAX_COLLECTION_NAME_CHARS}"
            ),
            Self::IllegalCharacter { character } => write!(
                formatter,
                "collection name may contain only letters, digits, '-' and '_', \
                 so {character:?} is not allowed"
            ),
            Self::BadFirstCharacter => write!(
                formatter,
                "collection name must start with a letter or a digit"
            ),
            Self::ReservedName => write!(
                formatter,
                "collection name is a reserved Windows device name and is refused on \
                 every platform so a collection means the same thing everywhere"
            ),
            Self::Escaped => write!(
                formatter,
                "collection file resolves outside the configured data directory and was refused"
            ),
        }
    }
}

impl std::error::Error for NameError {}

/// A collection name that has been proven safe to use as a filename stem.
///
/// The only way to build one is [`CollectionName::parse`], so a function
/// taking a `CollectionName` cannot be handed raw caller input by mistake.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CollectionName(String);

impl CollectionName {
    /// Trim, case-fold, and validate.
    pub fn parse(raw: &str) -> Result<Self, NameError> {
        let folded = raw.trim().to_ascii_lowercase();

        if folded.is_empty() {
            return Err(NameError::Empty);
        }
        let chars = folded.chars().count();
        if chars > MAX_COLLECTION_NAME_CHARS {
            return Err(NameError::TooLong { chars });
        }
        for character in folded.chars() {
            let allowed = character.is_ascii_alphanumeric() || character == '-' || character == '_';
            if !allowed {
                return Err(NameError::IllegalCharacter { character });
            }
        }
        let first = folded.chars().next().expect("non-empty");
        if !first.is_ascii_alphanumeric() {
            return Err(NameError::BadFirstCharacter);
        }
        if WINDOWS_DEVICE_NAMES.contains(&folded.as_str()) {
            return Err(NameError::ReservedName);
        }
        Ok(Self(folded))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The log filename for this collection.
    pub fn file_name(&self) -> String {
        format!("{}.{COLLECTION_EXTENSION}", self.0)
    }
}

impl fmt::Display for CollectionName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl serde::Serialize for CollectionName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// The directory collection logs live in, below the data root.
pub fn collections_dir(data_root: &Path) -> PathBuf {
    data_root.join(COLLECTIONS_DIR)
}

/// Where one collection's log lives, with a physical containment re-check.
///
/// The lexical rules in [`CollectionName::parse`] already make traversal
/// impossible, but they say nothing about the *directory* — a data root that
/// is itself a symlink, or a `collections` directory replaced with a junction,
/// would still write outside where the operator thinks it is. So the parent is
/// canonicalized and re-tested, component-wise. Two independent layers, the
/// same shape `code-context` uses for its root.
///
/// `data_root` must already be canonical.
pub fn collection_path(data_root: &Path, name: &CollectionName) -> Result<PathBuf, NameError> {
    let directory = collections_dir(data_root);
    // A directory that does not exist yet cannot have been replaced with a
    // link, and the caller creates it before writing.
    let resolved_directory = match std::fs::canonicalize(&directory) {
        Ok(resolved) => resolved,
        Err(_) => directory.clone(),
    };
    if !is_within(data_root, &resolved_directory) {
        return Err(NameError::Escaped);
    }
    Ok(resolved_directory.join(name.file_name()))
}

/// Component-wise containment.
///
/// Component-wise rather than a string prefix: `/srv/store-backup` must not
/// count as inside `/srv/store`, and `starts_with` on a string would say it is.
pub fn is_within(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

/// Recover the collection name from a log filename.
///
/// Used when listing the data directory at startup. A file that does not parse
/// back to a legal name is ignored rather than loaded — it was not written by
/// this plugin.
pub fn name_from_file(file_name: &str) -> Option<CollectionName> {
    let stem = file_name.strip_suffix(&format!(".{COLLECTION_EXTENSION}"))?;
    CollectionName::parse(stem).ok()
}

/// Render a canonical path for a human to read.
///
/// Windows canonicalization returns verbatim paths (`\\?\C:\Users\...`). The
/// prefix is meaningful to the OS and noise in a startup log, so it is stripped
/// here and nowhere else — comparisons keep using the real canonical path.
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
    fn ordinary_names_are_accepted() {
        for raw in ["docs", "team_wiki", "runbooks-2024", "a", "x1"] {
            assert_eq!(
                CollectionName::parse(raw).expect("legal").as_str(),
                raw,
                "{raw}"
            );
        }
    }

    #[test]
    fn names_are_trimmed_and_case_folded() {
        assert_eq!(
            CollectionName::parse("  Docs  ").expect("legal").as_str(),
            "docs"
        );
        assert_eq!(
            CollectionName::parse("DOCS").expect("legal"),
            CollectionName::parse("docs").expect("legal"),
            "one collection on a case-insensitive filesystem must be one collection everywhere"
        );
    }

    #[test]
    fn every_traversal_shape_is_refused() {
        // None of these can produce a path, because none of the characters
        // that build a path are in the accepted set at all.
        for raw in [
            "../secrets",
            "..",
            ".",
            "a/b",
            r"a\b",
            "/etc/passwd",
            r"C:\Windows",
            "docs.jsonl",
            "notes:stream",
            "a\0b",
            "with space",
            "emoji-\u{1F600}",
            "..\u{2044}etc",
        ] {
            assert!(
                CollectionName::parse(raw).is_err(),
                "{raw:?} must be refused"
            );
        }
    }

    #[test]
    fn an_illegal_character_is_named_in_the_error() {
        let error = CollectionName::parse("my docs").expect_err("space is illegal");
        assert_eq!(error, NameError::IllegalCharacter { character: ' ' });
        assert!(error.to_string().contains("letters, digits"), "{error}");
    }

    #[test]
    fn empty_and_overlong_names_are_refused() {
        assert_eq!(CollectionName::parse("   "), Err(NameError::Empty));
        let long = "a".repeat(MAX_COLLECTION_NAME_CHARS + 1);
        assert_eq!(
            CollectionName::parse(&long),
            Err(NameError::TooLong {
                chars: MAX_COLLECTION_NAME_CHARS + 1
            })
        );
        assert!(CollectionName::parse(&"a".repeat(MAX_COLLECTION_NAME_CHARS)).is_ok());
    }

    #[test]
    fn a_leading_dash_or_underscore_is_refused() {
        for raw in ["-docs", "_docs"] {
            assert_eq!(
                CollectionName::parse(raw),
                Err(NameError::BadFirstCharacter),
                "{raw}"
            );
        }
    }

    #[test]
    fn windows_device_names_are_refused_on_every_platform() {
        for raw in ["con", "CON", "nul", "aux", "com1", "LPT9", "prn"] {
            assert_eq!(
                CollectionName::parse(raw),
                Err(NameError::ReservedName),
                "{raw} names a device on Windows, not a file"
            );
        }
        // Not reserved: only the exact stems are.
        assert!(CollectionName::parse("console").is_ok());
        assert!(CollectionName::parse("com10").is_ok());
    }

    #[test]
    fn the_file_name_is_the_stem_plus_the_extension() {
        let name = CollectionName::parse("docs").expect("legal");
        assert_eq!(name.file_name(), "docs.jsonl");
        assert_eq!(name_from_file("docs.jsonl"), Some(name));
    }

    #[test]
    fn a_foreign_file_in_the_directory_is_not_mistaken_for_a_collection() {
        for file_name in [
            "README.md",
            "docs.jsonl.bak",
            "notes.json",
            ".hidden",
            "con.jsonl",
        ] {
            assert_eq!(name_from_file(file_name), None, "{file_name}");
        }
    }

    #[test]
    fn containment_is_component_wise_not_textual() {
        let root = Path::new("/srv/store");
        assert!(is_within(
            root,
            Path::new("/srv/store/collections/docs.jsonl")
        ));
        assert!(!is_within(
            root,
            Path::new("/srv/store-backup/collections/docs.jsonl")
        ));
        assert!(!is_within(root, Path::new("/srv")));
    }

    #[test]
    fn a_collection_path_lands_inside_the_data_root() {
        let tree = TempTree::new("collection-path");
        let root = tree.canonical_root();
        std::fs::create_dir_all(collections_dir(&root)).expect("create collections dir");

        let name = CollectionName::parse("docs").expect("legal");
        let path = collection_path(&root, &name).expect("inside the root");

        assert!(is_within(&root, &path), "{}", path.display());
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("docs.jsonl")
        );
    }

    /// The escape the lexical rules cannot see: the `collections` directory
    /// itself replaced with a link pointing somewhere else.
    #[test]
    fn a_linked_collections_directory_is_refused() {
        let tree = TempTree::new("collection-escape");
        tree.write("root/collections/.keep", "");
        tree.write("outside/.keep", "");

        let root = std::fs::canonicalize(tree.path().join("root")).expect("canonical root");
        let outside = std::fs::canonicalize(tree.path().join("outside")).expect("canonical");

        // Replace `root/collections` with a link to `outside`.
        std::fs::remove_dir_all(root.join(COLLECTIONS_DIR)).expect("remove real directory");
        let Ok(()) = link_directory(&outside, &root.join(COLLECTIONS_DIR)) else {
            eprintln!(
                "skipping link escape assertion: this platform refused to create a directory \
                 link (Windows needs Developer Mode or junction support)"
            );
            return;
        };

        let name = CollectionName::parse("docs").expect("legal");
        assert_eq!(collection_path(&root, &name), Err(NameError::Escaped));
    }

    #[test]
    fn the_verbatim_prefix_is_stripped_only_for_display() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\me\.tdcc\vector-store")),
            r"C:\Users\me\.tdcc\vector-store".to_string()
        );
        assert_eq!(display_path(Path::new("/home/me/.tdcc")), "/home/me/.tdcc");
    }
}
