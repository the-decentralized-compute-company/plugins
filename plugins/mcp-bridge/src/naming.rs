//! Turning an upstream tool name into a TDCC tool name.
//!
//! This is the crux of the plugin. Two MCP servers written by two strangers
//! will both call something `search`, `read`, or `query`, and a node that
//! bridges both has to keep them apart — and has to let a person reading a
//! transcript say which server answered.
//!
//! The scheme is one rule: every bridged tool is
//! `<alias>__<upstream name>`, with a **double** underscore, where `<alias>` is
//! the name the operator gave that server in the server list.
//!
//! Three properties fall out of it, and each is pinned by a test below:
//!
//! * **An alias may not contain `__`**, so splitting a bridged name at its
//!   *first* `__` always recovers the alias exactly. There is no ambiguity
//!   between alias `a_b` + tool `c` and alias `a` + tool `b_c`.
//! * **This plugin's own management tools contain no `__`**, so a bridged tool
//!   can never shadow `status`, `tools`, or `reconnect`.
//! * **The alias is the operator's word, not the upstream's.** An upstream
//!   server cannot choose, influence, or collide with another server's prefix,
//!   because it never sees it.
//!
//! On the host MCP endpoint the host adds its own plugin namespace on top, so
//! the fully qualified name a model sees is `mcp-bridge.<alias>__<tool>`.

use std::collections::BTreeSet;

/// Separator between the operator's alias and the upstream tool name.
pub const SEPARATOR: &str = "__";

/// Longest alias an operator may write. Short enough that the composed name
/// stays readable in a tool list.
pub const MAX_ALIAS_LENGTH: usize = 32;

/// Longest upstream name kept after sanitizing. Anything longer is truncated
/// and the truncation is reported by the `tools` tool.
pub const MAX_LOCAL_LENGTH: usize = 48;

/// How many numeric suffixes are tried before a colliding tool is dropped.
const MAX_DISAMBIGUATION: u32 = 99;

/// Why a discovered upstream tool did not end up projected under its own name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameNote {
    /// The upstream name survived unchanged.
    Verbatim,
    /// Characters outside `[A-Za-z0-9_-]` were replaced with `_`.
    Sanitized,
    /// The name was longer than [`MAX_LOCAL_LENGTH`] and was cut.
    Truncated,
    /// Sanitizing or truncating collided with another tool on the same server,
    /// so a numeric suffix was appended.
    Disambiguated,
}

impl NameNote {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Verbatim => "verbatim",
            Self::Sanitized => "sanitized",
            Self::Truncated => "truncated",
            Self::Disambiguated => "disambiguated",
        }
    }
}

/// One upstream tool name and the TDCC tool name it is projected under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignedName {
    /// Exactly what the upstream server called it. Calls go out under this
    /// name, never under the bridged one.
    pub upstream: String,
    /// `<alias>__<sanitized upstream name>`.
    pub bridged: String,
    /// What, if anything, had to change.
    pub notes: Vec<NameNote>,
}

/// Validate an operator-chosen server alias.
///
/// Deliberately narrow: lowercase ASCII, digits, and single underscores
/// between them. The ban on consecutive underscores is what makes the `__`
/// separator unambiguous, and the ban on a trailing underscore keeps
/// `alias___tool` from ever being produced.
pub fn validate_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty() {
        return Err("a server alias may not be empty".to_string());
    }
    if alias.len() > MAX_ALIAS_LENGTH {
        return Err(format!(
            "server alias '{alias}' is {} characters; the limit is {MAX_ALIAS_LENGTH}",
            alias.len()
        ));
    }
    let mut previous_underscore = false;
    for (index, character) in alias.chars().enumerate() {
        let ok = match character {
            'a'..='z' => true,
            '0'..='9' => index > 0,
            '_' => index > 0 && !previous_underscore,
            _ => false,
        };
        if !ok {
            return Err(format!(
                "server alias '{alias}' is not usable as a tool-name prefix. Use lowercase \
                 letters, digits, and single underscores, starting with a letter — for example \
                 'files' or 'github_ro'."
            ));
        }
        previous_underscore = character == '_';
    }
    if previous_underscore {
        return Err(format!(
            "server alias '{alias}' may not end with an underscore: the bridge joins the alias \
             and the tool name with '{SEPARATOR}'."
        ));
    }
    Ok(())
}

