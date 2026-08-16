//! What environment a bridged stdio server is launched with.
//!
//! This is the module with the largest blast radius in the plugin, so it is a
//! pure function over two maps and is tested that way.
//!
//! A child process launched from inside `tdcc` inherits `tdcc`'s environment by
//! default, and that environment contains, at minimum, `TDCC_PLUGIN_ENDPOINT` —
//! the node's plugin control endpoint — plus whatever keys the operator
//! exported for their *other* plugins: `TDCC_WEB_SEARCH_BRAVE_API_KEY`,
//! `TDCC_SEMANTIC_CACHE_API_KEY`, `TDCC_EVENT_WEBHOOK_URL`, and so on. Handing
//! all of that to a third-party binary because it happened to be started from
//! the right parent is not a decision anybody made on purpose.
//!
//! So the default is the other way round: the child gets a small platform
//! baseline plus exactly what the server's entry asked for.
//!
//! | Source | Default | Notes |
//! | --- | --- | --- |
//! | Platform baseline ([`BASELINE_UNIX`] / [`BASELINE_WINDOWS`]) | included | `PATH`, `HOME`, temp dirs — without these a great many programs simply do not run |
//! | `env_from = ["NAME"]` | copied from `tdcc`'s environment | how a key reaches a server without being written into the file |
//! | `env = { NAME = "value" }` | literal | wins over everything above |
//! | everything else in `tdcc`'s environment | **dropped** | `inherit_env = true` keeps it |
//!
//! [`crate::config::RESERVED_ENV_PREFIXES`] is removed last, from whatever the
//! rules above produced, so no combination of settings — `inherit_env`
//! included — can hand a bridged server the node's control endpoint.

use std::collections::BTreeMap;

use crate::config::RESERVED_ENV_PREFIXES;

/// Variables a Unix child is given even when nothing asked for them.
///
/// `PATH` and `HOME` because almost nothing runs without them; the locale and
/// `TZ` because their absence changes program output rather than preventing it;
/// `TMPDIR` because a program that cannot find a temp directory writes to the
/// working directory instead.
pub const BASELINE_UNIX: &[&str] = &[
    "HOME", "LANG", "LC_ALL", "LOGNAME", "PATH", "SHELL", "TERM", "TMPDIR", "TZ", "USER",
];

/// Variables a Windows child is given even when nothing asked for them.
///
/// `SystemRoot` is the one that is not optional: a process launched without it
/// cannot initialise Winsock, so anything that opens a socket fails in a way
/// that looks nothing like a missing environment variable.
pub const BASELINE_WINDOWS: &[&str] = &[
    "APPDATA",
    "COMSPEC",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "NUMBER_OF_PROCESSORS",
    "OS",
    "PATH",
    "PATHEXT",
    "PROCESSOR_ARCHITECTURE",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "WINDIR",
];

/// The baseline for the platform this binary was built for.
pub fn baseline() -> &'static [&'static str] {
    if cfg!(windows) {
        BASELINE_WINDOWS
    } else {
        BASELINE_UNIX
    }
}

/// One variable that `env_from` asked for but the `tdcc` process did not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingVariable {
    pub name: String,
}

