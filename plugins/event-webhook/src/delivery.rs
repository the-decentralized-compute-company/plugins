//! Getting the POST out, or failing loudly and moving on.
//!
//! Delivery runs on its own task, fed by a bounded channel. The node's mesh
//! event handler only ever does a non-blocking `try_send` into that channel, so
//! a webhook endpoint that has been down for an hour costs the node nothing but
//! a counter.
//!
//! The retry policy is deliberately narrow: retry what a server might recover
//! from, give up immediately on what it will never accept, and never retry more
//! than the configured number of times.

use std::time::Duration;

use reqwest::Client;
use reqwest::header::RETRY_AFTER;
use serde_json::Value;
use tokio::sync::mpsc::Receiver;

use crate::config::{INITIAL_BACKOFF, MAX_BACKOFF, PayloadFormat, Target, scrub};
use crate::event::{Delivery, now_ms};
use crate::stats::Stats;
use crate::{format, logging};

/// Longest snippet of a failure response body kept for the operator. Slack and
/// Discord put the useful part (`invalid_token`, `Missing Permissions`) first.
const MAX_BODY_SNIPPET: usize = 200;

#[derive(Clone, Debug)]
pub struct DeliveryConfig {
    pub target: Target,
    pub format: PayloadFormat,
    pub max_attempts: u32,
    pub max_list_items: usize,
}

/// What happened to one event, after every attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub delivered: bool,
    pub attempts: u32,
    pub status: Option<u16>,
    /// Already scrubbed of the webhook URL.
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// The endpoint accepted it.
    Success,
    /// Worth trying again: overload, rate limiting, or a transient server fault.
    Retryable,
    /// It will be rejected the same way forever; retrying is just noise.
    Permanent,
}

/// Maps an HTTP status onto a retry decision.
///
/// 408, 425, and 429 are the three 4xx codes that mean "later", so they are the
/// three carved out of the otherwise-permanent 4xx range. A wrong or revoked
/// webhook URL returns 401/403/404 and is never retried — hammering it would
/// just look like an attack from the endpoint's side.
pub fn classify(status: u16) -> Disposition {
    match status {
        200..=299 => Disposition::Success,
        408 | 425 | 429 => Disposition::Retryable,
        400..=499 => Disposition::Permanent,
        500..=599 => Disposition::Retryable,
        _ => Disposition::Permanent,
    }
}

/// Exponential backoff with full jitter, bounded by `max`.
///
/// The jitter is seeded rather than drawn from a global RNG so the range is
/// testable and no random-number dependency is needed. The guarantee callers
/// rely on: the result is always within `[capped/2, capped]`.
pub fn backoff_delay(attempt: u32, initial: Duration, max: Duration, seed: u64) -> Duration {
    let exponent = attempt.saturating_sub(1).min(16);
    let base_ms = (initial.as_millis() as u64).max(1);
    let capped = base_ms
        .saturating_mul(1u64 << exponent)
        .min((max.as_millis() as u64).max(1));
    let half = capped / 2;
    let spread = capped - half + 1;
    Duration::from_millis(half + xorshift(seed) % spread)
}

