//! Call-session repository (P3/P6).

use aero_common::{CallId, CallKind, CallMode, CallSession, ParticipantId, RoomId};
use sqlx::PgPool;

#[derive(Clone)]
pub struct CallRepo {
    pool: PgPool,
}

impl CallRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn start(
        &self,
        id: CallId,
        room: RoomId,
        initiator: ParticipantId,
        kind: CallKind,
        mode: CallMode,
        callees: &[ParticipantId],
    ) -> Result<CallSession, sqlx::Error> {
        let started_at = time::OffsetDateTime::now_utc();
        let kind_s = match kind {
            CallKind::Audio => "audio",
            CallKind::Video => "video",
        };
        let mode_s = match mode {
            CallMode::P2p => "p2p",
            CallMode::Sfu => "sfu",
        };

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"INSERT INTO call_sessions (id, room_id, initiator, kind, mode, started_at)
               VALUES ($1, $2, $3, $4, $5, $6)"#,
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(initiator.to_uuid())
        .bind(kind_s)
        .bind(mode_s)
        .bind(started_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"INSERT INTO call_participants (call_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'caller', $3) ON CONFLICT DO NOTHING"#,
        )
        .bind(id.to_uuid())
        .bind(initiator.to_uuid())
        .bind(started_at)
        .execute(&mut *tx)
        .await?;

        for callee in callees {
            sqlx::query(
                r#"INSERT INTO call_participants (call_id, participant_id, role, joined_at)
                   VALUES ($1, $2, 'callee', $3) ON CONFLICT DO NOTHING"#,
            )
            .bind(id.to_uuid())
            .bind(callee.to_uuid())
            .bind(started_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        Ok(CallSession {
            id,
            room_id: room,
            initiator,
            kind,
            mode,
            started_at,
            ended_at: None,
            end_reason: None,
        })
    }

    pub async fn end(&self, id: CallId, reason: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"UPDATE call_sessions
                  SET ended_at = NOW(), end_reason = $2
               WHERE id = $1 AND ended_at IS NULL"#,
        )
        .bind(id.to_uuid())
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_for_room(
        &self,
        room: RoomId,
        limit: i64,
    ) -> Result<Vec<CallSession>, sqlx::Error> {
        let limit = limit.clamp(1, 200);
        let rows = sqlx::query_as::<_, CallRow>(
            r#"SELECT id, room_id, initiator, kind, mode, started_at, ended_at, end_reason
               FROM call_sessions WHERE room_id = $1
               ORDER BY started_at DESC
               LIMIT $2"#,
        )
        .bind(room.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(CallSession::from).collect())
    }
}

#[derive(sqlx::FromRow)]
struct CallRow {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    initiator: uuid::Uuid,
    kind: String,
    mode: String,
    started_at: time::OffsetDateTime,
    ended_at: Option<time::OffsetDateTime>,
    end_reason: Option<String>,
}

impl From<CallRow> for CallSession {
    fn from(r: CallRow) -> Self {
        let kind = if r.kind == "video" { CallKind::Video } else { CallKind::Audio };
        let mode = if r.mode == "sfu" { CallMode::Sfu } else { CallMode::P2p };
        Self {
            id: CallId::from_uuid(r.id),
            room_id: RoomId::from_uuid(r.room_id),
            initiator: ParticipantId::from_uuid(r.initiator),
            kind,
            mode,
            started_at: r.started_at,
            ended_at: r.ended_at,
            end_reason: r.end_reason,
        }
    }
}
