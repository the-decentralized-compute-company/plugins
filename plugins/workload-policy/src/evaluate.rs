//! Policy evaluation.
//!
//! This is the whole decision, and it is a pure function of (policy, request,
//! timestamp, rate-limit state). No clock, no filesystem, no network — which is
//! why the tests at the bottom can pin down conflict precedence, midnight
//! wrap-around, and rate-limit exhaustion without a running host.
//!
//! **Structural properties only.** Every condition here is something the node
//! can read off a request descriptor: which model, which peer, how large, what
//! time it is. There is deliberately no way to write a rule about what a prompt
//! *says*; see the README section "What this deliberately does not do".

use crate::clock::Timestamp;
use crate::policy::{Action, Conditions, Decision, LimitScope, Policy, RequiredFields, Rule};
use crate::ratelimit::{Admission, TokenBuckets};

/// The structural facts about one request. Note what is absent: there is no
/// field for prompt text, messages, or any other content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    pub model: Option<String>,
    pub peer: Option<String>,
    pub owner: Option<String>,
    pub kind: Option<String>,
    pub context_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

/// Why a request got the answer it got. Stable strings — callers and dashboards
/// key on these, so treat a change here as a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeCode {
    AllowRule,
    DenyRule,
    DefaultAllow,
    DefaultDeny,
    RateLimited,
    RateLimitCapacity,
    IncompleteRequest,
    PolicyUnavailable,
}

impl OutcomeCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllowRule => "policy.allow_rule",
            Self::DenyRule => "policy.deny_rule",
            Self::DefaultAllow => "policy.default_allow",
            Self::DefaultDeny => "policy.default_deny",
            Self::RateLimited => "policy.rate_limited",
            Self::RateLimitCapacity => "policy.rate_limit_capacity",
            Self::IncompleteRequest => "policy.incomplete_request",
            Self::PolicyUnavailable => "policy.unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleTrace {
    pub rule_id: String,
    pub matched: bool,
    /// The first condition that did not hold, when the rule did not match.
    pub unmet_condition: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub decision: Decision,
    pub code: OutcomeCode,
    pub rule_id: Option<String>,
    pub reason: String,
    /// Fields the policy needs that the request did not carry.
    pub missing_fields: Vec<&'static str>,
    /// Set on a rate-limit refusal.
    pub retry_after_ms: Option<i64>,
    /// Populated only when the caller asked to be told why.
    pub trace: Vec<RuleTrace>,
}

impl Outcome {
    fn deny(code: OutcomeCode, reason: String) -> Self {
        Self {
            decision: Decision::Deny,
            code,
            rule_id: None,
            reason,
            missing_fields: Vec::new(),
            retry_after_ms: None,
            trace: Vec::new(),
        }
    }

    /// The refusal used when the policy file exists but could not be loaded.
    /// Built here so enforcement and reporting share one wording.
    pub fn policy_unavailable(reason: String) -> Self {
        Self::deny(OutcomeCode::PolicyUnavailable, reason)
    }
}

/// Evaluate one request. `buckets` is mutated only by `limit` rules, and only
/// by the single rule that matches.
pub fn evaluate(
    policy: &Policy,
    request: &Request,
    at: Timestamp,
    buckets: &mut TokenBuckets,
    explain: bool,
) -> Outcome {
    let missing = missing_required_fields(&policy.required, request);
    if !missing.is_empty() {
        // Fail closed. A condition over an absent field cannot be satisfied, so
        // accepting an incomplete descriptor would let any caller step around a
        // deny rule by simply not mentioning the model.
        let reason = format!(
            "this node's local policy is written in terms of {}, and the request did not supply: {}",
            policy.required.names().join(", "),
            missing.join(", ")
        );
        let mut outcome = Outcome::deny(OutcomeCode::IncompleteRequest, reason);
        outcome.missing_fields = missing;
        return outcome;
    }

    let mut trace = Vec::new();
    for rule in &policy.rules {
        let unmet = unmet_condition(&rule.when, request, at);
        if explain {
            trace.push(RuleTrace {
                rule_id: rule.id.clone(),
                matched: unmet.is_none(),
                unmet_condition: unmet,
            });
        }
        if unmet.is_some() {
            continue;
        }

        // First match wins. Precedence is document order and nothing else — no
        // specificity heuristics, no implicit deny-beats-allow. An operator can
        // read a policy top to bottom and know what it does.
        let mut outcome = apply_rule(rule, request, at, buckets);
        outcome.trace = trace;
        return outcome;
    }

    let mut outcome = match policy.default_action {
        Decision::Allow => Outcome {
            decision: Decision::Allow,
            code: OutcomeCode::DefaultAllow,
            rule_id: None,
            reason: "no rule matched; this node's policy default is allow".to_string(),
            missing_fields: Vec::new(),
            retry_after_ms: None,
            trace: Vec::new(),
        },
        Decision::Deny => Outcome::deny(
            OutcomeCode::DefaultDeny,
            "no rule matched; this node's policy default is deny".to_string(),
        ),
    };
    outcome.trace = trace;
    outcome
}

