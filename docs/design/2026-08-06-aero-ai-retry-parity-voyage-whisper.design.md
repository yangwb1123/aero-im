# Design — Retry/backoff parity for Voyage embedder & Whisper transcriber

- **Module**: `crates/aero-ai` (leaf dependency of `aero-server`; no other crate touches these types except via the `Embedder`/`Transcriber` traits)
- **Requirements**: `docs/requirements/2026-08-06-aero-ai-retry-parity-voyage-whisper.req.md` (REQ-1…REQ-5, AT-1…AT-5)
- **Date**: 2026-08-06 · **Status**: design (unimplemented) · **Change scope**: one crate, no DB migration, no config/env change, no new dependencies

---

## 0. Evidence verification (all cited symbols re-checked against source)

Every claim in the requirements spec was re-verified against `master` before designing. Result: **all confirmed**, with two trivial line-number drifts (symbols exact, line offsets ≤ 6):

| Spec claim | Verified location |
|---|---|
| `AnthropicClient` retry loop: 3 attempts, 1s/2s/4s backoff, retry on 429/5xx/transport | `anthropic.rs:211-247` (`let resp = loop {` at 212; sleep `Duration::from_secs(1 << (attempt - 1))` at 233/242; `attempt >= max_attempts` final-delivery at 229) |
| `is_retryable` = `429 ‖ is_server_error()` | `anthropic.rs:672` — private `fn`, `#[must_use]` |
| `VoyageEmbedder::embed_with` — single `.send().await?` at `embed.rs:121`, URL is const `VOYAGE_URL` at `embed.rs:23`, no `base_url` field | ✅ exact |
| Empty-text early return before any HTTP | `embed.rs:110-114` (zero vector, no retry — unchanged) |
| `WhisperTranscriber` — single send at `transcribe.rs:77`; **only** constructor is env-reading `from_env()` at `transcribe.rs:34` | ✅ exact |
| Interactive RAG embedding leg is `?`-fatal vs FTS warn-and-degrade | `service/service_impl.rs:250` (`retrieve_room_with_context`), `:277` (`retrieve_workspace_with_context`), `:1110` (`find_expert_with_usage_context`); FTS degrade at 255-262 |
| Queue-backed Embed jobs are mitigated by retry-on-claim | `worker/mod.rs:314` (`handle_embed` → `embed_text_with_context`; `MAX_ATTEMPTS=5`) |
| `settle_provider_error`: status-shaped errors → definitive no-charge → `cancel`; `AiError::Http` → ambiguous → reservation retained | `service/accounting.rs:415-440` (settle), `:775-784` (`definitive_no_charge`), `:786-793` (`has_http_failure_status` parses `"voyage "` / `"whisper "` prefixes) |
| Reserve → provider call → finalize/cancel exactly once per logical op | `accounting.rs` `embed_text_with_context` (~655), `embed_query_with_context` (~688), `transcribe_with_context` (~755) |
| Mock-server precedent `anthropic_stub` | `service/accounting/tests.rs:257` (spec said 263 — fn signature is at 257; pattern identical) |
| `CaptureSink` with `finalized`/`cancelled` atomics | `service/accounting/tests.rs:54-58` (spec said 81 — struct at 54; symbol exact) |
| No new deps needed (dev-deps = `pretty_assertions` + `tokio` only) | `crates/aero-ai/Cargo.toml` ✅ |
| `AiError::Http(String)` + `From<reqwest::Error>` | `error.rs:14, 49-51` — transport errors already surface as `AiError::Http`, which `definitive_no_charge` treats as ambiguous ✅ |

**One addition to the spec's risk list (confirmed by reading `anthropic.rs` tests)**: the anthropic retry loop itself has **no direct 429/5xx mock tests** today (20 in-file tests + accounting tests exercise it only via 200-path). The new AT-1…AT-4 mock-server tests therefore become the first direct coverage of this retry semantics; the shared helper is what they pin.

---

## 1. Design overview

```
┌─ accounting.rs (UNTOUCHED) ─────────────────────────────────────────┐
│  reserve → (exactly one logical provider call) → finalize | cancel  │
└───────────────────────────────┬─────────────────────────────────────┘
                                │ one call, retries are INSIDE
        ┌───────────────────────▼──────────────────────────┐
        │ embed_with / transcribe (modified)               │
        │   retry_with_backoff(3, 1s, attempt_closure)     │
        │     ├─ attempt 1: POST ── 429/5xx/transport ──┐  │
        │     ├─ attempt 2: POST ── 429/5xx/transport ──┤  │
        │     └─ attempt 3: POST ── final result/error  ◄┘  │
        └───────────────────────────────────────────────────┘
```

