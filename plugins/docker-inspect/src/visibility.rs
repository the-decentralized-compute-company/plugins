//! Which containers this node is willing to show, and how a caller's reference
//! is turned into a container id.
//!
//! A node contributing hardware to a mesh usually wants to expose *its own*
//! services and nothing else on the machine. The operator writes that as
//! `--container <pattern>` and `--label <key>[=<value>]`, repeatable, and this
//! module is the single place those are applied.
//!
//! Two properties matter more than the matching rules themselves:
//!
//! * **The filter is an allowlist, evaluated before anything is reported.** With
//!   no filter configured everything on the machine is visible; with any filter
//!   configured a container is visible only if it matches one of them. There is
//!   no way to widen it per request — no tool takes a filter argument.
//! * **A caller's reference never becomes part of a request path.** A name or id
//!   prefix from a tool call is matched against the *visible* containers the
//!   daemon reported, and the daemon's own 64-character id is what the next
//!   request uses. A hidden container cannot be reached by naming it exactly,
//!   and a reference containing `/` or `?` matches nothing rather than
//!   redirecting a request somewhere else.

use crate::model::ContainerSummary;

/// One `--container` pattern.
///
/// `*` matches any run of characters, including none; every other character is
/// literal. Matching is case sensitive, because Docker container names are, and
/// an allowlist that quietly matched more than it was written to match would be
/// the wrong kind of forgiving.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamePattern(String);

impl NamePattern {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self(pattern.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn matches(&self, name: &str) -> bool {
        glob_match(&self.0, name)
    }
}

/// One `--label` selector: a key that must be present, optionally with the
/// exact value it must have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelSelector {
    pub key: String,
    pub value: Option<String>,
}

impl LabelSelector {
    /// Parse `key` or `key=value`. An empty key is rejected: `--label =x` is
    /// almost certainly a typo, and treating it as "no constraint" would widen
    /// the allowlist silently.
    pub fn parse(raw: &str, source: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let (key, value) = match raw.split_once('=') {
            Some((key, value)) => (key.trim(), Some(value.trim().to_string())),
            None => (raw, None),
        };
        if key.is_empty() {
            return Err(format!(
                "{source} has an empty label key (`{raw}`). Write `--label com.example.expose` or \
                 `--label com.example.expose=true`."
            ));
        }
        Ok(Self {
            key: key.to_string(),
            value,
        })
    }

    pub fn matches(&self, labels: &std::collections::BTreeMap<String, String>) -> bool {
        match (labels.get(&self.key), &self.value) {
            (Some(actual), Some(expected)) => actual == expected,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// How the selector was written, for `status` output and error messages.
    pub fn describe(&self) -> String {
        match &self.value {
            Some(value) => format!("{}={}", self.key, value),
            None => self.key.clone(),
        }
    }
}

/// The operator's allowlist.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Visibility {
    pub names: Vec<NamePattern>,
    pub labels: Vec<LabelSelector>,
}

impl Visibility {
    /// Whether the operator restricted anything at all.
    pub fn is_filtered(&self) -> bool {
        !self.names.is_empty() || !self.labels.is_empty()
    }

    /// A container is visible when no filter is configured, or when it matches
    /// at least one pattern or at least one label selector.
    ///
    /// The selectors are a union rather than an intersection because they are an
    /// allowlist: `--container web --label com.example.expose` reads as "show
    /// the web container, and anything tagged for exposure".
    pub fn allows(&self, container: &ContainerSummary) -> bool {
        if !self.is_filtered() {
            return true;
        }
        let name_match = self
            .names
            .iter()
            .any(|pattern| container.names().iter().any(|name| pattern.matches(name)));
        name_match
            || self
                .labels
                .iter()
                .any(|selector| selector.matches(&container.labels))
    }

    /// Apply the allowlist, returning the visible containers and how many were
    /// hidden. The hidden *count* is reported so a caller can tell "nothing is
    /// running" from "nothing you may see is running"; nothing about a hidden
    /// container itself is returned.
    pub fn apply(&self, containers: Vec<ContainerSummary>) -> (Vec<ContainerSummary>, usize) {
        let total = containers.len();
        let visible: Vec<ContainerSummary> = containers
            .into_iter()
            .filter(|container| self.allows(container))
            .collect();
        let hidden = total - visible.len();
        (visible, hidden)
    }

    /// One line for `status` and the health check.
    pub fn describe(&self) -> String {
        if !self.is_filtered() {
            return "every container on this machine".to_string();
        }
        let mut parts = Vec::new();
        if !self.names.is_empty() {
            let names: Vec<&str> = self.names.iter().map(NamePattern::as_str).collect();
            parts.push(format!("names matching {}", names.join(", ")));
        }
        if !self.labels.is_empty() {
            let labels: Vec<String> = self.labels.iter().map(LabelSelector::describe).collect();
            parts.push(format!("labels {}", labels.join(", ")));
        }
        parts.join(" or ")
    }
}

/// Why a caller's container reference did not resolve to exactly one container.
#[derive(Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// The reference was empty or contained characters a Docker id or name
    /// cannot contain.
    Malformed(String),
    /// Nothing visible matched.
    NotFound,
    /// Several visible containers matched; the strings are their names.
    Ambiguous(Vec<String>),
}