fn xorshift(seed: u64) -> u64 {
    let mut state = seed | 1;
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

/// `Retry-After` in delta-seconds form, clamped so a hostile or confused
/// endpoint cannot park the worker for a day. The HTTP-date form is ignored
/// rather than half-parsed; the backoff schedule takes over in that case.
pub fn parse_retry_after(raw: Option<&str>, max: Duration) -> Option<Duration> {
    let seconds: u64 = raw?.trim().parse().ok()?;
    Some(Duration::from_secs(seconds).min(max))
}

/// Builds the one HTTP client this plugin uses.
///
/// * No redirects: a webhook endpoint that answers 302 is a misconfiguration,
///   and following it would post mesh data somewhere the operator never named.
/// * A fixed timeout, so a hung endpoint cannot pin the worker task.
pub fn build_client(request_timeout: Duration) -> reqwest::Result<Client> {
    Client::builder()
        .timeout(request_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("tdcc-event-webhook/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Posts one payload, retrying according to the policy above.
pub async fn deliver(client: &Client, config: &DeliveryConfig, payload: &Value) -> Outcome {
    let url = &config.target.url;
    let mut last_status = None;
    let mut last_error = None;

    for attempt in 1..=config.max_attempts {
        let (disposition, status, error, retry_after) =
            match client.post(url.clone()).json(payload).send().await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let retry_after = parse_retry_after(
                        response
                            .headers()
                            .get(RETRY_AFTER)
                            .and_then(|value| value.to_str().ok()),
                        MAX_BACKOFF,
                    );
                    let disposition = classify(status);
                    let error = if disposition == Disposition::Success {
                        None
                    } else {
                        let body = response.text().await.unwrap_or_default();
                        Some(describe_http_failure(status, &body, config))
                    };
                    (disposition, Some(status), error, retry_after)
                }
                // `without_url` strips the request URL that reqwest embeds in
                // its Display output; `scrub` is the second line of defence.
                Err(error) => (
                    Disposition::Retryable,
                    None,
                    Some(scrub(&error.without_url().to_string(), url)),
                    None,
                ),
            };

        last_status = status.or(last_status);
        match disposition {
            Disposition::Success => {
                return Outcome {
                    delivered: true,
                    attempts: attempt,
                    status,
                    error: None,
                };
            }
            Disposition::Permanent => {
                return Outcome {
                    delivered: false,
                    attempts: attempt,
                    status,
                    error,
                };
            }
            Disposition::Retryable => {
                last_error = error;
                if attempt == config.max_attempts {
                    break;
                }
                let delay = retry_after.unwrap_or_else(|| {
                    backoff_delay(
                        attempt,
                        INITIAL_BACKOFF,
                        MAX_BACKOFF,
                        now_ms() ^ attempt as u64,
                    )
                });
                tokio::time::sleep(delay).await;
            }
        }
    }

    Outcome {
        delivered: false,
        attempts: config.max_attempts,
        status: last_status,
        error: last_error,
    }
}

fn describe_http_failure(status: u16, body: &str, config: &DeliveryConfig) -> String {
    let snippet = scrub(body.trim(), &config.target.url);
    let snippet = format::truncate_text(&snippet, MAX_BODY_SNIPPET);
    if snippet.is_empty() {
        format!("HTTP {status} from {}", config.target.redacted())
    } else {
        format!("HTTP {status} from {}: {snippet}", config.target.redacted())
    }
}

/// Drains the queue forever, one event at a time.
///
/// Serial on purpose: chat webhooks are rate limited, and a burst of parallel
/// POSTs is the fastest way to get an integration throttled or disabled.
pub async fn run_worker(
    mut queue: Receiver<Delivery>,
    client: Client,
    config: DeliveryConfig,
    stats: std::sync::Arc<Stats>,
) {
    while let Some(job) = queue.recv().await {
        let payload = format::render(config.format, &job, config.max_list_items);
        let outcome = deliver(&client, &config, &payload).await;

        stats.record_retries(u64::from(outcome.attempts.saturating_sub(1)));
        if outcome.delivered {
            stats.record_delivered();
            stats.record_delivery_time(now_ms());
            stats.set_last_error(None);
        } else {
            stats.record_failed();
            let reason = outcome
                .error
                .clone()
                .unwrap_or_else(|| "delivery failed".to_string());
            stats.set_last_error(Some(reason.clone()));
            logging::warn(format!(
                "dropped {} after {} attempt(s): {reason}",
                job.event.kind.as_str(),
                outcome.attempts
            ));
        }
    }
    logging::info("delivery queue closed; worker stopping");
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Url;

    fn config() -> DeliveryConfig {
        DeliveryConfig {
            target: Target {
                url: Url::parse("https://hooks.slack.com/services/T0/B0/XXXXsecretXXXX").unwrap(),
                source: "TDCC_EVENT_WEBHOOK_URL",
            },
            format: PayloadFormat::Slack,
            max_attempts: 5,
            max_list_items: 12,
        }
    }

    #[test]
    fn success_rate_limiting_and_permanent_rejection_are_told_apart() {
        assert_eq!(classify(200), Disposition::Success);
        assert_eq!(classify(204), Disposition::Success);
        assert_eq!(classify(429), Disposition::Retryable);
        assert_eq!(classify(408), Disposition::Retryable);
        assert_eq!(classify(425), Disposition::Retryable);
        assert_eq!(classify(500), Disposition::Retryable);
        assert_eq!(classify(503), Disposition::Retryable);
        // A revoked or mistyped webhook URL: never worth retrying.
        assert_eq!(classify(401), Disposition::Permanent);
        assert_eq!(classify(403), Disposition::Permanent);
        assert_eq!(classify(404), Disposition::Permanent);
        assert_eq!(classify(400), Disposition::Permanent);
    }

    #[test]
    fn backoff_grows_stays_within_its_jitter_band_and_respects_the_cap() {
        let initial = Duration::from_millis(500);
        let max = Duration::from_secs(30);

        for attempt in 1..=10u32 {
            for seed in [1u64, 7, 12_345, u64::MAX] {
                let delay = backoff_delay(attempt, initial, max, seed);
                let uncapped = 500u64.saturating_mul(1 << (attempt - 1).min(16));
                let capped = uncapped.min(30_000);

                assert!(delay.as_millis() as u64 >= capped / 2, "attempt {attempt}");
                assert!(delay.as_millis() as u64 <= capped, "attempt {attempt}");
                assert!(delay <= max);
            }
        }
    }

    #[test]
    fn backoff_does_not_overflow_on_an_absurd_attempt_number() {
        let delay = backoff_delay(u32::MAX, Duration::from_secs(1), Duration::from_secs(30), 5);
        assert!(delay <= Duration::from_secs(30));
    }

    #[test]
    fn retry_after_is_honoured_in_seconds_and_clamped() {
        assert_eq!(
            parse_retry_after(Some("3"), MAX_BACKOFF),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            parse_retry_after(Some("  7 "), MAX_BACKOFF),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            parse_retry_after(Some("86400"), MAX_BACKOFF),
            Some(MAX_BACKOFF)
        );
        // The HTTP-date form is not parsed; the backoff schedule covers it.
        assert_eq!(
            parse_retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT"), MAX_BACKOFF),
            None
        );
        assert_eq!(parse_retry_after(None, MAX_BACKOFF), None);
    }

    #[test]
    fn a_failure_description_never_contains_the_webhook_secret() {
        let config = config();
        let body = "invalid_token for /services/T0/B0/XXXXsecretXXXX";

        let described = describe_http_failure(403, body, &config);

        assert!(described.contains("HTTP 403"));
        assert!(described.contains("invalid_token"));
        assert!(!described.contains("XXXXsecretXXXX"), "{described}");
        assert!(described.contains("https://hooks.slack.com/[redacted]"));
    }

    #[test]
    fn an_empty_failure_body_still_names_the_status_and_the_redacted_target() {
        let described = describe_http_failure(500, "   ", &config());
        assert_eq!(
            described,
            "HTTP 500 from https://hooks.slack.com/[redacted]"
        );
    }

    // ---------------------------------------------------------------------
    // The retry policy against a real socket. A scripted listener answers one
    // status per connection, so "was it retried?" is answered by how many
    // connections it actually served.
    // ---------------------------------------------------------------------

    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Reads exactly one HTTP/1.1 request, so the client finishes writing
    /// before we answer; replying early would race the request body.
    async fn read_one_request(socket: &mut TcpStream) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let Ok(read) = socket.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
            let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buffer.len() >= header_end + 4 + content_length {
                return;
            }
        }
    }

    /// Answers `statuses` in order, one per connection, then stops accepting.
    async fn scripted_server(statuses: Vec<u16>) -> (Target, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local addr");
        let served = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&served);

        tokio::spawn(async move {
            for status in statuses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                read_one_request(&mut socket).await;
                let body = r#"{"detail":"scripted"}"#;
                let response = format!(
                    "HTTP/1.1 {status} Scripted\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let target = crate::config::parse_target(
            &format!("http://{address}/hook"),
            "TDCC_EVENT_WEBHOOK_URL",
            false,
        )
        .expect("loopback http needs no insecure opt-in");
        (target, served)
    }

    fn config_for(target: Target, max_attempts: u32) -> DeliveryConfig {
        DeliveryConfig {
            target,
            format: PayloadFormat::Json,
            max_attempts,
            max_list_items: 12,
        }
    }

    fn test_client() -> Client {
        build_client(Duration::from_secs(5)).expect("client builds")
    }

    #[tokio::test]
    async fn a_successful_post_takes_exactly_one_attempt() {
        let (target, served) = scripted_server(vec![204]).await;

        let outcome = deliver(&test_client(), &config_for(target, 5), &json!({"ok": true})).await;

        assert!(outcome.delivered, "{outcome:?}");
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.status, Some(204));
        assert_eq!(served.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_transient_server_error_is_retried_and_then_succeeds() {
        let (target, served) = scripted_server(vec![503, 200]).await;

        let outcome = deliver(&test_client(), &config_for(target, 5), &json!({"ok": true})).await;

        assert!(outcome.delivered, "{outcome:?}");
        assert_eq!(outcome.attempts, 2);
        assert_eq!(served.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_revoked_webhook_is_not_retried_even_once() {
        let (target, served) = scripted_server(vec![403, 200]).await;

        let outcome = deliver(&test_client(), &config_for(target, 5), &json!({"ok": true})).await;

        assert!(!outcome.delivered, "{outcome:?}");
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.status, Some(403));
        assert_eq!(
            served.load(Ordering::SeqCst),
            1,
            "a permanent rejection must not be hammered"
        );
        assert!(outcome.error.expect("error").contains("HTTP 403"));
    }

    #[tokio::test]
    async fn retries_stop_at_max_attempts_and_report_the_last_failure() {
        let (target, served) = scripted_server(vec![500, 500, 500, 500]).await;

        let outcome = deliver(&test_client(), &config_for(target, 3), &json!({"ok": true})).await;

        assert!(!outcome.delivered, "{outcome:?}");
        assert_eq!(outcome.attempts, 3);
        assert_eq!(
            served.load(Ordering::SeqCst),
            3,
            "no attempt beyond the cap"
        );
        assert!(outcome.error.expect("error").contains("HTTP 500"));
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_fails_with_a_message_free_of_the_url() {
        // Bind then drop, so the port is closed and connections are refused.
        let address = {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr")
        };
        let target = crate::config::parse_target(
            &format!("http://{address}/services/XXXXsecretXXXX"),
            "TDCC_EVENT_WEBHOOK_URL",
            false,
        )
        .expect("valid target");

        let outcome = deliver(&test_client(), &config_for(target, 2), &json!({"ok": true})).await;

        assert!(!outcome.delivered);
        assert_eq!(outcome.attempts, 2, "a refused connection is retryable");
        let error = outcome.error.expect("error");
        assert!(!error.contains("XXXXsecretXXXX"), "{error}");
    }
}
