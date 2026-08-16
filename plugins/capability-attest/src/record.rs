//! The signed capability record, and what verifying one is worth.
//!
//! # What a signature here proves, and what it does not
//!
//! A valid signature proves exactly two things: the record was produced by the
//! holder of that node's mesh key, and none of its bytes changed afterwards.
//!
//! It does **not** prove the benchmark ran, or ran honestly. Nothing in a
//! signature can. The numbers are produced on the node's own hardware, by
//! software the node's operator controls, and are then signed by that same
//! operator's key. A node that wants to publish a throughput it never achieved
//! can do so, and the signature will verify. Routing on a signed record is
//! routing on an *attributable claim* — you know exactly whose claim it is, and
//! you can hold that key responsible when reality disagrees — not on a
//! guarantee.
//!
//! Every verification response carries both sentences, in
//! [`WHAT_A_SIGNATURE_PROVES`] and [`WHAT_A_SIGNATURE_DOES_NOT_PROVE`], so the
//! caveat travels with the data instead of living only in this comment.
//!
//! # What verification does check
//!
//! Signature and key binding, whether the pinned prompt still rebuilds to its
//! recorded hash, whether the headline numbers actually follow from the samples
//! in the same record, freshness, and — when a host-produced ownership
//! certificate is attached — who owns the node. That last one is checked with
//! `tdcc_identity::verify_node_ownership`, revocations and all.

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tdcc_identity::{OwnershipStatus, SignedNodeOwnership, TrustStore};

use crate::bench::{RunSample, median_u64, throughput_milli};
use crate::identity::{NodeSigner, decode_endpoint_id, verify_ownership, verify_signature};
use crate::profile::{BenchmarkProfile, sha256_hex};
use crate::vram::VramReading;

pub const RECORD_VERSION: u32 = 1;
pub const ATTESTER: &str = "capability-attest";

/// Domain separation, in the same style as `tdcc_identity`'s node ownership
/// claims: a signature over these bytes can never be replayed as a signature
/// over anything else this key signs.
const SIGNING_DOMAIN_TAG: &[u8] = b"tdcc-capability-attest-v1:";

/// Clock skew allowed before a record is treated as dated in the future.
const FUTURE_SKEW_MS: u64 = 60_000;

pub const WHAT_A_SIGNATURE_PROVES: &str = "This record was produced by the holder of node_endpoint_id's mesh key, and has not been \
     altered since it was signed.";

pub const WHAT_A_SIGNATURE_DOES_NOT_PROVE: &str = "It does not prove the benchmark was run, or run honestly. The measurement happens on \
     hardware the signer controls and is signed by that same signer, so a node can sign numbers \
     it never earned. Treat this as an attributable claim of capability, not as proof of it.";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Measurement {
    /// Every measured run, not just the summary. A verifier can recompute the
    /// headline numbers, and does — see [`measurement_problems`].
    pub runs: Vec<RunSample>,
    /// Thousandths of a token per second: 63_000 is 63 tok/s.
    pub median_output_tokens_per_second_milli: u64,
    pub median_time_to_first_token_us: u64,
    pub warmup_runs_discarded: u32,
    pub vram: VramReading,
}

impl Measurement {
    pub fn from_runs(
        runs: Vec<RunSample>,
        warmup_runs_discarded: u32,
        vram: VramReading,
    ) -> Result<Self> {
        let rates: Vec<u64> = runs
            .iter()
            .map(|run| run.output_tokens_per_second_milli)
            .collect();
        let latencies: Vec<u64> = runs.iter().map(|run| run.time_to_first_token_us).collect();
        Ok(Self {
            median_output_tokens_per_second_milli: median_u64(&rates)
                .ok_or_else(|| anyhow!("no measured runs to summarise"))?,
            median_time_to_first_token_us: median_u64(&latencies)
                .ok_or_else(|| anyhow!("no measured runs to summarise"))?,
            runs,
            warmup_runs_discarded,
            vram,
        })
    }

    /// Human-readable throughput. Derived, never signed.
    pub fn median_output_tokens_per_second(&self) -> f64 {
        self.median_output_tokens_per_second_milli as f64 / 1000.0
    }