/// Replace every character an MCP tool name should not carry with `_`.
///
/// The upstream owns its own naming, so this changes as little as possible:
/// `[A-Za-z0-9_-]` passes through untouched and everything else becomes `_`.
/// Nothing is lower-cased, nothing is collapsed, nothing is trimmed — a
/// faithful name is worth more than a tidy one, and every change is reported.
fn sanitize(upstream: &str) -> (String, Vec<NameNote>) {
    let mut notes = Vec::new();
    let mapped: String = upstream
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if mapped != upstream {
        notes.push(NameNote::Sanitized);
    }

    let mut local = mapped;
    if local.chars().count() > MAX_LOCAL_LENGTH {
        local = local.chars().take(MAX_LOCAL_LENGTH).collect();
        notes.push(NameNote::Truncated);
    }
    // An upstream name of "", "///", or "---" leaves nothing that reads as a
    // name. Falling back keeps the tool reachable, and the real name is still
    // in `tools` and in the call that goes out.
    if !local
        .chars()
        .any(|character| character.is_ascii_alphanumeric())
    {
        local = "tool".to_string();
        if !notes.contains(&NameNote::Sanitized) {
            notes.push(NameNote::Sanitized);
        }
    }
    (local, notes)
}

/// Assign a bridged name to every tool one upstream server published.
///
/// Input order does not matter: the names are assigned in sorted upstream
/// order so that the same `tools/list` response always produces the same
/// mapping, even if the upstream reorders its list between restarts.
///
/// A tool whose name cannot be made unique after [`MAX_DISAMBIGUATION`]
/// attempts is returned in the second half of the pair rather than silently
/// dropped, so the caller can report it.
pub fn assign_names(alias: &str, upstream_names: &[String]) -> (Vec<AssignedName>, Vec<String>) {
    let mut sorted: Vec<&String> = upstream_names.iter().collect();
    sorted.sort();
    sorted.dedup();

    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut assigned = Vec::new();
    let mut dropped = Vec::new();

    for upstream in sorted {
        let (local, mut notes) = sanitize(upstream);
        let mut candidate = format!("{alias}{SEPARATOR}{local}");
        if used.contains(&candidate) {
            let mut resolved = None;
            for suffix in 2..=MAX_DISAMBIGUATION {
                let next = format!("{alias}{SEPARATOR}{local}_{suffix}");
                if !used.contains(&next) {
                    resolved = Some(next);
                    break;
                }
            }
            match resolved {
                Some(next) => {
                    candidate = next;
                    notes.push(NameNote::Disambiguated);
                }
                None => {
                    dropped.push(upstream.clone());
                    continue;
                }
            }
        }
        if notes.is_empty() {
            notes.push(NameNote::Verbatim);
        }
        used.insert(candidate.clone());
        assigned.push(AssignedName {
            upstream: upstream.clone(),
            bridged: candidate,
            notes,
        });
    }

    (assigned, dropped)
}

