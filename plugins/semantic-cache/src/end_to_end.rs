//! End-to-end tests against a stub embeddings server.
//!
//! The unit tests elsewhere cover the rules in isolation. These cover the
//! thing the plugin actually claims: store one answer, ask for it in different
//! words, get it back, and see the saving show up in `stats`.
//!
//! The stub is a real HTTP server on loopback speaking the OpenAI embeddings
//! shape, not a mock of the client. Its "embeddings" are a two-dimensional toy
//! — each prompt is placed on the unit circle at an angle chosen by a keyword
//! — so the *geometry* is controlled and the assertions below are about the
//! cache's behaviour, never about how good any real embedder is. Nothing here
//! is a benchmark or a quality measurement of anything.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::{Config, resolve_embeddings_url};
use crate::keying::ChatMessage;
use crate::manifest::{AppState, LookupArgs, StoreArgs, handle_lookup, handle_stats, handle_store};

/// What the stub returns for a request body, and with what status.
#[derive(Clone, Copy)]
enum StubBehaviour {
    /// Answer with an embedding derived from the prompt text.
    Embed,
    /// Answer 503 with an OpenAI-shaped error body.
    Unavailable,
}

/// Place a prompt on the unit circle by keyword.
///
/// Two prompts about the same topic land within 0.02 radians of each other
/// (cosine > 0.999), and "passphrase" sits 0.35 radians away from "password"
/// — close, but below the conservative 0.95 default, which is exactly the
/// case the default exists to refuse.
fn angle_for(text: &str) -> f64 {
    let lowered = text.to_ascii_lowercase();
    let base = if lowered.contains("passphrase") {
        0.35
    } else if lowered.contains("password") {
        0.0
    } else if lowered.contains("france") || lowered.contains("french") {
        std::f64::consts::FRAC_PI_2
    } else {
        std::f64::consts::PI
    };
    // Deterministic jitter so two wordings of one topic are near but not
    // identical vectors — an exact-vector match would prove nothing.
    base + 0.002 * ((text.len() % 10) as f64)
}

fn embedding_for(text: &str) -> String {
    let angle = angle_for(text);
    format!(
        r#"{{"object":"list","data":[{{"object":"embedding","index":0,"embedding":[{},{}]}}]}}"#,
        angle.cos(),
        angle.sin()
    )
}

/// Start a stub embeddings server on an ephemeral loopback port.
///
/// Returns the base URL. The task lives as long as the test runtime.
async fn start_stub(behaviour: StubBehaviour) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback port available");
    let address = listener.local_addr().expect("bound address");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = serve_one(stream, behaviour).await;
            });
        }
    });
    format!("http://{address}/v1")
}

