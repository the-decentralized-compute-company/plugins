//! Backend request construction and response parsing.
//!
//! Two backends ship, and they exist for two different audiences:
//!
//! * **Brave Search API** — a hosted, key-based provider. Queries leave the
//!   machine and reach Brave.
//! * **SearXNG** — a self-hosted metasearch instance. People who run private
//!   compute frequently do not want their queries going to a third party, and
//!   this is the option for them.
//!
//! Everything in this module is pure: URLs in, URLs out; bytes in, results out.
//! The network lives in `engine.rs`, which keeps every response shape covered
//! by ordinary unit tests.

use reqwest::Url;
use serde::{Deserialize, Serialize};

/// One ranked result, in the shape the tool returns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchHit {
    pub rank: u32,
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Combine the user's query with an optional `site:` restriction.
///
/// Both backends understand `site:` in the query text, so one code path covers
/// them. The domain is validated rather than interpolated blindly — a value
/// with whitespace in it would silently become extra query terms.
pub fn build_query(query: &str, site: Option<&str>) -> Result<String, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("`query` must not be empty.".to_string());
    }
    let Some(site) = site.map(str::trim).filter(|site| !site.is_empty()) else {
        return Ok(query.to_string());
    };
    let site = site
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    let plausible = !site.is_empty()
        && site.len() <= 253
        && site.contains('.')
        && site
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-'));
    if !plausible {
        return Err(format!(
            "`site` must be a bare domain such as `example.com`, got `{site}`."
        ));
    }
    Ok(format!("{query} site:{site}"))
}

/// The Brave Search API request URL.
pub fn brave_request_url(endpoint: &Url, query: &str, count: u32) -> Url {
    let mut url = endpoint.clone();
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("count", &count.to_string());
    url
}

/// The SearXNG request URL, from an instance base such as
/// `https://searx.example` or `https://host/searxng`.
pub fn searxng_request_url(base: &Url, query: &str) -> Url {
    let mut url = append_path(base, "search");
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("format", "json")
        .append_pair("pageno", "1");
    url
}

/// Append a path segment, keeping any prefix the instance is mounted under.
///
/// `Url::join` would discard the last segment of a base without a trailing
/// slash, turning `https://host/searxng` into `https://host/search`.
fn append_path(base: &Url, segment: &str) -> Url {
    let mut url = base.clone();
    let path = base.path().trim_end_matches('/');
    url.set_path(&format!("{path}/{segment}"));
    url.set_query(None);
    url.set_fragment(None);
    url
}

#[derive(Debug, Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Debug, Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Debug, Deserialize)]
struct BraveResult {
    #[serde(default)]
    title: String,
    url: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Debug, Deserialize)]
struct SearxngResult {
    #[serde(default)]
    title: String,
    url: String,
    #[serde(default)]
    content: String,
}

/// Parse a Brave Search API response body.
pub fn parse_brave_response(body: &[u8], limit: usize) -> Result<Vec<SearchHit>, String> {
    let parsed: BraveResponse = serde_json::from_slice(body).map_err(|error| {
        format!("the Brave Search API returned a body this plugin could not parse: {error}")
    })?;
    let results = parsed.web.map(|web| web.results).unwrap_or_default();
    Ok(rank(
        results
            .into_iter()
            .map(|result| (result.title, result.url, result.description)),
        limit,
    ))
}

/// Parse a SearXNG JSON response body.
pub fn parse_searxng_response(body: &[u8], limit: usize) -> Result<Vec<SearchHit>, String> {
    let parsed: SearxngResponse = serde_json::from_slice(body).map_err(|error| {
        format!(
            "the SearXNG instance returned a body this plugin could not parse: {error}. Instances \
             only emit JSON when `json` is listed under `search.formats` in settings.yml; an \
             instance with the default HTML-only configuration answers with a web page."
        )
    })?;
    Ok(rank(
        parsed
            .results
            .into_iter()
            .map(|result| (result.title, result.url, result.content)),
        limit,
    ))
}

/// Normalise, de-duplicate by URL, and number the results.
///
/// Metasearch backends legitimately return the same URL from several engines,
/// and a model paying context for three copies of one page is a waste.
fn rank(results: impl Iterator<Item = (String, String, String)>, limit: usize) -> Vec<SearchHit> {
    let mut seen: Vec<String> = Vec::new();
    let mut hits = Vec::new();
    for (title, url, snippet) in results {
        if hits.len() >= limit {
            break;
        }
        let url = url.trim().to_string();
        if url.is_empty() || seen.iter().any(|existing| existing == &url) {
            continue;
        }
        seen.push(url.clone());
        hits.push(SearchHit {
            rank: hits.len() as u32 + 1,
            title: clean(&title),
            url,
            snippet: clean(&snippet),
        });
    }
    hits
}

