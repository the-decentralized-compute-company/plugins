//! Artifact identity — reused from the host, not reinvented.
//!
//! TDCC already has one name for "this exact file, from this exact repository,
//! at this exact commit": the **canonical ref**, `{repo}@{revision}/{file}`,
//! produced by `model_ref::format_canonical_ref` and carried on
//! `model_artifact::ResolvedModelArtifact::canonical_ref` and
//! `model_hf::HfModelIdentity::canonical_ref`. The mirror uses that string
//! verbatim as its artifact key so a mirrored artifact and a locally resolved
//! one are the same thing, spelled the same way.
//!
//! Everything derived from the file name — the quant selector, the
//! distribution id, the display model id — comes from `model_ref` too, so the
//! mirror's inventory reads the same as `tdcc`'s own model listings.
//!
//! Note what a canonical ref deliberately does *not* contain: a digest. The
//! Hugging Face file listing does not return per-file SHA-256
//! (`model_hf`'s `list_files` sets `ModelArtifactFile::sha256` to `None`), so
//! identity and integrity are two separate facts here. [`crate::cache`] always
//! carries both.

use std::fmt;
use std::path::Path;

use model_ref::{
    format_canonical_ref, format_model_ref, normalize_gguf_distribution_id,
    quant_selector_from_gguf_file,
};

use crate::digest::Sha256Hex;

/// Longest canonical ref the mirror will accept.
///
/// Repository names, revisions, and repo-relative paths are all bounded in
/// practice; this exists so a hostile peer cannot make the mirror allocate
/// unbounded strings out of an inventory announcement.
pub const MAX_CANONICAL_REF_BYTES: usize = 1024;

/// A parsed, validated canonical ref.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArtifactId {
    repo: String,
    revision: String,
    file: String,
}

impl ArtifactId {
    /// Parse `{repo}@{revision}/{file}`.
    ///
    /// The grammar is unambiguous because `repo` never contains `@` and a
    /// resolved `revision` never contains `/` — the host resolves refs to a
    /// commit sha before formatting (`HfModelRepository::resolve_revision`
    /// returns `info.sha`), so a branch name with a slash never reaches here.
    pub fn parse(canonical_ref: &str) -> Result<Self, ArtifactIdError> {
        let value = canonical_ref.trim();
        if value.is_empty() {
            return Err(ArtifactIdError::Empty);
        }
        if value.len() > MAX_CANONICAL_REF_BYTES {
            return Err(ArtifactIdError::TooLong(value.len()));
        }
        let (repo, rest) = value
            .split_once('@')
            .ok_or(ArtifactIdError::MissingRevision)?;
        let (revision, file) = rest.split_once('/').ok_or(ArtifactIdError::MissingFile)?;

        validate_repo(repo)?;
        validate_revision(revision)?;
        validate_file(file)?;

        Ok(Self {
            repo: repo.to_string(),
            revision: revision.to_string(),
            file: file.to_string(),
        })
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// Repo-relative path of the artifact file. Never used to build a local
    /// path — see [`ArtifactId::cache_key`].
    pub fn file(&self) -> &str {
        &self.file
    }

    pub fn canonical_ref(&self) -> String {
        format_canonical_ref(&self.repo, &self.revision, &self.file)
    }

    /// Quant selector (`Q4_K_M`, `UD-IQ2_M`, …) when the file name carries one.
    pub fn selector(&self) -> Option<String> {
        quant_selector_from_gguf_file(&self.file)
    }

    /// Shard-collapsed distribution id, matching `ResolvedModelArtifact::distribution_id`.
    pub fn distribution_id(&self) -> Option<String> {
        normalize_gguf_distribution_id(&self.file)
    }

    /// Display id in the same form `tdcc` shows elsewhere (`org/repo:Q4_K_M`).
    pub fn model_id(&self) -> String {
        format_model_ref(&self.repo, None, self.selector().as_deref())
    }

    /// Local storage key: the SHA-256 of the canonical ref, hex encoded.
    ///
    /// This is the whole path-traversal defence. No component of a
    /// caller-supplied ref ever reaches the filesystem — not the repo name, not
    /// the revision, not the file path. A peer that asks for
    /// `../../../../etc/passwd` gets a key that is 64 hex characters like every
    /// other key, and either the mirror holds that artifact or it does not.
    pub fn cache_key(&self) -> String {
        Sha256Hex::of_bytes(self.canonical_ref().as_bytes())
            .as_str()
            .to_string()
    }
}

impl fmt::Display for ArtifactId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.canonical_ref())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactIdError {
    Empty,
    TooLong(usize),
    MissingRevision,
    MissingFile,
    Repo(&'static str),
    Revision(&'static str),
    File(&'static str),
}

impl fmt::Display for ArtifactIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = match self {
            Self::Empty => "it is empty".to_string(),
            Self::TooLong(length) => {
                format!("it is {length} bytes, over the {MAX_CANONICAL_REF_BYTES} byte limit")
            }
            Self::MissingRevision => "it has no '@<revision>' segment".to_string(),
            Self::MissingFile => "it has no '/<file>' segment after the revision".to_string(),
            Self::Repo(reason) => format!("its repository is invalid: {reason}"),
            Self::Revision(reason) => format!("its revision is invalid: {reason}"),
            Self::File(reason) => format!("its file path is invalid: {reason}"),
        };
        write!(
            formatter,
            "expected a canonical model artifact ref like \
             'org/repo@0123abc/Model-Q4_K_M.gguf', but {detail}"
        )
    }
}

impl std::error::Error for ArtifactIdError {}

fn validate_repo(repo: &str) -> Result<(), ArtifactIdError> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(ArtifactIdError::Repo("it is not in 'owner/name' form"));
    };
    if owner.is_empty() || name.is_empty() {
        return Err(ArtifactIdError::Repo("owner and name must both be present"));
    }
    if name.contains('/') {
        return Err(ArtifactIdError::Repo("it has more than one '/'"));
    }
    for part in [owner, name] {
        if !part.bytes().all(is_name_byte) {
            return Err(ArtifactIdError::Repo(
                "only ASCII letters, digits, '.', '_', and '-' are allowed",
            ));
        }
        if part == "." || part == ".." {
            return Err(ArtifactIdError::Repo(
                "'.' and '..' are not repository names",
            ));
        }
    }
    Ok(())
}

