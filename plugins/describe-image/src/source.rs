//! Turning a caller-supplied string into bytes this plugin is allowed to read.
//!
//! Three shapes are accepted and they are told apart before anything is opened:
//! a `data:` URI, an `http`/`https` URL, and a local filesystem path. Every
//! other URL scheme is refused by name rather than falling through to the path
//! branch, because `file:///etc/shadow` reaching a filesystem resolver is
//! exactly the accident worth designing out.
//!
//! The path branch is the security core. It runs on hardware that may not
//! belong to the person asking the question, so it uses two independent layers:
//!
//! 1. A *lexical* check refuses `..` before a syscall happens, so a traversal
//!    attempt never even becomes a `stat`.
//! 2. A *physical* check canonicalizes the resolved path — which follows
//!    symlinks, junctions and `.` — and re-tests containment against the
//!    configured roots. A symlink inside a root that points outside it fails
//!    here, and nothing else can catch that.
//!
//! With no roots configured, the whole branch is refused. That is the default.

use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use reqwest::Url;

/// Media types this plugin will decode. Kept as an allowlist rather than a
/// "starts with image/" test: a decoder is only linked for these, so accepting
/// `image/avif` would mean promising something the binary cannot do.
pub const SUPPORTED_MEDIA_TYPES: &[&str] = &[
    "image/bmp",
    "image/gif",
    "image/jpeg",
    "image/png",
    "image/tiff",
    "image/webp",
];

/// Aliases people and servers actually write, mapped to the canonical type.
fn canonical_media_type(raw: &str) -> Option<&'static str> {
    let raw = raw.trim().to_ascii_lowercase();
    let raw = raw.split(';').next().unwrap_or_default().trim();
    match raw {
        "image/jpeg" | "image/jpg" | "image/pjpeg" => Some("image/jpeg"),
        "image/png" | "image/x-png" | "image/apng" => Some("image/png"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        "image/bmp" | "image/x-bmp" | "image/x-ms-bmp" => Some("image/bmp"),
        "image/tiff" | "image/x-tiff" => Some("image/tiff"),
        _ => None,
    }
}

/// What a caller's string turned out to be, before anything was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An inline `data:image/...;base64,...` URI.
    DataUri,
    /// An absolute `http` or `https` URL.
    Remote(Url),
    /// Everything else: treated as a filesystem path.
    Path,
}

/// Classify a caller-supplied image reference.
///
/// The ordering matters. `data:` wins first because a data URI is never a path.
/// Then anything that parses as a URL with a **multi-character** scheme is
/// treated as a URL — the multi-character rule is what keeps `C:\photos\a.png`
/// out of the URL branch, since `c:` is a single-letter scheme and a Windows
/// drive letter. Anything left is a path.
pub fn classify(raw: &str) -> Result<Kind, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("an image reference cannot be empty.".to_string());
    }
    if trimmed.len() >= 5 && trimmed[..5].eq_ignore_ascii_case("data:") {
        return Ok(Kind::DataUri);
    }

    if let Ok(url) = Url::parse(trimmed) {
        let scheme = url.scheme();
        if scheme.len() > 1 {
            return match scheme {
                "http" | "https" => Ok(Kind::Remote(url)),
                "file" => Err(
                    "`file:` URLs are not accepted. Pass the path itself — it is resolved inside \
                     the roots the operator configured with --root."
                        .to_string(),
                ),
                other => Err(format!(
                    "`{other}:` is not a supported image reference. Pass a local path, a \
                     `data:image/...;base64,...` URI, or an http/https URL."
                )),
            };
        }
    }
    Ok(Kind::Path)
}

/// A decoded `data:` URI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataUri {
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
}

