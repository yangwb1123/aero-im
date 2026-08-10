# Requirements Spec — Live-NATS acceptance suite + harness legs for the audit lane (tests-only: WorkQueue retention, dedup collapse/restart survival, cross-stream isolation)

- **Module**: `crates/aero-bus` (tests only — zero production code) + harness `scripts/`
- **Direction**: "Add the live-NATS acceptance suite + harness legs for the audit lane (tests-only: WorkQueue retention, dedup collapse/restart survival, cross-stream isolation) so moderation-priority and T-11 bus-level legs are executable in CI"
- **Source analysis**: `docs/auto/analyses/crates-aero-bus-src-3f54dec6.json` (selected direction; value 6 / risk-reduction 6 / effort 3 / confidence 8)
- **Design pin**: `docs/design/2026-08-07-aero-bus-live-nats-acceptance-suite.design.md` (§0 E1–E14 verification table, §1 test list, 37→39 pin change)
- **Supersedes (scope revision)**: `docs/requirements/2026-08-07-aero-bus-live-nats-acceptance-suite.req.md` — that file additionally carried BR6 (lib.rs doc fix) and BR5 (gate-doc G6 row), which are **not** in this direction's acceptance ("tests-only", "zero production API changes"); they are out of scope here and owned by the sibling seam directions (§3)
- **Campaign**: `aero-im-b5-outbox-relay`; gate **G6 (B5)** = "37/37、T-11、moderation 优先级" (`docs/campaigns/implementation-gate.md:78`)
- **Sibling seam contracts asserted by these tests** (dependency-owned, unlanded — the suite goes red until they land): `AUDIT_PRIORITY` (WorkQueue, `audit.priority.*`, `duplicate_window == max_age` = 24h) → `docs/requirements/2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md` (A4 bus half) + `docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md:92-100`; `AUDIT_EVENTS` (Limits, `audit.ws.*`, 7d, explicit `duplicate_window == max_age`) → `docs/requirements/2026-08-08-aero-bus-b5-1-audit-transport-seam.req.md` (BR1/BR2, E13)
- **Status**: Requirements (evidence verified below)
- **Verification date**: 2026-08-08 (line numbers are as-of-verification anchors; the **file/symbol** is the stable grep anchor per AGENTS.md §0)

