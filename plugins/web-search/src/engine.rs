//! The two tool implementations, and the only place in the plugin that touches
//! the network.
//!
//! Failure is reported, never swallowed. Every path that cannot produce a real
//! answer — no backend configured, an unauthorised key, a `robots.txt` refusal,
//! a binary response — returns an error naming the cause and, where there is
//! one, the setting that would fix it. An empty result list means the backend
//! genuinely found nothing.

use std::sync::Arc;

use reqwest::{Client, Response, StatusCode, Url};
use serde_json::{Value, json};
use tdcc_plugin::{PluginError, PluginResult};

use crate::config::{Config, SearchBackend, SearchSetup};
use crate::net;
use crate::readable::{self, truncate_chars};
use crate::robots::{self, MAX_ROBOTS_BYTES, RobotsCache, RobotsRules};
use crate::search::{self, SearchHit};

/// Cap on a search backend's JSON response. Generous for a page of results and
/// still bounded, so a misbehaving endpoint cannot stream the process to death.
const MAX_SEARCH_BYTES: usize = 4 * 1_024 * 1_024;

/// Redirect budget when fetching `robots.txt`, per RFC 9309.
const ROBOTS_REDIRECT_LIMIT: u32 = 5;

pub struct Engine {
    client: Client,
    config: Config,
    robots: RobotsCache,
}

impl Engine {
    pub fn new(config: Config) -> Result<Arc<Self>, String> {
        let client = net::build_client(&config.http)
            .map_err(|error| format!("could not build the HTTP client: {error}"))?;
        Ok(Arc::new(Self {
            client,
            config,
            robots: RobotsCache::new(),
        }))
    }

    /// A one-line status for the host's health check.
    ///
    /// Deliberately local: health must stay fast and independent of long-running
    /// work, so this never touches the network.
    pub fn health(&self) -> String {
        match &self.config.search {
            SearchSetup::Configured(backend) => format!("ok; search backend {}", backend.label()),
            SearchSetup::Unconfigured(_) => "ok; fetch only, no search backend configured".into(),
        }
    }

    // -- search ------------------------------------------------------------

    pub async fn search(
        &self,
        query: &str,
        site: Option<&str>,
        count: Option<u32>,
    ) -> PluginResult<Value> {
        let backend = match &self.config.search {
            SearchSetup::Configured(backend) => backend,
            // Not an empty success: the caller is told exactly what is missing.
            SearchSetup::Unconfigured(message) => {
                return Err(PluginError::invalid_request(message.clone()));
            }
        };

        let limits = &self.config.limits;
        let count = count
            .unwrap_or(limits.default_result_count)
            .clamp(1, limits.max_result_count);
        let full_query = search::build_query(query, site).map_err(PluginError::invalid_params)?;

        let hits = match backend {
            SearchBackend::Brave { endpoint, api_key } => {
                self.search_brave(endpoint, api_key, &full_query, count)
                    .await?
            }
            SearchBackend::Searxng { base } => {
                self.search_searxng(base, &full_query, count).await?
            }
        };

        Ok(json!({
            "backend": backend.label(),
            "query": full_query,
            "count": hits.len(),
            "results": hits,
        }))
    }

