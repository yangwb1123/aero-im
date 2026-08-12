//! B5-1 producer seam tests (S1–S4): the in-tx `AuditRepo::append_in_tx`
//! appends on the message send/edit and room create/archive write paths, and
//! the AFTER INSERT triggers (0242 / 0245) that materialize the governance
//! outbox rows in the SAME transaction. Three-part shape per test (Commit /
//! Rollback / Replay) + the security-boundary carve-out pins (G-SEC1).
//!
//! The suite is split by seam family so every file stays under the `800`-line
//! budget (R1 precedent): `send.rs` (message send/edit + the L1 window),
//! `room.rs` (room create/archive 1:1 lane), `carveout.rs` (audited-but-
//! unmapped / DM / group-DM boundary pins).
//!
//! Commit half runs with enforcement OFF (fresh-DB default) and NO binding —
//! proving 0242/0245 enqueue unconditionally (no runtime gate) and pinning
//! the row shape (priority 10, `source_system`, status 0).
//!
//! Naming prefix `moderation_finalize_outbox_parity_` keeps the harness slots
//! (`audit_governance::` and `moderation_finalize_outbox_parity`) non-empty.
//! Every test resets the governance table at start (order-independence within
//! a harness entry run) and re-asserts the enforcement singleton OFF where it
//! flips it on (restore at end — the singleton is GLOBAL).

use super::*;

use aero_common::{
    Block, RoomKind, GOVERNANCE_CLASS_MESSAGE, GOVERNANCE_CLASS_ROOM, LOCAL_ACTION_MESSAGE_EDIT,
    LOCAL_ACTION_ROOM_ARCHIVED, LOCAL_ACTION_ROOM_CREATE,
};

/// Fixture: workspace + owner actor + a channel room (bare
/// `RoomRepo::create_in_workspace` — NEVER the authorized path, which would
/// self-produce a `room.create` audit row and pollute the seam assertions).
async fn room_fixture(p: &PgPool, label: &str) -> (WorkspaceId, ParticipantId, RoomId) {
    let (ws, actor) = fixture(p).await;
    let room = crate::room::RoomRepo::new(p.clone())
        .create_in_workspace(
            ws,
            RoomKind::Channel,
            Some(format!("{label}-{}", uuid::Uuid::new_v4())),
            actor,
        )
        .await
        .expect("create channel room")
        .id;
    (ws, actor, room)
}

/// Seed an extra workspace member (participant + `workspace_members` edge) —
/// required by `aero_effective_workspace_access` (0185) on the DM/group-DM
/// find-or-create paths (`dm.rs` / `group_dm.rs` per-member loop), without
/// which the pair/member-set check returns `Forbidden`.
async fn enroll_member(p: &PgPool, ws: WorkspaceId, role: &str) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(id.to_uuid())
        .bind(format!("producer-member-{id}"))
        .execute(p)
        .await
        .expect("insert participant");
    sqlx::query(
        r"INSERT INTO workspace_members
              (workspace_id, participant_id, role, joined_at)
           VALUES ($1, $2, $3, now())",
    )
    .bind(ws.to_uuid())
    .bind(id.to_uuid())
    .bind(role)
    .execute(p)
    .await
    .expect("enroll workspace member");
    id
}

fn new_message(room: RoomId, sender: ParticipantId) -> crate::message::NewMessage {
    crate::message::NewMessage {
        room_id: room,
        sender_id: sender,
        blocks: vec![Block::text("b5-1-producer-seam")],
        reply_to: None,
        metadata: serde_json::Value::Null,
        expires_at: None,
    }
}

async fn audit_rows_for(p: &PgPool, ws: WorkspaceId, action: &str) -> Vec<(uuid::Uuid, String)> {
    sqlx::query_as::<_, (uuid::Uuid, String)>(
        "SELECT id, target FROM audit_events
          WHERE workspace_id = $1 AND action = $2
          ORDER BY created_at, id",
    )
    .bind(ws.to_uuid())
    .bind(action)
    .fetch_all(p)
    .await
    .expect("query audit rows")
}

/// 0242 window key recomputed from the leaf consts — the assertion target for
/// the merge test (same window key as the trigger's md5 preimage).
async fn window_key_for(
    p: &PgPool,
    ws: WorkspaceId,
    created_at: time::OffsetDateTime,
) -> uuid::Uuid {
    let window_epoch: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM $1::timestamptz) / $2)::bigint")
            .bind(created_at)
            .bind(L1_WINDOW_SECONDS)
            .fetch_one(p)
            .await
            .expect("window epoch");
    sqlx::query_scalar("SELECT md5($1)::uuid")
        .bind(format!(
            "{}|{}|{}",
            ws.to_uuid(),
            GOVERNANCE_CLASS_MESSAGE,
            window_epoch
        ))
        .fetch_one(p)
        .await
        .expect("recompute window key")
}

mod carveout;
mod room;
mod send;
