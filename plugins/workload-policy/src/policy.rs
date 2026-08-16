//! The policy document: what an operator writes, and what it is allowed to say.
//!
//! Parsing and validation live here and are pure — `&str` in, [`Policy`] or a
//! list of complaints out. Nothing in this module reads the clock, the network,
//! or the filesystem.
//!
//! Two decisions in here are worth reading before changing anything:
//!
//! * **Unknown keys are errors.** Every table sets `deny_unknown_fields`. A
//!   silently ignored `modle = "enforce"` is a policy that does not exist, on a
//!   machine whose owner believes it does.
//! * **Validation is all-or-nothing.** A document with one bad rule produces no
//!   policy at all, not a policy missing one rule. The rule that fails to parse
//!   is, by Murphy, the one that was holding the door shut.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::clock::{HourWindow, Weekday, Zone};

/// The only document version this build understands.
pub const POLICY_VERSION: u32 = 1;
/// Upper bound on rules in one document. Evaluation is linear in this.
pub const MAX_RULES: usize = 512;
/// Default number of decisions retained for the dry-run report.
pub const DEFAULT_OBSERVATIONS: usize = 500;
/// Upper bound an operator may raise the observation ring to.
pub const MAX_OBSERVATIONS: usize = 10_000;
const MAX_LIMIT_REQUESTS: u32 = 10_000;
const MAX_LIMIT_WINDOW_SECS: u32 = 86_400;
const MAX_ID_LENGTH: usize = 64;
const MAX_REASON_LENGTH: usize = 500;

/// Whether matched deny rules actually refuse work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Evaluate and record, but serve everything. The default, and the reason
    /// installing this plugin cannot take a node offline by itself.
    DryRun,
    /// Evaluate and refuse.
    Enforce,
}

impl Mode {
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "dry-run" | "dry_run" | "dryrun" => Ok(Self::DryRun),
            "enforce" => Ok(Self::Enforce),
            other => Err(format!(
                "unknown mode '{other}'; expected \"dry-run\" or \"enforce\""
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry-run",
            Self::Enforce => "enforce",
        }
    }
}

/// What a rule does when it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Deny,
    /// Allow while the rule's own budget lasts, then deny.
    Limit,
}

impl Action {
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            "limit" => Ok(Self::Limit),
            other => Err(format!(
                "unknown action '{other}'; expected \"allow\", \"deny\", or \"limit\""
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Limit => "limit",
        }
    }
}

/// The verdict for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
}