fn apply_rule(
    rule: &Rule,
    request: &Request,
    at: Timestamp,
    buckets: &mut TokenBuckets,
) -> Outcome {
    match rule.action {
        Action::Allow => Outcome {
            decision: Decision::Allow,
            code: OutcomeCode::AllowRule,
            rule_id: Some(rule.id.clone()),
            reason: rule.reason.clone().unwrap_or_else(|| {
                format!("allowed by this node's local policy rule '{}'", rule.id)
            }),
            missing_fields: Vec::new(),
            retry_after_ms: None,
            trace: Vec::new(),
        },
        Action::Deny => {
            let mut outcome = Outcome::deny(OutcomeCode::DenyRule, rule.refusal_reason());
            outcome.rule_id = Some(rule.id.clone());
            outcome
        }
        Action::Limit => apply_limit(rule, request, at, buckets),
    }
}

fn apply_limit(
    rule: &Rule,
    request: &Request,
    at: Timestamp,
    buckets: &mut TokenBuckets,
) -> Outcome {
    let Some(limit) = rule.limit else {
        // Unreachable for a validated policy: `action = "limit"` without a
        // [rule.limit] table fails to load. Refuse rather than silently
        // allowing, in case a future edit makes it reachable.
        let mut outcome = Outcome::deny(
            OutcomeCode::DenyRule,
            format!(
                "local policy rule '{}' is a limit rule with no budget and cannot be evaluated",
                rule.id
            ),
        );
        outcome.rule_id = Some(rule.id.clone());
        return outcome;
    };

    let key = bucket_key(rule, limit.per, request);
    let describe = limit.describe();
    match buckets.admit(&key, at.epoch_millis, limit.requests, limit.window_ms()) {
        Admission::Admitted => Outcome {
            decision: Decision::Allow,
            code: OutcomeCode::AllowRule,
            rule_id: Some(rule.id.clone()),
            reason: format!(
                "within the budget of this node's local policy rule '{}' ({describe})",
                rule.id
            ),
            missing_fields: Vec::new(),
            retry_after_ms: None,
            trace: Vec::new(),
        },
        Admission::OverLimit { retry_after_ms } => {
            let base = format!(
                "this node's local policy rule '{}' allows {describe}, and that budget is spent",
                rule.id
            );
            let mut outcome = Outcome::deny(
                OutcomeCode::RateLimited,
                match &rule.reason {
                    Some(reason) => format!("{reason} ({base})"),
                    None => base,
                },
            );
            outcome.rule_id = Some(rule.id.clone());
            outcome.retry_after_ms = Some(retry_after_ms);
            outcome
        }
        Admission::NoCapacity => {
            let mut outcome = Outcome::deny(
                OutcomeCode::RateLimitCapacity,
                format!(
                    "this node is tracking the maximum number of rate-limit buckets, so local policy rule '{}' cannot admit a new {} right now",
                    rule.id,
                    limit.per.as_str()
                ),
            );
            outcome.rule_id = Some(rule.id.clone());
            outcome
        }
    }
}