    /// Human-readable time to first token. Derived, never signed.
    pub fn median_time_to_first_token_ms(&self) -> f64 {
        self.median_time_to_first_token_us as f64 / 1000.0
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CapabilityClaim {
    pub version: u32,
    /// Derived from the node id, the timestamp, and the profile, so it is
    /// stable for a given record and needs no randomness.
    pub record_id: String,
    /// Lowercase hex of this node's endpoint id — the key the signature below
    /// verifies against, and the id peers route to.
    pub node_endpoint_id: String,
    pub attester: String,
    pub attester_version: String,
    pub measured_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    /// `"loopback"` or `"remote"`. A remote endpoint may have been served by
    /// another machine, so the numbers may not describe this node at all.
    pub endpoint_locality: String,
    pub profile: BenchmarkProfile,
    pub measurement: Measurement,
    /// The host-produced, owner-signed ownership certificate, carried
    /// unchanged. This plugin verifies it; it never issues one.
    pub ownership: Option<SignedNodeOwnership>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SignedCapabilityRecord {
    pub claim: CapabilityClaim,
    /// Ed25519 signature over [`canonical_claim_bytes`], lowercase hex.
    pub signature: String,
}

impl SignedCapabilityRecord {
    pub fn sign(signer: &NodeSigner, claim: CapabilityClaim) -> Result<Self> {
        if claim.node_endpoint_id != signer.endpoint_id_hex() {
            bail!("refusing to sign a claim about a different node");
        }
        let bytes = canonical_claim_bytes(&claim)?;
        let signature = signer.sign_hex(&bytes);
        Ok(Self { claim, signature })
    }
}

/// Build the claim for a completed benchmark.
#[allow(clippy::too_many_arguments)]
pub fn build_claim(
    node_endpoint_id: String,
    attester_version: String,
    measured_at_unix_ms: u64,
    ttl_ms: u64,
    endpoint_locality: String,
    profile: BenchmarkProfile,
    measurement: Measurement,
    ownership: Option<SignedNodeOwnership>,
) -> CapabilityClaim {
    let record_id = record_id(&node_endpoint_id, measured_at_unix_ms, &profile);
    CapabilityClaim {
        version: RECORD_VERSION,
        record_id,
        node_endpoint_id,
        attester: ATTESTER.to_string(),
        attester_version,
        measured_at_unix_ms,
        expires_at_unix_ms: measured_at_unix_ms.saturating_add(ttl_ms),
        endpoint_locality,
        profile,
        measurement,
        ownership,
    }
}

fn record_id(
    node_endpoint_id: &str,
    measured_at_unix_ms: u64,
    profile: &BenchmarkProfile,
) -> String {
    let seed = format!(
        "{node_endpoint_id}:{measured_at_unix_ms}:{}",
        profile.fingerprint()
    );
    sha256_hex(seed.as_bytes())[..32].to_string()
}

// ── Canonical encoding ──────────────────────────────────────────────────────

fn write_string(buffer: &mut Vec<u8>, value: &str) {
    buffer.extend_from_slice(&(value.len() as u64).to_le_bytes());
    buffer.extend_from_slice(value.as_bytes());
}

fn write_optional_string(buffer: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            buffer.push(1);
            write_string(buffer, value);
        }
        None => buffer.push(0),
    }
}

/// The exact bytes the node key signs.
///
/// Scalars are length-prefixed and ordered; the three nested structures
/// (`profile`, `measurement`, `ownership`) contribute their `serde_json`
/// encoding as a single length-prefixed field. That encoding is deterministic
/// because none of those types contains a map — `serde_json` emits struct
/// fields in declaration order — and because no claim field is a float, so
/// every value survives a JSON round trip exactly. Two tests hold that up:
/// `the_signature_covers_every_field_of_the_claim` (nothing sits outside the
/// signature) and `a_claim_contains_no_floating_point_number` (nothing inside
/// it can drift).
pub fn canonical_claim_bytes(claim: &CapabilityClaim) -> Result<Vec<u8>> {
    // Decoding rather than copying the hex rejects a malformed id before it can
    // be signed, and pins the id to its 32 raw bytes rather than to a casing.
    let node_endpoint_id = decode_endpoint_id(&claim.node_endpoint_id)?;

    let mut buffer = Vec::with_capacity(1024);
    buffer.extend_from_slice(SIGNING_DOMAIN_TAG);
    buffer.extend_from_slice(&claim.version.to_le_bytes());
    write_string(&mut buffer, &claim.record_id);
    buffer.extend_from_slice(&node_endpoint_id);
    write_string(&mut buffer, &claim.attester);
    write_string(&mut buffer, &claim.attester_version);
    buffer.extend_from_slice(&claim.measured_at_unix_ms.to_le_bytes());
    buffer.extend_from_slice(&claim.expires_at_unix_ms.to_le_bytes());
    write_string(&mut buffer, &claim.endpoint_locality);
    write_string(&mut buffer, &canonical_json(&claim.profile)?);
    write_string(&mut buffer, &canonical_json(&claim.measurement)?);
    let ownership = claim.ownership.as_ref().map(canonical_json).transpose()?;
    write_optional_string(&mut buffer, ownership.as_deref());
    Ok(buffer)
}

fn canonical_json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|error| anyhow!("cannot canonicalise a claim field: {error}"))
}

