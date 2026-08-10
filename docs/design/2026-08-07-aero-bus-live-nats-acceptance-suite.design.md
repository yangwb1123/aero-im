# Design — Live-NATS acceptance suite + G6 harness legs for the aero-bus seams, and lib.rs stream doc fix (BR6)

- **Module**: `crates/aero-bus` (tests only — zero production code) + harness `scripts/` + gate `docs/campaigns/implementation-gate.md`
- **Source**: `docs/requirements/2026-08-07-aero-bus-live-nats-acceptance-suite.req.md` (BR1–BR6 / A1–A5 / R-1..R-7)
- **Campaign**: `aero-im-b5-outbox-relay`; gate **G6 (B5)** = "37/37、T-11、moderation 优先级" (`docs/campaigns/implementation-gate.md:78`)
- **Verification date**: 2026-08-08 (line numbers are as-of anchors; the file/symbol is the stable grep anchor per AGENTS.md §0)

## 0. Evidence verification (the supplied spec's claims re-checked against the tree, 2026-08-08)

| # | Claim | Verdict | Evidence |
|---|---|---|---|
| E1 | Unit patterns `im_stream_duplicate_window_covers_outbox_retries` / `pull_config_bounds_redelivery_for_poison_messages` exist in `jetstream.rs` tests module, pure (no NATS) | ✅ | `jetstream.rs:461` / `:509` in `#[cfg(test)] mod tests` (:420). Both pure; assert `duplicate_window == max_age == 7d` and `max_deliver == 16` / `ack_wait == 120s`. |
| E2 | `live_nats_deduplicates_same_message_id` at :469, `#[ignore]`, `AERO__NATS__URL`-gated, IM_EVENTS-only | ✅ | `jetstream.rs:469-505`. Subject `im.events.idempotent.{nonce}`, double `publish_idempotent_ack` → `!first.duplicate` / `retry.duplicate` / equal seq, `stream.purge().filter(subject)` cleanup. No audit-namespace / WorkQueue / isolation coverage — gap confirmed. |
| E3 | `crates/aero-bus/tests/` empty | ✅ | Directory exists, zero entries. Placement stays in `jetstream.rs` tests module per acceptance. |
| E4 | `lib.rs:9-13` lists 3 streams; `bootstrap()` declares 4 — `LIVE_EVENTS` missing (BR6) | ✅ | `lib.rs` doc lists `IM_MESSAGES` / `IM_EVENTS` / `AI_QUEUE` only. `bootstrap()` (`jetstream.rs:237-298`) declares all 4 incl. `LIVE_EVENTS` (`live.stream.*`, Limits, 6h, 200k) at :284. |
| E5 | `test-integration.sh:17` knob; `:489` export; Step 4 (:503-517) already runs all `--ignored` with `AERO__NATS__URL` set | ✅ | :17 comment, :489 `export AERO__NATS__URL="${AERO__NATS__URL:-nats://localhost:4222}"`, :503-517 blanket `cargo test --workspace --lib --locked -- --ignored --test-threads=1` with skips; failure → `exit "$TEST_EXIT"` (:526). **R-4 correction confirmed**: the existing live test already runs in-harness whenever NATS is reachable — and **fails the harness when NATS is down** (pre-existing hard dependency). |
| E6 | `:147` source, `:149` guard self-test, `:166`/`:206` `b5_check` PASS | ✅ | All four exact. |
| E7 | `:436` t11, `:441-461` moderation-priority drill | ✅ | `b5_check "t11-fail-closed" "PASS"` :436; drill block :441-461, `b5_check "moderation-priority-drill" "PASS"` :459, `SKIP (0239 not landed)` :461. |
| E8 | `b5-pin.sh` 37 slots (15 executed + 22 `[PROPOSED]`); `b5_check` :73-78; `-ne 37` :87; final echo :130 | ✅ | `B5_CONTRACT_TEST_LIST` = 15 named + 22 `contract-test-NN[PROPOSED]` = 37. `b5_check` :73-78, `assert_b5_contract_pin` :84+, `-ne 37` :87, echo "37/37 (${executed}…): PASS" :130 (interpolated counts, hardcoded "37/37" prefix). |
| E9 | `test-b5-pin-guard.sh` hardcoded literals: "37/37 (15 executed, 22 \[PROPOSED\]): PASS" ×2, count-36 case, indices `[36]`, vacuous 37 | ✅ | `run_guard "list-is-exactly-37-slots" "B5 contract pin: 37/37 (15 executed, 22 \[PROPOSED\]): PASS"`; `check "skip-db-create-still-pins"` same string; `count-36-fails` builds 36 slots; duplicate/malformed use `B5_CONTRACT_TEST_LIST[36]`; vacuous `seq -w 1 37`. Post-change values are count-38 / index `[38]` (spec's BR4 wording describes the *target* state, not current — consistent). |
| E10 | `docker-compose.yml` NATS 2.10 `-js` on :4222/:8222 | ✅ | `:43-56`: `nats:2.10-alpine`, `command: ["-js", "-sd", "/data", "-m", "8222"]`, ports 4222/8222, healthcheck `:8222/healthz`. |
| E11 | `implementation-gate.md:78` G6 row | ✅ | `| G6（B5） | B5-1..4 | 37/37、T-11、moderation 优先级 |`. No bus legs. |
| E12 | `AUDIT_PRIORITY` / `AUDIT_EVENTS` don't exist in the tree; sibling specs own them | ✅ | `bootstrap()` declares only the 4 of E4; `subscribe()` maps 4 prefixes, unknown → fail-closed (`jetstream.rs:344-353`). Sibling specs: `2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md` (AUDIT_PRIORITY = WorkQueue, `audit.priority.*`, 24h, `duplicate_window == max_age`) and `2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.req.md` (AUDIT_EVENTS = Limits, `audit.ws.*`). Disjoint namespaces, order-independent. |
| E13 | `relay-mock-probe` leg FAIL+exit-1 / SKIP pattern (:533-548); empty-filter guard (:198-206) | ✅ | Both verified verbatim. |
| E14 | CI starts NATS | ✅ (supplementary) | `.github/workflows/ci.yml:85` `docker run --detach --name aero-nats --publish 4222:4222 nats:2.10-alpine -js` with a `/dev/tcp` readiness probe (:87); `AERO__NATS__URL: nats://localhost:4222` exported (:161). The new legs run **live** in CI, not SKIP. |

**Verdict**: every cited claim verified; zero fabrication. The one flagged correction (R-4) is accurate and material: Step 4's blanket `--ignored` run already executes the existing live test (and would fail the harness if NATS is down — a pre-existing hard NATS dependency, see §5 FM1). async_nats 0.36 API surface used by the new tests verified in the vendored source: `Stream::info(&mut self)` / `consumer_info(&self)` / `create_consumer` / `delete_consumer` / `purge().filter()`; `StreamInfo { config, state: { messages } }`; `consumer::Info { config, num_pending }`; `PublishAck { sequence, duplicate }`; `stream::Config.subjects: Vec<String>`; `RetentionPolicy` / `DeliverPolicy` both derive `PartialEq`.

## 1. API changes (per file)

**Zero production API changes.** No new public fn, no visibility changes, no `Cargo.toml` edits (so the harness's `--locked` invocations stay valid), no new dependencies. All test code reuses existing private items via descendant-module access (`mod tests` inside `jetstream.rs` already calls `bus.publish_idempotent_ack`, `poison_safe_pull_config`, `bus.js`).

| File | Change | Kind |
|---|---|---|
| `crates/aero-bus/src/jetstream.rs` | 7 new `#[tokio::test] #[ignore]` tests + 1 tiny poll helper in the tests module (after the existing live test, before `pull_config_bounds_redelivery_for_poison_messages` at :507) | tests only |
| `crates/aero-bus/src/lib.rs` | Doc comment: add `LIVE_EVENTS` line (BR6); list = 4 streams | doc only |
| `scripts/test-integration.sh` | New `run_bus_leg` helper + 2 invocations (`bus-priority-seam`, `bus-audit-events`) in the B5 closure section (between relay-mock-probe leg end ~:550 and the relay-coverage count ~:555) | harness |
| `scripts/b5-pin.sh` | `B5_CONTRACT_TEST_LIST` +2 executed slots; count literal `-ne 37` → `-ne 39`; header comments → 39/39 (17 executed); final echo prefix → 39/39 | harness |
| `scripts/test-b5-pin-guard.sh` | Self-test literals (2× "39/39 (17 executed, 22 \[PROPOSED\]): PASS", count-38 case, indices `[38]`, vacuous 39) | harness |
| `docs/campaigns/implementation-gate.md:78` | G6 row names both bus legs | gate doc |

## 2. Test design (`jetstream.rs` tests module)

Common template (mirrors E2): `let url = std::env::var("AERO__NATS__URL").expect(...)` → `JetStreamBus::connect(JetStreamConfig { url, bootstrap_streams: true }).await.expect(...)` → nonce = `SystemTime::now().as_nanos()` → nonce'd subjects/durables → best-effort cleanup (pre-delete stale durables, purge subjects, post-delete durables). New module-local imports: `use futures::StreamExt;` and `use async_nats::jetstream::stream;` (for `stream::RetentionPolicy`).

### BR1 — priority-seam tests (RED until B5-3 lands)

- **T1 `live_nats_audit_priority_declared_with_workqueue_retention`**: `get_stream("AUDIT_PRIORITY")` → `info().config.retention == stream::RetentionPolicy::WorkQueue`; `info().config.subjects == vec!["audit.priority.*".to_string()]`. (*Deliberately does not assert `duplicate_window`/`max_age` — that equality is B5-3's own unit-test assertion, not double-owned here.*)
- **T2 `live_nats_audit_priority_deduplicates_same_message_id`**: `publish_idempotent_ack("audit.priority.{nonce}", payload, "aero-bus-test-{nonce}")` twice → `!first.duplicate`, `retry.duplicate`, `retry.sequence == first.sequence`; cleanup purge.
- **T3 `live_nats_audit_priority_ack_consumes_message`**: publish 1 → `info().state.messages == 1` (WorkQueue retains unacked); durable consumer `aero-bus-test-ack-{nonce}` via `poison_safe_pull_config(subject, Some(&durable))` (DeliverPolicy::All ⇒ pre-created message delivered) → `timeout(5s, msgs.next())` → `msg.ack()` → short poll (`3 × 100ms`) until `info().state.messages == 0` (WorkQueue removes on ack); cleanup delete consumer + purge. Poll helper: `async fn wait_for_state(stream: &mut stream::Stream, want: u64) -> bool` looping `info().await` (re-fetches server state each call — verified `info()` re-queries).
- **T4 `live_nats_im_backlog_does_not_block_audit_priority`**: publish `im.room.{nonce}`; durable `aero-bus-test-im-{nonce}` on `im.room.{nonce}` created and **left unfetched** → `consumer_info(...).num_pending > 0` (All-policy durable, message retained, cursor pending — asserted immediately, server-side synchronous); publish `audit.priority.{nonce}`; durable `aero-bus-test-prio-{nonce}` → `timeout(5s, msgs.next())` must yield the message (independent streams/cursors ⇒ no cross-stream HOL); ack; cleanup: delete both durables + purge both subjects.

### BR3 — audit-transport tests (RED until B5-1 lands)

- `live_nats_audit_events_declared_post_bootstrap`: `get_stream("AUDIT_EVENTS")` → `retention == Limits`, `subjects == vec!["audit.ws.*"]`.
- `live_nats_audit_events_deduplicates_same_message_id`: double `publish_idempotent_ack("audit.ws.{nonce}", …)` → duplicate + identical sequence; purge cleanup.
- `live_nats_audit_events_durable_resumes_all`: pre-clean (best-effort delete durable `aero-audit-connector` if present + purge subject) → publish `audit.ws.{nonce}` **before** consumer creation → create durable via `poison_safe_pull_config(subject, Some("aero-audit-connector"))` → assert `consumer_info("aero-audit-connector").config.deliver_policy == DeliverPolicy::All` (resume-from-first-creation semantics) → `timeout(5s, msgs.next())` receives the pre-creation message → ack → cleanup delete consumer + purge. The literal connector durable name is asserted per the B5-1 seam contract; pre/post delete makes repeated runs deterministic (an existing durable with a stale cursor would otherwise resume mid-stream). Safety note: only touches a dev/CI NATS instance — running the harness against the same NATS as a live connector is already out of contract (§5 FM6).

## 3. Harness leg design (`scripts/test-integration.sh`)

```bash
# run_bus_leg <name> <cargo-filter> — DB-free live-NATS verdict leg (B5 G6).
# Reachable  → filtered `cargo test -p aero-bus --lib --locked <filter> -- --ignored
#               --test-threads=1`; empty-filter guard; FAIL verdict + exit 1 on failure.
# Unreachable → `B5-CHECK <name>: SKIP (NATS unreachable)`; harness continues.
run_bus_leg() {
    local name="$1" filter="$2"
    local nats_url="${AERO__NATS__URL:-nats://localhost:4222}"
    local host_port="${nats_url#nats://}" host port
    host_port="${host_port%%/*}"
    host="${host_port%%:*}" port="${host_port##*:}"
    if [ -z "$host" ] || [ -z "$port" ] || [ "$host_port" != "${host}:${port}" ]; then
        b5_check "$name" "SKIP (NATS unreachable: unparseable AERO__NATS__URL '${nats_url}')"
        return 0
    fi
    if ! timeout 5 bash -c "exec 3<>/dev/tcp/${host}/${port}" 2>/dev/null; then
        b5_check "$name" "SKIP (NATS unreachable: ${host}:${port})"
        return 0
    fi
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

run_bus_leg "bus-priority-seam" "live_nats_audit_priority"   # T1-T4 (BR1)
run_bus_leg "bus-audit-events" "live_nats_audit_events"      # BR3
```

Placement: in the B5 closure area, after the relay-mock-probe block (`:550`) and before the relay-coverage count (`:555`) — the relay-coverage AC4 logic is untouched (bus legs are not relay-execution legs). Verdict lines land in `$B5_LOG` and are mechanically required by `assert_b5_contract_pin` once the manifest lists them (BR4) — no separate gate code.

**Interaction with Step 4 (verified, R-4)**: the blanket `--ignored` run at :503-517 already executes the new tests when NATS is reachable and exits the harness on any failure (:526). Consequences, by design:
- NATS up + seams landed → Step 4 green, legs PASS (attributable, pin-enforced).
- NATS up + seams missing → Step 4 fails first with the new tests (red-until-seams-land, R-1). The legs' FAIL branch is the same root cause surfacing as a named verdict if Step 4 is bypassed (e.g., `SKIP_DB_CREATE` still runs Step 4, but a future split or manual invocation reaches the legs directly).
- NATS down → Step 4 already fails today on the existing live test (pre-existing hard dependency). The legs' `SKIP (NATS unreachable)` branch is therefore a defensive backstop that keeps the leg honest if ever reached in isolation — it never turns a red harness green.

## 4. Compatibility constraints

- **No dependency changes** → `--locked` (used by both Step 4 and the legs) stays valid. Only async_nats 0.36 APIs already exercised or verified in the vendored source are used (§0 E-verification: `info(&mut self)` re-fetches, `consumer_info(&self)`, `delete_consumer(&self, &str)`, `purge().filter(subject)`, `State.messages`, `ConsumerInfo.config.deliver_policy`, `RetentionPolicy`/`DeliverPolicy` PartialEq, `Config.subjects: Vec<String>`).
- **NATS server**: `nats:2.10-alpine` (docker-compose :48, CI :85) — no server features beyond stock JetStream (WorkQueue, Nats-Msg-Id dedup, pull consumers with `deliver_policy`). No server-config changes.
- **Determinism across runs**: nonce'd subjects + nonce'd durable names (T3/T4) make collisions impossible; pre-delete + post-delete of the tests' own durables and purge of nonce subjects keep the shared dev/CI NATS clean even after crashed runs (E2's purge pattern extended to consumers). The one literal-name durable (`aero-audit-connector`, resume test) is pre/post-deleted best-effort.
- **Test isolation**: `--test-threads=1` everywhere (Step 4 and the legs) — consistent with the harness's existing serialization for stateful tests.
- **lib.rs doc invariant**: BR6 makes the doc match `bootstrap()` (4 streams) *today*; the invariant is maintained by the sibling directions' own BR6 edits (B5-1/B5-3 each extend the doc when their stream lands). This direction does not add a test for the invariant (A5's grep check is the manual guard, per R-7).
- **Workspace lints**: tests module must not introduce clippy warnings (`cargo clippy --workspace --all-targets`); follow the existing tests' style (no `unwrap` in live tests where `expect` with message is the pattern, no `format!` in assert messages, etc.).

## 5. Failure modes

| # | Mode | Behavior | Handling / note |
|---|---|---|---|
| FM1 | NATS down/unreachable | Legs emit `SKIP (NATS unreachable)` (defensive). Step 4's blanket run **fails first** on the existing live test — pre-existing hard NATS dependency, not introduced here | Documented; not changed. CI always starts NATS (E14) |
| FM2 | Seams not landed (B5-1/B5-3) | New tests fail (get_stream errors / publish to unmapped subject errors) → Step 4 red → harness red | **Intended** red-until-seams-land (R-1); land in the same wave as the seams |
| FM3 | Test renamed/removed → filter matches 0 tests | `cargo test` exits 0 with "0 passed" → empty-filter guard converts to `FAIL (empty filter)` + `exit 1` | Mirrors `:198-206`; a named leg can never be vacuously green |
| FM4 | Crashed run leaves stale durables / WorkQueue messages | Nonce'd names prevent cross-run interference; pre/post best-effort delete + purge bounds junk | Failed runs may leave ≤3 orphan durables on the dev NATS until reset — bounded, harmless, nonce'd (no name collision ever) |
| FM5 | A real connector (`aero-audit-connector` durable) present on the same NATS as the harness | Resume test pre-deletes the durable (best-effort) | Out of contract — harness NATS is the dev/CI instance; a shared instance is already broken by Step 4's bootstrap mutations |
| FM6 | IPv6 / credential-bearing `AERO__NATS__URL` | `host:port` parse fails → `SKIP (NATS unreachable: unparseable …)` | Defensive; local/CI config is `nats://localhost:4222` (hostname/IPv4). No `nats://user:pass@` form in the codebase |
| FM7 | Manifest count drifts (37 vs 39) | Guard self-test (`:149`) runs before any DB work and fails fast on literal mismatch | Single-change lockstep requirement (BR4) |
| FM8 | Step 4 passes but a leg fails (or vice versa) | Impossible in practice — same tests, same NATS, deterministic — but the legs still enforce attribution via verdict lines | The pin closure requires both legs' verdict lines in fresh mode |
| FM9 | T4 flake (num_pending timing) | `num_pending` is server-side synchronous at consumer creation | Assert `> 0` (not `== 1`) for margin |

## 6. Migration steps (rollout — no DB migration, no schema change)

1. **Single change, four files + one doc**: (a) `jetstream.rs` tests (BR1+BR3), (b) `test-integration.sh` legs (BR2), (c) `b5-pin.sh` + `test-b5-pin-guard.sh` (BR4, lockstep literals), (d) `lib.rs` doc (BR6), (e) `implementation-gate.md` G6 row (BR5).
2. **Compile + lint**: `cargo check --workspace` clean; `cargo clippy --workspace --all-targets` no new warnings; `cargo test --workspace --lib` green (new tests are `#[ignore]` — the normal suite is untouched); `cargo test -p aero-bus --lib --locked` green.
3. **Guard self-test**: `bash scripts/test-b5-pin-guard.sh` → exit 0 (both the 39-slot positive and the count-38/duplicate/malformed/vacuous negatives).
4. **Live run (seams landed)**: with `docker compose up` NATS + a throwaway PG: `AERO__NATS__URL=nats://localhost:4222 scripts/test-integration.sh` (fresh mode) → B5 log contains `B5-CHECK bus-priority-seam: PASS`, `B5-CHECK bus-audit-events: PASS`, plus t11 / moderation-priority / the 15 existing executed slots; `assert_b5_contract_pin` prints `B5 contract pin: 39/39 (17 executed, 22 [PROPOSED]): PASS`.
5. **Interim state (seams not yet landed)**: harness is red when NATS is reachable (FM2) — accepted per R-1; the direction lands in the same wave as B5-1/B5-3, or the red G6 is the honest signal.
6. **Sibling follow-ups**: B5-3 lands `AUDIT_PRIORITY` (+ its own unit tests, `stream_for_subject`, `AUDIT_PRIORITY_SUBJECT_PREFIX`, lib.rs doc extension); B5-1 lands `AUDIT_EVENTS` similarly; B5-3(aero-storage) lands 0239 + claim ordering for the moderation drill. No rework of this direction's tests is expected — they assert exactly the seam contracts the siblings pin (E12).
7. **CI**: `.github/workflows/ci.yml` already provisions NATS (E14) — no CI change needed; the legs are live there, not SKIP.

## 7. Testable acceptance mapping (spec §5 A1–A5 ↔ concrete checks)

| Acceptance | Concrete check | Command / probe |
|---|---|---|
| **A1** — T1–T4 exist, assertions per BR1, red until B5-3 | `AERO__NATS__URL=nats://localhost:4222 cargo test -p aero-bus --lib --locked live_nats_audit_priority -- --ignored --test-threads=1` → 4 tests, all pass (seams landed) / all fail (seams missing: `get_stream` or publish errors) | cargo filter `live_nats_audit_priority` |
| **A2** — legs FAIL red when reachable, SKIP clean when not | Reachable: `B5-CHECK bus-priority-seam: PASS` + `exit 1` on failure (with stderr detail); empty-filter → `FAIL (empty filter)`. Unreachable (e.g. `AERO__NATS__URL=nats://127.0.0.1:1`): `B5-CHECK bus-priority-seam: SKIP (NATS unreachable: 127.0.0.1:1)` and harness continues | `scripts/test-integration.sh`; or source `b5-pin.sh` + invoke `run_bus_leg` directly |
| **A3** — manifest 39 = 17 executed + 22 `[PROPOSED]`; guard green | `bash scripts/test-b5-pin-guard.sh` → exit 0; `assert_b5_contract_pin` fails on 38-slot list (`B5 contract pin: 39/38`), on missing verdict lines, on vacuous list | guard self-test; `grep -c '\[PROPOSED\]' scripts/b5-pin.sh` → 22 |
| **A4** — G6 green only with bus legs + t11 + moderation drill | Gate row names both legs; fresh-mode harness: four verdict lines present, pin 39/39 passes; deleting any one of the four verdict lines from `$B5_LOG` → pin FAIL | `grep '^B5-CHECK'` on the B5 log |
| **A5** — lib.rs doc lists all 4 declared streams | `grep -c '^- \`' crates/aero-bus/src/lib.rs` → 4; each name appears in a `bootstrap()` `get_or_create_stream` call (`grep -E 'name: "(IM_MESSAGES|IM_EVENTS|AI_QUEUE|LIVE_EVENTS)"' crates/aero-bus/src/jetstream.rs`) | grep parity |

## 8. Out of scope / dependency-owned (do not build here)

- `AUDIT_PRIORITY` stream + `audit.priority.*` mapping + `stream_for_subject` extraction + `AUDIT_PRIORITY_SUBJECT_PREFIX` → sibling **B5-3 (aero-bus)** spec; the unit tests for those pure functions are theirs.
- `AUDIT_EVENTS` stream + `audit.ws.*` mapping + `AUDIT_SUBJECT_PREFIX` → sibling **B5-1 (aero-bus)**.
- T-11 fail-closed drill and moderation-priority drill → unchanged; only *declared together* in the G6 condition (BR5).
- The empty `crates/aero-bus/tests/` directory stays empty (placement per acceptance).
- No changes to `live_nats_deduplicates_same_message_id`, `bootstrap()`, `subscribe()`, `EventBus` trait, or any production code in `crates/aero-bus`.