- **Retries live inside the provider calls**, before control returns to `accounting.rs`. The accounting wrappers (`embed_text_with_context`, `embed_query_with_context`, `transcribe_with_context`) are byte-identical: one `reserve`, one `finalize`/`cancel` per logical operation. AT-3 pins this.
- **Shared semantics module** `crates/aero-ai/src/retry.rs` (crate-private `mod retry;` in `lib.rs`) holds the retry classification + backoff, extracted from the anthropic loop; `anthropic.rs` adopts the shared `is_retryable` and nothing else (its inline loop stays — minimal blast radius per req §6).
- **Two additive constructors** unblock the mock-server tests: `VoyageEmbedder::with_base_url`, `WhisperTranscriber::new` + `with_base_url`, mirroring `AnthropicClient::new`/`with_base_url` (`anthropic.rs:56-72`). No call-site changes.

---

## 2. API changes (concrete)

### 2.1 New module `crates/aero-ai/src/retry.rs`

```rust
//! Shared HTTP retry semantics — extracted from the Anthropic client loop
//! (anthropic.rs) so the Voyage and Whisper paid legs get identical
//! 429/5xx/transport retry with exponential backoff (requirements
//! 2026-08-06-aero-ai-retry-parity-voyage-whisper).

use std::time::Duration;
use crate::error::{AiError, Result};

/// Production backoff base: 1s ⇒ delays 1s, 2s, 4s — byte-identical to the
/// anthropic loop's `1 << (attempt - 1)` seconds.
pub(crate) const DEFAULT_RETRY_ATTEMPTS: usize = 3;
pub(crate) const DEFAULT_RETRY_BASE: Duration = Duration::from_secs(1);

/// Result of one attempt: `retryable` decides whether the loop sleeps and
/// re-invokes; `error` is delivered as-is when the attempt is final.
pub(crate) struct AttemptError {
    pub retryable: bool,
    pub error: AiError,
}

/// 429 (rate limit) and 5xx (server error) are retryable; other 4xx are
/// client errors and fail immediately. Byte-identical to anthropic.rs:672.
#[must_use]
pub(crate) fn is_retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// `base * 2^(attempt-1)` — attempt is 1-indexed: 1→base, 2→2×base, 3→4×base.
#[must_use]
pub(crate) fn backoff_delay(attempt: usize, base: Duration) -> Duration {
    // clippy-pedantic-clean: no `as` casts (cast_possible_truncation is warn).
    let exp = u32::try_from(attempt.saturating_sub(1)).unwrap_or(0);
    let factor = 1u32.checked_shl(exp).unwrap_or(u32::MAX);
    base.saturating_mul(factor)
}

/// Run `attempt` up to `max_attempts` times:
/// - `Ok(v)` → `Ok(v)` immediately (no extra requests);
/// - non-retryable error → `Err` immediately (no sleep, no extra request);
/// - retryable error while `attempt < max_attempts` → `sleep(backoff_delay(attempt, base))`, re-invoke;
/// - retryable error on the final attempt → `Err` with the **last** attempt's error.
///
/// Semantics mirror anthropic.rs:211-247 (which keeps its inline loop).
/// Tests pass `Duration::ZERO`/1ms so the suite stays fast.
pub(crate) async fn retry_with_backoff<T, F, Fut>(
    max_attempts: usize,
    base: Duration,
    mut attempt: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, AttemptError>>,
{
    let mut n = 0;
    loop {
        n += 1;
        match attempt().await {
            Ok(v) => return Ok(v),
            Err(a) if a.retryable && n < max_attempts => {
                let delay = backoff_delay(n, base);
                tracing::debug!(attempt = n, delay = ?delay, "ai provider retry");
                tokio::time::sleep(delay).await;
            }
            Err(a) => return Err(a.error),
        }
    }
}
```

`lib.rs`: add `mod retry;` (private module — all items `pub(crate)`; no `unreachable_pub`, no public-surface growth).

### 2.2 `VoyageEmbedder` (`embed.rs`) — additive only