    async fn search_brave(
        &self,
        endpoint: &Url,
        api_key: &str,
        query: &str,
        count: u32,
    ) -> PluginResult<Vec<SearchHit>> {
        let url = search::brave_request_url(endpoint, query, count);
        let response = self
            .client
            .get(url)
            .header("Accept", "application/json")
            .header("X-Subscription-Token", api_key)
            .send()
            .await
            .map_err(|error| {
                PluginError::internal(format!(
                    "could not reach the Brave Search API at {}: {}",
                    endpoint.as_str(),
                    redact(&error.to_string(), api_key)
                ))
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(PluginError::internal(brave_status_message(status)));
        }
        let body = read_capped(response, MAX_SEARCH_BYTES).await?;
        search::parse_brave_response(&body, count as usize).map_err(PluginError::internal)
    }

    async fn search_searxng(
        &self,
        base: &Url,
        query: &str,
        count: u32,
    ) -> PluginResult<Vec<SearchHit>> {
        let url = search::searxng_request_url(base, query);
        // A self-hosted instance is very often on the operator's own LAN, which
        // is exactly what the fetch guard blocks. That guard protects against a
        // model choosing an internal URL; this URL came from the operator's own
        // configuration, so it is trusted the same way `[[plugin]].url` is.
        let response = self
            .client
            .get(url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|error| {
                PluginError::internal(format!(
                    "could not reach the SearXNG instance at {}: {error}",
                    base.as_str()
                ))
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(PluginError::internal(format!(
                "the SearXNG instance at {} answered {status}. A 403 usually means the instance \
                 limits the JSON format or requires a token; check `search.formats` and \
                 `server.limiter` in its settings.yml.",
                base.as_str()
            )));
        }
        let body = read_capped(response, MAX_SEARCH_BYTES).await?;
        search::parse_searxng_response(&body, count as usize).map_err(PluginError::internal)
    }

    // -- fetch -------------------------------------------------------------

    pub async fn fetch(&self, requested: &str, max_chars: Option<u32>) -> PluginResult<Value> {
        let mut url = Url::parse(requested.trim()).map_err(|error| {
            PluginError::invalid_params(format!("`{requested}` is not a URL: {error}"))
        })?;
        net::check_url_shape(&url).map_err(PluginError::invalid_params)?;

        let mut redirects = 0u32;
        let response = loop {
            net::check_url_destination(&url, self.config.http.allow_private_network)
                .await
                .map_err(PluginError::invalid_params)?;
            self.check_robots(&url).await?;

            let response = self
                .client
                .get(url.clone())
                .header(
                    "Accept",
                    "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5",
                )
                .send()
                .await
                .map_err(|error| {
                    PluginError::internal(format!("could not fetch {url}: {error}"))
                })?;

            let status = response.status();
            if !status.is_redirection() {
                break response;
            }
            let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
            else {
                break response;
            };
            if redirects >= self.config.http.max_redirects {
                return Err(PluginError::internal(format!(
                    "{requested} exceeded the {} redirect limit; last hop was {url}.",
                    self.config.http.max_redirects
                )));
            }
            redirects += 1;
            url = url.join(&location).map_err(|error| {
                PluginError::internal(format!(
                    "{url} redirected to an unusable location `{location}`: {error}"
                ))
            })?;
        };

        let final_url = response.url().clone();
        let status = response.status();
        if !status.is_success() {
            return Err(PluginError::internal(format!(
                "{final_url} answered {status}."
            )));
        }

        let header = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let (media_type, charset) = net::parse_content_type(&header);
        if !net::is_textual_media_type(&media_type) {
            return Err(PluginError::invalid_request(format!(
                "{final_url} is `{media_type}`, which this tool does not convert to text. It \
                 returns readable text from HTML, plain text, and JSON only."
            )));
        }

        let bytes = read_capped(response, self.config.http.max_fetch_bytes).await?;
        let body = net::decode_body(&bytes, charset.as_deref());

        let is_html = media_type.contains("html")
            || media_type.contains("xml")
            || (media_type.is_empty() && looks_like_html(&body));
        let extracted = if is_html {
            readable::extract_readable(&body)
        } else {
            readable::Readable {
                title: None,
                text: body.trim().to_string(),
            }
        };

        let budget = max_chars
            .map(|max| max as usize)
            .unwrap_or(self.config.limits.max_text_chars)
            .clamp(200, self.config.limits.max_text_chars);
        let (text, truncated) = truncate_chars(&extracted.text, budget);

        Ok(json!({
            "url": requested,
            "final_url": final_url.as_str(),
            "status": status.as_u16(),
            "content_type": media_type,
            "title": extracted.title,
            "text": text,
            "chars": text.chars().count(),
            "truncated": truncated,
            "redirects": redirects,
            "robots_checked": self.config.http.respect_robots,
        }))
    }

    /// Refuse a fetch the site has asked crawlers not to make.
    ///
    /// A `robots.txt` that cannot be read is treated as a refusal, per RFC 9309:
    /// the alternative is deciding that an unreachable site has consented.
    async fn check_robots(&self, url: &Url) -> PluginResult<()> {
        if !self.config.http.respect_robots {
            return Ok(());
        }
        let origin = robots::origin_of(url);
        let rules = match self.robots.get(&origin) {
            Some(rules) => rules,
            None => {
                let rules = self.load_robots(&origin).await;
                self.robots.put(origin.clone(), rules.clone());
                rules
            }
        };

        if rules.allows(&robots::request_target(url)) {
            Ok(())
        } else {
            Err(PluginError::invalid_request(format!(
                "{origin}/robots.txt disallows {} for `{}`. web-search honours robots.txt because \
                 the request goes out from the operator's address, not the model's.",
                robots::request_target(url),
                crate::config::PRODUCT_TOKEN
            )))
        }
    }

    async fn load_robots(&self, origin: &str) -> RobotsRules {
        let Ok(mut url) = Url::parse(&format!("{origin}/robots.txt")) else {
            return RobotsRules::disallow_all();
        };

        // RFC 9309 asks for at least five redirects to be followed when
        // fetching robots.txt, and plenty of sites answer it only after an
        // apex-to-www or http-to-https hop. Treating those as "unreadable"
        // would refuse most of the web.
        for _ in 0..=ROBOTS_REDIRECT_LIMIT {
            if net::check_url_destination(&url, self.config.http.allow_private_network)
                .await
                .is_err()
            {
                return RobotsRules::disallow_all();
            }
            let Ok(response) = self.client.get(url.clone()).send().await else {
                return RobotsRules::disallow_all();
            };
            let status = response.status();

            if status.is_redirection() {
                let next = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|location| url.join(location).ok());
                match next {
                    Some(next) => {
                        url = next;
                        continue;
                    }
                    None => return RobotsRules::disallow_all(),
                }
            }
            // 4xx means "no rules published", which is consent by absence. 5xx
            // and anything unreadable mean "unknown", which is not.
            if status.is_client_error() {
                return RobotsRules::allow_all();
            }
            if !status.is_success() {
                return RobotsRules::disallow_all();
            }
            return match read_capped(response, MAX_ROBOTS_BYTES).await {
                Ok(bytes) => robots::parse_robots(
                    &String::from_utf8_lossy(&bytes),
                    crate::config::PRODUCT_TOKEN,
                ),
                Err(_) => RobotsRules::disallow_all(),
            };
        }
        RobotsRules::disallow_all()
    }
}

/// Read a body, stopping at `limit` bytes.
///
/// `Content-Length` is checked first so an obviously oversized response is
/// rejected before it is transferred, but the chunk loop is what actually
/// enforces the cap: a chunked response has no length to check.
async fn read_capped(response: Response, limit: usize) -> PluginResult<Vec<u8>> {
    if let Some(length) = response.content_length()
        && length > limit as u64
    {
        return Err(PluginError::invalid_request(format!(
            "{} is {length} bytes, over the {limit}-byte limit for one fetch.",
            response.url()
        )));
    }

    let url = response.url().clone();
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| PluginError::internal(format!("reading {url} failed: {error}")))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > limit {
            return Err(PluginError::invalid_request(format!(
                "{url} exceeded the {limit}-byte limit for one fetch; nothing was returned rather \
                 than a truncated document."
            )));
        }
    }
    Ok(body)
}

