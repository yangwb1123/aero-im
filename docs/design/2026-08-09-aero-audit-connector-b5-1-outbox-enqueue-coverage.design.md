# Design — B5-1 outbox enqueue coverage completion (R1 + R2 + R3)

> Source: `docs/requirements/2026-08-09-aero-audit-connector-b5-1-outbox-enqueue-coverage.req.md`.
> Status: source implementation complete; this design retains the historical
> residual analysis. Live acceptance remains environment-gated.

> **Current source status (2026-08-19):** the T-11 five-shape seed/bound,
> exact-token `message.recalled` lane, and associated parity tests are present.
> The remaining commands below require a fresh migrated PostgreSQL database.

## 0. Executive summary

The direction's problem statement ("v2 producer only enqueues the moderation
token", "`governance_lane_for` returns Some only for `message.moderated`") is
**stale**: migrations 0242 (L1 message lane) and 0245 (room lane) landed, and
`governance_lane_for` now has three arms. The historical residual delta was
exactly two items plus a verify-only third; both implementation items are now
present in the current tree:

- **R1**: `aero-audit-t11-drill` now seeds one admin-class and one room-class
  row (AC4).
- **R2**: `message.recalled` (the only in-scope unmapped non-moderation
  `message.*` token) now maps 1:1 onto the existing message lane via migration
  **0246** (trigger-only ownership, exact-token allowlist) + leaf const + parity
  db_test.
- **R3**: fail-open, claim ordering, and reconcile parity are already pinned —
  verify only, zero code change.

## 1. Verification record (evidence → tree)

