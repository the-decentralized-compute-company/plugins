//! The whole contribution surface of `notes-console` in one declaration.
//!
//! One `plugin!` invocation carries the metadata, the capability, the config
//! schema, the web UI projection, the MCP tool, and the HTTP bindings. The
//! host reads this manifest during initialization and projects it; the plugin
//! never registers a route or an MCP method itself.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then
//! lifecycle hooks. Omitting a field is fine, reordering is not.

use schemars::JsonSchema;
use serde::Deserialize;
use tdcc_plugin::{
    PluginMetadata, SimplePlugin, capability, config_integer, config_schema, config_setting,
    constraint_range, http, mcp, plugin, plugin_server_info, proto, web_ui, web_ui_bundle,
    web_ui_config_section, web_ui_page,
};

use crate::notes::NoteStore;

/// Default page size when a caller does not ask for one.
const DEFAULT_LIMIT: u32 = 25;
/// Upper bound on a single response, mirrored by the `max_notes` constraint.
const MAX_LIMIT: u32 = 200;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListArgs {
    /// Maximum number of notes to return. Query strings are parsed into typed
    /// arguments by the host, so `GET http/notes?limit=10` lands here as 10.
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddArgs {
    /// Note body. Required; the host rejects a request without it before the
    /// handler runs.
    text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoArgs {}

fn clamp_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize
}

pub fn notes_console_plugin(store: NoteStore) -> SimplePlugin {
    let store_for_status = store.clone();
    let store_for_list = store.clone();
    let store_for_add = store;

    plugin! {
        metadata: PluginMetadata::new(
            "notes-console",
            "0.1.0",
            plugin_server_info(
                "notes-console",
                "0.1.0",
                "Notes console",
                "Example plugin with a host-projected console page and HTTP notes API",
                None::<String>,
            ),
        ),

        // A stable contract another component could depend on by name instead
        // of by plugin id.
        provides: [capability("notes-console.v1")],

        // Operator-facing settings. The console renders these with its own
        // schema controls; the values live in the host's `[plugin.settings]`
        // and are read back by the bundle, not by this process.
        config: [config_schema("notes-console")
            .setting(
                config_setting("max_notes", config_integer())
                    .default_value(&DEFAULT_LIMIT)
                    .constraint(constraint_range(Some("1"), Some("200")))
                    .apply_mode(proto::PluginConfigApplyMode::DynamicValidationOnly)
                    .restart_scope(proto::PluginConfigRestartScope::None)
                    .description("How many notes the console page requests at a time.")
                    .label("Notes per page")
                    .help("Read by the plugin's console bundle; no restart required.")
                    .category("notes-console-display", "Display", "Console page display", 10)
                    .order(10)
                    .unit("notes")
                    .control_hint("number"),
            )],

        // v1 allows exactly one bundle root, and every page and config section
        // must reference it by id and name an entry script inside it.
        web_ui: [web_ui()
            .bundle(web_ui_bundle("main", "bundle"))
            .page(
                web_ui_page("notes", "Notes", "notes", "register-mesh-plugin-ui.js")
                    .bundle_id("main"),
            )
            .config_section(
                web_ui_config_section("notes-actions", "Notes console", "register-mesh-plugin-ui.js")
                    .parent_tab("integrations")
                    .bundle_id("main"),
            )],

        mcp: [
            // Projected as `notes-console.status` on the host MCP endpoint and
            // callable at POST /api/plugins/notes-console/tools/status.
            mcp::tool("status")
                .description("Report how many notes this plugin currently holds.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let store = store_for_status.clone();
                    Box::pin(async move {
                        Ok(serde_json::json!({
                            "capability": "notes-console.v1",
                            "notes": store.len(),
                        }))
                    })
                }),
        ],

        http: [
            // Mounted by the host at GET /api/plugins/notes-console/http/notes.
            http::get("/notes")
                .description("List recent notes, newest first.")
                .input::<ListArgs>()
                .handle(move |args: ListArgs, _context| {
                    let store = store_for_list.clone();
                    Box::pin(async move {
                        let limit = clamp_limit(args.limit);
                        Ok(serde_json::json!({
                            "limit": limit,
                            "total": store.len(),
                            "notes": store.recent(limit),
                        }))
                    })
                }),

            // Mounted at POST /api/plugins/notes-console/http/notes.
            http::post("/notes")
                .description("Append a note.")
                .input::<AddArgs>()
                .handle(move |args: AddArgs, _context| {
                    let store = store_for_add.clone();
                    Box::pin(async move {
                        let note = store.add(args.text);
                        Ok(serde_json::json!({ "note": note, "total": store.len() }))
                    })
                }),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdcc_plugin::Plugin;

    #[test]
    fn limit_is_clamped_into_the_advertised_range() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT as usize);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(10_000)), MAX_LIMIT as usize);
    }

    #[test]
    fn manifest_declares_one_bundle_root_and_matching_bundle_ids() {
        let plugin = notes_console_plugin(NoteStore::new());
        let manifest = plugin
            .manifest()
            .expect("declarative plugins have a manifest");
        let web_ui = manifest.web_ui.expect("web_ui is declared");

        let [bundle] = web_ui.bundles.as_slice() else {
            panic!("v1 permits exactly one bundle root");
        };
        assert_eq!(bundle.root_path, "bundle");
        assert!(web_ui.pages.iter().all(|page| page.bundle_id == bundle.id));
        assert!(
            web_ui
                .config_sections
                .iter()
                .all(|section| section.bundle_id == bundle.id)
        );
    }
}
