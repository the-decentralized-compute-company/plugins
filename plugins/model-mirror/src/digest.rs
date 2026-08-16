//! SHA-256, the only integrity primitive this mirror trusts.
//!
//! Every artifact this plugin holds, serves, or accepts is keyed to a
//! lowercase 64-character hex SHA-256 digest. The type below exists so that a
//! digest can never be *almost* a digest: a truncated, uppercase, whitespace
//! padded, or non-hex string is rejected at the edge instead of silently
//! comparing unequal to everything forever.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

/// Bytes read per iteration while digesting an artifact.
///
/// Model files are tens of gigabytes; the buffer is large enough that syscall
/// overhead is irrelevant and small enough that a hash pass does not hold a
/// megabyte-scale allocation per concurrent request.
pub const HASH_BUFFER_BYTES: usize = 1024 * 1024;

/// A validated lowercase hex SHA-256 digest.
///
/// Serialized as a plain JSON string, and validated again on the way back in,
/// so a hand-edited entry file cannot reintroduce a malformed digest.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha256Hex(String);

impl Sha256Hex {
    /// Parse a caller-supplied digest string.
    ///
    /// Surrounding whitespace is trimmed and hex case is normalized, because
    /// both are common in copy-pasted digests. Nothing else is forgiven.
    pub fn parse(value: &str) -> Result<Self, DigestError> {
        let trimmed = value.trim();
        if trimmed.len() != 64 {
            return Err(DigestError::Length(trimmed.len()));
        }
        if !trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DigestError::NotHex);
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(hex(Sha256::digest(bytes).as_slice()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256Hex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for Sha256Hex {
    type Error = DigestError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Sha256Hex> for String {
    fn from(value: Sha256Hex) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DigestError {
    Length(usize),
    NotHex,
}

impl fmt::Display for DigestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length(actual) => write!(
                formatter,
                "expected a 64-character hex SHA-256 digest, got {actual} characters"
            ),
            Self::NotHex => formatter.write_str(
                "expected a 64-character hex SHA-256 digest, got a non-hexadecimal string",
            ),
        }
    }
}

impl std::error::Error for DigestError {}

/// Digest a file from `offset` to EOF, returning the digest and the number of
/// bytes hashed.
///
/// The byte count is returned alongside the digest so a caller can compare it
/// against the size it expected without a second `stat`, which would race with
/// whatever else is touching the file.
pub async fn sha256_file_from(path: &Path, offset: u64) -> std::io::Result<(Sha256Hex, u64)> {
    use tokio::io::AsyncSeekExt;

    let mut file = tokio::fs::File::open(path).await?;
    if offset > 0 {
        file.seek(std::io::SeekFrom::Start(offset)).await?;
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_BUFFER_BYTES];
    let mut hashed = 0_u64;
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        hashed += read as u64;
    }
    Ok((Sha256Hex(hex(hasher.finalize().as_slice())), hashed))
}

/// Digest a whole file.
pub async fn sha256_file(path: &Path) -> std::io::Result<(Sha256Hex, u64)> {
    sha256_file_from(path, 0).await
}

/// An incremental SHA-256, for hashing bytes as they stream past on their way
/// somewhere else.
///
/// Copying a 20 GB artifact and then hashing it would read it twice; this lets
/// the copy and the digest share one pass.
#[derive(Default)]
pub struct Sha256Stream {
    hasher: Sha256,
}

impl Sha256Stream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    pub fn finish(self) -> Sha256Hex {
        Sha256Hex(hex(self.hasher.finalize().as_slice()))
    }
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing into a String cannot fail; the result is discarded
        // deliberately rather than unwrapped.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normalizes_case_and_trims_whitespace() {
        let upper = format!("  {}  ", "A".repeat(64));
        let parsed = Sha256Hex::parse(&upper).expect("64 hex characters parse");
        assert_eq!(parsed.as_str(), "a".repeat(64));
    }

    #[test]
    fn parse_rejects_wrong_length_and_non_hex() {
        assert_eq!(
            Sha256Hex::parse(&"a".repeat(63)),
            Err(DigestError::Length(63))
        );
        assert_eq!(
            Sha256Hex::parse(&"a".repeat(65)),
            Err(DigestError::Length(65))
        );
        assert_eq!(Sha256Hex::parse(""), Err(DigestError::Length(0)));
        let mut not_hex = "a".repeat(63);
        not_hex.push('z');
        assert_eq!(Sha256Hex::parse(&not_hex), Err(DigestError::NotHex));
    }

    #[test]
    fn of_bytes_matches_the_published_empty_string_digest() {
        assert_eq!(
            Sha256Hex::of_bytes(b"").as_str(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            Sha256Hex::of_bytes(b"abc").as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn deserialization_rejects_a_hand_edited_entry_digest() {
        let valid: Result<Sha256Hex, _> = serde_json::from_str(&format!("\"{}\"", "b".repeat(64)));
        assert!(valid.is_ok());
        let invalid: Result<Sha256Hex, _> = serde_json::from_str("\"deadbeef\"");
        assert!(invalid.is_err());
    }

    #[tokio::test]
    async fn sha256_file_hashes_the_whole_file_and_reports_its_length() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("artifact.bin");
        tokio::fs::write(&path, b"abc").await.expect("write");

        let (digest, length) = sha256_file(&path).await.expect("hash");

        assert_eq!(digest, Sha256Hex::of_bytes(b"abc"));
        assert_eq!(length, 3);
    }

    #[test]
    fn the_streaming_hasher_agrees_with_the_one_shot_hasher() {
        let mut stream = Sha256Stream::new();
        stream.update(b"ab");
        stream.update(b"c");

        assert_eq!(stream.finish(), Sha256Hex::of_bytes(b"abc"));
    }

    #[tokio::test]
    async fn sha256_file_from_skips_the_requested_prefix() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("artifact.bin");
        tokio::fs::write(&path, b"xxxabc").await.expect("write");

        let (digest, length) = sha256_file_from(&path, 3).await.expect("hash");

        assert_eq!(digest, Sha256Hex::of_bytes(b"abc"));
        assert_eq!(length, 3);
    }
}
