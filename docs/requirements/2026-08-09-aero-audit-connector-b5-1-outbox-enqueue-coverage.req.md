# B5-1 — Complete outbox enqueue coverage: map room.*/admin.* (and non-moderation message.*) actions onto the existing lanes

> Module: `crates/aero-audit-connector/src` (producer-side mapping authority:
> `migrations/0239`/`0242`/`0245` + `crates/aero-ai/src/governance.rs` +
> `crates/aero-common/src/model/audit.rs` + `crates/aero-storage/src/audit_governance.rs`).
> Source analysis: `docs/auto/analyses/crates-aero-audit-connector-src-7edb5949.json` (direction 2).
> Status: source implementation complete (R1/R2); live acceptance remains
> environment-gated.
> **Read §0 first**: the analysis snapshot predates migrations 0242 and 0245;
> most of this direction has since landed. The residual delta is small and
> precisely bounded (§1, §4).

> **Current source status (2026-08-19):** the T-11 drill now seeds the
> message, aggregate, admin, and room shapes with a bounded total; the exact
> `message.recalled` trigger and parity lane are present. Static/unit gates pass;
> the live acceptance commands below still require a fresh migrated PostgreSQL
> database.

## 0. Repo-state delta (analysis is stale — read this first)

The direction's problem statement claims "the v2 outbox producer only ever
enqueues the moderation token" and "`governance_lane_for` returns Some only for
`message.moderated`". **Both claims are false in the current tree.** Between the
analysis and today the enqueue mapping landed through three trigger slices with
pairwise-disjoint, exact-token allowlists:

| Producer (trigger) | Migration | Mapped tokens | Lane (class, priority) |
|---|---|---|---|
| `aero_enqueue_governance_audit` | 0239 | `message.moderated` | admin, 100 |
| `aero_enqueue_l1_aggregate_audit` | 0242 | `message.create` / `message.edit` | message (L1 window + spill), 10 |
| `aero_enqueue_room_audit` | 0245 | `room.create` / `room.archived` | room (1:1), 10 |

`governance_lane_for` (`crates/aero-ai/src/governance.rs:81`) now has three arms
(message.moderated → admin/100 at :83, room.create → room/10 at :97,
room.archived → room/10 at :103); unknown tokens still map to `None` (fail-open).
The storage-side parity suite (`audit_governance.rs`) gained
`room_lane_outbox_parity` (:1750), `message_lane_outbox_parity` (:1938),
`room_lane_unconditional_enqueue` (:2134), `l1_window_aggregates_*` (:1249,
:1503); the harness gained named slots for them (`scripts/b5-pin.sh:69-70`,
`scripts/test-integration.sh:644-665`).

What the direction still requires, verified against the tree:

- **At the time of this snapshot AC4 was NOT landed**: `aero-audit-t11-drill` seeds N 1:1 rows (column
  defaults ⇒ class `'message'`) + one window + one spill shape — **no
  admin-class row, no room-class row** (§4 R1).
- **At the time of this snapshot one in-scope token was unmapped**: `message.recalled` (migration 0238
  recall producer `crates/aero-storage/src/message/authorization.rs:368`,
  in-tx via `AuditRepo::append_in_tx`) is a non-moderation `message.*` action
  the analysis never listed; it passes through all three triggers and stays
  v1-only (§4 R2).
- Everything else in the supplied acceptance (AC1/AC2/AC3/AC5) is **landed and
  pinned**; the acceptance section re-states each in executable form and marks
  the residual items.

## 1. Scope

Close the enqueue-coverage gap for the direction's own title set —
`room.*` / `admin.*` / non-moderation `message.*` — **onto the existing lanes
only** (no new classes, no new priority values; the 0239 CHECKs
`class IN ('admin','message','room')` and `priority > 0` are the schema-change
boundary, a new lane value is a migration, never a silent literal).

**Explicitly out of scope** (recorded, not required):
- **L1 aggregation mechanics** (direction 1 in the source analysis — landed
  via 0242; not re-opened here).