## 1. Evidence verification (every cited file/symbol checked against the tree)

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-bus/src/jetstream.rs` — `live_nats_deduplicates_same_message_id` (:469), `poison_safe_pull_config` tests, `im_stream_duplicate_window_covers_outbox_retries` | ✅ **Verified**. Test module at `jetstream.rs:420`. `im_stream_duplicate_window_covers_outbox_retries` :461 asserts `duplicate_window == IM_MESSAGE_DUPLICATE_WINDOW` **and** `duplicate_window == max_age` (7d, :134-135); `live_nats_deduplicates_same_message_id` :469-505 — `#[ignore = "requires a live NATS JetStream at AERO__NATS__URL"]`, `JetStreamBus::connect(JetStreamConfig { url, bootstrap_streams: true })`, double `publish_idempotent_ack` on `im.events.idempotent.{nonce}` (IM_EVENTS-only), asserts `!first.duplicate` / `retry.duplicate` / `retry.sequence == first.sequence`, purge cleanup. `pull_config_bounds_redelivery_for_poison_messages` :507-511 (max_deliver=16, ack_wait=120s). **No audit namespace, no WorkQueue, no restart, no isolation coverage — gap confirmed.** |
| E2 | `crates/aero-bus/tests/` empty | ⚠️ **Correction C1**: the directory **does not exist** (`ls` → "No such file or directory"), it is not merely empty (the 08-07 design doc's E3 "Directory exists, zero entries" is inaccurate). Same gap either way; placement stays in the `jetstream.rs` tests module (E1) and `tests/` stays absent — creating it is out of scope. |
| E3 | `bootstrap()` declares exactly 4 streams — no audit namespace | ✅ **Verified**. `jetstream.rs:237-298`: `IM_MESSAGES` (`im.room.*`, Limits, 7d, `duplicate_window == max_age`), `IM_EVENTS` (`im.events.>`, Limits, 30d), `AI_QUEUE` (`ai.queue.*`, **WorkQueue**, 1d — the retention precedent), `LIVE_EVENTS` (`live.stream.*`, Limits, 6h, 200k). `subscribe()` :338-353: 4-prefix inline `starts_with` chain, unknown → `Err(BusError::Nats("unknown subject prefix: …"))` (fail-closed). `rg 'audit\.(priority|ws)\.' crates/aero-bus` = 0 hits. |
| E4 | async_nats 0.36 API surface needed by the tests | ✅ **Verified** in vendored source (`async-nats-0.36.0`): `Stream::info(&mut self)` (stream.rs:148), `purge()` + `.filter(subject)` (:669), `create_consumer` (:723), `consumer_info` (:815), `delete_consumer(&self, &str)` (:930); `PublishAck { sequence: u64, duplicate: bool }` (publish.rs:19-30); `RetentionPolicy`/`DeliverPolicy` enums. `futures` is a **regular** dependency of `aero-bus` (Cargo.toml) — `StreamExt` is available in tests with **no Cargo.toml change** (`--locked` stays valid). |
| E5 | Restart-survival fact base (the trap) | ✅ **Verified**. `docs/design/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.design.md` §0.1 L2 (:39-43): live-verified against nats v2.10.29 — `docker restart aero-nats` → re-publish of the same id still collapsed; `rebuildDedupe` (server/stream.go:993) re-indexes in-window `Nats-Msg-Id` headers from `GetSeqFromTime(now - Duplicates)`; with `duplicate_window == max_age` the rebuild is **complete**. §0.1 L1 (:34): an **unset** window is server-defaulted to `min(2m, …, max_age)` — the silent-shrink trap the tests' `duplicate_window == max_age` assertion guards. |
| E6 | `scripts/test-integration.sh:17` knob, `:489` export, `:503-517` blanket `--ignored` run | ✅ **Verified**. :17 `AERO__NATS__URL` knob comment; :489 `export AERO__NATS__URL="${AERO__NATS__URL:-nats://localhost:4222}"`; :505-517 `cargo test --workspace --lib --locked -- --ignored --test-threads=1` (+ 10 `--skip` entries), failure → `exit "$TEST_EXIT"` (:526). The existing live test (E1) already runs in-harness whenever NATS is reachable — the new tests execute there too (R-4). |
| E7 | `scripts/test-integration.sh` B5 scaffolding anchors | ✅ **Verified**. :147 `source …/b5-pin.sh`; :149 `bash …/test-b5-pin-guard.sh` (guard self-test before DB work); empty-filter guard pattern :198-206 (`grep -Eq 'test result: ok\. [1-9][0-9]* passed'` — a 0-test match is a FAIL); :436 `b5_check "t11-fail-closed" "PASS"`; :441-461 moderation-priority drill (0239-gated, `b5_check "moderation-priority-drill" "PASS"` :459, `SKIP (0239 not landed)` :461); relay-mock-probe leg :533-549 (FAIL + stderr + `exit 1` / `SKIP (…)` pattern); relay-coverage count :580-586; `assert_b5_contract_pin "$B5_LOG"` :593-595. **No bus legs exist** (`rg 'run_bus_leg|bus-priority|bus-audit' scripts/` = 0 hits). |
| E8 | `scripts/b5-pin.sh` 37 slots (15 executed + 22 `[PROPOSED]`); count literal; final echo | ✅ **Verified**. `B5_CONTRACT_TEST_LIST=(` at :29, list :29-68 (15 executed + 22 `[PROPOSED]` = 37); `b5_check` :73-78; `assert_b5_contract_pin` :84+ with exact-count `-ne 37` :87; final echo `B5 contract pin: 37/37 (${executed} executed, ${proposed} [PROPOSED]): PASS` :130. (Direction cites ":23-46" — header comment spans :1-28; the list itself is :29-68; drift noted.) |
| E9 | `scripts/test-b5-pin-guard.sh` hardcoded literals | ✅ **Verified** (119 lines, pure bash). `"B5 contract pin: 37/37 (15 executed, 22 \[PROPOSED\]): PASS"` ×2 (:64 positive, :113 SKIP_DB_CREATE case); count-36-fails case; duplicate/malformed indices `[36]` :80/:88; vacuous `seq -w 1 37` :96. |
| E10 | `.github/workflows/ci.yml:85,161` — CI starts NATS + exports URL | ✅ **Verified**. :85-93 `docker run --detach --name aero-nats --publish 4222:4222 nats:2.10-alpine -js` + `/dev/tcp/127.0.0.1/4222` readiness probe (30×1s, logs + exit 1 on failure); :161 `AERO__NATS__URL: nats://localhost:4222` in the integration job env. **The legs and the blanket run are live in CI, not SKIP.** |
| E11 | `docker-compose.yml:43-56` — local NATS instance | ✅ **Verified**. `nats:2.10-alpine`, `container_name: aero-nats`, `command: ["-js", "-sd", "/data", "-m", "8222"]`, ports 4222/8222, healthcheck `:8222/healthz`. The container name `aero-nats` is the T4 restart test's target (E10 + E11 agree). |
| E12 | `docs/campaigns/implementation-gate.md:78` G6 row | ✅ **Verified**. `| G6（B5） | B5-1..4 | 37/37、T-11、moderation 优先级 |`. "37/37" is campaign shorthand; the mechanical gate is the pin (E8) — this direction updates the pin, not the shorthand. |
| E13 | Design doc pins 7 tests, zero production API changes, 37→39 | ✅ **Verified**. `docs/design/2026-08-07-aero-bus-live-nats-acceptance-suite.design.md` §1: zero production API changes (all test code reuses private items via descendant-module access); §2 test design; §3 `run_bus_leg` helper + 2 invocations (`bus-priority-seam`, `bus-audit-events`); §4/§6: b5-pin 37→39, guard literals, `--test-threads=1`, nonce hygiene. **Reconciliation with the direction's enumeration — see C2.** |
| E14 | aero-bus has no sink transport (A5/T-11 constraint is statically checkable) | ✅ **Verified**. `crates/aero-bus/Cargo.toml` deps: aero-common, tokio, tokio-util, futures, async-trait, async-nats, serde, serde_json, thiserror, anyhow, tracing, bytes — **no reqwest/hyper, no aero-audit-connector**. The audit lane in aero-bus is transport-only; the sink is reachable only through `aero-audit-connector`'s claim/provision gates (`pg.rs:117` `ORDER BY priority DESC`, B5-2/B5-4). |

**Corrections to the supplied evidence** (all other cited claims verified as-is):

- **C1 — `tests/` does not exist**: `crates/aero-bus/tests/` is absent entirely, not "empty" (E2). The gap is identical; placement per the design pin is the `jetstream.rs` tests module, and the absent directory stays absent.
- **C2 — the 7-test list reconciliation (normative)**: the direction's acceptance enumerates **5** behaviors (WorkQueue declaration **with `duplicate_window == max_age`**, dedup `duplicate:true` + same seq, durable subscribe + ack removes from WorkQueue, **dedup survives docker restart**, isolation) and pins the count at **7**, citing the design doc's "exact test list". The 08-07 design doc's list (T1–T4 + 3 audit-events tests) has **no restart test** and its T1 deliberately does **not** assert `duplicate_window == max_age`. Per "preserve the supplied acceptance checks", the normative list here is: **T1–T5 below (5 priority-seam tests, incl. restart and the `duplicate_window == max_age` assertion — the latter consistent with the b5-3 seam contract, E13/E3: `duplicate_window: AUDIT_PRIORITY_MAX_AGE`, design :100) + T6–T7 (2 audit-events tests) = 7**. The design doc's third BR3 live test (`live_nats_audit_events_durable_resumes_all`) is **not** double-owned here: its assertion (`poison_safe_pull_config("audit.ws.<uuid>", Some("aero-audit-connector"))` → `DeliverPolicy::All`, pre-created message delivered) is B5-1's own unit test per `2026-08-08-aero-bus-b5-1-audit-transport-seam.req.md` (A-check "Unit: poison_safe_pull_config …"), and B5-1 additionally owns its own live tests (`live_nats_audit_stream_declared_and_deduplicates`, `live_nats_audit_expansion_publishes_exactly_n_frames`, ibid. E13/§8). Count stays 7 per the direction.
- **C3 — scope trimmed to tests-only**: the 08-07 req's BR6 (lib.rs doc — stale, lists 3 streams vs `bootstrap()`'s 4) and BR5 (gate-doc G6 row) are **not** in this direction's acceptance ("tests-only", "zero production API changes"). lib.rs doc ownership passes to the sibling seam directions (B5-1's 08-08 req E11/BR6 rewrites the doc to 5 streams when `AUDIT_EVENTS` lands). The G6 "37/37" shorthand stays (E12).