/// Build the environment for one bridged stdio server.
///
/// `parent` is the environment of the `tdcc` process. `baseline_names` is the
/// platform baseline — passed in rather than read from [`baseline`] so tests
/// can pin the behaviour on either platform.
///
/// Returns the child environment and the `env_from` names that were not found,
/// so the caller can say which variable the operator forgot to export instead
/// of launching a server that will fail with something less obvious.
pub fn child_environment(
    parent: &BTreeMap<String, String>,
    baseline_names: &[&str],
    env_from: &[String],
    env: &BTreeMap<String, String>,
    inherit_env: bool,
) -> (BTreeMap<String, String>, Vec<MissingVariable>) {
    let mut result: BTreeMap<String, String> = BTreeMap::new();

    if inherit_env {
        result.extend(
            parent
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
    } else {
        // On Windows the environment block is case-insensitive, so match the
        // baseline names case-insensitively and keep the parent's own spelling.
        for name in baseline_names {
            if let Some((actual, value)) = lookup(parent, name) {
                result.insert(actual, value);
            }
        }
    }

    let mut missing = Vec::new();
    for name in env_from {
        match lookup(parent, name) {
            Some((_, value)) => {
                result.insert(name.clone(), value);
            }
            None => missing.push(MissingVariable { name: name.clone() }),
        }
    }

    for (name, value) in env {
        result.insert(name.clone(), value.clone());
    }

    // Last, and unconditionally: no path through the rules above may leave the
    // node's control endpoint in a third-party process's environment.
    result.retain(|name, _| {
        !RESERVED_ENV_PREFIXES
            .iter()
            .any(|prefix| starts_with_ignore_case(name, prefix))
    });

    (result, missing)
}

/// Exact match first, then a case-insensitive fallback.
///
/// Windows environment blocks are case-insensitive — `Path`, `PATH`, and
/// `path` are one variable — while Unix blocks are not. Trying the exact
/// spelling first keeps Unix behaviour exact, and the fallback is what makes
/// the same baseline list work on Windows without a second table of spellings.
fn lookup(parent: &BTreeMap<String, String>, name: &str) -> Option<(String, String)> {
    if let Some(value) = parent.get(name) {
        return Some((name.to_string(), value.clone()));
    }
    parent
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(candidate, value)| (candidate.clone(), value.clone()))
}