/// Decode a `data:image/...;base64,...` URI.
///
/// Only base64 payloads are accepted: a percent-encoded data URI carrying
/// binary image bytes is legal in RFC 2397 and produced by nothing, while
/// accepting it would mean a second decoding path to get wrong.
///
/// `max_bytes` bounds the **decoded** size, and the encoded length is checked
/// against it first so an oversized payload is refused before it is expanded
/// into memory.
pub fn parse_data_uri(raw: &str, max_bytes: u64) -> Result<DataUri, String> {
    let trimmed = raw.trim();
    let body = trimmed
        .get(5..)
        .ok_or_else(|| "that data: URI is truncated.".to_string())?;
    let (meta, payload) = body.split_once(',').ok_or_else(|| {
        "that data: URI has no `,` separating its header from its data.".to_string()
    })?;

    let mut parts = meta.split(';');
    let declared = parts.next().unwrap_or_default().trim();
    let is_base64 = parts.any(|part| part.trim().eq_ignore_ascii_case("base64"));
    if !is_base64 {
        return Err(
            "that data: URI is not base64-encoded. Use `data:image/png;base64,<...>`; \
             percent-encoded image data is not accepted."
                .to_string(),
        );
    }
    if declared.is_empty() {
        return Err(format!(
            "that data: URI declares no media type. Say which it is — one of {}.",
            SUPPORTED_MEDIA_TYPES.join(", ")
        ));
    }
    let Some(media_type) = canonical_media_type(declared) else {
        return Err(format!(
            "`{declared}` is not an image type this plugin can decode. Supported: {}.",
            SUPPORTED_MEDIA_TYPES.join(", ")
        ));
    };

    // 4 base64 characters encode 3 bytes, so this is a cheap upper bound that
    // never rejects something that would have fitted.
    let payload = payload.trim();
    let upper_bound = (payload.len() as u64 / 4).saturating_mul(3);
    if upper_bound > max_bytes {
        return Err(format!(
            "that data: URI carries about {upper_bound} bytes, over the {max_bytes}-byte limit \
             for one image. Raise --max-image-bytes or send a smaller image."
        ));
    }

    let bytes = BASE64
        .decode(payload.as_bytes())
        .map_err(|error| format!("that data: URI's base64 payload is invalid: {error}"))?;
    if bytes.is_empty() {
        return Err("that data: URI decodes to no bytes.".to_string());
    }
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "that data: URI decodes to {} bytes, over the {max_bytes}-byte limit for one image.",
            bytes.len()
        ));
    }
    Ok(DataUri { media_type, bytes })
}

/// Why a path was refused.
///
/// The messages deliberately carry no absolute path: telling a caller where a
/// root lives on the contributor's disk, or confirming that some path outside
/// it exists, is a disclosure this plugin has no reason to make. `status`
/// reports the configured roots to an operator; a failed lookup does not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// No `--root` is configured, so the local-path branch is closed.
    NoRoots,
    /// The path contained a `..` segment.
    Traversal,
    /// Nothing readable exists at that path inside a root.
    NotFound,
    /// It resolved, but outside every configured root. This is the symlink,
    /// junction, and absolute-path-elsewhere case.
    Escaped,
    /// It resolved inside a root but is a directory, device, or similar.
    NotAFile,
    /// It exists but could not be resolved (permissions, I/O).
    Unresolvable(String),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRoots => write!(
                formatter,
                "this plugin has no image directory configured, so local paths are refused. The \
                 operator adds one with `--root <directory>` in [[plugin]].args or \
                 TDCC_DESCRIBE_IMAGE_ROOTS in the environment; until then, pass a \
                 `data:image/...;base64,...` URI instead"
            ),
            Self::Traversal => write!(formatter, "a path must not contain a '..' segment"),
            Self::NotFound => write!(formatter, "no such image inside the configured roots"),
            Self::Escaped => write!(
                formatter,
                "that path resolves outside every configured root and was refused"
            ),
            Self::NotAFile => write!(formatter, "that path is not a regular file"),
            Self::Unresolvable(reason) => {
                write!(formatter, "that path could not be resolved: {reason}")
            }
        }
    }
}

