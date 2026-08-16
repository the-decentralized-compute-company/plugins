//! Loaded policy, live counters, and the shapes the tools return.
//!
//! This is where the pure evaluator meets the two impure things it needs: the
//! policy file on disk and the wall clock. It also owns the three load-time
//! decisions that matter most on someone else's hardware:
//!
//! | Situation | Behaviour | Why |
//! | --- | --- | --- |
//! | No policy file | Permissive dry-run, recorded | Installing an unconfigured plugin must not take a node out of service. |
//! | Policy file that will not load | **Refuse everything**, loudly | The file existing is evidence the operator wanted rules. Serving wide open while their rules sit unparsed is the worst of the three outcomes. |
//! | Reload of a bad file over a good one | Keep the good one, return an error | A hot reload must never be able to break a running node with a typo. |
//!
//! The middle row is escapable with `--on-invalid-policy allow`, documented in
//! the README, for an operator who would rather keep serving than fail closed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use schemars::JsonSchema;
use serde::Serialize;

use crate::clock::Timestamp;
use crate::evaluate::{Outcome, Request, evaluate};
use crate::observe::{Counters, Ledger, Observation, TopValue};
use crate::policy::{Decision, Mode, Policy, parse_policy};
use crate::ratelimit::TokenBuckets;

/// The `type` field of a refusal envelope. Callers key on this to tell a local
/// policy refusal apart from a backend failure.
pub const REFUSAL_TYPE: &str = "workload_policy_denied";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyStatus {
    /// No policy file at the configured path.
    Absent,
    /// A policy file was read and validated.
    Loaded,
    /// A policy file exists but could not be read or validated.
    Invalid,
}

impl PolicyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Loaded => "loaded",
            Self::Invalid => "invalid",
        }
    }
}

/// What to do about a policy file that will not load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnInvalidPolicy {
    /// Refuse every request. The default.
    Deny,
    /// Keep serving as if no policy were configured. A recovery hatch, not a
    /// posture: it means a typo silently disables the operator's rules.
    Allow,
}

impl OnInvalidPolicy {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "deny" | "closed" | "fail-closed" => Ok(Self::Deny),
            "allow" | "open" | "fail-open" => Ok(Self::Allow),
            other => Err(format!(
                "unknown --on-invalid-policy value '{other}'; expected \"deny\" or \"allow\""
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Allow => "allow",
        }
    }
}

// ---------------------------------------------------------------------------
// Tool response shapes
// ---------------------------------------------------------------------------

