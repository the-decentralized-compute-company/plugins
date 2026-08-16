//! The exporter's own state: what it has collected, how long it took, and how
//! the last attempt went.
//!
//! This is the only place in the plugin that keeps mutable state across
//! scrapes, and it keeps exactly two kinds:
//!
//! - counters for collection attempts, so `tdcc_exporter_collections_total`
//!   behaves like a counter instead of resetting whenever a scrape lands;
//! - a genuine latency histogram of the exporter's own reads of the node API.
//!   It is the one distribution in the exposition that the exporter measures
//!   itself, which is why it is a histogram and the node's request latency is a
//!   summary. See README > "Latency".
//!
//! Nothing here caches node state. Each scrape reads `/api/status` fresh, so a
//! dashboard never shows a value that is older than the scrape that produced
//! it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::node::{self, NodeStatus};
use crate::render::{self, RenderInput};
use crate::settings::Settings;

/// Prometheus' default histogram bounds, in seconds.
///
/// They suit this measurement well: reading `/api/status` over loopback should
/// land in the first two buckets, and anything past 1 s means the node is
/// struggling — which is exactly the signal an operator wants.
pub const DEFAULT_BUCKET_BOUNDS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// A point-in-time copy of the collector's counters, safe to render from.
#[derive(Clone, Debug, PartialEq)]
pub struct CollectorSnapshot {
    pub successes: u64,
    pub failures: u64,
    /// Per-bucket observation counts, **not** cumulative. `render` accumulates
    /// them, because that is where the exposition rules live.
    pub buckets: Vec<(f64, u64)>,
    pub count: u64,
    pub sum_seconds: f64,
}

