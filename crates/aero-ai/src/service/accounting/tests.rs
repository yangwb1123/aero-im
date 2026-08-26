use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use aero_storage::{AiJobRepo, MessageRepo, RoomRepo};

use super::*;
use crate::embed::{Embedder, HashEmbedder, VoyageEmbedder, EMBED_DIM};
use crate::transcribe::{StubTranscriber, Transcriber};
use crate::usage::{
    UsageEvent, UsagePersistOutcome, UsageReservation, UsageReserveOutcome, UsageSink,
};

fn service(
    embedder: Arc<dyn Embedder + Send + Sync>,
    transcriber: Arc<dyn Transcriber>,
) -> AiService {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://aero:aero_dev_pw@localhost:5432/aero")
        .expect("valid lazy test URL");
    AiService::new(
        None,
        embedder,
        transcriber,
        AiJobRepo::new(pool.clone()),
        MessageRepo::new(pool.clone()),
        RoomRepo::new(pool),
        None,
    )
}

fn service_with_anthropic(client: Arc<AnthropicClient>) -> AiService {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://aero:aero_dev_pw@localhost:5432/aero")
        .expect("valid lazy test URL");
    AiService::new(
        Some(client),
        Arc::new(HashEmbedder::new()),
        Arc::new(StubTranscriber),
        AiJobRepo::new(pool.clone()),
        MessageRepo::new(pool.clone()),
        RoomRepo::new(pool),
        None,
    )
}

#[derive(Default)]
struct CaptureSink {
    events: Mutex<Vec<UsageEvent>>,
    finalized: AtomicUsize,
    cancelled: AtomicUsize,
}

#[async_trait::async_trait]
impl UsageSink for CaptureSink {
    async fn reserve(&self, event: UsageEvent) -> std::result::Result<UsageReserveOutcome, String> {
        let usage_id = event.usage_id;
        self.events.lock().unwrap().push(event);
        Ok(UsageReserveOutcome::Acquired(UsageReservation {
            usage_id,
            token: uuid::Uuid::new_v4(),
        }))
    }

    async fn finalize(
        &self,
        _reservation: UsageReservation,
        _actual_micros: u64,
        _outcome: Option<UsageOutcome>,
    ) -> std::result::Result<UsagePersistOutcome, String> {
        self.finalized.fetch_add(1, Ordering::Relaxed);
        Ok(UsagePersistOutcome::Inserted)
    }

    async fn cancel(&self, _reservation: UsageReservation) -> std::result::Result<bool, String> {
        self.cancelled.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }
}

struct RejectSink;

#[async_trait::async_trait]
impl UsageSink for RejectSink {
    async fn reserve(
        &self,
        _event: UsageEvent,
    ) -> std::result::Result<UsageReserveOutcome, String> {
        Err("ledger unavailable".to_owned())
    }

    async fn finalize(
        &self,
        _reservation: UsageReservation,
        _actual_micros: u64,
        _outcome: Option<UsageOutcome>,
    ) -> std::result::Result<UsagePersistOutcome, String> {
        unreachable!("rejected reservations cannot be finalized")
    }

    async fn cancel(&self, _reservation: UsageReservation) -> std::result::Result<bool, String> {
        unreachable!("rejected reservations cannot be cancelled")
    }
}

struct FinalizedSink;

#[async_trait::async_trait]
impl UsageSink for FinalizedSink {
    async fn reserve(
        &self,
        _event: UsageEvent,
    ) -> std::result::Result<UsageReserveOutcome, String> {
        Ok(UsageReserveOutcome::AlreadyFinalized(None))
    }

    async fn finalize(
        &self,
        _reservation: UsageReservation,
        _actual_micros: u64,
        _outcome: Option<UsageOutcome>,
    ) -> std::result::Result<UsagePersistOutcome, String> {
        unreachable!("a finalized operation cannot grant a reservation")
    }

    async fn cancel(&self, _reservation: UsageReservation) -> std::result::Result<bool, String> {
        unreachable!("a finalized operation cannot grant a reservation")
    }
}

#[derive(Default)]
struct ReplaySink {
    outcomes: Mutex<HashMap<uuid::Uuid, UsageOutcome>>,
}

#[async_trait::async_trait]
impl UsageSink for ReplaySink {
    async fn reserve(&self, event: UsageEvent) -> std::result::Result<UsageReserveOutcome, String> {
        if let Some(outcome) = self.outcomes.lock().unwrap().get(&event.usage_id).cloned() {
            return Ok(UsageReserveOutcome::AlreadyFinalized(Some(outcome)));
        }
        Ok(UsageReserveOutcome::Acquired(UsageReservation {
            usage_id: event.usage_id,
            token: uuid::Uuid::new_v4(),
        }))
    }

