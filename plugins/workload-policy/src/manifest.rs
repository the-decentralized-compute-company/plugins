//! Everything `workload-policy` contributes to the host, in one declaration.
//!
//! The `admission` hook is the one that matters: the host calls it before it
//! schedules any inbound work and acts on the answer. The four operations
//! beside it are for humans and gateways — each projected twice, once as an MCP
//! tool for an agent or the mesh MCP endpoint, and once as an HTTP route for a
//! gateway or a script. Every projection runs the same evaluator, so what the
//! node does and what `check` reports cannot drift.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`,
//! `admission`, then lifecycle hooks. Omitting a field is fine, reordering is
//! not.
//!
//! No `mesh` and no `events` are declared. Delivery is allowlist-based, so this
//! plugin receives no channel messages and no mesh events — it does not need
//! them, and a policy engine is a bad place to accept unsolicited input.

use schemars::JsonSchema;
use serde::Deserialize;
use tdcc_plugin::{
    PluginError, PluginMetadata, SimplePlugin, admission, capability, http, mcp, plugin,
    plugin_server_info,
};

use crate::admit::admit;
use crate::evaluate::Request;
use crate::state::{
    CheckResponse, PolicyState, PolicyView, ReloadFailure, ReloadResponse, ReportResponse,
};

/// Recent decisions returned by `report` when the caller does not ask for a
/// specific number.
const DEFAULT_REPORT_LIMIT: u32 = 50;
/// Ceiling on one report response, independent of the ring size.
const MAX_REPORT_LIMIT: u32 = 500;

/// One request, described structurally.
///
/// `deny_unknown_fields` is load-bearing rather than tidy: it is what makes
/// "this tool cannot be handed prompt text" true instead of merely intended. A
/// caller that sends `messages` or `prompt` gets an invalid-arguments error, so
/// content cannot arrive here by accident and end up in the decision log.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckArgs {
    /// Model the request asks this node to serve, as the caller wrote it — for
    /// example "qwen/qwen3-8b". Matched case-insensitively.
    #[serde(default)]
    pub model: Option<String>,
    /// Mesh peer id that submitted the request. Matched case-sensitively,
    /// because peer ids are opaque identifiers whose alphabets are case-bearing.
    #[serde(default)]
    pub peer: Option<String>,
    /// Owner or tenant identity the submitting side attached to the request.
    /// Matched case-sensitively.
    #[serde(default)]
    pub owner: Option<String>,
    /// What kind of work this is: "chat", "completion", "embedding", and so on.
    /// Matched case-insensitively.
    #[serde(default)]
    pub kind: Option<String>,
    /// Size of the prompt in tokens. The count only — this tool never receives
    /// the prompt itself.
    #[serde(default)]
    pub context_tokens: Option<u64>,
    /// Largest number of output tokens the request may generate.
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// Include a per-rule trace showing which rules were considered and which
    /// condition stopped each one. Useful while writing a policy.
    #[serde(default)]
    pub explain: bool,
}