fn brave_status_message(status: StatusCode) -> String {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => format!(
            "the Brave Search API rejected the credentials ({status}). Check \
             {} in the environment of the tdcc process.",
            crate::config::ENV_BRAVE_API_KEY
        ),
        StatusCode::TOO_MANY_REQUESTS => {
            "the Brave Search API rate-limited this node (429). Retry later or raise the plan \
             limit on the subscription."
                .to_string()
        }
        other => format!("the Brave Search API answered {other}."),
    }
}

/// Belt and braces: a transport error should never quote a URL that carried a
/// key, but if one ever does, it does not carry the key.
fn redact(message: &str, secret: &str) -> String {
    if secret.is_empty() {
        return message.to_string();
    }
    message.replace(secret, "<redacted>")
}

fn looks_like_html(body: &str) -> bool {
    let head = body.trim_start();
    head.len() >= 5
        && (head[..head.len().min(512)]
            .to_ascii_lowercase()
            .contains("<html")
            || head.starts_with("<!DOCTYPE")
            || head.starts_with("<!doctype"))
}

/// A one-connection-per-request HTTP server on loopback, used to exercise the
/// real request path without depending on anybody else's uptime.
#[cfg(test)]
mod stub {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// `(request target, status, content type, body)`.
    pub type Route = (&'static str, u16, &'static str, String);

