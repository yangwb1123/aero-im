# Requirements Spec — Live-NATS acceptance suite + G6 harness legs for the aero-bus seams, and lib.rs stream doc fix (BR6)

- **Module**: `crates/aero-bus` (+ harness `scripts/`, gate `docs/campaigns/implementation-gate.md`)
- **Direction**: "Add the live-NATS acceptance suite + G6 harness legs for the bus seams (A2/A4-bus) and fix the stale lib.rs stream doc (BR6)" — the seams' own acceptance (A2: stream declaration / Nats-Msg-Id dedup / WorkQueue-removal-on-ack; A4-bus: independent-stream isolation) is only provable against a live NATS, but `crates/aero-bus/tests/` is empty and the sole live-NATS test covers `IM_EVENTS` only; `scripts/test-integration.sh` carries the G6 scaffolding with no bus-level leg; `lib.rs` lists 3 streams while `bootstrap()` declares 4.
- **Source analysis**: `docs/auto/analyses/crates-aero-bus-f74336c1.json` (direction #3; value 7 / risk-reduction 8 / effort 3 / confidence 8)
- **Campaign**: `aero-im-b5-outbox-relay` (`docs/campaigns/campaign-aero-im-b5.yaml`); in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`; gate **G6 (B5)** = "37/37、T-11、moderation 优先级" (`docs/campaigns/implementation-gate.md:78`)
- **Status**: Requirements (verified evidence below)
- **Verification date**: 2026-08-07 (line numbers are as-of-verification anchors; drift is possible — the **file/symbol** is the stable grep anchor per AGENTS.md §0)

## 1. Evidence verification (every cited symbol checked against the repo)

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-bus/src/jetstream.rs` tests module — pure-unit patterns `im_stream_duplicate_window_covers_outbox_retries`, `pull_config_bounds_redelivery_for_poison_messages` | ✅ **Verified**. Both tests exist in the `#[cfg(test)] mod tests` at the bottom of `jetstream.rs`. `im_stream_duplicate_window_covers_outbox_retries` asserts `duplicate_window == IM_MESSAGE_DUPLICATE_WINDOW` and `duplicate_window == max_age` (7d); `pull_config_bounds_redelivery_for_poison_messages` asserts `max_deliver == POISON_MAX_DELIVER` (16) and non-zero `ack_wait` (120s). Both pure, no NATS. |
| E2 | `live_nats_deduplicates_same_message_id` — live `#[ignore]` pattern gated on `AERO__NATS__URL`, covers IM_EVENTS only | ✅ **Verified**. `jetstream.rs:469`, `#[ignore = "requires a live NATS JetStream at AERO__NATS__URL"]`, `connect(JetStreamConfig { bootstrap_streams: true })` → asserts `IM_MESSAGES` duplicate-window update → double `publish_idempotent_ack` on subject `im.events.idempotent.{nonce}` (IM_EVENTS stream) → `!first.duplicate`, `retry.duplicate`, `retry.sequence == first.sequence`; cleans up via `stream.purge().filter(subject)`. Confirmed: covers **only** `im.events.*` — no audit-namespace, no WorkQueue, no isolation assertions. |
| E3 | `crates/aero-bus/tests/` empty | ✅ **Verified**. Directory exists and is empty; `ls` yields no entries. The live-NATS pattern lives in the lib tests module (E2), not in `tests/`. |
| E4 | `crates/aero-bus/src/lib.rs:9-13` stale stream doc (BR6) | ✅ **Verified**. Doc lists exactly 3 streams: `IM_MESSAGES` (im.room.*, limits, 7d), `IM_EVENTS` (im.events.>, limits, 30d), `AI_QUEUE` (ai.queue.*, work-queue, 1d). `bootstrap()` (`jetstream.rs:237`) declares **4**: `IM_MESSAGES`, `IM_EVENTS`, `AI_QUEUE`, `LIVE_EVENTS` (live.stream.*, limits, 6h, max 200k). `LIVE_EVENTS` missing from the doc — BR6 confirmed. |
| E5 | `scripts/test-integration.sh:17` `AERO__NATS__URL` knob; `:489` export | ✅ **Verified**. Line 17: "# AERO__NATS__URL — live NATS used by ignored bus tests (default: localhost)"; line 489: `export AERO__NATS__URL="${AERO__NATS__URL:-nats://localhost:4222}"`. Step 4 (lines 503–517) runs `cargo test --workspace --lib --locked -- --ignored --test-threads=1` with this exported — so the existing live test (E2) is *already* exercised by the harness whenever NATS is reachable (evidence correction to the proposed note "today only manual #[ignore] runs exist": it is manual-only as a *named, verdict-bearing* leg, but the blanket `--ignored` run already executes it). |
| E6 | `scripts/test-integration.sh:147` (b5-pin.sh source), `:149` (guard self-test), `:166`/`:206` (`b5_check` PASS helper) | ✅ **Verified**. Line 147: `source "$(dirname ...)/b5-pin.sh"`; line 149: `bash .../test-b5-pin-guard.sh`; lines 166 and 206: `b5_check "${2}" "PASS"` inside `run_migration_regression` / `run_migrated_integration`. |
| E7 | `scripts/test-integration.sh:436` (t11-fail-closed), `:441-451` (moderation-priority drill) | ✅ **Verified**. Line 436: `b5_check "t11-fail-closed" "PASS"` (drill: relay missing → no grant, 0236 trigger RAISE / connector 403→dead verdicts in leg C, ~:425-436). Lines 441–461: moderation-priority drill — comment at 441-447 ("500 backlog rows + 1 moderation row; must be claimed and delivered first"), gated `if [ -f "migrations/0239_audit_governance_outbox.sql" ]`, `aero-audit-priority-drill` run, `b5_check "moderation-priority-drill" "PASS"` at 459, `SKIP (0239 not landed)` at 461. |
| E8 | `scripts/b5-pin.sh` 37/37 manifest + `b5_check`/`assert_b5_contract_pin` | ✅ **Verified**. `B5_CONTRACT_TEST_LIST` = exactly **37** slots: **15 executed** (`rolling_upgrade…` … `audit-provision-check`) + **22** `[PROPOSED]` placeholders. `b5_check` (:73-78) echoes `B5-CHECK <name>: <verdict>` and appends to `$B5_LOG`. `assert_b5_contract_pin` (:84+) enforces count `-ne 37` → FAIL, no dupes, no malformed, non-vacuous, and (fresh mode) every executed slot backed by a verdict line in the log. |
| E9 | `scripts/test-b5-pin-guard.sh` guard self-test | ✅ **Verified**. Pure bash, no services. Hardcoded expectations that must be updated in lockstep with any manifest change: "list-is-exactly-37-slots" expects `B5 contract pin: 37/37 (15 executed, 22 \[PROPOSED\]): PASS`; "count-36-fails" builds 36 slots; duplicate/malformed cases index `[36]`; vacuous case builds 37 `[PROPOSED]`; SKIP_DB_CREATE case expects the same `37/37 (15 executed…)` string. |
| E10 | `docker-compose.yml` local NATS instance | ✅ **Verified**. `:43-56`: `nats:2.10-alpine` service `aero-nats`, `command: ["-js", "-sd", "/data", "-m", "8222"]`, ports `4222:4222` + `8222:8222` (bind default `127.0.0.1`), healthcheck on `:8222/healthz`. This is the CI/local instance the `#[ignore]` runs and the new legs target. |
| E11 | `docs/campaigns/implementation-gate.md:78` G6 row | ✅ **Verified**. G6 (B5) row: 通过条件 = "37/37、T-11、moderation 优先级". No bus-leg component today. |
| E12 | (supplementary) seam contracts the new tests assert | ✅ **Verified (as dependency-owned)**. The streams under test are declared by sibling directions: `AUDIT_PRIORITY` (WorkQueue, `audit.priority.*`) → `docs/requirements/2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md` (A2/A4-bus); `AUDIT_EVENTS` (Limits, `audit.ws.*`) → `docs/requirements/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.req.md` (A2: "stream declared post-bootstrap; publish_idempotent dedup collapses retries to the same sequence; durable sub resumes with DeliverPolicy::All"). Neither stream exists in the current tree (`bootstrap()` declares only the 4 of E4) — the tests here are the acceptance vehicles that go red until those land. |
| E13 | (supplementary) leg-failure pattern to mirror | ✅ **Verified**. `relay-mock-probe` leg (:533-548): failure → print output to stderr + `b5_check … "FAIL …"` + `exit 1`; gated-skip → `b5_check … "SKIP (…)"`. Empty-filter guard precedent (:198-206): `grep -Eq 'test result: ok\. [1-9][0-9]* passed'` — a filter matching 0 tests must fail, not vacuously pass. |

## 2. Verified current state (what this direction changes)

```
crates/aero-bus (this module):
  bootstrap()  ──  declares 4 streams: IM_MESSAGES (Limits, 7d) · IM_EVENTS (Limits, 30d)
                  · AI_QUEUE (WorkQueue, 1d) · LIVE_EVENTS (Limits, 6h, 200k)
  lib.rs:9-13  ──  documents only 3 (LIVE_EVENTS missing)          ← BR6 doc debt
  tests        ──  pure-unit: duplicate-window / poison-pull-config patterns (E1)
                  live #[ignore]: live_nats_deduplicates_same_message_id (IM_EVENTS only, E2)
                  tests/ directory: empty (E3)

harness (scripts/test-integration.sh):
  :17/:489     ──  AERO__NATS__URL knob + export (default nats://localhost:4222)
  :147/:149    ──  sources b5-pin.sh + runs the pin-guard self-test up front
  :436         ──  t11-fail-closed leg (0236 RAISE / connector 403→dead unchanged)
  :441-461     ──  moderation-priority drill (500 backlog + 1 moderation, 0239-gated)
  :533-548     ──  relay-mock-probe leg (FAIL + exit 1 / SKIP pattern)
  :570-573     ──  assert_b5_contract_pin "$B5_LOG" closes the 37/37 gate

b5-pin.sh       ──  37 slots = 15 executed + 22 [PROPOSED]; count hardcoded `-ne 37`
test-b5-pin-guard.sh ──  self-test literals "37/37 (15 executed, 22 [PROPOSED])"
implementation-gate.md:78 ──  G6 = "37/37、T-11、moderation 优先级" (no bus legs)
docker-compose.yml:43-56 ──  NATS 2.10 JetStream on :4222 (CI/local instance)
```

**Gaps the direction closes** (all verified): (1) the bus seams' A2/A4-bus acceptance has no live-NATS vehicle except the IM_EVENTS-only dedup test (E2) — a regression in WorkQueue retention, `duplicate_window < max_age`, unknown-prefix fail-open, or cross-stream HOL blocking passes G6 silently; (2) the harness has no bus-level leg and the 37/37 pin has no aero-bus slots (E8), so "37/37、T-11、moderation 优先级" is unmeasurable for aero-bus deliverables; (3) the lib.rs stream doc is stale (E4).

## 3. Scope

**In scope (this direction, module `crates/aero-bus` + harness + gate doc)**:
- New **`#[ignore]` live-NATS tests in `jetstream.rs`'s tests module** (placement per the direction's acceptance; same module as the existing live pattern E2 — `crates/aero-bus/tests/` stays empty), asserting the A2/A4-bus acceptance of the *priority* seam: `AUDIT_PRIORITY` exists post-bootstrap with WorkQueue retention; double `publish_idempotent` → duplicate ack with identical sequence; ack consumes the message (WorkQueue removes on ack); `im.room.*` durable consumer left pending while an `audit.priority.*` message is still delivered (independent streams/cursors).
- **Two new `b5_check` legs** in `scripts/test-integration.sh`: `bus-priority-seam` and `bus-audit-events` — FAIL RED when `AERO__NATS__URL` is reachable (any assertion failure → `FAIL` verdict + `exit 1`), SKIP clean when not reachable. The `bus-audit-events` leg exercises the A2 pattern against the audit-transport namespace (`AUDIT_EVENTS` declared post-bootstrap; `publish_idempotent` dedup on a concrete `audit.ws.*` subject; durable consumer resumes with `DeliverPolicy::All`) — the B5-1 seam's live acceptance, per sibling spec E12.
- **Manifest update** via `scripts/b5-pin.sh` (+ `scripts/test-b5-pin-guard.sh` literals) so the guard stays green.
- **G6 declaration**: `docs/campaigns/implementation-gate.md` G6 row names the two bus legs among the 通过条件; the harness closure (`assert_b5_contract_pin`) mechanically requires their verdict lines once the manifest lists them.
- **Doc fix (BR6)**: `crates/aero-bus/src/lib.rs` stream list updated to match `bootstrap()` (all declared streams).

