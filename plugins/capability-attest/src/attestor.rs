//! Plugin state and the benchmark orchestration around it.
//!
//! One [`Attestor`] is shared by every handler. It is cheap to clone — the
//! runtime requires `Plugin: Clone`, and every clone shares the same `Arc`s —
//! and it holds three things: the resolved startup (or the reason startup
//! failed), the mutable state behind a mutex, and a lock that keeps two
//! benchmarks from overlapping.
//!
//! Nothing here fails silently. A misconfigured plugin still starts, so the
//! operator can read the reason out of `status` in the console, but `health`
//! reports it as unhealthy and every tool returns the same message.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use url::Url;

use crate::activity::{Deferral, Schedule, SchedulePolicy, contention_deferral, schedule_deferral};
use crate::bench::{RunSample, build_client, chat_request_body, measure_contention, stream_once};
use crate::config::{AttestConfig, EnvMap, resolve};
use crate::identity::{NodeSigner, OwnerAttribution, now_unix_ms};
use crate::record::{
    Measurement, SignedCapabilityRecord, VerificationReport, WHAT_A_SIGNATURE_DOES_NOT_PROVE,
    WHAT_A_SIGNATURE_PROVES, build_claim, verify,
};
use crate::vram;

/// Mesh channel this plugin sends and receives on.
pub const CHANNEL: &str = "capability-attest.v1";
/// A signed record, sent unsolicited or in reply to a request.
pub const MESSAGE_RECORD: &str = "record";
/// "Send me your latest record."
pub const MESSAGE_REQUEST: &str = "request";

/// How long after start-up the first benchmark is attempted. Long enough for
/// the node's own inference backend to finish coming up, so the first record is
/// not a measurement of a cold start.
const STARTUP_DELAY: Duration = Duration::from_secs(120);

/// Upper bound on how long the background loop waits between attempts. The
/// cooldown, not the tick, decides how often a benchmark actually runs.
const MAX_TICK: Duration = Duration::from_secs(300);

/// Cap on retained peer records. Mesh input is untrusted, so the map that holds
/// it has to be bounded.
const MAX_PEERS: usize = 256;

/// The resolved runtime, present only when configuration and the node key both
/// worked.
struct Ready {
    config: AttestConfig,
    signer: NodeSigner,
    attribution: OwnerAttribution,
    client: reqwest::Client,
    policy: SchedulePolicy,
}

enum Startup {
    Ready(Box<Ready>),
    /// Configuration or key loading failed. The message is operator-facing.
    Broken(String),
}

#[derive(Clone, Debug)]
struct PeerEntry {
    record: SignedCapabilityRecord,
    /// The peer id the host stamped on the mesh frame. Not covered by the
    /// record's signature — it says who relayed the record, not who signed it.
    transport_peer_id: String,
    received_at_unix_ms: u64,
}

#[derive(Default)]
struct Shared {
    latest: Option<SignedCapabilityRecord>,
    schedule: Schedule,
    peers: BTreeMap<String, PeerEntry>,
    last_attempt: Option<serde_json::Value>,
}

#[derive(Clone)]
pub struct Attestor {
    startup: Arc<Startup>,
    shared: Arc<Mutex<Shared>>,
    /// Held for the duration of a benchmark. `try_lock` rather than `lock`, so
    /// a second caller is told the node is busy instead of queueing behind a
    /// run that is itself several minutes long.
    bench_lock: Arc<tokio::sync::Mutex<()>>,
    background_started: Arc<AtomicBool>,
    version: String,
}

/// What one benchmark attempt did.
#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// Skipped on purpose. `reason` says which gate stopped it.
    Deferred { reason: Deferral, summary: String },
    /// A new signed record exists. `verification` is this plugin verifying its
    /// own output before publishing it.
    ///
    /// Boxed because deferral is the common outcome and this variant is an
    /// order of magnitude larger than the others.
    Measured {
        record: Box<SignedCapabilityRecord>,
        verification: Box<VerificationReport>,
    },
    /// The attempt ran and broke. Recorded rather than swallowed.
    Failed {
        error: String,
        consecutive_failures: u32,
    },
}