fn validate_revision(revision: &str) -> Result<(), ArtifactIdError> {
    if revision.is_empty() {
        return Err(ArtifactIdError::Revision("it is empty"));
    }
    if !revision.bytes().all(is_name_byte) {
        return Err(ArtifactIdError::Revision(
            "only ASCII letters, digits, '.', '_', and '-' are allowed",
        ));
    }
    if revision == "." || revision == ".." {
        return Err(ArtifactIdError::Revision("'.' and '..' are not revisions"));
    }
    Ok(())
}

/// Validate the repo-relative file path.
///
/// The mirror never turns this into a local path, so this is belt-and-braces
/// rather than the primary defence — but a ref that would be dangerous
/// anywhere else should not be allowed to propagate through mesh inventory
/// announcements either.
fn validate_file(file: &str) -> Result<(), ArtifactIdError> {
    if file.is_empty() {
        return Err(ArtifactIdError::File("it is empty"));
    }
    if file.contains('\\') {
        return Err(ArtifactIdError::File("backslashes are not path separators"));
    }
    if file.contains(':') {
        return Err(ArtifactIdError::File("':' is not allowed"));
    }
    if file.starts_with('/') {
        return Err(ArtifactIdError::File("it must be repository-relative"));
    }
    for segment in file.split('/') {
        match segment {
            "" => return Err(ArtifactIdError::File("it has an empty path segment")),
            "." | ".." => {
                return Err(ArtifactIdError::File(
                    "'.' and '..' path segments are not allowed",
                ));
            }
            _ => {}
        }
        if segment.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(ArtifactIdError::File("it has a control character"));
        }
    }
    Ok(())
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