**Dependency-owned (acceptance checks preserved here, built by sibling directions — do not build in this module)**:
- `AUDIT_PRIORITY` stream + `audit.priority.*` mapping + WorkQueue config → **B5-3 (aero-bus)** — sibling spec `2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md`; the live tests here assert that seam end-to-end.
- `AUDIT_EVENTS` stream + `audit.ws.*` mapping → **B5-1 (aero-bus)** — sibling spec `2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.req.md`; the `bus-audit-events` leg asserts its A2.
- T-11 fail-closed drill (relay missing → no grant, 0236 trigger RAISE / connector 403→dead) → **B5-2/B5-4** — unchanged, only *declared together* in the G6 condition.
- Moderation-priority drill (500 backlog + 1 moderation → moderation claimed and delivered first) → **B5-3 (aero-storage)** claim ordering — unchanged, only *declared together* in the G6 condition.

**Out of scope**: any production code change in `crates/aero-bus` (no `stream_for_subject` extraction, no `bootstrap()` additions — those are the sibling directions'); the empty `tests/` directory (the live tests go in `jetstream.rs` per the acceptance); the out-of-repo 37/37 contract-test names (22 `[PROPOSED]` placeholders, see §7); `aero-bus` unit tests for the seams' pure functions (owned by B5-1/B5-3); changing the existing `live_nats_deduplicates_same_message_id` behavior.

## 4. Requirements

### BR1 — Live-NATS acceptance tests for the priority seam (A2/A4-bus) in `jetstream.rs` tests module
- Four new `#[tokio::test]` + `#[ignore = "requires a live NATS JetStream at AERO__NATS__URL"]` tests in `jetstream.rs`'s tests module, following the existing live-test pattern (E2): `connect(JetStreamConfig { bootstrap_streams: true })`, unique nonce subjects per run, best-effort cleanup (`stream.purge().filter(subject)`, and consumer deletion for durables).
- **T1 `live_nats_audit_priority_declared_with_workqueue_retention`**: `get_stream("AUDIT_PRIORITY")` succeeds post-bootstrap; `stream.info().config.retention == stream::RetentionPolicy::WorkQueue`; `subjects == ["audit.priority.*"]`.
- **T2 `live_nats_audit_priority_deduplicates_same_message_id`**: `publish_idempotent_ack("audit.priority.<nonce>", payload, msg_id)` twice → `!first.duplicate`, `retry.duplicate`, `retry.sequence == first.sequence` (Nats-Msg-Id dedup on the priority stream).
- **T3 `live_nats_audit_priority_ack_consumes_message`**: publish 1 message to `audit.priority.<nonce>`; before ack `stream.info().state.messages == 1` (or `stream.get_message(seq)` resolves); after a durable consumer acks it, `state.messages == 0` (WorkQueue removes on ack).
- **T4 `live_nats_im_backlog_does_not_block_audit_priority`**: publish ≥1 message on `im.room.<nonce>`; create a durable consumer on `im.room.<nonce>` and **leave it pending** (no ack); publish 1 message on `audit.priority.<nonce>`; a durable consumer on `audit.priority.<nonce>` must receive it within a bounded timeout (e.g. `tokio::time::timeout(5s)`) while the IM consumer's `consumer_info.num_pending > 0` — independent streams/cursors, no cross-stream HOL blocking.
- Tests are placed next to the existing live test (E2) in the same module; no change to the empty `tests/` directory.

### BR2 — `bus-priority-seam` + `bus-audit-events` b5_check legs in `scripts/test-integration.sh`
- New "bus-seam legs" section in the B5 acceptance-gate closure area (after the relay-mock-probe leg, before the relay-coverage count / 37/37 pin call), DB-free, NATS-only. A shared helper (e.g. `run_bus_leg <name> <cargo-filter>`) implements the contract:
  - **Probe**: parse host:port from `AERO__NATS__URL` (strip `nats://` scheme/path; default `nats://localhost:4222`); TCP-reachability probe with a short timeout (e.g. `timeout 5 bash -c "</dev/tcp/$host/$port"`). Unparseable URL or failed connect → **unreachable**.
  - **Unreachable** → `b5_check "<name>" "SKIP (NATS unreachable)"` and continue (clean skip, does not fail the harness).
  - **Reachable** → `cargo test -p aero-bus --lib --locked "<filter>" -- --ignored --test-threads=1` (filter names per §6). Apply the empty-filter guard (pattern at `:198-206`): the output must match `test result: ok\. [1-9][0-9]* passed` — a filter matching 0 tests is a FAIL, never a vacuous PASS.
  - **Pass** → `b5_check "<name>" "PASS"`; **any failure** → print output to stderr, `b5_check "<name>" "FAIL (<detail>)"`, `exit 1` (mirror the relay-mock-probe leg, E13).
- `bus-priority-seam` runs the T1–T4 filter (`live_nats_audit_priority`); `bus-audit-events` runs the audit-transport filter (`live_nats_audit_events`, BR3).
- Both legs are verdict-bearing: their lines land in `$B5_LOG` and are therefore required by `assert_b5_contract_pin` once listed in the manifest (BR4) — this is the mechanical G6 enforcement.

### BR3 — `bus-audit-events` leg content (A2 for the audit-transport namespace)
- Live-NATS tests backing the leg (same module, same `#[ignore]` gate): `live_nats_audit_events_declared_post_bootstrap` — `get_stream("AUDIT_EVENTS")` succeeds with Limits retention and `subjects == ["audit.ws.*"]`; `live_nats_audit_events_deduplicates_same_message_id` — double `publish_idempotent_ack("audit.ws.<nonce>", …)` → second ack duplicate with identical sequence; `live_nats_audit_events_durable_resumes_all` — a durable consumer on a concrete `audit.ws.*` subject created via `poison_safe_pull_config(subject, Some("aero-audit-connector"))` starts at `DeliverPolicy::All` and receives a message published before its creation.
- Content mirrors the B5-1 sibling spec's live-NATS acceptance (E12); assertions stay at the seam level (declaration / dedup / resume) — no relay or exactly-N expansion mechanics in this module.

### BR4 — 37/37 manifest updated via `scripts/b5-pin.sh`; guard stays green
- `B5_CONTRACT_TEST_LIST` gains exactly two **executed** slots: `bus-priority-seam` and `bus-audit-events` (placed with the other executed drill slots, e.g. after `audit-provision-check`); the 22 `[PROPOSED]` placeholders are untouched → new total **39 slots = 17 executed + 22 [PROPOSED]**.
- `assert_b5_contract_pin`'s exact-count literal (currently `-ne 37` at `b5-pin.sh:87`) and the header comments ("37 slots: 15 executed + 22 [PROPOSED]") are updated to the new total in the same change.
- `scripts/test-b5-pin-guard.sh` literals updated in lockstep: "B5 contract pin: 39/39 (17 executed, 22 [PROPOSED]): PASS" (both occurrences), count-36 case → count-38, duplicate/malformed indices `[36]` → `[38]`, vacuous case → 39 `[PROPOSED]` slots.
- **Result**: `bash scripts/test-b5-pin-guard.sh` exits 0 (all positive and negative cases pass), and `scripts/test-integration.sh`'s fresh-mode closure requires verdict lines for both new slots (mechanical G6 enforcement).

### BR5 — G6 declared green only when the bus legs pass together with T-11 and the moderation-priority drill
- `docs/campaigns/implementation-gate.md` G6 row's 通过条件 updated from "37/37、T-11、moderation 优先级" to name the two bus legs, e.g. "37/37（含 bus-priority-seam / bus-audit-events）、T-11、moderation 优先级" (bus legs PASS when `AERO__NATS__URL` reachable, SKIP-with-reason when not).
- The harness closure (`assert_b5_contract_pin "$B5_LOG"`, `test-integration.sh:570-573`) already requires a `B5-CHECK <name>: PASS|SKIP` line for every executed slot — with BR4 the bus legs are gated by it; no separate gate code. The T-11 leg (`:436`) and moderation-priority drill (`:459`) are unchanged (E7) — they are *declared together* with the bus legs, not modified.
- G6 is declared green in the gate doc only when: both bus legs have PASS (or SKIP-with-reason when NATS unreachable) verdict lines **and** `t11-fail-closed` PASS **and** `moderation-priority-drill` PASS (0239 landed) **and** the 39-slot pin passes.

### BR6 — `crates/aero-bus/src/lib.rs` stream doc lists all declared streams
- The doc comment (lines 9–13) is updated to list **all four** streams `bootstrap()` declares, e.g. adding:
  `//! - LIVE_EVENTS  subjects = live.stream.*  retention=limits, 6h, file (max 200k msgs)`.
- Invariant stated in the doc: the list must match `bootstrap()` (currently 4 streams). When the B5-1/B5-3 sibling directions add `AUDIT_EVENTS`/`AUDIT_PRIORITY`, the doc is extended by those directions (their BR6); this direction fixes the *current* staleness only.

## 5. Acceptance checks (preserved from the direction, made testable)

### A1 — New `#[ignore]` live-NATS tests in `jetstream.rs` (A2/A4-bus) **[aero-bus]**
*Preserves: "AUDIT_PRIORITY exists post-bootstrap with WorkQueue retention; double publish_idempotent -> duplicate ack with identical sequence; ack consumes the message (WorkQueue removes on ack); im.room.* durable consumer left pending while an audit.priority.* message is still delivered (independent streams/cursors)."*
- **Assert 1 (T1)**: `connect(bootstrap_streams: true)` → `get_stream("AUDIT_PRIORITY")` exists with `retention == WorkQueue` and `subjects == ["audit.priority.*"]`.
- **Assert 2 (T2)**: `publish_idempotent_ack` twice with the same `Nats-Msg-Id` on `audit.priority.<nonce>` → second ack reports `duplicate == true` with `sequence == first.sequence`.
- **Assert 3 (T3)**: a durable consumer acks the single retained message on `audit.priority.<nonce>` → `state.messages` drops 1 → 0 (WorkQueue removes on ack).
- **Assert 4 (T4)**: `im.room.<nonce>` durable consumer left pending (`num_pending > 0`) while an `audit.priority.<nonce>` consumer receives its message within a 5s timeout — independent streams/cursors.
- **Run**: `AERO__NATS__URL=nats://localhost:4222 cargo test -p aero-bus --lib --locked live_nats_audit_priority -- --ignored --test-threads=1` → all pass. These tests **fail red until the B5-3 seam lands** (AUDIT_PRIORITY does not exist in the current tree, E4/E12) — the intended honest signal.

### A2 — New b5_check legs fail red when NATS reachable, SKIP clean when not **[aero-bus + harness]**
*Preserves: "New b5_check legs in scripts/test-integration.sh ('bus-priority-seam' and 'bus-audit-events') that FAIL RED when AERO__NATS__URL is reachable and SKIP clean when not."*
- **Reachable** (`/dev/tcp` probe succeeds): each leg runs its cargo filter; failure → `B5-CHECK <leg>: FAIL (<detail>)` in `$B5_LOG` + stderr detail + `exit 1` (harness red); success → `B5-CHECK <leg>: PASS`.
- **Unreachable** (probe fails or URL unparseable): `B5-CHECK <leg>: SKIP (NATS unreachable)`; harness continues (clean skip).
- **Empty-filter guard**: a filter matching 0 tests is FAIL, never vacuous PASS (mirror `:198-206`).
- **Manual check**: with docker-compose NATS up and the B5-3/B5-1 seams landed, run `scripts/test-integration.sh` → B5 log contains `B5-CHECK bus-priority-seam: PASS` and `B5-CHECK bus-audit-events: PASS`.

### A3 — 37/37 manifest updated; guard self-test stays green **[harness]**
*Preserves: "37/37 manifest updated via scripts/b5-pin.sh so scripts/test-b5-pin-guard.sh stays green."*
- `B5_CONTRACT_TEST_LIST` contains exactly 39 slots: 17 executed (15 existing + `bus-priority-seam` + `bus-audit-events`) + 22 `[PROPOSED]`; no duplicates, no malformed entries.
- `assert_b5_contract_pin` exact-count literal and `test-b5-pin-guard.sh` literals updated in the same change.
- **Run**: `bash scripts/test-b5-pin-guard.sh` → exit 0 ("all regression cases passed"). In fresh mode, `assert_b5_contract_pin` fails if either bus leg lacks a verdict line.

### A4 — G6 declared green only when the bus legs pass together with T-11 and the moderation-priority drill **[gate + harness]**
*Preserves: "G6 declared green only when the bus legs pass together with the T-11 fail-closed drill (relay missing -> no grant, 0236 trigger RAISE / connector 403->dead unchanged) and the moderation-priority drill (500 backlog + 1 moderation -> moderation claimed and delivered first)."*
- `docs/campaigns/implementation-gate.md` G6 row names both bus legs (BR5).
- **Run** (fresh mode, NATS up, seams landed): `scripts/test-integration.sh` → B5 log contains `B5-CHECK bus-priority-seam: PASS`, `B5-CHECK bus-audit-events: PASS`, `B5-CHECK t11-fail-closed: PASS`, `B5-CHECK moderation-priority-drill: PASS`, and `assert_b5_contract_pin` passes (39/39). Removing any one of these verdict lines makes the pin fail — the gate is unmeasurable-silent no longer.
- **Unchanged**: the T-11 drill's gates (0236 trigger RAISE / connector 403→dead) and the moderation-priority drill body — this direction touches neither.

### A5 — lib.rs doc lists all declared streams (BR6) **[aero-bus]**
*Preserves: "lib.rs doc updated to list all declared streams (BR6)."*
- `crates/aero-bus/src/lib.rs` doc comment lists `IM_MESSAGES`, `IM_EVENTS`, `AI_QUEUE`, **and `LIVE_EVENTS`** (4 streams = `bootstrap()`'s declarations).
- **Check**: `grep -c "^- \`" crates/aero-bus/src/lib.rs` → 4; each name also present in `bootstrap()`'s `get_or_create_stream` calls in `jetstream.rs`.

## 6. Test placement

| Item | Location | Harness |
|---|---|---|
| T1–T4 priority-seam live tests (`live_nats_audit_priority_*`) | `crates/aero-bus/src/jetstream.rs` tests module, next to `live_nats_deduplicates_same_message_id` | `#[ignore]` live NATS at `AERO__NATS__URL` — pattern: E2 (nonce subjects, purge cleanup) |
| Audit-transport live tests (`live_nats_audit_events_*`) | same module | `#[ignore]` live NATS; assertions per sibling B5-1 spec E12 |
| `bus-priority-seam` / `bus-audit-events` legs | `scripts/test-integration.sh`, B5 closure section (after relay-mock-probe leg) | DB-free; probe `/dev/tcp`; verdict via `b5_check`; FAIL → `exit 1`; SKIP (NATS unreachable) clean |
| Manifest + guard literals | `scripts/b5-pin.sh` (`B5_CONTRACT_TEST_LIST`, count literal) + `scripts/test-b5-pin-guard.sh` | `bash scripts/test-b5-pin-guard.sh` → exit 0; fresh-mode `assert_b5_contract_pin` in `test-integration.sh` |
| G6 row | `docs/campaigns/implementation-gate.md:78` | manual doc check; mechanically enforced by the pin closure |
| BR6 doc | `crates/aero-bus/src/lib.rs:9-13` | grep parity check (A5) |

Filter names are normative for the legs (BR2): `bus-priority-seam` → `live_nats_audit_priority`; `bus-audit-events` → `live_nats_audit_events`. Legs must use `-p aero-bus --lib --locked` (the tests live in the lib tests module) and `--test-threads=1` (consistent with Step 4).

## 7. Risks / [PROPOSED] items

- **R-1 — Red-until-seams-land is the intended signal, not a broken harness**: A1's tests and A2's legs fail red whenever NATS is reachable until the B5-3 (`AUDIT_PRIORITY`) / B5-1 (`AUDIT_EVENTS`) seams land (E12). This mirrors the moderation-priority drill's contract ("FAILS red if 0239 lands without the priority ordering"). Land the harness legs in the same wave as the seams, or accept a red G6 in the interim — a silent-green bus is the failure mode this direction eliminates.
- **R-2 — Manifest count 37 → 39**: adding two executed slots changes the pinned total; the "37/37" naming is campaign shorthand (G6, E11) and stays, while the guard's exact-count literal and the self-test literals move to 39/39 (17 executed). If the campaign prefers keeping exactly 37 slots, the alternative is dropping two `[PROPOSED]` placeholders — flagged, not the default (the placeholders represent out-of-repo contract coverage).
- **R-3 — WorkQueue test hygiene**: T2/T3 publish to `audit.priority.<nonce>` with no consumer — WorkQueue retains unacked messages up to `max_age`, so nonce subjects + purge cleanup are mandatory to keep repeated runs green (existing E2 cleanup pattern). T4 must delete its created durable consumers (best-effort) so `consumer_info` lookups don't collide across runs.
- **R-4 — Existing harness Step 4 already runs `--ignored`**: the new live tests also execute inside the blanket `cargo test --workspace --lib -- --ignored` run (`test-integration.sh:503-517`) whenever NATS is reachable — the legs add *named, attributable* verdicts on top; a failure there and in the legs is the same root cause (evidence correction to the direction's proposed note "today only manual #[ignore] runs exist").
- **R-5 — [PROPOSED] out-of-repo contract text**: the 22 out-of-repo contract-test slots stay placeholders (E8) — pinning the actual out-of-repo test list into `scripts/test-integration.sh` per the proposal is a separate step, out of scope here; the two new legs are in-repo executed slots.
- **R-6 — [PROPOSED] CI NATS**: the legs (and Step 4's blanket run) require a reachable NATS — `docker-compose.yml` provides it (E10); CI must start the `aero-nats` service for the legs to be live rather than SKIP. Without it, G6 still passes (SKIP-with-reason is a handled verdict) but the bus acceptance is unexercised — the honest-gate property depends on CI actually running NATS.
- **R-7 — Doc-parity drift**: BR6 fixes the doc once; the invariant ("doc lists every stream `bootstrap()` declares") is maintained by the sibling directions' own BR6 edits when they add streams (E12). No new test enforces the invariant; the A5 grep check is the manual guard.

## 8. Sequencing

1. **This direction (aero-bus + harness + gate)**: BR1–BR6 — purely additive: `#[ignore]` tests, two harness legs, manifest + guard literals, gate-doc row, lib.rs doc. No production code, no migration, no trait change. Red on NATS until the seams land (R-1) — land in the same wave as B5-1/B5-3.
2. **B5-3 (aero-bus)**: `AUDIT_PRIORITY` stream + `audit.priority.*` mapping — makes T1–T4 and `bus-priority-seam` green.
3. **B5-1 (aero-bus)**: `AUDIT_EVENTS` stream + `audit.ws.*` mapping — makes the `bus-audit-events` backing tests green; extends the lib.rs doc (its BR6).
4. **B5-3 (aero-storage)**: 0239 governance outbox + `ORDER BY priority DESC` claim — makes the moderation-priority drill green.
5. **B5-2/B5-4**: connector + provisioning gates — T-11 (403→dead, relay missing → no grant) unchanged; G6 is declared green only when all four verdict lines (both bus legs + t11 + moderation-priority) are PASS with the 39-slot pin (A4).