- Add fields `base_url: String` (default = existing const `VOYAGE_URL` — **keep the const name and value**; zero churn) and `retry_base: Duration` (default = `DEFAULT_RETRY_BASE`, i.e. production 1s).
- Add `#[must_use] pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self` (mirror `anthropic.rs:72`).
- Add crate-private `pub(crate) fn with_retry_base(mut self, base: Duration) -> Self` — the test-timing seam (REQ-1/§5: tests run at 1ms). Not re-exported; invisible to `aero-server`; production constructors never call it, so production is always 1s/2s/4s. No env vars ⇒ race-free.
- `from_env()` unchanged (already routes through `new`).
- `embed_with` URL becomes `format!("{}/v1/embeddings", self.base_url.trim_end_matches('/'))` — note the const already ends without `/`, so production URL is byte-identical.
- Wrap the POST (currently `embed.rs:121`) in `retry_with_backoff(DEFAULT_RETRY_ATTEMPTS, DEFAULT_RETRY_BASE, || async { … })`:

```rust
let result = retry_with_backoff(DEFAULT_RETRY_ATTEMPTS, self.retry_base, || async {
    match self
        .http
        .post(&url)
        .bearer_auth(&self.api_key)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        // Transport error: retryable — same variant as today's `?` (AiError::Http),
        // so accounting classification (ambiguous → retain) is unchanged.
        Err(e) => Err(AttemptError { retryable: true, error: AiError::Http(e.to_string()) }),
        Ok(resp) => {
            let status = resp.status();
            // Body read inside the closure: a reset mid-body is a retryable
            // transport error, not a final one (more resilient than today's `?`).
            let raw = match resp.text().await {
                Ok(raw) => raw,
                Err(e) => return Err(AttemptError { retryable: true, error: AiError::Http(e.to_string()) }),
            };
            if !status.is_success() {
                let msg = format!(
                    "voyage {}: {}",
                    status.as_u16(),
                    raw.chars().take(512).collect::<String>()
                );
                // INVARIANT: exact "voyage {status}:" prefix — accounting.rs
                // has_http_failure_status parses it (definitive no-charge → cancel).
                return Err(AttemptError { retryable: is_retryable(status), error: AiError::Embedding(msg) });
            }
            // Parse + dim check are NON-retryable (deterministic client-side or
            // corrupt-200; retrying the identical body cannot fix them) — matches
            // anthropic, where parsing happens after the retry loop.
            match serde_json::from_str::<VoyageResponse>(&raw) {
                Ok(parsed) => { /* existing .data.into_iter().next() + dim check, same errors */ }
                Err(e) => return Err(AttemptError { retryable: false, error: AiError::Json(e.to_string()) }),
            }
        }
    }
}).await;
result
```

Mapping table (today → after):

| Outcome today | After retry |
|---|---|
| 2xx + parse/dim ok | `Ok(vec)` on first success — unchanged |
| non-2xx `voyage {status}: {body512}` | final-attempt error, same string; `429`/`5xx` retried, other 4xx fail fast |
| transport `AiError::Http` | retried ×3, then `AiError::Http` of the **last** attempt — variant unchanged |
| 200 + bad JSON `AiError::Json` | immediate, no retry — variant unchanged |
| 200 + wrong dim `AiError::Embedding("voyage returned dim …")` | immediate, no retry — unchanged (prefix `voyage ` with no status → `has_http_failure_status` = false → ambiguous, same as today) |
| empty text | zero vector, **no HTTP at all** — unchanged |

### 2.3 `WhisperTranscriber` (`transcribe.rs`) — additive constructors + retry

```rust
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

impl WhisperTranscriber {
    /// Mirror AnthropicClient::new: explicit creds, default base URL.
    #[must_use]
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { api_key: api_key.into(), base_url: DEFAULT_BASE_URL.to_string(), model: model.into(), http, retry_base: DEFAULT_RETRY_BASE }
    }

    /// Override the API base URL (proxies and tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self { … }

    /// Crate-private test seam: 1ms base in tests, 1s in production (REQ-1/§5).
    pub(crate) fn with_retry_base(mut self, base: Duration) -> Self { … }

    #[must_use]
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("OPENAI_API_KEY").ok()?;
        let model = std::env::var("WHISPER_MODEL").unwrap_or_else(|_| "whisper-1".into());
        let mut t = Self::new(api_key, model);
        if let Ok(base) = std::env::var("OPENAI_BASE_URL") {
            if !base.trim().is_empty() { t = t.with_base_url(base); }
        }
        Some(t)
    }
}
```