- **`message.deleted`** — the R-D2 exclusion is retained by design (a
  moderation finalize mis-tokened as `message.deleted` must NOT enter the
  admin lane; pinned by `user_delete_token_stays_out_of_admin_lane`). The
  planned Rust-side 1:1 write for it is a sibling path; the parity fixture's
  half B already demonstrates the shape (`audit_governance.rs:455`).
- **Tokens outside the `room.`/`admin.`/`message.` prefix set**:
  `channel.metadata_changed`, `auth.login.new_ip`, `auth.login.recovery_code`,
  `guest.add`/`guest.remove`, `workspace.*`, `member.*`, `session.revoked`
  pass through fail-open and stay v1-only — none of them is a room/admin/
  message token, and a `LIKE 'room.%'`-style prefix widening is explicitly
  forbidden (0245 header).
- **`admin.*` local tokens**: none exist in the vocabulary — the admin lane's
  only in-vocabulary token is `message.moderated` (`MODERATION_OUTBOUND_ACTION
  = "admin.content.flag"` is the outbound contract token, not a local action;
  `audit.rs:150`). No new tokens are invented.
- **Editing landed migration text post-apply** (sqlx checksum discipline,
  0239 precedent) — the new mapping is a new migration file.
- **0243/0244** — auth-slice migrations, designed-only, not landed (0245
  header); untouched.
- **B5-4 provisioning gate** (separate direction in the analysis).

## 2. Evidence verification (all citations re-checked against the repo)

