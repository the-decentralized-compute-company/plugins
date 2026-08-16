//! `robots.txt` parsing, matching, and caching.
//!
//! Following the Robots Exclusion Protocol (RFC 9309) is the price of making
//! requests from somebody else's IP address. The parser and the matcher are
//! pure functions; the cache is the only stateful part, and it exists so that
//! reading ten pages from one site does not fetch `robots.txt` ten times.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::Url;

/// How long a fetched `robots.txt` stays authoritative.
const CACHE_TTL: Duration = Duration::from_secs(3_600);
/// Cap on cached origins, so a long-running node cannot grow this map without
/// bound. When it fills, the whole map is dropped — a blunt eviction, but this
/// is a courtesy cache, not a hot path.
const CACHE_CAPACITY: usize = 512;
/// `robots.txt` files are meant to be small. Anything past this is truncated
/// rather than trusted.
pub const MAX_ROBOTS_BYTES: usize = 512 * 1_024;

/// The rules that apply to one crawler on one origin.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RobotsRules {
    rules: Vec<Rule>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    allow: bool,
    pattern: String,
}

impl RobotsRules {
    /// No rules at all — used when a site has no `robots.txt` (404) or when the
    /// operator has turned the check off.
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// A blanket refusal, used when `robots.txt` could not be read.
    pub fn disallow_all() -> Self {
        Self {
            rules: vec![Rule {
                allow: false,
                pattern: "/".to_string(),
            }],
        }
    }

    /// Is this path (with its query string) allowed?
    ///
    /// RFC 9309: the most specific — longest — matching pattern wins, and an
    /// `Allow` beats a `Disallow` of the same length.
    pub fn allows(&self, path_and_query: &str) -> bool {
        let mut best: Option<&Rule> = None;
        for rule in &self.rules {
            if !pattern_matches(&rule.pattern, path_and_query) {
                continue;
            }
            best = match best {
                None => Some(rule),
                Some(current)
                    if rule.pattern.len() > current.pattern.len()
                        || (rule.pattern.len() == current.pattern.len() && rule.allow) =>
                {
                    Some(rule)
                }
                other => other,
            };
        }
        best.is_none_or(|rule| rule.allow)
    }
}

/// Parse `robots.txt` for one product token.
///
/// Groups are keyed by `User-agent`; consecutive `User-agent` lines share the
/// rules that follow them. The group naming our token wins; otherwise the `*`
/// group applies; otherwise everything is allowed.
pub fn parse_robots(text: &str, product_token: &str) -> RobotsRules {
    let token = product_token.to_ascii_lowercase();

    let mut matching: Vec<Rule> = Vec::new();
    let mut wildcard: Vec<Rule> = Vec::new();
    let mut matched_group = false;

    // Agent names collected since the last rule line; a rule line closes the
    // header block and starts a body.
    let mut current_agents: Vec<String> = Vec::new();
    let mut in_body = false;

    for raw_line in text.lines() {
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let field = field.trim().to_ascii_lowercase();
        let value = value.trim();

        match field.as_str() {
            "user-agent" => {
                if in_body {
                    // A new header block starts a new group.
                    current_agents.clear();
                    in_body = false;
                }
                current_agents.push(value.to_ascii_lowercase());
            }
            "allow" | "disallow" => {
                in_body = true;
                let allow = field == "allow";
                // An empty `Disallow:` is the documented way to say "nothing is
                // disallowed"; an empty `Allow:` carries no information.
                if value.is_empty() {
                    if !allow {
                        record(
                            &current_agents,
                            &token,
                            &mut matching,
                            &mut wildcard,
                            &mut matched_group,
                            None,
                        );
                    }
                    continue;
                }
                let rule = Rule {
                    allow,
                    pattern: normalize_pattern(value),
                };
                record(
                    &current_agents,
                    &token,
                    &mut matching,
                    &mut wildcard,
                    &mut matched_group,
                    Some(rule),
                );
            }
            // Sitemap, Crawl-delay, Host and anything else are not rules.
            _ => {}
        }
    }

    RobotsRules {
        rules: if matched_group { matching } else { wildcard },
    }
}

