# Design — Live-NATS acceptance suite + harness legs for the audit lane (tests-only, reconciled)

- **Module**: `crates/aero-bus` (tests only — zero production API changes) + harness `scripts/`
- **Source**: `docs/requirements/2026-08-08-aero-bus-live-nats-acceptance-suite.req.md` (BR1–BR4 / A1–A4 / C1–C3 — normative)
- **Supersedes**: `docs/design/2026-08-07-aero-bus-live-nats-acceptance-suite.design.md` — this file reconciles that design with the 08-08 requirements: the **normative test list is C2's T1–T7** (restart test included, `duplicate_window == max_age` asserted in T1, `durable_resumes_all` removed), and **scope is trimmed to tests-only (C3**: no lib.rs doc change, no gate-doc G6 row — sibling-owned)
- **Campaign**: `aero-im-b5-outbox-relay`; gate **G6 (B5)** = "37/37、T-11、moderation 优先级" (`docs/campaigns/implementation-gate.md:78`; the "37/37" shorthand stays — this direction updates the pin, not the shorthand)
- **Sibling seam contracts asserted (dependency-owned, unlanded — the suite goes red until they land)**: `AUDIT_PRIORITY` (WorkQueue, `audit.priority.*`, 24h, `duplicate_window == max_age`) → `docs/requirements/2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md` (A4 bus half) + `docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md:92-100`; `AUDIT_EVENTS` (Limits, `audit.ws.*`, 7d, explicit `duplicate_window == max_age`) → `docs/requirements/2026-08-08-aero-bus-b5-1-audit-transport-seam.req.md` (BR1/BR2)
- **Verification date**: 2026-08-08 (line numbers are as-of anchors; the **file/symbol** is the stable grep anchor per AGENTS.md §0 — four stale anchors in the source req are corrected below, D1–D4)

## 0. Evidence verification (every cited claim re-checked against the tree)

### 0.1 Claim-by-claim verdicts