/// Recover the alias from a bridged tool name.
///
/// Splits at the **first** `__`, which is exact because [`validate_alias`]
/// rejects an alias containing one.
pub fn split_bridged_name(bridged: &str) -> Option<(&str, &str)> {
    let (alias, local) = bridged.split_once(SEPARATOR)?;
    if alias.is_empty() || local.is_empty() {
        return None;
    }
    Some((alias, local))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn a_plain_alias_and_a_plain_tool_compose_verbatim() {
        let (assigned, dropped) = assign_names("files", &names(&["read_file"]));

        assert!(dropped.is_empty());
        assert_eq!(assigned[0].bridged, "files__read_file");
        assert_eq!(assigned[0].upstream, "read_file");
        assert_eq!(assigned[0].notes, vec![NameNote::Verbatim]);
    }

    #[test]
    fn two_servers_publishing_the_same_tool_do_not_collide() {
        let (files, _) = assign_names("files", &names(&["search"]));
        let (github, _) = assign_names("github", &names(&["search"]));

        assert_eq!(files[0].bridged, "files__search");
        assert_eq!(github[0].bridged, "github__search");
        assert_ne!(files[0].bridged, github[0].bridged);
    }

    /// The property the whole scheme rests on: one `__` in the alias would make
    /// this ambiguous, so `validate_alias` refuses one.
    #[test]
    fn the_alias_is_recoverable_from_a_bridged_name_even_with_underscores_everywhere() {
        validate_alias("a_b").expect("single underscores are fine");
        let (assigned, _) = assign_names("a_b", &names(&["c_d"]));
        assert_eq!(
            split_bridged_name(&assigned[0].bridged),
            Some(("a_b", "c_d"))
        );

        let (other, _) = assign_names("a", &names(&["b_c_d"]));
        assert_eq!(split_bridged_name(&other[0].bridged), Some(("a", "b_c_d")));

        assert_ne!(assigned[0].bridged, other[0].bridged);
    }

    #[test]
    fn an_alias_with_a_double_underscore_is_refused_because_it_would_be_ambiguous() {
        let error = validate_alias("a__b").expect_err("consecutive underscores are refused");
        assert!(error.contains("single underscores"), "{error}");
    }

    #[test]
    fn aliases_are_lowercase_ascii_starting_with_a_letter() {
        for good in ["files", "github_ro", "srv2", "a"] {
            validate_alias(good).unwrap_or_else(|error| panic!("{good}: {error}"));
        }
        for bad in [
            "",
            "Files",
            "2fast",
            "_leading",
            "trailing_",
            "with space",
            "dot.ted",
            "ünï",
        ] {
            assert!(
                validate_alias(bad).is_err(),
                "expected {bad:?} to be refused"
            );
        }
    }

    #[test]
    fn an_alias_longer_than_the_limit_is_refused_with_its_length() {
        let long = "a".repeat(MAX_ALIAS_LENGTH + 1);
        let error = validate_alias(&long).expect_err("over-long aliases are refused");
        assert!(
            error.contains(&format!("{}", MAX_ALIAS_LENGTH + 1)),
            "{error}"
        );
    }

    #[test]
    fn characters_a_tool_name_should_not_carry_are_replaced_and_reported() {
        let (assigned, _) = assign_names("srv", &names(&["weird name/with.punctuation"]));

        assert_eq!(assigned[0].bridged, "srv__weird_name_with_punctuation");
        assert_eq!(assigned[0].upstream, "weird name/with.punctuation");
        assert!(assigned[0].notes.contains(&NameNote::Sanitized));
    }

    #[test]
    fn a_name_with_nothing_nameable_left_in_it_still_gets_a_reachable_tool() {
        for upstream in ["", "///", "---", "___"] {
            let (assigned, _) = assign_names("srv", &names(&[upstream]));

            assert_eq!(assigned[0].bridged, "srv__tool", "{upstream:?}");
            assert_eq!(assigned[0].upstream, upstream);
            assert!(
                assigned[0].notes.contains(&NameNote::Sanitized),
                "{upstream:?}"
            );
        }
    }

    #[test]
    fn an_over_long_upstream_name_is_truncated_and_reported() {
        let long = "x".repeat(MAX_LOCAL_LENGTH + 10);
        let (assigned, _) = assign_names("srv", &names(&[&long]));

        assert_eq!(assigned[0].bridged.len(), "srv__".len() + MAX_LOCAL_LENGTH);
        assert_eq!(assigned[0].upstream, long);
        assert!(assigned[0].notes.contains(&NameNote::Truncated));
    }

    #[test]
    fn two_upstream_names_that_sanitize_to_the_same_thing_are_disambiguated_not_lost() {
        let (assigned, dropped) = assign_names("srv", &names(&["a.b", "a/b"]));

        assert!(dropped.is_empty());
        let bridged: Vec<&str> = assigned.iter().map(|item| item.bridged.as_str()).collect();
        assert_eq!(bridged, vec!["srv__a_b", "srv__a_b_2"]);
        // The upstream name is preserved on both, because that is what the
        // call goes out under.
        let upstream: Vec<&str> = assigned.iter().map(|item| item.upstream.as_str()).collect();
        assert_eq!(upstream, vec!["a.b", "a/b"]);
        assert!(assigned[1].notes.contains(&NameNote::Disambiguated));
    }

    #[test]
    fn the_mapping_is_stable_when_the_upstream_reorders_its_list() {
        let (forward, _) = assign_names("srv", &names(&["a.b", "a/b", "zeta"]));
        let (reversed, _) = assign_names("srv", &names(&["zeta", "a/b", "a.b"]));

        assert_eq!(forward, reversed);
    }

    #[test]
    fn a_duplicate_upstream_name_is_listed_once() {
        let (assigned, dropped) = assign_names("srv", &names(&["read", "read"]));

        assert!(dropped.is_empty());
        assert_eq!(assigned.len(), 1);
    }

    /// `status`, `tools`, and `reconnect` are this plugin's own. A bridged name
    /// always contains `__` and they never do, so the two sets cannot meet.
    #[test]
    fn a_bridged_name_can_never_shadow_a_management_tool() {
        for management in crate::bridge::MANAGEMENT_TOOLS {
            assert!(
                !management.contains(SEPARATOR),
                "{management} would be shadowable"
            );
        }
        let (assigned, _) = assign_names("status", &names(&["status"]));
        assert_eq!(assigned[0].bridged, "status__status");
    }

    #[test]
    fn splitting_refuses_names_that_are_not_bridged_names() {
        assert_eq!(split_bridged_name("status"), None);
        assert_eq!(split_bridged_name("__tool"), None);
        assert_eq!(split_bridged_name("alias__"), None);
    }
}