- `transcribe`: wrap the POST (currently `transcribe.rs:77`) in `retry_with_backoff(DEFAULT_RETRY_ATTEMPTS, self.retry_base, …)`. The multipart form is **rebuilt inside the closure** (`bytes.to_vec()` per attempt — `Bytes` is a ref-counted cheap clone; `file_name`/`mime` mapping computed once before the loop, it's a pure fn of `mime`).
- Attempt closure mapping: transport → retryable `AiError::Http`; non-2xx → `AttemptError { retryable: is_retryable(status), error: AiError::Internal(format!("whisper {status}: {body}")) }` with `body = resp.text().await.unwrap_or_default()` (keep `unwrap_or_default`, not `?`); 2xx JSON parse error → non-retryable (`AiError::Json`, today's `resp.json().await?` shape). `multipart mime:` construction error → non-retryable (`definitive_no_charge` treats it as definitive no-charge today — preserved).
- `from_env` behavior delta (documented, acceptable): `http` builder failure was `None` (fail-closed) and becomes `reqwest::Client::new()` fallback — exactly the `AnthropicClient::new` precedent (`anthropic.rs:59-62`).

### 2.4 `AnthropicClient` (`anthropic.rs`) — 3-line refactor only

- Delete private `fn is_retryable` at `:672-676`; call site at `:229` uses `crate::retry::is_retryable`.
- Inline loop, backoff literals, error construction: **untouched** (behavior byte-identical; 20 in-file tests + accounting tests are the guard).

### 2.5 Explicitly untouched

`complete_stream`, `complete_with_tools`, `run_agent_loop`, `AccountedToolChat`, `complete_accounted`, `complete_stream_accounted`, `AiWorker`, all `service_impl.rs` retrievers, all routes, `aero-server`, `web/`, migrations, config, env parsing.

---

## 3. Compatibility constraints

1. **Error-string contract with accounting** (`accounting.rs:775-793`): final-attempt errors must keep `"voyage {status}:"` / `"whisper {status}:"` as the **message prefix** with the status parsed as a bare `u16` before the first `:` — `has_http_failure_status` does `strip_prefix("voyage ").and_then(split(':')).parse::<u16>()`. Breaking the shape silently reclassifies a definitive no-charge into an ambiguous retained reservation (conservative charge). AT-2/AT-3 guard.
2. **Variant stability**: transport failures remain `AiError::Http` (→ ambiguous, retained — unchanged); status failures remain `AiError::Embedding`/`AiError::Internal` (→ definitive cancel — unchanged). `definitive_no_charge`'s catch-alls (`AiError::Invalid`, `Anthropic`, `multipart mime:` prefix) untouched.
3. **Public API**: strictly additive. Existing signatures (`from_env` both, `new` for Voyage) unchanged; new methods are `#[must_use]` builder-style, mirroring `AnthropicClient`. `lib.rs` re-exports unchanged (specifically: do **not** re-export `retry` — it is crate-private).
4. **Backoff parity**: production `base = 1s`, `max_attempts = 3` ⇒ 1s/2s/4s — numerically identical to `anthropic.rs:233/242` (`1 << (attempt - 1)` seconds). `is_retryable` is the same function object moved.
5. **No DB migration, no config/env change, no new dependency** — `tokio::time::sleep` + existing `reqwest`/`serde_json`/`bytes`; dev-deps remain `pretty_assertions` + `tokio`. Therefore AGENTS.md §4.2's "`cargo build` before `aero-cli migrate`" rule does **not** apply — there is no `migrations/` change.
6. **Timeouts unchanged** (Voyage 30s, Whisper 60s). Do not reduce per-attempt timeouts to compensate for retries.
7. **Idempotency**: retried POSTs re-send the identical body; a 429/5xx/transport attempt never produces a billed completion, so no duplicate charge. The `UsageContext::operation_id` + reservation is minted once per logical operation (`usage.rs`), not per attempt — untouched.

---

## 4. Failure modes & mitigations

| # | Failure mode | Impact | Mitigation / accepted trade-off |
|---|---|---|---|
| F1 | **Latency growth**: worst case ≈ 3 × client timeout + 3s backoff — Voyage ≈ 93s, Whisper ≈ 183s | Interactive `/api/rooms/:id/ask`, `/ask/context`, hybrid search, `find-expert`, `transcribe_bot` hold longer | Accepted precedent: anthropic already pays 60s×3 (`anthropic.rs:59`). Retries only trigger on 429/5xx/transport — healthy-path latency unchanged (single attempt). Do **not** lower per-attempt timeouts (constraint 6). |
| F2 | **Error-shape drift** breaks billing classification | `voyage {status}:` / `whisper {status}:` prefix lost ⇒ `has_http_failure_status` returns false ⇒ final 5xx treated as ambiguous ⇒ conservative retained reservation (over-charge) | Prefix is produced in one place per provider (the non-2xx branch), pinned by AT-2 (prefix assertions) + AT-3 (cancel counter). Code-review note: never reorder the format string. |
| F3 | **Double finalize/cancel** if retries were placed outside the provider | Double billing or double-cancel | Structural: retry loop is inside `embed_with`/`transcribe`; accounting wrappers untouched; `UsageSink::finalize` called at most once per logical op (AT-3 asserts counters). |
| F4 | **Body read failure mid-retry** (`resp.text()` transport error on a retryable status) | Today: fatal `?`. After: classified retryable → extra attempts | Deliberate improvement; on final attempt delivers `AiError::Http` (ambiguous, retained — conservative, same as today's transport handling). |
| F5 | **Parse/dim errors on 200** (corrupt 200 body, dim mismatch) | Not retried — deterministic, retrying the identical body cannot fix | Matches anthropic (parse after loop). Classification unchanged (Json → ambiguous). |
| F6 | **Stub/test hangs** if the mock never responds | CI deadlock | Mock pattern: `connection: close` + explicit `content-length`, per-connection accept loop (existing `anthropic_stub` precedent). Whisper stub must **drain `content-length` request bytes** before responding to avoid TCP backpressure deadlock on the multipart body (small bodies, but drain is deterministic). |
| F7 | **429 storm amplification** — 3 attempts × interactive callers under rate limit | More load on a limping provider | Same as anthropic today; backoff 1s/2s is modest. Out of scope to add jitter (parity, not improvement). |
| F8 | **Refactor regression in anthropic** when adopting shared `is_retryable` | Behavior drift in the most-used AI path | Refactor is a 3-line swap of an identical expression; existing 20 anthropic tests + accounting digest/stream tests + AT-5 gates cover it. |
| F9 | **Worker double-burn**: embed job retries inside provider **and** on claim | Attempts consumed faster? | No — retries happen inside one `handle_embed` invocation; the job's `attempts` counter increments once per logical operation. Side benefit: transient errors no longer burn one of `MAX_ATTEMPTS=5` (req §2). |

---

## 5. Migration steps (implementation order)

> **No DB migration** — nothing in `migrations/`, no `aero-cli migrate`, no config/env. The AGENTS.md §4.2 build-before-migrate cycle does not apply. Each step keeps the workspace green.

1. **`retry.rs`** — new module (2.1) + `mod retry;` in `lib.rs`. `cargo check -p aero-ai`.
2. **`anthropic.rs` swap** — delete private `is_retryable` (`:672`), import `crate::retry::is_retryable`. `cargo test -p aero-ai` (20 in-file + accounting tests must stay green — proves byte-identical semantics).
3. **`embed.rs`** — add `base_url` field + `with_base_url`; switch URL construction; wrap POST in `retry_with_backoff` per 2.2. `cargo check -p aero-ai`.
4. **`transcribe.rs`** — add `new` + `with_base_url`; `from_env` delegates; wrap POST per 2.3. `cargo check -p aero-ai`.
5. **Tests** — mock-server tests in `embed.rs`/`transcribe.rs` `#[cfg(test)]` mods (AT-1/2/4) + accounting test with real Voyage + `CaptureSink` (AT-3). Harness sketch in §6.
6. **Gates (AGENTS.md §4.3 commit checklist)**:
   - `cargo check --workspace` — clean;
   - `cargo test --workspace --lib` — green (existing 110+ aero-ai tests untouched in behavior);
   - `cargo clippy --workspace --all-targets` — **no new warnings** (sketch avoids `as` casts; `unreachable_pub` safe since `mod retry` is private);
   - `scripts/truth-check.sh` — `retry.rs` has 3 call sites (embed, transcribe, anthropic), no orphan module; `scripts/file-size-check.sh` — `retry.rs` ≈ 60 lines ≪ 800 WARN;
   - `scripts/web-check.sh` — untouched (no web change).
7. **Smoke (optional, live)** — `AERO_HOST`-style local run: no config change means existing smoke paths unaffected; provider calls only occur with keys set. Not required for this change.

---

## 6. Testable acceptance mapping

**Harness** (reuses `anthropic_stub` precedent, `accounting/tests.rs:257`): a status-sequenced variant —

```rust
// embed.rs #[cfg(test)] mod — mirror in transcribe.rs with a multipart-aware stub.
async fn voyage_stub(
    calls: Arc<AtomicUsize>,
    statuses: &'static [u16],          // index = attempt-1; LAST entry repeats (persistent-5xx)
    body: &'static str,                // 200 body
) -> (VoyageEmbedder, tokio::task::JoinHandle<()>)
```

- Tokio `TcpListener` on `127.0.0.1:0`; per accepted connection: `calls.fetch_add(1)`, read until `\r\n\r\n`, **drain `content-length` bytes** (whisper variant; embed body is tiny but drain both for uniformity), then write `HTTP/1.1 {status} …\r\ncontent-length: {n}\r\nconnection: close\r\n\r\n{body}`.
- Tests construct providers with `.with_base_url(format!("http://{address}"))` — **race-free**: no `std::env::set_var` anywhere (the reason for REQ-4).
- Backoff: tests construct providers with `.with_retry_base(Duration::from_millis(1))` (crate-private seam from §2.2/2.3) so AT-1…AT-4 run at 1ms backoff — suite cost ≈ 0s of sleeps, per REQ-1/§5 intent. Production constructors never touch the seam, so deployment is always 1s/2s/4s; `retry_with_backoff`'s `base` parameter is exercised by the provider call sites (`self.retry_base`) and by the fast helper unit tests (1ms).

| Acceptance | Test (file) | Assertions | Failure if |
|---|---|---|---|
| **AT-1** 429→200 | `embed.rs::tests::retry_429_then_200_succeeds` + `transcribe.rs::tests::retry_429_then_200_succeeds` | `embed_one("…")` → `Ok` 1024-dim; `calls == 2`. `transcribe(Bytes, "audio/webm")` → `Ok("hello")`; `calls == 2` | retry not wired / wrong retryability / stub broken |
| **AT-2** persistent 5xx | `embed.rs::tests::retry_persistent_500_fails_after_3` + whisper twin | `Err(AiError::Embedding(msg))` with `msg.starts_with("voyage 500")`; `calls == 3`. Whisper: `Err(AiError::Internal)` starts `whisper 500`; `calls == 3` | max-attempts off / error prefix drift (F2) |
| **AT-3** no double charge | `accounting/tests.rs::retry_429_then_200_finalizes_exactly_once` + `retry_exhausted_500_cancels_once` — `AiService::new(..)` with `Arc::new(VoyageEmbedder::new("k","m").with_base_url(addr))` + `CaptureSink` (`accounting/tests.rs:54`) | (a) `embed_query_with_context` → `Ok`; `sink.finalized == 1`, `sink.cancelled == 0`, `calls == 2`; (b) persistent 500 → `Err`; `sink.finalized == 0`, `sink.cancelled == 1`, `calls == 3` | double finalize / classification drift (F2/F3) |
| **AT-4** 4xx fail-fast | `embed.rs::tests::retry_400_fails_fast` + whisper twin | 400 on every request: `calls == 1`, `Err` message starts `voyage 400` / `whisper 400` (immediate, no sleep — assert elapsed < 100ms optional) | `is_retryable` contract broken |
| **AT-5** regression gates | CI / commit checklist | `cargo test --workspace --lib` green incl. 20 anthropic tests after shared-`is_retryable` swap; `cargo clippy --workspace --all-targets` zero new warnings; `scripts/truth-check.sh` 0 violations | shared-helper refactor regressed anthropic (F8) |

Plus two fast unit tests for the helper itself (1ms base, no mock server): `retry_with_backoff_returns_last_error_after_exhaustion` and `retry_with_backoff_non_retryable_is_immediate` — these pin the loop semantics and are what REQ-1's `base` parameter exists for.

**Mapping to requirements**: REQ-1 → §2.1 + helper unit tests; REQ-2 → §2.2 + AT-1/2/4 (embed); REQ-3 → §2.3 + AT-1/2/4 (whisper); REQ-4 → §2.2/2.3 constructors (used by every AT); REQ-5 → §2 design invariant + AT-3.

---

## 7. Out of scope (unchanged from requirements §3)

Streaming/tool-call paths, accounting protocol, timeouts, new deps, server/routes/web, DB migrations, config/env. `complete_stream_accounted`'s durable-chunk replay (`accounting.rs:564/644`) is untouched.