/// A refusal, pre-formatted so the component that called `check` can hand it
/// straight back to whoever submitted the work.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PolicyRefusal {
    /// Always `workload_policy_denied`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Stable outcome code, for example `policy.deny_rule`.
    pub code: String,
    /// One sentence naming the policy and the reason.
    pub message: String,
    /// The rule that refused, when a rule refused.
    pub rule_id: Option<String>,
    /// Milliseconds until a rate-limited caller may retry.
    pub retry_after_ms: Option<i64>,
    /// Path of the policy file that produced this refusal.
    pub node_policy_source: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TraceEntry {
    pub rule_id: String,
    pub matched: bool,
    /// The first condition that did not hold.
    pub unmet_condition: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CheckResponse {
    /// The answer to act on: `allow` or `deny`. In dry-run this is always
    /// `allow`; read `would_deny` to see what enforcing would have done.
    pub decision: String,
    /// Whether the policy is being applied (`mode = "enforce"`).
    pub enforced: bool,
    /// Whether an enforcing policy would have refused this request.
    pub would_deny: bool,
    /// Stable outcome code.
    pub code: String,
    /// Human-readable explanation, safe to log and to show an operator.
    pub reason: String,
    pub rule_id: Option<String>,
    pub retry_after_ms: Option<i64>,
    /// Fields the policy needs that this request did not carry.
    pub missing_fields: Vec<String>,
    pub mode: String,
    pub policy_status: String,
    pub policy_source: String,
    pub evaluated_at_epoch_ms: i64,
    /// Present if and only if `decision` is `deny`.
    pub error: Option<PolicyRefusal>,
    /// Present only when the caller asked for `explain`.
    pub trace: Option<Vec<TraceEntry>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuleView {
    pub id: String,
    pub action: String,
    pub reason: Option<String>,
    /// One line per condition, in evaluation order.
    pub conditions: Vec<String>,
    pub limit: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PolicyView {
    /// `absent`, `loaded`, or `invalid`.
    pub status: String,
    pub source: String,
    pub mode: String,
    pub default_action: String,
    pub timezone: String,
    pub on_invalid_policy: String,
    pub loaded_at_epoch_ms: Option<i64>,
    pub rules: Vec<RuleView>,
    /// Request fields this policy needs; a request omitting one is refused.
    pub required_request_fields: Vec<String>,
    pub observation_capacity: usize,
    /// Everything wrong with the policy file, when it would not load.
    pub errors: Vec<String>,
    /// Things that are not errors but that an operator should know.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReportResponse {
    pub mode: String,
    pub policy_status: String,
    pub policy_source: String,
    pub counters: Counters,
    /// Decisions currently retained in the ring.
    pub retained: usize,
    pub observation_capacity: usize,
    /// Distinct rate-limit buckets currently held, against a cap of 4096. A
    /// number pinned at the cap means limit rules are refusing new peers for
    /// want of bucket space rather than for want of budget.
    pub rate_limit_buckets: usize,
    pub top_models: Vec<TopValue>,
    pub top_peers: Vec<TopValue>,
    /// Most recent decisions, newest first.
    pub recent: Vec<Observation>,
    /// What these numbers mean for this node right now.
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReloadResponse {
    pub status: String,
    pub source: String,
    pub mode: String,
    pub rules: usize,
    pub loaded_at_epoch_ms: i64,
    pub warnings: Vec<String>,
}

/// A reload that did not happen, along with what is still in force.
#[derive(Debug, Clone)]
pub struct ReloadFailure {
    pub message: String,
    pub errors: Vec<String>,
    pub status: PolicyStatus,
    pub source: String,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct Inner {
    path: PathBuf,
    on_invalid: OnInvalidPolicy,
    status: PolicyStatus,
    errors: Vec<String>,
    loaded_at_epoch_ms: Option<i64>,
    policy: Policy,
    buckets: TokenBuckets,
    ledger: Ledger,
}

/// Shared policy state. Cheap to clone: every clone is the same node policy.
#[derive(Clone)]
pub struct PolicyState {
    inner: Arc<Mutex<Inner>>,
}

impl PolicyState {
    /// Build state without touching the disk. Used by
    /// `--print-package-manifest`, which must not depend on a node's policy.
    pub fn detached(path: PathBuf, on_invalid: OnInvalidPolicy) -> Self {
        Self::from_parts(
            path,
            on_invalid,
            PolicyStatus::Absent,
            Vec::new(),
            None,
            Policy::permissive(),
        )
    }

    /// Read and validate the policy file. Returns the state plus the lines the
    /// process should print to stderr, where they land in the tdcc log.
    pub fn load(path: PathBuf, on_invalid: OnInvalidPolicy) -> (Self, Vec<String>) {
        let display = path.display().to_string();
        let now = Timestamp::now(crate::clock::Zone::Utc).epoch_millis;
        match read_policy(&path) {
            LoadOutcome::Absent => {
                let messages = vec![format!(
                    "workload-policy: no policy file at {display}; allowing every request and recording what a policy would have matched (dry-run)"
                )];
                (
                    Self::from_parts(
                        path,
                        on_invalid,
                        PolicyStatus::Absent,
                        Vec::new(),
                        None,
                        Policy::permissive(),
                    ),
                    messages,
                )
            }
            LoadOutcome::Loaded(policy) => {
                let mut messages = vec![format!(
                    "workload-policy: loaded {} rule(s) from {display} in {} mode",
                    policy.rules.len(),
                    policy.mode.as_str()
                )];
                if policy.is_silently_permissive() {
                    messages.push(format!(
                        "workload-policy: mode is dry-run, so none of those {} rule(s) refuse anything yet",
                        policy.rules.len()
                    ));
                }
                (
                    Self::from_parts(
                        path,
                        on_invalid,
                        PolicyStatus::Loaded,
                        Vec::new(),
                        Some(now),
                        policy,
                    ),
                    messages,
                )
            }
            LoadOutcome::Invalid(errors) => {
                let mut messages = vec![format!(
                    "workload-policy: {display} could not be loaded, so the node is failing {}",
                    match on_invalid {
                        OnInvalidPolicy::Deny => "closed and will refuse policy-gated work",
                        OnInvalidPolicy::Allow =>
                            "open (--on-invalid-policy allow): your rules are NOT being applied",
                    }
                )];
                messages.extend(
                    errors
                        .iter()
                        .map(|error| format!("workload-policy:   {error}")),
                );
                (
                    Self::from_parts(
                        path,
                        on_invalid,
                        PolicyStatus::Invalid,
                        errors,
                        None,
                        Policy::permissive(),
                    ),
                    messages,
                )
            }
        }
    }

    fn from_parts(
        path: PathBuf,
        on_invalid: OnInvalidPolicy,
        status: PolicyStatus,
        errors: Vec<String>,
        loaded_at_epoch_ms: Option<i64>,
        policy: Policy,
    ) -> Self {
        let ledger = Ledger::new(policy.observation_capacity);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                path,
                on_invalid,
                status,
                errors,
                loaded_at_epoch_ms,
                policy,
                buckets: TokenBuckets::new(),
                ledger,
            })),
        }
    }

    /// A poisoned lock means a handler panicked mid-update. The policy itself is
    /// immutable once loaded, and the counters it guards have no cross-field
    /// invariant, so recovering keeps the node deciding instead of turning every
    /// later request into an error.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn check(&self, request: Request, explain: bool) -> CheckResponse {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let source = inner.path.display().to_string();

        let at = Timestamp::now(inner.policy.zone);
        let unavailable =
            inner.status == PolicyStatus::Invalid && inner.on_invalid == OnInvalidPolicy::Deny;

        let (outcome, mode) = if unavailable {
            let detail = inner
                .errors
                .first()
                .cloned()
                .unwrap_or_else(|| "the file could not be read".to_string());
            (
                Outcome::policy_unavailable(format!(
                    "this node's local workload policy ({source}) could not be loaded, so the node is refusing policy-gated work until it is fixed: {detail}"
                )),
                // A file that did not parse cannot tell us it wanted dry-run.
                Mode::Enforce,
            )
        } else {
            (
                evaluate(&inner.policy, &request, at, &mut inner.buckets, explain),
                inner.policy.mode,
            )
        };

        let enforced = mode == Mode::Enforce;
        let would_deny = outcome.decision == Decision::Deny;
        let decision = if would_deny && !enforced {
            Decision::Allow
        } else {
            outcome.decision
        };

        let reason = if would_deny && !enforced {
            format!(
                "dry-run: this request was served, but an enforcing policy would refuse it — {}",
                outcome.reason
            )
        } else {
            outcome.reason.clone()
        };

        let error = (decision == Decision::Deny).then(|| PolicyRefusal {
            kind: REFUSAL_TYPE.to_string(),
            code: outcome.code.as_str().to_string(),
            message: format!(
                "Local workload policy on this node declined the request: {}",
                outcome.reason
            ),
            rule_id: outcome.rule_id.clone(),
            retry_after_ms: outcome.retry_after_ms,
            node_policy_source: source.clone(),
        });

        inner.ledger.record(Observation {
            at_epoch_ms: at.epoch_millis,
            decision: decision.as_str().to_string(),
            enforced,
            would_deny,
            code: outcome.code.as_str().to_string(),
            rule_id: outcome.rule_id.clone(),
            model: request.model.clone(),
            peer: request.peer.clone(),
            owner: request.owner.clone(),
            kind: request.kind.clone(),
            context_tokens: request.context_tokens,
            max_output_tokens: request.max_output_tokens,
        });

        CheckResponse {
            decision: decision.as_str().to_string(),
            enforced,
            would_deny,
            code: outcome.code.as_str().to_string(),
            reason,
            rule_id: outcome.rule_id.clone(),
            retry_after_ms: outcome.retry_after_ms,
            missing_fields: outcome
                .missing_fields
                .iter()
                .map(|field| (*field).to_string())
                .collect(),
            mode: mode.as_str().to_string(),
            policy_status: inner.status.as_str().to_string(),
            policy_source: source,
            evaluated_at_epoch_ms: at.epoch_millis,
            error,
            trace: explain.then(|| {
                outcome
                    .trace
                    .iter()
                    .map(|entry| TraceEntry {
                        rule_id: entry.rule_id.clone(),
                        matched: entry.matched,
                        unmet_condition: entry.unmet_condition.map(ToString::to_string),
                    })
                    .collect()
            }),
        }
    }

    /// One line for the host's health check. Deliberately trivial work: a
    /// health probe must never wait on anything slower than a mutex.
    pub fn health_summary(&self) -> String {
        let inner = self.lock();
        match inner.status {
            PolicyStatus::Absent => {
                "no policy file: allowing and recording every request".to_string()
            }
            PolicyStatus::Loaded => format!(
                "{} rule(s) loaded, mode {}",
                inner.policy.rules.len(),
                inner.policy.mode.as_str()
            ),
            PolicyStatus::Invalid => format!(
                "policy file did not load; failing {}",
                inner.on_invalid.as_str()
            ),
        }
    }

    pub fn view(&self) -> PolicyView {
        let inner = self.lock();
        let source = inner.path.display().to_string();
        let mut warnings = Vec::new();

        match inner.status {
            PolicyStatus::Absent => warnings.push(format!(
                "No policy file at {source}. Every request is allowed and recorded; call the report tool to see what your traffic looks like, then write a policy."
            )),
            PolicyStatus::Invalid => warnings.push(match inner.on_invalid {
                OnInvalidPolicy::Deny => format!(
                    "{source} could not be loaded, so this node is refusing policy-gated work. Fix the errors below and call reload."
                ),
                OnInvalidPolicy::Allow => format!(
                    "{source} could not be loaded and this process was started with --on-invalid-policy allow, so your rules are NOT being applied and everything is being served."
                ),
            }),
            PolicyStatus::Loaded => {
                if inner.policy.is_silently_permissive() {
                    warnings.push(format!(
                        "{} rule(s) are loaded but mode = \"dry-run\", so nothing is being refused. Set mode = \"enforce\" in {source} and call reload when the report looks right.",
                        inner.policy.rules.len()
                    ));
                }
            }
        }

        PolicyView {
            status: inner.status.as_str().to_string(),
            source,
            mode: inner.policy.mode.as_str().to_string(),
            default_action: inner.policy.default_action.as_str().to_string(),
            timezone: inner.policy.zone.as_str().to_string(),
            on_invalid_policy: inner.on_invalid.as_str().to_string(),
            loaded_at_epoch_ms: inner.loaded_at_epoch_ms,
            rules: inner
                .policy
                .rules
                .iter()
                .map(|rule| RuleView {
                    id: rule.id.clone(),
                    action: rule.action.as_str().to_string(),
                    reason: rule.reason.clone(),
                    conditions: rule.when.describe(),
                    limit: rule.limit.map(|limit| limit.describe()),
                })
                .collect(),
            required_request_fields: inner
                .policy
                .required
                .names()
                .into_iter()
                .map(str::to_string)
                .collect(),
            observation_capacity: inner.ledger.capacity(),
            errors: inner.errors.clone(),
            warnings,
        }
    }

    pub fn report(&self, limit: usize) -> ReportResponse {
        let inner = self.lock();
        let counters = inner.ledger.counters().clone();
        let summary = match (inner.status, inner.policy.mode) {
            (PolicyStatus::Invalid, _) if inner.on_invalid == OnInvalidPolicy::Deny => format!(
                "The policy file could not be loaded, so all {} evaluated request(s) were refused.",
                counters.evaluated
            ),
            (_, Mode::DryRun) => format!(
                "Dry-run: {} request(s) evaluated, {} of which an enforcing policy would have refused. Nothing was actually refused.",
                counters.evaluated, counters.would_deny
            ),
            (_, Mode::Enforce) => format!(
                "Enforcing: {} request(s) evaluated, {} refused.",
                counters.evaluated, counters.denied
            ),
        };

        ReportResponse {
            mode: inner.policy.mode.as_str().to_string(),
            policy_status: inner.status.as_str().to_string(),
            policy_source: inner.path.display().to_string(),
            counters,
            retained: inner.ledger.retained(),
            observation_capacity: inner.ledger.capacity(),
            rate_limit_buckets: inner.buckets.tracked_keys(),
            top_models: inner.ledger.top_values(|entry| entry.model.as_ref()),
            top_peers: inner.ledger.top_values(|entry| entry.peer.as_ref()),
            recent: inner.ledger.recent(limit),
            summary,
        }
    }

    /// Re-read the policy file at the path this process was started with.
    ///
    /// The path is deliberately not a parameter: a reload that took a caller
    /// supplied path would turn this tool into a "read any file and tell me
    /// what is wrong with it" oracle.
    pub fn reload(&self) -> Result<ReloadResponse, ReloadFailure> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let source = inner.path.display().to_string();
        let now = Timestamp::now(crate::clock::Zone::Utc).epoch_millis;

        match read_policy(&inner.path) {
            LoadOutcome::Loaded(policy) => {
                let mut warnings = Vec::new();
                if policy.is_silently_permissive() {
                    warnings.push(format!(
                        "{} rule(s) loaded, but mode = \"dry-run\": nothing is being refused.",
                        policy.rules.len()
                    ));
                }
                // Bucket keys embed rule ids, so budgets from the previous rule
                // set would otherwise be spent against the new one.
                inner.buckets.clear();
                inner.ledger.set_capacity(policy.observation_capacity);
                inner.status = PolicyStatus::Loaded;
                inner.errors.clear();
                inner.loaded_at_epoch_ms = Some(now);
                let response = ReloadResponse {
                    status: inner.status.as_str().to_string(),
                    source,
                    mode: policy.mode.as_str().to_string(),
                    rules: policy.rules.len(),
                    loaded_at_epoch_ms: now,
                    warnings,
                };
                inner.policy = policy;
                Ok(response)
            }
            LoadOutcome::Absent => {
                if inner.status == PolicyStatus::Loaded {
                    // Deleting the file is not a way to drop the rules: that
                    // would make policy removal the easiest operation there is.
                    return Err(ReloadFailure {
                        message: format!(
                            "no policy file at {source}; keeping the {} rule(s) already loaded",
                            inner.policy.rules.len()
                        ),
                        errors: Vec::new(),
                        status: inner.status,
                        source,
                    });
                }
                inner.status = PolicyStatus::Absent;
                inner.errors.clear();
                inner.policy = Policy::permissive();
                let capacity = inner.policy.observation_capacity;
                inner.ledger.set_capacity(capacity);
                inner.buckets.clear();
                Ok(ReloadResponse {
                    status: inner.status.as_str().to_string(),
                    source,
                    mode: inner.policy.mode.as_str().to_string(),
                    rules: 0,
                    loaded_at_epoch_ms: now,
                    warnings: vec![
                        "No policy file: allowing every request and recording what a policy would have matched.".to_string(),
                    ],
                })
            }
            LoadOutcome::Invalid(errors) => {
                if inner.status == PolicyStatus::Loaded {
                    // The running policy stays in force. A typo must never be
                    // able to change what a live node accepts.
                    return Err(ReloadFailure {
                        message: format!(
                            "{source} did not load; the {} rule(s) loaded earlier are still in force",
                            inner.policy.rules.len()
                        ),
                        errors,
                        status: inner.status,
                        source,
                    });
                }
                // Nothing good to fall back to: record the failure so `check`
                // fails closed rather than looking like an unconfigured node.
                inner.status = PolicyStatus::Invalid;
                inner.errors = errors.clone();
                inner.policy = Policy::permissive();
                Err(ReloadFailure {
                    message: format!(
                        "{source} did not load; this node is now failing {}",
                        match inner.on_invalid {
                            OnInvalidPolicy::Deny => "closed",
                            OnInvalidPolicy::Allow => "open (--on-invalid-policy allow)",
                        }
                    ),
                    errors,
                    status: inner.status,
                    source,
                })
            }
        }
    }
}

