# Requirements Spec — DB-gated tests for uncovered notification fan-out sources & relay failure paths

- **Module**: `crates/aero-im-core`
- **Source direction**: `docs/auto/analyses/crates-aero-im-core-cfe64e80.json` (direction 1)
- **Date**: 2026-08-06 · **Status**: implemented in source (DB-gated acceptance remains `#[ignore]` until live Postgres is supplied)
- **Scope**: `#[ignore]`-gated Postgres integration tests covering the three original gaps: (1) user-group `@handle` and keyword-alert fan-out, (2) durable relay failure branches (`mark_failed` re-park, partial-batch count, superseded-lease fencing), and (3) `flush_notification_bundles`. The test-only MockBus seam and harness wiring are now present; no production dependencies or migrations were added.

> Current-source note (2026-08-19): the acceptance implementation is in
> `crates/aero-im-core/src/db_tests.rs`, `db_tests/notifications_tests.rs`,
> `db_tests/relay_tests.rs`, and `test_util.rs`. Hermetic workspace tests pass;
> the ignored suite still requires a throwaway migrated Postgres (and Redis for
> the existing `@here` leg) to execute.

---

## 1. Evidence verification (every direction citation re-checked against the working tree)

| Direction claim | Verified finding |
|---|---|
| `dispatch_notifications` expands user-group `@handle` mentions and keyword-alert subscribers | ✅ `crates/aero-im-core/src/service/orig.rs:652` (`pub(crate) async fn dispatch_notifications`). Reply-author branch :675–683; direct mentions :685–692; thread subscribers :688–712; broadcast `@channel`/`@here` :730–758; **user groups :741–764** (`self.user_groups.as_ref()` → `groups.resolve(workspace, handle)` → `groups.members(group.id)` → `targets.entry(m).or_insert(NotificationKind::Mention)`); **keyword alerts :766–790** (`self.keyword_alerts.as_ref()` → `alerts.matching_subscribers(workspace, &text)` → same `or_insert(Mention)`); block filter :818–832; workspace mute :834–867; batch mute/DND/snooze filter :927–965; mention/reply partition :967–979; immediate `insert_many_outboxed` :982–997; bundle `insert_many_idempotent` :999–1016 |
| `notification_service()` in `db_tests.rs:99-127` only wires `thread_subs` | ✅ Helper at `db_tests.rs:103–127` (doc comment from :99). Wires `with_workspaces`/`with_notifications`/`with_notification_prefs`/`with_block_repo`/`with_workspace_mutes`/`with_thread_subs`/`with_thread_mutes`/`with_thread_notification_prefs` (+`with_notification_bundles` iff `bundles`). **No `with_user_groups`, no `with_keyword_alerts`.** |
| grep found no `with_user_groups`/`with_keyword_alerts` in the test tree (historical gap) | ✅ historical finding; current `notification_service()` wires both repos and `at1a`–`at1e`/`at2_keyword` exercise them |
| `notifications_tests.rs` originally had 14 tests, none for group mentions or keyword alerts (historical gap) | ✅ historical finding; current source adds dedicated group and keyword acceptance tests plus bundle-flush coverage |
| `finish_claimed_outbox` → `mark_failed` re-park with backoff (`outbox.rs:57-95`) | ✅ `outbox.rs:57`; error branch :61–95: `repo.mark_failed(id, attempts, now, &error.to_string())` with three arms — `Err(mark_error)` warn, `Ok(true)` warn "retry scheduled", **`Ok(false)` debug "superseded lease"** (the fencing arm). `mark_failed` at `aero-storage/src/event_outbox.rs:476`: `SET available_at = now + outbox_backoff_delay(attempts), claimed_at = NULL, last_error = $4 WHERE id AND attempts = $2 AND claimed_at IS NOT NULL AND published_at IS NULL`; returns `rows_affected() > 0` |
| `finish_message_side_effect` error re-park (`side_effects.rs:52-82`) | ✅ `side_effects.rs:48` (fn spans :48–93); identical three-arm `mark_failed` handling; `mark_failed` at `message_side_effect.rs:307` (WHERE additionally `completed_at IS NULL`); `side_effect_backoff` :358 (1s · 2^attempts, cap 300s) |
| `relay_tests.rs` originally covered only success paths plus stale-version skip (historical gap) | ✅ historical finding; current `at3a`–`at3d` add failure, partial-batch, fencing, and side-effect re-park acceptance tests |
| `flush_notification_bundles` originally had no test call sites (historical gap) | ✅ historical finding; current `at4_notification_bundle_flush_materializes_durable_event` covers materialization, deterministic id, outbox payload, and idempotent re-flush |
| Production flush timer at `background.rs:459` | ✅ `crates/aero-server/src/bin/boot/background.rs:459` `im.flush_notification_bundles().await;` (timer :443–461, `AERO_NOTIFICATION_BUNDLES` + `AERO_NOTIFICATION_BUNDLE_FLUSH_SECS` gated) |
| `services.rs:89-91` wires user_groups/keyword_alerts | ✅ wiring present but at **:86** (`.with_user_groups(...)`) and **:88** (`.with_keyword_alerts(...)`) — the cited range is 3 lines off; the claim (production-wired) stands |
| Prior approved design `docs/auto/runs/add-db-gated-integration-tests-for-the-notificat-d779ddbf/artifacts/design-a77de8a6/task-1-design.md` | ✅ design gate PASS; its acceptance is now implemented in the current working tree (the final DB/Redis run remains environment-gated) |