## 2. Verified current state (what this direction changes)

```
crates/aero-bus (this module, all verified):
  bootstrap()   ── 4 streams: IM_MESSAGES (Limits, 7d, dup-window==max_age) · IM_EVENTS (Limits, 30d)
                   · AI_QUEUE (WorkQueue, 1d) · LIVE_EVENTS (Limits, 6h, 200k)   ← no audit namespace (E3)
  subscribe()   ── 4-prefix inline chain, unknown → fail-closed (E3)
  tests         ── pure-unit: duplicate-window / poison-pull-config (E1)
                   live #[ignore]: live_nats_deduplicates_same_message_id (IM_EVENTS-only, :469)
                   tests/ directory: absent (C1)

harness (scripts/test-integration.sh):
  :147/:149     ── sources b5-pin.sh + runs guard self-test up front
  :198-206      ── empty-filter guard pattern (legs must mirror)
  :436          ── t11-fail-closed leg; :441-461 moderation-priority drill (0239-gated)
  :489/:505-517 ── AERO__NATS__URL export + blanket --ignored run (executes the new tests already)
  :533-549      ── relay-mock-probe leg (FAIL + exit 1 / SKIP pattern) ← bus legs insert after this
  :593-595      ── assert_b5_contract_pin "$B5_LOG" (mechanically requires new verdict lines once listed)

b5-pin.sh ── 37 slots = 15 executed + 22 [PROPOSED] (:29-68); count literal `-ne 37` (:87); echo :130
test-b5-pin-guard.sh ── literals "37/37 (15 executed, 22 [PROPOSED])" (:64/:113), [36] (:80/:88), vacuous 37 (:96)
CI (ci.yml:85-93, :161) ── NATS container `aero-nats` + AERO__NATS__URL; docker-compose.yml:43-56 same name
```