impl Attestor {
    pub fn new(args: &[String], env: &EnvMap, version: impl Into<String>) -> Self {
        let startup = match build_ready(args, env) {
            Ok(ready) => Startup::Ready(Box::new(ready)),
            Err(error) => Startup::Broken(format!("{error:#}")),
        };
        Self {
            startup: Arc::new(startup),
            shared: Arc::new(Mutex::new(Shared::default())),
            bench_lock: Arc::new(tokio::sync::Mutex::new(())),
            background_started: Arc::new(AtomicBool::new(false)),
            version: version.into(),
        }
    }

    fn ready(&self) -> Result<&Ready> {
        match self.startup.as_ref() {
            Startup::Ready(ready) => Ok(ready),
            Startup::Broken(reason) => bail!("capability-attest is not configured: {reason}"),
        }
    }

    /// A poisoned lock means a handler panicked mid-update. The state behind it
    /// has no cross-field invariant that a panic could have half-applied, so
    /// recovering keeps the plugin usable rather than failing every later call.
    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Health is deliberately trivial: it reads one field and returns. It must
    /// never wait on a benchmark, a probe, or the lock a benchmark holds.
    pub fn health(&self) -> Result<String> {
        let ready = self.ready()?;
        let shared = self.shared();
        Ok(match &shared.latest {
            Some(record) => format!(
                "ok; last record {} at {}",
                record.claim.record_id, record.claim.measured_at_unix_ms
            ),
            None => format!("ok; no record yet for {}", ready.signer.endpoint_id_hex()),
        })
    }