/// Fixed-bound histogram backed by atomics.
///
/// Durations are accumulated as whole microseconds so the sum can live in an
/// `AtomicU64`; at microsecond resolution a `u64` overflows after about
/// 584,000 years of measured time.
#[derive(Debug)]
pub struct Histogram {
    bounds: &'static [f64],
    counts: Vec<AtomicU64>,
    sum_micros: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    pub fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            counts: bounds.iter().map(|_| AtomicU64::new(0)).collect(),
            sum_micros: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Record one observation. Values above the last bound land only in the
    /// implicit `+Inf` bucket, which `render` derives from `count`.
    pub fn observe(&self, seconds: f64) {
        if !seconds.is_finite() || seconds < 0.0 {
            return;
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_micros
            .fetch_add((seconds * 1e6).round() as u64, Ordering::Relaxed);
        if let Some(index) = self.bounds.iter().position(|bound| seconds <= *bound) {
            self.counts[index].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> (Vec<(f64, u64)>, u64, f64) {
        let buckets = self
            .bounds
            .iter()
            .zip(self.counts.iter())
            .map(|(bound, count)| (*bound, count.load(Ordering::Relaxed)))
            .collect();
        let count = self.count.load(Ordering::Relaxed);
        let sum_seconds = self.sum_micros.load(Ordering::Relaxed) as f64 / 1e6;
        (buckets, count, sum_seconds)
    }
}

/// Result of one scrape, shared by the HTTP route and the MCP tools.
#[derive(Clone, Debug)]
pub struct Scrape {
    /// Complete exposition text, valid whether or not collection succeeded.
    pub body: String,
    pub up: bool,
    /// Present only when collection failed; one readable sentence.
    pub error: Option<String>,
    /// Series produced from node state, excluding the exporter's own.
    pub series: usize,
    pub duration_seconds: f64,
}

pub struct Collector {
    settings: Settings,
    successes: AtomicU64,
    failures: AtomicU64,
    durations: Histogram,
}

impl Collector {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            successes: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            durations: Histogram::new(&DEFAULT_BUCKET_BOUNDS),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// One-line summary for the plugin health check.
    ///
    /// Reads cached counters only. Health must stay fast and independent of
    /// long-running work, and a node API that is slow or down is not a reason
    /// to declare the exporter unhealthy and have the host restart it.
    pub fn health_detail(&self) -> String {
        format!(
            "reading {} — {} collections ok, {} failed",
            self.settings.node.base_url(),
            self.successes.load(Ordering::Relaxed),
            self.failures.load(Ordering::Relaxed),
        )
    }

    /// Collect node state and render a complete exposition.
    ///
    /// Never fails: a node that cannot be read produces a valid exposition with
    /// `tdcc_up 0` and a comment naming the reason, which is what a Prometheus
    /// scrape needs in order to alert on the node being down rather than on the
    /// scrape being broken. Callers that want the failure as an error read
    /// `Scrape::error`.
    pub async fn scrape(&self) -> Scrape {
        let started = Instant::now();
        let collected =
            node::fetch_status(&self.settings.node, self.settings.collect_timeout).await;
        let duration_seconds = started.elapsed().as_secs_f64();
        self.durations.observe(duration_seconds);
        match &collected {
            Ok(_) => self.successes.fetch_add(1, Ordering::Relaxed),
            Err(_) => self.failures.fetch_add(1, Ordering::Relaxed),
        };

        let snapshot = self.snapshot();
        let status: Result<&NodeStatus, &str> = match &collected {
            Ok(status) => Ok(status),
            Err(error) => Err(error.as_str()),
        };
        let body = render::render_exposition(&RenderInput {
            settings: &self.settings,
            exporter_version: crate::EXPORTER_VERSION,
            status,
            collector: &snapshot,
        });

        Scrape {
            series: count_node_series(&body),
            up: collected.is_ok(),
            error: collected.err(),
            body,
            duration_seconds,
        }
    }

    fn snapshot(&self) -> CollectorSnapshot {
        let (buckets, count, sum_seconds) = self.durations.snapshot();
        CollectorSnapshot {
            successes: self.successes.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            buckets,
            count,
            sum_seconds,
        }
    }
}

/// Read `tdcc_exporter_series` back out of the rendered text.
///
/// Taking the number from the exposition rather than recomputing it means the
/// value the tools report and the value Prometheus stores cannot disagree.
fn count_node_series(body: &str) -> usize {
    body.lines()
        .find_map(|line| line.strip_prefix("tdcc_exporter_series "))
        .and_then(|value| value.trim().parse::<f64>().ok())
        .map(|value| value as usize)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::settings_from;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serve one canned `/api/status` response on a loopback port, the way the
    /// node's API does: `Content-Length`, no chunking, connection left for the
    /// client to close.
    ///
    /// This exercises the real socket path — connect, loopback check, request
    /// write, header parse, body read — which the parsing unit tests cannot.
    async fn spawn_node_api(body: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).await.expect("read request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
            }
            assert!(
                String::from_utf8_lossy(&request).starts_with("GET /api/status HTTP/1.1\r\n"),
                "unexpected request: {}",
                String::from_utf8_lossy(&request)
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        port
    }

    #[tokio::test]
    async fn a_live_node_api_produces_node_series() {
        let port = spawn_node_api(
            r#"{"node_id":"node-1","version":"0.72.1","node_state":"serving",
                "is_host":true,"llama_ready":true,"inflight_requests":3,
                "my_vram_gb":24.0,"token":"s3cret",
                "gpus":[{"name":"RTX 4090","vram_bytes":25769803776}],
                "peers":[{"id":"peer-a","role":"host","state":"serving","rtt_ms":12}],
                "routing_metrics":{"request_count":10,"successful_requests":9,
                  "avg_attempt_ms":250.0,"completion_tokens_observed":1234,
                  "local_node":{"local_attempt_count":12}}}"#,
        )
        .await;

        let settings = settings_from(
            &["--node-url".to_string(), format!("http://127.0.0.1:{port}")],
            None,
        )
        .expect("settings parse");
        let scrape = Collector::new(settings).scrape().await;

        assert!(scrape.up, "collection failed: {:?}", scrape.error);
        assert!(scrape.error.is_none());
        assert!(scrape.body.contains("tdcc_up 1"));
        assert!(scrape.body.contains("tdcc_requests_in_flight 3"));
        assert!(scrape.body.contains("tdcc_completion_tokens_total 1234"));
        assert!(
            scrape
                .body
                .contains("tdcc_request_attempt_duration_seconds_sum 3")
        );
        assert!(
            scrape
                .body
                .contains("tdcc_gpu_vram_bytes{gpu=\"0\",name=\"RTX 4090\"}")
        );
        assert!(
            scrape
                .body
                .contains("tdcc_peer_rtt_seconds{peer=\"peer-a\"} 0.012")
        );
        // The fixed node block, minus the throughput gauge this payload has no
        // sample for, plus one GPU series (VRAM only; no reserved or
        // allocatable figure) and four peer series (info, serving, vram, rtt —
        // this peer reports no router latency).
        assert_eq!(scrape.series, crate::render::FIXED_NODE_SERIES - 1 + 1 + 4);
        // The node's API token reached this process and must not leave it.
        assert!(!scrape.body.contains("s3cret"));
    }

    #[tokio::test]
    async fn a_non_200_from_the_node_api_is_reported_not_swallowed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut chunk = [0u8; 1024];
            let _ = stream.read(&mut chunk).await;
            let body = "Runtime status temporarily unavailable";
            let _ = stream
                .write_all(
                    format!(
                        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        });

        let settings = settings_from(
            &["--node-url".to_string(), format!("http://127.0.0.1:{port}")],
            None,
        )
        .expect("settings parse");
        let scrape = Collector::new(settings).scrape().await;

        let error = scrape.error.expect("a 503 is an error");
        assert!(error.contains("HTTP 503"), "{error}");
        assert!(error.contains("temporarily unavailable"), "{error}");
        assert!(scrape.body.contains("tdcc_up 0"));
    }

    #[test]
    fn observations_land_in_the_first_bucket_they_fit() {
        let histogram = Histogram::new(&DEFAULT_BUCKET_BOUNDS);
        histogram.observe(0.003);
        histogram.observe(0.005);
        histogram.observe(0.2);
        let (buckets, count, sum) = histogram.snapshot();
        assert_eq!(buckets[0], (0.005, 2), "0.005 is inclusive of its bound");
        assert_eq!(buckets[5], (0.25, 1));
        assert_eq!(count, 3);
        assert!((sum - 0.208).abs() < 1e-9, "sum was {sum}");
    }

    #[test]
    fn observations_beyond_the_last_bound_only_reach_inf() {
        let histogram = Histogram::new(&DEFAULT_BUCKET_BOUNDS);
        histogram.observe(30.0);
        let (buckets, count, _) = histogram.snapshot();
        assert!(buckets.iter().all(|(_, value)| *value == 0));
        assert_eq!(count, 1, "the +Inf bucket is derived from count");
    }

    #[test]
    fn nonsense_observations_are_ignored_rather_than_poisoning_the_sum() {
        let histogram = Histogram::new(&DEFAULT_BUCKET_BOUNDS);
        histogram.observe(f64::NAN);
        histogram.observe(-1.0);
        histogram.observe(f64::INFINITY);
        let (_, count, sum) = histogram.snapshot();
        assert_eq!(count, 0);
        assert_eq!(sum, 0.0);
    }

    #[test]
    fn series_count_is_read_back_from_the_rendered_text() {
        assert_eq!(count_node_series("tdcc_exporter_series 17\n"), 17);
        assert_eq!(count_node_series("nothing here\n"), 0);
    }

    #[tokio::test]
    async fn an_unreachable_node_still_produces_a_valid_exposition() {
        // Port 1 on loopback has nothing listening, which is the closest thing
        // to a guaranteed connection failure without inventing a fixture.
        let settings = settings_from(
            &[
                "--node-url".to_string(),
                "http://127.0.0.1:1".to_string(),
                "--collect-timeout-ms".to_string(),
                "250".to_string(),
            ],
            None,
        )
        .expect("settings parse");
        let collector = Collector::new(settings);
        let scrape = collector.scrape().await;

        assert!(!scrape.up);
        assert!(scrape.error.is_some(), "the failure must be reported");
        assert!(scrape.body.contains("tdcc_up 0"));
        assert!(scrape.body.contains("# collection failed:"));
        assert!(
            scrape
                .body
                .contains("tdcc_exporter_collections_total{outcome=\"error\"} 1")
        );
        assert_eq!(scrape.series, 0);
        assert!(collector.health_detail().contains("1 failed"));
    }
}
