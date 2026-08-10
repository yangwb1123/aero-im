# Requirements Spec — aero-live-srt: transactional session-end + per-session L1 summary seam at the SRT teardown path

- **Module**: `crates/aero-live-srt` (+ one storage slice in `crates/aero-storage`, + one migration)
- **Direction**: "Transactional session-end + per-session L1 summary seam at the SRT teardown path (the only B5-1 landing point in this crate)"
- **Source analysis**: `docs/auto/analyses/crates-aero-live-srt-e06b4c8e.json` (direction #1; value 8 / risk-reduction 7 / effort 5 / confidence 8)
- **Campaign**: `aero-im-b5-outbox-relay` (`docs/campaigns/campaign-aero-im-b5.yaml`); in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md` (B5-1); gate anchor `docs/campaigns/implementation-gate.md` (G6 row: "37/37、T-11、moderation 优先级"; row 1: outbox + in-tx, "‡ 类走 L1")
- **Status**: Requirements (verified evidence below)
- **Verification date**: 2026-08-07 (line numbers are as-of-verification anchors; drift is possible — the **file/symbol** is the stable grep anchor per AGENTS.md §0)

## 1. Evidence verification (every cited symbol checked against the repo)

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-live-srt/src/lib.rs:652-666` (`finalize_session`: warn-swallowed session.finish + mark_ended) | ✅ **Verified** (fn at 651-666). Sole teardown path for an established peer: `metrics::SessionCounter::removed()` → `session.finish().await` — `Err` ⇒ `warn!(error = %e, "SRT: error finalizing HLS on disconnect")` → `repo.mark_ended(id).await` — `Err` ⇒ `warn!(..., "SRT: failed to mark stream ended")`. Both best-effort, no transaction, **no outbox row, no summary capture, no returned identity**. Reached from `run_listener` error arm (lib.rs:341) and the shutdown drain loop (~415). |
| E2 | `lib.rs:1058-1069` (`SrtSession::finish` → `LiveResult<()>`, no summary) | ✅ **Verified** (exact lines). `pub async fn finish(&mut self) -> LiveResult<()>`: trailing `flush_segment` if `segmenter.has_segment_data()`, then `self.hls.finish()` mapped to `LiveError::Internal`. Returns unit — no segment_count/bytes/duration. `HlsWriter::finish` (crates/aero-live-hls/src/lib.rs:167) is idempotent (`finalized` flag → second call `Ok(())`), which is what makes "repeated finish() identical" achievable. |
| E3 | `lib.rs:1042-1052` (`flush_segment` → `hls.push_segment` per ~2s segment, zero counters) | ✅ **Verified** (fn at 1044-1053). `hls.push_segment(bytes.into(), SEGMENT_DURATION_SECS_F32)`; `HlsWriter::push_segment` (aero-live-hls/src/lib.rs:126) returns `HlsResult<PathBuf>`; **no counter/accumulator touches this path**. `SEGMENT_DURATION_SECS: u32 = 2` at lib.rs:136 (cited 134 — off by 2) and `SEGMENT_DURATION_SECS_F32: f32 = 2.0` at :140; a new segment flushes every ~2s of live video. |
| E4 | `lib.rs:1085-1124` (`resolve_stream` → `repo.mark_live`) | ✅ **Verified** (exact lines). `repo.mark_live(stream.id, &hls_url)` → `MarkLiveOutcome::{Started(_), AlreadyLive, NotFound}`; `AlreadyLive` → `LiveError::Protocol`; HLS-init failure rolls back via best-effort `repo.mark_ended` (also warn-swallowed, :1117). |
| E5 | `crates/aero-storage/src/stream.rs:179-260` (`mark_live` in-tx outbox CTE — the pattern B5-1 generalizes) | ✅ **Verified** (fn at 179; CTE `WITH transitioned AS (` at 228; `INSERT INTO stream_go_live_outbox` at 238; `RETURNING id, event_id` at 242). One tx: unlock-order resolve → `FOR UPDATE` → single data-modifying statement = `UPDATE streams SET status='live' ... RETURNING id, room_id, owner_id, title` + `INSERT INTO stream_go_live_outbox (id, event_id, stream_id, room_id, owner_id, title, subject, traceparent) ... RETURNING id, event_id` → commit → `MarkLiveOutcome::Started(GoLiveTransition { outbox_id, event_id })`. **This is the exact CTE shape the session-end path must mirror (acceptance b).** |
| E6 | `crates/aero-storage/src/stream_go_live_outbox.rs:40-53` (`GoLiveTransition`/`event_id`) | ✅ **Verified** (symbol drift: `GoLiveTransition { outbox_id: Uuid, event_id: Uuid }` at **26-28**; `MarkLiveOutcome` at **33-40**; cited 40-53 is the `StreamGoLiveOutboxRow` head whose first fields are `id`/`event_id`/`stream_id`). Full retryable machinery verified: `claim_due` (SKIP LOCKED, claim-token generation fence), `claim_by_id`, `assign_seq_if_absent`, `mark_failed` (bounded exponential backoff cap 300s), `mark_completed` (requires both nats + webhooks stages) — the clone source for the session-end repo. |
| E7 | `crates/aero-storage/src/stream.rs:264` (`mark_ended` — the half-transactional gap) | ✅ **Verified** (264-271). Plain single `UPDATE streams SET status='ended', ended_at=NOW() WHERE id=$1` on the pool — no tx, no outbox row, no event identity. **The asymmetry vs E5 is the core gap.** `streams` DDL (migrations/0002_p2_collab_ai.sql:90-106): `status CHECK ('idle','live','ended')`, `protocol CHECK ('rtmp','whip','srt')`, columns id/owner_id/room_id/title/stream_key/status/hls_path/protocol/started_at/ended_at/created_at — **no duration/bytes summary columns exist** (a summary needs its own outbox row or migration). |
| E8 | `crates/aero-live-srt/src/tests.rs:920` (DATABASE_URL-gated test — T-11/37-tests harness precedent) | ✅ **Verified** (test at 908, `connect_lazy("postgres://u:p@localhost/aero")` at 921). `idle_ingest_listener_stops_promptly_when_cancelled` builds a lazy pool, binds the listener, cancels — never queries, so it is hermetic in practice. **tests.rs contains exactly 37 test fns** (`rg '^\s*#\[(tokio::)?test'` = 37) — the crate's hermetic suite backing the "37/37" count; isolation_tests.rs and rotation_tests.rs add 5 each. |
| E9 | (supplementary) `crates/aero-live-srt/src/isolation_tests.rs` `CountingBackend` — the hermetic teardown-error harness | ✅ **Verified**. `finalize_broken_writer` at ~228 / 273-284: writer root deleted ⇒ `File::create` fails deterministically ⇒ `finish()` errors ⇒ swallowed "mirroring production `finalize_session`'s warn-swallow"; :1013 cancel-drain test ("Cancel → graceful drain (finalize_session: finish + mark_ended)"). This is the exact injection seam acceptance (b) needs on the hermetic side. |
| E10 | (supplementary) `MpegTsSegmenter` / `HlsWriter` — what summary data is derivable today | ✅ **Verified**. Segmenter (segmenter.rs:85-205) tracks only `packets_seen: u64` (whole 188-byte packets); `take_segment() -> Vec<u8>` (byte sum computable at flush), `has_segment_data()`; **no segment count, no duration accumulator**. `HlsWriter` tracks `segment_index: u64` + per-entry `duration_secs: f32` but exposes no sum. Per-segment duration fed to HLS is the constant 2.0 (E3), so `duration_secs = segment_count × SEGMENT_DURATION_SECS_F32` is the deterministic derivation. |
| E11 | (supplementary) B5-1 contract state: class list `message/room/admin`, L1 aggregation, migration 0239 | ✅ **Verified (as claimed)**. `ls migrations/` ends at 0238 — **`0239_audit_governance_outbox.sql` does not exist in-repo**. `docs/proposals/audit-contract-batch-aero-im.md:8` (B5-1: `class` message/room/admin, status 0/1/2/3, priority, delivery_mode); `docs/requirements/2026-08-06-aero-ai-b5-1-audit-governance-outbox.req.md` E11: "**no L1 aggregation exists in-repo**" (`NotifyBatch` is fan-out, not aggregation); A4: L1 window mechanics are [PROPOSED]. ⇒ **stream.\* class admission into the governance outbox is [PROPOSED]** — this direction builds the producer-side seam (row shape + stable identity), not the governance enqueue. |
| E12 | (supplementary) `crates/aero-live-srt/Cargo.toml` — dependency direction | ✅ **Verified**. Deps: `aero-common`, `aero-live-core`, `aero-storage`, `aero-live-hls` (+ tokio/bytes/tracing/thiserror/anyhow/serde/time/ulid + RustCrypto). `aero-storage` is **already** a dependency (`StreamRepo` is passed into `run_until_cancelled`, lib.rs:243-247), so a `mark_ended_in_tx` addition creates no new crate edge; no audit/relay crates anywhere. Hot path `feed_packet` (lib.rs:952-1020) + `pump` must stay storage-free (single-UDP-loop invariant, direction #2 of the same analysis). |

## 2. Verified current state (the pipeline this direction modifies)

```
publisher datagram
  └─ run_listener (lib.rs:345) — one UDP socket, peers: HashMap<SocketAddr, PeerState>
       └─ SrtSession::feed_packet (lib.rs:952) — decrypt → reorder → segmenter
            └─ feed_ts_bytes (lib.rs:1021) → SegmentEvent::CutBeforeKeyframe
                 └─ flush_segment (lib.rs:1044) → HlsWriter::push_segment(bytes, 2.0)   ← ~2s cadence, ZERO counters
disconnect / cancel / peer error
  └─ finalize_session (lib.rs:651-666) — the SOLE teardown path
       ├─ metrics::SessionCounter::removed()
       ├─ session.finish()  (lib.rs:1058) → LiveResult<()>  ← trailing flush + ENDLIST, no summary
       │     Err → warn! (swallowed)
       └─ repo.mark_ended(id) (stream.rs:264) → plain UPDATE, no tx, no outbox row, no event_id
             Err → warn! (swallowed)
```

Session START is fully transactional: `resolve_stream` (lib.rs:1085) → `mark_live` (stream.rs:179-260) commits `status='live'` + an immutable `stream_go_live_outbox` row in **one CTE statement**, returning stable `event_id`/`outbox_id` (`GoLiveTransition`, stream_go_live_outbox.rs:26-28), relayed by the durable claim/lease pump (E6). Session END is **half-transactional**: `finalize_session` is best-effort, warn-swallowed, and produces **no outbox row and no summary** — a stream.end audit event cannot exist in-tx today, and a governance admission of stream lifecycle would silently lose the session on HLS-finish or `mark_ended` failure.

Everything the summary needs is already observable at flush time: `bytes.len()` (E3/E10), constant `duration_secs = 2.0` per segment (E3), and a per-session increment counter — none of it is accumulated today. The `MpegTsSegmenter`/`HlsWriter` are pure in-memory/disk; the single UDP loop multiplexing all peers (E12) means the accumulation must stay in-memory in `SrtSession` and never await storage on the packet path.

## 3. Scope

**In scope (this crate + one storage slice + one migration):**
- Per-session L1 summary accumulation inside `SrtSession` (segment_count / bytes_total / duration_secs), advanced only on successful HLS segment pushes.
- Terminal `SrtSession::finish()` returning the summary, exactly-once on repeat calls.
- Same-transaction session-end outbox: `StreamRepo::mark_ended_in_tx` mirroring the `mark_live` CTE (stream.rs:228-242) — status flip + retryable `stream_session_end_outbox` row with stable `event_id`/`outbox_id`, claim/lease/backoff machinery cloned from `StreamGoLiveOutboxRepo` (E6).
- `finalize_session` rewiring: HLS-finish error still parks the retryable session-end row (partial summary), so no audit identity is lost on teardown.
- One new migration (`NNNN_stream_session_end_outbox.sql`) mirroring `migrations/0179_stream_go_live_outbox.sql` (immutable snapshot, no FK, due index, retryable claim state).

**Out of scope (explicit):**
- The governance outbox itself: `0239_audit_governance_outbox.sql` does not exist in-repo; class list `message/room/admin` and stream.\* admission are [PROPOSED] (E11). This direction only guarantees the producer-side row shape + stable identity so that admission has something durable to classify.
- L1 aggregation **window** bookkeeping ([PROPOSED], B5-1 req A4/E11): the per-session summary **is** the atomic aggregate unit (one row per session); merging windows are out of scope.
- The B5-3 moderation-priority drill (server-side; acceptance (c) is a recorded **precondition**, not a deliverable here).
- The WHIP and RTMP teardown paths (different crates — `aero-live-whip`, `aero-live-rtmp`); each needs the same seam later, but this direction lands only the SRT path ("the only B5-1 landing point in this crate").
- Any audit/relay I/O on the per-datagram path (`feed_packet`/`pump` stay storage-free — E12).

## 4. Requirements

### R1 — Per-session L1 summary accumulation (in-memory, hot-path-free)
`SrtSession` accumulates `segment_count: u64`, `bytes_total: u64`, `duration_secs: f32` (or an integer millis equivalent) in `flush_segment` (lib.rs:1044): after `hls.push_segment` returns `Ok`, add `1`, `bytes.len()`, and `SEGMENT_DURATION_SECS_F32` (the exact `duration_secs` value passed to `push_segment`). Counters advance **only on successful pushes** — a failed push contributes nothing, so a partial summary is always a truthful "segments durably written" count. No storage, no locks, no I/O on the packet path (E12); the counters are plain fields on `SrtSession`.

### R2 — Terminal `finish()` with exactly-once summary
`SrtSession::finish` (lib.rs:1058) returns the session summary instead of `()`: `pub async fn finish(&mut self) -> LiveResult<SessionSummary>`. Semantics:
- First call: trailing `flush_segment` (if `has_segment_data`) + `hls.finish()` (idempotent, E2), then the summary is **snapshotted once** and returned.
- Repeated calls: return the identical snapshot (segmenter is drained, HLS finalized — deterministic, no double-count).
- Failure during the trailing flush: the error propagates but the summary returned/observable reflects only successfully flushed segments (R1), and the partially-flushed trailing segment is never counted.
- Zero-segment session (disconnect before first keyframe): `Ok` with a zero summary — not an error.
This is the "terminal semantics mirroring status 0/1/2/3 DDL" (B5-1 normative terminal states): once terminal, the value is fixed and repeatable.

### R3 — Same-transaction session-end outbox (`mark_ended_in_tx`, mirrors stream.rs:238 CTE)
New `StreamRepo::mark_ended_in_tx(id, summary) -> Result<SessionEndOutcome, sqlx::Error>` in `crates/aero-storage/src/stream.rs`, cloned from `mark_live` (E5): one transaction, one data-modifying statement —

```sql
WITH transitioned AS (
      UPDATE streams
         SET status = 'ended', ended_at = now()
       WHERE id = $1 AND status = 'live'
   RETURNING id, room_id, owner_id, title, hls_path
  )
INSERT INTO stream_session_end_outbox
      (id, event_id, stream_id, room_id, owner_id, title, subject, traceparent,
       segment_count, bytes_total, duration_secs)
SELECT $2, $3, id, room_id, owner_id, title, $4, $5, $6, $7, $8
  FROM transitioned
RETURNING id, event_id
```

- Either both commit or neither — the audit row can never be lost between the status flip and the outbox write.
- Returns a `SessionEndTransition { outbox_id, event_id }` (sibling of `GoLiveTransition`, E6) or a no-op outcome when the row is not `live` (idempotent replay: second call after `ended` commits nothing — matches `MarkLiveOutcome::AlreadyLive` precedent).
- New table `stream_session_end_outbox` (migration `NNNN_stream_session_end_outbox.sql`, next ordinal per `ls migrations/*.sql`) mirroring `0179` DDL: immutable snapshot **with no FK** (deleting a stream must not erase committed integration work), due index `(available_at, created_at, id) WHERE completed_at IS NULL`, claim-state CHECK, `segment_count/bytes_total/duration_secs` summary columns.
- New sibling repo `crates/aero-storage/src/stream_session_end_outbox.rs` (clone of `stream_go_live_outbox.rs`, E6) with `claim_due` (SKIP LOCKED + claim-token fence), `claim_by_id`, `mark_failed` (bounded backoff cap 300s), `mark_completed` — the retryable parking machinery. Register in `lib.rs` (no root re-export collision: no `generate_token`-style helpers here).

### R4 — `finalize_session` error containment (no audit identity lost on teardown)
`finalize_session` (lib.rs:651-666) becomes:
1. `SessionCounter::removed()` (unchanged).
2. `session.finish()` — on `Err` (e.g. HLS root deleted, the `finalize_broken_writer` shape, E9): **still** call `mark_ended_in_tx` with the partial summary from R1/R2; warn but do not return early.
3. `mark_ended_in_tx(id, summary)` — on `Err`: warn (the row remains parked; the claim pump retries). The warn-swallow containment shape is preserved (isolation_tests mirror, E9) — what changes is that the *durability backstop* is the retryable outbox row, not the hope that the plain UPDATE succeeded.

The call site keeps its current shape (`backend.finalize` abstraction, lib.rs:341 + drain loop) so the hermetic `CountingBackend` harness continues to drive the exact listener path without a DB.

### R5 — Summary is the L1 aggregate unit with stable event identity
One `stream_session_end_outbox` row per session, aggregated by construction: the ~2s per-segment flushes (E3) never touch storage; only the terminal row carries `event_id`/`outbox_id` + `stream_id`/`room_id`/`owner_id`/`title`/`subject` snapshot + `segment_count`/`bytes_total`/`duration_secs`. This is the row shape the [PROPOSED] governance-outbox stream.\* admission (E11) classifies: default ingest priority lane, never merged into another session's row, no per-segment rows ever. It is also the shape the server-side B5-3 drill (acceptance (c)) needs to be able to backfill 500 of.

## 5. Acceptance checks (preserved from the direction, made testable)

### A1 — Hermetic: deterministic exactly-once summary
Feed N segments (drive `SrtSession` directly with a tempdir `HlsWriter`, existing pattern: tests.rs:946 `session_with_max_bandwidth_seeds_the_pacer`) then `finish()` → `SessionSummary { segment_count == N, bytes_total == Σ segment byte lengths, duration_secs == N × SEGMENT_DURATION_SECS_F32 }`. Repeated `finish()` returns **identical values** (exactly-once terminal semantics); a second call must not flush or count anything. Edge cases pinned: N=0 (zero summary, `Ok`), and finish-after-failed-trailing-flush (partial summary = successfully pushed segments only).

### A2 — DB-gated: induced HLS-finish error still parks a retryable session-end row
`#[ignore = "requires live Postgres"]` test (precedent: tests.rs:921 `connect_lazy`; storage db_tests pattern): resolve a stream live (via `mark_live`), then make `session.finish()` fail deterministically (delete the HLS dir before finish, mirroring `isolation_tests.rs` `finalize_broken_writer`, E9), run the production `finalize_session` path → assert:
- `streams.status == 'ended'` **and** exactly one `stream_session_end_outbox` row exists for that stream with `completed_at IS NULL`,
- the row is claimable through `claim_due`/`claim_by_id` (retryable parking, mirroring the stream.rs:238 CTE single-statement atomicity),
- the row carries the partial summary (R1) + non-null `event_id`/`outbox_id`,
- no audit event is lost on teardown (row present despite the HLS-finish error).

### A3 — Moderation-priority drill precondition (PROPOSED, server-side)
Recorded precondition, not implemented here: when the B5-3 drill lands, 500 aggregated `stream.session` rows + 1 `admin.content.flag` row → flag claimed first (`claim_due ORDER BY priority DESC`, no starvation of the admin lane by ingest high-volume events). This crate's obligation is structural: exactly **one** session-end row per session (R5) with a shape classifiable as the ingest default-priority lane. The drill itself is owned by the B5-3 storage slice; this spec pins the row shape + per-session cardinality that make it possible.

### A4 — 37/37 hermetic harness unaffected
All existing hermetic no-DB tests stay green: the 37 test fns in `crates/aero-live-srt/src/tests.rs` (E8) plus isolation_tests.rs and rotation_tests.rs (5 each) — `cargo test --workspace --lib` green. No new DB/relay dependency enters the crate's hot path: `feed_packet`/`pump` contain no storage await (R1 counters are in-memory; verifiable by construction + the existing isolation harness).

## 6. Test placement

| Test | Where | Gate |
|---|---|---|
| A1 summary determinism + terminal semantics + N=0 + partial-on-failure | `crates/aero-live-srt/src/tests.rs` (hermetic, tempdir `HlsWriter`, direct `SrtSession` drive) | `cargo test --workspace --lib` (no DB) |
| Teardown error-path drill (finish failure → partial summary, no return-early) | `crates/aero-live-srt/src/isolation_tests.rs` (extend `CountingBackend` `finalize_broken_writer` shape, E9) | hermetic |
| A2 `mark_ended_in_tx` CTE atomicity + retryable claim + idempotent replay | `crates/aero-storage/src/stream.rs` or `stream_session_end_outbox.rs` db_tests | `#[ignore = "requires live Postgres"]` + `DATABASE_URL`, throwaway DB (AGENTS.md §4.3) |
| Migration replay | `make migrate-smoke` (throwaway-DB full-chain replay) | **`cargo build` before `aero-cli migrate`** (AGENTS.md §4.2 — migrations are compile-time embedded) |

## 7. Risks / [PROPOSED] items

- **`0239_audit_governance_outbox` and stream.\* class admission are [PROPOSED]** (E11): this direction delivers the producer-side seam + row shape only. If the governance admission lands differently (e.g. a `class='stream'` enum extension vs a mapping), only the stamping side changes — the row shape and stable identity are contract-agnostic.
- **L1 aggregation window mechanics are [PROPOSED]** (B5-1 req A4): the per-session summary is the atomic unit; the window/EWMA bookkeeping stays out of scope.
- **A3 drill is server-side**: recorded as a precondition; verified when the B5-3 slice lands. This crate's cardinality guarantee (one row/session) is the only in-scope obligation.
- **New migration discipline**: adding `NNNN_stream_session_end_outbox.sql` requires `cargo build` before `aero-cli migrate`, else the migration silently no-ops (AGENTS.md §4.2). Migration ordinal = `ls migrations/*.sql | wc -l` + 1, never hardcoded.
- **file-size guard** (AGENTS.md §4.2, `scripts/file-size-check.sh` — 800 WARN / 1200 HARD for Rust): the session-end repo goes in a **new sibling file** `stream_session_end_outbox.rs` (clone of `stream_go_live_outbox.rs`, ~350 lines), not appended to `stream.rs`.
- **Hot-path invariant** (E12): any accidental storage await inside `feed_packet`/`pump` would stall every SRT publisher on the shared UDP loop — the R1 counters must be plain struct fields, no async.
- **Warn-swallow containment preserved**: `finalize_session` keeps its error-swallowing shape (isolation_tests mirror, E9); the fix is the durable retryable row, not converting teardown into a failure-propagation path (a teardown error must not take down the listener loop).
- The exact 37-test contract list is out-of-repo contract text per B5-1 req E12; the in-repo count that backs "37/37" here = 37 test fns in `tests.rs` (E8) — kept green by A4.

## 8. Sequencing

1. **Crate-only hermetic slice (R1+R2+A1)**: summary fields + terminal `finish()` + `SessionSummary` type + tests in `tests.rs`. Zero DB, zero migration — 37/37 must stay green (A4).
2. **Migration + storage slice (R3+A2)**: `NNNN_stream_session_end_outbox.sql` + `mark_ended_in_tx` CTE + `StreamSessionEndOutboxRepo` clone + db-gated A2 test. `cargo build` → `make migrate-smoke` before wiring.
3. **`finalize_session` rewiring (R4)**: call `mark_ended_in_tx` with the summary; extend `CountingBackend`/isolation tests for the finish-error-park path.
4. **Full gate (A4)**: `cargo check --workspace` · `cargo test --workspace --lib` (+ `-- --ignored` with throwaway DB) · `cargo clippy --workspace --all-targets` (no new warnings) · `scripts/{truth-check,file-size-check,web-check}.sh`.
5. **Hand off A3 precondition** to the B5-3 server-side slice: document the row shape/cardinality contract (R5) so the 500+1 drill can backfill `stream_session_end_outbox` rows.