**Additional load-bearing facts discovered while verifying (shape the tests):**

1. **`attempts` increments at claim time, not in `mark_failed`**: `claim_due` runs `SET claimed_at = $1, attempts = outbox.attempts + 1` (`event_outbox.rs:295–340`; same in `message_side_effect.rs claim_matching`). After one failed dispatch a row shows `attempts = 1`; the acceptance's "attempts+1" is satisfied by asserting the claim-time increment across repeated failed dispatches (0 → 1 → 2).
2. **Backoff values**: `outbox_backoff_delay(1) = 1s`, `(2) = 2s` (`event_outbox.rs:17, 519–525`, `BASE_BACKOFF_SECONDS = 1`, cap 300). Assertable with tolerance.
3. **`MockBus` failure injection**: the historical always-success bus now has a test-only atomic failure counter and one-shot rendezvous gate. `publish_claimed_outbox` still reaches it through `BusSink::publish_bytes_idempotent`, so the injected error deterministically exercises `mark_failed` without changing production behavior.
4. **The flush-produced outbox `event_id` is `bundle_delivery_id`, not `notify_delivery_id`**: flush inserts the Notify outbox row with `event_id = bundle_delivery_id(&bundle_ids, participant, room, thread_root)` — a UUIDv5 under frozen namespace `37301db9-9690-4ad7-8324-12d5cdd730a0` over the **sorted bundle row ids** (`notification_bundle.rs:36, 450–462`; outbox insert :355–367). `notify_delivery_id` (frozen namespace `6f1d2e3c-9b4a-4d57-8a2e-1c0f7b6a4d9e`, `orig.rs:79, 89`) is the **bundle row's `delivery_id` column** and the immediate-path outbox id. The acceptance's "deterministic `notify_delivery_id`" is preserved as "deterministic delivery id on both artifacts" with the exact symbols above (see correction C2).
5. **Side-effect error re-park is deterministically injectable without a bus**: `message_side_effect_jobs.message_id` has **no FK** (`migrations/0165_message_side_effect_jobs.sql` — plain `UUID NOT NULL`), so a job seeded with a nonexistent `message_id` makes `process_message_side_effect` fail at `messages.get(...).context("load side-effect message")` (`side_effects.rs:105–108`) → `mark_failed` → `Ok(true)` re-park. This covers the cited `side_effects.rs:52-82` branch with zero production seams.
6. **Flush deadline**: `NotificationBundleRepo::new(pool)` reads `AERO_BUNDLE_DEADLINE_SECS` (default 30s, floor 10s — `notification_bundle.rs:45–54`). Tests must **backdate `created_at` via SQL** (proven pattern in storage tests `notification_bundle.rs:640–680`); setting env vars in-test is process-global and unsafe under parallel test execution.
7. **Side-effect claim completes at bundle-insert time, not flush time**: `insert_many_idempotent(..., Some(source))` → `complete_bundle_source` (`notification_bundle.rs:420`) commits the claim inside the bundle-insert transaction. So the acceptance's "side-effect claim completed" is assertable immediately after send, independent of flush.
8. **`matching_subscribers`** (`keyword_alert.rs:312–340`): case-insensitive substring (`position(keyword IN lower($2)) > 0`), workspace-scoped, plus an effective-workspace-access filter. Alert creation: `add_authorized(participant, workspace, keyword)` (:99) requires effective workspace membership. Group creation: `UserGroupRepo::create_authorized(workspace, handle, name, caller)` (:182) requires the caller to hold a `workspace_members` role; `add_member_authorized` (:295); `resolve` (:600); `members` (:741). All `pub`.
9. **Test tenants resolve to the nil workspace**: `new_participant` enrolls into the reserved default workspace (`db_tests.rs:309–330`), legacy `svc.create_room` rooms carry `workspace_id = Uuid::nil()` (`room.rs:74`), and `room_workspace` (`room.rs:381`) returns it — so group/keyword expansion in tests runs against the nil workspace exactly as in production boot.
10. **`set_dnd` has no range validation** (`notification_prefs.rs:276–291`) and the DND window is end-exclusive — use the full-day window `(0, 1440)` (the `(0, 1439)` variant has a 1-minute hole at 23:59 UTC).
11. **BATCH_SERIAL**: relay tests claim GLOBAL state (`claim_due` without a message filter). The new failure-injection tests must hold the existing `BATCH_SERIAL` mutex (`db_tests.rs:71–75`) for their seed → dispatch → assert sections, and the runbook runs the suite with `--test-threads=1` (established convention in `relay_tests.rs:12–16`).