// ── Verification ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Fresh,
    Expired,
    /// Measured after the verifier's clock. Either the signer's clock is wrong
    /// or the timestamp is fabricated; neither is a record to route on.
    FromTheFuture,
}

#[derive(Clone, Debug, Serialize)]
pub struct OwnerReport {
    pub certificate_present: bool,
    pub owner_id: Option<String>,
    /// The `OwnershipStatus` name from `tdcc-identity`: `verified`, `expired`,
    /// `revoked_owner`, `untrusted_owner`, and so on.
    pub status: String,
    pub verified: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct VerificationReport {
    pub record_id: String,
    pub node_endpoint_id: String,
    pub profile_fingerprint: String,
    pub signature_valid: bool,
    /// Whether the pinned prompt hash matches the prompt the profile's own
    /// fields rebuild. A record whose prompt cannot be reproduced is not a
    /// reproducible benchmark.
    pub prompt_reproducible: bool,
    /// Whether the headline numbers follow from the samples in the same record.
    pub measurement_self_consistent: bool,
    pub freshness: Freshness,
    pub age_seconds: i64,
    pub endpoint_locality: String,
    pub vram_source: String,
    pub owner: OwnerReport,
    /// Everything wrong with the record, in the order it was found. Empty for a
    /// clean record.
    pub problems: Vec<String>,
    /// The single question a router wants answered. False whenever `problems`
    /// contains anything disqualifying.
    pub usable_for_routing: bool,
    pub what_this_proves: &'static str,
    pub what_this_does_not_prove: &'static str,
}

/// Verify a record end to end.
///
/// `max_age_ms` optionally overrides the record's own expiry with the
/// verifier's policy — a record can claim a 30-day lifetime; the node reading it
/// does not have to agree.
pub fn verify(
    record: &SignedCapabilityRecord,
    trust_store: &TrustStore,
    now_unix_ms: u64,
    max_age_ms: Option<u64>,
) -> VerificationReport {
    let claim = &record.claim;
    let mut problems = Vec::new();

    if claim.version != RECORD_VERSION {
        problems.push(format!(
            "record version {} is not supported (this plugin understands version {RECORD_VERSION})",
            claim.version
        ));
    }
    if claim.attester != ATTESTER {
        problems.push(format!(
            "record was produced by {:?}, not {ATTESTER}",
            claim.attester
        ));
    }

    let signature_valid = match canonical_claim_bytes(claim) {
        Ok(bytes) => match verify_signature(&claim.node_endpoint_id, &bytes, &record.signature) {
            Ok(()) => true,
            Err(reason) => {
                problems.push(reason);
                false
            }
        },
        Err(error) => {
            problems.push(error.to_string());
            false
        }
    };

    let prompt_reproducible = match claim.profile.prompt() {
        Ok(_) => true,
        Err(error) => {
            problems.push(error.to_string());
            false
        }
    };

    let measurement_issues = measurement_problems(&claim.measurement);
    let measurement_self_consistent = measurement_issues.is_empty();
    problems.extend(measurement_issues);

    let age_ms = i128::from(now_unix_ms) - i128::from(claim.measured_at_unix_ms);
    let freshness = if age_ms < -i128::from(FUTURE_SKEW_MS) {
        problems.push("record is dated in the future".to_string());
        Freshness::FromTheFuture
    } else if claim.expires_at_unix_ms <= now_unix_ms {
        problems.push(format!(
            "record expired at {} (now {now_unix_ms})",
            claim.expires_at_unix_ms
        ));
        Freshness::Expired
    } else if let Some(max_age_ms) = max_age_ms
        && age_ms > i128::from(max_age_ms)
    {
        problems.push(format!(
            "record is {}s old, older than the {}s the caller asked for",
            age_ms / 1000,
            max_age_ms / 1000
        ));
        Freshness::Expired
    } else {
        Freshness::Fresh
    };

    if claim.endpoint_locality != "loopback" {
        problems.push(format!(
            "measured against a {} endpoint, which may have been served by another machine",
            claim.endpoint_locality
        ));
    }

    let owner = match decode_endpoint_id(&claim.node_endpoint_id) {
        Ok(node_endpoint_id) => {
            let summary = verify_ownership(
                claim.ownership.as_ref(),
                &node_endpoint_id,
                trust_store,
                now_unix_ms,
            );
            if claim.ownership.is_some() && !summary.verified {
                problems.push(format!(
                    "attached owner certificate did not verify: {}",
                    status_name(&summary.status)
                ));
            }
            OwnerReport {
                certificate_present: claim.ownership.is_some(),
                owner_id: summary.owner_id.clone(),
                status: status_name(&summary.status),
                verified: summary.verified,
            }
        }
        Err(_) => OwnerReport {
            certificate_present: claim.ownership.is_some(),
            owner_id: None,
            status: "unchecked".to_string(),
            verified: false,
        },
    };

    // Ownership is attribution, not a precondition: an unowned node can still
    // publish an honest, verifiable measurement. Everything else is required.
    let usable_for_routing = signature_valid
        && prompt_reproducible
        && measurement_self_consistent
        && freshness == Freshness::Fresh
        && claim.version == RECORD_VERSION;

    VerificationReport {
        record_id: claim.record_id.clone(),
        node_endpoint_id: claim.node_endpoint_id.clone(),
        profile_fingerprint: claim.profile.fingerprint(),
        signature_valid,
        prompt_reproducible,
        measurement_self_consistent,
        freshness,
        age_seconds: (age_ms / 1000) as i64,
        endpoint_locality: claim.endpoint_locality.clone(),
        vram_source: claim.measurement.vram.source.clone(),
        owner,
        problems,
        usable_for_routing,
        what_this_proves: WHAT_A_SIGNATURE_PROVES,
        what_this_does_not_prove: WHAT_A_SIGNATURE_DOES_NOT_PROVE,
    }
}

/// Recompute the summary numbers from the samples they claim to summarise.
///
/// A signature stops anyone *else* editing the headline throughput. It does not
/// stop the signer from publishing a summary that its own samples contradict,
/// which is the cheapest possible way to inflate a record. So the summary is
/// derived again here and compared.
pub fn measurement_problems(measurement: &Measurement) -> Vec<String> {
    let mut problems = Vec::new();
    if measurement.runs.is_empty() {
        problems.push("record contains no measured runs".to_string());
        return problems;
    }

    for run in &measurement.runs {
        match throughput_milli(run.output_tokens, run.time_to_first_token_us, run.total_us) {
            // Exact equality, not a tolerance: every number in a claim is an
            // integer, so the recomputation either matches or the record is
            // claiming something its own samples do not support.
            Ok(expected) if expected != run.output_tokens_per_second_milli => {
                problems.push(format!(
                    "run {} reports {} milli-tok/s, but its own timings give {expected}",
                    run.run, run.output_tokens_per_second_milli
                ));
            }
            Ok(_) => {}
            Err(reason) => problems.push(format!("run {} is not measurable: {reason}", run.run)),
        }
    }

    let rates: Vec<u64> = measurement
        .runs
        .iter()
        .map(|run| run.output_tokens_per_second_milli)
        .collect();
    if let Some(expected) = median_u64(&rates)
        && expected != measurement.median_output_tokens_per_second_milli
    {
        problems.push(format!(
            "median_output_tokens_per_second_milli is {}, but the runs give {expected}",
            measurement.median_output_tokens_per_second_milli
        ));
    }

    let latencies: Vec<u64> = measurement
        .runs
        .iter()
        .map(|run| run.time_to_first_token_us)
        .collect();
    if let Some(expected) = median_u64(&latencies)
        && expected != measurement.median_time_to_first_token_us
    {
        problems.push(format!(
            "median_time_to_first_token_us is {}, but the runs give {expected}",
            measurement.median_time_to_first_token_us
        ));
    }

    problems
}

fn status_name(status: &OwnershipStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::TokenCountSource;
    use crate::identity::NodeSigner;
    use crate::profile::DEFAULT_FILLER_SENTENCE;

    const NOW: u64 = 1_800_000_000_000;

    struct Fixture {
        _directory: tempfile::TempDir,
        signer: NodeSigner,
    }

    fn fixture(seed: u8) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key");
        std::fs::write(&path, hex::encode([seed; 32])).unwrap();
        let signer = NodeSigner::load(Some(path.to_str().unwrap())).unwrap();
        Fixture {
            _directory: directory,
            signer,
        }
    }