fn starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn tdcc_environment() -> BTreeMap<String, String> {
        env(&[
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/home/operator"),
            ("TDCC_PLUGIN_ENDPOINT", "/run/tdcc/plugin.sock"),
            ("TDCC_PLUGIN_TRANSPORT", "unix"),
            ("MESH_LLM_PLUGIN_ENDPOINT", "/run/tdcc/plugin.sock"),
            ("TDCC_WEB_SEARCH_BRAVE_API_KEY", "brave-key"),
            ("GITHUB_TOKEN", "github-token"),
            ("AWS_SECRET_ACCESS_KEY", "aws-secret"),
        ])
    }

    #[test]
    fn a_child_gets_the_baseline_and_nothing_else_by_default() {
        let (child, missing) = child_environment(
            &tdcc_environment(),
            BASELINE_UNIX,
            &[],
            &BTreeMap::new(),
            false,
        );

        assert!(missing.is_empty());
        assert_eq!(child.get("PATH").map(String::as_str), Some("/usr/bin:/bin"));
        assert_eq!(
            child.get("HOME").map(String::as_str),
            Some("/home/operator")
        );
        // The keys the operator exported for their other plugins do not travel.
        assert!(!child.contains_key("TDCC_WEB_SEARCH_BRAVE_API_KEY"));
        assert!(!child.contains_key("GITHUB_TOKEN"));
        assert!(!child.contains_key("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn the_plugin_control_endpoint_never_reaches_a_child() {
        for inherit in [false, true] {
            let (child, _) = child_environment(
                &tdcc_environment(),
                BASELINE_UNIX,
                &[],
                &BTreeMap::new(),
                inherit,
            );

            assert!(
                !child.contains_key("TDCC_PLUGIN_ENDPOINT"),
                "inherit_env = {inherit}"
            );
            assert!(
                !child.contains_key("TDCC_PLUGIN_TRANSPORT"),
                "inherit_env = {inherit}"
            );
            assert!(
                !child.contains_key("MESH_LLM_PLUGIN_ENDPOINT"),
                "inherit_env = {inherit}"
            );
        }
    }

    /// The reserved sweep runs after everything else, so it also catches a
    /// value written literally in `env` — which `config` refuses, but this is
    /// the second, independent layer.
    #[test]
    fn a_reserved_name_written_literally_is_still_stripped() {
        let (child, _) = child_environment(
            &tdcc_environment(),
            BASELINE_UNIX,
            &[],
            &env(&[("TDCC_PLUGIN_ENDPOINT", "/tmp/attacker.sock")]),
            false,
        );

        assert!(!child.contains_key("TDCC_PLUGIN_ENDPOINT"));
    }

    #[test]
    fn a_reserved_name_in_a_different_case_is_still_stripped() {
        let (child, _) = child_environment(
            &tdcc_environment(),
            BASELINE_UNIX,
            &[],
            &env(&[("tdcc_plugin_endpoint", "/tmp/attacker.sock")]),
            false,
        );

        assert!(
            child.is_empty()
                || !child
                    .keys()
                    .any(|name| name.to_ascii_uppercase().starts_with("TDCC_PLUGIN_"))
        );
    }

    #[test]
    fn env_from_copies_a_key_out_of_the_tdcc_environment() {
        let (child, missing) = child_environment(
            &tdcc_environment(),
            BASELINE_UNIX,
            &names(&["GITHUB_TOKEN"]),
            &BTreeMap::new(),
            false,
        );

        assert!(missing.is_empty());
        assert_eq!(
            child.get("GITHUB_TOKEN").map(String::as_str),
            Some("github-token")
        );
        // Only the one that was asked for.
        assert!(!child.contains_key("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn an_env_from_name_the_operator_forgot_to_export_is_reported_not_guessed() {
        let (child, missing) = child_environment(
            &tdcc_environment(),
            BASELINE_UNIX,
            &names(&["NOTES_TOKEN"]),
            &BTreeMap::new(),
            false,
        );

        assert!(!child.contains_key("NOTES_TOKEN"));
        assert_eq!(
            missing,
            vec![MissingVariable {
                name: "NOTES_TOKEN".to_string()
            }]
        );
    }

    #[test]
    fn a_literal_env_entry_wins_over_the_baseline_and_over_env_from() {
        let (child, _) = child_environment(
            &tdcc_environment(),
            BASELINE_UNIX,
            &names(&["GITHUB_TOKEN"]),
            &env(&[("PATH", "/opt/sandbox/bin"), ("GITHUB_TOKEN", "override")]),
            false,
        );

        assert_eq!(
            child.get("PATH").map(String::as_str),
            Some("/opt/sandbox/bin")
        );
        assert_eq!(
            child.get("GITHUB_TOKEN").map(String::as_str),
            Some("override")
        );
    }

    #[test]
    fn inherit_env_hands_over_everything_that_is_not_reserved() {
        let (child, _) = child_environment(
            &tdcc_environment(),
            BASELINE_UNIX,
            &[],
            &BTreeMap::new(),
            true,
        );

        assert!(child.contains_key("TDCC_WEB_SEARCH_BRAVE_API_KEY"));
        assert!(child.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert!(!child.contains_key("TDCC_PLUGIN_ENDPOINT"));
    }

    #[test]
    fn the_windows_baseline_carries_systemroot_because_sockets_need_it() {
        assert!(
            BASELINE_WINDOWS
                .iter()
                .any(|name| name.eq_ignore_ascii_case("SystemRoot"))
        );
        let parent = env(&[
            ("SystemRoot", "C:\\Windows"),
            ("Path", "C:\\Windows\\System32"),
        ]);

        let (child, _) = child_environment(&parent, BASELINE_WINDOWS, &[], &BTreeMap::new(), false);

        // Matched case-insensitively, and the parent's own spelling is kept.
        assert_eq!(
            child.get("SystemRoot").map(String::as_str),
            Some("C:\\Windows")
        );
        assert_eq!(
            child.get("Path").map(String::as_str),
            Some("C:\\Windows\\System32")
        );
    }

    #[test]
    fn the_baseline_for_this_build_is_the_platforms_own() {
        let expected = if cfg!(windows) {
            BASELINE_WINDOWS
        } else {
            BASELINE_UNIX
        };
        assert_eq!(baseline(), expected);
    }
}
