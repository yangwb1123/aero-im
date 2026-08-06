Plan written to `docs/pi-batch/message-recall-plan.md`. Summary:

## Assessment
`pi-batch.py assess` → 处方档 **demo（S）**, workflow **L3_platform**, 产品化 **L2_platform_capability**（多租户信号）。克制规则已吸收：复用 `rooms.workspace_id` 租户边界 + `assert_room_access` 唯一守卫、事件带房间上下文、事务化审计、无新表无新 crate。评估缺失的 main_flow / data_source / error_path / tech_stack 均由本计划补全。

## Key design decisions (verified against code)
- **Recall = soft state transition, not tombstone**: `messages` + `recalled_at`/`recalled_by` (nullable), blocks → `"[此消息已被撤回]"` placeholder, `searchable_text=''`, `embedding=NULL`, `version+1`. Row/`message_id`/history retained; delete-after-recall still works.
- **Permission**: author OR room `owner`/`admin` (room roles are exactly `owner/admin/member` per `0001_init.sql`); member non-author → 403. Check order `404 → 403(tenant) → 409(deleted) → 409(recalled) → 403(role)` prevents cross-tenant existence oracles.
- **Event**: new `RoomEvent::Recalled(Message)` (tuple variant — no `kind` tag collision) → `ServerFrame::Recalled` → `{"type":"recalled","message":{...}}`; fans out via existing outbox → NATS → hub path; bus chokepoint extended for AI answer-cache invalidation.
- **Audit/history**: pre-recall blocks snapshotted into `message_edits` (editor=recaller) + `audit_log` `message.recalled` row, same transaction as the placeholder UPDATE — reuses `soft_delete_locked_outboxed_in_tx` composition; no new table.
- **Storage**: `recall_outboxed_authorized` mirrors `soft_delete_outboxed_authorized`: `resolve target → lock_effective_message_write_access → FOR UPDATE row lock → identity/state re-check → in-tx role re-check FOR UPDATE → snapshot+update+blob-GC+audit+outbox, one commit`.
- **Idempotency**: no new idempotency table — atomic `UPDATE ... WHERE recalled_at IS NULL AND deleted_at IS NULL` under row lock; duplicate recall returns **stable 409** (spec requires already-recalled as a failure path); outbox `event_id` + seq dedup for at-least-once replay.

## Persistence Design (§12 report, done before code)
Aggregate `Message` state machine (live → recalled → deleted) · 1 migration `0238` (2 nullable columns + `event_outbox_kind_check` drop+add + `messages_partitioned` mirror per 0174 convention) · identity = PK `messages.id` + immutable `(room_id, sender_id)` + no idempotency key · consistency boundary = single tx (9 steps) · no new indexes · deletion = orthogonal tombstone · additive Expand-Migrate-Contract, no backfill.

## Tests (5 layers)
Unit (pure `recall_authorized` permission matrix table-driven + serde round-trips) · Contract (frame.rs JSON shape: `type=="recalled"`, placeholder, seq) · Integration `#[ignore]` PG (full tx assertions, cross-workspace isolation, concurrency race) · Migration test (fresh DB + `information_schema` + smoke replay) · E2E smoke (two clients, WS render, 403/409 paths).

## Change radius
~18 files: 1 new migration, `Message`/`MessageRow` + ~15 `SELECT` lists (sqlx `query_as` requirement), `RoomEvent`/`ServerFrame`/`ClientFrame`/`EventOutboxKind` + all match arms, `materialize_outbox_payload` arm, service + handler + route, 3 web files, metrics. No config/deploy changes, rollback = drop 2 columns.

One decision to flag for the implementer: pre-recall content lands in `message_edits` (shows on the existing edit-history route) — deliberate reuse of the evidence table rather than a new `message_recalls` table; documented in §2 with the rationale.
