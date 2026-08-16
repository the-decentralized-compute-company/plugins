//! The host admission hook.
//!
//! This is the part that makes the plugin *enforcing* rather than advisory. The
//! host invokes it before it schedules any inbound work, consumes the answer,
//! and turns a refusal into a structured HTTP error. See
//! `docs/plugins/admission.md` in the tdcc repository for the host side of the
//! contract.
//!
//! It runs the same evaluator as the `check` tool, deliberately: an operator who
//! debugged a policy with `check` must be looking at the same decision the node
//! actually makes. The only difference is the shape of the answer.
//!
//! `check` is kept as-is and remains part of the compatibility surface. It is
//! how you ask "what would this node do with a request like *that*" without
//! submitting one, and it is what a gateway in front of a node still uses.

use tdcc_plugin::admission::{AdmissionDecision, AdmissionRequest};

use crate::evaluate::Request;
use crate::state::{CheckResponse, PolicyState};

impl From<AdmissionRequest> for Request {
    fn from(request: AdmissionRequest) -> Self {
        Self {
            model: request.model,
            peer: request.peer,
            owner: request.owner,
            kind: request.kind,
            // The host may mark this an estimate. The policy treats it as the
            // context size either way: a rule that draws a line at 8192 tokens
            // against an approximation is still the operator's line, and
            // refusing to evaluate it would leave the rule doing nothing. What
            // the estimate means is documented rather than silently absorbed.
            context_tokens: request.context_tokens,
            max_output_tokens: request.max_output_tokens,
        }
    }
}

/// Decide one inbound request for the host.
pub fn admit(state: &PolicyState, request: AdmissionRequest) -> AdmissionDecision {
    // No trace: this runs in front of every request, and a trace is a debugging
    // aid for a human reading one answer. Use `check` with `"explain": true`
    // when a rule is not doing what you expected.
    decision_from(state.check(request.into(), false))
}

/// Translate this plugin's answer into the host's.
///
/// The mapping is deliberately narrow. `decision` is the field to act on — in
/// dry-run it is `allow` even when `would_deny` is true, which is the whole
/// point of dry-run — so this reads `decision` and nothing else. Reading
/// `would_deny` here would make dry-run silently enforcing.
fn decision_from(response: CheckResponse) -> AdmissionDecision {
    let Some(refusal) = response.error else {
        return AdmissionDecision::allow();
    };
    let mut decision = AdmissionDecision::deny(refusal.code, refusal.message);
    decision.rule_id = refusal.rule_id;
    decision.retry_after_ms = refusal.retry_after_ms;
    decision
}

#[cfg(test)]
mod tests {
    use tdcc_plugin::admission::AdmissionVerdict;

    use super::*;
    use crate::state::OnInvalidPolicy;

    fn state_with(policy: &str) -> PolicyState {
        let directory = std::env::temp_dir().join(format!(
            "workload-policy-admit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock after 1970")
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).expect("a scratch directory");
        let path = directory.join("workload-policy.toml");
        std::fs::write(&path, policy).expect("write the policy");
        let (state, _messages) = PolicyState::load(path, OnInvalidPolicy::Deny);
        state
    }

    fn chat_request() -> AdmissionRequest {
        AdmissionRequest {
            model: Some("qwen/qwen3-8b".into()),
            kind: Some("chat".into()),
            context_tokens: Some(16_384),
            context_tokens_estimated: true,
            ..AdmissionRequest::default()
        }
    }