| # | Cited claim | Verdict (with anchor) |
|---|---|---|
| E1 | 0239 trigger `IF NEW.action <> 'message.moderated' THEN RETURN NEW`; `CHECK (class IN ('admin','message','room'))`; `priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)`; moderation stamp class 'admin'/100 | ✅ Verbatim in `migrations/0239_audit_governance_outbox.sql` (fail-open first line, Gate 1 runtime + Gate 2 binding RAISE, `ON CONFLICT (event_id) DO NOTHING`). |
| E2 | `GOVERNANCE_PRIORITY_MODERATION=100` / `BACKLOG=10` at governance.rs:31/:33; **mapping claim stale** | ✅ Constants confirmed at :31/:33. `governance_lane_for` at :81 has **three arms**: `LOCAL_ACTION_MODERATED`→admin/100 (:83), `LOCAL_ACTION_ROOM_CREATE`→room/10 (:97), `LOCAL_ACTION_ROOM_ARCHIVED`→room/10 (:103); `_ => None` fail-open. Message-lane tokens deliberately stay `None` (F5, documented in the fn doc). |
| E3 | Leaf vocabulary in `aero-common/src/model/audit.rs` | ✅ `MODERATION_OUTBOUND_ACTION` :150, `LOCAL_ACTION_MODERATED` :156, `LOCAL_ACTION_MESSAGE_CREATE` :168, `LOCAL_ACTION_MESSAGE_EDIT` :170, `AGGREGATED_MESSAGE_ACTION` :175, `AUDIT_SOURCE_SYSTEM` :182, `LOCAL_ACTION_ROOM_CREATE` :198, `LOCAL_ACTION_ROOM_ARCHIVED` :200, `GOVERNANCE_CLASS_*` derived from `AuditClass::as_str` :208-212. Typed 16-key twin `AuditClaimPayload` with `deny_unknown_fields`. Pins: `vocabulary_consts_are_pinned` :480, `l1_vocabulary_consts_are_pinned` :495, `room_vocabulary_consts_are_pinned` :511. |
| E4 | `audit.rs:154` insert, `append_in_tx` :127 | ✅ `append_in_tx` at :127; shared INSERT statement at :155 (spec's :154 is the `AuditId::new()` line — ±1 drift, substance exact). `AuditEvent.action` is a free-form dotted token (:23). |
| E5 | Parity fixture `rust_produced_payload_matches_0239_envelope` :455; `mixed_priority_claim_orders_moderation_first_then_fifo` pg.rs:546 | ✅ Both exact. Claim CTE at pg.rs:109-117 (`ORDER BY candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id FOR UPDATE SKIP LOCKED`, `status IN (0,1)`) — spec's ":78-96" is actually `reconcile`; the DESC-claim substance is confirmed. |
| E6 | 0241 reconcile + `governance_reconcile_backfills_disabled_window` :981 | ✅ 0241 is token-keyed `message.moderated`-only; test at :981. |
| E7 | Harness `audit_governance::` slot test-integration.sh:321-331 | ✅ Confirmed (0239-file-gated, `run_migrated_integration`, empty-filter guard = no vacuous green). Sibling slots at :366-453 (t11-fail-closed), :462-547 (moderation-priority-drill), :596-634 (L1 arbiter + drills), :644-665 (room/message-lane parity). `scripts/b5-pin.sh` lists `audit_governance::` :38, `t11-fail-closed` :41, `moderation-priority-drill` :42, `room_lane_outbox_parity` :69, `message_lane_outbox_parity` :70. |
| E8 | T-11 drill seeds per-class shapes | ✅ Current source seeds 1:1, window, spill, admin, room, and auth-shaped rows; reads transport failure from the `last_error` column and enforces `AERO_AUDIT_DRILL_ROWS ≤ 95` because the fixed claim batch is 100. |
| E9 | `aero-audit-priority-drill` | ✅ Exists (500 backlog/10 + 1 admin/100, batch 100, moderation-in-first-batch). |
| E10 | Migration numbering; `MIGRATION_COUNT` arbiter | ✅ 0239/0240/0241/0242/0245/0246 are present; 0243/0244 remain designed-only; the current migration count and arbiter agree at 244. |
| F2 | `message.recalled` producer at `authorization.rs:368`; only unmapped in-scope token | ✅ Exact line confirmed: `AuditRepo::append_in_tx(tx, workspace, Some(actor), "message.recalled", Some(&id.to_string()), json!({...}))`, in the same tx as the 0238 recall + `EventOutboxRepo::insert_room_event_in_tx`. Recall db_test (`recall_tests.rs:231-243`) pins the v1 audit row only. Passes 0239/0242/0245 fail-open → v1-only today. |
| F3/F4/F5 | Per-class seeding absent; trigger-only ownership (0245 header); lane authority split | ✅ All confirmed (0245 header declares trigger-owned set; `message.deleted` R-D2 excluded; `unknown_local_token_passes_through_unmapped` doc pins the no-message-arm rule). |
| — | Truth-check literal families | ✅ `scripts/truth-check-lib.sh` :157 `L1_TOKEN_LITERALS_CS=('"message.create"' '"message.edit"' '"message.batch"')`, :171 `ROOM_TOKEN_LITERALS_CS=('"room.create"' '"room.archived"')`, :335 enforcement loop. |

**Conclusion of the historical review:** the evidence identified the R1/R2
gaps. Those source changes are now present; the remaining acceptance rows are
verification commands only. The design below is retained as provenance and
preserves R3.

## 2. API changes

### 2.1 New public API (additive, non-breaking)

1. **Leaf const** — `crates/aero-common/src/model/audit.rs`:
   `pub const LOCAL_ACTION_MESSAGE_RECALLED: &str = "message.recalled";`
   placed in a new `// ---- Message-recall vocabulary (migration 0246) ----`
   block after the room-lane block. It is the **single legal literal site** for
   the token in Rust (truth-check Rule 3f extension, §2.3).
2. **Re-export** — `crates/aero-ai/src/governance.rs` `pub use` chain gains
   `LOCAL_ACTION_MESSAGE_RECALLED` (the chain's stated contract is that
   `aero_ai::governance::*` paths stay complete for consumers; the unit test
   module uses `use super::*` and needs the name in scope). **No mapping arm is
   added** — the const appears only in the `unknown_local_token_passes_through_unmapped`
   token list (§4 of the spec: the message lane is SQL-side, F5).
3. **SQL API** — `migrations/0246_message_recall_audit_governance.sql`:
   function `aero_enqueue_message_recall_audit()` + trigger
   `audit_events_message_recall_enqueue` (AFTER INSERT on `audit_events`,
   FOR EACH ROW). Exact 0245 template: `SET search_path = pg_catalog, public`,
   fail-open first line `IF NEW.action <> 'message.recalled' THEN RETURN NEW;`
   (exact token, **not** a `message.%` prefix), 1:1 row
   (`event_id = NEW.id`, status 0, `class 'message'`, explicit `priority 10`,
   16-key 0239 envelope with `action = NEW.action` verbatim,
   `source_system 'aero-im.source'`, `ON CONFLICT (event_id) DO NOTHING`),
   **no** aggregated/spill/count/window keys, **no** runtime gate, **no**
   binding lookup (0242/0245 D2 precedent). Header comment documents the
   cross-slice pins (leaf const ↔ SQL literal, `AUDIT_SOURCE_SYSTEM`
   deployment invariant, firing-order inertness).

### 2.2 Internal changes (no public surface)

- `crates/aero-storage/src/message/authorization.rs:368` — the
  `"message.recalled"` literal is spelled through
  `aero_common::model::audit::LOCAL_ACTION_MESSAGE_RECALLED`. Behavior
  identical; the recall audit row still flows through
  `AuditRepo::append_in_tx` (the single Rust-visible entry point; trigger-only
  ownership means Rust still **never writes** `audit_governance_outbox`).
- `crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs` — seeding +
  evidence loop extension (R1, §3).
- `crates/aero-storage/src/audit_governance.rs` — new db_test
  `recall_lane_outbox_parity` (R2, §3).
- `crates/aero-ai/src/governance.rs` — test token list only (§2.1.2).
- `scripts/truth-check-lib.sh` — literal family extension (§2.3).
- `scripts/test-integration.sh` — `MIGRATION_COUNT` arbiter 243 → 244 (§5).

### 2.3 CI contract changes

- `scripts/truth-check-lib.sh`: the message-lane literal family
  (`L1_TOKEN_LITERALS_CS` at :157) is extended with `'"message.recalled"'` so
  a bare literal of the token anywhere in `crates --glob '*.rs'` outside the
  leaf is a CI red (mirrors the 0242/0245 Rule 3f discipline). The SQL side
  stays unpoliced by design — the parity db_test IS the SQL pin (G2 closure).
- `scripts/b5-pin.sh`: **no change** (no slot added/removed/renamed — the new
  db_test rides the existing `audit_governance::` module filter).

### 2.4 Explicitly unchanged API surface

- `governance_lane_for(&str) -> Option<GovernanceLane>` — signature and all
  arms unchanged; `LOCAL_ACTION_MESSAGE_RECALLED` → `None` (pinned by the
  extended pass-through test; `is_admin_class` → false).
- `AuditClaimPayload` — no field changes (0246 rows must parse into the
  existing 16-key twin; `deny_unknown_fields` is the drift alarm).
- 0239 CHECKs (`class IN ('admin','message','room')`, `priority > 0`,
  `delivery_mode IN ('push')`) — **no new class, no new priority value**;
  `'message'`/10 are already admitted.
- `AuditRepo` — no signature change; `append_in_tx` remains the entry point.
- 0239/0240/0241/0242/0245 file text — untouched (sqlx checksum discipline).

## 3. Implementation plan

### 3.1 R2 — migration 0246 (message-recall lane)

File `migrations/0246_message_recall_audit_governance.sql`, 0245 template
verbatim with these substitutions:

| 0245 element | 0246 value |
|---|---|
| `aero_enqueue_room_audit` | `aero_enqueue_message_recall_audit` |
| `audit_events_room_enqueue` | `audit_events_message_recall_enqueue` |
| allowlist `NOT IN ('room.create','room.archived')` | `NEW.action <> 'message.recalled'` (single-token exact equality, 0239-style) |
| class `'room'` | `'message'` (`GOVERNANCE_CLASS_MESSAGE`) |
| priority `10` | `10` (`GOVERNANCE_PRIORITY_BACKLOG` — explicit, not the column default) |
| envelope `action = NEW.action` | `action = NEW.action` (verbatim `message.recalled`; **no** fabricated contract token) |

Header must document: cross-slice pins (leaf const `LOCAL_ACTION_MESSAGE_RECALLED`
↔ SQL literal; `AUDIT_SOURCE_SYSTEM` invariant); no L1-window interaction (0242's
allowlist is `message.create`/`message.edit` only — `message.recalled` can never
fold into a `message.batch` window, so the parity SUM side is untouched); trigger
name sorts `audit_events_governance_enqueue < audit_events_l1_aggregate <
audit_events_message_recall_enqueue < audit_events_room_enqueue <
audit_events_snaplink_delivery` and order is inert (pairwise-disjoint token
sets), and **both wrong-token copy-paste directions are behaviorally netted**:
0246 written as `<> 'message.moderated'` → parity gets 0 rows ≠ 3; 0246 firing
on `message.moderated` → `rust_produced_payload_matches_0239_envelope` half A's
`fetch_one` errors on 2 rows. **The widening tripwire is behavioral-only** —
the SQL allowlist has no static scan pin (truth-check polices only the Rust
literal family, §2.3), so the pin layer is the parity db_tests (FM3); trigger-
only ownership declaration extended with `message.recalled`; audit-row
immutability (append-only, no UPDATE path anywhere, lifecycle-only
DELETE — legal-hold-guarded `AuditRepo::sweep_before` retention sweep + daily-
partition drop in `ensure_audit_event_partitions`, both outbox-neutral; §3.3);
rollback = `DROP TRIGGER audit_events_message_recall_enqueue`.

### 3.2 R2 — leaf const + producer spelling

1. Add `LOCAL_ACTION_MESSAGE_RECALLED` to `audit.rs` (new vocabulary block).
2. Extend `room_vocabulary_consts_are_pinned` (:511) — or add a sibling pin in
   the same family — with a canonical-value assertion
   (`LOCAL_ACTION_MESSAGE_RECALLED == "message.recalled"`), mirroring the
   existing consts' assertions.
3. `authorization.rs:368` spells the token through the const.
4. `governance.rs` `pub use` chain + `unknown_local_token_passes_through_unmapped`
   token list (append `LOCAL_ACTION_MESSAGE_RECALLED` to the `[...]` array;
   assertions `governance_lane_for(...) == None` and `!is_admin_class(...)`
   then cover it automatically).

### 3.3 R2 — parity db_test

`crates/aero-storage/src/audit_governance.rs`, in the `audit_governance::`
module (the harness slot at test-integration.sh:321-331 picks it up with **no**
b5-pin change). **The test asserts ALL 16 envelope keys field-by-field** — not
just the four named in the earlier draft (action/source_system/event_id/
idempotency_key); the full expected-value table below is the contract.

**Envelope derivation (producer chain → exact expected values).** The recall
row's lifecycle: `authorization.rs:368` (`recall_locked_outboxed_in_tx`, inside
the recall tx — `message_edits` snapshot + `messages` UPDATE + blob GC +
audit append + room-event outbox) → `AuditRepo::append_in_tx` (`audit.rs:127`,
shared INSERT at :155) → `audit_events` (0007 columns; composite PK
`(id, created_at)` per 0146) → 0246 AFTER INSERT trigger →
`aero_snaplink_audit_payload` (0236:41, IMMUTABLE). The producer passes
`append_in_tx(tx, workspace, Some(actor), "message.recalled", Some(&id.to_string()),
json!({ "room_id": existing.room_id, "digest": digest }))` with
`digest = existing.searchable_text().chars().take(120).collect()`
(authorization.rs:300). Per-row expected values:

