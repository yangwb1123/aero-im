# Design: B5-1 item 1 completion — `message.create/edit` in-tx audit + L1 aggregate outbox rows (0242)

> Status: proposed · Slices: B5-1 (aero-common leaf + aero-storage write points + migrations/0242 + db_tests + drill seeds) · Depends on: 0239/0240/0241 + `governance.rs` + `audit_governance.rs` + `model/audit.rs` + `aero-audit-connector/` (**all untracked — must be committed first**, §5 step 0) · Does **not** depend on: B5-2/B5-3/B5-4 (their seams are in-tree)
> Requirements: `docs/requirements/2026-08-08-aero-ai-b5-1-l1-aggregation-in-tx-audit.req.md` (153 lines)
> This design adds the only missing links: (A) audit rows on the production create/edit paths, (B) the L1 window-aggregate trigger, (C) the runtime admin-class 1:1 guard. All line numbers below are verification-time anchors; symbols are the stable grep anchors (AGENTS.md §0).

## 0. Evidence verification (untrusted claims → confirmed repo facts)

All claims in the requirements spec's evidence table were re-checked against the working tree on 2026-08-08.

| Cited claim | Verification result |
|---|---|
| `governance.rs` `governance_lane_for` :74-83 / `is_admin_class` :90-96 / `[PROPOSED]` doc :86-89 / `admin_class_rows_never_aggregated` :195-199 | ✅ **Confirmed** (drift-free). `governance_lane_for` maps ONLY `message.moderated` → `{class:'admin', priority:100, outbound:'admin.content.flag', status:0}`; every other token → `None` (pass-through). Doc at :86-89 verbatim: "R5 classification for the [PROPOSED] L1 aggregation bypass: admin-class rows must stay 1:1…". Test at :195-199 asserts `is_admin_class(LOCAL_ACTION_MODERATED)` true, `!is_admin_class("message.create")`, `!is_admin_class("room.create")`. |
| `is_admin_class`/`governance_lane_for` zero consumers beyond re-export | ✅ `rg` over `crates/` finds only `governance.rs` + `lib.rs:37-39` re-export. The classification authority for a bypass that was never built — exactly as stated. |
| `crud.rs:31` `MessageRepo::insert` — no audit write | ✅ :31-40: begin → `lock_reply_parent_in_tx` → `insert_row_in_tx` → commit; zero audit. Callers are all `#[cfg(test)]` fixtures (incl. `audit_governance.rs:98`). |
| Production create = `insert_outboxed` (idempotency.rs:158) | ✅ :158-267; called from `aero-im-core/src/service/messages.rs:216`. Single tx: room-access lock (:182) → reply-parent lock → attachment lock → `insert_row_in_tx` → event_outbox row → side-effect rows → `message_send_keys` idempotency claim → commit; concurrent loser rolls back. Idempotent early return :167-173 (no new rows). `NewMessage` (mod.rs:55-62) has **no workspace field** — workspace must be resolved from `rooms.workspace_id` in-tx. Precedent query exists: `bot/governance_tx.rs:205 resolve_room_workspace` (bot-private, `pub(super)`). |
| Production edit = `edit_outboxed_authorized` (authorization.rs:403), no audit | ✅ :403-440: begin → `resolve_message_target` (:156, returns `(room_id, sender_id)`) → `lock_effective_message_write_access` → `lock_message_in_tx` → identity/version checks → `edit_locked_outboxed_in_tx` → commit. `edit_locked_outboxed_in_tx` (events.rs:69-160) writes `message_edits` history + `RoomEvent::Edited` outbox + side effects, **zero audit append**. Also called by `edit_outboxed_system` (events.rs:47, bot/system editor path) — so the audit write must go in `edit_outboxed_authorized`, NOT in `edit_locked_outboxed_in_tx` (§2.2). |
| Audited delete precedents | ✅ `crud.rs:364 soft_delete_audited` (`message.deleted`), :398 `soft_delete_moderated` (`message.moderated`, actor=None); `events.rs:267 soft_delete_outboxed_system` → append at :337 (workspace/action `Option`, `None` ⇒ no write); `authorization.rs:368` recall. |
| `events.rs:614` rollback mirror test | ✅ `audit_failure_rolls_back_delete_and_outbox_append`: unknown `WorkspaceId::new()` → `expect_err` FK (`sqlx::Error::Database`), message retained (`deleted_at IS NULL`), no outbox delete event. AC1 rollback half mirrors this mechanism. |
| `AuditRepo::append_in_tx` :127-136 → `append_on` :140-172 | ✅ `INSERT INTO audit_events (id, workspace_id, actor_id, action, target, detail, created_at)`; `audit_events` DDL `0007_audit.sql:10`: `workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE` — unknown workspace ⇒ FK error. 0146 converts to daily RANGE partition with pre-created partitions. |
| 0239 DDL + token-keyed trigger | ✅ `event_id UUID PRIMARY KEY` (= `audit_events.id`), `status INTEGER CHECK (status IN (0,1,2,3))`, `class CHECK ('admin','message','room')`, `priority SMALLINT CHECK (>0)`, `delivery_mode CHECK ('push')`, `payload JSONB CHECK (jsonb_typeof='object')`, claim-state CHECK, `audit_governance_due_idx (available_at, created_at, event_id) WHERE status IN (0,1)`. Trigger `aero_enqueue_governance_audit()` :62-132: Gate 1 runtime (fail-open), token check `NEW.action <> 'message.moderated' → RETURN NEW` (:83-84), Gate 2 binding RAISE (fail-closed), `INSERT … ON CONFLICT (event_id) DO NOTHING`. Envelope: `event_type 'aero.im.security'`, `schema_version 1`, `occurred_at`, `actor`, `targets`, `aggregate_type 'workspace'`, `aggregate_id`, `action 'admin.content.flag'`, `idempotency_key = NEW.id::text`, `data_classification 'confidential'`, `retention_class 'security'`. |
| 0236 v1 1:1 path | ✅ `audit_events_snaplink_delivery` AFTER INSERT trigger at 0236:129-133; every audit row → 1:1 into `snaplink_delivery_outbox` (runtime-gated). **Consequence**: once this slice starts writing `message.create/edit` audit rows, they ALSO flow 1:1 to v1 — existing semantic for all audit rows, out of scope, noted §3. |
| 0241 reconciler token-keyed | ✅ `WHERE audit.action = 'message.moderated'` (0241:69, "never fabricate"). Cannot see `message.create/edit`. |
| `audit_governance.rs` parity test + fixture | ✅ `moderation_finalize_outbox_parity` :247 asserts **gov exactly 1 row**; fixture `message_in_workspace` :80-98 uses raw `repo.insert(...)` ⇒ produces no audit row ⇒ parity assertion survives this slice (write points in production paths only). Helpers: `fixture` :44, `enable_enforcement_with_binding` :121, `restore_enforcement_disabled` :170, `moderate_finalize` :184, `count_governance_rows` :203, `reset_governance_table` :232. |
| Connector shape-agnostic | ✅ `pg.rs` `claim_due`: `status IN (0,1)` + lease filter + `ORDER BY (available_at, created_at, event_id)` + `FOR UPDATE SKIP LOCKED` — no payload/event_id-shape assumptions; `event_id: AuditId::from_uuid(row.event_id)` (any 128-bit UUID). `client.rs:393-400 receipt_event_id_matches`: value-level dual format (UUID text or ULID base32) — md5-derived event_id echoes match. `relay.rs`: 403 → `mark_dead` immediately (T-11), 422/409 → ≤1 retry → dead, transient → requeue. |
| Harness pins | ✅ `scripts/b5-pin.sh:37-38` names `audit_governance::` + `moderation_finalize_outbox_parity` (empty-filter guard); `scripts/test-integration.sh:306-318` B5-1 storage entries file-gated on `migrations/0239_audit_governance_outbox.sql` (exists ⇒ un-gated); relay/T-11 drill sections :325-341 same gate; drills probe `to_regclass('audit_governance_outbox')` → absent ⇒ SKIP exit 2. |
| Migration count | ✅ `ls migrations/*.sql | wc -l` = 241; last = 0241 ⇒ **next = 0242**. `git status`: 0239/0240/0241 + `governance.rs` + `audit_governance.rs` + `model/audit.rs` + `aero-audit-connector/` all **untracked**. |
| `MESSAGE_OUTBOUND_ACTION` absent | ✅ no occurrence of `MESSAGE_OUTBOUND_ACTION`/`message.activity` in `crates/`. `model/audit.rs:150 MODERATION_OUTBOUND_ACTION = "admin.content.flag"`, :156 `LOCAL_ACTION_MODERATED = "message.moderated"`, :164-168 `GOVERNANCE_CLASS_*`, literal pins :269-273. |
| `to_regprocedure` gate precedent | ⚠️ No existing `to_regprocedure` use — drills use `to_regclass`. AC2's probe is a new pattern (spec §6); it follows the same "runtime probe ⇒ SKIP-style early return" idiom. |