/// Characters a Docker container name or id can contain.
///
/// Docker's own rule for a name is `[a-zA-Z0-9][a-zA-Z0-9_.-]*`, and an id is
/// hexadecimal. Anything else in a reference is rejected before it is compared
/// against anything, which is what keeps `../`, `?`, `&`, and `%2f` from ever
/// being interesting.
fn is_reference_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
}

/// Match a caller's reference against the visible containers.
///
/// Accepted forms, in the order Docker itself accepts them: the full 64
/// character id, a unique id prefix, or a container name with or without the
/// leading `/` the API reports.
pub fn resolve<'a>(
    reference: &str,
    visible: &'a [ContainerSummary],
) -> Result<&'a ContainerSummary, ResolveError> {
    let reference = reference.trim();
    let reference = reference.strip_prefix('/').unwrap_or(reference);
    if reference.is_empty() {
        return Err(ResolveError::Malformed(
            "no container was named. Pass a container name or id from `list_containers`."
                .to_string(),
        ));
    }
    if !reference.chars().all(is_reference_char) {
        return Err(ResolveError::Malformed(format!(
            "`{reference}` is not a container name or id. Names are made of letters, digits, and \
             `_ . -`; ids are hexadecimal."
        )));
    }

    // Exact matches win outright, so a container literally named `web` is never
    // ambiguous with `web-2`, and a full id is never a prefix of two others.
    let exact: Vec<&ContainerSummary> = visible
        .iter()
        .filter(|container| {
            container.id.eq_ignore_ascii_case(reference)
                || container.names().iter().any(|name| name == reference)
        })
        .collect();
    if let [only] = exact.as_slice() {
        return Ok(only);
    }
    if exact.len() > 1 {
        return Err(ResolveError::Ambiguous(describe_candidates(&exact)));
    }

    let prefixed: Vec<&ContainerSummary> = visible
        .iter()
        .filter(|container| {
            container
                .id
                .to_ascii_lowercase()
                .starts_with(&reference.to_ascii_lowercase())
        })
        .collect();
    match prefixed.as_slice() {
        [] => Err(ResolveError::NotFound),
        [only] => Ok(only),
        several => Err(ResolveError::Ambiguous(describe_candidates(several))),
    }
}

fn describe_candidates(candidates: &[&ContainerSummary]) -> Vec<String> {
    candidates
        .iter()
        .take(10)
        .map(|container| format!("{} ({})", container.primary_name(), container.short_id()))
        .collect()
}