| # | Claim (from the supplied evidence) | Verdict | Proof (grep/read, this run) |
|---|---|---|---|
| V1 | `jetstream.rs:469` `live_nats_deduplicates_same_message_id`, IM_EVENTS-only, `#[ignore]` | ✅ exact | fn at `jetstream.rs:469`; subject `im.events.idempotent.{nonce}` (:482); `#[ignore = "requires a live NATS JetStream at AERO__NATS__URL"]` (:468); double `publish_idempotent_ack` → `!first.duplicate` / `retry.duplicate` / equal seq (:493-498); purge cleanup (:502-504) |
| V2 | `im_stream_duplicate_window_covers_outbox_retries` :461 asserts `duplicate_window == max_age` | ✅ exact | :461-465: `assert_eq!(config.duplicate_window, IM_MESSAGE_DUPLICATE_WINDOW); assert_eq!(config.duplicate_window, config.max_age);` (both 7d, :134-135) |
| V3 | `pull_config_bounds_redelivery_for_poison_messages` :507 | ⚠️ **D1** — fn is at **:509** (2-line drift; the 08-07 design doc had :509 correct, the 08-08 req E1 regressed it). Assertions as claimed: `max_deliver == POISON_MAX_DELIVER` (16, :102), `ack_wait > 0` (POISON_ACK_WAIT 120s, :114) | `grep -n "fn pull_config_bounds"` → 509 |
| V4 | `bootstrap()` no audit namespace — 4 streams; `subscribe()` 4-prefix chain, fail-closed; `rg audit.(priority\|ws).` = 0 hits | ✅ exact | `bootstrap()` :237-298 declares IM_MESSAGES / IM_EVENTS / AI_QUEUE (WorkQueue) / LIVE_EVENTS only; `subscribe()` :338-353 4-prefix `starts_with` chain → else `Err(BusError::Nats("unknown subject prefix: …"))`; `rg 'audit\.(priority\|ws)\.' crates/aero-bus` = 0 hits (exit 1) |
| V5 | `test-integration.sh` :17 knob; :489 export; :503-517 blanket run | ⚠️ **D2/D3** — :17 exact; `export AERO__NATS__URL` is at **:512** (claimed :489, 23-line drift); blanket `cargo test --workspace --lib --locked --` is at **:529-544** (claimed :503-517, 26-line drift). Root cause: the script gained +343 uncommitted lines (B5 closure scaffolding) between the 08-07 design (mtime 08-07 20:51) and the 08-08 req (mtime 08-08 06:38; script mtime 08-08 02:52) — the req copied the pre-expansion anchors. Content claims all true | grep :17, :512, :529-544 |
| V6 | B5 scaffolding: :147/:149, empty-filter guard :198-206, t11 :436, drill :441-461, probe leg :533-549, pin closure :593-595 | ⚠️ **D4** — :147 `source …/b5-pin.sh` ✅; :149 `bash …/test-b5-pin-guard.sh` ✅; empty-filter guard :196-206 (comment :196; claimed :198-206 — 2-line, trivial) ✅; t11 :436 ✅; drill :441-461 ✅ (PASS :459, SKIP :461); **probe leg is :553-574** (claimed :533-549 — 20-line drift); pin closure :590-595 (`assert_b5_contract_pin "$B5_LOG"` :593-595) ✅; `rg run_bus_leg\|bus-priority` = 0 hits (exit 1) — no legs exist ✅ | awk line dumps :505-560, :556-600 |
| V7 | `b5-pin.sh` 37 slots (15 executed + 22 `[PROPOSED]`), `-ne 37` :87; list :29-68 not :23-46 | ✅ exact | `B5_CONTRACT_TEST_LIST=(` :29, list :29-68 (15 named :30-44 + 22 `[PROPOSED]` :46-67, `)` :68); `b5_check` :73-78; `assert_b5_contract_pin` :84+; `-ne 37` :87; final echo "37/37 (…): PASS" :130 |
| V8 | `test-b5-pin-guard.sh` literals :64/:113 `37/37 (15 executed…)`, `[36]` :80/:88, vacuous 37 :96 | ✅ exact | `run_guard "list-is-exactly-37-slots" "B5 contract pin: 37/37 (15 executed, 22 \[PROPOSED\]): PASS"` :64; `[36]` :80 (duplicate) / :88 (malformed); vacuous `seq -w 1 37` :93-97 (:96 is the list-append line); :113 same literal in SKIP_DB_CREATE case |
| V9 | CI `nats:2.10-alpine -js` container **named `aero-nats`** + readiness probe (ci.yml:85-93), `AERO__NATS__URL` :161; docker-compose.yml:43-56 same container name | ✅ exact | ci.yml:85 `docker run --detach --name aero-nats --publish 4222:4222 nats:2.10-alpine -js`; :87-91 `/dev/tcp/127.0.0.1/4222` probe loop (30×1s); :92 `docker logs`; :93 `exit 1`; :161 env. docker-compose.yml:48-53 `image: nats:2.10-alpine` + `container_name: aero-nats` + `command: ["-js", "-sd", "/data", "-m", "8222"]` (the service block spans :44-57; claimed :43-56 ≈). The restart test's `docker restart aero-nats` target is valid in **both** environments |
| V10 | async_nats 0.36 API surface; `futures` already a dep → zero Cargo.toml edits, `--locked` valid | ✅ exact | Vendored `async-nats-0.36.0`: `Stream::info(&mut self) -> Result<&Info, InfoError>` (stream.rs:148 — the existing test's `let mut im_stream` + `.info()` at jetstream.rs:477-478 proves the `&mut` re-fetch contract), `consumer_info` (used jetstream.rs:215), `delete_consumer(&self, &str)` (stream.rs:930), `purge().filter(subject)` (stream.rs:1599, used :504), `PublishAck { stream, sequence: u64, duplicate: bool }` (publish.rs:19-30), `StreamInfo.state.messages: u64` (stream.rs:1278), `consumer::Info.num_pending: u64` (consumer/mod.rs:158), `RetentionPolicy` (stream.rs:1220) and `DeliverPolicy` (consumer/mod.rs:362) both derive `PartialEq, Eq`. `futures.workspace = true` (aero-bus/Cargo.toml:17) → no Cargo.toml change |
| V11 | b5-1 design §0.1 L2 restart-survival fact base + 2m unset-window trap | ✅ exact | `docs/design/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.design.md` :43 (restart survival, `rebuildDedupe` `server/stream.go:993`, `GetSeqFromTime(now - Duplicates)`, "with `retention == window` the rebuild is **complete**"); :34/:37 L1 (unset window → `min(2m default, server limit, max_age)`; IM_EVENTS live-verified carrying the 2m default — the trap) |
| V12 | `bootstrap()` update path applies config to an existing stream | ✅ exact | `bootstrap()` :242-248: `get_or_create_stream` then `update_stream` with the same config — "applying the desired config so upgrading an existing cluster receives the longer duplicate window too" — this is what T1's "must update an existing stream, not only fresh installs" assertion (existing :481 comment) relies on |

### 0.2 Corrections to the supplied evidence (beyond the req's own C1–C3)

- **D1** — `pull_config_bounds_redelivery_for_poison_messages` is at `jetstream.rs:509`, not :507. The "line-exact" claim in the evidence summary is off by 2 here (the 08-07 design doc had it right).
- **D2/D3/D4** — three `test-integration.sh` anchors are **stale by 20-26 lines** (copied from the pre-+343-line state): export `:489`→`:512`, blanket run `:503-517`→`:529-544`, probe leg `:533-549`→`:553-574`. All cited *symbols* exist and behave as claimed; only the anchors drifted. Insertion points below use the verified anchors.
- **C1–C3 from the req are confirmed** against the tree: `crates/aero-bus/tests/` does not exist (placement stays in the `jetstream.rs` tests module); the normative test list is T1–T7 as enumerated in §2 (the 08-07 design's T-list lacked the restart test and omitted the window assertion; its `durable_resumes_all` is B5-1 unit-owned — `2026-08-08-aero-bus-b5-1-audit-transport-seam.req.md` A-check "Unit: poison_safe_pull_config …"); BR5/BR6 (gate-doc G6 row, lib.rs doc) are out of scope (C3).

## 1. API changes

**Zero production API changes. Zero Cargo.toml edits** (so the harness's `--locked` invocations stay valid). All test code reuses existing private items via descendant-module access (`mod tests` inside `jetstream.rs` already calls `bus.publish_idempotent_ack`, `poison_safe_pull_config`, `bus.js`).

| File | Change | Kind |
|---|---|---|
| `crates/aero-bus/src/jetstream.rs` | 7 new `#[tokio::test] #[ignore]` live-NATS tests + 1 poll helper `wait_for_state`, inserted in the tests module (:420) **after** the end of `live_nats_deduplicates_same_message_id` (:505) and **before** `#[test]` `pull_config_bounds_redelivery_for_poison_messages` (:507-509). Module-local imports: `use futures::StreamExt;` (for `msgs.next()`) and `use async_nats::jetstream::stream;` (for `stream::RetentionPolicy`) — both available with existing deps | tests only |
| `scripts/test-integration.sh` | New `run_bus_leg <name> <cargo-filter>` helper + 2 invocations + the A5 transport-only grep guard (BR4), inserted **after** the relay-mock-probe block ends (:574) and **before** the Relay coverage (AC4) block (:576). The existing `assert_b5_contract_pin "$B5_LOG"` closure (:593-595) then mechanically requires the new verdict lines once the manifest lists them (BR3) | harness |
| `scripts/b5-pin.sh` | `B5_CONTRACT_TEST_LIST` gains exactly 2 **executed** slots `bus-priority-seam` + `bus-audit-events` (after `audit-provision-check`, :44); count literal `-ne 37`→`-ne 39` (:87); header comments (:1-28) and final echo (:130) → 39/39 (17 executed) | harness |
| `scripts/test-b5-pin-guard.sh` | Self-test literals re-synced in the **same change**: both `"B5 contract pin: 39/39 (17 executed, 22 \[PROPOSED\]): PASS"` (:64, :113); count-36 case → count-38; duplicate/malformed indices `[36]`→`[38]` (:80, :88); vacuous `seq -w 1 37`→`seq -w 1 39` (:96) | harness |
| ~~`crates/aero-bus/src/lib.rs`~~ | **Out of scope (C3)** — BR6 lib.rs doc rewrite is sibling-owned (B5-1's 08-08 req E11/BR6 writes the 5-stream doc when `AUDIT_EVENTS` lands) | — |
| ~~`docs/campaigns/implementation-gate.md:78`~~ | **Out of scope (C3)** — the "37/37" shorthand stays; only the mechanical pin changes | — |

## 2. Test design (`jetstream.rs` tests module) — normative list T1–T7 (req C2)

Common template (mirrors the existing live test at :469): `let url = std::env::var("AERO__NATS__URL").expect("AERO__NATS__URL")` → `JetStreamBus::connect(JetStreamConfig { url, bootstrap_streams: true }).await.expect(…)` → nonce = `SystemTime::now().as_nanos()` → nonce'd subjects/durables → best-effort cleanup (pre-delete stale durables via `delete_consumer`, purge subjects via `purge().filter(subject)`, post-delete durables). Follow the existing tests' style (`expect` with message, no bare `unwrap`, no `format!` in assert messages — clippy `pedantic` is `warn`).

Priority seam — filter `live_nats_audit_priority` (leg `bus-priority-seam`):

- **T1 `live_nats_audit_priority_declared_with_workqueue_retention`**: `get_stream("AUDIT_PRIORITY")` succeeds post-bootstrap; `info().config.retention == stream::RetentionPolicy::WorkQueue`; `info().config.subjects == vec!["audit.priority.*".to_string()]`; **`info().config.duplicate_window == info().config.max_age`** (the seam contract pins both = 24h; the equality assertion is the C2 correction — it guards the unset-window 2m trap, V11). `bootstrap()`'s get-or-create-then-`update_stream` path (:242-248) makes this hold on existing clusters too.
- **T2 `live_nats_audit_priority_deduplicates_same_message_id`**: `publish_idempotent_ack("audit.priority.{nonce}", payload, "aero-bus-test-{nonce}")` twice → `!first.duplicate`, `retry.duplicate`, `retry.sequence == first.sequence`; purge cleanup.
- **T3 `live_nats_audit_priority_ack_consumes_message`** (WorkQueue retention semantics — the `AI_QUEUE` precedent at :256-265): publish 1 → `info().state.messages == 1` (unacked WorkQueue message retained); durable consumer via `poison_safe_pull_config(subject, Some(&durable))` (`DeliverPolicy::All` ⇒ the pre-created message is delivered) → `tokio::time::timeout(5s, msgs.next())` → `msg.ack()` → short poll (`wait_for_state`, 3 × 100ms, re-fetching `info()` — `info(&mut self)` re-queries, V10) until `state.messages == 0` — **WorkQueue removes on ack**; cleanup: delete consumer + purge.
- **T4 `live_nats_audit_priority_dedup_survives_nats_restart`** (restart survival — the C2 addition; the 08-07 design had no restart test): publish id M on `audit.priority.{nonce}` → assert `!first.duplicate` → `std::process::Command::new("docker").args(["restart", "aero-nats"]).output()` — **assert success with a precondition message** ("docker + a container literally named `aero-nats` are environment preconditions of this opt-in live test, not skip conditions"; the target name is the CI/compose contract, V9) → reconnect with bounded backoff (fresh `JetStreamBus`, ≤30 × 1s attempts on `connect` failure — nats-server 2.10 with a data dir restarts in ~1-3s) → re-publish id M → `retry.duplicate && retry.sequence == first.sequence` (server-side `rebuildDedupe` re-indexes in-window `Nats-Msg-Id` headers after restart; `duplicate_window == max_age` makes the rebuild complete, V11) → purge cleanup. **`--test-threads=1` is mandatory** (harness Step 4 and the legs both enforce it) so no other test is mid-flight during the restart. The reconnect's `bootstrap_streams: true` re-declares streams idempotently — harmless.
- **T5 `live_nats_im_backlog_does_not_block_audit_priority`** (cross-stream isolation — the bus-level A4 leg, req A3): publish `im.room.{nonce}`; durable `aero-bus-test-im-{nonce}` on `im.room.{nonce}` created and **left unfetched** → `consumer_info(…).num_pending > 0` (assert `> 0`, not `== 1`, for margin — pending is server-side synchronous at creation); publish `audit.priority.{nonce}`; durable `aero-bus-test-prio-{nonce}` → `timeout(5s, msgs.next())` must yield it (independent streams/cursors ⇒ no cross-stream HOL); ack; cleanup: delete both durables + purge both subjects.

Audit-events — filter `live_nats_audit_events` (leg `bus-audit-events`):

- **T6 `live_nats_audit_events_declared_post_bootstrap`**: `get_stream("AUDIT_EVENTS")` → `retention == stream::RetentionPolicy::Limits`, `subjects == vec!["audit.ws.*".to_string()]`.
- **T7 `live_nats_audit_events_deduplicates_same_message_id`**: double `publish_idempotent_ack("audit.ws.{nonce}", …)` → `!first.duplicate` / `retry.duplicate` / identical `sequence`; purge cleanup.

**Not in this list (C2)**: `live_nats_audit_events_durable_resumes_all` (the 08-07 design's third BR3 test) is **removed** — its assertion (`poison_safe_pull_config("audit.ws.<uuid>", Some("aero-audit-connector"))` → `DeliverPolicy::All`, pre-created message delivered) is B5-1's own unit test per `2026-08-08-aero-bus-b5-1-audit-transport-seam.req.md`, and B5-1 additionally owns its live tests (`live_nats_audit_stream_declared_and_deduplicates`, `live_nats_audit_expansion_publishes_exactly_n_frames`). No double-ownership; count stays 7.

## 3. Harness leg design (`scripts/test-integration.sh`)

Insert after the relay-mock-probe block (`fi` at :574) and before the Relay coverage (AC4) block (:576). The relay-coverage AC4 logic is untouched (bus legs are not relay-execution legs).

```bash
# ---- B5 bus legs (live-NATS acceptance suite, aero-bus) ----
# run_bus_leg <name> <cargo-filter> — DB-free live-NATS verdict leg (B5 G6).
# Reachable  → filtered `cargo test -p aero-bus --lib --locked <filter> -- --ignored
#               --test-threads=1`; empty-filter guard; FAIL verdict + exit 1 on failure.
# Unreachable → `B5-CHECK <name>: FAIL (NATS unreachable)` + exit 1 — never SKIP:
#               Step 4 already hard-fails without NATS, so a SKIP branch would
#               have no legitimate reachable state (it would only mask a
#               Step-4→legs flap as a silent pass).
run_bus_leg() {
    local name="$1" filter="$2"
    local root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    # A5 transport-only guard (T-11) — FIRST, before any NATS reachability
    # gating: it is pure filesystem work and must run even when NATS is down.
    # Missing `rg` is fail-closed (truth-check-lib.sh:183 precedent).
    if ! command -v rg >/dev/null 2>&1; then
        echo "✗ ${name}: rg unavailable — A5 transport-only guard cannot run (fail-closed)" >&2
        b5_check "$name" "FAIL (rg unavailable)"
        exit 1
    fi
    if rg -q 'reqwest|hyper' "$root/crates/aero-bus/Cargo.toml" || rg -q 'aero-audit-connector' "$root/crates/aero-bus/"; then
        echo "✗ ${name}: audit-lane sink path — aero-bus must not depend on reqwest/hyper or aero-audit-connector" >&2
        b5_check "$name" "FAIL (audit-lane sink path)"
        exit 1
    fi
    # Parse AERO__NATS__URL defensively. Scheme, userinfo and path are stripped
    # first (userinfo is never probed, never echoed); host/port are validated
    # against a strict character class BEFORE they reach any shell string —
    # the probe is `bash -c "exec 3<>/dev/tcp/${host}/${port}"`, and an
    # unvalidated value is a command-injection vector (verified live:
    # `nats://127.0.0.1:5999$(touch …)` executes at word-expansion even with
    # the port closed — the round-trip check alone does NOT catch `;`/`$()`).
    local nats_url="${AERO__NATS__URL:-nats://localhost:4222}"
    local host_port host port
    host_port="${nats_url#nats://}"
    host_port="${host_port%%/*}"
    host_port="${host_port##*@}"
    host="${host_port%%:*}" port="${host_port##*:}"
    if [[ ! "$host" =~ ^[A-Za-z0-9.-]+$ ]] || [[ ! "$port" =~ ^[0-9]{1,5}$ ]] \
            || [ "$port" -lt 1 ] || [ "$port" -gt 65535 ]; then
        echo "✗ ${name}: unparseable AERO__NATS__URL (expected nats://host:port; got host='${host}' port='${port}')" >&2
        b5_check "$name" "FAIL (unparseable AERO__NATS__URL)"
        exit 1
    fi
    if command -v timeout >/dev/null 2>&1; then
        timeout 5 bash -c "exec 3<>/dev/tcp/${host}/${port}" 2>/dev/null
    else
        bash -c "exec 3<>/dev/tcp/${host}/${port}" 2>/dev/null
    fi || {
        echo "✗ ${name}: NATS unreachable at ${host}:${port}" >&2
        b5_check "$name" "FAIL (NATS unreachable)"
        exit 1
    }
    local out=""
    if ! out="$(AERO__NATS__URL="$nats_url" cargo test -p aero-bus --lib --locked \
            "$filter" -- --ignored --test-threads=1 2>&1)"; then
        echo "✗ ${name} FAILED (cargo test)" >&2
        echo "$out" >&2
        b5_check "$name" "FAIL (cargo test)"
        exit 1
    fi
    if ! grep -Eq 'test result: ok\. [1-9][0-9]* passed' <<<"$out"; then
        echo "✗ ${name}: no test matched (empty-filter guard) — filter '${filter}' must match ≥1 test" >&2
        echo "$out" >&2
        b5_check "$name" "FAIL (empty filter)"
        exit 1
    fi
    b5_check "$name" "PASS"
}

run_bus_leg "bus-priority-seam" "live_nats_audit_priority"   # T1-T5 (§2)
run_bus_leg "bus-audit-events" "live_nats_audit_events"      # T6-T7 (§2)
```

Contract: every non-PASS path is a FAIL with `exit 1`. Unreachable/unparseable (`AERO__NATS__URL` invalid or probe failure) → `FAIL (NATS unreachable)`/`FAIL (unparseable AERO__NATS__URL)` — **never SKIP**: Step 4's blanket run already hard-fails without NATS (pre-existing dependency), so a SKIP branch would have no legitimate reachable state and would be a silent-pass vector in the Step-4→legs flap window. Missing `rg` → `FAIL (rg unavailable)` (fail-closed, truth-check precedent :183). Reachable → run the filter; pass → `PASS`; any cargo failure → stderr detail + `FAIL (cargo test)` + `exit 1`; a filter matching 0 tests → `FAIL (empty filter)` + `exit 1` (mirrors the existing guard at :196-206 — a named leg can never be vacuously green). The **A5 grep guard** runs first in the leg, before URL parsing/probe — pure filesystem work must not be gated on NATS reachability; both `rg` targets are 0 hits today (verified this run).

**Interaction with Step 4 (:529-544, corrected anchor)**: the blanket `--ignored` run already executes the new tests when NATS is reachable and exits the harness on any failure (:549). Consequences, by design: NATS up + seams landed → Step 4 green, legs PASS (attributable, pin-enforced); NATS up + seams missing → Step 4 fails first with the new tests (red-until-seams-land); NATS down → Step 4 already fails today on the existing live test (pre-existing hard NATS dependency), and the legs then also FAIL (`NATS unreachable`) — the only hypothetical SKIP state (NATS up for Step 4, down seconds later for the legs) would be a silent-pass hole, so unreachable is a FAIL.

## 4. Compatibility constraints

- **No dependency changes** → `--locked` (used by Step 4 and the legs) stays valid. Only async_nats 0.36 APIs verified in the vendored source are used (V10): `info(&mut self)` (re-fetches), `consumer_info`, `delete_consumer(&self, &str)`, `purge().filter(subject)`, `PublishAck { sequence, duplicate }`, `State.messages: u64`, `ConsumerInfo.num_pending: u64`, `RetentionPolicy`/`DeliverPolicy` (`PartialEq, Eq` both).
- **NATS server**: `nats:2.10-alpine` (docker-compose :48, CI :85) — no server features beyond stock JetStream (WorkQueue, `Nats-Msg-Id` dedup, pull consumers with `deliver_policy`); no server-config changes. Dedup equality `duplicate_window == max_age` is within the 2.10 allowed boundary (b5-1 §0.1 L1: equality allowed, `>` rejected err 10052).
- **Determinism across runs**: nonce'd subjects + nonce'd durable names (T2-T5/T7) make collisions impossible; pre/post best-effort `delete_consumer` + subject purge keep the shared dev/CI NATS clean even after crashed runs. **T4 is the sole exception by design**: it restarts the server — `--test-threads=1` (Step 4 :530 and the legs) is the serialization contract.
- **T4 environment preconditions** (docker + container literally named `aero-nats`) are satisfied in CI (ci.yml:85) and docker-compose (docker-compose.yml:49). They are **hard preconditions, not skip conditions** (req R-3): a bare `nats-server` without docker makes T4 fail with a clear precondition message — accepted for an opt-in live test; the acceptance's "not SKIP" property is preserved. Operationally, point the harness at a **dedicated NATS container**: `docker restart aero-nats` force-disconnects every connected client, including a live `aero-server` on the same compose stack.
- **Lockstep literals**: `b5-pin.sh` and `test-b5-pin-guard.sh` must change in **one commit** — the guard self-test (:149) runs before any DB work and fails fast on mismatch.
- **Workspace lints**: `cargo clippy --workspace --all-targets` must add no warnings (AGENTS.md §4.2; `unsafe_code = "forbid"` — the docker `Command` in T4 is safe Rust, no `unsafe`).
- **Scope boundary (C3)**: no lib.rs doc change, no `implementation-gate.md` change, no `crates/aero-bus/tests/` creation, no change to `live_nats_deduplicates_same_message_id`, `bootstrap()`, `subscribe()`, or the `EventBus` trait.

## 5. Failure modes

| # | Mode | Behavior | Handling / note |
|---|---|---|---|
| FM1 | NATS down/unreachable | Legs emit `FAIL (NATS unreachable)` + `exit 1`. Step 4's blanket run **also** fails on the existing live test — pre-existing hard NATS dependency; the leg FAIL is the flap-window backstop (Step 4 green → NATS down before the legs → the legs must not silently skip) | Fail-closed; CI always starts NATS (V9) |
| FM2 | Seams not landed (B5-1/B5-3) | T1-T7 fail (get_stream errors / publish to unmapped subject → `unknown subject prefix` fail-closed) → Step 4 red → harness red | **Intended** red-until-seams-land (req R-1); land in the same wave as the seams, or accept the red G6 as the honest signal |
| FM3 | Test renamed/removed → filter matches 0 tests | `cargo test` exits 0 with "0 passed" → empty-filter guard converts to `FAIL (empty filter)` + `exit 1` | Mirrors :196-206; a named leg can never be vacuously green |
| FM4 | Crashed run leaves stale durables / WorkQueue messages | Nonce'd names prevent cross-run interference; pre/post best-effort delete + purge bounds junk | Failed runs may leave ≤5 orphan durables on the dev NATS until reset — bounded, harmless, nonce'd (no name collision ever) |
| FM5 | T4 restart races other tests | `--test-threads=1` in Step 4 and the legs serializes; no other test is mid-flight during the restart | The legs run after Step 4 in the same serialized process; a parallel manual invocation is out of contract |
| FM6 | T4 docker missing / container not named `aero-nats` | `docker restart` command fails → test fails with the precondition message (R-3) — **not** a skip | CI and compose both satisfy it (V9); bare-server local setups get a loud, attributable failure |
| FM7 | T4 reconnect timeout | Bounded backoff ≤30 × 1s; a server that stays down past that fails the test (honest) | nats-server 2.10 with a data dir restarts in ~1-3s; 30s is generous |
| FM8 | Manifest count drifts (37 vs 39) | Guard self-test (:149) runs before any DB work and fails fast on literal mismatch | Single-change lockstep requirement (BR3) |
| FM9 | T5 flake (num_pending timing) | `num_pending` is server-side synchronous at consumer creation | Assert `> 0` (not `== 1`) for margin |
| FM10 | A5 guard false positive (future legit dependency) | Leg FAILs with a named verdict — forces a conscious scope decision | Guard targets `reqwest\|hyper` in Cargo.toml and `aero-audit-connector` in `aero-bus/`; both 0 hits today (V14/E14) |
| FM11 | Step 4 passes but a leg fails (or vice versa) | Same tests, same NATS, deterministic — the only divergence is the flap window (NATS up for Step 4, down before the legs), which now fails red (`FAIL (NATS unreachable)`) instead of skipping | The pin closure (:593-595) requires both legs' verdict lines in fresh mode; a FAIL line never matches the `PASS\|SKIP` grep, so it also breaks the pin |

## 6. Migration steps (no DB migration, no schema change)

1. **Single change, three files**: (a) `crates/aero-bus/src/jetstream.rs` — T1-T7 + `wait_for_state` + imports (§2); (b) `scripts/test-integration.sh` — `run_bus_leg` + 2 invocations + A5 guard, inserted after :574 / before :576 (§3); (c) `scripts/b5-pin.sh` + `scripts/test-b5-pin-guard.sh` — 37→39 pin + literal re-sync, lockstep in the same commit (BR3).
2. **Compile + lint**: `cargo check --workspace` clean; `cargo clippy --workspace --all-targets` no new warnings; `cargo test -p aero-bus --lib --locked` green (new tests are `#[ignore]` — the normal suite is untouched).
3. **Guard self-test**: `bash scripts/test-b5-pin-guard.sh` → exit 0 (39-slot positive + count-38/duplicate/malformed/vacuous negatives all green).
4. **Live run (seams landed)**: with `docker compose up` NATS (container `aero-nats`) + throwaway PG: `AERO__NATS__URL=nats://localhost:4222 scripts/test-integration.sh` (fresh mode) → B5 log contains `B5-CHECK bus-priority-seam: PASS`, `B5-CHECK bus-audit-events: PASS`, plus t11 / moderation-priority / the 15 existing executed slots; `assert_b5_contract_pin` prints `B5 contract pin: 39/39 (17 executed, 22 [PROPOSED]): PASS`.
5. **Interim state (seams not yet landed)**: harness is red when NATS is reachable (FM2) — accepted per R-1; land in the same wave as B5-1/B5-3, or the red G6 is the honest signal.
6. **Sibling sequencing** (no rework of this direction's tests — they assert exactly the seam contracts the siblings pin): B5-3 (aero-bus) lands `AUDIT_PRIORITY` + mapping + `AUDIT_PRIORITY_SUBJECT_PREFIX` + its unit tests → T1-T5 / `bus-priority-seam` green; B5-1 (aero-bus) lands `AUDIT_EVENTS` + mapping + `AUDIT_SUBJECT_PREFIX` + its unit tests (incl. the `durable_resumes_all` case now owned there) + lib.rs doc → T6-T7 / `bus-audit-events` green; B5-3 (aero-storage) lands 0239 claim ordering → `moderation-priority-drill` green; B5-2/B5-4 connector/provisioning → T-11 behavior unchanged.
7. **CI**: `.github/workflows/ci.yml` already provisions NATS (V9) — no CI change needed; the legs and Step 4 run live there, not SKIP. The `docker restart aero-nats` in T4 targets the very container the CI job created at :85.

## 7. Testable acceptance mapping (req §5 A1–A4 ↔ concrete checks)

| Acceptance | Concrete check | Command / probe |
|---|---|---|
| **A1** — 7 `#[ignore]` live tests, green under the harness `--ignored` run with NATS up (CI leg, not SKIP) | `AERO__NATS__URL=nats://localhost:4222 cargo test -p aero-bus --lib --locked -- --ignored --test-threads=1` → all 7 pass (seams landed); red until B5-3/B5-1 land (get_stream errors / fail-closed publish — the honest signal). Assert 1 (T1: WorkQueue + `audit.priority.*` + `duplicate_window == max_age`), Assert 2 (T2: duplicate + same seq), Assert 3 (T3: `state.messages` 1→0 on ack), Assert 4 (T4: dedup survives `docker restart aero-nats` — duplicate + identical seq), Assert 5 (T5: `im.room.*` pending `> 0` while priority lane delivers ≤5s), Assert 6 (T6/T7: AUDIT_EVENTS Limits/`audit.ws.*` + dedup) | cargo filter `live_nats_audit_priority` (5) + `live_nats_audit_events` (2); CI integration job (ci.yml:85-93/:161) runs them live |
| **A2** — pin 37→39, 17 executed; guard re-synced | `bash scripts/test-b5-pin-guard.sh` → exit 0 (both `39/39 (17 executed, 22 \[PROPOSED\]): PASS` literals, count-38 negative, `[38]` indices, vacuous 39); `assert_b5_contract_pin` fails on a 38-slot list (`B5 contract pin: 39/38`), on a missing `B5-CHECK bus-priority-seam`/`bus-audit-events` verdict line, and on a vacuous list; `grep -c '\[PROPOSED\]' scripts/b5-pin.sh` → 22 | guard self-test + fresh-mode harness |
| **A3** — T5 is the bus-level A4 leg ('moderation 先达 sink' under `im.room.*` backlog) | Fresh-mode harness with NATS up and seams landed → B5 log contains `B5-CHECK bus-priority-seam: PASS` **and** `B5-CHECK moderation-priority-drill: PASS`; the claim-level half of A4 is B5-3 (aero-storage)'s 0239 drill (:441-461), untouched here | `grep '^B5-CHECK'` on the B5 log; pin closure :593-595 passes on the 39-slot manifest |
| **A4** — no audit-lane publish path reaches the sink without the connector's claim/provision gates (A5/T-11 constraint) | Grep guard inside the legs block: `rg 'reqwest\|hyper' crates/aero-bus/Cargo.toml` → 0 hits and `rg 'aero-audit-connector' crates/aero-bus/` → 0 hits (both verified 0 today); a violation → `B5-CHECK <leg>: FAIL (audit-lane sink path)` + `exit 1`. Semantically: the suite exercises only the idempotent `Nats-Msg-Id` publish seam and `poison_safe_pull_config` durables; the sink is reachable only via the connector's claim/provision gates; the `t11-fail-closed` drill body (:425-436) is untouched | the two `rg` probes; the legs' verdicts |

Filter names are normative: `bus-priority-seam` → `live_nats_audit_priority`; `bus-audit-events` → `live_nats_audit_events`. Legs use `-p aero-bus --lib --locked` + `--test-threads=1` (consistent with Step 4 :529-530; serialization also protects T4's restart).

## 8. Out of scope / dependency-owned (do not build here)

- `AUDIT_PRIORITY` stream + `audit.priority.*` mapping + `stream_for_subject` extraction + `AUDIT_PRIORITY_SUBJECT_PREFIX` → sibling **B5-3 (aero-bus)**; the unit tests for those pure functions are theirs.
- `AUDIT_EVENTS` stream + `audit.ws.*` mapping + `AUDIT_SUBJECT_PREFIX` + the `durable_resumes_all` assertion (now B5-1 unit-owned, C2) + B5-1's own live tests → sibling **B5-1 (aero-bus)**.
- lib.rs stream doc (BR6) and the gate-doc G6 row (BR5) → sibling seam directions (C3).
- T-11 fail-closed drill (:425-436) and moderation-priority drill (:441-461) bodies → unchanged; only *declared together* in the G6 closure.
- `crates/aero-bus/tests/` stays absent (C1); `live_nats_deduplicates_same_message_id`, `bootstrap()`, `subscribe()`, `EventBus` trait, and all production code in `crates/aero-bus` unchanged.