**Gaps closed** (all verified): (1) zero live coverage of WorkQueue retention (the `AI_QUEUE`-precedent semantics the `AUDIT_PRIORITY` seam clones), of dedup collapse across NATS restarts (`rebuildDedupe`, E5), or of cross-stream isolation — a regression in any of these passes G6 silently today; (2) the harness has no bus-level leg and the 37-slot pin has no aero-bus slots, so the bus half of "37/37、T-11、moderation 优先级" is unmeasurable; (3) no test asserts the `duplicate_window == max_age` equality live (the unset-window 2m trap, E5).

## 3. Scope

**In scope (tests-only + harness + pin; zero production API changes, zero Cargo.toml edits — `--locked` stays valid)**:
- **7 new `#[tokio::test] #[ignore]` live-NATS tests** in `jetstream.rs`'s tests module (placement: after `live_nats_deduplicates_same_message_id` at :505, before `pull_config_bounds_redelivery_for_poison_messages` at :507), following the E1 pattern (`connect(JetStreamConfig { bootstrap_streams: true })`, nonce'd subjects/durables, best-effort purge + consumer deletion cleanup) — §4 BR1.
- **Two new verdict-bearing harness legs** `bus-priority-seam` + `bus-audit-events` in `scripts/test-integration.sh` via a shared `run_bus_leg` helper (DB-free; `/dev/tcp` probe; SKIP when NATS unreachable; FAIL + `exit 1` otherwise; empty-filter guard; **plus the A5 transport-only grep guard, §4 BR4**) — inserted after the relay-mock-probe block (:549) and before the Relay coverage (AC4) block (:580). The existing `assert_b5_contract_pin` closure (:593-595) mechanically requires their verdict lines once the manifest lists them.
- **Pin update**: `scripts/b5-pin.sh` 37→39 (17 executed + 22 `[PROPOSED]`), `-ne 37`→`-ne 39`, header comments, final echo; `scripts/test-b5-pin-guard.sh` literals re-synced in the same change — §4 BR3.