**Conclusion**: every claim the spec makes about the working tree is accurate; no corrections needed beyond what the spec itself already pinned (production-path write points, fixture-preserving placement, connector zero-change).

## 1. Current state and the gap

```
audit_events (partitioned, 0146, workspace_id NOT NULL FK)
  │  AFTER INSERT ──▶ [0236] v1: every row → snaplink_delivery_outbox (1:1, runtime-gated)
  │  AFTER INSERT ──▶ [0239] v2: ONLY message.moderated → audit_governance_outbox (1:1 admin)
  │
  ├─ producers: deletes/recalls/moderations (crud.rs:364/:398, events.rs:267, authorization.rs:368) ✔ in-tx audit
  └─ message.create / message.edit  ──▶ ❌ NO audit row at all (production paths)
                                         ❌ NO L1 aggregate (window) outbox row — [PROPOSED] bypass never built
```

The classification authority (`governance.rs`), the outbox table (0239), the connector (claim/settle/dead, shape-agnostic), the harness pins, and the v1 path all exist. This design closes exactly three gaps: the audit write on the two production paths (A), the 0242 L1 aggregate trigger (B), and the runtime admin-class 1:1 guard (C).

## 2. API changes

### 2.1 aero-common — leaf constant (R4)

`crates/aero-common/src/model/audit.rs`, alongside :150:

```rust
/// [PROPOSED] Outbound contract token for the L1 window-aggregate envelope.
/// Single source of truth: 0242 trigger uses the literal + text cross-pin;
/// if the out-of-repo sink contract pins a different name, this one-line
/// leaf edit + the literal pin test (below) absorb it.
pub const MESSAGE_OUTBOUND_ACTION: &str = "message.activity";
```

Append to the literal-pin test block at :269-276: `assert_eq!(MESSAGE_OUTBOUND_ACTION, "message.activity");`.

### 2.2 aero-ai — re-export + comment landing (R4/R5)

`crates/aero-ai/src/governance.rs`:
- Add `MESSAGE_OUTBOUND_ACTION` to the `pub use aero_common::model::audit::{…}` re-export chain (mirrors `MODERATION_OUTBOUND_ACTION`).
- Update the `[PROPOSED]` doc at :86-89 and the test doc at :190-192 to reference the landed 0242 trigger (`aero_enqueue_l1_aggregate_audit`, token allowlist) instead of "proposed bypass".
- `governance_lane_for` / `is_admin_class` / all 6 unit tests: **byte-identical** (R5 — 0239 trigger + sibling auth slice depend on the mapping).

`crates/aero-ai/src/lib.rs:37-39`: add `MESSAGE_OUTBOUND_ACTION` to the existing re-export list.

### 2.3 aero-storage — production write points (R1/R2)

Three new `pub(crate)` helpers — thin, test-injectable seams (mirror the `events.rs:614` failure-injection pattern):

```rust
// crates/aero-storage/src/message/audit_tx.rs (new module; `pub mod audit_tx` + `pub use` in message/mod.rs)
// or: appended to crates/aero-storage/src/audit.rs — placement decision: message/audit_tx.rs keeps
// audit.rs (shared, other-domain) free of message types; both compile under -p aero-storage.

/// Resolve a room's workspace inside the caller's tx. rooms.workspace_id is NOT NULL
/// and immutable; unknown room ⇒ sqlx::Error::RowNotFound (NOT a FK failure — the
/// message row already exists by the time this is called on the create path, so a
/// missing room here is a logic error, surfaced as a normal tx rollback).
pub(crate) async fn resolve_room_workspace_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
) -> Result<WorkspaceId, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>("SELECT workspace_id FROM rooms WHERE id = $1")
        .bind(room.to_uuid())
        .fetch_one(&mut **tx)
        .await
        .map(WorkspaceId::from_uuid)
}

/// R1: exactly one `message.create` audit row per committed insert, same tx.
/// detail = { room_id, message_id, block_count, client_message_id? } (JSON object).
/// The ONLY call site is `insert_outboxed`; tests inject `WorkspaceId::new()` via a
/// test-only variant (or by mutating the room) to force the FK failure (AC1 rollback half).
pub(crate) async fn append_message_create_audit_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    message_id: MessageId,
    sender: ParticipantId,
    block_count: usize,
    client_message_id: Option<Uuid>,
) -> Result<AuditId, sqlx::Error> {
    let workspace = resolve_room_workspace_in_tx(tx, room).await?;
    let detail = serde_json::json!({
        "room_id": room.to_string(),
        "message_id": message_id.to_string(),
        "block_count": block_count,
        "client_message_id": client_message_id.map(|k| k.to_string()),
    });
    AuditRepo::append_in_tx(tx, workspace, Some(sender), "message.create",
                            Some(&message_id.to_string()), detail).await
}

/// R2: exactly one `message.edit` audit row per committed edit, same tx.
/// detail = { room_id, message_id, version } — version is the NEW version after
/// the locked UPDATE (edit_locked_outboxed_in_tx increments it).
pub(crate) async fn append_message_edit_audit_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    message_id: MessageId,
    editor: ParticipantId,
    version: i32,
) -> Result<AuditId, sqlx::Error> {
    let workspace = resolve_room_workspace_in_tx(tx, room).await?;
    let detail = serde_json::json!({
        "room_id": room.to_string(),
        "message_id": message_id.to_string(),
        "version": version,
    });
    AuditRepo::append_in_tx(tx, workspace, Some(editor), "message.edit",
                            Some(&message_id.to_string()), detail).await
}
```

**Call-site wiring** (both inside the existing single transaction — atomicity is free):