/// Recover a canonical ref from a path inside a Hugging Face hub cache.
///
/// The layout is the one `model_hf::huggingface_identity_for_path_in_cache`
/// parses: `<root>/models--<owner>--<name>/snapshots/<revision>/<file...>`.
/// Reproducing it here (rather than depending on `model-hf`, which pulls the
/// whole HF client stack) keeps the plugin's dependency surface small; the
/// layout is a stable on-disk convention owned by Hugging Face, not a TDCC
/// invention.
///
/// Returns `None` when `path` is not inside `cache_root` or does not match the
/// snapshot layout — the caller then has to supply an explicit
/// `canonical_ref`, which is the honest outcome for a file the mirror cannot
/// identify on its own.
pub fn canonical_ref_from_hf_cache_path(path: &Path, cache_root: &Path) -> Option<ArtifactId> {
    let relative = path.strip_prefix(cache_root).ok()?;
    let mut components = relative.components();
    let repo_folder = components.next()?.as_os_str().to_str()?;
    let repo = repo_folder.strip_prefix("models--")?.replace("--", "/");
    if components.next()?.as_os_str() != std::ffi::OsStr::new("snapshots") {
        return None;
    }
    let revision = components.next()?.as_os_str().to_str()?.to_string();
    let file = components
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?
        .join("/");
    if file.is_empty() {
        return None;
    }
    ArtifactId::parse(&format_canonical_ref(&repo, &revision, &file)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parses_a_canonical_ref_and_round_trips_it() {
        let id = ArtifactId::parse("org/repo@abc123/Qwen3-8B-Q4_K_M.gguf").expect("valid ref");

        assert_eq!(id.repo(), "org/repo");
        assert_eq!(id.revision(), "abc123");
        assert_eq!(id.file(), "Qwen3-8B-Q4_K_M.gguf");
        assert_eq!(id.canonical_ref(), "org/repo@abc123/Qwen3-8B-Q4_K_M.gguf");
    }

    #[test]
    fn derives_the_same_identity_fields_the_host_derives() {
        let id = ArtifactId::parse("org/repo@abc123/Qwen3-8B-Q4_K_M.gguf").expect("valid ref");
        assert_eq!(id.selector().as_deref(), Some("Q4_K_M"));
        assert_eq!(id.distribution_id().as_deref(), Some("Qwen3-8B-Q4_K_M"));
        assert_eq!(id.model_id(), "org/repo:Q4_K_M");

        let sharded =
            ArtifactId::parse("org/repo@abc123/UD-IQ2_M/GLM-5.1-UD-IQ2_M-00001-of-00006.gguf")
                .expect("valid ref");
        assert_eq!(sharded.selector().as_deref(), Some("UD-IQ2_M"));
        assert_eq!(
            sharded.distribution_id().as_deref(),
            Some("GLM-5.1-UD-IQ2_M")
        );
        assert_eq!(sharded.model_id(), "org/repo:UD-IQ2_M");
    }

    #[test]
    fn rejects_refs_that_are_structurally_wrong() {
        assert_eq!(ArtifactId::parse("   "), Err(ArtifactIdError::Empty));
        assert_eq!(
            ArtifactId::parse("org/repo/file.gguf"),
            Err(ArtifactIdError::MissingRevision)
        );
        assert_eq!(
            ArtifactId::parse("org/repo@abc123"),
            Err(ArtifactIdError::MissingFile)
        );
        assert!(matches!(
            ArtifactId::parse("orgrepo@abc123/file.gguf"),
            Err(ArtifactIdError::Repo(_))
        ));
        assert!(matches!(
            ArtifactId::parse("org/repo/extra@abc123/file.gguf"),
            Err(ArtifactIdError::Repo(_))
        ));
        assert!(matches!(
            ArtifactId::parse(&format!("org/repo@abc123/{}", "a".repeat(2048))),
            Err(ArtifactIdError::TooLong(_))
        ));
    }

    #[test]
    fn rejects_traversal_and_absolute_file_paths() {
        for hostile in [
            "org/repo@abc123/../../../etc/passwd",
            "org/repo@abc123/./model.gguf",
            "org/repo@abc123//model.gguf",
            "org/repo@abc123/sub//model.gguf",
        ] {
            assert!(
                matches!(ArtifactId::parse(hostile), Err(ArtifactIdError::File(_))),
                "{hostile} should be rejected"
            );
        }
        // A leading '/' after the revision produces an empty first segment.
        assert!(matches!(
            ArtifactId::parse("org/repo@abc123//etc/passwd"),
            Err(ArtifactIdError::File(_))
        ));
        // Windows drive letters and backslash separators.
        assert!(matches!(
            ArtifactId::parse("org/repo@abc123/C:/Windows/System32/config"),
            Err(ArtifactIdError::File(_))
        ));
        assert!(matches!(
            ArtifactId::parse("org/repo@abc123/sub\\model.gguf"),
            Err(ArtifactIdError::File(_))
        ));
        // Traversal smuggled through the repo or revision segment.
        assert!(matches!(
            ArtifactId::parse("../../etc@abc123/passwd"),
            Err(ArtifactIdError::Repo(_))
        ));
        assert!(matches!(
            ArtifactId::parse("org/repo@../../etc/passwd"),
            Err(ArtifactIdError::Revision(_))
        ));
    }

    #[test]
    fn cache_key_is_stable_hex_and_unique_per_ref() {
        let first = ArtifactId::parse("org/repo@abc123/a.gguf").expect("valid");
        let second = ArtifactId::parse("org/repo@abc123/b.gguf").expect("valid");
        let repeat = ArtifactId::parse("org/repo@abc123/a.gguf").expect("valid");

        assert_eq!(first.cache_key(), repeat.cache_key());
        assert_ne!(first.cache_key(), second.cache_key());
        assert_eq!(first.cache_key().len(), 64);
        assert!(first.cache_key().bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn recovers_identity_from_a_hugging_face_snapshot_path() {
        let root = PathBuf::from("/cache/hub");
        let path = root
            .join("models--org--repo")
            .join("snapshots")
            .join("abc123")
            .join("UD-IQ2_M")
            .join("GLM-5.1-UD-IQ2_M-00001-of-00006.gguf");

        let id = canonical_ref_from_hf_cache_path(&path, &root).expect("snapshot layout");

        assert_eq!(id.repo(), "org/repo");
        assert_eq!(id.revision(), "abc123");
        assert_eq!(id.file(), "UD-IQ2_M/GLM-5.1-UD-IQ2_M-00001-of-00006.gguf");
    }

    #[test]
    fn refuses_to_guess_identity_outside_the_snapshot_layout() {
        let root = PathBuf::from("/cache/hub");
        assert!(
            canonical_ref_from_hf_cache_path(&PathBuf::from("/elsewhere/model.gguf"), &root)
                .is_none()
        );
        assert!(
            canonical_ref_from_hf_cache_path(
                &root.join("models--org--repo").join("model.gguf"),
                &root
            )
            .is_none()
        );
        assert!(
            canonical_ref_from_hf_cache_path(
                &root
                    .join("models--org--repo")
                    .join("snapshots")
                    .join("abc123"),
                &root
            )
            .is_none()
        );
    }
}
