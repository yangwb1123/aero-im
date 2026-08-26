# Requirements Spec — Fix silent UTF-8 data loss in the Anthropic SSE parser

- **Module**: `crates/aero-ai`
- **Source direction**: `docs/auto/analyses/crates-aero-ai-f8cd3622.json` (direction 1)
- **Date**: 2026-08-06 · **Status**: implemented in source and regression-tested
- **Scope**: byte-preserving incremental UTF-8 handling in `SseParser` (`crates/aero-ai/src/anthropic.rs`) so a multi-byte character split across network chunks is never dropped, plus CJK/emoji/property regression tests. Accounting, retry policy, other provider paths, and wire format remain unchanged by this fix.

> Current-source note (2026-08-19): `SseParser` retains incomplete UTF-8 tails,
> warns on genuine mid-stream corruption, and the external `anthropic/tests.rs`
> module covers two-/three-chunk CJK, four-push emoji, every split offset, empty
> pushes, and corruption handling. The old all-or-nothing `from_utf8` behavior
> described below is historical evidence for the direction.

---

## 1. Evidence verification (all direction citations re-checked against `master`)

| Direction claim | Verified finding |
|---|---|
| `SseParser::push_bytes` at `anthropic.rs:618-621` does `if let Ok(s) = std::str::from_utf8(bytes) { self.buffer.push_str(s) }` and silently drops invalid chunks | ✅ exact — `crates/aero-ai/src/anthropic.rs:618-621`:
  ```rust
  fn push_bytes(&mut self, bytes: &Bytes) {
      if let Ok(s) = std::str::from_utf8(bytes) {
          self.buffer.push_str(s);
          self.drain_events();
      }
  }
  ```
  No `else` branch: no event, no error, no log. `drain_events` at 625-633, `extract_text_delta` at 644-666. `SseParser` (612) is `String`-buffered only — there is no partial-UTF-8 retention mechanism anywhere. Sole production consumer: `complete_stream`'s `futures::stream::unfold` at 383 feeds `parser.push_bytes(&bytes)` |
| `complete_stream` reads `resp.bytes_stream()` — arbitrary byte boundaries | ✅ `crates/aero-ai/src/anthropic.rs:339` (`pub async fn complete_stream`), `resp.bytes_stream()` at 380; each yielded `Bytes` is fed whole into `push_bytes`. reqwest makes no UTF-8-boundary guarantee |
| CJK text is the normal case on this path | ✅ Chinese-first product, code-verified: `ANSWER_SYSTEM_PROMPT` (`service/tools.rs:60`) ends with `用中文回答`; the ask/stream user prompt is Chinese (`service_impl.rs:572` — `问题: {q}\n\n…请基于上述上下文作答…`); streamed via `complete_stream_accounted(client, …, ANSWER_SYSTEM_PROMPT, …, 800)` at `service_impl.rs:574-587`; consumed by the `/ask/stream` SSE route (`server/src/routes/ai.rs:151` → `answer_question_stream_with_usage_context`) |
| Persisted `StreamOutcome` replay payload saves the corrupted chunks | ✅ `crates/aero-ai/src/service/accounting.rs:544` (`complete_stream_accounted`) collects every yielded chunk into `StreamOutcome { chunks: Vec<String>, terminal_error }` (56-59) and persists it via `sink.finalize(reservation, …, Some(UsageOutcome { kind: STREAM_OUTCOME, payload }))` (606-620); on `ProviderReservation::Replayed` (558-570) the saved `chunks` are re-served verbatim — a retry replays the same loss |
| Existing test only splits at ASCII boundaries | ✅ `anthropic.rs:961-985` `sse_parser_buffers_across_chunks` splits the JSON *key* (`…content_block_delt` / `a"…`) and payload `"Hi"` — every chunk is individually valid UTF-8, so `from_utf8` always succeeds and the bug is untested. Baseline: `cargo test -p aero-ai sse_parser` → 5/5 green on `master` |

**Impact analysis (sharpened from the direction, code-verified)** — the loss is *larger* than a single character. If chunk N ends with the leading byte of a CJK char (e.g. `中` = `E4 B8 AD`):

1. Chunk N alone fails `from_utf8` → **the entire chunk is dropped**, including any complete SSE events earlier in that same chunk.
2. Chunk N+1 starts with continuation bytes (`B8 AD`), which are invalid at position 0 → **it is dropped too**.
3. The yielded text therefore has a gap covering every event in both boundary chunks; `max_tokens = 800` (`service_impl.rs:586`) means a non-trivial Chinese answer spans many chunks, so at least one boundary lands inside a 3-byte char with near-certainty. The `StreamOutcome` replay payload then bakes the gap in durably.