/// Bucket keys are rule-scoped, so two rules limiting the same peer keep
/// separate budgets. The unit separator cannot appear in a rule id, so an
/// identifier containing one cannot impersonate another rule's bucket.
fn bucket_key(rule: &Rule, scope: LimitScope, request: &Request) -> String {
    let value = match scope {
        LimitScope::Node => "",
        LimitScope::Peer => request.peer.as_deref().unwrap_or_default(),
        LimitScope::Owner => request.owner.as_deref().unwrap_or_default(),
        LimitScope::Model => request.model.as_deref().unwrap_or_default(),
    };
    format!("{}\u{1f}{}\u{1f}{}", rule.id, scope.as_str(), value)
}

fn missing_required_fields(required: &RequiredFields, request: &Request) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if required.model && is_blank(&request.model) {
        missing.push("model");
    }
    if required.peer && is_blank(&request.peer) {
        missing.push("peer");
    }
    if required.owner && is_blank(&request.owner) {
        missing.push("owner");
    }
    if required.kind && is_blank(&request.kind) {
        missing.push("kind");
    }
    if required.context_tokens && request.context_tokens.is_none() {
        missing.push("context_tokens");
    }
    if required.max_output_tokens && request.max_output_tokens.is_none() {
        missing.push("max_output_tokens");
    }
    missing
}

/// An empty string is treated as an absent value: `peer = ""` is not an
/// identity, and accepting it would create one shared anonymous rate-limit
/// bucket that every caller could hide in.
fn is_blank(value: &Option<String>) -> bool {
    value.as_deref().is_none_or(|value| value.trim().is_empty())
}