/// `*` wildcard matching, iterative so a pattern like `*a*a*a*a*` cannot blow
/// up: the classic two-pointer algorithm is linear in the subject.
fn glob_match(pattern: &str, subject: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let subject: Vec<char> = subject.chars().collect();

    let (mut p, mut s) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);

    while s < subject.len() {
        if p < pattern.len() && (pattern[p] == subject[s]) {
            p += 1;
            s += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = s;
            p += 1;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            resume += 1;
            s = resume;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|character| *character == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_container;

    #[test]
    fn a_pattern_without_a_wildcard_is_an_exact_match() {
        let pattern = NamePattern::new("web");
        assert!(pattern.matches("web"));
        assert!(!pattern.matches("web-2"));
        assert!(!pattern.matches("myweb"));
    }

    #[test]
    fn wildcards_match_prefixes_suffixes_and_the_middle() {
        assert!(NamePattern::new("tdcc-*").matches("tdcc-node"));
        assert!(!NamePattern::new("tdcc-*").matches("other-node"));
        assert!(NamePattern::new("*-node").matches("tdcc-node"));
        assert!(NamePattern::new("*node*").matches("my-node-1"));
        assert!(NamePattern::new("*").matches("anything"));
        assert!(NamePattern::new("a*b*c").matches("axxbyyc"));
        assert!(!NamePattern::new("a*b*c").matches("axxbyy"));
    }

    #[test]
    fn matching_is_case_sensitive_so_an_allowlist_matches_what_it_says() {
        assert!(!NamePattern::new("web").matches("WEB"));
        assert!(!NamePattern::new("tdcc-*").matches("TDCC-node"));
    }

    #[test]
    fn a_pathological_pattern_still_terminates() {
        let pattern = NamePattern::new("*a*a*a*a*a*a*b");
        assert!(!pattern.matches(&"a".repeat(64)));
    }

    #[test]
    fn label_selectors_parse_as_presence_or_equality() {
        assert_eq!(
            LabelSelector::parse("com.example.expose", "`--label`"),
            Ok(LabelSelector {
                key: "com.example.expose".into(),
                value: None
            })
        );
        assert_eq!(
            LabelSelector::parse("com.example.expose=true", "`--label`"),
            Ok(LabelSelector {
                key: "com.example.expose".into(),
                value: Some("true".into())
            })
        );
        assert!(LabelSelector::parse("=true", "`--label`").is_err());
    }

    #[test]
    fn label_equality_is_exact_and_presence_ignores_the_value() {
        let container = test_container("a".repeat(64), "web", &[("com.example.expose", "true")]);

        assert!(
            LabelSelector::parse("com.example.expose", "s")
                .unwrap()
                .matches(&container.labels)
        );
        assert!(
            LabelSelector::parse("com.example.expose=true", "s")
                .unwrap()
                .matches(&container.labels)
        );
        assert!(
            !LabelSelector::parse("com.example.expose=false", "s")
                .unwrap()
                .matches(&container.labels)
        );
        assert!(
            !LabelSelector::parse("com.example.other", "s")
                .unwrap()
                .matches(&container.labels)
        );
    }

    fn fleet() -> Vec<ContainerSummary> {
        vec![
            test_container("a".repeat(64), "tdcc-node", &[("role", "mesh")]),
            test_container("b".repeat(64), "postgres", &[("role", "database")]),
            test_container(
                "c".repeat(64),
                "billing-secrets",
                &[("com.example.expose", "true")],
            ),
        ]
    }

    #[test]
    fn no_filter_shows_everything_and_hides_nothing() {
        let (visible, hidden) = Visibility::default().apply(fleet());

        assert_eq!(visible.len(), 3);
        assert_eq!(hidden, 0);
        assert_eq!(
            Visibility::default().describe(),
            "every container on this machine"
        );
    }

    #[test]
    fn a_name_pattern_hides_everything_it_does_not_match() {
        let visibility = Visibility {
            names: vec![NamePattern::new("tdcc-*")],
            labels: Vec::new(),
        };

        let (visible, hidden) = visibility.apply(fleet());

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].primary_name(), "tdcc-node");
        assert_eq!(hidden, 2);
    }

    #[test]
    fn name_and_label_selectors_are_a_union_not_an_intersection() {
        let visibility = Visibility {
            names: vec![NamePattern::new("tdcc-*")],
            labels: vec![LabelSelector::parse("com.example.expose=true", "s").unwrap()],
        };

        let (visible, hidden) = visibility.apply(fleet());

        let names: Vec<String> = visible.iter().map(|c| c.primary_name()).collect();
        assert_eq!(names, vec!["tdcc-node", "billing-secrets"]);
        assert_eq!(hidden, 1);
        assert!(visibility.describe().contains("names matching tdcc-*"));
        assert!(
            visibility
                .describe()
                .contains("labels com.example.expose=true")
        );
    }

    #[test]
    fn a_hidden_container_cannot_be_resolved_by_naming_it_exactly() {
        let visibility = Visibility {
            names: vec![NamePattern::new("tdcc-*")],
            labels: Vec::new(),
        };
        let (visible, _) = visibility.apply(fleet());

        assert!(matches!(
            resolve("postgres", &visible),
            Err(ResolveError::NotFound)
        ));
        assert!(matches!(
            resolve(&"b".repeat(64), &visible),
            Err(ResolveError::NotFound)
        ));
        assert!(resolve("tdcc-node", &visible).is_ok());
    }

    #[test]
    fn a_reference_resolves_by_name_id_or_unique_prefix() {
        let containers = fleet();

        assert_eq!(
            resolve("tdcc-node", &containers).unwrap().id,
            "a".repeat(64)
        );
        assert_eq!(
            resolve("/tdcc-node", &containers).unwrap().id,
            "a".repeat(64)
        );
        assert_eq!(
            resolve(&"b".repeat(64), &containers).unwrap().id,
            "b".repeat(64)
        );
        assert_eq!(resolve("ccc", &containers).unwrap().id, "c".repeat(64));
        assert_eq!(
            resolve(&"B".repeat(12), &containers).unwrap().id,
            "b".repeat(64),
            "hex ids compare case insensitively"
        );
    }

    #[test]
    fn an_exact_name_wins_over_a_prefix_of_another_container() {
        let containers = vec![
            test_container("d".repeat(64), "web", &[]),
            test_container("e".repeat(64), "web-2", &[]),
        ];

        assert_eq!(resolve("web", &containers).unwrap().id, "d".repeat(64));
    }

    #[test]
    fn an_ambiguous_prefix_lists_the_candidates_instead_of_guessing() {
        let containers = vec![
            test_container(format!("ab{}", "0".repeat(62)), "one", &[]),
            test_container(format!("ab{}", "1".repeat(62)), "two", &[]),
        ];

        let Err(ResolveError::Ambiguous(candidates)) = resolve("ab", &containers) else {
            panic!("an ambiguous prefix must not resolve");
        };
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0].contains("one"), "{candidates:?}");
    }

    #[test]
    fn a_reference_with_path_or_query_characters_is_refused_before_matching() {
        let containers = fleet();

        for reference in [
            "../../secrets",
            "tdcc-node/json",
            "tdcc-node?all=1",
            "tdcc node",
            "%2e%2e",
            "",
            "   ",
        ] {
            assert!(
                matches!(
                    resolve(reference, &containers),
                    Err(ResolveError::Malformed(_))
                ),
                "`{reference}` must be refused as malformed"
            );
        }
    }
}