/// Backends return snippets with `<strong>` highlighting and entity escapes.
fn clean(text: &str) -> String {
    let stripped = strip_tags(text);
    crate::readable::decode_entities(&stripped)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for character in text.chars() {
        match character {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(character),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("test URL parses")
    }

    #[test]
    fn a_bare_query_passes_through_and_an_empty_one_is_rejected() {
        assert_eq!(build_query("  rust async  ", None).unwrap(), "rust async");
        assert!(build_query("   ", None).is_err());
    }

    #[test]
    fn a_site_restriction_becomes_a_site_operator() {
        assert_eq!(
            build_query("borrow checker", Some("doc.rust-lang.org")).unwrap(),
            "borrow checker site:doc.rust-lang.org"
        );
        // A pasted URL is tolerated and reduced to its host.
        assert_eq!(
            build_query("x", Some("https://example.com/")).unwrap(),
            "x site:example.com"
        );
    }

    #[test]
    fn an_implausible_site_value_is_refused_rather_than_smuggled_into_the_query() {
        for site in ["not a domain", "example", "exa mple.com", "a.com OR b"] {
            assert!(
                build_query("x", Some(site)).is_err(),
                "`{site}` should be refused"
            );
        }
    }

    #[test]
    fn the_brave_request_carries_the_query_and_count() {
        let request = brave_request_url(&url(crate::config::DEFAULT_BRAVE_ENDPOINT), "a b", 5);

        let pairs: Vec<(String, String)> = request
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert!(pairs.contains(&("q".into(), "a b".into())), "{pairs:?}");
        assert!(pairs.contains(&("count".into(), "5".into())), "{pairs:?}");
    }

    #[test]
    fn the_searxng_request_asks_for_json_and_keeps_any_mount_prefix() {
        assert_eq!(
            searxng_request_url(&url("https://searx.example"), "cats").as_str(),
            "https://searx.example/search?q=cats&format=json&pageno=1"
        );
        assert_eq!(
            searxng_request_url(&url("https://host/searxng"), "cats").path(),
            "/searxng/search"
        );
        assert_eq!(
            searxng_request_url(&url("https://host/searxng/"), "cats").path(),
            "/searxng/search"
        );
    }

    #[test]
    fn a_brave_body_becomes_ranked_hits() {
        let body = br#"{
            "web": { "results": [
                {"title": "First <strong>hit</strong>", "url": "https://a.example/1",
                 "description": "A &amp; B"},
                {"title": "Second", "url": "https://b.example/2", "description": "Two"}
            ]}
        }"#;

        let hits = parse_brave_response(body, 10).expect("parses");

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].rank, 1);
        assert_eq!(hits[0].title, "First hit");
        assert_eq!(hits[0].snippet, "A & B");
        assert_eq!(hits[1].url, "https://b.example/2");
    }

    #[test]
    fn a_searxng_body_becomes_ranked_hits() {
        let body = br#"{"query":"cats","results":[
            {"url":"https://a.example/","title":"Cats","content":"About cats","engine":"duckduckgo"}
        ]}"#;

        let hits = parse_searxng_response(body, 10).expect("parses");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Cats");
        assert_eq!(hits[0].snippet, "About cats");
    }

    #[test]
    fn duplicate_urls_collapse_and_the_limit_is_respected() {
        let body = br#"{"results":[
            {"url":"https://a.example/","title":"One","content":"x"},
            {"url":"https://a.example/","title":"One again","content":"x"},
            {"url":"https://b.example/","title":"Two","content":"y"},
            {"url":"https://c.example/","title":"Three","content":"z"}
        ]}"#;

        let hits = parse_searxng_response(body, 2).expect("parses");

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://a.example/");
        assert_eq!(hits[1].url, "https://b.example/");
        assert_eq!(hits[1].rank, 2);
    }

    #[test]
    fn a_result_without_a_url_is_skipped_rather_than_returned_blank() {
        let body = br#"{"results":[{"url":"   ","title":"No link","content":"x"},
                                    {"url":"https://a.example/","title":"Real","content":"y"}]}"#;

        let hits = parse_searxng_response(body, 10).expect("parses");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Real");
    }

    #[test]
    fn zero_results_is_an_honest_empty_list_not_an_error() {
        assert_eq!(
            parse_brave_response(b"{\"web\":{\"results\":[]}}", 10).unwrap(),
            Vec::new()
        );
        assert_eq!(parse_brave_response(b"{}", 10).unwrap(), Vec::new());
        assert_eq!(
            parse_searxng_response(b"{\"results\":[]}", 10).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn an_html_body_from_a_json_disabled_instance_explains_the_prerequisite() {
        let error = parse_searxng_response(b"<!doctype html><html>", 10)
            .expect_err("HTML is not a JSON response");

        assert!(error.contains("search.formats"), "{error}");
    }
}