impl From<CheckArgs> for Request {
    fn from(args: CheckArgs) -> Self {
        Self {
            model: args.model,
            peer: args.peer,
            owner: args.owner,
            kind: args.kind,
            context_tokens: args.context_tokens,
            max_output_tokens: args.max_output_tokens,
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ReportArgs {
    /// How many recent decisions to include, newest first. Defaults to 50 and
    /// is clamped to 500; the lifetime counters are always returned in full.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Input for the operations that take none.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoArgs {}

fn clamp_report_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(DEFAULT_REPORT_LIMIT).min(MAX_REPORT_LIMIT) as usize
}

fn check(state: &PolicyState, args: CheckArgs) -> CheckResponse {
    let explain = args.explain;
    state.check(args.into(), explain)
}

fn report(state: &PolicyState, args: ReportArgs) -> ReportResponse {
    state.report(clamp_report_limit(args.limit))
}

/// A reload that did not happen is an error, not a quiet success — but it is an
/// error that carries the whole complaint list, so an operator can fix the file
/// in one pass.
fn reload(state: &PolicyState) -> Result<ReloadResponse, PluginError> {
    state.reload().map_err(reload_error)
}

fn reload_error(failure: ReloadFailure) -> PluginError {
    let ReloadFailure {
        message,
        errors,
        status,
        source,
    } = failure;
    let mut error = PluginError::internal(message.clone());
    error.data_json = serde_json::json!({
        "type": "workload_policy_reload_failed",
        "message": message,
        "errors": errors,
        "policy_status": status.as_str(),
        "policy_source": source,
    })
    .to_string();
    error
}

pub fn workload_policy_plugin(state: PolicyState) -> SimplePlugin {
    plugin! {
        metadata: PluginMetadata::new(
            "workload-policy",
            "0.2.0",
            plugin_server_info(
                "workload-policy",
                "0.2.0",
                "Workload policy",
                "Node-side admission policy: what this machine will and will not accept",
                None::<String>,
            ),
        ),

        // A named contract, so a caller can depend on "something that decides"
        // rather than on this plugin's id. The host's own admission capability
        // is declared by the `admission` section below; this one is for anything
        // that wants this plugin's richer `check`/`report`/`policy` surface.
        provides: [capability("workload-policy.v1")],

        mcp: [
            mcp::tool("check")
                .description(
                    "Ask this node's local workload policy whether it accepts a request, \
                     described structurally: model, submitting peer, owner, request kind, \
                     context size, and output size. Returns allow or deny with a stable \
                     outcome code and, on a refusal, an error envelope to hand back to the \
                     submitter. Never accepts prompt content, and judges nothing about what \
                     a request says.",
                )
                .input::<CheckArgs>()
                .output::<CheckResponse>()
                .handle({
                    let state = state.clone();
                    move |args: CheckArgs, _context| {
                        let state = state.clone();
                        Box::pin(async move { Ok(check(&state, args)) })
                    }
                }),

            mcp::tool("report")
                .description(
                    "Show what this node's policy has been doing: how many requests were \
                     evaluated, how many were refused, and — in dry-run — how many an \
                     enforcing policy would have refused. Includes the most recent decisions \
                     so a first policy can be written from real traffic.",
                )
                .input::<ReportArgs>()
                .output::<ReportResponse>()
                .handle({
                    let state = state.clone();
                    move |args: ReportArgs, _context| {
                        let state = state.clone();
                        Box::pin(async move { Ok(report(&state, args)) })
                    }
                }),

            mcp::tool("policy")
                .description(
                    "Show the policy this node currently has loaded: its source file, whether \
                     it loaded at all, whether it is enforcing or in dry-run, every rule in \
                     evaluation order, and any warnings or load errors.",
                )
                .input::<NoArgs>()
                .output::<PolicyView>()
                .handle({
                    let state = state.clone();
                    move |_args: NoArgs, _context| {
                        let state = state.clone();
                        Box::pin(async move { Ok(state.view()) })
                    }
                }),

            mcp::tool("reload")
                .description(
                    "Re-read the policy file this process was started with. On success the new \
                     rules take effect immediately; on failure the previously loaded policy \
                     stays in force and the error lists everything wrong with the file. Takes \
                     no path: the file is fixed at startup.",
                )
                .input::<NoArgs>()
                .output::<ReloadResponse>()
                .handle({
                    let state = state.clone();
                    move |_args: NoArgs, _context| {
                        let state = state.clone();
                        Box::pin(async move { reload(&state) })
                    }
                }),
        ],

        http: [
            // POST /api/plugins/workload-policy/http/check
            //
            // A refusal is a 200 with "decision": "deny". The HTTP status
            // describes whether the *evaluation* worked, not what it decided —
            // a gateway that only checks the status code will fail open.
            http::post("/check")
                .description("Evaluate one request against this node's local workload policy.")
                .input::<CheckArgs>()
                .output::<CheckResponse>()
                .handle({
                    let state = state.clone();
                    move |args: CheckArgs, _context| {
                        let state = state.clone();
                        Box::pin(async move { Ok(check(&state, args)) })
                    }
                }),

            // GET /api/plugins/workload-policy/http/report?limit=100
            http::get("/report")
                .description("Counters and recent decisions, newest first.")
                .input::<ReportArgs>()
                .output::<ReportResponse>()
                .handle({
                    let state = state.clone();
                    move |args: ReportArgs, _context| {
                        let state = state.clone();
                        Box::pin(async move { Ok(report(&state, args)) })
                    }
                }),

            // GET /api/plugins/workload-policy/http/policy
            http::get("/policy")
                .description("The policy currently loaded, with its warnings and load errors.")
                .input::<NoArgs>()
                .output::<PolicyView>()
                .handle({
                    let state = state.clone();
                    move |_args: NoArgs, _context| {
                        let state = state.clone();
                        Box::pin(async move { Ok(state.view()) })
                    }
                }),

            // POST /api/plugins/workload-policy/http/reload
            http::post("/reload")
                .description("Re-read the policy file this process was started with.")
                .input::<NoArgs>()
                .output::<ReloadResponse>()
                .handle({
                    let state = state.clone();
                    move |_args: NoArgs, _context| {
                        let state = state.clone();
                        Box::pin(async move { reload(&state) })
                    }
                }),
        ],

        // The enforcement point. The host invokes this before it schedules any
        // inbound work and acts on the answer — a `deny` stops the request and
        // becomes a structured error naming this plugin, the outcome code, and
        // the operator's own reason.
        //
        // It must stay fast. The host applies a hard deadline (2s by default)
        // and treats a hook that misses it as unavailable, which fails the node
        // closed. Everything below this call is a mutex and pure evaluation; no
        // file is read and no lock is held across an await.
        admission: [
            admission::hook()
                .description(
                    "Refuse inbound work this node's local policy does not accept: wrong model, \
                     wrong peer, wrong time of day, too much context, over a rate limit. Never \
                     reads prompt content.",
                )
                .handle({
                    let state = state.clone();
                    move |request: admission::AdmissionRequest, _context| {
                        let state = state.clone();
                        Box::pin(async move { Ok(admit(&state, request)) })
                    }
                }),
        ],

        // Health must stay independent of anything slow. This reads one mutex
        // and formats a line.
        health: {
            let state = state.clone();
            move |_context| {
                let summary = state.health_summary();
                Box::pin(async move { Ok(summary) })
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tdcc_plugin::Plugin;

    use crate::state::OnInvalidPolicy;

    fn plugin() -> SimplePlugin {
        workload_policy_plugin(PolicyState::detached(
            PathBuf::from("policy.toml"),
            OnInvalidPolicy::Deny,
        ))
    }

    #[test]
    fn report_limits_are_clamped_into_the_advertised_range() {
        assert_eq!(clamp_report_limit(None), DEFAULT_REPORT_LIMIT as usize);
        assert_eq!(clamp_report_limit(Some(0)), 0);
        assert_eq!(clamp_report_limit(Some(10_000)), MAX_REPORT_LIMIT as usize);
    }

    #[test]
    fn the_manifest_declares_every_operation_on_both_projections() {
        let manifest = plugin()
            .manifest()
            .expect("declarative plugins have a manifest");

        for name in ["check", "report", "policy", "reload"] {
            assert!(
                manifest
                    .operations
                    .iter()
                    .any(|operation| operation.name == name),
                "missing MCP tool '{name}'"
            );
        }
        for path in ["/check", "/report", "/policy", "/reload"] {
            assert!(
                manifest
                    .http_bindings
                    .iter()
                    .any(|binding| binding.path == path),
                "missing HTTP route '{path}'"
            );
        }
        assert_eq!(
            manifest.capabilities,
            vec![
                "workload-policy.v1",
                admission::ADMISSION_CAPABILITY.to_string().as_str()
            ]
        );
    }

    /// This is the declaration that makes the plugin enforcing. Without both
    /// halves the host either never consults it (no capability) or cannot call
    /// it (no operation), and in the second case fails the node closed.
    #[test]
    fn the_manifest_declares_the_admission_hook_the_host_enforces() {
        let manifest = plugin()
            .manifest()
            .expect("declarative plugins have a manifest");

        assert!(
            manifest
                .capabilities
                .iter()
                .any(|capability| capability == admission::ADMISSION_CAPABILITY),
            "the host resolves the hook through this capability"
        );
        assert!(
            manifest
                .operations
                .iter()
                .any(|operation| operation.name == admission::ADMISSION_OPERATION),
            "the host invokes '{}' on whoever declares the capability",
            admission::ADMISSION_OPERATION
        );
    }

    #[test]
    fn the_plugin_subscribes_to_no_mesh_traffic_and_declares_no_web_ui() {
        let manifest = plugin()
            .manifest()
            .expect("declarative plugins have a manifest");

        // Delivery is allowlist-based, so declaring nothing means receiving
        // nothing.
        assert!(manifest.mesh_channels.is_empty());
        assert!(manifest.mesh_event_subscriptions.is_empty());
        // No config schema either: host-owned settings never reach the plugin
        // process, so a schema here would render controls that change nothing.
        assert!(manifest.config_schema.is_none());
        assert!(manifest.web_ui.is_none());
    }

    #[test]
    fn check_arguments_reject_anything_that_looks_like_content() {
        let rejected = serde_json::from_value::<CheckArgs>(serde_json::json!({
            "model": "qwen/qwen3-8b",
            "prompt": "summarise this document"
        }));

        assert!(
            rejected.is_err(),
            "content fields must be rejected, not silently ignored"
        );

        let accepted = serde_json::from_value::<CheckArgs>(serde_json::json!({
            "model": "qwen/qwen3-8b",
            "context_tokens": 4096
        }))
        .expect("structural fields are accepted");
        assert_eq!(accepted.context_tokens, Some(4096));
    }

    #[test]
    fn a_failed_reload_becomes_an_error_carrying_the_whole_complaint_list() {
        let error = reload_error(ReloadFailure {
            message: "policy.toml did not load".to_string(),
            errors: vec!["rule 'x': unknown action 'dney'".to_string()],
            status: crate::state::PolicyStatus::Loaded,
            source: "policy.toml".to_string(),
        });

        assert_eq!(error.message, "policy.toml did not load");
        let data: serde_json::Value = serde_json::from_str(&error.data_json).expect("data is JSON");
        assert_eq!(data["policy_status"], "loaded");
        assert_eq!(data["errors"][0], "rule 'x': unknown action 'dney'");
    }
}
