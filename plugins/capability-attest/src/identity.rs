//! Keys, borrowed rather than invented.
//!
//! Everything here goes through `tdcc-identity`: the node key path and its
//! loader, the ownership certificate format, the trust store, and the
//! verification routine. This plugin defines no key file, no key format, and no
//! key location of its own.
//!
//! # Which key signs, and why it is the small one
//!
//! Records are signed with the **node key** (`<TDCC_HOME>/.tdcc/key`), not the
//! owner key. Two reasons, both deliberate:
//!
//! * The subject of a capability record is a node, and the node key's public
//!   half *is* the endpoint id peers route to. A verifier needs nothing beyond
//!   the record and the peer id it already has.
//! * The owner key can sign node ownership certificates. A plugin that held it
//!   could mint those. This plugin never opens the owner keystore.
//!
//! Attribution still works: when the host has written a `node-ownership.json`,
//! the record carries that host-produced, owner-signed certificate unchanged,
//! and verification checks it with `tdcc_identity::verify_node_ownership`
//! against the local trust store. This plugin never signs an ownership claim.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use tdcc_identity::{
    OwnershipSummary, SignedNodeOwnership, TrustStore, default_node_key_path,
    default_node_ownership_path, default_trust_store_path, load_node_key_bytes_from_path,
    load_node_ownership, load_trust_store, verify_node_ownership,
};

/// The node signing key, held for the life of the process.
///
/// `SigningKey` zeroizes its secret on drop; nothing here copies the bytes back
/// out, and the secret never reaches a log line, a record, or a tool response.
pub struct NodeSigner {
    signing: SigningKey,
    endpoint_id_hex: String,
}

impl std::fmt::Debug for NodeSigner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Derived Debug on a key type is how secrets end up in logs.
        formatter
            .debug_struct("NodeSigner")
            .field("endpoint_id_hex", &self.endpoint_id_hex)
            .finish_non_exhaustive()
    }
}

impl NodeSigner {
    /// Load the node key, defaulting to the path `tdcc-identity` resolves.
    pub fn load(path_override: Option<&str>) -> Result<Self> {
        let path: PathBuf = match path_override {
            Some(path) => PathBuf::from(path),
            None => default_node_key_path()
                .context("resolving the default node key path (is TDCC_HOME set correctly?)")?,
        };
        let bytes = load_node_key_bytes_from_path(&path).with_context(|| {
            format!(
                "reading the node key at {}. Start `tdcc` once, or run `tdcc auth init`, so the \
                 node identity exists before attestation runs",
                path.display()
            )
        })?;
        let signing = SigningKey::from_bytes(&bytes);
        let endpoint_id_hex = hex::encode(signing.verifying_key().as_bytes());
        Ok(Self {
            signing,
            endpoint_id_hex,
        })
    }

    /// This node's endpoint id, lowercase hex — the same encoding the host uses
    /// for `MeshPeer.peer_id` and for `node_endpoint_id` in ownership claims.
    pub fn endpoint_id_hex(&self) -> &str {
        &self.endpoint_id_hex
    }

    pub fn sign_hex(&self, message: &[u8]) -> String {
        hex::encode(self.signing.sign(message).to_bytes())
    }
}

/// Check a detached signature against an endpoint id.
///
/// Uses `verify_strict`, which rejects small-order public keys and the
/// malleable signature forms that plain `verify` accepts.
pub fn verify_signature(
    endpoint_id_hex: &str,
    message: &[u8],
    signature_hex: &str,
) -> Result<(), String> {
    let endpoint_id = decode_endpoint_id(endpoint_id_hex).map_err(|error| error.to_string())?;
    let verifying_key = VerifyingKey::from_bytes(&endpoint_id)
        .map_err(|error| format!("node_endpoint_id is not a valid public key: {error}"))?;
    let signature_bytes =
        hex::decode(signature_hex).map_err(|error| format!("signature is not hex: {error}"))?;
    let signature_bytes: [u8; 64] = signature_bytes
        .try_into()
        .map_err(|_| "signature must be 64 bytes".to_string())?;
    verifying_key
        .verify_strict(message, &Signature::from_bytes(&signature_bytes))
        .map_err(|_| "signature does not verify against node_endpoint_id".to_string())
}

pub fn decode_endpoint_id(endpoint_id_hex: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(endpoint_id_hex)
        .map_err(|error| anyhow!("node_endpoint_id is not hex: {error}"))?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("node_endpoint_id must be 32 bytes"))
}

/// The host-produced ownership certificate and trust store, if present.
///
/// Both are optional. A node with no owner certificate still produces valid,
/// verifiable capability records — they just carry no statement about who owns
/// the machine.
#[derive(Clone, Debug, Default)]
pub struct OwnerAttribution {
    pub ownership: Option<SignedNodeOwnership>,
    pub trust_store: TrustStore,
    /// Why the certificate is absent, when it is. Surfaced in `status` so an
    /// operator is not left guessing.
    pub note: Option<String>,
}