    async fn finalize(
        &self,
        reservation: UsageReservation,
        _actual_micros: u64,
        outcome: Option<UsageOutcome>,
    ) -> std::result::Result<UsagePersistOutcome, String> {
        let outcome = outcome.ok_or_else(|| "missing replay outcome".to_owned())?;
        self.outcomes
            .lock()
            .unwrap()
            .insert(reservation.usage_id, outcome);
        Ok(UsagePersistOutcome::Inserted)
    }

    async fn cancel(&self, _reservation: UsageReservation) -> std::result::Result<bool, String> {
        Ok(true)
    }
}

struct PaidEmbedder(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Embedder for PaidEmbedder {
    async fn embed_one(&self, _text: &str) -> Result<Vec<f32>> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(vec![0.0; EMBED_DIM])
    }

    fn dim(&self) -> usize {
        EMBED_DIM
    }

    fn model_id(&self) -> &'static str {
        "paid-test"
    }

    fn is_paid_provider(&self) -> bool {
        true
    }
}

struct PaidTranscriber(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Transcriber for PaidTranscriber {
    async fn transcribe(&self, _bytes: bytes::Bytes, _mime: &str) -> Result<String> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok("transcript".to_owned())
    }

    fn name(&self) -> &'static str {
        "paid-test"
    }

    fn is_paid_provider(&self) -> bool {
        true
    }
}

struct FailingPaidEmbedder;

#[async_trait::async_trait]
impl Embedder for FailingPaidEmbedder {
    async fn embed_one(&self, _text: &str) -> Result<Vec<f32>> {
        Err(AiError::Embedding(
            "voyage 400: provider rejected request".to_owned(),
        ))
    }

    fn dim(&self) -> usize {
        EMBED_DIM
    }

    fn model_id(&self) -> &'static str {
        "failing-paid-test"
    }

    fn is_paid_provider(&self) -> bool {
        true
    }
}

struct AmbiguousPaidEmbedder;

#[async_trait::async_trait]
impl Embedder for AmbiguousPaidEmbedder {
    async fn embed_one(&self, _text: &str) -> Result<Vec<f32>> {
        Err(AiError::Http("response body timed out".to_owned()))
    }

    fn dim(&self) -> usize {
        EMBED_DIM
    }

    fn model_id(&self) -> &'static str {
        "ambiguous-paid-test"
    }

    fn is_paid_provider(&self) -> bool {
        true
    }
}