fn record(
    agents: &[String],
    token: &str,
    matching: &mut Vec<Rule>,
    wildcard: &mut Vec<Rule>,
    matched_group: &mut bool,
    rule: Option<Rule>,
) {
    if agents.iter().any(|agent| agent == token) {
        *matched_group = true;
        if let Some(rule) = rule {
            matching.push(rule);
        }
    } else if agents.iter().any(|agent| agent == "*")
        && let Some(rule) = rule
    {
        wildcard.push(rule);
    }
}

fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(index) => &line[..index],
        None => line,
    }
}

fn normalize_pattern(value: &str) -> String {
    if value.starts_with('/') {
        value.to_string()
    } else {
        format!("/{value}")
    }
}

/// Glob matching as RFC 9309 defines it: `*` matches any run of characters and
/// a trailing `$` anchors the end of the path. Everything else is a literal
/// prefix match.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(stripped) => (stripped, true),
        None => (pattern, false),
    };

    let segments: Vec<&str> = pattern.split('*').collect();
    let last_index = segments.len() - 1;
    let mut cursor = 0usize;

    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            continue;
        }
        if index == 0 {
            // The first segment must match at the very start.
            if !path[cursor..].starts_with(segment) {
                return false;
            }
            cursor += segment.len();
            continue;
        }
        if anchored && index == last_index {
            // Anchor the tail rather than taking the leftmost match, so
            // `/*.pdf$` still matches `/a.pdf/b.pdf`.
            if !path[cursor..].ends_with(segment) {
                return false;
            }
            cursor = path.len();
            continue;
        }
        match path[cursor..].find(segment) {
            Some(offset) => cursor += offset + segment.len(),
            None => return false,
        }
    }

    if anchored {
        // With `$`, the last segment has to land exactly on the end. When the
        // pattern ends in `*`, any remaining tail is fine.
        let ends_with_wildcard = segments.last().is_some_and(|segment| segment.is_empty());
        return ends_with_wildcard || cursor == path.len();
    }
    true
}

/// Per-origin cache of parsed `robots.txt` rules.
pub struct RobotsCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
}

struct CacheEntry {
    fetched_at: Instant,
    rules: RobotsRules,
}

impl RobotsCache {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, origin: &str) -> Option<RobotsRules> {
        let entries = self.lock();
        let entry = entries.get(origin)?;
        (entry.fetched_at.elapsed() < CACHE_TTL).then(|| entry.rules.clone())
    }

    pub fn put(&self, origin: String, rules: RobotsRules) {
        let mut entries = self.lock();
        if entries.len() >= CACHE_CAPACITY {
            entries.clear();
        }
        entries.insert(
            origin,
            CacheEntry {
                fetched_at: Instant::now(),
                rules,
            },
        );
    }

    /// A poisoned lock means a caller panicked mid-update. The map holds no
    /// cross-entry invariant, so recovering it keeps the plugin usable instead
    /// of failing every later fetch.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, CacheEntry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for RobotsCache {
    fn default() -> Self {
        Self::new()
    }
}

/// The cache key and the `robots.txt` location for a URL: scheme, host, port.
pub fn origin_of(url: &Url) -> String {
    let mut origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    if let Some(port) = url.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    origin
}

