//! Shared helpers for the facade (FR-1…FR-7), room-lane (R1/R2/R5) and
//! recall-lane (R3) drills (design D2 hoist). These were private fns of
//! `facade.rs` / the parent module; they are hoisted verbatim so the room /
//! recall drills reuse the SAME seam helpers — a drift between lanes would
//! red both suites, never one.
//!
//! Load-bearing pins (do not weaken):
//!   * `cleanup_facade_rows` deletes audit rows by (`workspace_id`,
//!     `LOCAL_ACTION_*` allowlist) — bound consts, never literals. The
//!     allowlist now includes `LOCAL_ACTION_ROOM_ARCHIVED` and
//!     `LOCAL_ACTION_MESSAGE_RECALLED` (recall audit rows carry
//!     `workspace_id` — E9: `append_in_tx(tx, workspace, …)`).
//!   * `ENVELOPE_KEYS` is the single definition of the 16-key 0239 envelope
//!     list; the room/recall/moderation drills all assert it (a drift in one
//!     lane's envelope reds the others too).
//!   * `window_row_for` recomputes the 0242 md5 window PK — the R3
//!     "no-L1-folding" pin binds the RECALL audit row's `created_at` (DR-4).

use aero_common::{
    GOVERNANCE_CLASS_MESSAGE, L1_WINDOW_SECONDS, LOCAL_ACTION_MESSAGE_CREATE,
    LOCAL_ACTION_MESSAGE_EDIT, LOCAL_ACTION_MESSAGE_RECALLED, LOCAL_ACTION_MODERATED,
    LOCAL_ACTION_ROOM_ARCHIVED, LOCAL_ACTION_ROOM_CREATE,
};

use super::*;

/// The 0239 16-key envelope (`aero_enqueue_governance_audit` `jsonb_build_object`
/// list, byte-identical in 0242/0245/0246). Single definition — every
/// lane drill asserts the same list.
pub(crate) const ENVELOPE_KEYS: [&str; 16] = [
    "event_id",
    "source_system",
    "event_type",
    "schema_id",
    "schema_version",
    "occurred_at",
    "actor",
    "targets",
    "aggregate_type",
    "aggregate_id",
    "action",
    "outcome",
    "payload",
    "data_classification",
    "retention_class",
    "idempotency_key",
];

/// D3 scrub: DELETE the 0245 class-'room' outbox rows the fixture's
/// `create_room_in_workspace` enqueued in the same tx (same priority 10,
/// earlier `created_at` ⇒ sorts ahead in `claim_due`'s total order). No-op
/// when 0245 absent. Run after ALL facade writes, before any assertion.
pub(crate) async fn scrub_room_lane_rows(pool: &PgPool) {
    sqlx::query("DELETE FROM audit_governance_outbox WHERE class = 'room'")
        .execute(pool)
        .await
        .expect("scrub room-lane outbox rows");
}

/// Recompute the 0242 window PK from a server-stamped audit `created_at`.
/// Two-step copy of the drift-pinned shape (`aero-storage
/// producer/mod.rs::window_key_for`): epoch =
/// `floor(extract(epoch FROM $1::timestamptz) / 60)::bigint`, then
/// `md5(ws::text || '|' || class || '|' || epoch)::uuid`. The explicit
/// `::bigint` matches the trigger's preimage; the class and bucket bind the
/// leaf consts so a preimage drift reds loudly at the `event_id == key`
/// pins. Inline SQL required — aero-storage's test helper is unreachable.
pub(crate) async fn window_row_for(
    pool: &PgPool,
    ws: WorkspaceId,
    created_at: time::OffsetDateTime,
) -> Uuid {
    let window_epoch: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM $1::timestamptz) / $2)::bigint")
            .bind(created_at)
            .bind(L1_WINDOW_SECONDS)
            .fetch_one(pool)
            .await
            .expect("window epoch floor");
    sqlx::query_scalar("SELECT md5($1)::uuid")
        .bind(format!(
            "{}|{}|{}",
            ws.to_uuid(),
            GOVERNANCE_CLASS_MESSAGE,
            window_epoch
        ))
        .fetch_one(pool)
        .await
        .expect("recompute 0242 window key")
}