| # | key | expected value (`NEW` = the recall audit row) | anchor |
|---|---|---|---|
| 1 | `event_id` | `NEW.id::text` — `AuditId::new()` ULID-as-UUID, hyphenated text | `AuditRepo::append_on` id bind; 0239/0245 template |
| 2 | `source_system` | `'aero-im.source'` | `AUDIT_SOURCE_SYSTEM` (audit.rs:182); connector payload-guard value (client.rs `validate_delivery_payload`) |
| 3 | `event_type` | `'aero.im.security'` | `AUDIT_EVENT_TYPE` (audit.rs:234) |
| 4 | `schema_id` | `'aero.im.security'` | `AUDIT_SCHEMA_ID` (audit.rs:235) |
| 5 | `schema_version` | `1` | `AUDIT_SCHEMA_VERSION` (audit.rs:236) |
| 6 | `occurred_at` | PG jsonb spelling of `NEW.created_at` — expected computed as `SELECT (to_jsonb($1::timestamptz))::text` with surrounding quotes trimmed (room test :1815-1818 pattern), then `row.4["occurred_at"].as_str() == expected`; `NEW.created_at` = the `now_utc()` bound by `append_on` — server-stamped, single clock domain | room `occurred_at` assertion; `append_on` created_at bind |
| 7 | `actor` | `{"id": <recaller participant uuid text>, "type": "participant"}` — **recaller identity, NOT the message author**: the producer passes `Some(actor)` where `actor` is the participant whose authority executed the recall (author-within-window OR room owner/admin — the `recall_role_allowed_in_tx` gate), and the same actor is written `messages.recalled_by` + `message_edits.editor_id` in the same tx. `actor_id` is never NULL on this producer → `type:'participant'` always; the 0239 `'system'` COALESCE branch is unreachable | `Some(actor)` at :368; `recall_role_allowed_in_tx` (author-or-admin); recalled_by bind in the same tx |
| 8 | `targets` | `[{"id": <message id uuid text>, "type": "resource"}]` — **non-empty** single-element array; producer passes `Some(&id.to_string())` (the recalled message id), so the `[]` branch is unreachable. **Room-copy trap replaced**: the room test asserts `json!([])` ("target NULL → empty targets") — a verbatim copy fails loudly, must be this recall-specific expectation | `Some(&id.to_string())` at :368; `AUDIT_TARGET_TYPE_RESOURCE` (audit.rs:243) |
| 9 | `aggregate_type` | `'workspace'` | `AUDIT_AGGREGATE_TYPE` (audit.rs:237) |
| 10 | `aggregate_id` | `NEW.workspace_id::text` — workspace uuid text (the `Some(access.workspace)` param, authorization.rs:271) | `workspace` param → `workspace_id` bind |
| 11 | `action` | `NEW.action` = `'message.recalled'` **verbatim** (local token; NOT the fabricated `admin.content.flag`) | `LOCAL_ACTION_MESSAGE_RECALLED`; exact-token allowlist |
| 12 | `outcome` | `'success'` | `AUDIT_OUTCOME_SUCCESS` (audit.rs:238) |
| 13 | `payload` | `aero_snaplink_audit_payload(NEW.detail)` = sanitized `{"room_id": <room uuid text>, "digest": <first 120 chars of pre-recall searchable_text>}` — the object passes `aero_sanitize_snaplink_audit_value` unchanged (neither key matches the secret-key filter, 0236:3-38) and is ≪ the 65536-byte limit → no `omitted` object. **Room-copy trap replaced**: the room test asserts `json!({})` (detail `'{}'`) — a verbatim copy fails loudly, must be the detail object | detail json at :370-374; sanitizer 0236 |
| 14 | `data_classification` | `'confidential'` | `AUDIT_DATA_CLASSIFICATION` (audit.rs:239) |
| 15 | `retention_class` | `'security'` | `AUDIT_RETENTION_CLASS` (audit.rs:240) |
| 16 | `idempotency_key` | `NEW.id::text` = `event_id` (sink dedup) | 0239/0245 template |