1. `insert_outboxed` (idempotency.rs:158): insert the audit row after `MessageSideEffectRepo::insert_in_tx` (idempotency.rs:~230) and before the `message_send_keys` claim block (idempotency.rs:~239). Order inside the tx is irrelevant to atomicity; this spot keeps the audit row adjacent to the other side-effect rows. The idempotent replay early-return (:167-173) and the concurrent-loser rollback path both leave zero audit rows (loser's row rolls back with the tx — verified behavior, no code change needed).
2. `edit_outboxed_authorized` (authorization.rs:403): after `edit_locked_outboxed_in_tx` returns `Ok(Some(edited))` (:438-445), before `tx.commit()`. `edited.message.version` is the post-increment version. **Deliberately NOT inside `edit_locked_outboxed_in_tx`** — that function is shared with `edit_outboxed_system` (events.rs:47, unfurl/transcribe bot edits); the spec scopes R2 to the user path, and bot edits remain a future slice (documented in a comment at the call site).

Also per R4: add a doc line on `crud.rs:31 insert` and `crud.rs:168 edit`: "test/internal helper; production paths `insert_outboxed` / `edit_outboxed_authorized` own in-tx audit" (D4, deferred).

### 2.4 migrations/0242 — the L1 aggregate trigger (R3)

`migrations/0242_audit_governance_l1_aggregate.sql` — new file, additive, **does not touch 0239/0240/0241 or 0236**:

```sql
-- 0242: L1 window aggregation for high-volume message.create/edit audit rows.
-- Parallel AFTER INSERT trigger on audit_events (coexists with 0236 v1 and
-- 0239 v2; neither is modified). Firing order is alphabetical by trigger name:
-- audit_events_governance_enqueue ('g') < audit_events_l1_aggregate ('l') <
-- audit_events_snaplink_delivery ('s'); irrelevant here — disjoint tokens.
-- Token allowlist (runtime admin-class 1:1 guard): ONLY message.create /
-- message.edit aggregate. message.moderated (admin class) and everything else
-- pass through untouched — admin rows must stay 1:1 (governance.rs
-- is_admin_class / admin_class_rows_never_aggregated).
-- Unconditional: no runtime gate, no binding lookup — audit delivery must not
-- be suppressed by snaplink_commercial_runtime (sibling auth slice R6), and
-- message class therefore needs NO 0241-style reconciler (token-keyed).

CREATE OR REPLACE FUNCTION aero_enqueue_l1_aggregate_audit()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    v_window_epoch BIGINT;
    v_window_start TIMESTAMPTZ;
    v_window_end   TIMESTAMPTZ;
    v_key          TEXT;
    v_event_id     UUID;
    v_count        INTEGER;
    v_inserted     INTEGER;
BEGIN
    IF NEW.action NOT IN ('message.create', 'message.edit') THEN
        RETURN NEW; -- pass-through: admin ('message.moderated'), room.*, auth.*, ""
    END IF;

    -- Fixed 60s window (spec D1). Canonical window key = epoch-seconds, which
    -- is locale/format-independent and drill-recomputable from SQL or Rust.
    v_window_epoch := floor(extract(epoch FROM NEW.created_at) / 60)::bigint;
    v_window_start := to_timestamp(v_window_epoch * 60);
    v_window_end   := to_timestamp((v_window_epoch + 1) * 60);
    v_key := NEW.workspace_id::text || '|' || 'message' || '|' || v_window_epoch::text;
    v_event_id := md5(v_key)::uuid;  -- deterministic PK; per (workspace, class, window)

    INSERT INTO audit_governance_outbox
           (event_id, status, class, priority, payload)
    VALUES (
        v_event_id, 0, 'message', 10,  -- 10 = GOVERNANCE_PRIORITY_BACKLOG (governance.rs)
        jsonb_build_object(
            'event_id', v_event_id::text,
            'event_type', 'aero.im.security',
            'schema_id', 'aero.im.security',
            'schema_version', 1,
            'occurred_at', v_window_start,
            'window_start', v_window_start,
            'window_end', v_window_end,
            'class', 'message',
            'aggregate_type', 'workspace',
            'aggregate_id', NEW.workspace_id::text,
            'action', 'message.activity',  -- MESSAGE_OUTBOUND_ACTION (leaf model/audit.rs; text cross-pin)
            'count', 1,
            'first_event_at', NEW.created_at,
            'last_event_at', NEW.created_at,
            'data_classification', 'confidential',
            'retention_class', 'security',
            'idempotency_key', v_event_id::text,
            'aggregated', true  -- shared parity-exemption key (cross-slice contract):
                                -- MUST stay at envelope TOP level (payload->>'aggregated' =
                                -- 'true'); sibling auth slice §2.6/§6 AC2 uses the SAME key
                                -- for its login-failure L1 rows. Never move into a nested
                                -- detail object or rename (auth_outbox_parity_1to1 oracle
                                -- depends on the single expression).
        )
    )
    ON CONFLICT (event_id) DO UPDATE
        SET payload = payload || jsonb_build_object(
                'count', (payload->>'count')::int + 1,
                'last_event_at', NEW.created_at
            )
        WHERE audit_governance_outbox.status = 0;  -- only the live (enqueue-state) row merges

    GET DIAGNOSTICS v_inserted = ROW_COUNT;
    IF v_inserted = 0 THEN
        -- Window row is claimed (1) / delivered (2) / dead (3): never rewrite an
        -- in-flight or terminal row (would break settle's fenced re-read).
        -- Spill: fresh event_id, count = 1, spill = true. Events are never lost.
        INSERT INTO audit_governance_outbox
               (event_id, status, class, priority, payload)
        VALUES (
            gen_random_uuid(), 0, 'message', 10,
            jsonb_build_object(
                'event_id', v_event_id::text,
                'event_type', 'aero.im.security',
                'schema_id', 'aero.im.security',
                'schema_version', 1,
                'occurred_at', NEW.created_at,
                'window_start', v_window_start,
                'window_end', v_window_end,
                'class', 'message',
                'aggregate_type', 'workspace',
                'aggregate_id', NEW.workspace_id::text,
                'action', 'message.activity',
                'count', 1,
                'first_event_at', NEW.created_at,
                'last_event_at', NEW.created_at,
                'spill', true,
                'data_classification', 'confidential',
                'retention_class', 'security',
                'idempotency_key', v_event_id::text,  -- sink dedups by this, not by row PK
                'aggregated', true  -- shared parity-exemption key (see window-row comment)
            )
        );
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS audit_events_l1_aggregate ON audit_events;
CREATE TRIGGER audit_events_l1_aggregate
    AFTER INSERT ON audit_events
    FOR EACH ROW
    EXECUTE FUNCTION aero_enqueue_l1_aggregate_audit();
```

Design points worth stating explicitly:
- **Envelope**: overlaps the 0239 family (`event_type`/`schema_id`/`schema_version`/`occurred_at`/`aggregate_*`/`action`/`data_classification`/`retention_class`/`idempotency_key`) and adds the window fields. No `actor`/`targets` objects (the window is a workspace-level count, not an actor trace) — the connector is shape-agnostic (verified §0). `source_system` omitted (0239's comes from the binding; L1 has no binding) — [PROPOSED] note: add a constant if the sink contract requires it. **Cross-slice coordination (with the sibling auth/connector slice, `2026-08-08-aero-audit-connector-b5-1-producer-side.design.md` §6 AC2/§7 D13)**:both the window row and the spill row carry top-level `'aggregated', true` — the shared parity-exemption key; the sibling's login-failure L1 rows use the identical top-level key so `auth_outbox_parity_1to1`'s outbox-side exemption is the single expression `payload->>'aggregated' = 'true'` for BOTH slices' aggregate rows.
- **`(payload->>'count')::int` cast**: safe — the trigger is the only writer of these deterministic event_ids and always writes a numeric `count`; md5-derived UUIDs cannot collide with random ULID `audit_events.id` values in practice (2^122 space).
- **No `first_event_at` update on merge** — the window's first event timestamp is preserved; `last_event_at` advances.
- **`ROW_COUNT = 0`** happens exactly when the conflict row exists with `status != 0`. The spill insert uses a fresh `gen_random_uuid()` so it can never conflict with the window row.
- **Concurrent same-window inserts**: unique index + row lock serialize; the loser's `DO UPDATE` re-reads `payload->>'count'` under the row lock ⇒ atomic convergence. PG guarantees `ON CONFLICT DO UPDATE` sees the committed/updated row state after the blocker commits.
- **0242 lands after audit rows already exist**: aggregation is go-forward only (trigger fires on new INSERTs). Old `message.create/edit` rows were never written (this slice's audit write lands with the code), and pre-0242 audit rows already flowed 1:1 to v1. No backfill — documented limitation, consistent with D2 (no message-class reconciler).

### 2.5 Connector, harness, web — **zero changes**

- `aero-audit-connector`: no code edits (R6). Drill seed sets extended only (§6 AC4).
- `scripts/b5-pin.sh` / `scripts/test-integration.sh`: no new entries; AC4 reuses the 0239 file gate; new db_tests self-gate via `to_regprocedure` (§6 AC2).
- `web/`: no WS frames involved; nothing to change.

## 3. Compatibility constraints

| # | Constraint | Enforced by |
|---|---|---|
| C1 | 0239/0240/0241 DDL + triggers byte-identical | New migration is additive; 0242 `DROP TRIGGER IF EXISTS` names only its own trigger. Cross-pin comment in 0242 cites the 0239 names. |
| C2 | `governance_lane_for`/`is_admin_class` + 6 unit tests byte-identical | R5; only doc comments change (`[PROPOSED]` → landed reference). |
| C3 | Existing 5 `audit_governance.rs` db_tests stay green — esp. parity "gov exactly 1 row" | Audit write points are in `insert_outboxed`/`edit_outboxed_authorized`; the parity fixture uses raw `MessageRepo::insert` (audit_governance.rs:98) ⇒ no audit rows ⇒ no 0239/0242 trigger firing. Token allowlist excludes `message.moderated` (parity) and `message.deleted` (`non_moderation_action_passes_through_unmapped`). |
| C4 | Connector state machine unchanged: claim → POST → settle / requeue / dead; 403 ⇒ immediate dead; Idempotency-Key = `AuditId` Display of any 128-bit UUID; receipt echo value-level | Verified shape-agnostic (§0). Aggregate rows carry `status 0`/`priority 10`/object payload ⇒ pass 0239 CHECKs. |
| C5 | v1 0236 path untouched and still 1:1 | This slice adds audit rows that *also* flow to v1 (all audit rows do). v1 sees new `action` values (`message.create`/`message.edit`) passed through verbatim — documented; v1 sink contract is out-of-repo. |
| C6 | 0241 reconciler unaffected | Token-keyed to `message.moderated` only; message class has no disabled window (unconditional trigger, D2) ⇒ no reconciler extension. |
| C7 | aero-storage never imports aero-ai | Action literal `'message.activity'` + text cross-pin comment in 0242 (existing discipline, mirror of 0239's `'admin.content.flag'`). |
| C8 | No new outbox columns/indexes; due-index compatible | Aggregate rows use defaults (`available_at`/`created_at` = `clock_timestamp()`), so `audit_governance_due_idx` ordering still works. |
| C9 | Harness pins un-moved | `b5-pin.sh:37-38` names unchanged; new tests live under the existing `audit_governance::` filter or `message::idempotency` filter. |
| C10 | Raw `MessageRepo::insert`/`edit` stay unaudited (D4) | All callers are `#[cfg(test)]` fixtures; reversing D4 later requires updating parity's "exactly 1 row" assertion (documented at both crud.rs doc lines and in §7). |
| C11 | Idempotent replay never double-audits | Early return at idempotency.rs:167-173 happens before the tx opens ⇒ no audit row, no count increment (AC1 idempotent leg). Concurrent loser rolls back its audit row with the tx. |

## 4. Failure modes

| # | Failure | Mechanism | Consequence & mitigation |
|---|---|---|---|
| F1 | `audit_events` INSERT fails (e.g. unknown workspace → FK; partition gap) | `append_*_audit_in_tx` returns `Err` inside the open tx | **Fail-closed by design (AC1)**: entire tx rolls back — message row, outbox row, side effects, aggregate upsert all gone. This is the spec-mandated semantics ("no message row without its audit row"), mirroring `events.rs:614`. Availability mitigation: 0146 pre-creates daily partitions (tiny insert-failure window); 0235 metering trigger is the same-shape precedent. |
| F2 | 0242 trigger body errors (PL/pgSQL exception) | Any exception in an AFTER INSERT trigger aborts the triggering INSERT and thus the whole tx | Same rollback as F1. Mitigation: the body has no RAISE, no runtime lookups, no binding — only two INSERT/UPDATE statements with `ON CONFLICT`; the only cast (`(payload->>'count')::int`) is safe because the trigger is the sole writer of these keys (documented in migration comment). |
| F3 | Concurrent same-window inserts | Two tx fire the trigger for the same (workspace, window) | Unique index on `event_id` + row lock serialize: loser blocks, then `DO UPDATE` re-reads the count under the lock ⇒ atomic increment, no lost updates. No test (mechanism-guaranteed, spec D4). |
| F4 | Event arrives after window row is claimed/delivered/dead (`status != 0`) | `ON CONFLICT DO UPDATE … WHERE status = 0` matches 0 rows ⇒ `ROW_COUNT = 0` ⇒ spill row (fresh uuid, `count = 1`, `spill = true`) | Event never lost; in-flight/terminal rows never rewritten (settle's fenced re-read stays valid). Spill rows are ordinary outbox rows (claim/settle/dead identical). **Bounded amplification note**: in the worst case (relay actively draining during a hot window) each post-claim event creates its own spill row; the sink dedups by `idempotency_key` (= window key) so duplicates collapse on replay; windows close after 60s ⇒ the tail is short. Accepted (spec D5). |
| F5 | Concurrent spill creation (two events both see `ROW_COUNT = 0`) | Both insert fresh-uuid spill rows | Two spill rows for one window: harmless — no loss, extra delivery, sink dedups by `idempotency_key`. |
| F6 | Connector outage / sink 403 on aggregate rows | Normal claim/settle machinery | 403 ⇒ `mark_dead` immediately (T-11); 422/409 ⇒ ≤1 retry ⇒ dead; transient ⇒ requeue. Identical for 1:1 and aggregate shapes (AC4). Dead rows stay dead (0241 semantics; no resurrection). |
| F7 | Relay delivery of an aggregate row after the window it summarizes | N/A — payload is a self-contained snapshot (count + window timestamps, no event-id references) | No dangling references after `audit_events` retention sweep (R8). Aggregate rows are compact durable records not subject to audit retention. |
| F8 | 0242 not yet migrated while code writes audit rows | Trigger absent ⇒ no aggregate rows | Correct degradation: audit rows are the durable fact; aggregation is derived and go-forward. 0241's reconciler does not (and must not) backfill message class. Ordering in §5 step 5 (build → migrate) prevents the silent-no-op footgun. |
| F9 | `resolve_room_workspace_in_tx` misses the room (logic error) | `fetch_one` ⇒ `RowNotFound` error ⇒ rollback | Correct: a message row existing without its room is an invariant violation; rollback surfaces it. (Deliberately not FK-based — the message row is already inserted by then.) |
| F10 | `audit_events` retention sweep races the trigger | Sweep is set-based soft-delete/partition-drop on old rows; trigger only fires on INSERT | No interaction: sweep touches old rows; trigger only reads `NEW`. Legal-hold guard already skips preserved rows (audit.rs:64). |

## 5. Migration steps (sequenced)

> Per AGENTS.md §4.2: migrations are compile-time embedded — **always `cargo build` before `aero-cli migrate`**.

1. **Step 0 — commit the untracked foundation (BLOCKING, risk ①)**: `git add` + commit 0239/0240/0241, `crates/aero-ai/src/governance.rs`, `crates/aero-storage/src/audit_governance.rs`, `crates/aero-common/src/model/audit.rs`, `crates/aero-audit-connector/`, harness/doc changes. A later `git reset --hard master` must not be able to delete the B5-1 base.
2. **Step 1 — leaf**: aero-common `MESSAGE_OUTBOUND_ACTION` + pin test (§2.1). `cargo check -p aero-common` + `cargo test -p aero-common --lib`.
3. **Step 2 — mapping**: aero-ai re-export + `[PROPOSED]` comment landing (§2.2). `cargo test -p aero-ai --lib` (6 governance tests still green).
4. **Step 3 — storage write points**: new `message/audit_tx.rs` helpers + wiring into `insert_outboxed` and `edit_outboxed_authorized` (§2.3); crud.rs doc notes (D4). `cargo check -p aero-storage`; all existing non-ignored tests green (audit write must not affect non-PG tests — it's behind `pub(crate)` helpers only).
5. **Step 4 — migration 0242**: write `migrations/0242_audit_governance_l1_aggregate.sql` (§2.4). **`cargo build`** (embeds migrations into the binary), then on a **throwaway DB** (`CREATE DATABASE` → `aero-cli migrate`): `make migrate-smoke` replays the full chain to prove fresh-deploy.
6. **Step 5 — db_tests AC1/AC2/AC3** in `audit_governance.rs` (+ create-leg in `message/idempotency.rs` if preferred), all `#[ignore = "requires live Postgres"]` + `DATABASE_URL`-gated, AC2 tests self-gating on `SELECT to_regprocedure('aero_enqueue_l1_aggregate_audit()')` (§6). Run with `cargo test -p aero-storage --lib -- --ignored` on the throwaway DB; `DROP DATABASE` after.
7. **Step 6 — drill seeds AC4**: extend the seed sets in `crates/aero-audit-connector/src/bin/aero-audit-relay-drill.rs` and `aero-audit-t11-drill.rs` with mixed shapes (1:1 v4-uuid rows + md5-derived aggregate rows); existing assertions unchanged.
8. **Step 7 — full gates (AC5)**: `cargo check --workspace` · `cargo test --workspace --lib` (+ `-- --ignored` on throwaway DB) · `cargo clippy --workspace --all-targets` (zero new warnings) · `scripts/{truth-check,file-size-check,web-check}.sh` · `scripts/test-integration.sh` B5 segment (audit_governance::, moderation_finalize_outbox_parity, a3-relay-drill, t11-fail-closed, moderation-priority-drill) all PASS.

## 6. Testable acceptance mapping

| AC (req §5) | Test artifact | Location | Key assertions | Command |
|---|---|---|---|---|
| **AC1 commit half (create)** | `create_audits_in_tx_and_aggregates_once` (new, `#[ignore]`) | `audit_governance.rs` (or `message/idempotency.rs`) | After `insert_outboxed` commit: `audit_events` exactly 1 row (`action='message.create'`, `target` = message id, `actor` = sender, workspace = room's); `audit_governance_outbox` exactly 1 row (`status=0`, `class='message'`, `priority=10`, `payload->>'count'='1'`, `idempotency_key` = event_id::text). | `cargo test -p aero-storage --lib -- --ignored audit_governance::` (throwaway DB) |
| **AC1 commit half (edit)** | same test, second phase | same | `edit_outboxed_authorized` → exactly 1 `message.edit` row (`detail->>'version'` = 2) + same-window aggregate `count='2'`. | same |
| **AC1 rollback half (create)** | `create_audit_failure_rolls_back_message_and_aggregate` (new) | same | Open tx → `insert_row_in_tx` → call helper with `WorkspaceId::new()` → `expect_err` (`sqlx::Error::Database`, FK) → rollback → message row absent, `COUNT(audit_events)=0`, `count_governance_rows` unchanged (**aggregate upsert rolled back**). Mechanism mirrors `events.rs:614` (unknown workspace ⇒ FK on `audit_events.workspace_id`). | same |
| **AC1 rollback half (edit)** | `edit_audit_failure_rolls_back_version_and_aggregate` (new) | same | Edit in-tx with unknown workspace → error → version unchanged (still 1), no audit row, aggregate count unchanged. | same |
| **AC1 idempotent leg** | same test, third phase | same | Replay `insert_outboxed` with same `client_message_id` → `MessageInsertOutcome::Created`-free replay path (existing behavior) → no new audit row, count unchanged. | same |
| **AC2 L1 drill** | `l1_window_aggregates_five_creates_into_one_row` (new, `to_regprocedure`-gated) | `audit_governance.rs` | N=5 production-path messages, one 60s window, one workspace: exactly **1** outbox row; `event_id` = SQL-recomputed `md5(workspace||'\|'||'message'||'\|'||epoch/60)::uuid`; `count='5'`; `COUNT(outbox) < COUNT(audit_events)` (1 < 5); second workspace ⇒ second row; same-window `message.edit` ⇒ `count=6`; direct-SQL audit row with `created_at` in previous minute ⇒ second window row (recomputed key), both windows coexist. | `cargo test -p aero-storage --lib -- --ignored audit_governance::` |
| **AC3 admin-class 1:1 runtime pin** | `moderate_amid_aggregate_stays_1_1` (new) + existing unit test | `audit_governance.rs` + `governance.rs:195` | (a) N creates (aggregate count=N) then `moderate_finalize` (helper :184): exactly 1 admin row (`event_id = audit_events.id` 1:1, class 'admin', priority 100, status 0) AND aggregate row untouched (count still N, no extra row, no increment); (b) direct-SQL audit rows per token `message.moderated`/`message.deleted`/`room.create`/`auth.login`/`""` → zero aggregate rows created/incremented. | same |
| **AC4 relay drill (mixed shapes)** | seed extension in `aero-audit-relay-drill.rs` (existing bin) | `crates/aero-audit-connector/src/bin/` | Seeds now mix 1:1 (v4 uuid event_id) + aggregate (md5 event_id, count payload): all rows `status=2`; existing `COUNT(status=2)==N` + event_id set-parity assertions cover both; `Idempotency-Key` = `AuditId` Display of the md5 event_id echoed by the stub sink and validated value-level by `validate_audit_receipt` — any mismatch ⇒ permanent ⇒ dead ⇒ parity red. | `scripts/test-integration.sh` (A3 section, 0239 file gate) |
| **AC4 T-11 drill (mixed shapes)** | seed extension in `aero-audit-t11-drill.rs` | same | Relay absent (403) with mixed shapes: both shapes `mark_dead` (status 3), zero delivered, per-attempt `attempts` evidence, `last_error` transport. Fail-closed identical for both shapes. | `scripts/test-integration.sh` (T-11 section) |
| **AC5 full gates** | regression sweep | repo-wide | All existing suites green (governance 6 unit tests byte-identical; audit_governance 5 db_tests byte-identical); clippy zero new warnings; truth-check/file-size-check/web-check 0 violations; test-integration.sh B5 segment all PASS. | §5 step 7 command list |

**Gate note (AC2)**: new db_tests probe `SELECT to_regprocedure('aero_enqueue_l1_aggregate_audit()')` and early-return if NULL, so `audit_governance::` harness entries never go red before 0242 lands — same idiom as the drills' `to_regclass` probes (verified §0).

## 7. Risks / decision points (additions to req §7)

- **D6 (audit helper module placement)** — [PROPOSED] default: new `message/audit_tx.rs` under aero-storage. Alternative (append to `audit.rs`) mixes message types into the shared audit repo; either compiles, both `pub(crate)`. Chosen: dedicated module keeps `audit.rs` domain-clean.
- **D7 (window key encoding)** — [PROPOSED] default: `md5(workspace_id::text || '|' || class || '|' || floor(epoch/60)::bigint::text)::uuid`. Rationale: epoch-seconds is locale/format-independent (a `to_char` format string could drift across PG locale settings), trivially recomputable in tests and drills, and stable across re-runs of the same minute. If the out-of-repo sink contract needs a human-readable window key, this is a one-function change + AC2 recomputation update (same "single source" discipline as D3).
- **D8 (edit-path system edits)** — bot edits (`edit_outboxed_system`) are deliberately NOT audited by this slice (write point in `edit_outboxed_authorized` only, per R2). If a later slice audits system edits, the write moves into `edit_locked_outboxed_in_tx` and AC2's `count=6` mixing leg is where the behavior is pinned. Documented at the call site.
- **Risk ⑤ (v1 row amplification)** — every new `message.create/edit` audit row also enters v1 `snaplink_delivery_outbox` (0236 trigger, 1:1). Volume on the v1 lane grows with message volume. Out of scope (v1 is the legacy lane; cutover semantics are pinned by parity test Half 4), but flagged for the ops review: if v1 relay volume is a concern, the 0236 runtime gate already exists as a suppression knob.
- **Risk ⑥ (trigger body regression)** — F2 makes the L1 trigger a write-path dependency: any future edit that makes the body raise converts to message-send outages (fail-closed). Mitigation: the migration comment pins the no-RAISE / no-lookup / ON-CONFLICT-only contract; code review gate for 0242 changes.
- All other risks (D1-D5, risks ①-④) are carried verbatim from the requirements spec §7; nothing in this design overturns them.

## 8. Verification summary

Every evidence claim checked in §0 was confirmed against the working tree; the requirements spec needs no corrections. The design preserves: 0239/0240/0241 byte-identical, `governance_lane_for`/`is_admin_class` byte-identical, all 5 existing db_tests + 6 unit tests byte-identical, connector zero code change, harness zero new entries. New surface: one leaf constant, three `pub(crate)` storage helpers + two call-site lines each, one additive migration (0242), ~6 new db_tests, two drill seed extensions.