impl OwnerAttribution {
    pub fn load() -> Self {
        let (ownership, note) = match default_node_ownership_path() {
            Ok(path) if path.exists() => match load_node_ownership(&path) {
                Ok(ownership) => (Some(ownership), None),
                Err(error) => (
                    None,
                    Some(format!("{} could not be read: {error}", path.display())),
                ),
            },
            Ok(path) => (
                None,
                Some(format!(
                    "no owner certificate at {}; records will carry no ownership attribution",
                    path.display()
                )),
            ),
            Err(error) => (None, Some(format!("owner certificate path: {error}"))),
        };

        let trust_store = default_trust_store_path()
            .ok()
            .and_then(|path| load_trust_store(&path).ok())
            .unwrap_or_default();

        Self {
            ownership,
            trust_store,
            note,
        }
    }
}

/// Verify a carried ownership certificate against the node it claims to cover.
///
/// Delegates entirely to `tdcc_identity::verify_node_ownership`, so expiry,
/// revoked owners, revoked certificates, revoked node ids, and the local trust
/// policy are all applied exactly as the host applies them.
pub fn verify_ownership(
    ownership: Option<&SignedNodeOwnership>,
    node_endpoint_id: &[u8; 32],
    trust_store: &TrustStore,
    now_unix_ms: u64,
) -> OwnershipSummary {
    verify_node_ownership(
        ownership,
        node_endpoint_id,
        trust_store,
        trust_store.policy,
        now_unix_ms,
    )
}

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_unix_ms() -> Result<u64> {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| anyhow!("system clock is before the Unix epoch"))?;
    u64::try_from(since_epoch.as_millis())
        .map_err(|_| anyhow!("system clock is implausibly far in the future"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer_from(seed: [u8; 32], directory: &std::path::Path) -> NodeSigner {
        let path = directory.join("key");
        std::fs::write(&path, hex::encode(seed)).unwrap();
        NodeSigner::load(Some(path.to_str().unwrap())).unwrap()
    }

    #[test]
    fn the_endpoint_id_is_the_public_half_of_the_node_key() {
        let directory = tempfile::tempdir().unwrap();
        let signer = signer_from([7u8; 32], directory.path());

        let expected = hex::encode(
            SigningKey::from_bytes(&[7u8; 32])
                .verifying_key()
                .as_bytes(),
        );
        assert_eq!(signer.endpoint_id_hex(), expected);
        assert_eq!(signer.endpoint_id_hex().len(), 64);
    }

    #[test]
    fn a_signature_verifies_against_the_endpoint_id_and_nothing_else() {
        let directory = tempfile::tempdir().unwrap();
        let signer = signer_from([3u8; 32], directory.path());
        let other = signer_from([4u8; 32], directory.path());

        let signature = signer.sign_hex(b"capability record");

        assert!(
            verify_signature(signer.endpoint_id_hex(), b"capability record", &signature).is_ok()
        );
        assert!(
            verify_signature(other.endpoint_id_hex(), b"capability record", &signature).is_err(),
            "a record must not verify under a different node's id"
        );
        assert!(
            verify_signature(signer.endpoint_id_hex(), b"other bytes", &signature).is_err(),
            "a signature must not cover bytes it did not sign"
        );
    }

    #[test]
    fn malformed_signatures_and_ids_are_rejected_with_a_reason() {
        let directory = tempfile::tempdir().unwrap();
        let signer = signer_from([9u8; 32], directory.path());
        let signature = signer.sign_hex(b"payload");

        assert!(
            verify_signature("not-hex", b"payload", &signature)
                .unwrap_err()
                .contains("hex")
        );
        assert!(
            verify_signature("aabb", b"payload", &signature)
                .unwrap_err()
                .contains("32 bytes")
        );
        assert!(
            verify_signature(signer.endpoint_id_hex(), b"payload", "aabb")
                .unwrap_err()
                .contains("64 bytes")
        );
    }

    #[test]
    fn a_missing_node_key_says_how_to_create_one() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("absent");

        let error = NodeSigner::load(Some(missing.to_str().unwrap())).unwrap_err();

        assert!(format!("{error:#}").contains("tdcc auth init"), "{error:#}");
    }

    #[test]
    fn the_signer_debug_output_carries_no_secret() {
        let directory = tempfile::tempdir().unwrap();
        let signer = signer_from([5u8; 32], directory.path());

        let rendered = format!("{signer:?}");

        assert!(rendered.contains(signer.endpoint_id_hex()));
        assert!(!rendered.contains(&hex::encode([5u8; 32])));
    }
}