    fn profile() -> BenchmarkProfile {
        BenchmarkProfile::build(
            "demo-model".into(),
            64,
            32,
            0.0,
            1.0,
            42,
            1,
            3,
            DEFAULT_FILLER_SENTENCE.into(),
        )
        .unwrap()
    }

    fn sample(run: u32, ttft_us: u64, total_us: u64, tokens: u64) -> RunSample {
        RunSample {
            run,
            time_to_first_token_us: ttft_us,
            total_us,
            output_tokens: tokens,
            output_tokens_per_second_milli: throughput_milli(tokens, ttft_us, total_us).unwrap(),
            token_count_source: TokenCountSource::ServerUsage,
            prompt_tokens: Some(260),
        }
    }

    fn measurement() -> Measurement {
        Measurement::from_runs(
            vec![
                sample(1, 100_000, 1_100_000, 32),
                sample(2, 110_000, 1_150_000, 32),
                sample(3, 90_000, 1_050_000, 32),
            ],
            1,
            VramReading::unavailable("test"),
        )
        .unwrap()
    }

    fn record(fixture: &Fixture) -> SignedCapabilityRecord {
        let claim = build_claim(
            fixture.signer.endpoint_id_hex().to_string(),
            "0.1.0".into(),
            NOW,
            7_200_000,
            "loopback".into(),
            profile(),
            measurement(),
            None,
        );
        SignedCapabilityRecord::sign(&fixture.signer, claim).unwrap()
    }