---

## 2. Problem statement (historical gaps; now closed by the acceptance suite)

- `dispatch_notifications` now has DB-gated tests for user-group expansion, room-membership intersection, sender exclusion, direct/reply precedence, block/DND/channel-mute suppression, and keyword-alert authorization.
- The durable relay failure branches now have deterministic MockBus coverage for re-park/backoff, partial success counts, superseded-lease fencing, and nonexistent-message side-effect retries.
- `flush_notification_bundles` now has a service-level test for expiration, deterministic UUIDv5 delivery id, NotifyBatch payload/outbox, completed source claim, and idempotent second flush.

---

## 3. Non-goals / scope boundary

1. **No production-code changes** in any crate. The only touched files are test-only: `db_tests.rs` (harness), `db_tests/notifications_tests.rs`, `db_tests/relay_tests.rs`, `test_util.rs` (MockBus failure knob — already `cfg(test)`-compiled).
2. **No re-specification or modification of the existing at1–at7 suite** (already in the working tree, uncommitted): its tests, helpers, and `BATCH_SERIAL` convention are reused as-is.
3. **No new dependencies**; no root `Cargo.toml` changes; no migrations (schema is final).
4. Not in scope: presence wiring (`@here` online-roster narrowing), metrics counters, `orig.rs` refactors, NATS/Redis integration, the side-effect superseded-lease arm *of `mark_failed`* (not deterministically injectable without a production seam — only the outbox superseded arm is covered, per AC-3).
5. The acceptance checks below are preserved 1:1 from the direction; nothing beyond them is required.

---

