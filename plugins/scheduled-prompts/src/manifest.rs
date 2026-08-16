//! The whole contribution surface of `scheduled-prompts`, in one declaration.
//!
//! Six MCP tools, the same six operations mounted over HTTP, one capability, a
//! health hook, and the lifecycle hook that starts the scheduler. The host
//! synthesizes `tools/list`, `tools/call`, the JSON Schema for every argument,
//! and the request validation that runs before a handler is entered; this
//! plugin opens no socket and speaks no MCP.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then the
//! lifecycle hooks.
//!
//! # Four absences, all deliberate
//!
//! * **No tool that creates, edits, or deletes a job.** That is the whole
//!   design, not an omission. See README.md > "Why a model cannot create a
//!   job".
//! * **No `config_schema`.** `[plugin.settings]` never reaches a plugin
//!   process, so a schema here would draw console controls that could not move
//!   a single job. Configuration comes from `[[plugin]].args`, the environment,
//!   and the jobs file — see [`crate::config`].
//! * **No `mesh` channels and no `events`.** A schedule is this machine's own
//!   business. Declaring nothing means the host's allowlist guarantees nothing
//!   arrives.
//! * **No `inference`.** This plugin *calls* an OpenAI-compatible endpoint; it
//!   does not attach one to the mesh.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tdcc_plugin::{
    PluginMetadata, SimplePlugin, capability, http, mcp, plugin, plugin_server_info,
};

use crate::config::{PLUGIN_NAME, PLUGIN_VERSION};
use crate::scheduler::Scheduler;

/// Arguments for the tools that take none.
///
/// `deny_unknown_fields` throughout this module is worth the line: it
/// guarantees there is nowhere for prompt content to land in a scheduling call,
/// which turns a documented boundary into a type-system one.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

/// Arguments for the tools that act on exactly one job.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobArgs {
    /// The id of the job to act on, exactly as `list` reports it. There is no
    /// "all jobs" form: a tool that spends GPU time names its target.
    pub job_id: String,
}

/// Arguments for `history`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryArgs {
    /// Restrict the answer to one job. Omit it for every declared job.
    #[serde(default)]
    pub job_id: Option<String>,
    /// How many runs to return per job, newest first. Clamped to 1-200;
    /// defaults to 20. Only the last `history_per_job` runs exist on disk.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Arguments for `pause`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PauseArgs {
    /// The id of the job to stop running, exactly as `list` reports it.
    pub job_id: String,
    /// A short note about why, kept with the pause and shown by `list`. It is
    /// not written to disk, because the pause itself is not.
    #[serde(default)]
    pub note: Option<String>,
}

