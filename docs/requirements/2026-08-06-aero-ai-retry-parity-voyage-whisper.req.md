# Requirements Spec — Retry/backoff parity for Voyage embedder & Whisper transcriber

- **Module**: `crates/aero-ai`
- **Source direction**: `docs/auto/analyses/crates-aero-ai-f8cd3622.json` (direction 2)
- **Date**: 2026-08-06 · **Status**: implemented in source and provider-tested
- **Scope**: retry with exponential backoff on Voyage and Whisper interactive HTTP calls for transient 429/5xx/transport errors. Shared policy, provider test seams, and error-prefix/accounting invariants are implemented; no timeout or accounting-wrapper changes.

> Current-source note (2026-08-19): `crates/aero-ai/src/retry.rs` is the shared
> 3-attempt 1s/2s/4s policy used by Anthropic, Voyage, and Whisper. Voyage and
> Whisper expose base-URL seams for deterministic mock servers; their unit tests
> cover 429→200, persistent 5xx, and fail-fast 4xx. The no-retry findings below
> are historical evidence for the direction.

---

## 1. Evidence verification (all direction citations re-checked against `master`)

| Direction claim | Verified finding |
|---|---|
| `AnthropicClient::complete_with_usage_model` retries 429/5xx/transport with backoff 1s/2s/4s, 3 attempts | ✅ `crates/aero-ai/src/anthropic.rs:191-249` — loop with `max_attempts = 3`, `Duration::from_secs(1 << (attempt - 1))` sleeps at 233 (HTTP status) and 242 (transport); `is_retryable` at 672 = `429 ‖ is_server_error()` |
| `VoyageEmbedder::embed_with` — historical single `.send().await?`, no retry | ✅ historical finding; current `embed_with` uses `retry_with_backoff(3, 1s)` and preserves the `voyage <numeric-status>:` error prefix |
| `WhisperTranscriber` — historical single send, no retry | ✅ historical finding; current `transcribe` uses the shared retry helper and preserves the `whisper <numeric-status>:` error prefix |
| Interactive RAG callers treat the embedding leg as fatal | ✅ `crates/aero-ai/src/service/service_impl.rs:250` (`retrieve_room_with_context`) and `:277` (`retrieve_workspace_with_context`) — `embed_query_with_context(..).await?` is fatal, while the FTS leg right below it degrades with a `tracing::warn!`; `:1110` (`find_expert_with_usage_context`) is the same fatal pattern |
| Queue-backed Embed jobs are mitigated by retry-on-claim | ✅ `crates/aero-ai/src/worker/mod.rs:314` — `embed_text_with_context` in `handle_embed`; job rows are re-claimed on failure (`MAX_ATTEMPTS=5`, §2 of AGENTS.md). Interactive `/ask`, `/ask/context` (routes/ai.rs), hybrid search (routes/search.rs), `find_expert` (find_expert.rs:117) have no such safety net |
| `settle_provider_error` interacts with retries | ✅ `crates/aero-ai/src/service/accounting.rs` — `settle_provider_error`: `AiError::Embedding` with a `voyage {status}:` prefix and `AiError::Internal` with `whisper {status}:` are `definitive_no_charge` (reservation **cancelled**); `AiError::Http` (transport) is **ambiguous** (reservation retained, conservative charge) |
| Reservation/finalize protocol: reserve → provider call → finalize/cancel | ✅ `accounting.rs` `embed_text_with_context` / `embed_query_with_context` / `transcribe_with_context` — each reserves, calls `self.embedder.embed_*` / `self.transcriber.transcribe`, then finalizes (or settles error) exactly once per logical operation |

**Discrepancy resolved**: the direction's mock-server tests required two seams. Current `VoyageEmbedder` and `WhisperTranscriber` provide explicit constructors/base-URL overrides (plus test-only millisecond retry bases), and provider tests use local `TcpListener` counters without mutating process-wide env vars.

---

## 2. Problem statement (historical gap; now closed by the shared retry implementation)

- Voyage query embeddings (`embed_query` → `embed_with("query")`) and Whisper transcription are the **only two paid HTTP legs without retry** in `aero-ai`. One transient 429/5xx/connection reset fails the whole user-facing operation:
  - `/api/rooms/:id/ask` and `/ask/context` (`routes/ai.rs` → `retrieve_room_with_context`/`retrieve_workspace_with_context`),
  - hybrid search (`routes/search.rs`),
  - `/api/workspaces/:id/find-expert` (`find_expert.rs:117` → `find_expert_with_usage_context`, `service_impl.rs:1110`),
  - voice-message transcription (`server/src/transcribe_bot.rs:141` — a 429 permanently leaves the Voice block untranscribed, fail-open but lossy).