## 4. Requirements

### REQ-0 — Test harness changes (test-only)

- **`notification_service()`** (`db_tests.rs:103`) gains two builder calls, mirroring production boot (`services.rs:86/88`):
  `.with_user_groups(UserGroupRepo::new(pool.clone()))` and `.with_keyword_alerts(KeywordAlertRepo::new(pool.clone()))`.
  Both repos are already workspace deps of `aero-im-core` (no `Cargo.toml` change). Every existing caller keeps working (additive builders).
- **`MockBus`** (`test_util.rs`) gains test-only failure knobs, interior-mutable so the bus can be armed *after* setup publishes succeed:
  - `fail_publishes: std::sync::atomic::AtomicUsize` — fail the next N `publish` calls with `BusError::Nats("injected failure")` (do not record them); `usize::MAX` ≈ "always errors". Armed after seeding, disarmed (0) during setup.
  - A rendezvous gate for the superseded-lease test: when armed, `publish` signals a `tokio::sync::oneshot::Sender` and awaits a release `oneshot::Receiver` before returning the injected error. (Exact field shape is the implementer's choice; the capability is: block inside `publish`, let the test mutate the claimed row mid-flight, then release.)
- **New shared helpers** in `db_tests.rs` (or the submodule, re-exported):
  - `backdate_outbox_row(pool, id)` — `UPDATE event_outbox SET available_at = now() - interval '1 minute' WHERE id = $1` (re-arm for the 2nd/3rd failed dispatch; `mark_failed` already clears `claimed_at`).
  - `backdate_bundle(pool, message_id)` — `UPDATE notification_bundles SET created_at = now() - interval '1 minute' WHERE message_id = $1` (flush deadline is ≥10s; proven storage-test pattern `notification_bundle.rs:640–680`).
  - `outbox_row_state(pool, id) -> (attempts: i32, last_error: Option<String>, available_at: Option<OffsetDateTime>, published_at: Option<OffsetDateTime>)` — single-row SELECT for the re-park assertions.
  - `bundle_delivery_ids(pool, message_id) -> Vec<uuid::Uuid>` — the bundle row ids consumed by flush (read **before** flush).
  - `expected_bundle_delivery_id(participant, room, thread_root, bundle_ids) -> uuid::Uuid` — recomputes the flush outbox `event_id` in-test: `Uuid::new_v5(&Uuid::parse_str("37301db9-9690-4ad7-8324-12d5cdd730a0").unwrap(), name)` with `name = format!("{participant}:{room}:{thread_root}:{sorted_ids_with_colons}")` per the documented algorithm (`notification_bundle.rs:450–462`). The frozen namespace is hardcoded in the test — a namespace change fails loudly by design.
  - Reuse existing helpers: `group_room` (:77), `new_participant` (:309, nil-workspace enrollment), `insert_channel_mute` (:142), `insert_side_effect_job` (:180, raw SQL — used for the nonexistent-message job), `count_notifications` (:225), `count_notify_outbox` (:244), `count_bundles` (:255), `pending_notify_outbox` (:279), `side_effect_job_state` (:296), `BATCH_SERIAL` (:71).
- **Test placement**: group/keyword/flush tests → `db_tests/notifications_tests.rs`; relay failure-injection tests → `db_tests/relay_tests.rs`. All `#[tokio::test] #[ignore = "requires running Postgres with migrations applied"]`, unique emails via `unique_email`, fresh room per test.

### AT-1 — User-group `@handle` fan-out (acceptance check 1)

Setup (all sub-tests): `notification_service(pool, false)`; `group_room` with alice (sender), bob, carol, dave; plus eve = `new_participant` (workspace member, **not** a room member). `let groups = UserGroupRepo::new(pool.clone());` `let g = groups.create_authorized(nil_ws, "team", "Team", alice.id).await.unwrap();` then `add_member_authorized(nil_ws, g.id, X, alice.id)` for bob, carol, eve. Send messages with `Block::text("@team ...")` (handle tokens parsed by `group_handle_tokens`, resolved by `groups.resolve(nil_ws, "team")`).

- **AT-1a expansion ∩ room-members, excluding sender**: alice sends `Block::text("hello @team")`. Assert exactly 2 `notifications` rows for the message: (bob, kind `mention`, actor alice) and (carol, kind `mention`); **no** row for alice (sender exclusion), dave (group non-member), eve (group member but not a room member).
- **AT-1b `or_insert` never overrides a stronger kind**: (i) bob sends root; alice replies `reply_to = Some(root.id)` with text `"@team"` → bob's row kind must be `reply` (reply-author branch :675–683 inserted `Reply` first; group expansion's `or_insert(Mention)` at :764 must not downgrade), carol's row kind `mention`. (ii) alice sends `[Block::Mention { participant: bob.id }, Block::text("@team")]` → exactly one row for bob, kind `mention` (no duplicate, no overwrite).
- **AT-1c block filter**: `BlockRepo::block(bob, alice)` (bob blocks the sender); alice sends `"@team"` → bob absent, carol present (block filter :818–832 applies to group-expanded targets).
- **AT-1d DND filter**: `NotificationPrefsRepo::set_dnd(bob, Some(0), Some(1440))` (full-day window, end-exclusive — constraint 10); alice sends `"@team"` → bob absent, carol present (batch mute/DND filter :927–965).
- **AT-1e room mute**: `insert_channel_mute(bob, room)` (raw SQL — `NotificationPrefsRepo::mute` is `#[cfg(test)] pub(crate)` in aero-storage, unreachable from here); alice sends `"@team"` → bob absent, carol present.