pub fn scheduled_prompts_plugin(scheduler: Arc<Scheduler>) -> SimplePlugin {
    // One clone per handler: the handlers are `Fn`, so each owns its reference
    // to the shared scheduler rather than borrowing one.
    let for_list = Arc::clone(&scheduler);
    let for_status = Arc::clone(&scheduler);
    let for_history = Arc::clone(&scheduler);
    let for_run_now = Arc::clone(&scheduler);
    let for_pause = Arc::clone(&scheduler);
    let for_resume = Arc::clone(&scheduler);
    let for_http_list = Arc::clone(&scheduler);
    let for_http_status = Arc::clone(&scheduler);
    let for_http_history = Arc::clone(&scheduler);
    let for_http_run_now = Arc::clone(&scheduler);
    let for_http_pause = Arc::clone(&scheduler);
    let for_http_resume = Arc::clone(&scheduler);
    let for_health = Arc::clone(&scheduler);
    let for_init = scheduler;

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "Scheduled prompts",
                "Runs operator-declared prompts on a schedule and delivers the result to a file \
                 or a webhook",
                None::<String>,
            ),
        ),

        // A stable name for "this node runs prompts on a schedule", so a caller
        // can depend on the contract rather than on this plugin's id.
        provides: [capability("scheduled-prompts.v1")],

        mcp: [
            // Projected as `scheduled-prompts.list` on the host MCP endpoint.
            mcp::tool("list")
                .title("List scheduled jobs")
                .description(
                    "List every job the operator declared: its schedule, the model it uses, where \
                     its output goes, whether it is enabled or paused, when it is next due, and a \
                     summary of its last run. Reads local state only and always answers. Jobs \
                     come from the operator's jobs file — no tool here can create, edit, or \
                     delete one.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let scheduler = Arc::clone(&for_list);
                    Box::pin(async move { Ok(scheduler.list()) })
                }),

            mcp::tool("status")
                .title("Scheduler status")
                .description(
                    "Diagnostics for the scheduler itself: which jobs file it read and whether it \
                     loaded, the endpoint prompts are sent to, the timezone, the concurrency cap \
                     and how many slots are free, where run history and output are written, and \
                     how many ticks have run. Touches no network and always answers, including \
                     when every other tool is failing.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let scheduler = Arc::clone(&for_status);
                    Box::pin(async move { Ok(scheduler.status()) })
                }),

            mcp::tool("history")
                .title("Recent runs")
                .description(
                    "Recent runs for one job or for every job, newest first, with the outcome, \
                     the duration, token counts, and the first line of any error. Skips are not \
                     listed one by one — they cost nothing and would crowd out real runs — so \
                     they are counted by reason in `skips_by_reason` instead. Model output is \
                     never stored here; it went to the job's sink.",
                )
                .input::<HistoryArgs>()
                .handle(move |args: HistoryArgs, _context| {
                    let scheduler = Arc::clone(&for_history);
                    Box::pin(async move {
                        Ok(scheduler.history(args.job_id.as_deref(), args.limit)?)
                    })
                }),

            mcp::tool("run_now")
                .title("Run a job now")
                .description(
                    "Run one already-declared job immediately, outside its schedule. This spends \
                     GPU time on this machine, so it names one job and has no \"all\" form. It \
                     still honours the guards that protect the node: a job already running, a job \
                     backing off after failures, a full concurrency cap, and — unless the jobs \
                     file opted that job in — a job outside its allowed hours are all refused, \
                     with the reason. Waits up to 45 seconds; a longer run continues in the \
                     background and its outcome appears in `history`.",
                )
                .input::<JobArgs>()
                .handle(move |args: JobArgs, _context| {
                    let scheduler = Arc::clone(&for_run_now);
                    Box::pin(async move { Ok(scheduler.run_now(&args.job_id).await?) })
                }),

            mcp::tool("pause")
                .title("Pause a job")
                .description(
                    "Stop a job running from its next occurrence onwards. A run already in flight \
                     is not cancelled. The pause is deliberately not written to disk: restarting \
                     the node clears it, because the jobs file is the only durable statement of \
                     what this machine has agreed to run.",
                )
                .input::<PauseArgs>()
                .handle(move |args: PauseArgs, _context| {
                    let scheduler = Arc::clone(&for_pause);
                    Box::pin(async move {
                        Ok(scheduler.pause(&args.job_id, args.note.as_deref())?)
                    })
                }),

            mcp::tool("resume")
                .title("Resume a paused job")
                .description(
                    "Let a paused job, or one that quarantined itself after repeated failures, \
                     run again from its next scheduled occurrence. Occurrences missed while it \
                     was paused are not replayed. This cannot start a job the jobs file disabled \
                     — only the operator can do that, by editing the file.",
                )
                .input::<JobArgs>()
                .handle(move |args: JobArgs, _context| {
                    let scheduler = Arc::clone(&for_resume);
                    Box::pin(async move { Ok(scheduler.resume(&args.job_id)?) })
                }),
        ],

        // The same six operations, mounted by the host under
        // /api/plugins/scheduled-prompts/http/…. One implementation, one set of
        // caveats; the two projections cannot drift.
        http: [
            http::get("/jobs")
                .description("List every declared job and its state.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let scheduler = Arc::clone(&for_http_list);
                    Box::pin(async move { Ok(scheduler.list()) })
                }),

            http::get("/status")
                .description("Scheduler diagnostics. No network.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let scheduler = Arc::clone(&for_http_status);
                    Box::pin(async move { Ok(scheduler.status()) })
                }),

            http::get("/history")
                .description("Recent runs, newest first.")
                .input::<HistoryArgs>()
                .handle(move |args: HistoryArgs, _context| {
                    let scheduler = Arc::clone(&for_http_history);
                    Box::pin(async move {
                        Ok(scheduler.history(args.job_id.as_deref(), args.limit)?)
                    })
                }),

            // POST, not GET: these three change what the machine does.
            http::post("/run")
                .description("Run one declared job now, subject to the same guards.")
                .input::<JobArgs>()
                .handle(move |args: JobArgs, _context| {
                    let scheduler = Arc::clone(&for_http_run_now);
                    Box::pin(async move { Ok(scheduler.run_now(&args.job_id).await?) })
                }),

            http::post("/pause")
                .description("Pause one job until it is resumed or the node restarts.")
                .input::<PauseArgs>()
                .handle(move |args: PauseArgs, _context| {
                    let scheduler = Arc::clone(&for_http_pause);
                    Box::pin(async move {
                        Ok(scheduler.pause(&args.job_id, args.note.as_deref())?)
                    })
                }),

            http::post("/resume")
                .description("Resume one paused or quarantined job.")
                .input::<JobArgs>()
                .handle(move |args: JobArgs, _context| {
                    let scheduler = Arc::clone(&for_http_resume);
                    Box::pin(async move { Ok(scheduler.resume(&args.job_id)?) })
                }),
        ],

        // Reads two atomics and a slice length, so it stays responsive no
        // matter how long a run is taking.
        health: move |_context| {
            let line = for_health.health_line();
            Box::pin(async move { Ok(line) })
        },

        // The host may re-run this if the control session is re-established;
        // the loop slot makes sure only one scheduler ever exists. A jobs file
        // that did not load leaves the loop unstarted, so a broken file means
        // nothing runs rather than some of it.
        on_initialized: move |_context| {
            let scheduler = Arc::clone(&for_init);
            Box::pin(async move {
                if scheduler.can_schedule() && scheduler.claim_loop_slot() {
                    scheduler.spawn_loop();
                }
                Ok(())
            })
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdcc_plugin::Plugin;

    use crate::config::{Config, EnvMap};
    use crate::history::Store;
    use crate::jobs::JobsFile;
    use crate::scheduler::JobsSource;

    fn manifest() -> tdcc_plugin::proto::PluginManifest {
        let dir = std::env::temp_dir().join(format!(
            "tdcc-scheduled-prompts-manifest-{}",
            std::process::id()
        ));
        let config = Config::parse(
            &["--state-dir".to_string(), dir.display().to_string()],
            &EnvMap::from([("HOME".to_string(), "/home/tester".to_string())]),
        )
        .expect("config parses");
        let scheduler = Scheduler::new(
            config,
            JobsSource {
                file: JobsFile::empty(),
                error: None,
                present: false,
            },
            Store::new(&dir),
        )
        .expect("scheduler builds");

        scheduled_prompts_plugin(Arc::new(scheduler))
            .manifest()
            .expect("declarative plugins have a manifest")
    }

    #[test]
    fn every_tool_is_declared_with_a_description_a_model_can_act_on() {
        let manifest = manifest();

        for name in ["list", "status", "history", "run_now", "pause", "resume"] {
            let operation = manifest
                .operations
                .iter()
                .find(|operation| operation.name == name)
                .unwrap_or_else(|| panic!("`{name}` is declared"));
            assert!(
                operation.description.len() > 80,
                "`{name}` needs a description a model can act on"
            );
            let schema = &operation.input_schema_json;
            assert!(schema.contains("\"type\":\"object\""), "{name}: {schema}");
            assert!(
                schema.contains("\"additionalProperties\":false"),
                "{name} must leave nowhere for prompt content to land: {schema}"
            );
        }

        // The four tools that take arguments advertise them; `list` and
        // `status` take none, and their schema is an empty object.
        for name in ["history", "run_now", "pause", "resume"] {
            let operation = manifest
                .operations
                .iter()
                .find(|operation| operation.name == name)
                .expect("declared");
            assert!(
                operation.input_schema_json.contains("\"properties\""),
                "{name}: {}",
                operation.input_schema_json
            );
        }
    }

    #[test]
    fn no_tool_can_create_edit_or_delete_a_job() {
        let manifest = manifest();

        let names: Vec<&str> = manifest
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect();
        for forbidden in [
            "create",
            "add",
            "new",
            "define",
            "schedule",
            "edit",
            "update",
            "delete",
            "remove",
            "set_prompt",
        ] {
            assert!(
                !names.iter().any(|name| name.contains(forbidden)),
                "`{forbidden}` appears in the tool surface: {names:?}. The schedule belongs to \
                 the operator; see README.md > Why a model cannot create a job."
            );
        }
    }

    #[test]
    fn argument_schemas_carry_the_doc_comments_and_reject_unknown_fields() {
        let manifest = manifest();

        let run_now = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "run_now")
            .expect("run_now is declared");
        let schema = &run_now.input_schema_json;
        assert!(schema.contains("exactly as `list` reports it"), "{schema}");
        assert!(schema.contains("\"required\""), "{schema}");
        assert!(
            schema.contains("additionalProperties\":false"),
            "an argument struct must leave nowhere for prompt content to land: {schema}"
        );
    }

    #[test]
    fn the_http_routes_mirror_the_tools_and_use_post_for_the_ones_that_act() {
        let manifest = manifest();

        let get = tdcc_plugin::proto::HttpMethod::Get as i32;
        let post = tdcc_plugin::proto::HttpMethod::Post as i32;
        let mut by_path: Vec<(&str, i32)> = manifest
            .http_bindings
            .iter()
            .map(|binding| (binding.path.as_str(), binding.method))
            .collect();
        by_path.sort();

        assert_eq!(
            by_path,
            vec![
                ("/history", get),
                ("/jobs", get),
                ("/pause", post),
                ("/resume", post),
                ("/run", post),
                ("/status", get),
            ],
            "reads are GET; the three that change what the machine does are POST"
        );
    }

    #[test]
    fn nothing_is_declared_that_this_plugin_does_not_use() {
        let manifest = manifest();

        // A schedule is this machine's own business: no channel, no events.
        assert!(manifest.mesh_channels.is_empty());
        assert!(manifest.mesh_event_subscriptions.is_empty());
        // It calls an endpoint; it does not attach one.
        assert!(manifest.endpoints.is_empty());
        // Settings never reach the process, and there is no bundle to serve.
        assert!(manifest.config_schema.is_none());
        assert!(manifest.web_ui.is_none());
        assert_eq!(
            manifest.capabilities,
            vec!["scheduled-prompts.v1".to_string()]
        );
    }
}