/// T-11 claim predicate (pg.rs `claim_due` WHERE verbatim): whether the row
/// is due/claimable — `true` = not lost, `false` = terminal.
pub(crate) async fn claim_predicate_holds(pool: &PgPool, event_id: Uuid) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM audit_governance_outbox
             WHERE event_id = $1
               AND status IN (0, 1)
               AND available_at <= clock_timestamp()
               AND (lease_expires_at IS NULL OR lease_expires_at <= clock_timestamp()))",
    )
    .bind(event_id)
    .fetch_one(pool)
    .await
    .expect("claim-predicate probe")
}

/// FR-7: re-park a requeued row into the due set (mirrors
/// `force_lease_expiry`; after a requeue the row is status 0, so only
/// `available_at` gates `claim_due`). Deterministic — no sleep, the
/// backdate overrides any backoff drift.
pub(crate) async fn force_re_due(pool: &PgPool, event_id: AuditId) {
    sqlx::query(
        "UPDATE audit_governance_outbox
            SET available_at = clock_timestamp() - interval '1 second',
                lease_expires_at = NULL
          WHERE event_id = $1",
    )
    .bind(event_id.to_uuid())
    .execute(pool)
    .await
    .expect("force re-due");
}

/// End-of-drill cleanup: outbox rows (window ∪ moderation ids), audit rows
/// by (workspace, `LOCAL_ACTION_*` allowlist — bound, never literals),
/// `event_outbox` rows by message ids, singleton restore. The allowlist
/// covers every audit token the facade + room + recall drills produce
/// (`message.create`/`message.edit`/`room.create`/`room.archived`/
/// `message.recalled`/`message.moderated` — recall audit rows carry
/// `workspace_id`, E9).
pub(crate) async fn cleanup_facade_rows(
    pool: &PgPool,
    ws: WorkspaceId,
    message_ids: &[MessageId],
    window_ids: &[AuditId],
    moderation_ids: &[AuditId],
) {
    let outbox_ids: Vec<Uuid> = window_ids
        .iter()
        .chain(moderation_ids)
        .map(AuditId::to_uuid)
        .collect();
    if !outbox_ids.is_empty() {
        sqlx::query("DELETE FROM audit_governance_outbox WHERE event_id = ANY($1)")
            .bind(&outbox_ids)
            .execute(pool)
            .await
            .expect("clean facade outbox rows");
    }
    sqlx::query(
        "DELETE FROM audit_events
          WHERE workspace_id = $1 AND action = ANY($2)",
    )
    .bind(ws.to_uuid())
    .bind(vec![
        LOCAL_ACTION_MESSAGE_CREATE,
        LOCAL_ACTION_MESSAGE_EDIT,
        LOCAL_ACTION_ROOM_CREATE,
        LOCAL_ACTION_ROOM_ARCHIVED,
        LOCAL_ACTION_MESSAGE_RECALLED,
        LOCAL_ACTION_MODERATED,
    ])
    .execute(pool)
    .await
    .expect("clean facade audit rows");
    let msg_ids: Vec<Uuid> = message_ids.iter().map(MessageId::to_uuid).collect();
    if !msg_ids.is_empty() {
        sqlx::query("DELETE FROM event_outbox WHERE message_id = ANY($1)")
            .bind(&msg_ids)
            .execute(pool)
            .await
            .expect("clean facade event-outbox rows");
    }
    restore_enforcement_disabled(pool).await;
}

/// Mutable-column state of one governance outbox row (settle/requeue/dead
/// assertions).
pub(crate) struct RowState {
    pub status: i32,
    pub attempts: i64,
    pub delivered_at: Option<time::OffsetDateTime>,
    pub last_error: Option<String>,
    pub claim_token: Option<Uuid>,
    pub lease_expires_at: Option<time::OffsetDateTime>,
}

pub(crate) async fn outbox_row(pool: &PgPool, event_id: AuditId) -> RowState {
    let (status, attempts, delivered_at, last_error, claim_token, lease_expires_at) =
        sqlx::query_as::<
            _,
            (
                i32,
                i64,
                Option<time::OffsetDateTime>,
                Option<String>,
                Option<Uuid>,
                Option<time::OffsetDateTime>,
            ),
        >(
            "SELECT status, attempts, delivered_at, last_error, claim_token, lease_expires_at
               FROM audit_governance_outbox WHERE event_id = $1",
        )
        .bind(event_id.to_uuid())
        .fetch_one(pool)
        .await
        .expect("governance row");
    RowState {
        status,
        attempts,
        delivered_at,
        last_error,
        claim_token,
        lease_expires_at,
    }
}