    /// A record carrying a real, owner-signed ownership certificate, produced
    /// with the same `tdcc-identity` call the host uses.
    fn owned_record(fixture: &Fixture) -> (SignedCapabilityRecord, tdcc_identity::OwnerKeypair) {
        let owner = tdcc_identity::OwnerKeypair::generate();
        let node_endpoint_id = decode_endpoint_id(fixture.signer.endpoint_id_hex()).unwrap();
        let ownership = tdcc_identity::sign_node_ownership(
            &owner,
            &node_endpoint_id,
            NOW + 86_400_000,
            Some("studio".into()),
            Some("studio-host".into()),
        )
        .unwrap();
        let claim = build_claim(
            fixture.signer.endpoint_id_hex().to_string(),
            "0.1.0".into(),
            NOW,
            7_200_000,
            "loopback".into(),
            profile(),
            measurement(),
            Some(ownership),
        );
        (
            SignedCapabilityRecord::sign(&fixture.signer, claim).unwrap(),
            owner,
        )
    }

    #[test]
    fn a_freshly_signed_record_verifies_cleanly() {
        let fixture = fixture(1);
        let signed = record(&fixture);

        let report = verify(&signed, &TrustStore::default(), NOW + 1_000, None);

        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert!(report.signature_valid);
        assert!(report.prompt_reproducible);
        assert!(report.measurement_self_consistent);
        assert_eq!(report.freshness, Freshness::Fresh);
        assert!(report.usable_for_routing);
    }