Plus per-row absence asserts: **no** `aggregated`/`spill`/`count`/
`window_start`/`window_end` keys (1:1 rows never carry L1 markers; the parity
SUM side never sees them). Row columns: `event_id` 1:1 with the audit ids,
`status = 0`, `class = GOVERNANCE_CLASS_MESSAGE` (**leaf const** —
`aero-common/src/model/audit.rs:210`, importable), and `priority = 10`
**comment-pinned** to `GOVERNANCE_PRIORITY_BACKLOG` (NOT "from the leaf
consts": the const lives in `aero-ai/governance.rs:33` and aero-storage cannot
import aero-ai — the room test's comment is the precedent: `"priority 10 =
GOVERNANCE_PRIORITY_BACKLOG (comment-pinned; aero-storage cannot import
aero-ai)"` at :1793-1796).

Test body (room `:1750` structure with the recall-specific substitutions):

- Probe: `recall_trigger_migrated(p)` — `to_regprocedure('aero_enqueue_message_recall_audit()')`
  mirror of `room_trigger_migrated` (:1696); `None` ⇒ eprintln SKIP + return.
- `reset_governance_table`, `fixture`, fixed `now()` timestamp; assert
  `snaplink_commercial_runtime.enabled = FALSE` at the start (fresh-DB default;
  the suite runs on shared DBs — mirrors `message_lane_outbox_parity`'s
  re-assert). **No enforcement setup for the commit half** (0246 is gate-free —
  the unconditional-enqueue property is itself asserted, mirroring
  `room_lane_unconditional_enqueue` :2134 semantics in the same tx; with
  enforcement disabled the 0236 v1 trigger produces no v1 rows, so this test is
  v2-lane-only by design).
- Insert N (`3`) `message.recalled` rows in one tx **via
  `AuditRepo::append_in_tx` — the real producer seam, REQUIRED, not the
  `insert_audit_row_returning_id` helper**: the raw helper hardcodes detail
  `'{}'` and can never produce the recall `{room_id, digest}` payload (a
  verbatim room-copy test would silently assert `json!({})` — fail-loud, but
  wrong). Collect the returned `AuditId`s; detail = `json!({ "room_id": <fixture
  room uuid>, "digest": <fixture 120-char string> })`, actor = the fixture
  participant (the recaller). Assert the table above per row, payload parses
  into `AuditClaimPayload` (deny_unknown_fields) and is Value-equal to the raw
  payload, `action == LOCAL_ACTION_MESSAGE_RECALLED` verbatim, and the
  no-L1-marker keys.
- Rollback half (tx abort) → 0 outbox rows.
- Replay half: same-id duplicate `audit_events` INSERT with `created_at + 1s`
  (a fixed-ts duplicate would hit the composite PK `(id, created_at)` instead
  of exercising dedup — fail-loud but vacuous) → still exactly N outbox rows
  (ON CONFLICT dedup). Re-assert `enabled = FALSE` before replay: with
  enforcement on, the 0236 v1 trigger's plain INSERT raises on
  `snaplink_delivery_outbox` PK `'audit:'||id` — the v2 ON CONFLICT contract is
  what's pinned (room test's `restore_enforcement_disabled` precedent).

