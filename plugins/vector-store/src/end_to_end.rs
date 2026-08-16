//! End-to-end tests against a stub embeddings server.
//!
//! The unit tests elsewhere cover the rules in isolation. These cover the
//! thing the plugin actually claims: ingest a real document, restart the
//! process, ask a question in different words, get the right passage back with
//! a usable citation.
//!
//! The stub is a **real HTTP server on loopback** speaking the OpenAI
//! embeddings shape, not a mock of the client — so the request encoding, the
//! batching, the response parsing and the ordering are all exercised for real.
//! Its "embeddings" are a small toy: each text is placed on the unit circle at
//! an angle chosen by keyword, so the *geometry* is controlled and every
//! assertion below is about the store's behaviour. **Nothing here is a
//! benchmark, and nothing here says anything about how well a real embedding
//! model retrieves.**

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::{Config, prepare_data_dir};
use crate::manifest::{
    AppState, DeleteArgs, DeleteScope, InputDocument, QueryArgs, StatsArgs, UpsertArgs,
    handle_delete, handle_query, handle_stats, handle_upsert,
};
use crate::store::VectorStore;
use crate::testsupport::TempTree;

/// What the stub returns, and with what status.
#[derive(Clone, Copy)]
enum StubBehaviour {
    /// Answer with embeddings derived from the input texts.
    Embed,
    /// Answer 503 with an OpenAI-shaped error body.
    Unavailable,
}

/// Place a text on the unit circle by keyword.
///
/// Texts about one topic land within 0.02 radians of each other, which is a
/// cosine above 0.999; different topics sit a quarter-turn apart.
/// Deterministic jitter keeps two wordings of one topic near but not
/// identical, so a hit proves nearest-neighbour behaviour rather than vector
/// equality.
fn angle_for(text: &str) -> f64 {
    let lowered = text.to_ascii_lowercase();
    let base = if lowered.contains("install") || lowered.contains("set up") {
        0.0
    } else if lowered.contains("backup") || lowered.contains("restore") {
        std::f64::consts::FRAC_PI_2
    } else if lowered.contains("licence") || lowered.contains("license") {
        std::f64::consts::PI
    } else {
        3.0 * std::f64::consts::FRAC_PI_2
    };
    base + 0.002 * ((text.len() % 10) as f64)
}

