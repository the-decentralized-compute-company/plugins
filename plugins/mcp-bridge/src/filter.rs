//! Which of an upstream server's tools this node is willing to expose.
//!
//! A filesystem MCP server publishes read *and* write tools in one list. An
//! operator who wants three of a server's forty tools should not have to fork
//! the server, so every server in the list carries an allowlist and a denylist,
//! and both are matched against the **upstream** name — the one written in that
//! server's own documentation, not the bridged one this plugin invents.
//!
//! The rules, in the order they are applied:
//!
//! 1. A non-empty `allow_tools` means *only* matching tools are candidates. An
//!    empty one means every tool is a candidate.
//! 2. `deny_tools` removes matches from whatever is left. **Deny wins**, so
//!    adding a pattern to the denylist can never be undone by the allowlist.
//!
//! Matching is case-sensitive, because MCP tool names are identifiers rather
//! than prose, and `*` is the only metacharacter.

use schemars::JsonSchema;
use serde::Serialize;

/// Why a tool the upstream published is or is not exposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FilterOutcome {
    /// No allowlist was configured and no denylist pattern matched.
    Allowed,
    /// An allowlist was configured and this tool matched it.
    AllowedByPattern(String),
    /// An allowlist was configured and this tool matched none of it.
    NotOnAllowlist,
    /// A denylist pattern matched. Wins over anything above.
    DeniedByPattern(String),
}

impl FilterOutcome {
    pub fn is_exposed(&self) -> bool {
        matches!(self, Self::Allowed | Self::AllowedByPattern(_))
    }

    /// A sentence naming the setting an operator would change.
    pub fn reason(&self) -> String {
        match self {
            Self::Allowed => "no allowlist configured and no denylist match".to_string(),
            Self::AllowedByPattern(pattern) => format!("matched allow_tools pattern '{pattern}'"),
            Self::NotOnAllowlist => {
                "allow_tools is set for this server and no pattern matched".to_string()
            }
            Self::DeniedByPattern(pattern) => format!("matched deny_tools pattern '{pattern}'"),
        }
    }
}

/// Decide whether one upstream tool name is exposed.
pub fn decide(name: &str, allow: &[String], deny: &[String]) -> FilterOutcome {
    // Deny is evaluated first so its precedence is a property of the code
    // rather than of the order the caller happens to read the result in.
    if let Some(pattern) = deny.iter().find(|pattern| glob_match(pattern, name)) {
        return FilterOutcome::DeniedByPattern(pattern.clone());
    }
    if allow.is_empty() {
        return FilterOutcome::Allowed;
    }
    match allow.iter().find(|pattern| glob_match(pattern, name)) {
        Some(pattern) => FilterOutcome::AllowedByPattern(pattern.clone()),
        None => FilterOutcome::NotOnAllowlist,
    }
}

/// `*` matches any run of characters including none; every other character is
/// literal. Linear in the pattern and the value, with no backtracking blowup,
/// because these run once per tool at startup on operator-supplied input.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();

    let mut pattern_index = 0usize;
    let mut value_index = 0usize;
    let mut star_index: Option<usize> = None;
    let mut match_index = 0usize;

    while value_index < value.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star_index = Some(pattern_index);
            match_index = value_index;
            pattern_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == value[value_index] {
            pattern_index += 1;
            value_index += 1;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            match_index += 1;
            value_index = match_index;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn no_lists_at_all_exposes_everything() {
        assert_eq!(decide("read_file", &[], &[]), FilterOutcome::Allowed);
        assert!(decide("read_file", &[], &[]).is_exposed());
    }

    #[test]
    fn an_allowlist_exposes_three_tools_out_of_forty() {
        let allow = patterns(&["read_file", "list_directory", "search_files"]);

        for exposed in ["read_file", "list_directory", "search_files"] {
            assert!(
                decide(exposed, &allow, &[]).is_exposed(),
                "{exposed} should be exposed"
            );
        }
        for hidden in ["write_file", "move_file", "edit_file", "create_directory"] {
            assert_eq!(
                decide(hidden, &allow, &[]),
                FilterOutcome::NotOnAllowlist,
                "{hidden} should be hidden"
            );
        }
    }

    #[test]
    fn deny_wins_over_allow() {
        let allow = patterns(&["*"]);
        let deny = patterns(&["write_*"]);

        assert!(decide("read_file", &allow, &deny).is_exposed());
        assert_eq!(
            decide("write_file", &allow, &deny),
            FilterOutcome::DeniedByPattern("write_*".to_string())
        );
    }

    #[test]
    fn an_explicit_allow_entry_cannot_re_enable_a_denied_tool() {
        let allow = patterns(&["delete_everything"]);
        let deny = patterns(&["delete_everything"]);

        assert!(!decide("delete_everything", &allow, &deny).is_exposed());
    }

    #[test]
    fn the_outcome_names_the_setting_an_operator_would_change() {
        let denied = decide("write_file", &[], &patterns(&["write_*"]));
        assert!(
            denied.reason().contains("deny_tools"),
            "{}",
            denied.reason()
        );

        let unlisted = decide("write_file", &patterns(&["read_*"]), &[]);
        assert!(
            unlisted.reason().contains("allow_tools"),
            "{}",
            unlisted.reason()
        );
    }

    #[test]
    fn matching_is_case_sensitive_because_tool_names_are_identifiers() {
        assert!(!decide("ReadFile", &patterns(&["readfile"]), &[]).is_exposed());
        assert!(decide("ReadFile", &patterns(&["ReadFile"]), &[]).is_exposed());
    }

    #[test]
    fn glob_stars_match_any_run_including_none() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("read_*", "read_"));
        assert!(glob_match("read_*", "read_file"));
        assert!(glob_match("*_file", "read_file"));
        assert!(glob_match("a*b*c", "abc"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(!glob_match("read_*", "write_file"));
    }

    #[test]
    fn a_literal_pattern_is_an_exact_match_not_a_prefix() {
        assert!(glob_match("read", "read"));
        assert!(!glob_match("read", "read_file"));
        assert!(!glob_match("read_file", "read"));
    }

    /// A pathological pattern must not take exponential time: this is operator
    /// input, but it runs inside the node's startup path.
    #[test]
    fn a_pathological_pattern_still_terminates_quickly() {
        let pattern = "*".repeat(64) + "z";
        let value = "a".repeat(512);

        assert!(!glob_match(&pattern, &value));
    }
}