    #[test]
    fn every_verification_response_carries_the_caveat() {
        let fixture = fixture(1);
        let report = verify(&record(&fixture), &TrustStore::default(), NOW, None);

        assert_eq!(report.what_this_proves, WHAT_A_SIGNATURE_PROVES);
        assert_eq!(
            report.what_this_does_not_prove,
            WHAT_A_SIGNATURE_DOES_NOT_PROVE
        );
        assert!(
            WHAT_A_SIGNATURE_DOES_NOT_PROVE.contains("never earned"),
            "the caveat must say plainly that a node can sign numbers it did not earn"
        );
    }

    #[test]
    fn the_record_round_trips_through_json_with_a_stable_signature() {
        let fixture = fixture(2);
        let signed = record(&fixture);

        let encoded = serde_json::to_string(&signed).unwrap();
        let decoded: SignedCapabilityRecord = serde_json::from_str(&encoded).unwrap();

        assert_eq!(
            canonical_claim_bytes(&signed.claim).unwrap(),
            canonical_claim_bytes(&decoded.claim).unwrap(),
            "canonical bytes must survive a JSON round trip, or peers cannot verify"
        );
        assert!(verify(&decoded, &TrustStore::default(), NOW, None).signature_valid);
    }

    #[test]
    fn the_signature_covers_every_field_of_the_claim() {
        // The property that keeps a future field from being added outside the
        // signature: bump any one value in the serialised claim and the bytes
        // the key signs must change.
        let fixture = fixture(3);
        // Signed with an ownership certificate attached so that field is a real
        // object rather than a null the mutation below could not touch.
        let (signed, _owner) = owned_record(&fixture);
        let baseline = canonical_claim_bytes(&signed.claim).unwrap();

        let encoded = serde_json::to_value(&signed.claim).unwrap();
        let keys: Vec<String> = encoded
            .as_object()
            .unwrap()
            .keys()
            .map(String::to_owned)
            .collect();
        assert_eq!(keys.len(), 11, "claim fields changed; revisit the encoding");

        for key in keys {
            let mut mutated = encoded.clone();
            let field = mutated.get_mut(&key).unwrap();
            if !bump(field) {
                panic!("no way to mutate claim field {key}; extend `bump`");
            }
            let mutated: CapabilityClaim =
                serde_json::from_value(mutated).unwrap_or_else(|error| {
                    panic!("mutating {key} produced an invalid claim: {error}")
                });
            let bytes = canonical_claim_bytes(&mutated).unwrap_or_default();
            assert_ne!(
                bytes, baseline,
                "changing {key} did not change the signed bytes"
            );
        }
    }