    /// Start the periodic benchmark loop. Idempotent: a re-initialize does not
    /// start a second one.
    pub fn spawn_background_loop(&self) {
        if self.background_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(ready) = self.ready() else {
            return;
        };
        // Tick often enough that a node which was busy last time gets another
        // chance soon; the cooldown, not this, decides how often a benchmark
        // actually runs. Floored so a small `--min-interval-secs` can never
        // turn this into a spin loop.
        let tick = ready
            .config
            .interval
            .min(ready.config.min_interval)
            .min(MAX_TICK)
            .max(Duration::from_secs(30));
        let attestor = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(STARTUP_DELAY).await;
            loop {
                // Every gate lives inside `attempt`, so the loop itself only
                // has to keep time.
                let _ = attestor.attempt(false).await;
                tokio::time::sleep(tick).await;
            }
        });
    }

    pub fn latest_record(&self) -> Option<SignedCapabilityRecord> {
        self.shared().latest.clone()
    }

    /// Run one attempt, from the gates through to a stored, signed record.
    pub async fn attempt(&self, ignore_cooldown: bool) -> Result<AttemptOutcome> {
        let ready = self.ready()?;

        let Ok(guard) = self.bench_lock.try_lock() else {
            let reason = Deferral::NodeBusy {
                detail: "a benchmark is already running".to_string(),
            };
            return Ok(self.record_attempt(AttemptOutcome::Deferred {
                summary: reason.summary(),
                reason,
            }));
        };

        let now = now_unix_ms()?;
        if let Some(reason) = {
            let shared = self.shared();
            schedule_deferral(now, &shared.schedule, &ready.policy, ignore_cooldown)
        } {
            drop(guard);
            return Ok(self.record_attempt(AttemptOutcome::Deferred {
                summary: reason.summary(),
                reason,
            }));
        }

        let contention = measure_contention(&ready.client, &ready.config).await;
        if let Some(reason) = contention_deferral(&contention) {
            drop(guard);
            return Ok(self.record_attempt(AttemptOutcome::Deferred {
                summary: reason.summary(),
                reason,
            }));
        }

        let outcome = match self.measure(ready).await {
            Ok(record) => {
                let verification = verify(
                    &record,
                    &ready.attribution.trust_store,
                    now_unix_ms()?,
                    None,
                );
                let mut shared = self.shared();
                shared.latest = Some(record.clone());
                shared.schedule.last_finished_unix_ms = Some(now_unix_ms()?);
                shared.schedule.consecutive_failures = 0;
                drop(shared);
                AttemptOutcome::Measured {
                    record: Box::new(record),
                    verification: Box::new(verification),
                }
            }
            Err(error) => {
                let mut shared = self.shared();
                shared.schedule.last_finished_unix_ms = Some(now_unix_ms()?);
                shared.schedule.consecutive_failures =
                    shared.schedule.consecutive_failures.saturating_add(1);
                let consecutive_failures = shared.schedule.consecutive_failures;
                drop(shared);
                AttemptOutcome::Failed {
                    error: format!("{error:#}"),
                    consecutive_failures,
                }
            }
        };
        drop(guard);
        Ok(self.record_attempt(outcome))
    }

    /// Warm up, measure, probe VRAM, sign.
    async fn measure(&self, ready: &Ready) -> Result<SignedCapabilityRecord> {
        let config = &ready.config;
        let profile = &config.profile;
        let url = config.chat_completions_url()?;
        let prompt = profile.prompt()?;
        let body = chat_request_body(profile, &prompt);

        for warmup in 1..=profile.warmup_runs {
            stream_once(&ready.client, &url, config.api_key.as_deref(), &body)
                .await
                .map_err(|error| anyhow!("warmup run {warmup} failed: {error:#}"))?;
        }

        let mut runs: Vec<RunSample> = Vec::with_capacity(profile.measured_runs as usize);
        for run in 1..=profile.measured_runs {
            let outcome = stream_once(&ready.client, &url, config.api_key.as_deref(), &body)
                .await
                .map_err(|error| anyhow!("measured run {run} failed: {error:#}"))?;
            let sample = outcome
                .into_sample(run)
                .map_err(|reason| anyhow!("measured run {run} is not usable: {reason}"))?;
            runs.push(sample);
        }

        let vram = vram::probe(config.vram_probe, config.vram_total_mib_override).await;
        let measurement = Measurement::from_runs(runs, profile.warmup_runs, vram)?;

        let claim = build_claim(
            ready.signer.endpoint_id_hex().to_string(),
            self.version.clone(),
            now_unix_ms()?,
            config.record_ttl.as_millis() as u64,
            config.endpoint_locality.as_str().to_string(),
            profile.clone(),
            measurement,
            ready.attribution.ownership.clone(),
        );
        SignedCapabilityRecord::sign(&ready.signer, claim)
    }

    fn record_attempt(&self, outcome: AttemptOutcome) -> AttemptOutcome {
        if let Ok(value) = serde_json::to_value(&outcome) {
            // Keep the summary, not the whole record: `status` should stay
            // small enough to read.
            let trimmed = match value {
                serde_json::Value::Object(mut fields) => {
                    fields.remove("record");
                    fields.insert(
                        "at_unix_ms".to_string(),
                        serde_json::json!(now_unix_ms().unwrap_or_default()),
                    );
                    serde_json::Value::Object(fields)
                }
                other => other,
            };
            self.shared().last_attempt = Some(trimmed);
        }
        outcome
    }

    /// Pause attestation. `seconds == 0` clears an existing hold.
    pub fn hold(&self, seconds: u64, reason: Option<String>) -> Result<serde_json::Value> {
        let now = now_unix_ms()?;
        let mut shared = self.shared();
        if seconds == 0 {
            shared.schedule.hold_until_unix_ms = None;
            shared.schedule.hold_reason = None;
            return Ok(serde_json::json!({ "held": false }));
        }
        let until = now.saturating_add(seconds.saturating_mul(1000));
        shared.schedule.hold_until_unix_ms = Some(until);
        shared.schedule.hold_reason = reason.clone();
        Ok(serde_json::json!({
            "held": true,
            "until_unix_ms": until,
            "reason": reason,
        }))
    }

    /// Verify any record, ours or a peer's.
    pub fn verify_record(
        &self,
        value: serde_json::Value,
        max_age_seconds: Option<u64>,
    ) -> Result<VerificationReport> {
        let record: SignedCapabilityRecord = serde_json::from_value(value)
            .map_err(|error| anyhow!("that is not a capability record: {error}"))?;
        let trust_store = match self.startup.as_ref() {
            Startup::Ready(ready) => ready.attribution.trust_store.clone(),
            // A misconfigured plugin can still check somebody else's record.
            Startup::Broken(_) => tdcc_identity::TrustStore::default(),
        };
        Ok(verify(
            &record,
            &trust_store,
            now_unix_ms()?,
            max_age_seconds.map(|seconds| seconds.saturating_mul(1000)),
        ))
    }

    /// Accept a record that arrived over the mesh.
    ///
    /// Untrusted input: it is parsed, verified, and refused unless the
    /// signature checks out. A record that fails verification is not stored at
    /// all — keeping it would only give a hostile peer a way to fill the map.
    pub fn accept_peer_record(&self, transport_peer_id: &str, body: &[u8]) -> Result<String> {
        let record: SignedCapabilityRecord = serde_json::from_slice(body).map_err(|error| {
            anyhow!("peer sent something that is not a capability record: {error}")
        })?;

        if let Ok(ready) = self.ready()
            && record.claim.node_endpoint_id == ready.signer.endpoint_id_hex()
        {
            bail!("refusing a peer record that claims to be from this node");
        }

        let trust_store = match self.startup.as_ref() {
            Startup::Ready(ready) => &ready.attribution.trust_store,
            Startup::Broken(_) => return Err(anyhow!("plugin is not configured")),
        };
        let report = verify(&record, trust_store, now_unix_ms()?, None);
        if !report.signature_valid {
            bail!(
                "peer record for {} does not verify: {}",
                record.claim.node_endpoint_id,
                report.problems.join("; ")
            );
        }

        let node_endpoint_id = record.claim.node_endpoint_id.clone();
        let entry = PeerEntry {
            record,
            transport_peer_id: transport_peer_id.to_string(),
            received_at_unix_ms: now_unix_ms()?,
        };

        let mut shared = self.shared();
        shared.peers.insert(node_endpoint_id.clone(), entry);
        if shared.peers.len() > MAX_PEERS {
            // Drop the least recently received entry, so a flood of new peers
            // cannot evict a peer that is still talking to us.
            if let Some(oldest) = shared
                .peers
                .iter()
                .min_by_key(|(_, entry)| entry.received_at_unix_ms)
                .map(|(key, _)| key.clone())
            {
                shared.peers.remove(&oldest);
            }
        }
        Ok(node_endpoint_id)
    }

    pub fn forget_peer(&self, transport_peer_id: &str) {
        let mut shared = self.shared();
        shared
            .peers
            .retain(|_, entry| entry.transport_peer_id != transport_peer_id);
    }

    /// Peer records, re-verified at read time so freshness is current rather
    /// than whatever it was when the record arrived.
    pub fn peers(&self) -> Result<serde_json::Value> {
        let now = now_unix_ms()?;
        let trust_store = match self.startup.as_ref() {
            Startup::Ready(ready) => ready.attribution.trust_store.clone(),
            Startup::Broken(_) => tdcc_identity::TrustStore::default(),
        };
        let entries: Vec<PeerEntry> = self.shared().peers.values().cloned().collect();
        // Two records are only on the same scale if they pin the same model,
        // context, and sampling settings. Saying so per peer is more useful to
        // a router than making it compare fingerprints itself.
        let own_profile = match self.startup.as_ref() {
            Startup::Ready(ready) => Some(ready.config.profile.clone()),
            Startup::Broken(_) => None,
        };

        let peers: Vec<serde_json::Value> = entries
            .iter()
            .map(|entry| {
                let report = verify(&entry.record, &trust_store, now, None);
                let claim = &entry.record.claim;
                let comparable = own_profile
                    .as_ref()
                    .map(|profile| profile.comparable_with(&claim.profile));
                serde_json::json!({
                    "node_endpoint_id": claim.node_endpoint_id,
                    "comparable_with_this_node": comparable,
                    "transport_peer_id": entry.transport_peer_id,
                    "transport_peer_id_matches_signing_key":
                        entry.transport_peer_id.eq_ignore_ascii_case(&claim.node_endpoint_id),
                    "received_at_unix_ms": entry.received_at_unix_ms,
                    "model": claim.profile.model,
                    "profile_fingerprint": claim.profile.fingerprint(),
                    // The signed integers, plus the same values in the units a
                    // human reads. Only the integers are covered by the
                    // signature; the two floats are conveniences.
                    "median_output_tokens_per_second_milli":
                        claim.measurement.median_output_tokens_per_second_milli,
                    "median_time_to_first_token_us":
                        claim.measurement.median_time_to_first_token_us,
                    "median_output_tokens_per_second":
                        claim.measurement.median_output_tokens_per_second(),
                    "median_time_to_first_token_ms":
                        claim.measurement.median_time_to_first_token_ms(),
                    "vram": claim.measurement.vram,
                    "verification": report,
                })
            })
            .collect();

        Ok(serde_json::json!({
            "peers": peers,
            "note": "transport_peer_id is stamped by the sending host and is not covered by the \
                     record signature; a mismatch means the record was relayed, not that it is fake.",
            "what_a_signature_proves": WHAT_A_SIGNATURE_PROVES,
            "what_a_signature_does_not_prove": WHAT_A_SIGNATURE_DOES_NOT_PROVE,
        }))
    }

    pub fn status(&self) -> Result<serde_json::Value> {
        let shared = self.shared();
        let latest = shared.latest.clone();
        let schedule = shared.schedule.clone();
        let last_attempt = shared.last_attempt.clone();
        let peer_count = shared.peers.len();
        drop(shared);

        let mut status = serde_json::json!({
            "plugin": "capability-attest",
            "version": self.version,
            "peers_known": peer_count,
            "last_attempt": last_attempt,
            "schedule": {
                "hold_until_unix_ms": schedule.hold_until_unix_ms,
                "hold_reason": schedule.hold_reason,
                "last_finished_unix_ms": schedule.last_finished_unix_ms,
                "consecutive_failures": schedule.consecutive_failures,
            },
            "what_a_signature_proves": WHAT_A_SIGNATURE_PROVES,
            "what_a_signature_does_not_prove": WHAT_A_SIGNATURE_DOES_NOT_PROVE,
        });

        match self.startup.as_ref() {
            Startup::Broken(reason) => {
                status["state"] = serde_json::json!("misconfigured");
                status["error"] = serde_json::json!(reason);
            }
            Startup::Ready(ready) => {
                status["state"] = serde_json::json!("ready");
                status["node_endpoint_id"] = serde_json::json!(ready.signer.endpoint_id_hex());
                status["endpoint"] = serde_json::json!(redacted_url(&ready.config.endpoint));
                status["endpoint_locality"] =
                    serde_json::json!(ready.config.endpoint_locality.as_str());
                status["profile"] = serde_json::to_value(&ready.config.profile)?;
                status["profile_fingerprint"] =
                    serde_json::json!(ready.config.profile.fingerprint());
                status["contention_signal"] = serde_json::json!(match &ready.config.busy_url {
                    Some(url) => format!(
                        "busy probe {} at {}",
                        redacted_url(url),
                        ready.config.busy_pointer
                    ),
                    None => format!(
                        "guard probe latency proxy, limit {}ms (configure --busy-url for a real signal)",
                        ready.config.max_guard_ttft_ms
                    ),
                });
                status["owner_certificate"] = serde_json::json!({
                    "present": ready.attribution.ownership.is_some(),
                    "note": ready.attribution.note,
                });
            }
        }

        status["record"] = match &latest {
            Some(record) => serde_json::json!({
                "present": true,
                "record_id": record.claim.record_id,
                "measured_at_unix_ms": record.claim.measured_at_unix_ms,
                "expires_at_unix_ms": record.claim.expires_at_unix_ms,
                "median_output_tokens_per_second":
                    record.claim.measurement.median_output_tokens_per_second(),
                "median_time_to_first_token_ms":
                    record.claim.measurement.median_time_to_first_token_ms(),
                // The individual runs, in readable units, so an operator can
                // see spread rather than just a median. The record itself
                // carries these as exact integers.
                "runs": record
                    .claim
                    .measurement
                    .runs
                    .iter()
                    .map(|run| serde_json::json!({
                        "run": run.run,
                        "output_tokens": run.output_tokens,
                        "output_tokens_per_second": run.output_tokens_per_second(),
                        "time_to_first_token_ms": run.time_to_first_token_ms(),
                    }))
                    .collect::<Vec<_>>(),
                "vram": record.claim.measurement.vram,
            }),
            None => serde_json::json!({ "present": false }),
        };

        Ok(status)
    }
}