async fn anthropic_stub(
    calls: Arc<AtomicUsize>,
    text: &'static str,
) -> (Arc<AnthropicClient>, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            calls.fetch_add(1, Ordering::Relaxed);
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4_096];
            loop {
                let Ok(read) = socket.read(&mut buffer).await else {
                    break;
                };
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let body = serde_json::json!({
                "content": [{"type": "text", "text": text}],
                "usage": {"input_tokens": 100, "output_tokens": 10}
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    let client =
        AnthropicClient::new("test-key", "test-model").with_base_url(format!("http://{address}"));
    (Arc::new(client), task)
}

async fn voyage_retry_stub(
    statuses: Vec<u16>,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4_096];
            loop {
                let Ok(read) = socket.read(&mut buffer).await else {
                    break;
                };
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let call = observed.fetch_add(1, Ordering::Relaxed);
            let status = statuses.get(call).copied().unwrap_or(500);
            let (status_text, body) = if status == 200 {
                (
                    "OK",
                    serde_json::json!({
                        "data": [{"embedding": vec![0.0; EMBED_DIM], "index": 0}]
                    })
                    .to_string(),
                )
            } else {
                ("Error", "{\"error\":\"try again\"}".to_owned())
            };
            let response = format!(
                "HTTP/1.1 {status} {status_text}\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (format!("http://{address}"), calls, task)
}

#[tokio::test]
async fn local_fallbacks_do_not_require_or_emit_usage() {
    let svc = service(Arc::new(HashEmbedder::new()), Arc::new(StubTranscriber));
    assert_eq!(svc.embed_text("hello").await.unwrap().len(), EMBED_DIM);
    assert!(!svc
        .transcribe(bytes::Bytes::from_static(b"voice"), "audio/webm")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn paid_provider_is_not_called_without_durable_reservation() {
    let embed_calls = Arc::new(AtomicUsize::new(0));
    let transcribe_calls = Arc::new(AtomicUsize::new(0));
    let svc = service(
        Arc::new(PaidEmbedder(embed_calls.clone())),
        Arc::new(PaidTranscriber(transcribe_calls.clone())),
    );
    let embed_error = svc.embed_text("hello").await.unwrap_err().to_string();
    assert!(embed_error.contains("usage sink is not configured"));
    assert_eq!(embed_calls.load(Ordering::Relaxed), 0);

    let svc = svc.with_usage_sink(Arc::new(RejectSink));
    let transcribe_error = svc
        .transcribe(bytes::Bytes::from_static(b"voice"), "audio/webm")
        .await
        .unwrap_err()
        .to_string();
    assert!(transcribe_error.contains("ledger unavailable"));
    assert_eq!(transcribe_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn ordinary_provider_failure_cancels_its_reservation() {
    let sink = Arc::new(CaptureSink::default());
    let svc = service(Arc::new(FailingPaidEmbedder), Arc::new(StubTranscriber))
        .with_usage_sink(sink.clone());
    let error = svc.embed_text("hello").await.unwrap_err().to_string();
    assert!(error.contains("provider rejected request"));
    assert_eq!(sink.events.lock().unwrap().len(), 1);
    assert_eq!(sink.finalized.load(Ordering::Relaxed), 0);
    assert_eq!(sink.cancelled.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn voyage_retry_success_is_one_accounted_operation() {
    let (url, calls, server) = voyage_retry_stub(vec![429, 200]).await;
    let embedder = VoyageEmbedder::new("test-key", "voyage-test")
        .with_base_url(url)
        .with_retry_base(std::time::Duration::from_millis(1));
    let sink = Arc::new(CaptureSink::default());
    let svc = service(Arc::new(embedder), Arc::new(StubTranscriber)).with_usage_sink(sink.clone());

    let values = svc
        .embed_query_with_context("hello", UsageContext::new(None), "voyage_query")
        .await
        .expect("retry should recover the provider call");
    assert_eq!(values.len(), EMBED_DIM);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(sink.finalized.load(Ordering::Relaxed), 1);
    assert_eq!(sink.cancelled.load(Ordering::Relaxed), 0);
    server.abort();
}

#[tokio::test]
async fn voyage_retry_failure_cancels_once_after_last_attempt() {
    let (url, calls, server) = voyage_retry_stub(vec![500, 500, 500]).await;
    let embedder = VoyageEmbedder::new("test-key", "voyage-test")
        .with_base_url(url)
        .with_retry_base(std::time::Duration::from_millis(1));
    let sink = Arc::new(CaptureSink::default());
    let svc = service(Arc::new(embedder), Arc::new(StubTranscriber)).with_usage_sink(sink.clone());

    let error = svc
        .embed_query_with_context("hello", UsageContext::new(None), "voyage_query")
        .await
        .expect_err("persistent provider failure should surface");
    assert!(error.to_string().contains("voyage 500:"));
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    assert_eq!(sink.finalized.load(Ordering::Relaxed), 0);
    assert_eq!(sink.cancelled.load(Ordering::Relaxed), 1);
    server.abort();
}

#[tokio::test]
async fn ambiguous_transport_failure_retains_conservative_reservation() {
    let sink = Arc::new(CaptureSink::default());
    let svc = service(Arc::new(AmbiguousPaidEmbedder), Arc::new(StubTranscriber))
        .with_usage_sink(sink.clone());
    let error = svc.embed_text("hello").await.unwrap_err().to_string();
    assert!(error.contains("outcome is ambiguous"));
    assert_eq!(sink.events.lock().unwrap().len(), 1);
    assert_eq!(sink.finalized.load(Ordering::Relaxed), 0);
    assert_eq!(sink.cancelled.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn finalized_stable_retry_does_not_call_provider_again() {
    let calls = Arc::new(AtomicUsize::new(0));
    let svc = service(
        Arc::new(PaidEmbedder(calls.clone())),
        Arc::new(StubTranscriber),
    )
    .with_usage_sink(Arc::new(FinalizedSink));
    let error = svc.embed_text("hello").await.unwrap_err().to_string();
    assert!(error.contains("without a replayable result"));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn worker_stable_root_replays_embedding_without_second_provider_call() {
    let calls = Arc::new(AtomicUsize::new(0));
    let sink = Arc::new(ReplaySink::default());
    let svc = service(
        Arc::new(PaidEmbedder(calls.clone())),
        Arc::new(StubTranscriber),
    )
    .with_usage_sink(sink);
    let context = UsageContext::for_job(ulid::Ulid::new(), Some(uuid::Uuid::new_v4()));

    let first = svc
        .embed_text_with_context("worker input", context, "voyage_embed_document")
        .await
        .unwrap();
    // Simulate the worker result write failing after usage finalization.
    let replayed = svc
        .embed_text_with_context("worker input", context, "voyage_embed_document")
        .await
        .unwrap();

    assert_eq!(replayed, first);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn digest_retry_replays_summary_after_business_write_failure() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (client, server) = anthropic_stub(calls.clone(), "durable digest").await;
    let svc =
        service_with_anthropic(client.clone()).with_usage_sink(Arc::new(ReplaySink::default()));
    let workspace = Some(uuid::Uuid::new_v4());
    let context = UsageContext::for_request(
        "delivery-key",
        uuid::Uuid::new_v4(),
        "digest:subscription:delivery",
        workspace,
    );
    let messages = [ChatMsg::user("summarize")];

    let first = svc
        .complete_accounted(
            &client,
            context,
            "anthropic_summarize",
            "summarize",
            5_000,
            "system",
            &messages,
            200,
        )
        .await
        .unwrap();
    // The caller deliberately discards `first`, modeling
    // save_prepared_summary failing after provider+usage commit.
    let replayed = svc
        .complete_accounted(
            &client,
            context,
            "anthropic_summarize",
            "summarize",
            5_000,
            "system",
            &messages,
            200,
        )
        .await
        .unwrap();

    assert_eq!(first, replayed);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    server.abort();
}

#[tokio::test]
async fn streaming_retry_replays_durable_chunks_without_opening_provider() {
    let client = Arc::new(
        AnthropicClient::new("test-key", "test-model").with_base_url("http://127.0.0.1:9"),
    );
    let sink = Arc::new(ReplaySink::default());
    let context = UsageContext::for_request(
        "stream-retry",
        uuid::Uuid::new_v4(),
        "ask-stream:room:question",
        Some(uuid::Uuid::new_v4()),
    );
    sink.outcomes.lock().unwrap().insert(
        context.operation_id("anthropic_stream_answer"),
        UsageOutcome {
            kind: STREAM_OUTCOME.into(),
            payload: serde_json::to_value(StreamOutcome {
                chunks: vec!["hello ".into(), "world".into()],
                terminal_error: None,
            })
            .unwrap(),
        },
    );
    let svc = service_with_anthropic(client.clone()).with_usage_sink(sink);

    let stream = svc
        .complete_stream_accounted(
            &client,
            context,
            "anthropic_stream_answer",
            "answer_stream_estimate",
            5_000,
            "system",
            &[ChatMsg::user("question")],
            200,
        )
        .await
        .unwrap();
    let chunks = stream.map(|item| item.unwrap()).collect::<Vec<_>>().await;
    assert_eq!(chunks, ["hello ", "world"]);
}

struct FixedToolChat;

#[async_trait::async_trait]
impl ToolChat for FixedToolChat {
    async fn next_turn(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _tools: &[ToolDef],
        _max_tokens: u32,
    ) -> Result<AgentTurn> {
        Ok(AgentTurn {
            text: "ok".to_owned(),
            tool_uses: Vec::new(),
            usage: Usage {
                input_tokens: 100,
                output_tokens: 10,
            },
        })
    }
}

#[tokio::test]
async fn agent_turns_have_stable_distinct_durable_ids() {
    let sink = Arc::new(CaptureSink::default());
    let svc = service(Arc::new(HashEmbedder::new()), Arc::new(StubTranscriber))
        .with_usage_sink(sink.clone());
    let context = UsageContext::new(Some(uuid::Uuid::new_v4()));
    let chat = svc.accounted_tool_chat(&FixedToolChat, context, "agent", "answer_agentic", 5_000);

    let messages = [serde_json::json!({"role": "user", "content": "hi"})];
    chat.next_turn("system", &messages, &[], 100).await.unwrap();
    chat.next_turn("system", &messages, &[], 100).await.unwrap();

    let events = sink.events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].usage_id, context.operation_id("agent_turn_0"));
    assert_eq!(events[1].usage_id, context.operation_id("agent_turn_1"));
    assert_ne!(events[0].usage_id, events[1].usage_id);
    assert!(events.iter().all(|event| event.kind == "answer_agentic"));
}

#[test]
fn production_service_modules_cannot_bypass_accounting_boundary() {
    let service_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/service");
    let forbidden = [
        ".complete_with_usage_model(",
        ".complete_with_tools(",
        ".complete_stream(",
        ".embed_one(",
        ".embed_query(",
        ".transcribe(",
    ];
    let mut violations = Vec::new();
    for entry in std::fs::read_dir(service_dir).expect("read service source directory") {
        let path = entry.expect("service directory entry").path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("rs")
            || path.file_name().and_then(std::ffi::OsStr::to_str) == Some("accounting.rs")
        {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read service source");
        for needle in forbidden {
            if source.contains(needle) {
                violations.push(format!("{} contains {needle}", path.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "raw provider calls must stay in service/accounting.rs:\n{}",
        violations.join("\n")
    );
}