enum LoadOutcome {
    Absent,
    Loaded(Policy),
    Invalid(Vec<String>),
}

fn read_policy(path: &Path) -> LoadOutcome {
    match std::fs::read_to_string(path) {
        Ok(text) => match parse_policy(&text) {
            Ok(policy) => LoadOutcome::Loaded(policy),
            Err(errors) => LoadOutcome::Invalid(errors),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LoadOutcome::Absent,
        // "The file is there but I cannot read it" is not the same as "there is
        // no policy", and must not be treated as permission to serve freely.
        Err(error) => {
            LoadOutcome::Invalid(vec![format!("cannot read {}: {error}", path.display())])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    /// A scratch policy file that removes itself.
    struct TempPolicy {
        path: PathBuf,
    }

    impl TempPolicy {
        fn new(contents: Option<&str>) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "workload-policy-test-{}-{}.toml",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            let temp = Self { path };
            if let Some(contents) = contents {
                temp.write(contents);
            }
            temp
        }

        fn write(&self, contents: &str) {
            std::fs::write(&self.path, contents).expect("write test policy");
        }

        fn remove(&self) {
            let _ = std::fs::remove_file(&self.path);
        }

        fn state(&self, on_invalid: OnInvalidPolicy) -> PolicyState {
            PolicyState::load(self.path.clone(), on_invalid).0
        }
    }

    impl Drop for TempPolicy {
        fn drop(&mut self) {
            self.remove();
        }
    }

    fn model_request(model: &str) -> Request {
        Request {
            model: Some(model.to_string()),
            ..Request::default()
        }
    }

    const ENFORCING: &str = r#"
version = 1
mode = "enforce"
default = "deny"

[[rule]]
id = "allowed-models"
action = "allow"
when.models = ["qwen/*"]
"#;

    #[test]
    fn an_unconfigured_node_serves_everything_and_says_so() {
        let temp = TempPolicy::new(None);
        let (state, messages) = PolicyState::load(temp.path.clone(), OnInvalidPolicy::Deny);

        let response = state.check(model_request("anything/at-all"), false);
        assert_eq!(response.decision, "allow");
        assert!(!response.would_deny);
        assert_eq!(response.policy_status, "absent");
        assert_eq!(response.mode, "dry-run");

        let view = state.view();
        assert_eq!(view.status, "absent");
        assert!(view.warnings[0].contains("No policy file"));
        assert!(messages[0].contains("no policy file"));
    }

    #[test]
    fn an_enforcing_policy_refuses_with_a_structured_error() {
        let temp = TempPolicy::new(Some(ENFORCING));
        let state = temp.state(OnInvalidPolicy::Deny);

        let allowed = state.check(model_request("qwen/qwen3-8b"), false);
        assert_eq!(allowed.decision, "allow");
        assert!(allowed.error.is_none());

        let refused = state.check(model_request("someone/else"), false);
        assert_eq!(refused.decision, "deny");
        assert!(refused.enforced);
        assert_eq!(refused.code, "policy.default_deny");
        let error = refused.error.expect("a refusal carries an error envelope");
        assert_eq!(error.kind, REFUSAL_TYPE);
        assert!(error.message.contains("Local workload policy"));
        assert_eq!(error.node_policy_source, temp.path.display().to_string());
    }

    #[test]
    fn dry_run_serves_the_request_and_records_what_it_would_have_done() {
        let temp = TempPolicy::new(Some(
            r#"
version = 1
default = "deny"

[[rule]]
id = "allowed-models"
action = "allow"
when.models = ["qwen/*"]
"#,
        ));
        let state = temp.state(OnInvalidPolicy::Deny);

        let response = state.check(model_request("someone/else"), false);

        assert_eq!(response.decision, "allow");
        assert!(response.would_deny);
        assert!(!response.enforced);
        // Nothing was refused, so there is no refusal to hand back.
        assert!(response.error.is_none());
        assert!(response.reason.starts_with("dry-run:"));

        let report = state.report(10);
        assert_eq!(report.counters.evaluated, 1);
        assert_eq!(report.counters.would_deny, 1);
        assert_eq!(report.counters.denied, 0);
        assert!(report.summary.contains("Dry-run"));
        assert_eq!(report.top_models[0].value, "someone/else");
    }

    #[test]
    fn a_policy_file_that_does_not_load_fails_closed_by_default() {
        let temp = TempPolicy::new(Some("version = 1\nmodle = \"enforce\"\n"));
        let (state, messages) = PolicyState::load(temp.path.clone(), OnInvalidPolicy::Deny);

        let response = state.check(model_request("qwen/qwen3-8b"), false);

        assert_eq!(response.decision, "deny");
        assert!(response.enforced);
        assert_eq!(response.code, "policy.unavailable");
        assert_eq!(response.policy_status, "invalid");
        assert!(response.error.is_some());
        assert!(messages.iter().any(|line| line.contains("failing closed")));

        let view = state.view();
        assert!(!view.errors.is_empty());
        assert!(view.warnings[0].contains("refusing"));
    }

    #[test]
    fn the_escape_hatch_keeps_serving_and_says_the_policy_is_not_applied() {
        let temp = TempPolicy::new(Some("version = 1\nmodle = \"enforce\"\n"));
        let state = temp.state(OnInvalidPolicy::Allow);

        let response = state.check(model_request("qwen/qwen3-8b"), false);

        assert_eq!(response.decision, "allow");
        assert_eq!(response.policy_status, "invalid");
        assert!(state.view().warnings[0].contains("NOT being applied"));
    }

    #[test]
    fn a_bad_reload_never_replaces_a_good_policy() {
        let temp = TempPolicy::new(Some(ENFORCING));
        let state = temp.state(OnInvalidPolicy::Deny);

        temp.write("version = 1\n[[rule]]\nid = \"broken\"\naction = \"dney\"\n");
        let failure = state.reload().expect_err("an invalid file must not load");

        assert!(failure.message.contains("still in force"));
        assert_eq!(failure.status, PolicyStatus::Loaded);
        assert!(failure.errors[0].contains("dney"));
        // The previously loaded rules are still deciding.
        assert_eq!(
            state.check(model_request("qwen/qwen3-8b"), false).decision,
            "allow"
        );
        assert_eq!(
            state.check(model_request("other/model"), false).decision,
            "deny"
        );
    }

    #[test]
    fn a_good_reload_swaps_the_rules_in_place() {
        let temp = TempPolicy::new(Some(ENFORCING));
        let state = temp.state(OnInvalidPolicy::Deny);
        assert_eq!(
            state.check(model_request("other/model"), false).decision,
            "deny"
        );

        temp.write(
            r#"
version = 1
mode = "enforce"
default = "allow"
"#,
        );
        let response = state.reload().expect("valid policy reloads");

        assert_eq!(response.rules, 0);
        assert_eq!(
            state.check(model_request("other/model"), false).decision,
            "allow"
        );
    }

    #[test]
    fn deleting_the_policy_file_is_not_a_way_to_drop_the_rules() {
        let temp = TempPolicy::new(Some(ENFORCING));
        let state = temp.state(OnInvalidPolicy::Deny);

        temp.remove();
        let failure = state
            .reload()
            .expect_err("a missing file must not clear a policy");

        assert!(failure.message.contains("keeping"));
        assert_eq!(
            state.check(model_request("other/model"), false).decision,
            "deny"
        );
    }

    #[test]
    fn an_invalid_file_written_over_no_file_starts_failing_closed() {
        let temp = TempPolicy::new(None);
        let state = temp.state(OnInvalidPolicy::Deny);
        assert_eq!(
            state.check(model_request("anything"), false).decision,
            "allow"
        );

        temp.write("version = 1\nmodle = true\n");
        state.reload().expect_err("an invalid file must not load");

        // There was no good policy to keep, so the node fails closed instead of
        // continuing to look unconfigured.
        let response = state.check(model_request("anything"), false);
        assert_eq!(response.decision, "deny");
        assert_eq!(response.code, "policy.unavailable");
    }

    #[test]
    fn the_view_describes_the_loaded_rules_and_required_fields() {
        let temp = TempPolicy::new(Some(
            r#"
version = 1
mode = "enforce"

[[rule]]
id = "per-peer"
action = "limit"
reason = "Fair share."
when.models = ["qwen/*"]
limit = { requests = 60, per_seconds = 60, per = "peer" }
"#,
        ));
        let view = temp.state(OnInvalidPolicy::Deny).view();

        assert_eq!(view.status, "loaded");
        assert_eq!(view.mode, "enforce");
        assert_eq!(view.rules.len(), 1);
        assert_eq!(view.rules[0].action, "limit");
        assert_eq!(
            view.rules[0].limit.as_deref(),
            Some("60 requests per 60 s, per peer")
        );
        assert!(view.rules[0].conditions[0].contains("qwen/*"));
        assert_eq!(view.required_request_fields, vec!["model", "peer"]);
        assert!(view.warnings.is_empty());
    }

    #[test]
    fn explain_is_returned_only_when_requested() {
        let temp = TempPolicy::new(Some(ENFORCING));
        let state = temp.state(OnInvalidPolicy::Deny);

        assert!(
            state
                .check(model_request("qwen/qwen3-8b"), false)
                .trace
                .is_none()
        );

        let explained = state.check(model_request("qwen/qwen3-8b"), true);
        let trace = explained.trace.expect("explain was requested");
        assert_eq!(trace[0].rule_id, "allowed-models");
        assert!(trace[0].matched);
    }

    #[test]
    fn on_invalid_policy_parses_its_documented_spellings() {
        assert_eq!(OnInvalidPolicy::parse("deny"), Ok(OnInvalidPolicy::Deny));
        assert_eq!(OnInvalidPolicy::parse("ALLOW"), Ok(OnInvalidPolicy::Allow));
        assert!(OnInvalidPolicy::parse("maybe").is_err());
    }
}