fn build_ready(args: &[String], env: &EnvMap) -> Result<Ready> {
    let config = resolve(args, env)?;
    let signer = NodeSigner::load(config.node_key_path.as_deref())?;
    let client = build_client(config.request_timeout)?;
    let policy = SchedulePolicy::new(config.min_interval.as_millis() as u64);
    Ok(Ready {
        config,
        signer,
        attribution: OwnerAttribution::load(),
        client,
        policy,
    })
}

/// A URL with any embedded credentials removed.
///
/// `status` is readable by anyone who can reach the console, and an endpoint
/// URL is one of the places a token ends up by accident.
pub fn redacted_url(url: &Url) -> String {
    let mut redacted = url.clone();
    if redacted.password().is_some() {
        let _ = redacted.set_password(None);
    }
    if !redacted.username().is_empty() {
        let _ = redacted.set_username("");
    }
    redacted.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn broken() -> Attestor {
        Attestor::new(&args(&["--model", "demo"]), &env(&[]), "0.1.0")
    }

    #[test]
    fn a_misconfigured_plugin_starts_but_says_why_everywhere() {
        let attestor = broken();

        let status = attestor.status().unwrap();
        assert_eq!(status["state"], "misconfigured");
        assert!(
            status["error"].as_str().unwrap().contains("--endpoint"),
            "{status}"
        );

        let health = attestor.health().unwrap_err();
        assert!(health.to_string().contains("not configured"), "{health}");
    }

    #[test]
    fn a_misconfigured_plugin_still_publishes_the_caveat() {
        let status = broken().status().unwrap();

        assert_eq!(status["what_a_signature_proves"], WHAT_A_SIGNATURE_PROVES);
        assert_eq!(
            status["what_a_signature_does_not_prove"],
            WHAT_A_SIGNATURE_DOES_NOT_PROVE
        );
    }

    #[test]
    fn credentials_in_an_endpoint_url_never_reach_status() {
        let url = Url::parse("http://someone:hunter2@127.0.0.1:8000/v1").unwrap();

        let redacted = redacted_url(&url);

        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("someone"));
        assert!(redacted.contains("127.0.0.1:8000"));
    }

    #[test]
    fn a_hold_can_be_set_and_cleared() {
        let attestor = broken();

        let held = attestor.hold(600, Some("driver update".into())).unwrap();
        assert_eq!(held["held"], true);
        assert_eq!(held["reason"], "driver update");

        let cleared = attestor.hold(0, None).unwrap();
        assert_eq!(cleared["held"], false);
    }

    #[tokio::test]
    async fn an_unconfigured_plugin_refuses_to_benchmark_rather_than_reporting_success() {
        let error = broken().attempt(true).await.unwrap_err();

        assert!(error.to_string().contains("not configured"), "{error}");
    }

    #[test]
    fn a_peer_record_that_is_not_a_record_is_rejected_with_a_readable_error() {
        let error = broken()
            .accept_peer_record("aa".repeat(32).as_str(), b"{\"nonsense\":true}")
            .unwrap_err();

        assert!(error.to_string().contains("capability record"), "{error}");
    }

    #[test]
    fn peers_are_empty_and_still_carry_the_caveat_before_anything_arrives() {
        let peers = broken().peers().unwrap();

        assert_eq!(peers["peers"].as_array().unwrap().len(), 0);
        assert_eq!(peers["what_a_signature_proves"], WHAT_A_SIGNATURE_PROVES);
    }

    // ── The whole loop, against a real endpoint ─────────────────────────────

    struct Node {
        _directory: tempfile::TempDir,
        attestor: Attestor,
    }

    /// A fully configured attestor pointed at `address`, with its own node key.
    fn node(address: std::net::SocketAddr, seed: u8, extra: &[&str]) -> Node {
        let directory = tempfile::tempdir().unwrap();
        let key_path = directory.path().join("key");
        std::fs::write(&key_path, hex::encode([seed; 32])).unwrap();

        let mut arguments = args(&[
            "--endpoint",
            &format!("http://{address}/v1"),
            "--model",
            "demo-model",
            "--warmup-runs",
            "0",
            "--measured-runs",
            "3",
            "--context-tokens",
            "16",
            "--max-output-tokens",
            "8",
            "--vram-probe",
            "off",
            "--node-key-path",
            key_path.to_str().unwrap(),
        ]);
        arguments.extend(args(extra));

        Node {
            _directory: directory,
            attestor: Attestor::new(&arguments, &env(&[]), "0.1.0"),
        }
    }

    async fn endpoint() -> std::net::SocketAddr {
        crate::testutil::serve_forever(
            crate::testutil::SSE_HEAD,
            crate::testutil::SSE_CHUNKS,
            Duration::from_millis(5),
        )
        .await
    }

    #[tokio::test]
    async fn a_full_attempt_produces_a_record_that_verifies_against_this_node() {
        let node = node(endpoint().await, 31, &[]);

        let outcome = node.attestor.attempt(false).await.unwrap();

        let AttemptOutcome::Measured {
            record,
            verification,
        } = outcome
        else {
            panic!("expected a measurement, got {outcome:?}");
        };
        assert!(
            verification.usable_for_routing,
            "the plugin must verify its own output before publishing it: {:?}",
            verification.problems
        );
        assert_eq!(record.claim.measurement.runs.len(), 3);
        assert_eq!(record.claim.endpoint_locality, "loopback");
        assert_eq!(record.claim.measurement.vram.source, "unavailable");
        assert!(
            record
                .claim
                .measurement
                .median_output_tokens_per_second_milli
                > 0
        );

        let status = node.attestor.status().unwrap();
        assert_eq!(status["state"], "ready");
        assert_eq!(status["record"]["present"], true);
        assert!(node.attestor.health().unwrap().contains("last record"));
    }

    #[tokio::test]
    async fn a_real_measured_record_survives_a_json_round_trip_bit_for_bit() {
        // Regression: with `f64` timings, a real run produced
        // `31.165399999999998`, which `serde_json` read back as `31.1654`. The
        // record then failed to verify on the receiving node even though it had
        // been signed correctly. Synthetic round numbers never showed it — only
        // an actual measurement did.
        let node = node(endpoint().await, 61, &[]);
        let AttemptOutcome::Measured { record, .. } = node.attestor.attempt(false).await.unwrap()
        else {
            panic!("expected a measurement");
        };

        let encoded = serde_json::to_string(&record).unwrap();
        let decoded: SignedCapabilityRecord = serde_json::from_str(&encoded).unwrap();

        assert_eq!(*record, decoded);
        assert_eq!(encoded, serde_json::to_string(&decoded).unwrap());
        assert!(
            verify(
                &decoded,
                &tdcc_identity::TrustStore::default(),
                now_unix_ms().unwrap(),
                None
            )
            .signature_valid
        );
    }

    #[tokio::test]
    async fn a_second_attempt_is_refused_by_the_cooldown_and_says_when_to_retry() {
        let node = node(endpoint().await, 32, &[]);

        node.attestor.attempt(false).await.unwrap();
        let second = node.attestor.attempt(false).await.unwrap();

        let AttemptOutcome::Deferred { reason, summary } = second else {
            panic!("a run right after another must not measure again: {second:?}");
        };
        assert!(matches!(reason, Deferral::Cooldown { .. }), "{reason:?}");
        assert!(summary.contains("cooling down"), "{summary}");
    }

    #[tokio::test]
    async fn a_hold_outranks_an_explicit_request_to_ignore_the_cooldown() {
        let node = node(endpoint().await, 33, &[]);
        node.attestor
            .hold(600, Some("driver update".into()))
            .unwrap();

        let outcome = node.attestor.attempt(true).await.unwrap();

        let AttemptOutcome::Deferred { reason, .. } = outcome else {
            panic!("a hold must stop even a forced run: {outcome:?}");
        };
        assert!(matches!(reason, Deferral::Hold { .. }), "{reason:?}");
    }

    #[tokio::test]
    async fn an_endpoint_that_cannot_be_reached_never_reports_a_measurement() {
        let dead = crate::testutil::dead_address().await;
        let node = node(dead, 34, &["--request-timeout-secs", "2"]);

        let outcome = node.attestor.attempt(false).await.unwrap();

        // The guard probe cannot answer, so load is unknown and the run is
        // deferred rather than attempted blind. Either way, nothing is
        // published and the reason is recorded.
        assert!(
            !matches!(outcome, AttemptOutcome::Measured { .. }),
            "{outcome:?}"
        );
        let status = node.attestor.status().unwrap();
        assert_ne!(status["last_attempt"], serde_json::Value::Null);
        assert_eq!(status["record"]["present"], false);
    }

    #[tokio::test]
    async fn a_peer_record_is_stored_only_while_it_still_verifies() {
        let address = endpoint().await;
        let peer = node(address, 41, &[]);
        let mine = node(address, 42, &[]);

        let AttemptOutcome::Measured { record, .. } = peer.attestor.attempt(false).await.unwrap()
        else {
            panic!("the peer needs a record to send");
        };
        let peer_id = record.claim.node_endpoint_id.clone();
        let body = serde_json::to_vec(&record).unwrap();

        let stored = mine.attestor.accept_peer_record(&peer_id, &body).unwrap();
        assert_eq!(stored, peer_id);

        let peers = mine.attestor.peers().unwrap();
        let listed = &peers["peers"][0];
        assert_eq!(listed["node_endpoint_id"], peer_id);
        assert_eq!(listed["transport_peer_id_matches_signing_key"], true);
        assert_eq!(listed["comparable_with_this_node"], true);
        assert_eq!(listed["verification"]["usable_for_routing"], true);

        // Tamper with the one number a router would actually use.
        let mut forged = record.clone();
        forged
            .claim
            .measurement
            .median_output_tokens_per_second_milli *= 10;
        let error = mine
            .attestor
            .accept_peer_record(&peer_id, &serde_json::to_vec(&forged).unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("does not verify"), "{error}");

        // The tampered record did not replace the good one.
        let peers = mine.attestor.peers().unwrap();
        assert_eq!(peers["peers"].as_array().unwrap().len(), 1);
        assert_eq!(
            peers["peers"][0]["median_output_tokens_per_second_milli"],
            serde_json::json!(
                record
                    .claim
                    .measurement
                    .median_output_tokens_per_second_milli
            )
        );

        mine.attestor.forget_peer(&peer_id);
        assert_eq!(
            mine.attestor.peers().unwrap()["peers"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn a_peer_claiming_to_be_this_node_is_refused() {
        let mine = node(endpoint().await, 51, &[]);

        let AttemptOutcome::Measured { record, .. } = mine.attestor.attempt(false).await.unwrap()
        else {
            panic!("expected a measurement");
        };

        let error = mine
            .attestor
            .accept_peer_record(
                "ff".repeat(32).as_str(),
                &serde_json::to_vec(&record).unwrap(),
            )
            .unwrap_err();

        assert!(error.to_string().contains("from this node"), "{error}");
    }
}