/// The path plus query string that `robots.txt` rules are matched against.
pub fn request_target(url: &Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{}", url.path(), query),
        None => url.path().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "tdcc-web-search";

    #[test]
    fn an_empty_file_allows_everything() {
        assert!(parse_robots("", TOKEN).allows("/anything"));
    }

    #[test]
    fn a_wildcard_group_applies_when_no_group_names_us() {
        let rules = parse_robots("User-agent: *\nDisallow: /private\n", TOKEN);

        assert!(!rules.allows("/private"));
        assert!(!rules.allows("/private/deep"));
        assert!(rules.allows("/public"));
    }

    #[test]
    fn an_empty_disallow_means_everything_is_allowed() {
        let rules = parse_robots("User-agent: *\nDisallow:\n", TOKEN);

        assert!(rules.allows("/anything"));
    }

    #[test]
    fn a_group_naming_our_token_replaces_the_wildcard_group() {
        let robots =
            "User-agent: *\nDisallow: /\n\nUser-agent: tdcc-web-search\nDisallow: /admin\n";
        let rules = parse_robots(robots, TOKEN);

        assert!(rules.allows("/docs"));
        assert!(!rules.allows("/admin/users"));
    }

    #[test]
    fn agent_matching_ignores_case() {
        let rules = parse_robots("User-Agent: TDCC-Web-Search\nDisallow: /x\n", TOKEN);

        assert!(!rules.allows("/x"));
        assert!(rules.allows("/y"));
    }

    #[test]
    fn consecutive_user_agent_lines_share_one_rule_body() {
        let robots = "User-agent: other-bot\nUser-agent: tdcc-web-search\nDisallow: /shared\n";
        let rules = parse_robots(robots, TOKEN);

        assert!(!rules.allows("/shared"));
    }

    #[test]
    fn the_longest_matching_rule_wins_and_allow_breaks_a_tie() {
        let robots = "User-agent: *\nDisallow: /docs\nAllow: /docs/public\n";
        let rules = parse_robots(robots, TOKEN);

        assert!(!rules.allows("/docs/secret"));
        assert!(rules.allows("/docs/public/page"));

        let tie = parse_robots("User-agent: *\nDisallow: /a\nAllow: /a\n", TOKEN);
        assert!(tie.allows("/a"));
    }

    #[test]
    fn wildcards_and_end_anchors_are_honoured() {
        let robots = "User-agent: *\nDisallow: /*.pdf$\nDisallow: /tmp/*/cache\n";
        let rules = parse_robots(robots, TOKEN);

        assert!(!rules.allows("/files/report.pdf"));
        assert!(rules.allows("/files/report.pdf.html"));
        assert!(!rules.allows("/tmp/a/cache"));
        assert!(rules.allows("/tmp/a/data"));
        // The anchor binds the tail, not the leftmost occurrence.
        assert!(!rules.allows("/a.pdf/b.pdf"));
    }

    #[test]
    fn query_strings_take_part_in_matching() {
        let rules = parse_robots("User-agent: *\nDisallow: /search?q=\n", TOKEN);

        assert!(!rules.allows("/search?q=cats"));
        assert!(rules.allows("/search"));
    }

    #[test]
    fn comments_and_unknown_fields_are_ignored() {
        let robots = "# a comment\nSitemap: https://example.com/sitemap.xml\n\
                      User-agent: *   # trailing comment\nCrawl-delay: 10\nDisallow: /no\n";
        let rules = parse_robots(robots, TOKEN);

        assert!(!rules.allows("/no"));
        assert!(rules.allows("/yes"));
    }

    #[test]
    fn a_pattern_without_a_leading_slash_is_still_rooted() {
        let rules = parse_robots("User-agent: *\nDisallow: private\n", TOKEN);

        assert!(!rules.allows("/private"));
    }

    #[test]
    fn disallow_all_blocks_everything_and_allow_all_blocks_nothing() {
        assert!(!RobotsRules::disallow_all().allows("/"));
        assert!(!RobotsRules::disallow_all().allows("/anything"));
        assert!(RobotsRules::allow_all().allows("/anything"));
    }

    #[test]
    fn origins_and_request_targets_are_derived_from_the_url() {
        let url = Url::parse("https://example.com:8443/a/b?q=1#frag").unwrap();

        assert_eq!(origin_of(&url), "https://example.com:8443");
        assert_eq!(request_target(&url), "/a/b?q=1");
        assert_eq!(
            origin_of(&Url::parse("http://example.com/x").unwrap()),
            "http://example.com"
        );
    }

    #[test]
    fn the_cache_returns_what_was_stored_and_misses_unknown_origins() {
        let cache = RobotsCache::new();
        cache.put("https://example.com".into(), RobotsRules::disallow_all());

        assert!(cache.get("https://example.com").is_some());
        assert!(!cache.get("https://example.com").unwrap().allows("/a"));
        assert!(cache.get("https://other.example").is_none());
    }
}
