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

    /// The room a call belongs to, or `None` if the call id is unknown. Used to
    /// access-gate per-call reads (transcript / recap) against the call's room.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn room_id(&self, id: CallId) -> Result<Option<RoomId>, sqlx::Error> {
        let row: Option<(uuid::Uuid,)> =
            sqlx::query_as(r"SELECT room_id FROM call_sessions WHERE id = $1")
                .bind(id.to_uuid())
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(r,)| RoomId::from_uuid(r)))
    }

    /// Mark a call answered (first answer wins; later answers are no-ops). Lets a
    /// later [`end`](Self::end) distinguish a connected call from a missed one.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn mark_answered(&self, id: CallId) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"UPDATE call_sessions SET answered_at = NOW()
               WHERE id = $1 AND answered_at IS NULL",
        )
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// If the call exists and was **never answered**, return its initiator and the
    /// callees (the `role='callee'` participants) so the caller can drop a "missed
    /// call" notice to each. Returns `None` if the call was answered or is unknown.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the queries.
    pub async fn unanswered_callees(
        &self,
        id: CallId,
    ) -> Result<Option<(ParticipantId, Vec<ParticipantId>)>, sqlx::Error> {
        let row: Option<(uuid::Uuid,)> = sqlx::query_as(
            r"SELECT initiator FROM call_sessions WHERE id = $1 AND answered_at IS NULL",
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        let Some((initiator,)) = row else {
            return Ok(None);
        };
        let callees: Vec<(uuid::Uuid,)> = sqlx::query_as(
            r"SELECT participant_id FROM call_participants WHERE call_id = $1 AND role = 'callee'",
        )
        .bind(id.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(Some((
            ParticipantId::from_uuid(initiator),
            callees.into_iter().map(|(u,)| ParticipantId::from_uuid(u)).collect(),
        )))
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

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored call_session
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn participant(p: &PgPool, tag: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("call-{tag}-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn room(p: &PgPool, creator: ParticipantId) -> RoomId {
        let id = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1, 'direct', $2, $3, now(), $4)",
        )
        .bind(id.to_uuid())
        .bind(format!("call-room-{id}"))
        .bind(creator.to_uuid())
        .bind(uuid::Uuid::parse_str(DEFAULT_WS).expect("uuid"))
        .execute(p)
        .await
        .expect("insert room");
        id
    }

    /// An unanswered call surfaces its callees (for the "missed call" notice);
    /// once answered, it no longer does.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn call_session_missed_detection() {
        let p = pool();
        let repo = CallRepo::new(p.clone());
        let caller = participant(&p, "caller").await;
        let callee = participant(&p, "callee").await;
        let r = room(&p, caller).await;
        let call_id = CallId::new();
        repo.start(call_id, r, caller, CallKind::Audio, CallMode::P2p, &[callee])
            .await
            .expect("start call");

        // Never answered → returns (initiator, [callee]).
        let un = repo.unanswered_callees(call_id).await.unwrap().expect("unanswered");
        assert_eq!(un.0, caller, "initiator");
        assert_eq!(un.1, vec![callee], "callees");

        // Answered → no longer a missed call.
        repo.mark_answered(call_id).await.unwrap();
        assert!(
            repo.unanswered_callees(call_id).await.unwrap().is_none(),
            "answered call is not missed"
        );

        // Cleanup (call_participants cascade on call_sessions delete).
        sqlx::query("DELETE FROM call_sessions WHERE id = $1").bind(call_id.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1").bind(r.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind(vec![caller.to_uuid(), callee.to_uuid()])
            .execute(&p)
            .await
            .ok();
    }
}