| # | Cited evidence | Verified result |
|---|---|---|
| E1 | `migrations/0239_audit_governance_outbox.sql` — trigger `IF NEW.action <> 'message.moderated' THEN RETURN NEW`; class/priority DDL | ✅ Confirmed. Trigger body early-returns for every non-moderation token (fail-open, "never raise, never block"); DDL `class TEXT NOT NULL DEFAULT 'message' CHECK (class IN ('admin','message','room'))`; `priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)`; moderation stamp `class 'admin'`/`100`; `ON CONFLICT (event_id) DO NOTHING`. Gate 1 (runtime) + Gate 2 (binding RAISE) apply to the moderation token only. |
| E2 | `crates/aero-ai/src/governance.rs:31/:33/:74` — `GOVERNANCE_PRIORITY_MODERATION=100` / `BACKLOG=10`; `governance_lane_for` single-token mapping | ⚠️ **Constants confirmed at :31/:33 (100 / 10); the mapping claim is stale.** `governance_lane_for` is at :81 and now has three arms: `message.moderated` → admin/100 (:83), `room.create` → room/10 (:97), `room.archived` → room/10 (:103). Unknown → `None`. Unit pins: `moderation_lane_preempts_backlog_under_desc_claim`, `unknown_local_token_passes_through_unmapped`, `user_delete_token_stays_out_of_admin_lane` (R-D2), `room_lane_maps_to_room_class`. |
| E3 | `crates/aero-common/src/model/audit.rs:165-169` — `GOVERNANCE_CLASS_ADMIN/MESSAGE/ROOM` single-sourced vocabulary | ✅ Confirmed, current lines :208-212 (derived from `AuditClass::as_str`). Token vocabulary at :150-200: `MODERATION_OUTBOUND_ACTION` :150, `LOCAL_ACTION_MODERATED` :156, `MESSAGE_CREATE` :168, `MESSAGE_EDIT` :170, `AGGREGATED_MESSAGE_ACTION` :175, `AUDIT_SOURCE_SYSTEM` :182, `ROOM_CREATE` :198, `ROOM_ARCHIVED` :200. Typed 16-key twin `AuditClaimPayload` with `deny_unknown_fields` (the drift alarm). Canonical-value pins in `vocabulary_consts_are_pinned` / `l1_vocabulary_consts_are_pinned` / `room_vocabulary_consts_are_pinned`. |
| E4 | `crates/aero-storage/src/audit.rs:154` — `AuditRepo::insert` in-tx producer | ✅ Confirmed. INSERT at :154; `append_in_tx` at :127; `AuditEvent.action` is a free-form dotted token (:23). All classes share this single entry point. |
| E5 | `crates/aero-storage/src/audit_governance.rs` — parity fixture `rust_produced_payload_matches_0239_envelope` | ✅ Confirmed at :455. Half A: real 0239 trigger row (admin class) parses into the typed twin Value-equal; half B: Rust-produced `message.deleted` 1:1 row, class `'message'`/priority 10, field-by-field vs the 0239 envelope, same tx as the audit append. |
| E6 | `crates/aero-audit-connector/src/pg.rs:151-173` — mixed-priority claim test | ⚠️ Line numbers drifted; symbol confirmed at :546. `mixed_priority_claim_orders_moderation_first_then_fifo` seeds 40 backlog (priority 10, earlier `available_at`) + 10 admin (priority 100, later `available_at`), claims 25, asserts claimed set = {10 admin} ∪ {15 earliest backlog} — DESC preemption independent of enqueue order. Claim CTE (`pg.rs:78-96`): `ORDER BY candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id FOR UPDATE SKIP LOCKED`, `status IN (0, 1)`. |
| E7 | `scripts/test-integration.sh` — `audit_governance::` db_tests filter | ✅ Confirmed at :321-331 (named entry, gated on the 0239 file; empty-filter guard = no vacuous green). Sibling slots: t11-fail-closed :366-453, moderation-priority-drill :462-547, L1 arbiter + drills :596-634, room/message-lane parity :644-665. `scripts/b5-pin.sh` lists `audit_governance::`, `moderation_finalize_outbox_parity`, `t11-fail-closed`, `moderation-priority-drill`, `room_lane_outbox_parity`, `message_lane_outbox_parity`. |
| E8 | `aero-audit-t11-drill` / `aero-audit-priority-drill` | ✅ Both exist under `crates/aero-audit-connector/src/bin/`. t11-drill seeds N 1:1 (default class `'message'`) + window + spill shapes, runs 2 rounds against a deterministically closed token endpoint, asserts pending==total, terminal==0, SUM(attempts)==total×round, transport-errors==total, plus per-shape evidence for window/spill. **No admin/room-class seed (AC4 gap).** priority-drill seeds 500 backlog (10) + 1 admin (100), batch 100, asserts moderation-in-first-batch + drain-501 + parity-501. |
| E9 | 0241 disabled-window backfill parity | ✅ `migrations/0241_governance_reconcile.sql` is token-keyed `message.moderated`-only by design ("backfilling an unmapped action would FABRICATE a governance claim"); `governance_reconcile_backfills_disabled_window` (`audit_governance.rs:981`) pins COUNT(outbox)==COUNT(audit) for the mapped subset after a disabled window. 0242/0245 have **no runtime gate** (D2), so no disabled window exists for them — no reconciler extension (0245 header decision, adopted). |
| E10 | Migration numbering | ✅ 0239/0240/0241/0242/0245 landed; **0243/0244 do not exist** (auth-slice designed-only per the 0245 header); next free = **0246**. `MIGRATION_COUNT` arbiter (`test-integration.sh:597-605`) currently pins 243 = actual file count. |

## 3. Additional findings (beyond the cited evidence)

- **F1 — no `admin.*` local tokens exist anywhere in production code.** The
  only `admin.` literal is `MODERATION_OUTBOUND_ACTION` (`audit.rs:150`), an
  *outbound* contract token. "admin.* actions" in the direction therefore
  resolves to the admin **lane**, whose sole in-vocabulary token is
  `message.moderated` — already enqueued (0239). Nothing to add.