fn embedding_json(text: &str, index: usize) -> String {
    let angle = angle_for(text);
    format!(
        r#"{{"object":"embedding","index":{index},"embedding":[{},{}]}}"#,
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
/// `Connection: close` keeps this to a few dozen lines: no keep-alive, no
/// chunked encoding, no pipelining.
async fn serve_one(mut stream: TcpStream, behaviour: StubBehaviour) -> std::io::Result<()> {
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 4096];
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
            let inputs: Vec<String> = parsed
                .get("input")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .map(|value| value.as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default();

            let entries: Vec<String> = inputs
                .iter()
                .enumerate()
                .map(|(index, text)| embedding_json(text, index))
                .collect();
            (
                "200 OK",
                format!(r#"{{"object":"list","data":[{}]}}"#, entries.join(",")),
            )
        }
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// The corpus used by most tests below: a small manual with real structure.
const MANUAL: &str = "\
# Operations Manual

This manual covers running the service.

## Install

To install the service, unpack the archive and run the setup script. It will
ask you for a data directory.

The installer needs about two gigabytes of free space.

## Backup and restore

Take a backup before every upgrade. The backup command writes a single archive
to the path you give it.

To restore, stop the service first, then unpack the archive over the data
directory.

## Licence

The service is distributed under a permissive licence.
";

async fn state_with(base_url: &str, tree: &TempTree, extra: &[&str]) -> Arc<AppState> {
    let root = prepare_data_dir(tree.path()).expect("data dir");
    let mut argv = vec![
        format!("--data-dir={}", root.display()),
        format!("--embeddings-url={base_url}"),
        "--embedding-model=stub-embedder".to_string(),
        // Small chunks so the sample manual really does split.
        "--chunk-chars=200".to_string(),
        "--chunk-overlap-chars=60".to_string(),
        "--max-chunk-chars=400".to_string(),
    ];
    argv.extend(extra.iter().map(|value| value.to_string()));

    let config = Config::resolve(&argv, &BTreeMap::new()).expect("valid config");
    let store = Arc::new(VectorStore::open(&root, config.store_limits()).expect("store opens"));
    Arc::new(AppState::new(config, store).expect("state builds"))
}

fn document(id: &str, text: &str, source: &str) -> InputDocument {
    InputDocument {
        id: id.to_string(),
        text: text.to_string(),
        source: Some(source.to_string()),
        metadata: None,
    }
}

fn query(collection: &str, text: &str) -> QueryArgs {
    QueryArgs {
        collection: collection.to_string(),
        query: text.to_string(),
        top_k: Some(3),
        min_score: None,
        filter: None,
        source_prefix: None,
        document_ids: None,
    }
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_document_can_be_ingested_and_then_found_by_a_differently_worded_question() {
    let base = start_stub(StubBehaviour::Embed).await;
    let tree = TempTree::new("e2e-round-trip");
    let state = state_with(&base, &tree, &[]).await;

    let upserted = handle_upsert(
        Arc::clone(&state),
        UpsertArgs {
            collection: "manual".to_string(),
            documents: vec![document("manual", MANUAL, "docs/manual.md")],
        },
    )
    .await
    .expect("ingest");

    assert!(upserted.chunks_written > 1, "{upserted:#?}");
    assert_eq!(upserted.chunks_replaced, 0);
    assert_eq!(upserted.embedding_model, "stub-embedder");
    assert_eq!(upserted.dimensions, 2);

    // "how do I set up the service" never appears in the manual; the passage
    // about installing does.
    let found = handle_query(
        Arc::clone(&state),
        query("manual", "how do I set up the service"),
    )
    .await
    .expect("query");

    assert!(!found.results.is_empty(), "{found:#?}");
    let best = &found.results[0];
    assert!(
        best.chunk.text.contains("install") || best.chunk.text.contains("Install"),
        "the install passage should win: {best:#?}"
    );
    assert!(best.score > 0.99, "{}", best.score);
}

#[tokio::test]
async fn a_result_carries_everything_a_citation_needs() {
    let base = start_stub(StubBehaviour::Embed).await;
    let tree = TempTree::new("e2e-citation");
    let state = state_with(&base, &tree, &[]).await;

    handle_upsert(
        Arc::clone(&state),
        UpsertArgs {
            collection: "manual".to_string(),
            documents: vec![document("manual", MANUAL, "docs/manual.md")],
        },
    )
    .await
    .expect("ingest");

    let found = handle_query(
        Arc::clone(&state),
        query("manual", "how do I restore a backup"),
    )
    .await
    .expect("query");
    let best = &found.results[0];

    let citation = best
        .chunk
        .citation
        .as_deref()
        .expect("a citation is produced");
    assert!(citation.starts_with("docs/manual.md:"), "{citation}");
    assert!(best.chunk.line_start >= 1);
    assert!(best.chunk.line_end >= best.chunk.line_start);
    assert!(
        best.chunk
            .heading_path
            .iter()
            .any(|title| title.contains("Backup")),
        "the passage should know which section it is in: {:?}",
        best.chunk.heading_path
    );

    // The claimed line span really does contain the passage's opening words.
    let lines: Vec<&str> = MANUAL.lines().collect();
    let quoted =
        lines[(best.chunk.line_start - 1) as usize..=(best.chunk.line_end - 1) as usize].join("\n");
    let opening = best
        .chunk
        .text
        .split_whitespace()
        .next()
        .expect("non-empty passage");
    assert!(
        quoted.contains(opening.trim_matches('#')),
        "citation {citation} does not contain the passage it points at:\n{quoted}"
    );
}

#[tokio::test]
async fn the_store_survives_a_restart_and_still_answers() {
    let base = start_stub(StubBehaviour::Embed).await;
    let tree = TempTree::new("e2e-restart");

    {
        let state = state_with(&base, &tree, &[]).await;
        handle_upsert(
            Arc::clone(&state),
            UpsertArgs {
                collection: "manual".to_string(),
                documents: vec![document("manual", MANUAL, "docs/manual.md")],
            },
        )
        .await
        .expect("ingest");
    }

    // A whole new process's worth of state, reading only what is on disk.
    let restarted = state_with(&base, &tree, &[]).await;
    let (collections, chunks) = restarted.store.counts();
    assert_eq!(collections, 1);
    assert!(chunks > 1);

    let found = handle_query(
        Arc::clone(&restarted),
        query("manual", "installing the service"),
    )
    .await
    .expect("query after restart");
    assert!(!found.results.is_empty(), "{found:#?}");
    assert_eq!(found.embedding_model, "stub-embedder");
}

#[tokio::test]
async fn re_ingesting_a_shortened_document_leaves_no_stale_passages() {
    let base = start_stub(StubBehaviour::Embed).await;
    let tree = TempTree::new("e2e-replace");
    let state = state_with(&base, &tree, &[]).await;

    handle_upsert(
        Arc::clone(&state),
        UpsertArgs {
            collection: "manual".to_string(),
            documents: vec![document("manual", MANUAL, "docs/manual.md")],
        },
    )
    .await
    .expect("ingest");

    let second = handle_upsert(
        Arc::clone(&state),
        UpsertArgs {
            collection: "manual".to_string(),
            documents: vec![document(
                "manual",
                "# Operations Manual\n\nThe licence is permissive.\n",
                "docs/manual.md",
            )],
        },
    )
    .await
    .expect("re-ingest");

    assert!(second.chunks_replaced > 0, "{second:#?}");

    // The section that no longer exists must no longer be retrievable.
    let found = handle_query(
        Arc::clone(&state),
        query("manual", "how do I restore a backup"),
    )
    .await
    .expect("query");
    assert!(
        found
            .results
            .iter()
            .all(|hit| !hit.chunk.text.contains("unpack the archive over")),
        "an edited document left a stale passage behind: {found:#?}"
    );

    let stats = handle_stats(Arc::clone(&state), StatsArgs { collection: None })
        .await
        .expect("stats");
    assert_eq!(stats.collections[0].documents, 1);
}

#[tokio::test]
async fn a_metadata_filter_narrows_the_search() {
    let base = start_stub(StubBehaviour::Embed).await;
    let tree = TempTree::new("e2e-filter");
    let state = state_with(&base, &tree, &[]).await;

    handle_upsert(
        Arc::clone(&state),
        UpsertArgs {
            collection: "manual".to_string(),
            documents: vec![
                InputDocument {
                    id: "public".to_string(),
                    text: "To install the service, run the public setup script.".to_string(),
                    source: Some("docs/public.md".to_string()),
                    metadata: Some(BTreeMap::from([(
                        "audience".to_string(),
                        "public".to_string(),
                    )])),
                },
                InputDocument {
                    id: "internal".to_string(),
                    text: "To install the service internally, run the staff setup script."
                        .to_string(),
                    source: Some("docs/internal.md".to_string()),
                    metadata: Some(BTreeMap::from([(
                        "audience".to_string(),
                        "internal".to_string(),
                    )])),
                },
            ],
        },
    )
    .await
    .expect("ingest");

    let found = handle_query(
        Arc::clone(&state),
        QueryArgs {
            collection: "manual".to_string(),
            query: "how do I set up the service".to_string(),
            top_k: Some(10),
            min_score: None,
            filter: Some(BTreeMap::from([(
                "audience".to_string(),
                "public".to_string(),
            )])),
            source_prefix: None,
            document_ids: None,
        },
    )
    .await
    .expect("query");

    assert_eq!(found.returned, 1, "{found:#?}");
    assert!(found.filtered);
    assert_eq!(found.results[0].chunk.document_id, "public");
    assert_eq!(found.collection_chunks, 2, "both chunks are stored");
}

#[tokio::test]
async fn collections_are_namespaces_end_to_end() {
    let base = start_stub(StubBehaviour::Embed).await;
    let tree = TempTree::new("e2e-namespaces");
    let state = state_with(&base, &tree, &[]).await;

    for collection in ["alpha", "beta"] {
        handle_upsert(
            Arc::clone(&state),
            UpsertArgs {
                collection: collection.to_string(),
                documents: vec![document(
                    "manual",
                    &format!("Installing the {collection} service is straightforward."),
                    &format!("docs/{collection}.md"),
                )],
            },
        )
        .await
        .expect("ingest");
    }

    let found = handle_query(Arc::clone(&state), query("alpha", "how do I install"))
        .await
        .expect("query");
    assert_eq!(found.returned, 1);
    assert!(found.results[0].chunk.text.contains("alpha"));

    // Dropping one namespace leaves the other intact.
    handle_delete(
        Arc::clone(&state),
        DeleteArgs {
            collection: "alpha".to_string(),
            scope: DeleteScope::Collection,
            document_ids: None,
        },
    )
    .await
    .expect("delete");

    assert!(
        handle_query(Arc::clone(&state), query("alpha", "how do I install"))
            .await
            .is_err(),
        "a deleted collection must not answer"
    );
    let survivor = handle_query(Arc::clone(&state), query("beta", "how do I install"))
        .await
        .expect("the other namespace is untouched");
    assert_eq!(survivor.returned, 1);
}

#[tokio::test]
async fn a_second_embedding_model_is_refused_against_a_real_ingest() {
    let base = start_stub(StubBehaviour::Embed).await;
    let tree = TempTree::new("e2e-model-pin");

    let first = state_with(&base, &tree, &[]).await;
    handle_upsert(
        Arc::clone(&first),
        UpsertArgs {
            collection: "manual".to_string(),
            documents: vec![document("manual", MANUAL, "docs/manual.md")],
        },
    )
    .await
    .expect("ingest");
    drop(first);

    // The operator swaps the model and restarts.
    let root = prepare_data_dir(tree.path()).expect("data dir");
    let config = Config::resolve(
        &[
            format!("--data-dir={}", root.display()),
            format!("--embeddings-url={base}"),
            "--embedding-model=a-different-embedder".to_string(),
        ],
        &BTreeMap::new(),
    )
    .expect("valid config");
    let store = Arc::new(VectorStore::open(&root, config.store_limits()).expect("store opens"));
    let swapped = Arc::new(AppState::new(config, store).expect("state builds"));

    let error = handle_query(Arc::clone(&swapped), query("manual", "how do I install"))
        .await
        .expect_err("comparing across embedding spaces must be refused");
    assert!(error.message.contains("stub-embedder"), "{}", error.message);
    assert!(
        error.message.contains("a-different-embedder"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("not comparable"),
        "{}",
        error.message
    );

    let error = handle_upsert(
        Arc::clone(&swapped),
        UpsertArgs {
            collection: "manual".to_string(),
            documents: vec![document(
                "other",
                "Installing something else.",
                "docs/other.md",
            )],
        },
    )
    .await
    .expect_err("writing a second space into one collection must be refused");
    assert!(
        error.message.contains("delete the collection"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn an_unreachable_backend_fails_the_query_rather_than_reporting_no_matches() {
    let embedding_stub = start_stub(StubBehaviour::Embed).await;
    let tree = TempTree::new("e2e-backend-down");

    // Ingest while the backend works…
    let state = state_with(&embedding_stub, &tree, &[]).await;
    handle_upsert(
        Arc::clone(&state),
        UpsertArgs {
            collection: "manual".to_string(),
            documents: vec![document("manual", MANUAL, "docs/manual.md")],
        },
    )
    .await
    .expect("ingest");
    drop(state);

    // …then bring up a process pointed at a backend that is failing.
    let broken = start_stub(StubBehaviour::Unavailable).await;
    let down = state_with(&broken, &tree, &[]).await;

    let error = handle_query(Arc::clone(&down), query("manual", "how do I install"))
        .await
        .expect_err("an outage must not look like an empty index");
    assert!(error.message.contains("503"), "{}", error.message);
    assert!(
        error.message.contains("model is loading"),
        "{}",
        error.message
    );

    // `stats` still answers, because it needs no network at all.
    let stats = handle_stats(Arc::clone(&down), StatsArgs { collection: None })
        .await
        .expect("stats never touch the network");
    assert!(stats.total_chunks > 0);
}

#[tokio::test]
async fn a_failed_ingest_stores_nothing() {
    let broken = start_stub(StubBehaviour::Unavailable).await;
    let tree = TempTree::new("e2e-failed-ingest");
    let state = state_with(&broken, &tree, &[]).await;

    let error = handle_upsert(
        Arc::clone(&state),
        UpsertArgs {
            collection: "manual".to_string(),
            documents: vec![document("manual", MANUAL, "docs/manual.md")],
        },
    )
    .await
    .expect_err("the embedder is down");
    assert!(error.message.contains("503"), "{}", error.message);

    let stats = handle_stats(Arc::clone(&state), StatsArgs { collection: None })
        .await
        .expect("stats");
    assert!(
        stats.collections.is_empty(),
        "a half-embedded ingest must leave no collection behind: {stats:#?}"
    );
    assert_eq!(state.store.counts(), (0, 0));
}

#[tokio::test]
async fn batching_sends_every_chunk_and_keeps_them_in_order() {
    let base = start_stub(StubBehaviour::Embed).await;
    let tree = TempTree::new("e2e-batching");
    // A batch size well below the chunk count, so several requests are made
    // and the vectors have to be stitched back together in order.
    let state = state_with(&base, &tree, &["--embed-batch-size=2"]).await;

    // Each section is padded so the packer keeps them apart and the ingest
    // needs several batches — which is what makes stitching observable.
    let section = |heading: &str, topic: &str| {
        format!(
            "## {heading}\n\n{}\n",
            format!("This paragraph is about {topic} and says so repeatedly. ").repeat(6)
        )
    };
    let mixed = format!(
        "{}\n{}\n{}\n{}\n",
        section("Install", "install"),
        section("Backup and restore", "backup"),
        section("Licence", "licence"),
        section("Installing again", "install"),
    );
    let mixed = mixed.as_str();

    let upserted = handle_upsert(
        Arc::clone(&state),
        UpsertArgs {
            collection: "manual".to_string(),
            documents: vec![document("manual", mixed, "docs/manual.md")],
        },
    )
    .await
    .expect("ingest");
    assert!(upserted.chunks_written >= 3, "{upserted:#?}");
    assert!(
        upserted.embedding_calls > 1,
        "a batch size of 2 should have taken several calls: {upserted:#?}"
    );

    // If a vector had been paired with the wrong chunk, the licence question
    // would return an install passage.
    let found = handle_query(
        Arc::clone(&state),
        query("manual", "what licence is this under"),
    )
    .await
    .expect("query");
    assert!(
        found.results[0]
            .chunk
            .text
            .to_lowercase()
            .contains("licence"),
        "vectors were paired with the wrong chunks: {:#?}",
        found.results[0].chunk
    );
}

#[tokio::test]
async fn deleting_one_document_leaves_its_neighbours_retrievable() {
    let base = start_stub(StubBehaviour::Embed).await;
    let tree = TempTree::new("e2e-delete-document");
    let state = state_with(&base, &tree, &[]).await;

    handle_upsert(
        Arc::clone(&state),
        UpsertArgs {
            collection: "manual".to_string(),
            documents: vec![
                document(
                    "install",
                    "Installing the service takes a minute.",
                    "docs/i.md",
                ),
                document("backup", "Restoring a backup takes longer.", "docs/b.md"),
            ],
        },
    )
    .await
    .expect("ingest");

    let deleted = handle_delete(
        Arc::clone(&state),
        DeleteArgs {
            collection: "manual".to_string(),
            scope: DeleteScope::Documents,
            document_ids: Some(vec!["install".to_string()]),
        },
    )
    .await
    .expect("delete");
    assert_eq!(deleted.outcome.documents_deleted, 1);
    assert!(!deleted.outcome.collection_removed);

    let found = handle_query(
        Arc::clone(&state),
        query("manual", "how do I restore a backup"),
    )
    .await
    .expect("query");
    assert_eq!(found.returned, 1);
    assert_eq!(found.results[0].chunk.document_id, "backup");
}

#[tokio::test]
async fn an_empty_result_says_whether_the_collection_was_empty_or_the_scores_were_low() {
    let base = start_stub(StubBehaviour::Embed).await;
    let tree = TempTree::new("e2e-empty-result");
    let state = state_with(&base, &tree, &[]).await;

    handle_upsert(
        Arc::clone(&state),
        UpsertArgs {
            collection: "manual".to_string(),
            documents: vec![document(
                "licence",
                "The licence is permissive.",
                "docs/l.md",
            )],
        },
    )
    .await
    .expect("ingest");

    let found = handle_query(
        Arc::clone(&state),
        QueryArgs {
            collection: "manual".to_string(),
            query: "how do I install the service".to_string(),
            top_k: Some(5),
            // The stub puts these topics a quarter-turn apart, so nothing
            // clears this bar.
            min_score: Some(0.9),
            filter: None,
            source_prefix: None,
            document_ids: None,
        },
    )
    .await
    .expect("query");

    assert_eq!(found.returned, 0);
    assert_eq!(found.collection_chunks, 1);
    assert!(
        found
            .notes
            .iter()
            .any(|note| note.contains("none scored at or above min_score")),
        "an empty result must distinguish itself from an empty collection: {:?}",
        found.notes
    );
}