    /// The gap this closes: an enforcing rule now actually stops the request,
    /// and the answer carries everything the host needs to explain it.
    #[test]
    fn an_enforcing_deny_rule_refuses_the_request() {
        let state = state_with(
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

        let decision = admit(&state, chat_request());

        assert_eq!(decision.decision, AdmissionVerdict::Deny);
        assert_eq!(decision.code.as_deref(), Some("policy.deny_rule"));
        assert_eq!(decision.rule_id.as_deref(), Some("context-cap"));
        assert!(
            decision
                .reason
                .as_deref()
                .expect("a refusal carries a sentence")
                .contains("caps context at 8192"),
            "the operator's own words reach the caller"
        );
    }

    #[test]
    fn a_request_the_policy_permits_is_admitted() {
        let state = state_with(
            r#"
version = 1
mode = "enforce"

[[rule]]
id = "context-cap"
action = "deny"
when.context_tokens_over = 8192
"#,
        );

        let decision = admit(
            &state,
            AdmissionRequest {
                context_tokens: Some(1_024),
                ..chat_request()
            },
        );

        assert_eq!(decision.decision, AdmissionVerdict::Allow);
        assert!(decision.code.is_none());
    }

    /// Dry-run must stay dry-run at the enforcement point. A plugin that started
    /// refusing the moment it was wired to the host would make the recommended
    /// "install, observe, then enforce" workflow a trap.
    #[test]
    fn dry_run_admits_what_an_enforcing_policy_would_refuse() {
        let state = state_with(
            r#"
version = 1
mode = "dry-run"

[[rule]]
id = "context-cap"
action = "deny"
when.context_tokens_over = 8192
"#,
        );

        let response = state.check(chat_request().into(), false);
        assert!(response.would_deny, "the rule matches");
        assert_eq!(
            decision_from(response).decision,
            AdmissionVerdict::Allow,
            "dry-run serves; it does not enforce"
        );
    }

    /// A rule keyed on a field the host cannot supply must refuse, not silently
    /// match nothing. At the OpenAI ingress the peer identity is genuinely
    /// unavailable, so a peer allow-list closes the node rather than opening it.
    #[test]
    fn a_rule_needing_a_field_the_host_cannot_supply_refuses() {
        let state = state_with(
            r#"
version = 1
mode = "enforce"
default = "deny"

[[rule]]
id = "known-peers"
action = "allow"
when.peers = ["12D3KooW*"]
"#,
        );

        let decision = admit(&state, chat_request());

        assert_eq!(decision.decision, AdmissionVerdict::Deny);
        assert_eq!(
            decision.code.as_deref(),
            Some("policy.incomplete_request"),
            "a missing field is named, not treated as an unmet condition"
        );
    }

    /// A rate-limit refusal carries `retry_after_ms`, which the host turns into
    /// a 429 with a `Retry-After` header rather than a flat 403.
    #[test]
    fn a_rate_limited_refusal_tells_the_caller_when_to_come_back() {
        let state = state_with(
            r#"
version = 1
mode = "enforce"

[[rule]]
id = "fair-share"
action = "limit"
limit = { requests = 1, per_seconds = 60, per = "node" }
"#,
        );

        assert_eq!(
            admit(&state, chat_request()).decision,
            AdmissionVerdict::Allow,
            "the first request spends the budget"
        );
        let decision = admit(&state, chat_request());

        assert_eq!(decision.decision, AdmissionVerdict::Deny);
        assert_eq!(decision.code.as_deref(), Some("policy.rate_limited"));
        assert!(
            decision.retry_after_ms.unwrap_or_default() > 0,
            "a rate-limited caller is told when to retry"
        );
    }

    /// The host descriptor and the policy's request model must line up field for
    /// field, or a rule silently stops applying.
    #[test]
    fn every_descriptor_field_the_policy_understands_is_carried_across() {
        let request = AdmissionRequest {
            model: Some("qwen/qwen3-8b".into()),
            kind: Some("chat".into()),
            peer: Some("12D3KooWabc".into()),
            owner: Some("team-a".into()),
            context_tokens: Some(4_096),
            context_tokens_estimated: true,
            max_output_tokens: Some(512),
            stream: Some(true),
        };

        let carried: Request = request.into();

        assert_eq!(carried.model.as_deref(), Some("qwen/qwen3-8b"));
        assert_eq!(carried.kind.as_deref(), Some("chat"));
        assert_eq!(carried.peer.as_deref(), Some("12D3KooWabc"));
        assert_eq!(carried.owner.as_deref(), Some("team-a"));
        assert_eq!(carried.context_tokens, Some(4_096));
        assert_eq!(carried.max_output_tokens, Some(512));
    }
}