### AT-2 — Keyword-alert fan-out (acceptance check 2)

Setup: `notification_service(pool, false)`; `group_room` (alice sender, bob, carol, dave); eve = `new_participant` (workspace member, not room member). `let alerts = KeywordAlertRepo::new(pool.clone());` — `add_authorized(bob, nil_ws, "invoice")`, `add_authorized(alice, nil_ws, "invoice")` (sender's own alert), `add_authorized(eve, nil_ws, "invoice")` (non-room-member), `add_authorized(carol, nil_ws, "payroll")` (non-matching).

- alice sends `Block::text("send the invoice now")` (searchable text — `Message::searchable_text` aggregates `Block::searchable_text`). Assert exactly **1** `notifications` row: (bob, kind `mention`, actor alice). Nothing for alice (sender exclusion :787–789), eve (not a room member — `member_set.contains` guard), carol (keyword doesn't match), dave (no alert).
- Negative control: alice sends `Block::text("nothing relevant here")` → 0 rows for that message (no alert matches; `matching_subscribers` returns only bob's "invoice" match on "invoice", carol's "payroll" doesn't match this text).

### AT-3 — Relay failure injection (acceptance check 3) — `relay_tests.rs`, under `BATCH_SERIAL`

All rows are seeded via `send_message` with mentions on `bundles=false` services (at6a's shape: the Notify outbox row is left pending by the fast path). The MockBus failure knob is armed **after** seeding so setup publishes succeed. Assertions use `outbox_row_state`.

- **AT-3a re-park with backoff**: seed one pending Notify row (attempts 0). Arm `fail_publishes = usize::MAX`; `svc.dispatch_event_outbox_batch(10)` → returns `Ok(0)` (published count 0). Assert row: `attempts == 1` (claim-time increment, constraint 1), `last_error` set and non-empty (contains the injected error), `claimed_at IS NULL`, `published_at IS NULL`, `available_at` in `(now(), now() + 5s)` (`outbox_backoff_delay(1) = 1s`, constraint 2). Re-arm (`backdate_outbox_row`), dispatch again → `attempts == 2`, `available_at` backed off again (≈2s window), `last_error` refreshed. MockBus recorded **zero** publishes.
- **AT-3b partial-batch published count**: seed **two** pending Notify rows (two messages). Arm `fail_publishes = 1` (first publish attempt fails, second succeeds); `dispatch_event_outbox_batch(10)` → returns `Ok(1)`. Assert exactly one row published (`published_at NOT NULL`) and exactly one row re-parked (`attempts == 1`, `last_error` set, `available_at` in future); `bus.published.len() == 1` and its `event_id` equals the succeeded message's `notify_delivery_id(msg.id, NotifyBatchKind::Mention)` (deterministic — `orig.rs:89`). The count does not depend on claim order (which of the two rows fails first is deterministic per run but not asserted).
- **AT-3c superseded lease (`mark_failed` → `Ok(false)`) fenced without panic**: seed one pending Notify row; arm the MockBus **rendezvous gate** (block inside `publish`). Spawn `dispatch_event_outbox_batch(10)`; when the gate signals (row is claimed: `attempts == 1`, `claimed_at` set), simulate the concurrent winner: `UPDATE event_outbox SET published_at = now() WHERE id = $1`; release the gate (publish returns the injected error). Join → no panic, returns `Ok(0)`. Assert the fencing did **not** clobber the winner: row `published_at IS NOT NULL`, `last_error IS NULL`, `attempts == 1`, `available_at` unchanged from claim time. (The `Ok(false)` arm logs `debug!` "superseded lease" — `outbox.rs:87–95`.)
- **AT-3d side-effect error re-park** (covers the cited `side_effects.rs:48–93` branch): seed a `notifications`-kind job with a **nonexistent** `message_id` via `insert_side_effect_job` (raw SQL; no FK on `message_side_effect_jobs.message_id` — migration 0165). `svc.dispatch_message_side_effect_batch(10)` → returns `Ok(0)` (completed count). Assert job: `attempts == 1`, `last_error` contains "load side-effect message", `claimed_at IS NULL`, `completed_at IS NULL`, `available_at` in `(now(), now() + 5s)` (`side_effect_backoff(1) = 1s`). Re-arm (`UPDATE message_side_effect_jobs SET available_at = now() - interval '1 minute'`) → second dispatch → `attempts == 2`, re-parked again.

### AT-4 — `flush_notification_bundles` end-to-end + idempotence (acceptance check 4)

Setup: `notification_service(pool, true)` (bundles wired); `group_room` with alice, bob. alice sends root; bob replies `reply_to = Some(root.id)`.

Precondition (before flush): exactly 1 `notification_bundles` row for the reply: `participant_id = alice`, `message_id = reply.id`, `thread_root = root.id`, kind `'reply'`, `delivery_id == notify_delivery_id(reply.id, NotifyBatchKind::Reply)` (frozen namespace — `orig.rs:79, 89`); 0 `notifications` rows, 0 `event_outbox` Notify rows for the reply (at3's routing, :265); the side-effect job for the reply is `completed_at IS NOT NULL` (claim completed at bundle-insert time — constraint 7).

Act: read `bundle_ids = bundle_delivery_ids(pool, reply.id)`; `backdate_bundle(pool, reply.id)`; `svc.flush_notification_bundles().await`.

Assert (first flush): `count_bundles(reply.id) == 0` (consumed); `count_notifications(reply.id, alice) == 1` with kind `'reply'` and `delivery_id == expected_bundle_delivery_id(alice, room, root.id, bundle_ids)`; `count_notify_outbox(reply.id) == 1` and `pending_notify_outbox(reply.id)` returns `event_id == expected_bundle_delivery_id(...)` (the deterministic flush id — constraint 4; the outbox row is **pending**: `published_at IS NULL`, awaiting the relay); the outbox payload decodes to `RoomEvent::NotifyBatch { room_id, message_id: reply.id, by: bob, delivery_id: <same uuid>, recipients: [alice] }`; the side-effect job remains completed.

Assert (idempotent second flush): call `svc.flush_notification_bundles().await` again → `count_bundles == 0`, `count_notifications == 1`, `count_notify_outbox == 1` (nothing inserted; flush selects zero expired bundles).

### AT-5 — Acceptance gates (acceptance check 5)

The full suite stays green after the change:
- `cargo check --workspace` — clean.
- `cargo clippy --workspace --all-targets` — no new warnings (AGENTS.md §4.2: root lints `warn`, no new ones).
- `cargo test --workspace --lib` — hermetic run green (new tests are `#[ignore]`).
- `scripts/truth-check.sh` — 0 violations (test-only helpers are all reachable from `#[tokio::test]` fns).
- `scripts/file-size-check.sh` — new code stays under thresholds.

---

## 5. Acceptance mapping (direction checks → this spec)

| Direction acceptance check | Preserved as |
|---|---|
| 1. `notification_service()` gains `with_user_groups` + `with_keyword_alerts`; group mention expands to group-members ∩ room-members excluding sender, never overrides a stronger direct mention kind (`or_insert`), filtered by block/mute/DND | REQ-0 + AT-1a–1e |
| 2. Keyword-alert fan-out: matching subscriber gets `NotificationKind::Mention` only when room member and not sender; non-matching get nothing | AT-2 |
| 3. Failure injection (MockBus always errors): `dispatch_event_outbox_batch` leaves `last_error` set, `attempts+1`, `available_at` backed off; correct published count for partial batch failure; superseded lease (`Ok(false)`) handled without panicking | AT-3a–3c (+ AT-3d covers the cited `side_effects.rs` error branch deterministically) |
| 4. `flush_notification_bundles`: bundle row → flush → bundle consumed, inbox row inserted, Notify outbox row pending with deterministic delivery id, side-effect claim completed; second flush idempotent | AT-4 |
| 5. Full suite green (`cargo check`, `clippy --all-targets` no new warnings, `truth-check.sh`) | AT-5 |

**Corrections to the direction's text (all verified; none change intent):**
- **C1** — boot wiring is at `services.rs:86`/`:88` (not :89–91).
- **C2** — the flush-produced outbox `event_id` is `bundle_delivery_id` (UUIDv5 over sorted bundle ids, namespace `37301db9-…`), while `notify_delivery_id` is the bundle row's `delivery_id` column and the immediate-path id. AT-4 asserts both deterministically.
- **C3** — `attempts` increments at claim time (`claim_due`), not in `mark_failed`; AT-3 asserts 0 → 1 → 2 across repeated failed dispatches.
- **C4** — the prior suite is in the working tree (uncommitted) rather than absent; the three gap claims were re-verified and all hold.

## 6. Runbook (how the DB-gated suite is executed)

```
cd /home/u1/aero-im
cargo build            # migrations are compile-time embedded; build BEFORE migrate (AGENTS.md §4.2)
# fresh throwaway DB, never the shared dev DB (AGENTS.md §4.3):
createdb aero_test_relay   # or per local convention; apply: aero-cli migrate
DATABASE_URL=postgres://.../aero_test_relay \
  cargo test -p aero-im-core --lib -- --ignored --test-threads=1
# then the AT-5 gates; then: dropdb aero_test_relay
```

---

## 7. Failure modes the suite guards (regression contract)

- Group/keyword fan-out silently dropping recipients (wrong intersection, sender included, non-member notified, stronger kind downgraded) or leaking room membership.
- Suppression layers (block/mute/DND) bypassed for group/keyword-expanded targets.
- Relay failure strands rows unparked (`available_at` never set → row invisible to `claim_due` → notification lost forever) or double-publishes after lease races (superseded `mark_failed` clobbering a concurrent completion).
- Bundle flush losing deferred replies (bundle consumed without inbox/outbox materialization), duplicating them (idempotence broken), or regressing the deterministic delivery id contract.