**Dependency-owned (the seams these tests assert; unlanded today — the suite is red until they land, R-1)**:
- `AUDIT_PRIORITY` stream (`audit.priority.*`, WorkQueue, 24h, `duplicate_window == max_age`) + `AUDIT_PRIORITY_SUBJECT_PREFIX` + `stream_for_subject` mapping + B5-3's own unit tests → **B5-3 (aero-bus)** — `docs/requirements/2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md` BR1/BR5, design :54/:92-100.
- `AUDIT_EVENTS` stream (`audit.ws.*`, Limits, 7d, explicit `duplicate_window == max_age`) + `AUDIT_SUBJECT_PREFIX` + mapping + B5-1's unit tests (incl. the `poison_safe_pull_config`/`DeliverPolicy::All` durable-resume case and its own live tests) → **B5-1 (aero-bus)** — `docs/requirements/2026-08-08-aero-bus-b5-1-audit-transport-seam.req.md` BR1/BR2.
- Sink delivery + provisioning gates (T-11's 403→dead / relay-missing→no-grant **behavior**) → **B5-2/B5-4** (`aero-audit-connector`); the moderation-priority claim ordering drill → **B5-3 (aero-storage)**. This direction only makes the bus-level legs executable; it changes none of those legs' bodies (:436/:441-461 untouched).

**Out of scope**: any production change in `crates/aero-bus` (no `bootstrap()` additions, no `subscribe()` mapping, no `stream_for_subject` extraction — sibling-owned); the lib.rs stream doc (C3; sibling BR6); the gate-doc G6 row text ("37/37" shorthand stays, E12); creating `crates/aero-bus/tests/`; modifying `live_nats_deduplicates_same_message_id`; any `Cargo.toml`/dependency change.

## 4. Requirements

### BR1 — 7 live-NATS tests in the `jetstream.rs` tests module (test list normative per C2)

Common template (mirrors E1): `let url = std::env::var("AERO__NATS__URL").expect("AERO__NATS__URL")` → `JetStreamBus::connect(JetStreamConfig { url, bootstrap_streams: true })` → nonce = `SystemTime::now().as_nanos()` → nonce'd subjects/durables → best-effort cleanup (pre-delete stale durables, purge subjects, post-delete durables). Module-local additions: `use futures::StreamExt;` and `use async_nats::jetstream::stream;` (for `RetentionPolicy`) — both already available via existing deps (E4).

Priority seam — filter `live_nats_audit_priority` (leg `bus-priority-seam`):

- **T1 `live_nats_audit_priority_declared_with_workqueue_retention`**: `get_stream("AUDIT_PRIORITY")` succeeds post-bootstrap; `info().config.retention == stream::RetentionPolicy::WorkQueue`; `info().config.subjects == vec!["audit.priority.*".to_string()]`; **`info().config.duplicate_window == info().config.max_age`** (the seam contract pins both = 24h, E13; equality is the assertion — the unset-window 2m trap, E5).
- **T2 `live_nats_audit_priority_deduplicates_same_message_id`**: `publish_idempotent_ack("audit.priority.{nonce}", payload, "aero-bus-test-{nonce}")` twice → `!first.duplicate`, `retry.duplicate`, `retry.sequence == first.sequence`; purge cleanup.
- **T3 `live_nats_audit_priority_ack_consumes_message`** (WorkQueue retention semantics): publish 1 → `info().state.messages == 1` (unacked WorkQueue message retained); durable consumer via `poison_safe_pull_config(subject, Some(&durable))` (`DeliverPolicy::All` ⇒ pre-created message delivered) → `tokio::time::timeout(5s, msgs.next())` → `msg.ack()` → short poll (`wait_for_state` helper, 3 × 100ms, re-fetching `info()`) until `state.messages == 0` — **WorkQueue removes on ack** (AI_QUEUE precedent semantics); cleanup delete consumer + purge.
- **T4 `live_nats_audit_priority_dedup_survives_nats_restart`** (restart survival, E5): publish with id M on `audit.priority.{nonce}` → assert `!first.duplicate` → **`docker restart aero-nats`** via `std::process::Command` (container name is the CI/compose contract, E10/E11; assert the command succeeds — `docker` presence and container `aero-nats` are environment preconditions of this opt-in live test, not skip conditions) → reconnect (fresh `JetStreamBus`, retry connect with bounded backoff, e.g. ≤30s) → re-publish id M → `retry.duplicate && retry.sequence == first.sequence` (broker-side `rebuildDedupe` re-indexes in-window ids after restart; `duplicate_window == max_age` makes the rebuild complete) → purge cleanup. `--test-threads=1` (harness + Step 4) guarantees no concurrent test is mid-flight during the restart.
- **T5 `live_nats_im_backlog_does_not_block_audit_priority`** (cross-stream isolation — the bus-level A4 leg): publish `im.room.{nonce}`; durable `aero-bus-test-im-{nonce}` on `im.room.{nonce}` created and **left unfetched** → `consumer_info(...).num_pending > 0` (server-side synchronous at creation; assert `> 0`, not `== 1`, for margin); publish `audit.priority.{nonce}`; durable `aero-bus-test-prio-{nonce}` → `timeout(5s, msgs.next())` must yield it (independent streams/cursors ⇒ no cross-stream HOL); ack; cleanup delete both durables + purge both subjects.

Audit-events — filter `live_nats_audit_events` (leg `bus-audit-events`):

- **T6 `live_nats_audit_events_declared_post_bootstrap`**: `get_stream("AUDIT_EVENTS")` → `retention == Limits`, `subjects == vec!["audit.ws.*".to_string()]` (transport seam contract, E13).
- **T7 `live_nats_audit_events_deduplicates_same_message_id`**: double `publish_idempotent_ack("audit.ws.{nonce}", …)` → `!first.duplicate` / `retry.duplicate` / identical `sequence`; purge cleanup.

### BR2 — `run_bus_leg` harness legs in `scripts/test-integration.sh`

Shared helper (per the design pin §3), inserted after the relay-mock-probe block (:549) and before the Relay coverage (AC4) block (:580):

```bash
# run_bus_leg <name> <cargo-filter> — DB-free live-NATS verdict leg (B5 G6).
# Reachable  → filtered `cargo test -p aero-bus --lib --locked <filter> -- --ignored
#               --test-threads=1`; empty-filter guard; FAIL verdict + exit 1 on failure.
# Unreachable → `B5-CHECK <name>: SKIP (NATS unreachable)`; harness continues.
run_bus_leg() { … }   # per design doc §3 (parse AERO__NATS__URL, /dev/tcp probe,
                      # cargo test, grep 'test result: ok\. [1-9][0-9]* passed' guard)

run_bus_leg "bus-priority-seam" "live_nats_audit_priority"   # T1-T5 (BR1)
run_bus_leg "bus-audit-events" "live_nats_audit_events"      # T6-T7 (BR1)
```

Contract: **unreachable** (unparseable URL or failed probe) → `b5_check "<name>" "SKIP (NATS unreachable)"`, continue; **reachable** → run the filter; pass → `b5_check "<name>" "PASS"`; any cargo failure → stderr detail + `b5_check "<name>" "FAIL (cargo test)"` + `exit 1`; a filter matching 0 tests → `FAIL (empty filter)` + `exit 1` (mirror :198-206 — a named leg can never be vacuously green). The **A5 transport-only grep guard (BR4)** runs inside the legs block; a violation fails the leg. Both legs are verdict-bearing: `assert_b5_contract_pin "$B5_LOG"` (:593-595) requires their lines once listed (BR3). Interaction with Step 4 (:505-517): the same tests already run there when NATS is up — the legs add named, attributable verdicts; the FAIL branches surface the same root cause if Step 4 is bypassed.

### BR3 — 37→39 pin update (lockstep, single change)

- `scripts/b5-pin.sh`: `B5_CONTRACT_TEST_LIST` gains exactly two **executed** slots `bus-priority-seam` and `bus-audit-events` (with the other executed drill slots, e.g. after `audit-provision-check`); the 22 `[PROPOSED]` placeholders untouched → **39 slots = 17 executed + 22 [PROPOSED]**. Exact-count literal `-ne 37` (:87) → `-ne 39`; header comments ("37 slots: 15 executed + 22 [PROPOSED]" :1-28) and final echo (:130) → 39/39.
- `scripts/test-b5-pin-guard.sh` re-synced in the same change: both literals (:64, :113) → `"B5 contract pin: 39/39 (17 executed, 22 \[PROPOSED\]): PASS"`; count-36-fails case → count-38; duplicate/malformed indices `[36]` (:80, :88) → `[38]`; vacuous case (:96) → `seq -w 1 39`.
- Result: `bash scripts/test-b5-pin-guard.sh` exits 0; fresh-mode `assert_b5_contract_pin` requires verdict lines for both new slots — the mechanical G6 enforcement.

### BR4 — A4/A5 mapping: isolation = moderation-priority bus leg; T-11 = transport-only constraint

- **Isolation (A4 bus half)**: T5 is the live-NATS leg of `2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md` A4 ("Bus ([aero-bus], live-NATS): backlog load on `im.room.*` … does not delay `audit.priority.*` delivery — priority consumer receives while the backlog consumer's pending > 0") and of BR5 (stream isolation / no cross-stream HOL). It goes green together with the `moderation-priority-drill` verdict (:459) — the claim-level half is B5-3 (aero-storage)'s.
- **T-11 (A5 constraint test)**: the suite asserts the b5-3 A5/DR4 constraint — *no audit-lane publish path reaches the sink without the connector's claim/provision gates* — as a static grep guard in the legs block (mechanically attached to the legs' verdicts; no third pin slot): `rg -q 'reqwest|hyper' crates/aero-bus/Cargo.toml` → must be empty and `rg -q 'aero-audit-connector' crates/aero-bus/` → must be empty (E14: both are 0 hits today; a violation → `FAIL` on the leg). Semantically the suite is transport-only: the tests publish exclusively via the idempotent `Nats-Msg-Id` seam (the claim-owner relay's event_id convention) and consume via `poison_safe_pull_config` durables; the sink is reachable only through `aero-audit-connector`'s claim/provision gates (B5-2/B5-4). The `t11-fail-closed` drill body (:425-436) is untouched.

## 5. Acceptance checks (preserved from the direction, made testable)

### A1 — 7 new `#[ignore]` live tests, all green under the harness `--ignored` run with NATS up (CI leg, not SKIP)
*Preserves: "37-tests: 7 new #[ignore] live tests (WorkQueue declaration with duplicate_window==max_age, double publish_idempotent → duplicate:true + same seq, durable subscribe + ack removes from WorkQueue, dedup survives docker restart of aero-nats, isolation: backlog consumer pending>0 on im.room.* while priority-lane consumer receives) all pass under scripts/test-integration.sh --ignored run with NATS up (CI leg, not SKIP)."*
- **Assert 1 (T1)**: post-bootstrap `get_stream("AUDIT_PRIORITY")` → `retention == WorkQueue`, `subjects == ["audit.priority.*"]`, `duplicate_window == max_age`.
- **Assert 2 (T2)**: double `publish_idempotent_ack` on `audit.priority.<nonce>` with one `Nats-Msg-Id` → second ack `duplicate == true`, `sequence == first.sequence`.
- **Assert 3 (T3)**: 1 published message → `state.messages == 1`; durable consumer (via `poison_safe_pull_config`) acks → poll → `state.messages == 0` (WorkQueue removes on ack).
- **Assert 4 (T4)**: `docker restart aero-nats` between two publishes of the same id → re-publish reports `duplicate == true` with the identical sequence (dedup survives restart; `rebuildDedupe`).
- **Assert 5 (T5)**: `im.room.<nonce>` durable left pending (`num_pending > 0`) while the `audit.priority.<nonce>` consumer receives within `timeout(5s)` — independent streams/cursors.
- **Assert 6 (T6/T7)**: `AUDIT_EVENTS` declared post-bootstrap (Limits, `audit.ws.*`); double publish on `audit.ws.<nonce>` → duplicate + identical sequence.
- **Run**: `AERO__NATS__URL=nats://localhost:4222 cargo test -p aero-bus --lib --locked -- --ignored --test-threads=1` → all 7 pass. In CI: the integration job (ci.yml :161 env, :85-93 NATS) runs `scripts/test-integration.sh` → Step 4 (:505-517) executes them live (not SKIP) and the legs (BR2) produce named PASS verdicts. **Red until the seams land** — `AUDIT_PRIORITY`/`AUDIT_EVENTS` do not exist in the tree today (E3): `get_stream` errors / unrouted publish → intended honest signal (R-1).

### A2 — `b5-pin.sh` slot list updated; guard self-test re-synced (37→39, 17 executed)
*Preserves: "b5-pin.sh slot list updated with guard test test-b5-pin-guard.sh re-synced (37→39, 17 executed)."*
- `B5_CONTRACT_TEST_LIST` = exactly 39 slots: 17 executed (15 existing + `bus-priority-seam` + `bus-audit-events`) + 22 `[PROPOSED]`; no dupes, no malformed (enforced by `assert_b5_contract_pin` :84+).
- **Run**: `bash scripts/test-b5-pin-guard.sh` → exit 0 (39-slot positive + count-38/duplicate/malformed/vacuous negatives all green). Fresh-mode `assert_b5_contract_pin "$B5_LOG"` fails if either new slot lacks a `B5-CHECK` line; deleting either verdict line from the B5 log → pin FAIL.

### A3 — Isolation test is the bus-level A4 leg ('moderation 先达 sink' under `im.room.*` backlog)
*Preserves: "Moderation priority: isolation test is the bus-level A4 leg ('moderation 先达 sink' under im.room.* backlog)."*
- T5 exists under the `live_nats_audit_priority` filter and asserts delivery on the priority lane while the IM-lane durable's `num_pending > 0` — the live-NATS half of b5-3 spec A4 (claim-level half = `moderation-priority-drill` :459).
- **Run**: fresh-mode harness with NATS up and seams landed → B5 log contains `B5-CHECK bus-priority-seam: PASS` **and** `B5-CHECK moderation-priority-drill: PASS`; G6 closure (`assert_b5_contract_pin`, :593-595) passes on the 39-slot manifest.

### A4 — No audit-lane publish path reaches the sink without the connector's claim/provision gates (A5 constraint test)
*Preserves: "T-11: suite asserts no audit-lane publish path exists that reaches the sink without the connector's claim/provision gates (A5 constraint test)."*
- **Grep guard** (inside the legs block, mechanically attached to both legs' verdicts): `rg -n 'reqwest|hyper' crates/aero-bus/Cargo.toml` → 0 hits; `rg -n 'aero-audit-connector' crates/aero-bus/` → 0 hits (verified today, E14). A violation → `B5-CHECK <leg>: FAIL (audit-lane sink path)` + `exit 1`.
- **Semantic constraint**: the suite exercises only the idempotent publish seam and durable pull consumers on the audit namespaces; aero-bus is transport-only (no sink code, no connector dependency). The sink remains reachable solely via the connector's claim (`pg.rs:117`) / provisioning (B5-4) gates; the `t11-fail-closed` drill (:436) and its 0236-RAISE / 403→dead gates are unchanged.

## 6. Test placement

| Item | Location | Harness |
|---|---|---|
| T1–T5 (`live_nats_audit_priority_*`) | `crates/aero-bus/src/jetstream.rs` tests module, after `live_nats_deduplicates_same_message_id` (:505), before `pull_config_bounds_redelivery_for_poison_messages` (:507) | `#[ignore]` live NATS at `AERO__NATS__URL`; pattern E1 (nonce subjects, purge + durable-delete cleanup) |
| T6–T7 (`live_nats_audit_events_*`) | same module | `#[ignore]`; seam-level assertions only (declaration/dedup) — resume/`DeliverPolicy::All` is B5-1 unit-owned (C2) |
| `bus-priority-seam` / `bus-audit-events` legs + A5 grep guard | `scripts/test-integration.sh`, after relay-mock-probe block (:549), before Relay coverage (AC4) (:580) | `run_bus_leg`; `/dev/tcp` probe; `b5_check` verdicts; FAIL → `exit 1`; SKIP (NATS unreachable) clean |
| Pin + guard literals | `scripts/b5-pin.sh` (list :29-68, `-ne 37` :87, echo :130) + `scripts/test-b5-pin-guard.sh` (:64, :80, :88, :96, :113) | `bash scripts/test-b5-pin-guard.sh` → exit 0; fresh-mode pin closure :593-595 |

Filter names are normative: `bus-priority-seam` → `live_nats_audit_priority`; `bus-audit-events` → `live_nats_audit_events`. Legs use `-p aero-bus --lib --locked` + `--test-threads=1` (consistent with Step 4; serialization also protects T4's docker restart).

## 7. Risks / notes

- **R-1 — Red-until-seams-land is the intended signal**: T1–T7 fail whenever NATS is reachable until B5-3 (`AUDIT_PRIORITY`) / B5-1 (`AUDIT_EVENTS`) land (E3, C2). Land in the same wave as the seams, or accept the red G6 as the honest signal — a silent-green bus is the failure mode this direction eliminates. CI is unaffected in the interim only in that CI runs the harness and will go red; this matches the existing moderation-priority drill's contract.
- **R-2 — Lockstep literals**: b5-pin.sh and test-b5-pin-guard.sh must change in one commit (guard self-test :149 fails fast on mismatch — FM7 of the design pin).
- **R-3 — T4 docker coupling**: `docker restart aero-nats` requires docker + a container literally named `aero-nats` — both satisfied in CI (ci.yml:85) and docker-compose (docker-compose.yml:45). A local bare `nats-server` (no docker) makes T4 fail with a clear precondition message — acceptable for an opt-in live test; the acceptance's "not SKIP" property is preserved.
- **R-4 — Step 4 interaction**: the blanket `--ignored` run (:505-517) executes the new tests too (and would fail the harness if NATS is down — pre-existing hard NATS dependency, design pin FM1). The legs add named verdicts; failures surface in Step 4 first.
- **R-5 — Test hygiene**: nonce'd subjects/durables prevent cross-run collisions; pre/post best-effort delete + purge keep the shared dev/CI NATS clean (T3/T4/T5 create durables; T4 restarts the server — `--test-threads=1` is mandatory).
- **R-6 — Lints**: workspace clippy `all`+`pedantic` are `warn` (AGENTS.md §4.2) — the tests must introduce no new warnings (`cargo clippy --workspace --all-targets`); follow the existing tests' style (`expect` with message, no bare `unwrap`, no `format!` in assert messages).
- **R-7 — No Cargo.toml changes**: all APIs needed are already available (E4); `--locked` stays valid for Step 4 and the legs.

## 8. Sequencing

1. **This direction**: BR1 (7 tests) + BR2 (legs + A5 guard) + BR3 (pin 37→39, guard re-sync) — one change; `cargo check --workspace` clean; `cargo test -p aero-bus --lib --locked` green (new tests are `#[ignore]`); `bash scripts/test-b5-pin-guard.sh` exit 0; `cargo clippy --workspace --all-targets` no new warnings.
2. **B5-3 (aero-bus)** lands `AUDIT_PRIORITY` (+ mapping, its unit tests) → T1–T5 and `bus-priority-seam` green.
3. **B5-1 (aero-bus)** lands `AUDIT_EVENTS` (+ mapping, its unit tests incl. the durable-resume case, lib.rs doc) → T6–T7 and `bus-audit-events` green.
4. **B5-3 (aero-storage)** 0239 claim ordering → `moderation-priority-drill` green; **B5-2/B5-4** connector/provisioning → T-11 behavior unchanged.
5. **G6 closure** (fresh mode, NATS up, seams landed): B5 log contains `bus-priority-seam` / `bus-audit-events` / `t11-fail-closed` / `moderation-priority-drill` PASS lines; `assert_b5_contract_pin` prints `B5 contract pin: 39/39 (17 executed, 22 [PROPOSED]): PASS`.