    /// Start the server and return its base URL. It stops when the test
    /// process ends; there is nothing to tear down.
    pub async fn start(routes: Vec<Route>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let port = listener.local_addr().expect("local address").port();
        let routes = Arc::new(routes);

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let routes = Arc::clone(&routes);
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 8 * 1_024];
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let target = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();

                    let (status, content_type, body) = routes
                        .iter()
                        .find(|(path, ..)| *path == target)
                        .map(|(_, status, content_type, body)| {
                            (*status, *content_type, body.clone())
                        })
                        .unwrap_or((404, "text/plain", "not found".to_string()));

                    // Redirect bodies carry their target in the body slot.
                    let location = if (300..400).contains(&status) {
                        format!("Location: {body}\r\n")
                    } else {
                        String::new()
                    };
                    let payload = if location.is_empty() {
                        body
                    } else {
                        String::new()
                    };
                    let response = format!(
                        "HTTP/1.1 {status} Stub\r\nContent-Type: {content_type}\r\n{location}\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        format!("http://127.0.0.1:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config that trusts loopback, so the stub server is reachable.
    fn local_config(extra: &[&str]) -> Config {
        let mut args = vec!["--allow-private-network".to_string()];
        args.extend(extra.iter().map(|value| (*value).to_string()));
        Config::parse(&args, &Default::default()).expect("parses")
    }

    fn searxng_engine(base: &str) -> Arc<Engine> {
        let config = Config::parse(
            &["--searxng-url".to_string(), base.to_string()],
            &Default::default(),
        )
        .expect("parses");
        Engine::new(config).expect("client builds")
    }

    #[tokio::test]
    async fn a_searxng_instance_is_queried_and_its_results_are_ranked() {
        let base = stub::start(vec![(
            "/search?q=rust+ownership&format=json&pageno=1",
            200,
            "application/json",
            r#"{"query":"rust ownership","results":[
                {"url":"https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html",
                 "title":"Understanding <strong>Ownership</strong>","content":"Ownership &amp; borrowing"},
                {"url":"https://example.com/dup","title":"Dup","content":"one"},
                {"url":"https://example.com/dup","title":"Dup again","content":"two"}
            ]}"#
            .to_string(),
        )])
        .await;

        let result = searxng_engine(&base)
            .search("rust ownership", None, None)
            .await
            .expect("the stub answers with JSON");