/// Component-wise containment test.
///
/// Component-wise, not string-prefix: `/srv/photos-backup` must not count as
/// being inside `/srv/photos`, and a textual `starts_with` would say it is.
/// Both arguments must already be canonical for this to mean anything.
pub fn is_within(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

/// True when a path contains a `..` component.
///
/// `..` is refused outright rather than normalized away. `a/../b` does stay
/// inside a root, but accepting it means the resolver has to agree with the
/// filesystem about what `..` means across symlinks, and that is exactly the
/// class of bug the two-layer design exists to avoid.
pub fn has_traversal(input: &str) -> bool {
    Path::new(input)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
}

/// Resolve a caller-supplied path and prove the result is inside a root.
///
/// Absolute paths are accepted — people naturally paste
/// `C:\Users\me\Pictures\a.png` — and are held to the same containment test as
/// a relative one. A relative path is tried against each root in order and the
/// first hit wins, so with a single root (the common case) it reads exactly
/// like a path relative to that directory.
///
/// `roots` must already be canonical; [`crate::config`] canonicalizes them at
/// startup and fails there if one does not exist.
pub fn resolve_in_roots(roots: &[PathBuf], input: &str) -> Result<PathBuf, PathError> {
    if roots.is_empty() {
        return Err(PathError::NoRoots);
    }
    let trimmed = input.trim();
    if has_traversal(trimmed) {
        return Err(PathError::Traversal);
    }

    let requested = Path::new(trimmed);
    let candidates: Vec<PathBuf> = if requested.is_absolute() {
        vec![requested.to_path_buf()]
    } else {
        roots.iter().map(|root| root.join(requested)).collect()
    };

    let mut last = PathError::NotFound;
    for candidate in candidates {
        match std::fs::canonicalize(&candidate) {
            Ok(resolved) => {
                if !roots.iter().any(|root| is_within(root, &resolved)) {
                    last = PathError::Escaped;
                    continue;
                }
                // `is_file` follows the link that canonicalize already
                // resolved, so this rejects directories and device nodes
                // without reopening anything.
                if !resolved.is_file() {
                    last = PathError::NotAFile;
                    continue;
                }
                return Ok(resolved);
            }
            Err(error) => {
                last = match error.kind() {
                    std::io::ErrorKind::NotFound => PathError::NotFound,
                    other => PathError::Unresolvable(other.to_string()),
                };
            }
        }
    }
    Err(last)
}

/// Read a file that [`resolve_in_roots`] already approved, refusing anything
/// over the byte cap.
///
/// The length is taken from the already-open handle's metadata rather than from
/// a separate `fs::metadata` call, so a file swapped between the check and the
/// read cannot get a larger body through.
pub fn read_capped(path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
    use std::io::Read as _;

    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("that image could not be opened: {}", error.kind()))?;
    let length = file
        .metadata()
        .map_err(|error| format!("that image could not be inspected: {}", error.kind()))?
        .len();
    if length > max_bytes {
        return Err(format!(
            "that image is {length} bytes, over the {max_bytes}-byte limit for one image. Raise \
             --max-image-bytes or point at a smaller file."
        ));
    }

    // One byte past the cap, so a file that grew between the metadata read and
    // this read is caught rather than truncated silently.
    let mut bytes = Vec::with_capacity(length as usize);
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("that image could not be read: {}", error.kind()))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "that image grew past the {max_bytes}-byte limit while it was being read."
        ));
    }
    if bytes.is_empty() {
        return Err("that image file is empty.".to_string());
    }
    Ok(bytes)
}