**Audit-row immutability (restated, 0246 header + test doc)**: `audit_events`
is **append-only** — no `UPDATE audit_events` exists anywhere (zero AFTER
UPDATE/DELETE triggers; all five triggers 0236/0239/0242/0245/0246 are AFTER
INSERT), and the only DELETE paths are **lifecycle-only**: the
legal-hold-guarded retention sweep (`AuditRepo::sweep_before`, audit.rs:64-71,
`NOT EXISTS legal_holds` guard per 0154) and the daily-partition DROP in
`ensure_audit_event_partitions` (0146/0154, `keep_days`-bounded), plus
test/drill isolation cleanup. The trail is never rewritten, and outbox rows are
never deleted while the relay runs.

### 3.4 R1 — T-11 drill per-class seeding

`crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs`:

1. After the 1:1 loop, seed two additional rows with the full-column INSERT:
   - **admin**: `class = 'admin'`, `priority = 100`
     (`GOVERNANCE_PRIORITY_MODERATION`); payload conforming 1:1 envelope
     (`event_id` = own PK, `source_system = AUDIT_SOURCE_SYSTEM`,
     `idempotency_key` = own PK — the payload guard runs before transport;
     a guard-failing row would dead and break `terminal == 0`);
   - **room**: `class = 'room'`, `priority = 10` (`GOVERNANCE_PRIORITY_BACKLOG`),
     same conforming envelope shape.
2. `let total = rows + 5;` (constant shift; all invariants are already written
   in terms of `total`). **Clamp `rows ≤ 95`**: `dispatch_batch` calls
   `claim_due` exactly once per batch with the drill's fixed
   `batch_size: 100` (relay.rs `dispatch_batch` → pg.rs `claim_due`
   `LIMIT $1`), and the drill asserts `claimed == total` ⇒ `total = rows + 5
   ≤ 100`. `AERO_AUDIT_DRILL_ROWS` is bounded in the current source; the
   drill must **document the bound in its `//!` header** and **fail loud at
   startup** (`anyhow::bail!` after the existing `> 0` filter, e.g.
   "AERO_AUDIT_DRILL_ROWS must be ≤ 95 (total = rows + 5 ≤ claim batch 100)") —
   instead of the confusing mid-run `round 1: claimed 100 rows, expected
   101` (empirically reproduced at rows=97). Default 3 is unaffected; the
   harness slot runs the default.
3. **§3.4.3 — per-key evidence reads `last_error` from the COLUMN.** Extend
   the per-round evidence loop key list from `[window_key, spill_key]` to
   `[window_key, spill_key, admin_key, room_key]`; the SELECT tuple
   `(status, attempts, payload)` gains **`class, priority, last_error`
   columns** — `last_error` MUST be read from the column (`row.last_error`),
   never from the payload: `deliver(&Claim)` never mutates `claim.payload`
   (relay.rs `deliver_claim`), and the requeue/mark_dead paths write
   `last_error = $5` / `$4` on the row only (pg.rs :202/:231; the current
   drill's `row.2["last_error"]` payload read at :243/:254 is the pre-existing
   red — `round 1: aggregated row … last_error=Null`).
   Assertions per key: `status == 0`, `attempts == round`, column
   `last_error` contains `"transport failed"`, `class`/`priority` equal the
   seeded values (per-class evidence — a seed that silently drops a shape
   fails the shifted totals). The window/spill keys keep their existing
   payload `count`/`aggregated` asserts; the admin/room keys assert the
   conforming 3-key envelope (`event_id`/`source_system`/`idempotency_key`).
4. Unchanged: exit-2 `to_regclass` probe, TRUNCATE-at-start, closed token
   endpoint, both rounds, 1.2s retry sleep, PASS line format
   (`drill: t11-pending: PASS` — the harness greps it).

### 3.5 CI literal policing + arbiter

1. `scripts/truth-check-lib.sh` :157 family gains `'"message.recalled"'`.
2. `scripts/test-integration.sh` :604: `-ne 243` → `-ne 244`, and the comment
   updates (244 = 243 landed + 0246; 0243/0244 still designed-only — the
   HANDOFF note stays for the auth slice).

## 4. Compatibility constraints (binding)

1. **No new lane values**: 0239 CHECKs are the schema-change boundary; `'message'`
   and `10` are pre-admitted. A new class/priority is a separate migration.
2. **Exact-token allowlist only**: `message.recalled`, never `message.%`
   (0245 header: "a prefix would fabricate governance claims for tokens no
   contract defined"). `message.deleted` stays excluded (R-D2; pinned by
   `user_delete_token_stays_out_of_admin_lane` and
   `non_moderation_action_passes_through_unmapped`).
3. **No L1-window merge**: `message.recalled` must stay 1:1. If it ever folded
   into the 0242 window, recall counts would corrupt `message.batch`
   create/edit aggregates and the parity SUM side. The 1:1 requirement is
   load-bearing.
