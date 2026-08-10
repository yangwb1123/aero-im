# Design — Fix silent UTF-8 data loss in the Anthropic SSE parser

- **Module**: `crates/aero-ai` (leaf; `SseParser` is crate-private and has **no consumers outside `anthropic.rs`** — verified by grep)
- **Requirements**: `docs/requirements/2026-08-06-aero-ai-sse-parser-utf8-loss.req.md` (REQ-1…REQ-3, AT-1…AT-4)
- **Date**: 2026-08-06 · **Rev**: 2 — finalized for implementation (both completed reviews' dispositions + 3 new §6 tests folded in) · **Status**: design (unimplemented) · **Change scope**: one file (`crates/aero-ai/src/anthropic.rs`), no DB migration, no config/env change, no new dependencies (the warn-assertion subscriber in §6 uses only `tracing` + std), no wire-format change

---

## 0. Evidence verification (all cited symbols re-checked against source)

Every claim in the requirements spec was re-verified against the working tree before designing. Result: **all confirmed**. Two path notes (both are pre-crate-move shorthand in the spec, symbols exact): `accounting.rs` lives at `crates/aero-ai/src/service/accounting.rs`, and the SSE route lives at `crates/aero-server/src/routes/ai.rs` (not `server/src/routes/ai.rs`).

| Spec claim | Verified location |
|---|---|
| `SseParser::push_bytes` = `if let Ok(s) = std::str::from_utf8(bytes) { … }`, **no `else`, no log** | ✅ exact — `anthropic.rs:618-621`; `drain_events` 625-633, `next_chunk` 635, `extract_text_delta` 644-666. Struct at 612 is `String`-buffered + `VecDeque<String>` only — no partial-UTF-8 retention mechanism |
| Sole production consumer = `complete_stream`'s unfold | ✅ `complete_stream` at 339, single `.send()` at 367, `resp.bytes_stream()` at 380, `futures::stream::unfold` at 382 feeding `parser.push_bytes(&bytes)`; grep shows **zero other `push_bytes`/`SseParser` references** in `crates/` |
| Chinese-first stream path | ✅ `ANSWER_SYSTEM_PROMPT` in `crates/aero-ai/src/service/tools.rs` — `用中文回答` at line 62; Chinese user prompt at `service_impl.rs:572` (`问题: {q}…请基于上述上下文作答`); `complete_stream_accounted(…, ANSWER_SYSTEM_PROMPT, …, 800)` at 574-587; route call at `crates/aero-server/src/routes/ai.rs:151` |
| `complete_stream_accounted` persists corrupted chunks + verbatim replay | ✅ `service/accounting.rs:544`; `StreamOutcome { chunks: Vec<String>, terminal_error }` at 55-59; `Replayed` branch re-serves saved `chunks` at 558-570; `finalize(…, UsageOutcome { kind: STREAM_OUTCOME, … })` at 606-620 (`STREAM_OUTCOME` const at 31) |
| `sse_parser_buffers_across_chunks` ASCII-only | ✅ `anthropic.rs:961-985` — splits JSON key (`…content_block_delt` / `a",…`) and `"Hi"`; every chunk individually valid UTF-8 |
| Baseline green | ✅ `cargo test -p aero-ai sse_parser` → **5/5 passed** on current tree |

**One addition to the spec's risk list (confirmed by reading the struct)**: `SseParser` **already has a field named `pending: VecDeque<String>`** (extracted text chunks, 615). The spec's recommended implementation suggests naming the tail buffer `pending` — that name is taken. The design below uses `tail: Vec<u8>`; `#[derive(Default)]` (612) still works since `Vec<u8>: Default`.

**Review dispositions (2026-08-06 — both completed reviews folded in, zero blocking findings)**: the two independent reviews (test-adequacy + UTF-8-correctness) re-confirmed every load-bearing claim — the `Utf8Error::error_len()` contract (retaining ≤3 bytes on `None` is always sufficient and bounded), clippy-cleanliness of the double-parse under `unsafe_code = "forbid"`, prepend-before-split SSE-framing safety (`\n\n` is ASCII and can never be trapped in `tail`), `Some(_)` state consistency, and accounting-replay isolation (`Replayed` re-serves saved `chunks` via `stream::iter` — no bytes, no parser, no tail). Their non-blocking deltas are adopted here: (1) **tail-empty zero-copy fast path** in `push_bytes` (§2.3); (2) **`tail.len()` captured before `clear()`** so the `warn!` field is truthful (§2.3); (3) **warn message reworded** — counts the *dropped remainder*, not "invalid bytes" (§2.3); (4) **AT-1 single-event-fixture note** (§6); (5) **three §6 test gaps closed** — 4-byte emoji in the AT-2 sweep fixture + explicit 4-push `F0 9F 98 80` tail-cap test, empty-bytes push test (with/without nonempty tail), explicit `error_len() == Some(_)` corruption test (§6).

---

## 1. Design overview

```
anthropic.rs:382-394  complete_stream unfold (UNTOUCHED)
        │  resp.bytes_stream() → arbitrary byte boundaries (reqwest)
        ▼
SseParser (crate-private, ONLY used here — verified)
  push_bytes(&Bytes)          ← the only function that changes (618-621)
    │  tail empty? → from_utf8(bytes) directly       (zero-copy fast path —
    │  else combined = tail ++ bytes                   today's cost for valid chunks)
    │  match from_utf8(data)                         (prepend order: continuation
    │    Ok(s)            → buffer.push_str(s); drain_events()    bytes belong
    │    Err(e), None     → truncated tail: push valid prefix, retain ≤3 bytes  BEFORE
    │                       in tail; drain_events()   ← THE BUG FIX        new data)
    │    Err(e), Some(_)  → mid-stream corruption: push valid prefix,
    │                       warn! + drop remainder     ← defensive, never silent
    ▼
  drain_events / extract_text_delta / next_chunk (UNTOUCHED — event logic correct,
  covered by existing tests)
    ▼
complete_stream_accounted (accounting.rs UNTOUCHED) — persisted StreamOutcome
becomes correct automatically once chunks are complete
```

- **Root cause (verified)**: `push_bytes` is all-or-nothing over each network chunk. `中` = `E4 B8 AD` split as `…E4` | `B8 AD…` yields two individually-invalid chunks → **both dropped whole**, including every complete SSE event inside them. The gap is then persisted into the `STREAM_OUTCOME` reservation (`accounting.rs:606-620`) and replayed verbatim on retry (`558-570`).
- **Fix core**: `Utf8Error::error_len()` — `None` ⇔ truncated sequence at the end of input (the network-boundary case) → retain the tail (≤3 bytes — max incomplete UTF-8 sequence) for the next chunk; `Some(_)` ⇔ genuine mid-stream corruption → `tracing::warn!`, never silent. No `from_utf8_lossy` (U+FFFD substitution is still data corruption and would corrupt the JSON `text` payload mid-string).
- **Streaming semantics unchanged**: `drain_events()` still runs after every push that appends bytes; time-to-first-token unchanged. Per-push cost for valid chunks with an empty tail is a zero-copy `from_utf8` borrow — identical to today; the `combined` allocation now happens only on the rare slow path (a tail exists, i.e. the previous chunk ended mid-character).

---

## 2. API changes (concrete)

### 2.1 Public API: **none**

`SseParser` is a private struct (`struct SseParser`, no `pub`); `push_bytes(&Bytes)` / `next_chunk()` signatures are unchanged (REQ-1 §6: "No wire-format or `SseParser` API changes"). `complete_stream`'s `impl Stream<Item = Result<String>>` item type and the SSE route wire format are untouched. No new deps — `bytes` + `std` suffice, and `tracing` is already a crate dependency (used at `anthropic.rs:234,243` and `worker/mod.rs`), so `tracing::warn!` needs no import changes. The one private helper added (`handle_invalid`) is crate-private like the struct — no public surface change.

### 2.2 `SseParser` struct (`anthropic.rs:612-616`) — one additive field

```rust
#[derive(Default)]
struct SseParser {
    buffer: String,
    pending: VecDeque<String>,
    /// Bytes of an incomplete multi-byte UTF-8 sequence split across chunks
    /// (1-3 bytes — the max UTF-8 continuation tail). Drained (prepended)
    /// into the next pushed chunk; never exceeds 3 bytes, so memory is
    /// bounded regardless of chunk sizes.
    tail: Vec<u8>,
}
```

Name is `tail`, **not** `pending` — the spec's suggested name collides with the existing `pending: VecDeque<String>` (see §0 addition). `#[derive(Default)]` keeps working.

### 2.3 `push_bytes` (`anthropic.rs:618-621`) — replacement body + private `handle_invalid` helper

```rust
fn push_bytes(&mut self, bytes: &Bytes) {
    let tail_len = self.tail.len();
    if tail_len == 0 {
        // Fast path: no retained tail — parse the chunk directly (zero-copy
        // borrow, byte-identical to today for valid chunks). The Err arm is
        // the same state machine as the slow path; only the data source differs.
        match std::str::from_utf8(bytes) {
            Ok(s) => {
                self.buffer.push_str(s);
                self.drain_events();
            }
            Err(e) => self.handle_invalid(e, bytes, 0),
        }
        return;
    }

    // Slow path: prepend the retained ≤3-byte tail — continuation bytes belong
    // BEFORE the new chunk's bytes (a 3-byte char may span up to 3 chunks, a
    // 4-byte char up to 4).
    let mut combined = Vec::with_capacity(tail_len + bytes.len());
    combined.extend_from_slice(&self.tail);
    combined.extend_from_slice(bytes);
    self.tail.clear();

    match std::str::from_utf8(&combined) {
        Ok(s) => {
            self.buffer.push_str(s);
            self.drain_events();
        }
        Err(e) => self.handle_invalid(e, &combined, tail_len),
    }
}

/// Shared `Err` handling: retain a truncated tail (`error_len() == None`) or
/// warn + drop on genuine corruption (`Some(_)`). Never silent.
fn handle_invalid(&mut self, e: std::str::Utf8Error, data: &[u8], tail_len: usize) {
    let valid = &data[..e.valid_up_to()];
    if !valid.is_empty() {
        // Guaranteed valid: valid_up_to() is the first invalid index.
        self.buffer.push_str(
            std::str::from_utf8(valid)
                .expect("utf8 prefix before first invalid index is valid"),
        );
        self.drain_events();
    }
    if e.error_len().is_none() {
        // Truncated multi-byte sequence at the tail — network-boundary
        // artifact. Retain it (≤3 bytes) for the next chunk; nothing is dropped.
        self.tail.extend_from_slice(&data[e.valid_up_to()..]);
    } else {
        // Genuine mid-stream corruption (Anthropic wire is JSON-escaped valid
        // UTF-8, so this is defensive only). Never silent. `tail_len` is the
        // pre-clear retained-tail length — captured before `clear()` so the
        // log is truthful (review disposition).
        let dropped = data.len() - e.valid_up_to();
        tracing::warn!(
            bytes = data.len(),
            tail = tail_len,
            valid = e.valid_up_to(),
            "anthropic sse: dropped {dropped} bytes after invalid UTF-8 mid-stream",
        );
    }
}
```

Notes:

- **Zero-copy fast path (adopted reviewer delta)**: when `tail` is empty — the overwhelmingly common case — the chunk is parsed straight from the borrowed `Bytes`, identical to today's cost for valid chunks. The `combined` allocation happens only on the slow path (the previous chunk ended mid-character).
- **Prepend order is load-bearing**: a 3-byte char can span up to 3 chunks (`E4` | `B8` | `AD…`) and a 4-byte char up to 4 (`F0` | `9F` | `98` | `80…`); the retained tail must precede the new chunk's bytes, and a single ≤3-byte tail is sufficient because `error_len() == None` guarantees exactly one truncated sequence at the very end of the input (a complete invalid sequence would report `Some`).
- The `valid` prefix is guaranteed valid UTF-8 by the `Utf8Error` contract (`valid_up_to()` is the first invalid index), so `from_utf8(valid).expect(...)` cannot fail. `unsafe` is **forbidden**: `Cargo.toml:43` sets workspace `unsafe_code = "forbid"` (AGENTS.md §4.2) — `from_utf8_unchecked` is not an option; the double-parse above is required and clippy-clean (both reviews verified: `expect_used` is restriction-group, not enabled by the workspace's `all`+`pedantic` lints).
- `drain_events()` runs on every push that appends bytes (Ok, or Err-with-nonempty-prefix) — same call-site behavior as today for valid chunks. If the prefix is empty (chunk fully invalid), no append → no drain, matching today's silent-skip but now with a warn.
- **Warn wording (adopted reviewer delta)**: the message counts the *dropped remainder* ("dropped N bytes after invalid UTF-8"), not "N invalid bytes" — the counted bytes may include potentially-salvageable trailing bytes after the first invalid sequence; dropping the whole remainder is deliberate.
- **Edge — empty `bytes`**: fast path → `from_utf8(&[])` is `Ok("")`, push nothing, `drain_events()` no-op — identical to today. Slow path with empty `bytes` → `combined == tail` alone → still `Err(None)` → tail re-retained **unchanged** (no growth, no loop). Both covered by the empty-push test (§6).

---

## 3. Compatibility constraints

1. **No behavior change for ASCII/valid chunks**: the `Ok` path is byte-identical to today; `sse_parser_buffers_across_chunks` and `sse_parser_queues_multiple_chunks` must pass unmodified (they will — every existing fixture chunk is valid UTF-8).
2. **Event-splitting logic untouched**: `drain_events` (625), `extract_text_delta` (644), `next_chunk` (635) are byte-identical. A `\n\n` separator split across chunks is ASCII, never retained in `tail`, and handled by the existing buffer logic unchanged.
3. **Downstream automatically correct**: `complete_stream_accounted` (`accounting.rs:544`), `StreamOutcome` persistence (606-620), and `Replayed` verbatim replay (558-570) need zero changes — they persist exactly what the parser yields. Once the parser yields complete chunks, the durable replay payload is intact.
4. **Streaming/latency**: per-push `drain_events()` unchanged → time-to-first-token unchanged. Added state is a `Vec<u8>` capped at 3 bytes (max incomplete UTF-8 tail); the `combined` allocation happens **only on the slow path** (a tail exists — i.e. the previous chunk ended mid-character); valid chunks with an empty tail take the zero-copy fast path.
5. **Scope isolation**: retry/backoff parity and `message_delta` usage parsing are separate directions (same analysis file) — do not touch `anthropic.rs`'s retry loop (211-247), `complete_stream`'s single-`send` structure (339-394), or `extract_text_delta`'s event filtering (644-666).
6. **Crate isolation**: `SseParser` is crate-private with no consumers outside `anthropic.rs` (grep-verified), so the change cannot leak into `aero-server` or other crates. `cargo check --workspace` suffices for compile impact.

---

## 4. Failure modes & mitigations

| Failure mode | Detection | Mitigation |
|---|---|---|
| **Truncated tail at chunk boundary** (`error_len() == None`) — the bug being fixed | Parser state: `tail` non-empty | Retained, prepended to next chunk, re-parsed. Never dropped, never replaced. Bounded ≤3 bytes |
| **Mid-stream corruption** (`error_len() == Some(_)`) — malformed bytes not at a boundary (e.g. proxy mangling, non-UTF-8 garbage) | `tracing::warn!` with byte counts (`bytes`, pre-clear `tail`, `valid`, `dropped`) | Valid prefix preserved; invalid remainder dropped with an audible log — the warn counts the *dropped remainder*, which may include potentially-salvageable trailing bytes (dropping the whole remainder is deliberate). **Never silent** (today it is). This is defensive only — Anthropic's wire contract is JSON-escaped valid UTF-8, and `serde_json::from_str` in `extract_text_delta` would reject a corrupted `data:` line anyway (`.ok()?` → event skipped) |
| **4-chunk split of a 4-byte char** (e.g. emoji `F0 9F 98 80`) | `error_len() == None` three times in a row | Same path: tail grows 1→2→3 bytes across pushes, resolved when the 4th byte arrives. Tail cap = 3 bytes = max incomplete sequence length, so memory is bounded regardless of chunk sizes |
| **Empty `bytes` push** | Parser state: nothing appended; push returns promptly | Fast path: `from_utf8(&[])` is `Ok("")` → no-op, no drain (identical to today). Slow path with nonempty tail: `combined == tail` alone → still `Err(None)` → tail re-retained **unchanged** (no growth, no loop). Covered by the empty-push test (§6) |
| **Corrupted `data:` JSON line** (parser-level) | `extract_text_delta` returns `None` via `.ok()?` | Existing behavior, unchanged — not in scope (usage parsing is a separate direction) |
| **Parser yields nothing at stream end** (regression) | AT-1/AT-2 assert on full concatenation | Property test over every split offset catches any dropped-byte regression |
| **`unsafe` temptation in prefix push** | `cargo clippy --workspace --all-targets` (workspace `unsafe_code = "forbid"`, `Cargo.toml:43`) | Design already uses `from_utf8(valid).expect(...)` — double-parse of a guaranteed-valid prefix; see §2.3 note |
| **Infinite loop / latency regression** | `drain_events` is a `while` on `buffer.find("\n\n")`, terminates because each iteration drains | No new loops added; `drain_events` untouched |

---

## 5. Migration steps (implementation order)

No DB migration, no config, no env. Pure code change in one file; order is REQ-3's red-first discipline:

1. **Red — land tests first** (`anthropic.rs` `#[cfg(test)] mod tests`, §6 AT-1…AT-3): add the CJK split tests, the sweep property test, the 4-push emoji test and the corruption test against the **current** parser. Run `cargo test -p aero-ai sse_parser` — the new tests fail (both boundary chunks are dropped, so `next_chunk()` yields `None`/missing deltas; the corruption test observes zero warns and an empty `buffer`). **Capture the failing output** (commit message or PR description) as the executable record of the bug. This is the first executable proof (the spec's caveat: frequency was reasoned from code inspection only). Two caveats: (i) tests that assert on the private `tail` field cannot compile until the field exists — the red batch therefore uses only the pre-existing surface (`next_chunk()` behavior, `p.buffer` contents, warn count); the `tail` assertions land with the fix in the same commit (§5.2/§5.4). (ii) the empty-bytes-push test passes on both old and new code — it is a no-op-equivalence guard, not a red marker; the red record comes from the CJK/sweep/4-push/corruption failures.
2. **Green — apply REQ-1** (§2.3): add `tail: Vec<u8>` field + replace `push_bytes` body (+ private `handle_invalid` helper), and land the post-fix-only `tail` assertions (cap ≤3, empty invariants) in the same change — they reference the new field and cannot compile before it. Run `cargo test -p aero-ai sse_parser` — the red-batch tests now pass **without modification**; the 5 existing tests still pass.
3. **Full crate + workspace gates** (AGENTS.md §4.3): `cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets` (no new warnings — the double-parse `.expect()` is restriction-group, not enabled by the workspace's `all`+`pedantic` lints; verified by both reviews) · `scripts/truth-check.sh` / `file-size-check.sh` / `web-check.sh` (web untouched, still run per commit gate). **File-size reality** (verified from `scripts/file-size-check.sh`: WARN > 800, HARD > 1200, exit code counts HARD violations only): `anthropic.rs` is already **985 lines — in the WARN zone today**. The ~65–115 lines of new tests land it at **~1050–1100**: the gate **passes** (exit 0) but prints a ⚠️ WARN banner, and the margin to the 1200-line HARD limit is thin. Do not expand scope in this file; splitting the SSE client out is a separate refactor, out of scope here.
4. **Commit**: single commit containing tests + fix + recorded red output; do not fold in retry-parity or `message_delta` directions.

---

## 6. Testable acceptance mapping

All tests live in `anthropic.rs`'s existing `#[cfg(test)] mod tests` (961+ — new tests append to it), using `use super::*;` and `bytes::Bytes` (already imported at line 12). Fixtures are real Anthropic SSE frames carrying CJK (`中` = `E4 B8 AD`, `文` = `E6 96 87`) and a 4-byte emoji (`😀` = `F0 9F 98 80`). Tests that read the private `tail` field are **post-fix-only** (they cannot compile before the field exists) and land in the same commit as the fix (§5.2); red-first (§5.1) runs the subset that compiles against the current struct.

| Acceptance criterion | Test implementation | Pass condition |
|---|---|---|
| **AT-1** — CJK split across 2 chunks (direction's exact scenario) | `sse_parser_cjk_split_two_chunks`. ⚠️ **Single-event fixture** (review note): the cut must fall inside the *first* event — chunk 1 must contain no complete event, or the "no complete event yet" assertion breaks (the pitfall hit in review scratch runs with a 2-event fixture). Chunk 1 ends with `…"text":"` + `E4` (lead byte of `中`); chunk 2 starts `B8 AD…` + rest of event + `\n\n`; also the mirror split (`E6` of `文`). After push 1, assert `next_chunk() == None` (mirroring the existing ASCII test's `"event not yet complete"` at 961-985) | `next_chunk()` yields the delta with the full char; concatenated output == expected full text; zero dropped characters |
| **AT-1b** — CJK split across 3 chunks | `sse_parser_cjk_split_three_chunks`: `E4` \| `B8` \| `AD…` across three pushes | Same assertion (exercises `tail` surviving two consecutive `None`-errors) |
| **AT-2** — multi-event + CJK at event boundaries | `sse_parser_property_every_split_offset`: two consecutive `content_block_delta` events, one `\n\n` separator split inside a chunk, one CJK char straddling another boundary; and the **property test**: for a fixture with ≥2 events, ≥2 CJK chars **and ≥1 4-byte emoji (`😀`)** — the emoji makes the sweep cover 2-chunk cuts of 4-byte chars for free — enumerate **every** split point `s ∈ 1..len-1`, feed `bytes[..s]` then `bytes[s..]`, fresh parser per split | (a) concatenation of all yielded chunks == expected full text; (b) yielded-chunk count == number of `text_delta` events (every event boundary detected); (c) every yielded chunk is valid UTF-8 — (a)+(b) carry the red signal; (c) is by-construction weak (chunks are `String`) and must not be relied on as the failure detector. Post-fix-only extra: assert `p.tail.is_empty()` after each pair of pushes. Optional cheap extension (unchanged): 3-chunk variant for every pair of cuts `s1 < s2` with `s2 - s1 <= 2` |
| **AT-2** — 4-byte tail cap proof | `sse_parser_emoji_split_four_pushes`: explicit `F0` \| `9F` \| `98` \| `80…` one byte per push; after each of the first three pushes assert `p.tail.len()` == 1, 2, 3 respectively (and ≤ 3 always — the bounded-memory invariant); the 4th push completes the char | Full emoji present in yielded text; `p.tail` empty after completion; `p.tail.len() <= 3` at every step (post-fix-only assertion — lands with the fix, §5.2) |
| **AT-2** — empty-bytes push | `sse_parser_empty_bytes_push`: (a) `push_bytes(&Bytes::new())` on a fresh parser → no-op: nothing appended, `next_chunk()` still `None`, push returns promptly (no loop); (b) after a push leaving a nonempty tail (e.g. `…E4`), push `Bytes::new()` → tail **unchanged** (no growth, no loop), and the subsequent `B8 AD…` push still completes the char | Behavior identical to a parser that never saw the empty push (assert on yielded text + `p.tail` contents/length; tail assertions post-fix-only). Guards both §2.3 empty-`bytes` edges — the fast-path `Ok("")` no-op and the slow-path tail-re-retain |
| **AT-2** — mid-stream corruption (`Some(_)`) | `sse_parser_corrupt_mid_stream_warns_and_retains_prefix`: push a chunk with `0xFF` mid-chunk after a valid prefix (e.g. `…"text":"A\xFFB"…`); second case: retained tail then non-continuation byte (`E4` then `'A'` → `Some(1)` at index 0 — verified in review scratch runs). Assert the warn with a minimal `tracing::Subscriber` in the test module (count `Event`s at `WARN` via `tracing::subscriber::with_default`; delegate to `NoSubscriber`; ~15 lines, no new deps) | (a) warn fired exactly once per corrupt push; (b) valid prefix retained in `p.buffer` (red-eligible — fails on current code, which drops the whole chunk); (c) `p.tail` empty (post-fix-only); (d) a subsequent good chunk parses normally — state consistent after corruption |
| **AT-3** — regression marker | §6 tests landed **before** REQ-1 (step §5.1) | Red run against current parser recorded (failing output captured); same tests pass post-fix without modification; tests remain permanent coverage. Note: the empty-push test passes pre-fix too (no-op equivalence) — expected; the red record comes from the CJK/sweep/4-push/corruption failures |
| **AT-4** — gates stay clean | Run gates (§5.3) | `cargo test -p aero-ai` green (5 existing + new); `cargo clippy --workspace --all-targets` no new warnings; `cargo test --workspace --lib` green |

**Mapping verification**: every row maps to exactly one of AT-1…AT-4 — the first row = AT-1; the second = AT-1b (AT-1's 3-chunk variant); the four AT-2 rows (sweep with emoji fixture, 4-byte cap proof, empty-bytes push, `Some(_)` corruption) are AT-2 sub-cases — AT-2 is the behavioral edge criterion, expanded per review to close the three previously untested gaps; the last two rows = AT-3 and AT-4. No orphan tests, no invented AT numbers.

Baseline for comparison: `cargo test -p aero-ai sse_parser` = 5/5 green on the current tree (verified in §0).

---

## 7. Out of scope (unchanged from requirements §3)

1. **No accounting changes** — `accounting.rs` untouched; persisted payload becomes correct automatically.
2. **No retry/backoff changes** — `complete_stream` keeps its single-`send` structure (retry parity is a separate direction, `docs/design/2026-08-06-aero-ai-retry-parity-voyage-whisper.design.md`).
3. **No `message_delta` usage parsing** — `extract_text_delta` keeps dropping non-`text_delta` events.
4. **No lossy replacement** — `from_utf8_lossy` explicitly out.
5. **No new dependencies.**
6. **No wire-format or `SseParser` API changes** — only internal buffering + `push_bytes` body.