- The asymmetry is deliberate *inside* the retrievers (FTS failures warn-and-degrade, `service_impl.rs:255-262`) but the paid embedding leg is `?`-fatal — the opposite of the resilience intent.
- Anthropic completions already set the precedent: 3 attempts, 1s/2s/4s exponential backoff, retry on 429/5xx/transport, fail fast on other 4xx (`anthropic.rs:191-249,672`).
- **Side benefit (unchanged semantics)**: the queue-backed Embed worker (`worker/mod.rs:314`) also benefits — retries absorb the transient errors *before* burning one of `MAX_ATTEMPTS=5` job attempts.

## 3. Non-goals (explicitly out of scope)

1. No changes to `complete_stream` / `complete_with_tools` / agent loop (their own call paths stay as-is).
2. No changes to the reservation/accounting protocol in `accounting.rs` — retries live **inside** the provider call so the accounting wrapper still sees exactly one logical operation.
3. No timeout changes (Voyage client 30s, Whisper 60s stay; worst-case interactive latency grows to 3×timeout+3s backoff — the same trade-off the Anthropic path already accepts, `anthropic.rs:59`).
4. No new dependencies (existing `tokio::time::sleep` + the in-crate mock-server pattern; dev-deps stay `pretty_assertions` + `tokio`).

---

## 4. Requirements

### REQ-1 — Shared retry helper (`crates/aero-ai/src/retry.rs`, new module)

Extract the anthropic retry semantics into a crate-internal helper:

- `pub(crate) fn is_retryable(status: reqwest::StatusCode) -> bool` — `429` or `status.is_server_error()`; byte-identical to `anthropic.rs:672`. `anthropic.rs` must adopt the shared fn (remove its private copy) so there is exactly one definition and no dead-code drift.
- `pub(crate) fn backoff_delay(attempt: usize, base: Duration) -> Duration` — `base * 2^(attempt-1)` (production base = 1s ⇒ 1s/2s/4s, identical to `anthropic.rs:233`).
- `pub(crate) async fn retry_with_backoff<T, F, Fut>(max_attempts: usize, base: Duration, attempt: F) -> Result<T>` where `F: FnMut() -> Fut`, `Fut: Future<Output = Result<T, AttemptError>>`, `AttemptError { retryable: bool, error: AiError }`. Semantics, mirroring `anthropic.rs:212-247`:
  - `Ok(v)` → return `Ok(v)`;
  - non-retryable error → return `Err(error)` immediately (no sleep, no extra request);
  - retryable error and `attempt < max_attempts` → `tokio::time::sleep(backoff_delay(attempt, base))` then re-invoke;
  - retryable error on the final attempt → return `Err(error)` (the **last** attempt's error is delivered).
- `base` is a parameter so tests pass `Duration::from_millis(1)` (or `ZERO`); production callers pass 1s. Production behavior is unchanged from the anthropic precedent.

### REQ-2 — Retry in `VoyageEmbedder::embed_with` (`crates/aero-ai/src/embed.rs`)

- Wrap the POST (currently `embed.rs:121`) in `retry_with_backoff` with `max_attempts = 3`, `base = 1s`.
- Attempt closure maps: transport error → `AttemptError { retryable: true, error: AiError::Http(..) }` (matches today's `?`); non-2xx response → retryable iff `is_retryable(status)`, error = `AiError::Embedding("voyage {status}: {body}")`; 2xx → parse + dimension-check as today.
- Covers **both** roles automatically: `embed_one` (document) and `embed_query` (query) both route through `embed_with` (`embed.rs:126-131`), so the interactive query path (`embed_query_with_context` → `embedder.embed_query`) and the document path both get retry.
- Empty-text early return (zero vector, `embed.rs:110-114`) stays before any HTTP — no retry for it.
- **Invariant**: the final-attempt error message must keep the exact `"voyage {status}:"` prefix shape so `definitive_no_charge` (`accounting.rs`) continues to classify final 4xx/5xx as no-charge → `cancel` (unchanged behavior).

### REQ-3 — Retry in `WhisperTranscriber::transcribe` (`crates/aero-ai/src/transcribe.rs`)

- Wrap the POST (currently `transcribe.rs:77`) in `retry_with_backoff` with `max_attempts = 3`, `base = 1s`.
- Build the multipart form inside the attempt closure (reconstruct per attempt; `Bytes` is cheap-clone).
- Attempt closure maps: transport error → retryable `AiError::Http`; non-2xx → retryable iff `is_retryable(status)`, error = `AiError::Internal("whisper {status}: {body}")`.
- **Invariant**: final-attempt error keeps the exact `"whisper {status}:"` prefix so `definitive_no_charge` classifies it as no-charge → `cancel` (unchanged behavior).

### REQ-4 — Minimal test seams (required for the acceptance tests)

- `VoyageEmbedder::with_base_url(self, base_url: impl Into<String>) -> Self` — mirrors `AnthropicClient::with_base_url` (`anthropic.rs:72`); `new`/`from_env` default to the existing `VOYAGE_URL` const. Additive, no call-site changes.
- `WhisperTranscriber::new(api_key: impl Into<String>, model: impl Into<String>) -> Self` + `with_base_url(self, base_url: impl Into<String>) -> Self` — mirror the `AnthropicClient` shape; `from_env()` delegates to `new` with the env-derived values (default base URL = existing `https://api.openai.com/v1`). This replaces the current no-constructor state (`transcribe.rs:34`) and makes the mock-server tests race-free (no `std::env::set_var` in parallel tests).
- No changes to `AccountedToolChat`, `complete_accounted`, `complete_stream_accounted`, worker, or any server route.

### REQ-5 — Accounting invariant (no double charge)

- Retries occur **inside** `embed_with` / `transcribe`, i.e. *before* control returns to `embed_text_with_context` / `embed_query_with_context` / `transcribe_with_context` (`accounting.rs`). Those wrappers are untouched: exactly one `reserve` → (N internal HTTP attempts) → exactly one `finalize` (success) or one `settle_provider_error` (failure) per logical operation. `UsageSink::finalize` is called at most once per logical operation; retried requests that failed never reach the sink.
- On final failure: `AiError::Embedding`/`AiError::Internal` (status-shaped) → reservation cancelled (definitive no-charge, as today); `AiError::Http` (transport) → reservation retained as ambiguous (as today). No accounting behavior change.

---

## 5. Acceptance criteria (testable; preserved from the direction + concretized)

Test harness: local TCP mock server per the existing `anthropic_stub` pattern (`accounting/tests.rs:263` — tokio `TcpListener` on `127.0.0.1:0`, per-request `AtomicUsize` counter, hand-written HTTP response with `content-length`). Tests use `base = 1ms` so the suite stays fast. Provider tests live in the `#[cfg(test)]` mods of `embed.rs` / `transcribe.rs`; the accounting test in `service/accounting/tests.rs` reusing `CaptureSink` (`accounting/tests.rs:81`).

- **AT-1 (429 → 200)** — mock returns `429` on request 1 and a valid `200` on request 2. Assert: `VoyageEmbedder::embed_one` (via `with_base_url`) succeeds with a 1024-dim vector, and the server counter == **exactly 2**. Repeat for `WhisperTranscriber::transcribe` (mock returns `{"text": "hello"}` on the 200) — succeeds, counter == 2. Voyage mock body must carry `{"data":[{"embedding":[1024 floats], "index":0}]}` (dimension check at `embed.rs:139-144`).
- **AT-2 (persistent 5xx)** — mock returns `500` on every request. Assert: `VoyageEmbedder` call returns `Err(AiError::Embedding(..))` whose message starts with `voyage 500`, after **exactly 3** requests; `WhisperTranscriber` returns `Err(AiError::Internal(..))` whose message starts with `whisper 500`, after **exactly 3** requests.
- **AT-3 (no double charge)** — `AiService` with real `VoyageEmbedder` (`with_base_url` → mock) + `CaptureSink`: (a) 429→200 mock, `embed_query_with_context(..)` succeeds → `sink.finalized == 1`, `sink.cancelled == 0`, server counter == 2; (b) persistent-500 mock, same call → `Err`, `sink.finalized == 0`, `sink.cancelled == 1`, server counter == 3. This pins "exactly one `finalize` per logical operation" and that the definitive-no-charge classification still holds after retry exhaustion.
- **AT-4 (retry classification guard)** — mock returns `400` on every request: single request (counter == 1), immediate `Err(AiError::Embedding(..))`, no sleep — pins the `is_retryable` contract (non-429 4xx fail fast) mirrored from `anthropic.rs:672`. Same for Whisper (`AiError::Internal`, counter == 1).
- **AT-5 (regression gates)** — `cargo test --workspace --lib` green (existing 110+ `aero-ai` tests untouched, including `anthropic.rs` tests after it adopts the shared `is_retryable`); `cargo clippy --workspace --all-targets` introduces no new warnings (AGENTS.md §4.2).

## 6. Risks / notes for the implementer

- **Latency**: worst case per call ≈ 3 × client timeout + 3s backoff (Voyage ≈ 93s, Whisper ≈ 183s) on the interactive `/ask`/search/find-expert paths. Accepted precedent: the Anthropic path already behaves this way (`anthropic.rs:59`, 60s × 3). Do not reduce per-attempt timeouts in this change.
- **Error-shape coupling**: `definitive_no_charge` parses `"voyage {status}:"` / `"whisper {status}:"` prefixes (`accounting.rs` `has_http_failure_status`). Any refactor of the error formatting breaks accounting classification — AT-2/AT-3 guard this.
- **Idempotency of retried POSTs**: re-sending the identical embeddings/transcriptions body is safe — Voyage/OpenAI bill per completed request; a 429/5xx/transport attempt never produces a billed duplicate. The operation-id + reservation (`usage.rs` `UsageContext::operation_id`) is minted once per logical operation, not per attempt.
- **`anthropic.rs` refactor is limited to** swapping its private `is_retryable` for the shared one — behavior must stay byte-identical (existing tests + AT-5 cover it).