- **F2 — `message.recalled` is the only unmapped in-scope token.** Producer:
  `crates/aero-storage/src/message/authorization.rs:368`
  (`AuditRepo::append_in_tx(..., "message.recalled", ...)` — same tx as the
  recall itself, migration 0238). It is a non-moderation `message.*` action
  (the direction's title set), passes through 0239/0242/0245 fail-open, and
  stays v1-only. The recall db_test (`message/recall_tests.rs:231-243`) pins
  the audit row but asserts nothing about the v2 outbox.
- **F3 — T-11 per-class seeding (AC4) is not implemented** (see E8).
- **F4 — producer ownership is trigger-only.** The 0245 header declares the
  binding rule: for trigger-owned tokens, `audit_governance_outbox` rows are
  produced ONLY by SQL triggers; Rust never writes the outbox for them.
  `message.recalled` mapping must follow this precedent (a new trigger), not a
  Rust outbox write.
- **F5 — lane authority split is deliberate and must be preserved.**
  `governance_lane_for` maps the admin + room lanes only; message-lane tokens
  (`message.create`/`message.edit`) stay `None` there BY DESIGN ("mapping them
  here would collide with the R-D2 mis-tokened-rows-stay-out pin",
  `unknown_local_token_passes_through_unmapped` doc). A `message.recalled`
  mapping therefore lives in the SQL allowlist (like L1), cross-pinned by a
  storage db_test (like `room_lane_outbox_parity`).

## 4. Requirements

### R1 — T-11 drill seeds one row per class (closes AC4)

`crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs` must seed, in
addition to the existing N 1:1 rows + window + spill shapes:

- one **admin-class** row: `class = 'admin'`, `priority = 100`
  (`GOVERNANCE_PRIORITY_MODERATION`), 1:1-shaped payload (`event_id` = its own
  PK, `source_system` = `AUDIT_SOURCE_SYSTEM`, own `idempotency_key` — the
  payload guard runs before transport, so a non-conforming payload would dead
  and break `terminal == 0`);
- one **room-class** row: `class = 'room'`, `priority = 10`
  (`GOVERNANCE_PRIORITY_BACKLOG`), same conforming envelope shape.

The drill's invariants are already written in terms of the seeded total
(`total = rows + 2` today → `rows + 4`), so the shift is constant-only. The
per-round per-shape evidence loop must extend to the two new keys: `status ==
0`, `attempts == round`, `last_error` contains the transport failure, and
`class`/`priority` columns equal the seeded values (the per-class evidence —
a seed that silently drops a shape fails the shifted totals). Exit-2 SKIP when
the 0239 table is absent, self-isolating `TRUNCATE`, closed-token-endpoint
mechanism, and both rounds are unchanged. The harness t11 leg
(`test-integration.sh:366-453`) needs no edits: its `pending=[0-9]+` greps
tolerate the extra rows.

### R2 — `message.recalled` maps to the message lane, 1:1 (closes AC1's non-moderation message.* clause)

Add `migrations/0246_message_recall_audit_governance.sql` (next free number;
`MIGRATION_COUNT` arbiter flips 243 → 244 in the same commit, 0245-header
precedent), following the 0245 room-lane template verbatim:

- **Function + trigger**: `aero_enqueue_message_recall_audit()` +
  `audit_events_message_recall_enqueue` AFTER INSERT on `audit_events`. Name
  sorts between `audit_events_l1_aggregate` and `audit_events_room_enqueue`;
  order is inert (token sets pairwise disjoint, 0245 header).
- **Exact-token allowlist, fail-open first line**:
  `IF NEW.action <> 'message.recalled' THEN RETURN NEW;` — NOT a
  `message.%` prefix (0245: "a prefix would fabricate governance claims for
  tokens no contract defined"). Never raises, never blocks.
- **1:1 row**: `event_id = NEW.id` (the pinned `UNIQUE(event_id)` dedup
  contract), `status 0`, `class 'message'` (`GOVERNANCE_CLASS_MESSAGE`),
  `priority 10` spelled explicitly (`GOVERNANCE_PRIORITY_BACKLOG`), the
  16-key 0239 envelope with `action = 'message.recalled'` **verbatim** (no
  fabricated contract token), `source_system = 'aero-im.source'`
  (`AUDIT_SOURCE_SYSTEM` — the connector's `validate_delivery_payload`
  rejects mismatches, PayloadGuard permanent ⇒ dead after ≤1 retry), **no
  `aggregated`/`spill`/`count`/`window_*` keys** (1:1 rows never carry L1
  markers, 0245 precedent — the parity SUM side never sees them),
  `ON CONFLICT (event_id) DO NOTHING` (replay-idempotent).
- **No runtime gate, no binding lookup** (0242/0245 D2 precedent): no
  disabled-window gap ⇒ 0241 stays moderation-only, no reconciler extension.
- **Leaf const**: `LOCAL_ACTION_MESSAGE_RECALLED: &str = "message.recalled"`
  in `crates/aero-common/src/model/audit.rs` (single legal literal site);
  the producer at `authorization.rs:368` spells the token through the const.
  Add a canonical-value pin in the leaf's `room_vocabulary_consts_are_pinned`
  family.
- **`governance_lane_for` stays untouched** (F5): the message lane is
  SQL-side; `is_admin_class("message.recalled") == false` is pinned by
  extending the `unknown_local_token_passes_through_unmapped` token list with
  `LOCAL_ACTION_MESSAGE_RECALLED` (stays `None`).
- **Parity db_test**: `recall_lane_outbox_parity` in
  `audit_governance.rs`, mirroring `room_lane_outbox_parity` (:1750):
  N `message.recalled` audit inserts → exactly N 1:1 outbox rows; `class`,
  `priority`, and every envelope field asserted from the leaf consts (never
  inline literals); payload parses into `AuditClaimPayload`
  (`deny_unknown_fields` = the drift alarm); probe-gated on the 0246
  function existing (the `room_trigger_migrated` pattern). Because the test
  lives inside the `audit_governance::` module, the existing harness slot
  (`test-integration.sh:321-331`) picks it up automatically — **no new
  b5-pin slot, no change to the pinned slot list**.
- **truth-check**: extend `scripts/truth-check-lib.sh`'s message-lane literal
  policing (the `L1_TOKEN_LITERALS_CS` family, :157) to the 0246 allowlist
  literal so a bare `"message.recalled"` outside the leaf is a CI red.

### R3 — No-touch requirements (verify-only; all landed pins must stay green)

- **Fail-open** (AC2): `governance.rs` unit tests
  (`unknown_local_token_passes_through_unmapped`,
  `user_delete_token_stays_out_of_admin_lane`,
  `room_lane_maps_to_room_class`) and `non_moderation_action_passes_through_unmapped`
  (`audit_governance.rs:832` — unmapped token ⇒ zero governance rows, v1 row
  still produced) unchanged and green; no trigger body gains a raise path.
- **Claim ordering** (AC3): `mixed_priority_claim_orders_moderation_first_then_fifo`
  (`pg.rs:546`) and the `aero-audit-priority-drill` harness slot unchanged.
- **Reconcile parity** (AC5): `governance_reconcile_backfills_disabled_window`
  (`audit_governance.rs:981`) unchanged; 0241 remains `message.moderated`-only
  (the only gated token); 0246 must NOT add a reconciler arm.
- **Landed migration text untouched** (0239/0240/0241/0242/0245 — sqlx
  checksum discipline).

## 5. Acceptance criteria (testable)

> Commands assume a throwaway Postgres (`CREATE DATABASE` → `cargo build` →
> `aero-cli migrate` → run → `DROP DATABASE`, AGENTS.md §4.1/§4.3; the
> migration is compile-time embedded, so build before migrate). Supplied
> acceptance items are preserved; each is restated in executable form against
> the current tree, with live-environment checks marked **[LIVE VERIFY]**.

**AC1 — Enqueue coverage: room.* and admin.* (and non-moderation message.*) actions produce outbox rows with correct class/priority.**
Per-class parity coverage is in place: admin — `rust_produced_payload_matches_0239_envelope` half A (real 0239 row, `class='admin'`, priority 100); message 1:1 — same test half B (`class='message'`, priority 10) and the `message.recalled` arm; message L1 — `l1_window_aggregates_5_rows_to_1_outbox` + `message_lane_outbox_parity`; room — `room_lane_outbox_parity` + `room_lane_unconditional_enqueue`. **[LIVE VERIFY]** Re-run the `audit_governance::` harness filter on a migrated throwaway DB; the empty-filter guard prevents a vacuous green.

**AC2 — Fail-open preserved: unmapped tokens still pass through.**
`cargo test -p aero-ai --lib` green: `unknown_local_token_passes_through_unmapped` (after R2, includes `LOCAL_ACTION_MESSAGE_RECALLED` → `None`), `user_delete_token_stays_out_of_admin_lane` (R-D2), `room_lane_maps_to_room_class`. `DATABASE_URL=<migrated throwaway> cargo test -p aero-storage --lib --locked audit_governance:: -- --ignored --test-threads=1` green, including `non_moderation_action_passes_through_unmapped`: `message.deleted` produces exactly zero governance rows and one v1 `snaplink_delivery_outbox` row. No trigger body (0239/0242/0245/0246) ever raises for an unmapped token.

**AC3 — Claim ordering: admin (100) precedes room/message backlog (10) regardless of enqueue order.**
`DATABASE_URL=<migrated throwaway> cargo test -p aero-audit-connector --lib mixed_priority_claim_orders_moderation_first_then_fifo -- --ignored` green (seeded set = {10 admin, later `available_at`} ∪ {40 backlog, earlier `available_at`}; claimed 25 = {10 admin} ∪ {15 earliest backlog}). `DATABASE_URL=<migrated throwaway> cargo run -p aero-audit-connector --bin aero-audit-priority-drill` exits 0 with `moderation-in-first-batch`, `drain-501`, `parity-501` PASS; harness slot `moderation-priority-drill` (`test-integration.sh:462-547`) PASS.

**AC4 — T-11: drill seeds one row per class — relay absent ⇒ all stay status 0, SUM(attempts) grows, no false dead.**
Current source: `DATABASE_URL=<migrated throwaway> cargo run -p aero-audit-connector --bin aero-audit-t11-drill` seeds total = N + 5 (1:1 + window + spill + admin + room + auth-shaped row), enforces N ≤ 95, and asserts `COUNT(status=0) == total`, no terminal rows, growing attempts, and transport `last_error`. **[LIVE VERIFY]** Run the drill on a fresh migrated database; the harness slot is unchanged.

**AC5 — Reconcile parity: 0241 disabled-window backfill keeps COUNT(outbox) == COUNT(audit) for the mapped subset after a disabled window.**
`DATABASE_URL=<migrated throwaway> cargo test -p aero-storage --lib --locked governance_reconcile_backfills_disabled_window -- --ignored` green: with enforcement disabled and no binding, a `message.moderated` audit row commits with zero outbox rows; after re-enable, `aero_reconcile_governance_audit` backfills exactly one outbox row (COUNT parity for the mapped subset = `message.moderated`, the only gated token). Unchanged by R1/R2 — 0242/0245/0246 have no runtime gate, so no disabled window and no reconciler extension (0245 header decision).

**AC6 — `message.recalled` rows land in the message lane.**
`DATABASE_URL=<migrated throwaway> cargo test -p aero-storage --lib --locked audit_governance:: -- --ignored --test-threads=1` green, including `recall_lane_outbox_parity`: N in-tx `message.recalled` audit inserts (via `AuditRepo::append_in_tx`, the `authorization.rs:368` producer seam) → exactly N outbox rows with `class='message'`, `priority=10`, 16-key envelope, `action='message.recalled'` verbatim, `source_system='aero-im.source'`, no L1 marker keys; each payload parses into `AuditClaimPayload` Value-equal. `cargo test -p aero-ai --lib` still green with `LOCAL_ACTION_MESSAGE_RECALLED` in the pass-through list (`governance_lane_for` → `None`; `is_admin_class` → false).

**AC7 — Mapping is exact-token and trigger-owned.**
`rg -n "aero_enqueue_message_recall_audit" migrations/` matches exactly
`migrations/0246_message_recall_audit_governance.sql` (static single-definition
arbiter, 0242/0245 precedent); the 0246 body's allowlist is the exact token
`message.recalled` (no prefix); `message.recalled` is not written to the outbox
by any Rust code (truth-check literal policing passes); the 0246 file exists
and `MIGRATION_COUNT` arbiter (`test-integration.sh:597-605`) passes with 244.

**Cross-checks required before merge** (AGENTS.md §4.3): `cargo build` (before
migrate — migrations are compile-time embedded) · `cargo check --workspace`
clean · `cargo test --workspace --lib` green · `cargo clippy --workspace
--all-targets` no new warnings · `scripts/truth-check.sh`,
`scripts/file-size-check.sh`, `scripts/b5-pin.sh` guard all clean.

## 6. Change surface (complete list)

| File | Change |
|---|---|
| `crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs` | R1: seed admin-class + room-class rows; `total` N+4; per-shape evidence loop extended (status/attempts/class/priority/last_error) |
| `migrations/0246_message_recall_audit_governance.sql` | **New** (R2): `aero_enqueue_message_recall_audit` + trigger `audit_events_message_recall_enqueue`; exact-token allowlist; 1:1 message-class row, 16-key envelope, `ON CONFLICT DO NOTHING`; no runtime gate |
| `crates/aero-common/src/model/audit.rs` | R2: `LOCAL_ACTION_MESSAGE_RECALLED` const + canonical-value pin |
| `crates/aero-storage/src/message/authorization.rs` | R2: `:368` spells the token through the leaf const (no bare literal) |
| `crates/aero-storage/src/audit_governance.rs` | R2: `recall_lane_outbox_parity` db_test (probe-gated, mirror of `room_lane_outbox_parity`) |
| `crates/aero-ai/src/governance.rs` | R2: `unknown_local_token_passes_through_unmapped` token list gains `LOCAL_ACTION_MESSAGE_RECALLED` (stays `None`); no mapping arm added |
| `scripts/truth-check-lib.sh` | R2: message-lane literal policing extends to the 0246 allowlist literal |
| `scripts/test-integration.sh` | R2: `MIGRATION_COUNT` arbiter 243 → 244 (R1 needs no harness edits; R2's test rides the existing `audit_governance::` slot) |
| `scripts/b5-pin.sh` | **No change** (no slot added/removed/renamed) |
| `migrations/0239/0240/0241/0242/0245` | **No change** (landed text untouched, sqlx checksums) |

## 7. Risks and notes

- **Stale-analysis drift**: any implementation must re-verify against the
  three-trigger current state; "extend the 0239 trigger" is wrong — the new
  mapping is a fourth trigger with its own exact allowlist (0245 template).
- **L1 window collision**: `message.recalled` must NOT enter the 0242 window —
  it would fold recall counts into `message.batch` create/edit aggregates and
  corrupt the parity SUM side. The 1:1 requirement (R2) is load-bearing, not
  stylistic; `l1_window_aggregates_*` and `message_lane_outbox_parity` guard
  the window side and must stay green.
- **Payload guard**: R1's new seed rows and R2's trigger rows must carry
  `source_system='aero-im.source'` + own `event_id` + own `idempotency_key`
  (AERO_AUDIT_SOURCE_SYSTEM deployment invariant) or
  `validate_delivery_payload` deads them after ≤1 retry — breaking
  `terminal == 0` in T-11 and the parity fixtures.
- **b5-pin slot list is pinned**: the acceptance mentions the
  `moderation_finalize_outbox_parity` slot (current 48/48; 37/37 is the
  historical analysis-time baseline). R1/R2 must not add, remove, or rename
  slots — new tests ride existing module filters.
- **Trigger-name ordering**: `audit_events_message_recall_enqueue` sorts
  between `audit_events_l1_aggregate` and `audit_events_room_enqueue`; token
  sets are pairwise disjoint so firing order is inert (0245 header) — do not
  "fix" ordering.
- **`message.deleted` stays excluded** (R-D2): R2's exact-token allowlist must
  not tempt a `message.%` widening; `user_delete_token_stays_out_of_admin_lane`
  and `non_moderation_action_passes_through_unmapped` are the regression pins.