/// Read one HTTP/1.1 request, answer it, and close.
///
/// `Connection: close` keeps this to a dozen lines: no keep-alive, no chunked
/// encoding, no pipelining.
async fn serve_one(mut stream: TcpStream, behaviour: StubBehaviour) -> std::io::Result<()> {
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 1024];
    let mut body_offset = None;
    let mut content_length = 0_usize;

    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);

        if body_offset.is_none()
            && let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let headers = String::from_utf8_lossy(&raw[..position]).to_ascii_lowercase();
            content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            body_offset = Some(position + 4);
        }
        if let Some(offset) = body_offset
            && raw.len() >= offset + content_length
        {
            break;
        }
    }

    let body = body_offset
        .map(|offset| String::from_utf8_lossy(&raw[offset..]).to_string())
        .unwrap_or_default();

    let (status, payload) = match behaviour {
        StubBehaviour::Unavailable => (
            "503 Service Unavailable",
            r#"{"error":{"message":"model is loading"}}"#.to_string(),
        ),
        StubBehaviour::Embed => {
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let text = parsed
                .pointer("/input/0")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            ("200 OK", embedding_for(&text))
        }
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

async fn state_against(base_url: &str) -> Arc<AppState> {
    let config = Config {
        embeddings_url: resolve_embeddings_url(base_url).expect("stub URL is valid"),
        embedding_model: "stub-embedder".to_string(),
        request_timeout: std::time::Duration::from_secs(5),
        ..Config::default()
    };
    Arc::new(AppState::new(config).expect("state builds"))
}

fn user(content: &str) -> ChatMessage {
    ChatMessage {
        role: "user".to_string(),
        content: content.to_string(),
    }
}

fn system(content: &str) -> ChatMessage {
    ChatMessage {
        role: "system".to_string(),
        content: content.to_string(),
    }
}

fn lookup(model: &str, messages: Vec<ChatMessage>) -> LookupArgs {
    LookupArgs {
        model: model.to_string(),
        messages,
        temperature: Some(0.0),
        top_p: None,
        tools: None,
        extra_key: None,
        min_similarity: None,
    }
}

fn store(model: &str, messages: Vec<ChatMessage>, completion: &str) -> StoreArgs {
    StoreArgs {
        model: model.to_string(),
        messages,
        completion: completion.to_string(),
        temperature: Some(0.0),
        top_p: None,
        tools: None,
        extra_key: None,
        finish_reason: Some("stop".to_string()),
        is_error: false,
        prompt_tokens: Some(120),
        completion_tokens: Some(45),
        ttl_seconds: None,
    }
}

const ORIGINAL: &str = "how do I reset my password?";
const REWORDED: &str = "what is the procedure to reset my password?";

#[tokio::test]
async fn a_reworded_prompt_reuses_the_answer_and_the_saving_is_measurable() {
    let state = state_against(&start_stub(StubBehaviour::Embed).await).await;

    let stored = handle_store(
        Arc::clone(&state),
        store(
            "qwen3-8b",
            vec![user(ORIGINAL)],
            "Use the reset link on the sign-in page.",
        ),
    )
    .await
    .expect("the stub answers");
    assert!(stored.stored);
    assert!(!stored.replaced);

    // Different words, same question.
    let hit = handle_lookup(Arc::clone(&state), lookup("qwen3-8b", vec![user(REWORDED)]))
        .await
        .expect("the stub answers");
    assert!(hit.hit, "a reworded equivalent prompt must hit: {hit:?}");
    assert_eq!(hit.match_kind, Some(crate::store::MatchKind::Semantic));
    assert_eq!(
        hit.completion.as_deref(),
        Some("Use the reset link on the sign-in page.")
    );
    assert!(hit.similarity.expect("a hit has a score") > 0.95);
    let saved = hit.tokens_saved.expect("a hit reports what it saved");
    assert_eq!(saved.total_tokens, 165);

    // The identical wording is served without spending an embedding call.
    let before = state.store.snapshot(0).counters.embedding_calls;
    let exact = handle_lookup(Arc::clone(&state), lookup("qwen3-8b", vec![user(ORIGINAL)]))
        .await
        .expect("no network needed for an exact match");
    assert!(exact.hit);
    assert_eq!(exact.match_kind, Some(crate::store::MatchKind::Exact));
    assert_eq!(
        state.store.snapshot(0).counters.embedding_calls,
        before,
        "an exact hit must not call the embedder"
    );

    // An unrelated question is a miss, and reports how close it got.
    let miss = handle_lookup(
        Arc::clone(&state),
        lookup("qwen3-8b", vec![user("what is the capital of France?")]),
    )
    .await
    .expect("the stub answers");
    assert!(!miss.hit);
    assert_eq!(miss.miss_reason.as_deref(), Some("below_threshold"));
    assert!(
        miss.best_similarity
            .expect("a score is reported for tuning")
            < 0.1
    );

    // These are the exact figures quoted in README.md's `stats` example. They
    // are pinned here so the documentation cannot drift away from the code.
    let stats = handle_stats(&state);
    assert_eq!(stats.snapshot.counters.lookups, 3);
    assert_eq!(stats.snapshot.hits, 2);
    assert_eq!(stats.snapshot.misses, 1);
    assert_eq!(stats.snapshot.counters.hits_exact, 1);
    assert_eq!(stats.snapshot.counters.hits_semantic, 1);
    assert_eq!(
        stats
            .snapshot
            .counters
            .misses_by_reason
            .get("below_threshold"),
        Some(&1)
    );
    assert_eq!(stats.snapshot.counters.stores_accepted, 1);
    assert_eq!(stats.snapshot.counters.embedding_calls, 3);
    assert_eq!(stats.snapshot.counters.embedding_failures, 0);
    assert!((stats.snapshot.hit_rate - 2.0 / 3.0).abs() < 1e-9);
    // Two hits on one entry recorded as 120 prompt + 45 completion tokens.
    assert_eq!(stats.snapshot.counters.prompt_tokens_saved, 240);
    assert_eq!(stats.snapshot.counters.completion_tokens_saved, 90);
    assert_eq!(stats.snapshot.tokens_saved_total, 330);
    assert_eq!(stats.snapshot.entries, 1);
    assert_eq!(stats.snapshot.buckets, 1);
    // 192 overhead + 64-char bucket digest + 8 model + 27 query + 39
    // completion + 2 stub dimensions at 4 bytes each. A real 768-dimension
    // embedder would add roughly 3 KiB per entry.
    assert_eq!(stats.snapshot.approx_bytes, 338);
}

#[tokio::test]
async fn a_near_neighbour_below_the_threshold_is_not_served() {
    let state = state_against(&start_stub(StubBehaviour::Embed).await).await;
    handle_store(
        Arc::clone(&state),
        store("qwen3-8b", vec![user(ORIGINAL)], "Use the reset link."),
    )
    .await
    .expect("the stub answers");

    // The stub places this 0.35 radians away — cosine ≈ 0.94, which is close
    // enough to look tempting and not close enough to be safe.
    let miss = handle_lookup(
        Arc::clone(&state),
        lookup("qwen3-8b", vec![user("how do I reset my passphrase?")]),
    )
    .await
    .expect("the stub answers");

    assert!(
        !miss.hit,
        "the conservative default exists for exactly this case"
    );
    assert_eq!(miss.miss_reason.as_deref(), Some("below_threshold"));
    let best = miss.best_similarity.expect("the score is reported");
    assert!(
        (0.9..0.95).contains(&best),
        "near but not near enough: {best}"
    );
}

#[tokio::test]
async fn a_cached_answer_never_crosses_a_model_or_a_system_prompt() {
    let state = state_against(&start_stub(StubBehaviour::Embed).await).await;
    handle_store(
        Arc::clone(&state),
        store(
            "qwen3-8b",
            vec![system("Answer in one sentence."), user(ORIGINAL)],
            "Use the reset link.",
        ),
    )
    .await
    .expect("the stub answers");

    let cases = [
        (
            "a different completion model",
            lookup(
                "llama3-70b",
                vec![system("Answer in one sentence."), user(REWORDED)],
            ),
        ),
        (
            "a different system prompt",
            lookup(
                "qwen3-8b",
                vec![system("Answer in detail."), user(REWORDED)],
            ),
        ),
        (
            "no system prompt at all",
            lookup("qwen3-8b", vec![user(REWORDED)]),
        ),
        ("a different temperature", {
            let mut args = lookup(
                "qwen3-8b",
                vec![system("Answer in one sentence."), user(REWORDED)],
            );
            args.temperature = Some(0.7);
            args
        }),
        ("a different tool set", {
            let mut args = lookup(
                "qwen3-8b",
                vec![system("Answer in one sentence."), user(REWORDED)],
            );
            args.tools = Some(vec![serde_json::json!({"type": "function"})]);
            args
        }),
    ];

    for (description, args) in cases {
        let result = handle_lookup(Arc::clone(&state), args)
            .await
            .expect("the stub answers");
        assert!(
            !result.hit,
            "{description} must not reuse the cached answer"
        );
        assert_eq!(
            result.miss_reason.as_deref(),
            Some("bucket_empty"),
            "{description} should not even be a candidate"
        );
    }

    // The original shape still hits, so the isolation above is not just the
    // cache being broken.
    let control = handle_lookup(
        Arc::clone(&state),
        lookup(
            "qwen3-8b",
            vec![system("Answer in one sentence."), user(REWORDED)],
        ),
    )
    .await
    .expect("the stub answers");
    assert!(control.hit, "the matching shape must still hit");
}

#[tokio::test]
async fn an_errored_backend_surfaces_as_an_error_on_both_paths() {
    let state = state_against(&start_stub(StubBehaviour::Unavailable).await).await;

    let error = handle_lookup(Arc::clone(&state), lookup("qwen3-8b", vec![user(ORIGINAL)]))
        .await
        .expect_err("a 503 must not look like a cold cache");
    assert!(error.message.contains("503"), "{}", error.message);
    assert!(
        error.message.contains("model is loading"),
        "{}",
        error.message
    );

    let error = handle_store(
        Arc::clone(&state),
        store("qwen3-8b", vec![user(ORIGINAL)], "an answer"),
    )
    .await
    .expect_err("a store that cannot embed must not report success");
    assert!(error.message.contains("503"), "{}", error.message);

    let counters = state.store.snapshot(0).counters;
    assert_eq!(counters.embedding_failures, 2);
    assert_eq!(counters.stores_accepted, 0);
}

#[tokio::test]
async fn a_zero_ttl_never_expires_while_a_short_one_does() {
    let state = state_against(&start_stub(StubBehaviour::Embed).await).await;

    let mut args = store("qwen3-8b", vec![user(ORIGINAL)], "Use the reset link.");
    args.ttl_seconds = Some(0);
    let stored = handle_store(Arc::clone(&state), args)
        .await
        .expect("the stub answers");
    assert_eq!(
        stored.expires_in_seconds, None,
        "ttl_seconds = 0 is the documented never-expire escape hatch"
    );

    // With expiry disabled the entry survives, which is the behaviour the
    // escape hatch promises.
    let hit = handle_lookup(Arc::clone(&state), lookup("qwen3-8b", vec![user(ORIGINAL)]))
        .await
        .expect("the stub answers");
    assert!(hit.hit);

    // A one-second TTL on a fresh entry does expire, on the store's own clock.
    let mut args = store(
        "qwen3-8b",
        vec![user("what is my password policy?")],
        "Twelve characters.",
    );
    args.ttl_seconds = Some(1);
    handle_store(Arc::clone(&state), args)
        .await
        .expect("the stub answers");
    let now = state.clock.monotonic_ms();
    assert_eq!(
        state.store.purge_expired(now + 2_000),
        1,
        "the second entry is past its TTL and the first never expires"
    );
    assert_eq!(state.store.len(), 1);
}