The truncation scenario and its user-visible frequency are reasoned from code inspection (direction's own caveat) — no live repro exists; the spec's tests below are the first executable evidence.

---

## 2. Problem statement (historical gap; now closed by incremental decoding)

- `SseParser::push_bytes` (`anthropic.rs:618-621`) treats a chunk as all-or-nothing: any chunk that is not wholly valid UTF-8 is silently discarded. reqwest's `bytes_stream()` (`anthropic.rs:380`) yields at arbitrary byte boundaries, so a 3-byte CJK character split across two chunks produces two individually-invalid chunks, and **every SSE event contained in either chunk is lost without any error, log, or accounting signal**.
- The affected path is the flagship interactive RAG stream: `/api/rooms/:id/ask/stream` (`server/src/routes/ai.rs`) → `answer_question_stream_with_usage_context` (`service_impl.rs:543`) → `complete_stream_accounted` (`accounting.rs:544`) → `complete_stream` (`anthropic.rs:339`). The system prompt mandates Chinese output (`tools.rs:60`), so multi-byte characters are guaranteed in replies.
- The corruption is durable, not transient: `complete_stream_accounted` persists the corrupted `chunks` into the `STREAM_OUTCOME` reservation (`accounting.rs:56-59, 606-620`) and replays them verbatim on retry (`accounting.rs:558-570`).
- The existing test (`sse_parser_buffers_across_chunks`, `anthropic.rs:961-985`) only splits ASCII, so the parser's UTF-8 behavior is entirely untested.

---

## 3. Non-goals (explicitly out of scope)

1. **No accounting changes** — `accounting.rs` (`complete_stream_accounted`, `StreamOutcome`, reservation protocol) is untouched. The fix is purely inside `SseParser`; the persisted payload automatically becomes correct once the parser yields correct chunks.
2. **No retry/backoff changes** — single-`send` behavior of `complete_stream` stays (retry parity is a separate direction).
3. **No `message_delta` usage parsing** — `extract_text_delta` keeps dropping non-`text_delta` events (usage accounting is a separate direction).
4. **No lossy replacement** — `String::from_utf8_lossy` is explicitly out: substituting U+FFFD for a split char is still data corruption and would corrupt the `text` JSON payload mid-string.
5. **No new dependencies** — `bytes::Bytes` and `std` suffice; tests live in the existing `#[cfg(test)] mod tests` of `anthropic.rs`.
6. **No wire-format or `SseParser` API changes** — `push_bytes(&Bytes)` / `next_chunk()` signatures stay; only the internal buffering and `push_bytes` body change.

---

## 4. Requirements

### REQ-1 — Byte-preserving incremental UTF-8 buffering in `SseParser` (`crates/aero-ai/src/anthropic.rs`)

Replace the all-or-nothing `from_utf8` gate (618-621) with a stateful incremental decode. Invariants (these are the contract; the exact implementation below is recommended, not mandated):

- **Every byte of every pushed chunk is either appended to `self.buffer` or retained internally for the next chunk. No byte is ever dropped, and no byte is ever replaced.**
- `self.buffer` only ever contains complete, valid UTF-8 (it stays a `String`; `drain_events` / `extract_text_delta` / `next_chunk` are unchanged).
- `drain_events()` is invoked after every push that appends bytes (same call-site behavior as today, so SSE latency is unchanged).

Recommended implementation (~10 lines, no new deps):

- Add a bounded tail buffer to `SseParser` (e.g. `pending: Vec<u8>`, at most 3 bytes — the max UTF-8 sequence length — so memory is bounded regardless of chunk sizes).
- On `push_bytes`: form `combined = pending.drain(..) ++ bytes`; call `std::str::from_utf8(&combined)`:
  - `Ok(s)` → `buffer.push_str(s)`, `drain_events()`.
  - `Err(e)` where `e.error_len() == None` → the error is an **incomplete multi-byte sequence at the tail** (the chunk ended mid-character); push the valid prefix `&combined[..e.valid_up_to()]`, retain `&combined[e.valid_up_to()..]` (1-3 bytes) in `pending` for the next chunk, `drain_events()`.
  - `Err(e)` where `e.error_len() == Some(_)` → genuinely malformed mid-stream data (not a boundary artifact); push the valid prefix, drop the invalid remainder, and emit `tracing::warn!` — never silent. (The Anthropic wire contract is JSON-escaped valid UTF-8, so this branch is defensive only.)

The `e.error_len() == None` distinction is the key: `std::str::Utf8Error::error_len()` returns `None` exactly when the invalid point is a truncated sequence at the end of the input, which is precisely the network-boundary case.

### REQ-2 — Regression tests in `crates/aero-ai/src/anthropic.rs` (`#[cfg(test)] mod tests`)

All tests use the existing module setup (`use super::*;`, `bytes::Bytes` already imported at `anthropic.rs:12`). Fixtures are real Anthropic SSE frames carrying CJK text (`中` = `E4 B8 AD`, `文` = `E6 96 87`).

- **CJK split across 2 chunks (the direction's exact scenario)**: chunk 1 ends with the leading byte of `中` (`…E4`), chunk 2 starts with its continuation bytes (`B8 AD…`). Assert: after both pushes, `next_chunk()` yields the full delta text with **zero dropped characters**; concatenated output == expected full text.
- **CJK split across 3 chunks**: lead byte in chunk 1, first continuation byte in chunk 2, second continuation byte + rest in chunk 3; same assertion.
- **Multi-event fixture with CJK at event boundaries**: two consecutive `content_block_delta` events whose text ends/starts mid-sentence, split so a `\n\n` separator lands inside one chunk and a CJK char straddles another boundary; assert both deltas are yielded intact and in order.
- **Property test — every split offset**: for a fixture with ≥2 events and ≥2 CJK chars (spanning at least one 3-byte char), enumerate **every** split point `s` in `1..fixture.len()-1`; feed `bytes[..s]` then `bytes[s..]`; assert (a) concatenation of all yielded chunks == expected full text, (b) number of yielded chunks == number of `text_delta` events (all event boundaries detected), (c) every yielded chunk is valid UTF-8. This subsumes all intra-char cuts of 1, 2, and 3-byte sequences (the direction's "1..=3-byte offset" requirement). Optional extension if cheap: a 3-chunk variant for every pair of cuts `s1 < s2` with `s2 - s1 <= 2` (both cuts inside one char).

### REQ-3 — Red-first regression marker (documents the bug before the fix)

- Land REQ-2's tests **before** applying REQ-1. Running `cargo test -p aero-ai` against the unmodified parser must fail on the new tests: with the current `push_bytes`, both boundary chunks fail `from_utf8` and are dropped, so the CJK split test observes `next_chunk() == None` (or a missing delta). Capture the failing test output (e.g. in the commit message or PR description) as the executable record that the bug is real.
- Then apply REQ-1; the same tests must pass without modification. The tests remain permanent regression coverage.

---

## 5. Acceptance criteria (testable; preserved from the direction + concretized)

- **AT-1 (CJK split across chunk boundaries)** — in `anthropic.rs` tests: first chunk ends with the leading byte of `中`/`文` (e.g. `…"text":"\u{4e2d}` truncated at `E4`), second chunk carries the continuation bytes. Assert: concatenated `next_chunk()` output equals the full text with **zero dropped characters**. Also the 3-chunk variant (AT-1b).
- **AT-2 (property test)** — a Chinese stream fixture (≥2 `content_block_delta` events, ≥2 CJK chars) split at **every** offset `s ∈ [1, len-1]`: full text preserved, yielded-event count == expected event count (all event boundaries detected). Implemented as a loop over split points; passes post-fix.
- **AT-3 (regression marker)** — the REQ-2 tests demonstrably fail against the current `from_utf8`-based implementation (run before the fix; failing output recorded), then pass after the fix. This is a process gate: the fix commit must contain the tests plus the recorded pre-fix failure evidence.
- **AT-4 (gates stay clean)** — `cargo test -p aero-ai` green (existing 5 `sse_parser` tests + new tests); `cargo clippy --workspace --all-targets` introduces no new warnings (AGENTS.md §4.2). `cargo test --workspace --lib` also stays green (AGENTS.md §4.3 commit gate).

---

## 6. Risks / notes for the implementer

- **`Utf8Error::error_len()` semantics** are the crux of the fix: `None` ⇔ truncated tail (retain in `pending`); `Some(_)` ⇔ invalid bytes in the middle (warn + drop remainder). Do not implement this by hand-rolling UTF-8 classification — the std error already encodes the distinction.
- **`pending` must be drained before appending the next chunk** (prepend order matters: continuation bytes belong *before* the new chunk's bytes, and a 3-byte char may span up to 3 chunks, so a single retained tail of ≤3 bytes is sufficient).
- **Do not touch `drain_events` / `extract_text_delta` / `next_chunk`** — the event-splitting logic is correct and covered by existing tests; only the byte→`String` boundary changes.
- **No behavior change for ASCII/valid chunks** — the `Ok` path is byte-identical to today; existing tests (`sse_parser_buffers_across_chunks`, `sse_parser_queues_multiple_chunks`) must pass unmodified.
- **Latency/streaming semantics unchanged**: `drain_events` still runs per push, so time-to-first-token is unaffected; the only added state is a ≤3-byte tail buffer.
- **Downstream correctness is automatic**: once `SseParser` yields complete chunks, `complete_stream_accounted` persists an intact `StreamOutcome` (`accounting.rs:56-59`) and replays it intact (`accounting.rs:558-570`) — no changes needed in `accounting.rs`, `service_impl.rs`, or `routes/ai.rs`.
- **Scope discipline**: retry/backoff parity and `message_delta` usage accounting are separate directions in the same analysis file — do not fold them in.