        assert_eq!(result["backend"], "searxng");
        assert_eq!(result["count"], 2, "{result:#}");
        assert_eq!(result["results"][0]["title"], "Understanding Ownership");
        assert_eq!(result["results"][0]["snippet"], "Ownership & borrowing");
        assert_eq!(result["results"][1]["rank"], 2);
    }

    #[tokio::test]
    async fn a_searxng_instance_without_json_enabled_explains_itself() {
        let base = stub::start(vec![(
            "/search?q=x&format=json&pageno=1",
            200,
            "text/html",
            "<!doctype html><html><body>search page</body></html>".to_string(),
        )])
        .await;

        let error = searxng_engine(&base)
            .search("x", None, None)
            .await
            .expect_err("an HTML body is not a result set");

        assert!(
            error.message.contains("search.formats"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn a_site_restriction_reaches_the_backend_as_a_site_operator() {
        let base = stub::start(vec![(
            "/search?q=cats+site%3Aexample.com&format=json&pageno=1",
            200,
            "application/json",
            r#"{"results":[{"url":"https://example.com/cats","title":"Cats","content":"c"}]}"#
                .to_string(),
        )])
        .await;

        let result = searxng_engine(&base)
            .search("cats", Some("example.com"), None)
            .await
            .expect("the stub only answers the site-restricted target");

        assert_eq!(result["query"], "cats site:example.com");
        assert_eq!(result["count"], 1);
    }

    #[tokio::test]
    async fn a_full_page_fetch_strips_chrome_and_reports_its_metadata() {
        let base = stub::start(vec![
            (
                "/robots.txt",
                200,
                "text/plain",
                "User-agent: *\nDisallow: /private\n".to_string(),
            ),
            (
                "/article",
                200,
                "text/html; charset=utf-8",
                "<html><head><title>Stub &amp; Co</title></head><body>\
                 <nav>Home About Contact</nav>\
                 <script>tracker('x')</script>\
                 <div class=\"cookie-banner\">Accept cookies?</div>\
                 <main><h1>Heading</h1><p>The body of the article.</p></main>\
                 <footer>Legal</footer></body></html>"
                    .to_string(),
            ),
        ])
        .await;
        let engine = Engine::new(local_config(&[])).expect("client builds");

        let page = engine
            .fetch(&format!("{base}/article"), None)
            .await
            .expect("the stub page is allowed by its robots.txt");

        assert_eq!(page["status"], 200);
        assert_eq!(page["title"], "Stub & Co");
        assert_eq!(page["text"], "# Heading\n\nThe body of the article.");
        assert_eq!(page["truncated"], false);
        assert_eq!(page["robots_checked"], true);
    }

    #[tokio::test]
    async fn a_path_disallowed_by_robots_is_refused_rather_than_fetched() {
        let base = stub::start(vec![
            (
                "/robots.txt",
                200,
                "text/plain",
                "User-agent: *\nDisallow: /private\n".to_string(),
            ),
            (
                "/private/report",
                200,
                "text/html",
                "<p>secret</p>".to_string(),
            ),
        ])
        .await;
        let engine = Engine::new(local_config(&[])).expect("client builds");

        let error = engine
            .fetch(&format!("{base}/private/report"), None)
            .await
            .expect_err("robots.txt disallows this path");

        assert!(error.message.contains("robots.txt"), "{}", error.message);
        assert!(
            error.message.contains(crate::config::PRODUCT_TOKEN),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn ignoring_robots_is_possible_but_never_the_default() {
        let base = stub::start(vec![
            (
                "/robots.txt",
                200,
                "text/plain",
                "User-agent: *\nDisallow: /\n".to_string(),
            ),
            (
                "/page",
                200,
                "text/html",
                "<p>Content anyway.</p>".to_string(),
            ),
        ])
        .await;

        let engine = Engine::new(local_config(&["--ignore-robots"])).expect("client builds");
        let page = engine
            .fetch(&format!("{base}/page"), None)
            .await
            .expect("the guard was explicitly turned off");
        assert_eq!(page["text"], "Content anyway.");
        assert_eq!(page["robots_checked"], false);
    }

    #[tokio::test]
    async fn redirects_are_followed_and_counted_up_to_the_configured_budget() {
        let base = stub::start(vec![
            ("/robots.txt", 404, "text/plain", String::new()),
            ("/start", 302, "text/plain", "/middle".to_string()),
            ("/middle", 302, "text/plain", "/end".to_string()),
            ("/end", 200, "text/html", "<p>Arrived.</p>".to_string()),
            ("/loop", 302, "text/plain", "/loop".to_string()),
        ])
        .await;
        let engine = Engine::new(local_config(&[])).expect("client builds");

        let page = engine
            .fetch(&format!("{base}/start"), None)
            .await
            .expect("two hops are within the budget");
        assert_eq!(page["text"], "Arrived.");
        assert_eq!(page["redirects"], 2);
        assert_eq!(page["final_url"], format!("{base}/end"));

        let engine = Engine::new(local_config(&["--max-redirects", "2"])).expect("client builds");
        let error = engine
            .fetch(&format!("{base}/loop"), None)
            .await
            .expect_err("a redirect loop must terminate");
        assert!(
            error.message.contains("redirect limit"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused_rather_than_silently_truncated() {
        let base = stub::start(vec![
            ("/robots.txt", 404, "text/plain", String::new()),
            ("/big", 200, "text/html", "x".repeat(5_000)),
        ])
        .await;
        let engine =
            Engine::new(local_config(&["--max-fetch-bytes", "1024"])).expect("client builds");

        let error = engine
            .fetch(&format!("{base}/big"), None)
            .await
            .expect_err("the body is over the cap");

        assert!(error.message.contains("1024"), "{}", error.message);
    }

    #[tokio::test]
    async fn a_binary_media_type_is_refused_with_the_type_named() {
        let base = stub::start(vec![
            ("/robots.txt", 404, "text/plain", String::new()),
            ("/report", 200, "application/pdf", "%PDF-1.7".to_string()),
        ])
        .await;
        let engine = Engine::new(local_config(&[])).expect("client builds");

        let error = engine
            .fetch(&format!("{base}/report"), None)
            .await
            .expect_err("PDFs are not converted to text");

        assert!(
            error.message.contains("application/pdf"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn an_error_status_is_reported_and_not_returned_as_empty_text() {
        let base = stub::start(vec![
            ("/robots.txt", 404, "text/plain", String::new()),
            ("/gone", 404, "text/html", "<p>nope</p>".to_string()),
        ])
        .await;
        let engine = Engine::new(local_config(&[])).expect("client builds");

        let error = engine
            .fetch(&format!("{base}/gone"), None)
            .await
            .expect_err("a 404 is a failed fetch");

        assert!(error.message.contains("404"), "{}", error.message);
    }

    #[tokio::test]
    async fn max_chars_truncates_and_says_so() {
        let base = stub::start(vec![
            ("/robots.txt", 404, "text/plain", String::new()),
            (
                "/long",
                200,
                "text/html",
                format!("<p>{}</p>", "word ".repeat(400)),
            ),
        ])
        .await;
        let engine = Engine::new(local_config(&[])).expect("client builds");

        let page = engine
            .fetch(&format!("{base}/long"), Some(300))
            .await
            .expect("the page is fetchable");

        assert_eq!(page["truncated"], true);
        assert!(
            page["chars"].as_u64().unwrap_or_default() <= 300,
            "{page:#}"
        );
    }

    #[test]
    fn brave_status_messages_name_the_setting_that_fixes_them() {
        let message = brave_status_message(StatusCode::UNAUTHORIZED);
        assert!(
            message.contains(crate::config::ENV_BRAVE_API_KEY),
            "{message}"
        );

        let message = brave_status_message(StatusCode::TOO_MANY_REQUESTS);
        assert!(message.contains("429"), "{message}");

        let message = brave_status_message(StatusCode::BAD_GATEWAY);
        assert!(message.contains("502"), "{message}");
    }

    #[test]
    fn redaction_removes_a_key_that_leaked_into_a_transport_error() {
        let message = redact("error sending request for url (…?token=abc123)", "abc123");

        assert!(!message.contains("abc123"), "{message}");
        assert!(message.contains("<redacted>"), "{message}");
        assert_eq!(redact("no secret here", ""), "no secret here");
    }

    #[test]
    fn html_sniffing_only_fires_on_documents_that_declare_themselves() {
        assert!(looks_like_html("<!DOCTYPE html><html><body>hi"));
        assert!(looks_like_html("\n  <html lang=\"en\">"));
        assert!(!looks_like_html("{\"a\": 1}"));
        assert!(!looks_like_html("plain sentence"));
        assert!(!looks_like_html(""));
    }

    #[test]
    fn health_reports_whether_search_is_usable_without_touching_the_network() {
        let config = Config::parse(
            &[],
            &[(
                crate::config::ENV_SEARXNG_URL.to_string(),
                "https://searx.example".to_string(),
            )]
            .into_iter()
            .collect(),
        )
        .expect("parses");
        let engine = Engine::new(config).expect("client builds");
        assert!(engine.health().contains("searxng"), "{}", engine.health());

        let engine = Engine::new(Config::parse(&[], &Default::default()).expect("parses"))
            .expect("client builds");
        assert!(
            engine.health().contains("fetch only"),
            "{}",
            engine.health()
        );
    }

    #[tokio::test]
    async fn search_without_a_backend_returns_the_configuration_error() {
        let engine = Engine::new(Config::parse(&[], &Default::default()).expect("parses"))
            .expect("client builds");

        let error = engine
            .search("anything", None, None)
            .await
            .expect_err("an unconfigured backend cannot search");

        assert!(
            error.message.contains(crate::config::ENV_BRAVE_API_KEY),
            "{}",
            error.message
        );
    }

    /// Exercises the real pipeline — DNS guard, robots.txt fetch and parse,
    /// redirect following, size cap, readable extraction — against live sites.
    ///
    /// Ignored by default: it depends on third parties being up, and a test
    /// suite that fails because somebody else's CDN blipped is a test suite
    /// people stop trusting. Run it with `cargo test -- --ignored --nocapture`
    /// after changing anything in the fetch path.
    #[tokio::test]
    #[ignore = "makes real outbound requests"]
    async fn fetch_reads_live_pages_end_to_end() {
        let engine = Engine::new(Config::parse(&[], &Default::default()).expect("parses"))
            .expect("client builds");

        // A site with no robots.txt at all (404 = consent by absence).
        let page = engine
            .fetch("https://example.com/", None)
            .await
            .expect("example.com is fetchable");
        assert_eq!(page["status"], 200);
        assert_eq!(page["title"], "Example Domain");
        assert!(
            page["text"]
                .as_str()
                .unwrap_or_default()
                .starts_with("# Example Domain"),
            "{page:#}"
        );

        // A site whose robots.txt is only reachable through a redirect, and
        // whose markup is a real template with navigation and a footer.
        let page = engine
            .fetch("https://www.rust-lang.org/", Some(4_000))
            .await
            .expect("rust-lang.org is fetchable");
        let text = page["text"].as_str().unwrap_or_default();
        assert!(!text.is_empty(), "{page:#}");
        assert!(
            !text.contains("<div"),
            "markup leaked into the text: {text}"
        );
        println!("--- title: {:?}", page["title"]);
        println!("--- text:\n{text}");
    }

    #[tokio::test]
    async fn fetch_refuses_non_http_schemes_and_loopback_targets_before_any_request() {
        let engine = Engine::new(Config::parse(&[], &Default::default()).expect("parses"))
            .expect("client builds");

        let error = engine
            .fetch("file:///etc/passwd", None)
            .await
            .expect_err("file URLs are refused");
        assert!(error.message.contains("scheme"), "{}", error.message);

        let error = engine
            .fetch("http://127.0.0.1:9337/v1/models", None)
            .await
            .expect_err("loopback is refused");
        assert!(
            error.message.contains("--allow-private-network"),
            "{}",
            error.message
        );
    }
}