impl Decision {
    fn parse_default(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            other => Err(format!(
                "unknown default '{other}'; expected \"allow\" or \"deny\" (a default cannot be \"limit\")"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// Which counter a `limit` rule spends from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitScope {
    /// One budget for the whole node.
    Node,
    /// One budget per submitting peer.
    Peer,
    /// One budget per owner identity.
    Owner,
    /// One budget per requested model.
    Model,
}

impl LimitScope {
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "node" => Ok(Self::Node),
            "peer" => Ok(Self::Peer),
            "owner" => Ok(Self::Owner),
            "model" => Ok(Self::Model),
            other => Err(format!(
                "unknown limit scope '{other}'; expected \"node\", \"peer\", \"owner\", or \"model\""
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Peer => "peer",
            Self::Owner => "owner",
            Self::Model => "model",
        }
    }
}

/// A wildcard string pattern. `*` matches any run of characters, including
/// none; every other character is literal.
///
/// Case sensitivity is deliberately split. Model ids and request kinds are
/// matched case-insensitively because operators type them by hand and mean the
/// same model either way. Peer and owner identifiers are matched
/// case-sensitively because they are opaque identifiers — base58 and hex-cased
/// alphabets have distinct values that differ only in case, and folding them
/// together in an allow list would quietly widen it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    raw: String,
    matcher: String,
    case_insensitive: bool,
}

impl Pattern {
    pub fn new(raw: impl Into<String>, case_insensitive: bool) -> Self {
        let raw = raw.into();
        let matcher = if case_insensitive {
            raw.to_ascii_lowercase()
        } else {
            raw.clone()
        };
        Self {
            raw,
            matcher,
            case_insensitive,
        }
    }

    pub fn matches(&self, value: &str) -> bool {
        if self.case_insensitive {
            glob_match(&self.matcher, &value.to_ascii_lowercase())
        } else {
            glob_match(&self.matcher, value)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl Serialize for Pattern {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw)
    }
}

/// Ordered wildcard match. `*` is the only metacharacter.
fn glob_match(pattern: &str, value: &str) -> bool {
    let segments: Vec<&str> = pattern.split('*').collect();
    if segments.len() == 1 {
        return pattern == value;
    }

    let first = segments[0];
    let Some(mut rest) = value.strip_prefix(first) else {
        return false;
    };

    let last = segments[segments.len() - 1];
    for segment in &segments[1..segments.len() - 1] {
        if segment.is_empty() {
            continue;
        }
        match rest.find(segment) {
            Some(index) => rest = &rest[index + segment.len()..],
            None => return false,
        }
    }

    last.is_empty() || (rest.len() >= last.len() && rest.ends_with(last))
}

/// Every condition a rule may declare. All declared conditions must hold for
/// the rule to match; an empty set matches every request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Conditions {
    pub models: Option<Vec<Pattern>>,
    pub peers: Option<Vec<Pattern>>,
    pub owners: Option<Vec<Pattern>>,
    pub kinds: Option<Vec<String>>,
    pub context_tokens_over: Option<u64>,
    pub max_output_tokens_over: Option<u64>,
    pub hours: Option<Vec<HourWindow>>,
    pub days: Option<Vec<Weekday>>,
}

impl Conditions {
    /// Human-readable one-line-per-condition summary, for the `policy` tool.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(models) = &self.models {
            lines.push(format!("model matches one of: {}", join_patterns(models)));
        }
        if let Some(peers) = &self.peers {
            lines.push(format!("peer matches one of: {}", join_patterns(peers)));
        }
        if let Some(owners) = &self.owners {
            lines.push(format!("owner matches one of: {}", join_patterns(owners)));
        }
        if let Some(kinds) = &self.kinds {
            lines.push(format!("kind is one of: {}", kinds.join(", ")));
        }
        if let Some(threshold) = self.context_tokens_over {
            lines.push(format!("context_tokens is greater than {threshold}"));
        }
        if let Some(threshold) = self.max_output_tokens_over {
            lines.push(format!("max_output_tokens is greater than {threshold}"));
        }
        if let Some(hours) = &self.hours {
            let windows: Vec<String> = hours.iter().map(HourWindow::to_string).collect();
            lines.push(format!("local time is within: {}", windows.join(", ")));
        }
        if let Some(days) = &self.days {
            let names: Vec<&str> = days.iter().map(|day| day.as_str()).collect();
            lines.push(format!("day is one of: {}", names.join(", ")));
        }
        if lines.is_empty() {
            lines.push("matches every request".to_string());
        }
        lines
    }
}