4. **Trigger-only ownership**: Rust never writes `audit_governance_outbox` for
   this token; the 0246 trigger is the sole producer (extended 0245-header
   declaration). The `authorization.rs` change is a literal-spelling edit only.
5. **Landed migration text untouched** (0239/0240/0241/0242/0245 — sqlx
   checksums). The mapping is a **new** migration file; "extend the 0239
   trigger" is the known stale-analysis trap.
6. **No runtime gate, no binding lookup, no reconciler arm**: 0246 is D2 like
   0242/0245 — no disabled-window gap exists for it, so 0241 stays
   `message.moderated`-only and `governance_reconcile_backfills_disabled_window`
   (:981) is untouched.
7. **b5-pin slot list pinned**: no slot added/removed/renamed; new tests ride
   existing module filters (`audit_governance::`).
8. **`MIGRATION_COUNT` flips in the same commit as the migration file** (0245
   precedent, symmetric HANDOFF rule).
9. **Envelope discipline**: 16 keys exactly; `source_system =
   'aero-im.source'` (deployment invariant `AERO_AUDIT_SOURCE_SYSTEM`);
   own `event_id`/`idempotency_key`; no L1 marker keys on 1:1 rows.
10. **Firing order is inert** — do not "fix" trigger name ordering; token sets
    are pairwise disjoint (0239: message.moderated · 0242: message.create/
    message.edit · 0245: room.create/room.archived · 0246: message.recalled).

## 5. Failure modes and mitigations