/// The first declared condition that does not hold, or `None` when the rule
/// matches. Order is fixed so the `explain` trace is reproducible.
fn unmet_condition(
    conditions: &Conditions,
    request: &Request,
    at: Timestamp,
) -> Option<&'static str> {
    // A condition over a field the request did not carry is *not* satisfied, so
    // every arm here reports the condition rather than falling through. The
    // missing-required-fields check above is what stops that from becoming a
    // way to dodge a deny rule by omitting a field.
    if let Some(models) = &conditions.models {
        let Some(model) = request.model.as_deref() else {
            return Some("models");
        };
        if !models.iter().any(|pattern| pattern.matches(model)) {
            return Some("models");
        }
    }
    if let Some(peers) = &conditions.peers {
        let Some(peer) = request.peer.as_deref() else {
            return Some("peers");
        };
        if !peers.iter().any(|pattern| pattern.matches(peer)) {
            return Some("peers");
        }
    }
    if let Some(owners) = &conditions.owners {
        let Some(owner) = request.owner.as_deref() else {
            return Some("owners");
        };
        if !owners.iter().any(|pattern| pattern.matches(owner)) {
            return Some("owners");
        }
    }
    if let Some(kinds) = &conditions.kinds {
        let Some(kind) = request.kind.as_deref() else {
            return Some("kinds");
        };
        let kind = kind.trim().to_ascii_lowercase();
        if !kinds.iter().any(|allowed| allowed == &kind) {
            return Some("kinds");
        }
    }
    if let Some(threshold) = conditions.context_tokens_over {
        let Some(context_tokens) = request.context_tokens else {
            return Some("context_tokens_over");
        };
        if context_tokens <= threshold {
            return Some("context_tokens_over");
        }
    }
    if let Some(threshold) = conditions.max_output_tokens_over {
        let Some(max_output_tokens) = request.max_output_tokens else {
            return Some("max_output_tokens_over");
        };
        if max_output_tokens <= threshold {
            return Some("max_output_tokens_over");
        }
    }
    if let Some(hours) = &conditions.hours
        && !hours.iter().any(|window| window.contains(at.minute_of_day))
    {
        return Some("hours");
    }
    if let Some(days) = &conditions.days
        && !days.contains(&at.weekday)
    {
        return Some("days");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Weekday;
    use crate::policy::parse_policy;

    fn policy(text: &str) -> Policy {
        parse_policy(text).expect("test policy must be valid")
    }

    fn request(model: &str) -> Request {
        Request {
            model: Some(model.to_string()),
            ..Request::default()
        }
    }

    /// Wednesday, 14:30.
    fn afternoon() -> Timestamp {
        Timestamp::at(1_700_000_000_000, 14 * 60 + 30, Weekday::Wed)
    }

    /// Wednesday, 23:30.
    fn late_evening() -> Timestamp {
        Timestamp::at(1_700_000_000_000, 23 * 60 + 30, Weekday::Wed)
    }

    fn decide(policy: &Policy, request: &Request, at: Timestamp) -> Outcome {
        evaluate(policy, request, at, &mut TokenBuckets::new(), false)
    }

    #[test]
    fn an_empty_policy_allows_and_says_it_was_the_default() {
        let outcome = decide(&Policy::permissive(), &request("any/model"), afternoon());

        assert_eq!(outcome.decision, Decision::Allow);
        assert_eq!(outcome.code, OutcomeCode::DefaultAllow);
        assert_eq!(outcome.rule_id, None);
    }

    #[test]
    fn an_allow_list_is_the_pair_of_an_allow_rule_and_a_default_deny() {
        let policy = policy(
            r#"
version = 1
mode = "enforce"
default = "deny"

[[rule]]
id = "allowed-models"
action = "allow"
when.models = ["qwen/*", "meta/llama-3*"]
"#,
        );

        assert_eq!(
            decide(&policy, &request("qwen/qwen3-8b"), afternoon()).code,
            OutcomeCode::AllowRule
        );
        assert_eq!(
            decide(&policy, &request("Meta/Llama-3.1-8B"), afternoon()).code,
            OutcomeCode::AllowRule
        );

        let refused = decide(&policy, &request("some/other-model"), afternoon());
        assert_eq!(refused.decision, Decision::Deny);
        assert_eq!(refused.code, OutcomeCode::DefaultDeny);
    }

    #[test]
    fn the_first_matching_rule_wins_even_when_a_later_rule_disagrees() {
        let text = |first: &str, second: &str| {
            format!(
                r#"
version = 1
mode = "enforce"

[[rule]]
id = "first"
action = "{first}"
when.models = ["qwen/*"]

[[rule]]
id = "second"
action = "{second}"
when.models = ["qwen/*"]
"#
            )
        };

        let allow_first = policy(&text("allow", "deny"));
        let outcome = decide(&allow_first, &request("qwen/qwen3-8b"), afternoon());
        assert_eq!(outcome.decision, Decision::Allow);
        assert_eq!(outcome.rule_id.as_deref(), Some("first"));

        // Swapping the order swaps the answer. Nothing else does.
        let deny_first = policy(&text("deny", "allow"));
        let outcome = decide(&deny_first, &request("qwen/qwen3-8b"), afternoon());
        assert_eq!(outcome.decision, Decision::Deny);
        assert_eq!(outcome.rule_id.as_deref(), Some("first"));
    }

    #[test]
    fn size_conditions_are_strictly_greater_than_the_threshold() {
        let policy = policy(
            r#"
version = 1
mode = "enforce"

[[rule]]
id = "context-cap"
action = "deny"
reason = "This node caps context at 8192 tokens."
when.context_tokens_over = 8192
"#,
        );
        let with_context = |tokens: u64| Request {
            context_tokens: Some(tokens),
            ..Request::default()
        };

        assert_eq!(
            decide(&policy, &with_context(8192), afternoon()).decision,
            Decision::Allow
        );
        let refused = decide(&policy, &with_context(8193), afternoon());
        assert_eq!(refused.decision, Decision::Deny);
        assert_eq!(refused.reason, "This node caps context at 8192 tokens.");
    }

    #[test]
    fn an_overnight_window_is_the_only_time_work_is_accepted() {
        let policy = policy(
            r#"
version = 1
mode = "enforce"
default = "deny"

[[rule]]
id = "overnight"
action = "allow"
when.hours = ["22:00-06:00"]
"#,
        );

        assert_eq!(
            decide(&policy, &Request::default(), late_evening()).decision,
            Decision::Allow
        );
        assert_eq!(
            decide(&policy, &Request::default(), afternoon()).decision,
            Decision::Deny
        );
        // 05:59 is still inside the wrapped window; 06:00 is not.
        let dawn = Timestamp::at(0, 5 * 60 + 59, Weekday::Thu);
        assert_eq!(
            decide(&policy, &Request::default(), dawn).decision,
            Decision::Allow
        );
        let morning = Timestamp::at(0, 6 * 60, Weekday::Thu);
        assert_eq!(
            decide(&policy, &Request::default(), morning).decision,
            Decision::Deny
        );
    }

    #[test]
    fn weekday_conditions_gate_on_the_evaluated_day() {
        let policy = policy(
            r#"
version = 1
mode = "enforce"
default = "deny"

[[rule]]
id = "weekends-only"
action = "allow"
when.days = ["sat", "sun"]
"#,
        );

        let saturday = Timestamp::at(0, 60, Weekday::Sat);
        assert_eq!(
            decide(&policy, &Request::default(), saturday).decision,
            Decision::Allow
        );
        assert_eq!(
            decide(&policy, &Request::default(), afternoon()).decision,
            Decision::Deny
        );
    }

    #[test]
    fn all_conditions_in_one_rule_must_hold_together() {
        let policy = policy(
            r#"
version = 1
mode = "enforce"

[[rule]]
id = "big-jobs-off-hours"
action = "deny"
when.models = ["qwen/*"]
when.context_tokens_over = 1000
when.hours = ["09:00-17:00"]
"#,
        );
        let big = |model: &str| Request {
            model: Some(model.to_string()),
            context_tokens: Some(5_000),
            ..Request::default()
        };
        let business_hours = Timestamp::at(0, 10 * 60, Weekday::Wed);

        assert_eq!(
            decide(&policy, &big("qwen/qwen3-8b"), business_hours).decision,
            Decision::Deny
        );
        // Wrong model.
        assert_eq!(
            decide(&policy, &big("meta/llama-3"), business_hours).decision,
            Decision::Allow
        );
        // Right model, wrong hour.
        assert_eq!(
            decide(&policy, &big("qwen/qwen3-8b"), late_evening()).decision,
            Decision::Allow
        );
    }

    #[test]
    fn a_request_missing_a_field_the_policy_needs_is_refused_not_waved_through() {
        let policy = policy(
            r#"
version = 1
mode = "enforce"

[[rule]]
id = "block-a-model"
action = "deny"
when.models = ["banned/*"]
"#,
        );

        let outcome = decide(&policy, &Request::default(), afternoon());

        assert_eq!(outcome.decision, Decision::Deny);
        assert_eq!(outcome.code, OutcomeCode::IncompleteRequest);
        assert_eq!(outcome.missing_fields, vec!["model"]);
        assert!(outcome.reason.contains("did not supply: model"));
    }

    #[test]
    fn a_blank_identifier_counts_as_missing() {
        let policy = policy(
            r#"
version = 1
[[rule]]
id = "known-peers"
action = "allow"
when.peers = ["12D3*"]
"#,
        );
        let blank = Request {
            peer: Some("   ".to_string()),
            ..Request::default()
        };

        assert_eq!(
            decide(&policy, &blank, afternoon()).code,
            OutcomeCode::IncompleteRequest
        );
    }

    #[test]
    fn a_policy_that_needs_nothing_accepts_a_bare_request() {
        let policy = policy(
            r#"
version = 1
mode = "enforce"
default = "deny"

[[rule]]
id = "overnight"
action = "allow"
when.hours = ["22:00-06:00"]
"#,
        );

        // Time-of-day comes from the node, not the caller, so an empty
        // descriptor is still complete here.
        assert_eq!(
            decide(&policy, &Request::default(), late_evening()).code,
            OutcomeCode::AllowRule
        );
    }

    #[test]
    fn a_limit_rule_admits_its_burst_and_then_refuses_with_a_retry_hint() {
        let policy = policy(
            r#"
version = 1
mode = "enforce"

[[rule]]
id = "per-peer"
action = "limit"
limit = { requests = 2, per_seconds = 60, per = "peer" }
"#,
        );
        let mut buckets = TokenBuckets::new();
        let peer = |name: &str| Request {
            peer: Some(name.to_string()),
            ..Request::default()
        };
        let at = afternoon();

        assert_eq!(
            evaluate(&policy, &peer("peer-a"), at, &mut buckets, false).decision,
            Decision::Allow
        );
        assert_eq!(
            evaluate(&policy, &peer("peer-a"), at, &mut buckets, false).decision,
            Decision::Allow
        );

        let refused = evaluate(&policy, &peer("peer-a"), at, &mut buckets, false);
        assert_eq!(refused.decision, Decision::Deny);
        assert_eq!(refused.code, OutcomeCode::RateLimited);
        assert_eq!(refused.retry_after_ms, Some(30_000));

        // A different peer has its own budget.
        assert_eq!(
            evaluate(&policy, &peer("peer-b"), at, &mut buckets, false).decision,
            Decision::Allow
        );
    }

    #[test]
    fn two_limit_rules_do_not_share_a_budget() {
        let policy = policy(
            r#"
version = 1
mode = "enforce"

[[rule]]
id = "chat-rate"
action = "limit"
when.kinds = ["chat"]
limit = { requests = 1, per_seconds = 60, per = "node" }

[[rule]]
id = "embedding-rate"
action = "limit"
when.kinds = ["embedding"]
limit = { requests = 1, per_seconds = 60, per = "node" }
"#,
        );
        let mut buckets = TokenBuckets::new();
        let of_kind = |kind: &str| Request {
            kind: Some(kind.to_string()),
            ..Request::default()
        };
        let at = afternoon();

        assert_eq!(
            evaluate(&policy, &of_kind("chat"), at, &mut buckets, false).decision,
            Decision::Allow
        );
        assert_eq!(
            evaluate(&policy, &of_kind("embedding"), at, &mut buckets, false).decision,
            Decision::Allow
        );
        assert_eq!(
            evaluate(&policy, &of_kind("chat"), at, &mut buckets, false).decision,
            Decision::Deny
        );
    }

    #[test]
    fn kinds_match_case_insensitively() {
        let policy = policy(
            r#"
version = 1
mode = "enforce"

[[rule]]
id = "no-embeddings"
action = "deny"
when.kinds = ["Embedding"]
"#,
        );
        let request = Request {
            kind: Some("EMBEDDING".to_string()),
            ..Request::default()
        };

        assert_eq!(
            decide(&policy, &request, afternoon()).decision,
            Decision::Deny
        );
    }

    #[test]
    fn explain_records_every_rule_considered_and_stops_at_the_match() {
        let policy = policy(
            r#"
version = 1
mode = "enforce"

[[rule]]
id = "not-this-one"
action = "deny"
when.models = ["banned/*"]

[[rule]]
id = "this-one"
action = "deny"
when.models = ["qwen/*"]

[[rule]]
id = "never-reached"
action = "allow"
"#,
        );

        let outcome = evaluate(
            &policy,
            &request("qwen/qwen3-8b"),
            afternoon(),
            &mut TokenBuckets::new(),
            true,
        );

        assert_eq!(outcome.rule_id.as_deref(), Some("this-one"));
        assert_eq!(outcome.trace.len(), 2);
        assert_eq!(outcome.trace[0].rule_id, "not-this-one");
        assert!(!outcome.trace[0].matched);
        assert_eq!(outcome.trace[0].unmet_condition, Some("models"));
        assert!(outcome.trace[1].matched);
    }

    #[test]
    fn no_trace_is_produced_unless_it_is_asked_for() {
        let outcome = decide(&Policy::permissive(), &request("any/model"), afternoon());

        assert!(outcome.trace.is_empty());
    }

    #[test]
    fn outcome_codes_are_stable_strings() {
        assert_eq!(OutcomeCode::DenyRule.as_str(), "policy.deny_rule");
        assert_eq!(
            OutcomeCode::IncompleteRequest.as_str(),
            "policy.incomplete_request"
        );
        assert_eq!(
            OutcomeCode::PolicyUnavailable.as_str(),
            "policy.unavailable"
        );
    }
}