fn join_patterns(patterns: &[Pattern]) -> String {
    patterns
        .iter()
        .map(Pattern::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Limit {
    pub requests: u32,
    pub per_seconds: u32,
    pub per: LimitScope,
}

impl Limit {
    pub fn window_ms(&self) -> i64 {
        i64::from(self.per_seconds) * 1_000
    }

    pub fn describe(&self) -> String {
        format!(
            "{} requests per {} s, per {}",
            self.requests,
            self.per_seconds,
            self.per.as_str()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rule {
    pub id: String,
    pub action: Action,
    pub reason: Option<String>,
    pub when: Conditions,
    pub limit: Option<Limit>,
}

impl Rule {
    /// The sentence a refused caller sees. An operator-written `reason` is
    /// always preferred; the fallback names the rule so a refusal is never
    /// anonymous.
    pub fn refusal_reason(&self) -> String {
        self.reason.clone().unwrap_or_else(|| {
            format!(
                "this node's local workload policy rule '{}' does not accept this request",
                self.id
            )
        })
    }
}

/// Request fields the loaded policy actually depends on.
///
/// A rule matches only when every condition it declares is satisfied, and a
/// condition over an absent field is not satisfied. On its own that would let a
/// caller walk past a deny rule by omitting `model`, so the evaluator refuses a
/// request that omits any field the policy references. This struct is how the
/// evaluator knows which those are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RequiredFields {
    pub model: bool,
    pub peer: bool,
    pub owner: bool,
    pub kind: bool,
    pub context_tokens: bool,
    pub max_output_tokens: bool,
}

impl RequiredFields {
    pub fn names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.model {
            names.push("model");
        }
        if self.peer {
            names.push("peer");
        }
        if self.owner {
            names.push("owner");
        }
        if self.kind {
            names.push("kind");
        }
        if self.context_tokens {
            names.push("context_tokens");
        }
        if self.max_output_tokens {
            names.push("max_output_tokens");
        }
        names
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Policy {
    pub mode: Mode,
    pub default_action: Decision,
    pub zone: Zone,
    pub observation_capacity: usize,
    pub rules: Vec<Rule>,
    pub required: RequiredFields,
}

impl Policy {
    /// What runs when the operator has written no policy file: evaluate
    /// nothing, refuse nothing, record everything. Permissive but visible.
    pub fn permissive() -> Self {
        Self {
            mode: Mode::DryRun,
            default_action: Decision::Allow,
            zone: Zone::Local,
            observation_capacity: DEFAULT_OBSERVATIONS,
            rules: Vec::new(),
            required: RequiredFields::default(),
        }
    }

    /// True when the operator wrote rules that currently refuse nothing. The
    /// `policy` tool promotes this to a warning, because a policy that is not
    /// enforcing is the single most likely thing to be misunderstood here.
    pub fn is_silently_permissive(&self) -> bool {
        self.mode == Mode::DryRun && !self.rules.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Deserialization surface
// ---------------------------------------------------------------------------
//
// The raw types below mirror the TOML exactly and take every enum as a string,
// so a typo produces "unknown action 'dney'; expected ..." instead of serde's
// generic variant error. They are private: nothing outside this module should
// see a policy that has not been validated.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    version: u32,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default, rename = "default")]
    default_action: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    observe: Option<usize>,
    #[serde(default, rename = "rule")]
    rules: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    id: String,
    action: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    when: RawConditions,
    #[serde(default)]
    limit: Option<RawLimit>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConditions {
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(default)]
    peers: Option<Vec<String>>,
    #[serde(default)]
    owners: Option<Vec<String>>,
    #[serde(default)]
    kinds: Option<Vec<String>>,
    #[serde(default)]
    context_tokens_over: Option<u64>,
    #[serde(default)]
    max_output_tokens_over: Option<u64>,
    #[serde(default)]
    hours: Option<Vec<String>>,
    #[serde(default)]
    days: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimit {
    requests: u32,
    per_seconds: u32,
    #[serde(default)]
    per: Option<String>,
}

/// Parse and validate a policy document.
///
/// The error is every complaint found, not just the first: an operator fixing a
/// policy file at 2 a.m. should not have to run the loop six times.
pub fn parse_policy(text: &str) -> Result<Policy, Vec<String>> {
    let raw: RawPolicy = toml::from_str(text).map_err(|error| vec![error.to_string()])?;
    validate(raw)
}

fn validate(raw: RawPolicy) -> Result<Policy, Vec<String>> {
    let mut errors = Vec::new();

    if raw.version != POLICY_VERSION {
        errors.push(format!(
            "unsupported policy version {}; this build understands version {POLICY_VERSION}",
            raw.version
        ));
    }

    let mode = parse_optional(raw.mode.as_deref(), Mode::parse, Mode::DryRun, &mut errors);
    let default_action = parse_optional(
        raw.default_action.as_deref(),
        Decision::parse_default,
        Decision::Allow,
        &mut errors,
    );
    let zone = parse_optional(
        raw.timezone.as_deref(),
        Zone::parse,
        Zone::Local,
        &mut errors,
    );

    let observation_capacity = match raw.observe {
        Some(value) if value > MAX_OBSERVATIONS => {
            errors.push(format!(
                "observe = {value} exceeds the maximum of {MAX_OBSERVATIONS}"
            ));
            DEFAULT_OBSERVATIONS
        }
        Some(value) => value,
        None => DEFAULT_OBSERVATIONS,
    };

    if raw.rules.len() > MAX_RULES {
        errors.push(format!(
            "{} rules declared; the maximum is {MAX_RULES}",
            raw.rules.len()
        ));
    }

    let mut seen_ids = BTreeSet::new();
    let mut rules = Vec::with_capacity(raw.rules.len());
    for (index, raw_rule) in raw.rules.into_iter().enumerate() {
        match validate_rule(index, raw_rule, &mut seen_ids) {
            Ok(rule) => rules.push(rule),
            Err(mut rule_errors) => errors.append(&mut rule_errors),
        }
    }

    if errors.is_empty() {
        let required = required_fields(&rules);
        Ok(Policy {
            mode,
            default_action,
            zone,
            observation_capacity,
            rules,
            required,
        })
    } else {
        Err(errors)
    }
}

fn parse_optional<T: Copy>(
    value: Option<&str>,
    parse: fn(&str) -> Result<T, String>,
    fallback: T,
    errors: &mut Vec<String>,
) -> T {
    match value {
        None => fallback,
        Some(value) => match parse(value) {
            Ok(parsed) => parsed,
            Err(error) => {
                errors.push(error);
                fallback
            }
        },
    }
}

fn validate_rule(
    index: usize,
    raw: RawRule,
    seen_ids: &mut BTreeSet<String>,
) -> Result<Rule, Vec<String>> {
    let mut errors = Vec::new();
    // Rules are identified by id everywhere else — in refusals, counters, and
    // rate-limit bucket keys — so a rule with a broken id is reported by
    // position instead.
    let label = if raw.id.trim().is_empty() {
        format!("rule #{}", index + 1)
    } else {
        format!("rule '{}'", raw.id)
    };

    let id = raw.id.trim().to_string();
    if id.is_empty() {
        errors.push(format!("{label}: id must not be empty"));
    } else if id.len() > MAX_ID_LENGTH {
        errors.push(format!(
            "{label}: id is {} characters; the maximum is {MAX_ID_LENGTH}",
            id.len()
        ));
    } else if !id
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_.:".contains(character))
    {
        errors.push(format!(
            "{label}: id may only contain ASCII letters, digits, '-', '_', '.', and ':'"
        ));
    } else if !seen_ids.insert(id.clone()) {
        errors.push(format!("{label}: duplicate rule id"));
    }

    let action = match Action::parse(&raw.action) {
        Ok(action) => Some(action),
        Err(error) => {
            errors.push(format!("{label}: {error}"));
            None
        }
    };

    if let Some(reason) = &raw.reason
        && reason.len() > MAX_REASON_LENGTH
    {
        errors.push(format!(
            "{label}: reason is {} characters; the maximum is {MAX_REASON_LENGTH}",
            reason.len()
        ));
    }

    let limit = match (action, raw.limit) {
        (Some(Action::Limit), None) => {
            errors.push(format!(
                "{label}: action = \"limit\" requires a [rule.limit] table with `requests` and `per_seconds`"
            ));
            None
        }
        (Some(Action::Limit), Some(raw_limit)) => match validate_limit(&label, raw_limit) {
            Ok(limit) => Some(limit),
            Err(mut limit_errors) => {
                errors.append(&mut limit_errors);
                None
            }
        },
        (_, Some(_)) => {
            // Silently ignoring the table would look like a working rate limit.
            errors.push(format!(
                "{label}: [rule.limit] is only meaningful with action = \"limit\""
            ));
            None
        }
        (_, None) => None,
    };

    let when = validate_conditions(&label, raw.when, &mut errors);

    match (action, errors.is_empty()) {
        (Some(action), true) => Ok(Rule {
            id,
            action,
            reason: raw.reason,
            when,
            limit,
        }),
        _ => Err(errors),
    }
}

fn validate_limit(label: &str, raw: RawLimit) -> Result<Limit, Vec<String>> {
    let mut errors = Vec::new();

    if raw.requests == 0 {
        errors.push(format!(
            "{label}: limit.requests must be at least 1; use action = \"deny\" to refuse everything"
        ));
    } else if raw.requests > MAX_LIMIT_REQUESTS {
        errors.push(format!(
            "{label}: limit.requests = {} exceeds the maximum of {MAX_LIMIT_REQUESTS}",
            raw.requests
        ));
    }

    if raw.per_seconds == 0 {
        errors.push(format!("{label}: limit.per_seconds must be at least 1"));
    } else if raw.per_seconds > MAX_LIMIT_WINDOW_SECS {
        errors.push(format!(
            "{label}: limit.per_seconds = {} exceeds the maximum of {MAX_LIMIT_WINDOW_SECS} (24 h)",
            raw.per_seconds
        ));
    }

    let per = match raw.per.as_deref() {
        None => LimitScope::Node,
        Some(value) => match LimitScope::parse(value) {
            Ok(scope) => scope,
            Err(error) => {
                errors.push(format!("{label}: {error}"));
                LimitScope::Node
            }
        },
    };

    if errors.is_empty() {
        Ok(Limit {
            requests: raw.requests,
            per_seconds: raw.per_seconds,
            per,
        })
    } else {
        Err(errors)
    }
}

fn validate_conditions(label: &str, raw: RawConditions, errors: &mut Vec<String>) -> Conditions {
    Conditions {
        models: patterns(label, "models", raw.models, true, errors),
        peers: patterns(label, "peers", raw.peers, false, errors),
        owners: patterns(label, "owners", raw.owners, false, errors),
        kinds: strings(label, "kinds", raw.kinds, errors),
        context_tokens_over: raw.context_tokens_over,
        max_output_tokens_over: raw.max_output_tokens_over,
        hours: parsed_list(label, "hours", raw.hours, HourWindow::parse, errors),
        days: parsed_list(label, "days", raw.days, Weekday::parse, errors),
    }
}

/// An empty condition list matches nothing, which reads as "no condition" and
/// behaves as "never matches". That gap is where a rule silently stops working,
/// so it is rejected instead.
fn require_non_empty(
    label: &str,
    field: &str,
    values: &[String],
    errors: &mut Vec<String>,
) -> bool {
    if values.is_empty() {
        errors.push(format!(
            "{label}: when.{field} is an empty list; remove it, or list at least one value"
        ));
        return false;
    }
    if let Some(position) = values.iter().position(|value| value.trim().is_empty()) {
        errors.push(format!("{label}: when.{field}[{position}] is empty"));
        return false;
    }
    true
}

fn patterns(
    label: &str,
    field: &str,
    values: Option<Vec<String>>,
    case_insensitive: bool,
    errors: &mut Vec<String>,
) -> Option<Vec<Pattern>> {
    let values = values?;
    if !require_non_empty(label, field, &values, errors) {
        return None;
    }
    Some(
        values
            .into_iter()
            .map(|value| Pattern::new(value.trim(), case_insensitive))
            .collect(),
    )
}

fn strings(
    label: &str,
    field: &str,
    values: Option<Vec<String>>,
    errors: &mut Vec<String>,
) -> Option<Vec<String>> {
    let values = values?;
    if !require_non_empty(label, field, &values, errors) {
        return None;
    }
    Some(
        values
            .into_iter()
            .map(|value| value.trim().to_ascii_lowercase())
            .collect(),
    )
}

fn parsed_list<T>(
    label: &str,
    field: &str,
    values: Option<Vec<String>>,
    parse: fn(&str) -> Result<T, String>,
    errors: &mut Vec<String>,
) -> Option<Vec<T>> {
    let values = values?;
    if !require_non_empty(label, field, &values, errors) {
        return None;
    }
    let mut parsed = Vec::with_capacity(values.len());
    for value in values {
        match parse(&value) {
            Ok(item) => parsed.push(item),
            Err(error) => errors.push(format!("{label}: when.{field}: {error}")),
        }
    }
    Some(parsed)
}

fn required_fields(rules: &[Rule]) -> RequiredFields {
    let mut required = RequiredFields::default();
    for rule in rules {
        required.model |= rule.when.models.is_some();
        required.peer |= rule.when.peers.is_some();
        required.owner |= rule.when.owners.is_some();
        required.kind |= rule.when.kinds.is_some();
        required.context_tokens |= rule.when.context_tokens_over.is_some();
        required.max_output_tokens |= rule.when.max_output_tokens_over.is_some();

        // A limit keyed on an identifier needs that identifier, or every
        // request without it would share one anonymous bucket.
        match rule.limit.map(|limit| limit.per) {
            Some(LimitScope::Peer) => required.peer = true,
            Some(LimitScope::Owner) => required.owner = true,
            Some(LimitScope::Model) => required.model = true,
            Some(LimitScope::Node) | None => {}
        }
    }
    required
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_DOCUMENT: &str = r#"
version = 1
mode = "enforce"
default = "deny"
timezone = "utc"
observe = 25

[[rule]]
id = "overnight-window"
action = "allow"
when.hours = ["22:00-06:00"]
when.days = ["Mon", "tue", "WEDNESDAY"]

[[rule]]
id = "small-jobs-only"
action = "deny"
reason = "This node caps context at 8192 tokens."
when.context_tokens_over = 8192

[[rule]]
id = "per-peer-rate"
action = "limit"
when.models = ["Qwen/Qwen3-*"]
limit = { requests = 60, per_seconds = 60, per = "peer" }
"#;

    #[test]
    fn a_complete_document_parses_into_the_declared_shape() {
        let policy = parse_policy(FULL_DOCUMENT).expect("document is valid");

        assert_eq!(policy.mode, Mode::Enforce);
        assert_eq!(policy.default_action, Decision::Deny);
        assert_eq!(policy.zone, Zone::Utc);
        assert_eq!(policy.observation_capacity, 25);
        assert_eq!(policy.rules.len(), 3);
        assert_eq!(policy.rules[0].id, "overnight-window");
        assert_eq!(
            policy.rules[2].limit.expect("limit rule").per,
            LimitScope::Peer
        );
    }

    #[test]
    fn omitted_top_level_fields_default_to_permissive_and_visible() {
        let policy = parse_policy("version = 1").expect("minimal document is valid");

        assert_eq!(policy.mode, Mode::DryRun);
        assert_eq!(policy.default_action, Decision::Allow);
        assert_eq!(policy.zone, Zone::Local);
        assert_eq!(policy.observation_capacity, DEFAULT_OBSERVATIONS);
        assert!(policy.rules.is_empty());
    }

    #[test]
    fn required_fields_follow_the_conditions_the_rules_actually_use() {
        let policy = parse_policy(FULL_DOCUMENT).expect("document is valid");

        assert_eq!(
            policy.required,
            RequiredFields {
                model: true,
                peer: true,
                owner: false,
                kind: false,
                context_tokens: true,
                max_output_tokens: false,
            }
        );
        assert_eq!(
            policy.required.names(),
            vec!["model", "peer", "context_tokens"]
        );
    }

    #[test]
    fn a_node_scoped_limit_does_not_make_any_identifier_required() {
        let policy = parse_policy(
            r#"
version = 1
[[rule]]
id = "node-rate"
action = "limit"
limit = { requests = 5, per_seconds = 1 }
"#,
        )
        .expect("document is valid");

        assert_eq!(policy.required, RequiredFields::default());
        assert_eq!(policy.rules[0].limit.expect("limit").per, LimitScope::Node);
    }

    #[test]
    fn a_misspelled_key_is_an_error_rather_than_a_rule_that_does_nothing() {
        let errors = parse_policy("version = 1\nmodle = \"enforce\"\n")
            .expect_err("unknown keys must be rejected");

        assert!(
            errors[0].contains("modle"),
            "error should name the unknown key: {errors:?}"
        );
    }

    #[test]
    fn a_misspelled_condition_is_an_error_too() {
        let errors = parse_policy(
            r#"
version = 1
[[rule]]
id = "typo"
action = "deny"
when.modelz = ["a"]
"#,
        )
        .expect_err("unknown condition keys must be rejected");

        assert!(errors[0].contains("modelz"), "{errors:?}");
    }

    #[test]
    fn every_complaint_is_reported_at_once() {
        let errors = parse_policy(
            r#"
version = 4

[[rule]]
id = "one"
action = "dney"

[[rule]]
id = "one"
action = "deny"
when.hours = ["25:00-06:00"]
"#,
        )
        .expect_err("document is invalid");

        let joined = errors.join("\n");
        assert!(joined.contains("unsupported policy version 4"), "{joined}");
        assert!(joined.contains("unknown action 'dney'"), "{joined}");
        assert!(joined.contains("duplicate rule id"), "{joined}");
        assert!(joined.contains("25:00-06:00"), "{joined}");
    }

    #[test]
    fn a_limit_action_without_a_limit_table_is_rejected() {
        let errors = parse_policy(
            r#"
version = 1
[[rule]]
id = "no-budget"
action = "limit"
"#,
        )
        .expect_err("limit rules need a budget");

        assert!(errors[0].contains("[rule.limit]"), "{errors:?}");
    }

    #[test]
    fn a_limit_table_on_a_deny_rule_is_rejected() {
        let errors = parse_policy(
            r#"
version = 1
[[rule]]
id = "confused"
action = "deny"
limit = { requests = 1, per_seconds = 1 }
"#,
        )
        .expect_err("a deny rule has no budget to spend");

        assert!(errors[0].contains("only meaningful"), "{errors:?}");
    }

    #[test]
    fn out_of_range_limits_are_rejected() {
        let errors = parse_policy(
            r#"
version = 1
[[rule]]
id = "silly"
action = "limit"
limit = { requests = 0, per_seconds = 999999 }
"#,
        )
        .expect_err("limits have bounds");

        let joined = errors.join("\n");
        assert!(
            joined.contains("limit.requests must be at least 1"),
            "{joined}"
        );
        assert!(joined.contains("limit.per_seconds"), "{joined}");
    }

    #[test]
    fn an_empty_condition_list_is_rejected_rather_than_matching_nothing() {
        let errors = parse_policy(
            r#"
version = 1
[[rule]]
id = "empty"
action = "deny"
when.models = []
"#,
        )
        .expect_err("an empty list is a trap");

        assert!(errors[0].contains("empty list"), "{errors:?}");
    }

    #[test]
    fn rule_ids_are_constrained_so_they_stay_usable_as_identifiers() {
        let errors = parse_policy(
            r#"
version = 1
[[rule]]
id = "no spaces here"
action = "deny"
"#,
        )
        .expect_err("ids are identifiers");

        assert!(errors[0].contains("ASCII letters"), "{errors:?}");
    }

    #[test]
    fn malformed_toml_reports_the_parse_error_and_nothing_else() {
        let errors = parse_policy("version = ").expect_err("not valid TOML");

        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn glob_matches_prefix_suffix_and_middle() {
        assert!(glob_match("qwen/*", "qwen/qwen3-8b"));
        assert!(glob_match("*-8b", "qwen/qwen3-8b"));
        assert!(glob_match("qwen/*3*8b", "qwen/qwen3-8b"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("exact", "exact"));

        assert!(!glob_match("qwen/*", "meta/llama"));
        assert!(!glob_match("exact", "exacto"));
        assert!(!glob_match("a*a", "a"));
        assert!(!glob_match("qwen/*3*8b", "qwen/qwen3-70b"));
    }

    #[test]
    fn model_patterns_ignore_case_but_peer_patterns_do_not() {
        let model = Pattern::new("Qwen/Qwen3-*", true);
        assert!(model.matches("qwen/qwen3-8b"));
        assert!(model.matches("QWEN/QWEN3-8B"));

        // Base58 and hex-cased peer ids differ by case, so folding would widen
        // an allow list past what the operator wrote.
        let peer = Pattern::new("12D3KooWAbc*", false);
        assert!(peer.matches("12D3KooWAbcdef"));
        assert!(!peer.matches("12d3koowabcdef"));
    }

    #[test]
    fn conditions_describe_themselves_for_the_policy_view() {
        let policy = parse_policy(FULL_DOCUMENT).expect("document is valid");

        let described = policy.rules[0].when.describe();
        assert!(described.iter().any(|line| line.contains("22:00-06:00")));
        assert!(described.iter().any(|line| line.contains("mon, tue, wed")));

        assert_eq!(
            Conditions::default().describe(),
            vec!["matches every request".to_string()]
        );
    }

    #[test]
    fn a_rule_without_a_reason_still_refuses_by_name() {
        let policy = parse_policy(
            r#"
version = 1
[[rule]]
id = "anonymous"
action = "deny"
"#,
        )
        .expect("document is valid");

        assert!(policy.rules[0].refusal_reason().contains("'anonymous'"));
    }

    #[test]
    fn a_dry_run_policy_with_rules_is_flagged_as_silently_permissive() {
        let policy = parse_policy(
            r#"
version = 1
[[rule]]
id = "deny-all"
action = "deny"
"#,
        )
        .expect("document is valid");

        assert!(policy.is_silently_permissive());
        assert!(!Policy::permissive().is_silently_permissive());
    }
}