| # | Failure mode | Mechanism | Mitigation / pin |
|---|---|---|---|
| FM1 | 0246 trigger body error aborts the recall tx (AFTER-trigger fail-closed blast radius) | Any error in the body rolls back audit INSERT + recall + outbox together | Body is sub-query-free and cast-free; only non-builtin call is `aero_snaplink_audit_payload` (0236, IMMUTABLE, already on every row's path). All outbox CHECKs admit the row (class 'message', priority 10, status 0, payload jsonb object). |
| FM2 | Payload guard deads rows | `validate_delivery_payload` (client.rs:511) rejects `source_system != config` (permanent ⇒ dead after ≤1 retry) | 0246 writes `AUDIT_SOURCE_SYSTEM`; R1 seeds carry conforming envelopes; T-11 `terminal == 0` and parity fixtures red on any mismatch. |
| FM3 | L1 window collision corrupts aggregates | **Two distinct widening mechanisms** — (a) a widened 0242 allowlist (`message.%`) would fold `message.recalled` INTO the `message.batch` window, corrupting create/edit aggregates; (b) a widened 0246 (`message.%`) would NOT fold anything — it would emit **extra 1:1 rows** for every `message.*` token (e.g. `message.deleted`), breaking exact-N count contracts. Only (a) touches the window. | (a) 0242's allowlist is landed text (sqlx-checksummed) and untouched; (b) 0246's allowlist is the exact single token. Both directions are caught **behaviorally, not statically** (no SQL-allowlist scan exists; truth-check polices only the Rust literal family): a widened 0246 trips `non_moderation_action_passes_through_unmapped` (:832, `message.deleted` ⇒ 0 rows becomes 1), `rust_produced_payload_matches_0239_envelope` (:455 half A, `fetch_one` errors on 2 rows), and `l1_window_aggregates_5_rows_to_1_outbox` (:1249, `count < 6` breaks). `message_lane_outbox_parity` (:1938) alone is widening-BLIND (its SUM side reads only aggregated/spill rows) — the scoped exact-count asserts are the net. |
| FM4 | Leaf ↔ SQL literal drift (SQL side unpoliced by truth-check) | SQL literal diverges from the const | `recall_lane_outbox_parity` recomputes every field from the leaf consts (priority 10 comment-pinned — `GOVERNANCE_PRIORITY_BACKLOG` lives in aero-ai, aero-storage cannot import it; room precedent :1793-1796) — equality red on drift (G2 closure, room-lane precedent). |
| FM5 | Stale-analysis implementation drift | Implementer "extends the 0239 trigger" or adds a Rust outbox write | AC7 static arbiter (single `aero_enqueue_message_recall_audit` definition in exactly 0246) + trigger-only ownership declaration + truth-check literal policing. |
| FM6 | Replay duplicate rows | `audit_events` composite PK `(id, created_at)` (0146:93) admits a same-id re-insert **only with a varied `created_at`** → trigger re-fires with the same `NEW.id` | `ON CONFLICT (event_id) DO NOTHING` = at-most-once per event; the parity replay half inserts the duplicate with `created_at + 1s` and asserts still-N. The v1 0236 plain INSERT would dedup fail-loud on its own PK (`'audit:'||id`) with enforcement on — the replay half re-asserts `enabled = FALSE` first (§3.3). |
| FM7 | Migration count drift | 0246 added without flipping the arbiter (or vice versa) | `MIGRATION_COUNT` arbiter fails CI with exact expected/found; same-commit rule. |
| FM8 | T-11 seed silently drops a shape | A seeding bug that skips admin/room rows — the **first trip is `claimed == total`** (the claim set ≠ the seeded set; `dispatch_batch` claims exactly once per batch with `batch_size: 100`), not `SUM(attempts)`; the shifted totals then fail every round | Constant-shift totals + per-shape evidence (status/attempts/class/priority + **column** last_error, §3.4.3) both must pass; `rows ≤ 96` startup clamp keeps `total ≤ 100`; harness `pending=[0-9]+` greps tolerate the extra rows (no harness edit needed). |
| FM9 | `message.deleted` re-widening temptation | R2's exact allowlist invites a `message.%` widening | R-D2 pins (`user_delete_token_stays_out_of_admin_lane`, `non_moderation_action_passes_through_unmapped`) are R3 verify-only and stay green. |
| FM10 | Payload guard deads R1 admin seed (FM2 variant) | Admin/room seeds without own `idempotency_key`/`event_id`/`source_system` | Seed INSERT includes all three keys (§3.4.1); drill's `terminal == 0` invariant is the tripwire. |

## 6. Migration steps (operational order)

1. **Add** `migrations/0246_message_recall_audit_governance.sql` (§3.1).
2. **`cargo build`** — migrations are compile-time embedded
   (`aero-storage/db.rs` `sqlx::migrate!("../../migrations")`); build **before**
   migrate or the new migration silently no-ops (AGENTS.md §4.2).
3. **Code changes** (§3.2–§3.4): leaf const + pins, producer spelling,
   governance.rs test list + re-export, parity db_test, T-11 drill.
4. **CI changes** (§3.5): truth-check family + `MIGRATION_COUNT` 243 → 244
   (same commit as step 1).
5. **Live verify on a throwaway DB** (AGENTS.md §4.3): `CREATE DATABASE` →
   `cargo build` → `aero-cli migrate` → run acceptance commands (§7) →
   `DROP DATABASE`. `make migrate-smoke` replays the full chain for
   fresh-deploy verification (0246 applies cleanly on top of 243).
6. **Static gates**: `cargo check --workspace` · `cargo test --workspace --lib`
   · `cargo clippy --workspace --all-targets` (no new warnings) ·
   `scripts/{truth-check,file-size-check,b5-pin}.sh` all clean.
7. **Rollback path** (if a defect escapes): hotfix migration
   `DROP TRIGGER audit_events_message_recall_enqueue` (parent-trigger drop
   clones to daily partitions); existing rows stay claimable (claim_due is
   class-agnostic); function left defined is harmless (0239/0242/0245
   precedent). The R1 drill change is a bin-only edit — revert the file.

## 7. Testable acceptance mapping

> Commands assume a throwaway migrated PG (`DATABASE_URL=<throwaway>`), per
> AGENTS.md §4.3. Supplied acceptance items preserved; residual items marked
> **[LIVE VERIFY]** for a fresh migrated PostgreSQL database; source changes are
> already present in the current tree.

> The table below is a historical acceptance transcript. Any legacy `[OPEN]`
> marker means “live database command not rerun in the current environment,”
> not a missing source implementation.

| AC | Executable check | Status |
|---|---|---|
| AC1 — enqueue coverage per class | `DATABASE_URL=<migrated throwaway> cargo test -p aero-storage --lib --locked audit_governance:: -- --ignored --test-threads=1` green (manual path runs with the AC6 closure — `--nocapture` + `! grep -q 'SKIP: 0246 not migrated'` — so the `recall_lane_outbox_parity` leg cannot vacuous-green on a stale binary/un-migrated DB): `rust_produced_payload_matches_0239_envelope` (:455, admin half A + message half B), `l1_window_aggregates_5_rows_to_1_outbox` (:1249), `message_lane_outbox_parity` (:1938), `room_lane_outbox_parity` (:1750), `room_lane_unconditional_enqueue` (:2134); harness `audit_governance::` slot (:321-331) PASS. | **[OPEN: `recall_lane_outbox_parity`]** |
| AC2 — fail-open | `cargo test -p aero-ai --lib` green: `unknown_local_token_passes_through_unmapped` (list gains `LOCAL_ACTION_MESSAGE_RECALLED`), `user_delete_token_stays_out_of_admin_lane`, `room_lane_maps_to_room_class`; storage: `non_moderation_action_passes_through_unmapped` (:832) — `message.deleted` ⇒ 0 governance rows, 1 v1 row. No trigger body raises for unmapped tokens. | Landed + R2 test-list extension |
| AC3 — claim ordering | `DATABASE_URL=<throwaway> cargo test -p aero-audit-connector --lib mixed_priority_claim_orders_moderation_first_then_fifo -- --ignored` green (10 admin later-available + 40 backlog earlier-available; claimed 25 = {10 admin} ∪ {15 earliest}); `aero-audit-priority-drill` exits 0 with moderation-in-first-batch / drain-501 / parity-501; harness slot :462-547 PASS. | Landed, verify-only |
| AC4 — T-11 per-class seeding | `DATABASE_URL=<throwaway> cargo run -p aero-audit-connector --bin aero-audit-t11-drill` exits 0, prints `drill: t11-pending: PASS`; total = N+4 with **N ≤ 96 enforced at startup** (`AERO_AUDIT_DRILL_ROWS` > 96 ⇒ fail-loud bail); rounds 1/2: `status=0 == total`, `status IN (1,2,3) == 0`, `SUM(attempts) == total×r`, transport `COUNT(last_error LIKE …) == total` (column); per-shape evidence for admin (100) and room (10) keys incl. **class/priority/last_error columns** — `last_error` read from the COLUMN, the payload never carries it (§3.4.3); harness `t11-fail-closed` (:366-453) PASS with **no harness edits** (default rows=3 ⇒ total 7 ≤ 100). | **[OPEN: R1]** |
| AC5 — reconcile parity | `DATABASE_URL=<throwaway> cargo test -p aero-storage --lib --locked governance_reconcile_backfills_disabled_window -- --ignored` green; 0241 stays `message.moderated`-only; 0246 adds no reconciler arm. | Landed, verify-only |
| AC6 — [R2] recall rows land in message lane | Same `audit_governance::` command as AC1, with the manual-path stale-binary closure: `OUT="$(DATABASE_URL=<migrated throwaway> cargo test -p aero-storage --lib --locked audit_governance:: -- --ignored --test-threads=1 --nocapture 2>&1)" && grep -q 'recall_lane_outbox_parity \.\.\. ok' <<<"$OUT" && grep -Eq 'test result: ok\. [1-9][0-9]* passed' <<<"$OUT" && ! grep -q 'SKIP: 0246 not migrated' <<<"$OUT"` — includes `recall_lane_outbox_parity`: N in-tx `message.recalled` appends via `AuditRepo::append_in_tx` → exactly N outbox rows, class from leaf const (`GOVERNANCE_CLASS_MESSAGE`), priority comment-pinned 10 (room precedent — aero-storage cannot import aero-ai), **all 16 envelope keys asserted field-by-field** (non-empty `targets=[{id,type:'resource'}]`, `payload={room_id,digest}`, `occurred_at` = `to_jsonb(NEW.created_at)` spelling, actor = recaller), action verbatim, `source_system='aero-im.source'`, no L1 marker keys, `AuditClaimPayload` parse Value-equal; `cargo test -p aero-ai --lib` green with the const in the pass-through list (`governance_lane_for` → None, `is_admin_class` → false). **`--nocapture` is load-bearing** (empirically verified against libtest): a passing test's `eprintln!` is captured and hidden by default — without `--nocapture` the SKIP grep is vacuous and the stale-binary window stays open. Closure verdict = cargo exit 0 AND the test's `... ok` line AND ≥1 passed AND no `SKIP: 0246 not migrated` (the `recall_trigger_migrated` probe's eprintln, mirroring `room_trigger_migrated` :1704 — fired ⇒ stale `aero-cli` binary or un-migrated DB ⇒ red). Harness path needs none of this: `run_migrated_integration` (:184) builds via `cargo run` before migrating (AGENTS.md §4.2), so the trigger is always present and the empty-filter guard already applies. | **[OPEN: R2]** |
| AC7 — [R2] exact-token, trigger-owned | Mechanical arbiter, 0242/0245 harness pattern (test-integration.sh:596-615/:648-655), every leg fail-loud: (1) count — `test "$(ls migrations/*.sql | wc -l)" -eq 244` (same-commit with the 0246 file; HANDOFF: if the auth slice's 0243/0244 land first, flip to the actual count, mirroring the :600-603 note); (2) single definition — `RECALL_DEFS="$(rg -l "aero_enqueue_message_recall_audit" migrations/ 2>/dev/null || true)"`; red unless exactly one file AND `grep -qx "migrations/0246_message_recall_audit_governance.sql" <<<"$RECALL_DEFS"` (absent → 0 matches → red; renumbered copy → 2 → red); (3) allowlist in the 0246 file — `grep -q "IF NEW.action <> 'message.recalled' THEN RETURN NEW;"` (byte-identical with the §2.1.3 template literal — single-line form, NOT 0239's two-line `THEN`/`RETURN NEW;` layout; layout drift false-reds fail-loud, forcing alignment) AND `! grep -qE "message\.%|LIKE 'message"` (verified empirically: trips `LIKE 'message.%'` and `LIKE 'message%'`; never false-positives the legitimate `'message.recalled'` token — ERE `%` is literal, verified against 0239/0242/0245; residual: case-lowered `like 'message%'` with no dot evades — the file's own style is uppercase, accepted; add `-i` if ever needed). No Rust outbox write — the net is the parity exact-N count (a Rust co-writer in the recall tx adds outbox rows → red) + trigger-only ownership declaration (truth-check polices only the Rust token-literal family, §2.3); `MIGRATION_COUNT` arbiter (:597-605) passes with 244. | **[OPEN: R2]** |

**Merge gates (AGENTS.md §4.3)**: `cargo build` (pre-migrate) · `cargo check
--workspace` clean · `cargo test --workspace --lib` green · `cargo clippy
--workspace --all-targets` no new warnings · `scripts/truth-check.sh`,
`scripts/file-size-check.sh`, `scripts/b5-pin.sh` clean (no slot churn).

## 8. Change surface (complete)

| File | Change |
|---|---|
| `migrations/0246_message_recall_audit_governance.sql` | **New** (R2): function + trigger, exact-token allowlist, 1:1 message-class row, 16-key envelope, `ON CONFLICT DO NOTHING`, no gate/binding |
| `crates/aero-common/src/model/audit.rs` | R2: `LOCAL_ACTION_MESSAGE_RECALLED` const + canonical-value pin (room_vocabulary family) |
| `crates/aero-storage/src/message/authorization.rs` | R2: :368 spells token through the const |
| `crates/aero-storage/src/audit_governance.rs` | R2: `recall_lane_outbox_parity` + `recall_trigger_migrated` probe |
| `crates/aero-ai/src/governance.rs` | R2: re-export + pass-through test token list; no mapping arm |
| `crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs` | R1: admin/room seed rows; current total = `rows + 5` with **`rows ≤ 95` startup clamp + docstring bound**; evidence loop gains class/priority/last-error column checks |
| `scripts/truth-check-lib.sh` | R2: literal family gains `'"message.recalled"'` |
| `scripts/test-integration.sh` | R2: `MIGRATION_COUNT` 243 → 244 (same commit) |
| `scripts/b5-pin.sh` | **No change** |
| `migrations/0239/0240/0241/0242/0245` | **No change** (landed text untouched) |