/// A label for a resolved source, safe to put in a tool result.
///
/// A caller already knows the string it passed, so echoing the file name back
/// is not a disclosure — but the canonical absolute path is, so it never
/// appears. A data URI collapses to a fixed label rather than a megabyte of
/// base64 in the response.
pub fn label_for(kind: &Kind, raw: &str, resolved: Option<&Path>) -> String {
    match kind {
        Kind::DataUri => "data: URI".to_string(),
        Kind::Remote(url) => url.to_string(),
        Kind::Path => resolved
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| raw.trim().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory tree that deletes itself. Cheaper than a
    /// `tempfile` dependency for the handful of cases that need real inodes.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let unique = format!(
                "describe-image-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock is after 1970")
                    .as_nanos()
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).expect("temp tree is creatable");
            Self(path)
        }

        fn write(&self, relative: &str, bytes: &[u8]) -> PathBuf {
            let target = self.0.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).expect("parent is creatable");
            }
            std::fs::write(&target, bytes).expect("file is writable");
            target
        }

        fn canonical(&self, relative: &str) -> PathBuf {
            std::fs::canonicalize(self.0.join(relative)).expect("path resolves")
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(windows)]
    fn link_directory(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[cfg(not(windows))]
    fn link_directory(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[test]
    fn data_uris_are_recognised_whatever_their_case() {
        assert_eq!(classify("data:image/png;base64,AAAA"), Ok(Kind::DataUri));
        assert_eq!(classify("DATA:image/png;base64,AAAA"), Ok(Kind::DataUri));
        assert_eq!(classify("  data:image/gif;base64,AA  "), Ok(Kind::DataUri));
    }

    #[test]
    fn http_urls_are_recognised_and_other_schemes_are_refused_by_name() {
        assert!(matches!(
            classify("https://example.com/cat.jpg"),
            Ok(Kind::Remote(_))
        ));

        let error = classify("file:///etc/shadow").expect_err("file URLs are refused");
        assert!(error.contains("file:"), "{error}");
        assert!(error.contains("--root"), "{error}");

        let error = classify("ftp://example.com/cat.jpg").expect_err("ftp is refused");
        assert!(error.contains("ftp"), "{error}");
    }

    #[test]
    fn a_windows_drive_letter_is_a_path_and_not_a_url_scheme() {
        // `Url::parse` happily reads `c:` as a scheme. The one-character rule
        // is what stops `C:\photos\a.png` being refused as an unknown scheme.
        assert_eq!(classify(r"C:\photos\a.png"), Ok(Kind::Path));
        assert_eq!(classify("C:/photos/a.png"), Ok(Kind::Path));
        assert_eq!(classify("photos/a.png"), Ok(Kind::Path));
        assert_eq!(classify("/srv/photos/a.png"), Ok(Kind::Path));
    }

    #[test]
    fn an_empty_reference_is_refused() {
        assert!(classify("   ").is_err());
    }

    #[test]
    fn a_well_formed_data_uri_decodes() {
        let parsed = parse_data_uri("data:image/png;base64,aGVsbG8=", 1_000).expect("decodes");
        assert_eq!(parsed.media_type, "image/png");
        assert_eq!(parsed.bytes, b"hello");
    }

    #[test]
    fn data_uri_media_type_aliases_are_normalized() {
        for (declared, expected) in [
            ("image/jpg", "image/jpeg"),
            ("image/JPEG", "image/jpeg"),
            ("image/x-png", "image/png"),
            ("image/x-ms-bmp", "image/bmp"),
        ] {
            let parsed = parse_data_uri(&format!("data:{declared};base64,aGk="), 1_000)
                .unwrap_or_else(|error| panic!("{declared} should decode: {error}"));
            assert_eq!(parsed.media_type, expected);
        }
    }

    #[test]
    fn every_malformed_data_uri_says_what_is_wrong() {
        let cases = [
            ("data:image/png;base64", "no `,`"),
            ("data:image/png,AAAA", "not base64-encoded"),
            ("data:;base64,AAAA", "declares no media type"),
            ("data:text/plain;base64,aGk=", "can decode"),
            ("data:image/avif;base64,aGk=", "can decode"),
            ("data:image/png;base64,!!!!", "invalid"),
            ("data:image/png;base64,", "no bytes"),
        ];
        for (input, expected) in cases {
            let error = parse_data_uri(input, 1_000).expect_err("must fail");
            assert!(error.contains(expected), "{input} -> {error}");
        }
    }

    #[test]
    fn an_oversized_data_uri_is_refused_before_it_is_decoded() {
        // ~750 KiB of payload against a 1 KiB cap: the encoded-length bound
        // catches it, so the megabyte is never materialised.
        let payload = "A".repeat(1_000_000);
        let error = parse_data_uri(&format!("data:image/png;base64,{payload}"), 1_024)
            .expect_err("over the cap");
        assert!(error.contains("--max-image-bytes"), "{error}");
    }

    #[test]
    fn traversal_is_detected_in_both_separator_styles() {
        assert!(has_traversal("../secrets.png"));
        assert!(has_traversal("photos/../../secrets.png"));
        assert!(!has_traversal("photos/holiday.png"));
        assert!(!has_traversal("photos/..hidden.png"));
        #[cfg(windows)]
        assert!(has_traversal(r"photos\..\..\secrets.png"));
    }

    #[test]
    fn with_no_roots_configured_every_local_path_is_refused() {
        let error = resolve_in_roots(&[], "photo.png").expect_err("no roots means no files");
        assert_eq!(error, PathError::NoRoots);
        assert!(error.to_string().contains("--root"), "{error}");
    }

    #[test]
    fn containment_is_component_wise_not_textual() {
        let root = Path::new("/srv/photos");
        assert!(is_within(root, Path::new("/srv/photos/a.png")));
        // The bug a textual prefix check would have.
        assert!(!is_within(root, Path::new("/srv/photos-backup/a.png")));
        assert!(!is_within(root, Path::new("/srv/other/a.png")));
    }

    #[test]
    fn a_file_inside_a_root_resolves_by_relative_and_absolute_path_alike() {
        let tree = TempTree::new("inside");
        tree.write("album/holiday.png", b"not really a png");
        let root = tree.canonical("");
        let roots = vec![root.clone()];

        let relative = resolve_in_roots(&roots, "album/holiday.png").expect("relative resolves");
        assert!(is_within(&root, &relative));

        let absolute = resolve_in_roots(&roots, &relative.to_string_lossy())
            .expect("the same file by absolute path resolves");
        assert_eq!(absolute, relative);
    }

    #[test]
    fn a_relative_path_is_tried_against_each_root_in_order() {
        let tree = TempTree::new("multi-root");
        tree.write("one/a.png", b"a");
        tree.write("two/b.png", b"b");
        let roots = vec![tree.canonical("one"), tree.canonical("two")];

        assert_eq!(
            resolve_in_roots(&roots, "b.png").expect("found in the second root"),
            tree.canonical("two/b.png")
        );
        assert_eq!(
            resolve_in_roots(&roots, "a.png").expect("found in the first root"),
            tree.canonical("one/a.png")
        );
    }

    #[test]
    fn an_absolute_path_outside_every_root_is_refused() {
        let tree = TempTree::new("outside");
        tree.write("root/a.png", b"a");
        let secret = tree.write("elsewhere/secret.png", b"s");
        let roots = vec![tree.canonical("root")];

        assert_eq!(
            resolve_in_roots(&roots, &secret.to_string_lossy()),
            Err(PathError::Escaped)
        );
    }

    #[test]
    fn traversal_is_refused_before_the_filesystem_is_touched() {
        let tree = TempTree::new("traversal");
        tree.write("root/a.png", b"a");
        tree.write("secret.png", b"s");
        let roots = vec![tree.canonical("root")];

        assert_eq!(
            resolve_in_roots(&roots, "../secret.png"),
            Err(PathError::Traversal)
        );
        // Even one that would have landed back inside.
        assert_eq!(
            resolve_in_roots(&roots, "sub/../a.png"),
            Err(PathError::Traversal)
        );
    }

    /// The escape this plugin exists to refuse: a symlink (or, on Windows, a
    /// directory junction) that lives inside a root but points outside it.
    /// Lexical checks cannot catch this — only canonicalizing and re-testing
    /// containment can.
    #[test]
    fn a_symlink_pointing_outside_a_root_is_refused() {
        let tree = TempTree::new("symlink");
        tree.write("root/a.png", b"a");
        tree.write("outside/secret.png", b"a password screenshot");
        let root = tree.canonical("root");
        let outside = tree.canonical("outside");

        let Ok(()) = link_directory(&outside, &root.join("escape")) else {
            eprintln!(
                "skipping symlink escape assertion: this platform refused to create a directory \
                 link (Windows needs Developer Mode or junction support)"
            );
            return;
        };
        // The link really does reach the outside file...
        assert!(
            root.join("escape").join("secret.png").exists(),
            "the test is meaningless unless the link actually resolves"
        );

        // ...and the resolver still refuses to hand it back.
        let roots = vec![root.clone()];
        assert_eq!(
            resolve_in_roots(&roots, "escape/secret.png"),
            Err(PathError::Escaped)
        );
        // The legitimate file beside it is unaffected.
        assert!(resolve_in_roots(&roots, "a.png").is_ok());
    }

    #[test]
    fn a_directory_inside_a_root_is_not_an_image() {
        let tree = TempTree::new("directory");
        tree.write("root/album/a.png", b"a");
        let roots = vec![tree.canonical("root")];

        assert_eq!(resolve_in_roots(&roots, "album"), Err(PathError::NotAFile));
    }

    #[test]
    fn a_missing_file_is_not_found_rather_than_escaped() {
        let tree = TempTree::new("missing");
        tree.write("root/a.png", b"a");
        let roots = vec![tree.canonical("root")];

        assert_eq!(
            resolve_in_roots(&roots, "nope.png"),
            Err(PathError::NotFound)
        );
    }

    #[test]
    fn no_error_message_leaks_where_the_roots_live() {
        let tree = TempTree::new("no-leak");
        tree.write("root/a.png", b"a");
        let root = tree.canonical("root");
        let rendered = root.to_string_lossy().into_owned();

        for error in [
            PathError::NoRoots,
            PathError::Traversal,
            PathError::NotFound,
            PathError::Escaped,
            PathError::NotAFile,
        ] {
            assert!(
                !error.to_string().contains(&rendered),
                "{error} must not disclose the root path"
            );
        }
    }

    #[test]
    fn reading_refuses_a_file_over_the_cap_and_accepts_one_under_it() {
        let tree = TempTree::new("read-cap");
        let small = tree.write("small.bin", &[7u8; 100]);
        let big = tree.write("big.bin", &[7u8; 10_000]);

        assert_eq!(
            read_capped(&small, 1_000).expect("under the cap").len(),
            100
        );
        let error = read_capped(&big, 1_000).expect_err("over the cap");
        assert!(error.contains("--max-image-bytes"), "{error}");
    }

    #[test]
    fn an_empty_file_is_refused_rather_than_handed_to_a_decoder() {
        let tree = TempTree::new("empty");
        let empty = tree.write("empty.png", b"");
        assert!(read_capped(&empty, 1_000).is_err());
    }

    #[test]
    fn labels_identify_a_source_without_disclosing_where_it_lives() {
        let path = Path::new("/srv/photos/album/holiday.png");
        assert_eq!(
            label_for(&Kind::Path, "album/holiday.png", Some(path)),
            "holiday.png"
        );
        assert_eq!(
            label_for(&Kind::DataUri, "data:image/png;base64,AAAA", None),
            "data: URI"
        );
        assert_eq!(
            label_for(
                &Kind::Remote(Url::parse("https://example.com/cat.jpg").unwrap()),
                "https://example.com/cat.jpg",
                None
            ),
            "https://example.com/cat.jpg"
        );
    }
}