    /// Change a JSON value into a different valid value of the same type.
    fn bump(value: &mut serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(text) => {
                // Keep hex fields hex so they still parse.
                if text.chars().all(|character| character.is_ascii_hexdigit()) && !text.is_empty() {
                    let flipped = if text.starts_with('a') { 'b' } else { 'a' };
                    text.replace_range(0..1, &flipped.to_string());
                } else {
                    text.push('x');
                }
                true
            }
            serde_json::Value::Number(number) => {
                let bumped = number.as_f64().unwrap_or(0.0) + 1.0;
                *value = serde_json::json!(bumped as u64);
                true
            }
            serde_json::Value::Bool(flag) => {
                *flag = !*flag;
                true
            }
            serde_json::Value::Array(items) => items.iter_mut().any(bump),
            serde_json::Value::Object(fields) => fields.values_mut().any(bump),
            serde_json::Value::Null => false,
        }
    }

    #[test]
    fn a_record_signed_by_another_node_does_not_verify() {
        let mine = fixture(4);
        let theirs = fixture(5);

        let mut signed = record(&mine);
        signed.claim.node_endpoint_id = theirs.signer.endpoint_id_hex().to_string();

        let report = verify(&signed, &TrustStore::default(), NOW, None);

        assert!(!report.signature_valid);
        assert!(!report.usable_for_routing);
    }

    #[test]
    fn signing_a_claim_about_another_node_is_refused_outright() {
        let mine = fixture(6);
        let theirs = fixture(7);

        let claim = build_claim(
            theirs.signer.endpoint_id_hex().to_string(),
            "0.1.0".into(),
            NOW,
            7_200_000,
            "loopback".into(),
            profile(),
            measurement(),
            None,
        );

        let error = SignedCapabilityRecord::sign(&mine.signer, claim).unwrap_err();
        assert!(error.to_string().contains("different node"), "{error}");
    }

    #[test]
    fn an_inflated_headline_is_caught_even_though_it_is_correctly_signed() {
        // The cheapest dishonest record: sign real samples, publish a better
        // median. The signature is valid; the arithmetic is not.
        let fixture = fixture(8);
        let mut claim = build_claim(
            fixture.signer.endpoint_id_hex().to_string(),
            "0.1.0".into(),
            NOW,
            7_200_000,
            "loopback".into(),
            profile(),
            measurement(),
            None,
        );
        claim.measurement.median_output_tokens_per_second_milli *= 4;
        let signed = SignedCapabilityRecord::sign(&fixture.signer, claim).unwrap();

        let report = verify(&signed, &TrustStore::default(), NOW, None);

        assert!(report.signature_valid, "the signature really is valid");
        assert!(!report.measurement_self_consistent);
        assert!(!report.usable_for_routing);
        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("median_output_tokens_per_second")),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn a_run_whose_rate_does_not_follow_from_its_timings_is_caught() {
        let mut measurement = measurement();
        measurement.runs[1].output_tokens_per_second_milli = 9_999_000;

        let problems = measurement_problems(&measurement);

        assert!(
            problems.iter().any(|problem| problem.contains("run 2")),
            "{problems:?}"
        );
    }

    #[test]
    fn expiry_and_a_caller_supplied_maximum_age_both_apply() {
        let fixture = fixture(9);
        let signed = record(&fixture);

        let expired = verify(&signed, &TrustStore::default(), NOW + 7_200_001, None);
        assert_eq!(expired.freshness, Freshness::Expired);
        assert!(!expired.usable_for_routing);

        let too_old_for_caller = verify(
            &signed,
            &TrustStore::default(),
            NOW + 600_000,
            Some(300_000),
        );
        assert_eq!(too_old_for_caller.freshness, Freshness::Expired);
        assert!(
            too_old_for_caller
                .problems
                .iter()
                .any(|problem| problem.contains("the caller asked for"))
        );

        let acceptable = verify(&signed, &TrustStore::default(), NOW + 60_000, Some(300_000));
        assert_eq!(acceptable.freshness, Freshness::Fresh);
    }

    #[test]
    fn a_record_from_the_future_is_not_routable() {
        let fixture = fixture(10);
        let signed = record(&fixture);

        let report = verify(&signed, &TrustStore::default(), NOW - 600_000, None);

        assert_eq!(report.freshness, Freshness::FromTheFuture);
        assert!(!report.usable_for_routing);
    }

    #[test]
    fn a_remote_endpoint_is_reported_as_a_problem_but_the_record_still_verifies() {
        let fixture = fixture(11);
        let claim = build_claim(
            fixture.signer.endpoint_id_hex().to_string(),
            "0.1.0".into(),
            NOW,
            7_200_000,
            "remote".into(),
            profile(),
            measurement(),
            None,
        );
        let signed = SignedCapabilityRecord::sign(&fixture.signer, claim).unwrap();

        let report = verify(&signed, &TrustStore::default(), NOW, None);

        assert!(report.signature_valid);
        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("another machine")),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn an_unowned_node_still_produces_a_routable_record() {
        let fixture = fixture(12);

        let report = verify(&record(&fixture), &TrustStore::default(), NOW, None);

        assert!(!report.owner.certificate_present);
        assert_eq!(report.owner.status, "unsigned");
        assert!(
            report.usable_for_routing,
            "ownership is attribution, not a precondition for a measurement"
        );
    }

    #[test]
    fn a_carried_owner_certificate_is_verified_against_the_same_node() {
        let fixture = fixture(20);
        let (signed, _owner) = owned_record(&fixture);

        let report = verify(&signed, &TrustStore::default(), NOW, None);

        assert!(report.owner.certificate_present);
        assert_eq!(report.owner.status, "verified");
        assert!(report.owner.verified);
        assert!(report.owner.owner_id.is_some());
        assert!(report.problems.is_empty(), "{:?}", report.problems);
    }

    #[test]
    fn a_revoked_owner_is_reported_even_though_the_measurement_signature_is_good() {
        let fixture = fixture(21);
        let (signed, owner) = owned_record(&fixture);
        let mut trust_store = TrustStore::default();
        trust_store.revoke_owner(owner.owner_id(), Some("key rotated out".into()));

        let report = verify(&signed, &trust_store, NOW, None);

        assert!(report.signature_valid);
        assert_eq!(report.owner.status, "revoked_owner");
        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("owner certificate")),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn an_owner_certificate_for_a_different_node_does_not_transfer() {
        let mine = fixture(22);
        let theirs = fixture(23);
        let (_, owner) = owned_record(&theirs);
        // A certificate the owner really signed — for somebody else's node.
        let foreign = tdcc_identity::sign_node_ownership(
            &owner,
            &decode_endpoint_id(theirs.signer.endpoint_id_hex()).unwrap(),
            NOW + 86_400_000,
            None,
            None,
        )
        .unwrap();
        let claim = build_claim(
            mine.signer.endpoint_id_hex().to_string(),
            "0.1.0".into(),
            NOW,
            7_200_000,
            "loopback".into(),
            profile(),
            measurement(),
            Some(foreign),
        );
        let signed = SignedCapabilityRecord::sign(&mine.signer, claim).unwrap();

        let report = verify(&signed, &TrustStore::default(), NOW, None);

        assert!(report.signature_valid);
        assert_eq!(report.owner.status, "mismatched_node_id");
        assert!(!report.owner.verified);
    }

    #[test]
    fn a_claim_contains_no_floating_point_number() {
        // The rule the whole signing scheme depends on. `serde_json` does not
        // round-trip `f64` exactly — a real measurement produced
        // `31.165399999999998` on the way out and `31.1654` on the way back,
        // which broke verification for a record that had been signed correctly.
        // Integers cannot do that, so nothing in a claim is allowed to be one.
        let fixture = fixture(13);
        let (signed, _owner) = owned_record(&fixture);
        let encoded = serde_json::to_value(&signed.claim).unwrap();

        let mut floats = Vec::new();
        collect_floats(&encoded, String::new(), &mut floats);

        assert!(
            floats.is_empty(),
            "a signed claim must carry no floating-point value, found: {floats:?}"
        );
    }

    fn collect_floats(value: &serde_json::Value, path: String, found: &mut Vec<String>) {
        match value {
            serde_json::Value::Number(number) => {
                if number.as_i64().is_none() && number.as_u64().is_none() {
                    found.push(format!("{path} = {number}"));
                }
            }
            serde_json::Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    collect_floats(item, format!("{path}[{index}]"), found);
                }
            }
            serde_json::Value::Object(fields) => {
                for (key, field) in fields {
                    collect_floats(field, format!("{path}.{key}"), found);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn the_domain_tag_keeps_these_bytes_out_of_other_signature_schemes() {
        let fixture = fixture(14);
        let signed = record(&fixture);
        let bytes = canonical_claim_bytes(&signed.claim).unwrap();

        assert!(bytes.starts_with(SIGNING_DOMAIN_TAG));
        assert_ne!(
            SIGNING_DOMAIN_TAG, b"tdcc-node-ownership-v1:",
            "must not collide with the node ownership domain in tdcc-identity"
        );
    }
}
