//! Call-session repository (P3/P6).

use aero_common::{CallId, CallKind, CallMode, CallSession, Error, ParticipantId, RoomId};
use sqlx::{PgPool, Postgres, Transaction};

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
            r"INSERT INTO call_sessions (id, room_id, initiator, kind, mode, started_at)
               VALUES ($1, $2, $3, $4, $5, $6)",
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
            r"INSERT INTO call_participants (call_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'caller', $3) ON CONFLICT DO NOTHING",
        )
        .bind(id.to_uuid())
        .bind(initiator.to_uuid())
        .bind(started_at)
        .execute(&mut *tx)
        .await?;

        for callee in callees {
            sqlx::query(
                r"INSERT INTO call_participants (call_id, participant_id, role, joined_at)
                   VALUES ($1, $2, 'callee', $3) ON CONFLICT DO NOTHING",
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

    /// Start a user-authored call under the same transaction that proves the
    /// initiator's effective room access and snapshots the callee roster.
    ///
    /// The authorization helper owns the global workspace -> room -> membership
    /// lock order. Room membership revocation therefore either commits first
    /// and rejects this write, or waits until the complete call aggregate has
    /// committed. Direct-call block state is checked before any row is inserted.
    pub async fn start_authorized(
        &self,
        id: CallId,
        room: RoomId,
        initiator: ParticipantId,
        kind: CallKind,
        mode: CallMode,
    ) -> aero_common::Result<(CallSession, Vec<ParticipantId>)> {
        self.start_authorized_inner(id, room, initiator, kind, mode, true)
            .await
    }

    /// Create the durable root for a server-minted group-call id.
    ///
    /// Unlike an ordinary invite, this records only the creator. Other room
    /// members become active legs when they explicitly join.
    pub async fn start_group_authorized(
        &self,
        id: CallId,
        room: RoomId,
        initiator: ParticipantId,
        kind: CallKind,
    ) -> aero_common::Result<CallSession> {
        let (call, _) = self
            .start_authorized_inner(id, room, initiator, kind, CallMode::Sfu, false)
            .await?;
        Ok(call)
    }

    async fn start_authorized_inner(
        &self,
        id: CallId,
        room: RoomId,
        initiator: ParticipantId,
        kind: CallKind,
        mode: CallMode,
        include_room_members: bool,
    ) -> aero_common::Result<(CallSession, Vec<ParticipantId>)> {
        let mut tx = self.pool.begin().await?;
        let room_kind = lock_effective_call_room(&mut tx, room, initiator).await?;

        let mut callees = if include_room_members {
            sqlx::query_scalar::<_, uuid::Uuid>(
                r"SELECT participant_id
                    FROM room_members
                   WHERE room_id = $1
                     AND participant_id <> $2
                   ORDER BY participant_id",
            )
            .bind(room.to_uuid())
            .bind(initiator.to_uuid())
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(ParticipantId::from_uuid)
            .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        callees.sort_unstable_by_key(aero_common::ParticipantId::to_uuid);

        if room_kind == "direct" && direct_call_is_blocked(&mut tx, initiator, &callees).await? {
            return Err(Error::Forbidden("blocked".into()));
        }

        let started_at = time::OffsetDateTime::now_utc();
        insert_call_aggregate(
            &mut tx, id, room, initiator, kind, mode, &callees, started_at,
        )
        .await?;
        tx.commit().await?;

        Ok((
            CallSession {
                id,
                room_id: room,
                initiator,
                kind,
                mode,
                started_at,
                ended_at: None,
                end_reason: None,
            },
            callees,
        ))
    }

    /// Authorize an active call operation without changing durable state.
    ///
    /// This is the transaction fence for ICE/offer/caption/roster relay and WS
    /// preflight. `recipient`, when present, must also retain effective room
    /// access and an active call leg.
    pub async fn authorize_active(
        &self,
        id: CallId,
        actor: ParticipantId,
        expected_room: RoomId,
        required_mode: Option<CallMode>,
        recipient: Option<ParticipantId>,
    ) -> aero_common::Result<CallSession> {
        let mut tx = self.pool.begin().await?;
        let call = lock_authorized_active_call(
            &mut tx,
            id,
            actor,
            expected_room,
            required_mode,
            recipient,
        )
        .await?;
        tx.commit().await?;
        Ok(call)
    }

    /// Authorize a prospective SFU joiner against an active canonical call.
    pub async fn authorize_joinable(
        &self,
        id: CallId,
        actor: ParticipantId,
        expected_room: RoomId,
        required_mode: CallMode,
    ) -> aero_common::Result<CallSession> {
        let mut tx = self.pool.begin().await?;
        let call =
            lock_authorized_joinable_call(&mut tx, id, actor, expected_room, required_mode).await?;
        tx.commit().await?;
        Ok(call)
    }

    /// Stamp an answer while access, both active legs, and call lifecycle are
    /// held by one transaction.
    pub async fn answer_authorized(
        &self,
        id: CallId,
        actor: ParticipantId,
        recipient: ParticipantId,
        expected_room: RoomId,
    ) -> aero_common::Result<CallSession> {
        let mut tx = self.pool.begin().await?;
        let call =
            lock_authorized_active_call(&mut tx, id, actor, expected_room, None, Some(recipient))
                .await?;
        sqlx::query(
            "UPDATE call_sessions
                SET answered_at = COALESCE(answered_at, NOW())
              WHERE id = $1",
        )
        .bind(id.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(call)
    }

    /// Commit the active-to-ended transition under the actor's effective room
    /// access lock. Only one concurrent End can pass the locked lifecycle check.
    pub async fn end_authorized(
        &self,
        id: CallId,
        actor: ParticipantId,
        expected_room: RoomId,
        reason: &str,
    ) -> aero_common::Result<CallSession> {
        let mut tx = self.pool.begin().await?;
        let call =
            lock_authorized_active_call(&mut tx, id, actor, expected_room, None, None).await?;
        sqlx::query(
            "UPDATE call_sessions
                SET ended_at = NOW(), end_reason = $2
              WHERE id = $1",
        )
        .bind(id.to_uuid())
        .bind(reason)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(call)
    }

    /// Atomically mark the authenticated participant's active leg as left.
    pub async fn leave_authorized(
        &self,
        id: CallId,
        actor: ParticipantId,
        expected_room: RoomId,
    ) -> aero_common::Result<CallSession> {
        let mut tx = self.pool.begin().await?;
        let call = lock_authorized_active_call(
            &mut tx,
            id,
            actor,
            expected_room,
            Some(CallMode::Sfu),
            None,
        )
        .await?;
        sqlx::query(
            "UPDATE call_participants
                SET left_at = NOW()
              WHERE call_id = $1
                AND participant_id = $2
                AND left_at IS NULL",
        )
        .bind(id.to_uuid())
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(call)
    }

    /// Mark one exact authenticated SFU leg generation as left.
    ///
    /// Authorization and the generation CAS share the call lifecycle lock, so
    /// the caller may publish Leave only after proving this is still the
    /// current transport incarnation.
    pub async fn leave_authorized_generation(
        &self,
        id: CallId,
        actor: ParticipantId,
        expected_room: RoomId,
        expected_generation: i64,
    ) -> aero_common::Result<CallSession> {
        let mut tx = self.pool.begin().await?;
        let call = lock_authorized_active_call(
            &mut tx,
            id,
            actor,
            expected_room,
            Some(CallMode::Sfu),
            None,
        )
        .await?;
        let result = sqlx::query(
            r"UPDATE call_participants
                  SET left_at = NOW()
                WHERE call_id = $1
                  AND participant_id = $2
                  AND leg_generation = $3
                  AND left_at IS NULL",
        )
        .bind(id.to_uuid())
        .bind(actor.to_uuid())
        .bind(expected_generation)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(Error::Conflict("stale call participant generation".into()));
        }
        tx.commit().await?;
        Ok(call)
    }

    /// Admit or reconnect one SFU participant under commit-time room access.
    pub async fn join_participant_authorized(
        &self,
        id: CallId,
        participant: ParticipantId,
        expected_room: RoomId,
        expected_kind: CallKind,
    ) -> aero_common::Result<CallSession> {
        let (call, _) = self
            .join_participant_authorized_generation(id, participant, expected_room, expected_kind)
            .await?;
        Ok(call)
    }

    /// Admit or reconnect one SFU participant and mint a durable leg
    /// generation under the same commit-time authorization fence.
    ///
    /// A new participant starts at generation 1. Every reconnect, including
    /// one whose previous leg is still active, advances the generation so
    /// delayed cleanup from an older transport cannot close the current leg.
    pub async fn join_participant_authorized_generation(
        &self,
        id: CallId,
        participant: ParticipantId,
        expected_room: RoomId,
        expected_kind: CallKind,
    ) -> aero_common::Result<(CallSession, i64)> {
        let mut tx = self.pool.begin().await?;
        let call =
            lock_authorized_joinable_call(&mut tx, id, participant, expected_room, CallMode::Sfu)
                .await?;
        if call.kind != expected_kind {
            return Err(Error::Conflict("call kind does not match".into()));
        }
        let generation = sqlx::query_scalar::<_, i64>(
            r"INSERT INTO call_participants
                  (call_id, participant_id, role, joined_at, left_at, leg_generation)
              VALUES ($1, $2, $3, NOW(), NULL, 1)
         ON CONFLICT (call_id, participant_id)
         DO UPDATE SET joined_at = EXCLUDED.joined_at,
                       left_at = NULL,
                       leg_generation = call_participants.leg_generation + 1
         RETURNING leg_generation",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(if call.initiator == participant {
            "caller"
        } else {
            "member"
        })
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((call, generation))
    }

    pub async fn end(&self, id: CallId, reason: &str) -> Result<(), sqlx::Error> {
        self.end_if_active(id, reason).await?;
        Ok(())
    }

    /// Atomically end an active call.
    ///
    /// Returns `true` only for the caller that won the active-to-ended
    /// transition. This lets signaling fail closed when a concurrent end
    /// already committed instead of publishing a second, stale lifecycle
    /// event.
    pub async fn end_if_active(&self, id: CallId, reason: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE call_sessions
                  SET ended_at = NOW(), end_reason = $2
               WHERE id = $1 AND ended_at IS NULL",
        )
        .bind(id.to_uuid())
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn list_for_room(
        &self,
        room: RoomId,
        limit: i64,
    ) -> Result<Vec<CallSession>, sqlx::Error> {
        let limit = limit.clamp(1, 200);
        let rows = sqlx::query_as::<_, CallRow>(
            r"SELECT id, room_id, initiator, kind, mode, started_at, ended_at, end_reason
               FROM call_sessions WHERE room_id = $1
               ORDER BY started_at DESC
               LIMIT $2",
        )
        .bind(room.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(CallSession::from).collect())
    }

    /// Fetch one call session by id.
    ///
    /// The WS SFU signaling path uses this to ensure server-owned media is only
    /// started for an active `mode = 'sfu'` call in the claimed room; existing
    /// P2P/mesh relay frames remain untouched.
    pub async fn get(&self, id: CallId) -> Result<Option<CallSession>, sqlx::Error> {
        let row = sqlx::query_as::<_, CallRow>(
            r"SELECT id, room_id, initiator, kind, mode, started_at, ended_at, end_reason
               FROM call_sessions WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(CallSession::from))
    }

    /// Whether `participant` belongs to the persisted call roster.
    ///
    /// Direct-call initiators/callees are recorded when the call starts. Group
    /// calls additionally use the live SFU router for joined participants.
    pub async fn is_participant(
        &self,
        id: CallId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar::<_, bool>(
            r"SELECT EXISTS(
                   SELECT 1 FROM call_participants
                    WHERE call_id = $1
                      AND participant_id = $2
                      AND left_at IS NULL
               )",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&self.pool)
        .await
    }

    /// Return the generation of the participant's current active leg.
    ///
    /// Ended calls and legs with `left_at` set have no current generation.
    pub async fn current_participant_generation(
        &self,
        id: CallId,
        participant: ParticipantId,
    ) -> Result<Option<i64>, sqlx::Error> {
        sqlx::query_scalar(
            r"SELECT legs.leg_generation
                FROM call_participants AS legs
                JOIN call_sessions AS calls ON calls.id = legs.call_id
               WHERE legs.call_id = $1
                 AND legs.participant_id = $2
                 AND calls.ended_at IS NULL
                 AND legs.left_at IS NULL",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await
    }

    /// Check an exact generation and active/inactive leg state on a canonical
    /// call that has not ended.
    pub async fn participant_generation_matches(
        &self,
        id: CallId,
        participant: ParticipantId,
        generation: i64,
        active: bool,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            r"SELECT EXISTS(
                   SELECT 1
                     FROM call_participants AS legs
                     JOIN call_sessions AS calls ON calls.id = legs.call_id
                    WHERE legs.call_id = $1
                      AND legs.participant_id = $2
                      AND legs.leg_generation = $3
                      AND calls.ended_at IS NULL
                      AND (legs.left_at IS NULL) = $4
               )",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(generation)
        .bind(active)
        .fetch_one(&self.pool)
        .await
    }

    /// Record an active group-call leg, but only while the canonical call is
    /// still active. Rejoining a previously-left leg clears `left_at`;
    /// reconnecting an already-active leg is idempotent.
    pub async fn join_participant_if_active(
        &self,
        id: CallId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"INSERT INTO call_participants
                   (call_id, participant_id, role, joined_at, left_at)
               SELECT id,
                      $2,
                      CASE WHEN initiator = $2 THEN 'caller' ELSE 'member' END,
                      NOW(),
                      NULL
                 FROM call_sessions
                WHERE id = $1 AND ended_at IS NULL
               ON CONFLICT (call_id, participant_id)
               DO UPDATE SET joined_at = EXCLUDED.joined_at, left_at = NULL
               WHERE call_participants.left_at IS NOT NULL",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 1 {
            return Ok(true);
        }
        sqlx::query_scalar::<_, bool>(
            r"SELECT EXISTS(
                   SELECT 1
                     FROM call_sessions AS calls
                     JOIN call_participants AS legs
                       ON legs.call_id = calls.id
                    WHERE calls.id = $1
                      AND calls.ended_at IS NULL
                      AND legs.participant_id = $2
                      AND legs.left_at IS NULL
               )",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&self.pool)
        .await
    }

    /// Mark a participant's active group-call leg as left. Historical
    /// membership remains available while authorization immediately stops
    /// treating the leg as active.
    pub async fn leave_participant(
        &self,
        id: CallId,
        participant: ParticipantId,
    ) -> Result<(), sqlx::Error> {
        self.leave_participant_if_active(id, participant).await?;
        Ok(())
    }

    /// Compare-and-set an active participant leg to left while serializing
    /// against the call's End transition.
    ///
    /// Returns `true` only when this invocation changed an active leg. The
    /// canonical call row is locked until commit, so an End racing this leave
    /// observes a deterministic before/after order.
    pub async fn leave_participant_if_active(
        &self,
        id: CallId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let call_active = sqlx::query_scalar::<_, bool>(
            r"SELECT ended_at IS NULL
                FROM call_sessions
               WHERE id = $1
               FOR UPDATE",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        if !call_active {
            tx.rollback().await?;
            return Ok(false);
        }
        let result = sqlx::query(
            r"UPDATE call_participants
                  SET left_at = NOW()
                WHERE call_id = $1
                  AND participant_id = $2
                  AND left_at IS NULL",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// Compare-and-set one exact active leg generation to left while
    /// serializing against the canonical call's End transition.
    pub async fn leave_participant_if_generation(
        &self,
        id: CallId,
        participant: ParticipantId,
        generation: i64,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let call_active = sqlx::query_scalar::<_, bool>(
            r"SELECT ended_at IS NULL
                FROM call_sessions
               WHERE id = $1
               FOR UPDATE",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        if !call_active {
            tx.rollback().await?;
            return Ok(false);
        }
        let result = sqlx::query(
            r"UPDATE call_participants
                  SET left_at = NOW()
                WHERE call_id = $1
                  AND participant_id = $2
                  AND leg_generation = $3
                  AND left_at IS NULL",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(generation)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
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
        self.mark_answered_if_active(id).await?;
        Ok(())
    }

    /// Mark the call answered only if it is still active. Returns whether the
    /// call was active (an already-answered active call also returns `true`).
    pub async fn mark_answered_if_active(&self, id: CallId) -> Result<bool, sqlx::Error> {
        sqlx::query(
            r"UPDATE call_sessions SET answered_at = NOW()
               WHERE id = $1 AND answered_at IS NULL AND ended_at IS NULL",
        )
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        sqlx::query_scalar::<_, bool>(
            r"SELECT EXISTS(
                   SELECT 1 FROM call_sessions
                    WHERE id = $1 AND ended_at IS NULL AND answered_at IS NOT NULL
               )",
        )
        .bind(id.to_uuid())
        .fetch_one(&self.pool)
        .await
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
            callees
                .into_iter()
                .map(|(u,)| ParticipantId::from_uuid(u))
                .collect(),
        )))
    }
}

async fn lock_effective_call_room(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    actor: ParticipantId,
) -> aero_common::Result<String> {
    let resolved_kind = sqlx::query_scalar::<_, String>("SELECT kind FROM rooms WHERE id = $1")
        .bind(room.to_uuid())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Error::NotFound(format!("room {room}")))?;
    let effective: bool = sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, NULL)")
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .fetch_one(&mut **tx)
        .await?;
    if !effective {
        return Err(Error::Forbidden("room access denied".into()));
    }
    let locked_kind =
        sqlx::query_scalar::<_, String>("SELECT kind FROM rooms WHERE id = $1 FOR SHARE")
            .bind(room.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| Error::NotFound(format!("room {room}")))?;
    if locked_kind != resolved_kind {
        return Err(Error::Conflict("room identity changed".into()));
    }
    Ok(locked_kind)
}

async fn direct_call_is_blocked(
    tx: &mut Transaction<'_, Postgres>,
    initiator: ParticipantId,
    callees: &[ParticipantId],
) -> Result<bool, sqlx::Error> {
    if callees.is_empty() {
        return Ok(false);
    }
    for callee in callees {
        crate::user_blocks::lock_user_block_pair(tx, initiator, *callee).await?;
    }
    let callee_ids = callees
        .iter()
        .map(aero_common::ParticipantId::to_uuid)
        .collect::<Vec<_>>();
    sqlx::query_scalar(
        r"SELECT EXISTS (
              SELECT 1
                FROM user_blocks
               WHERE (
                         blocker_id = $1
                     AND blocked_id = ANY($2)
                     )
                  OR (
                         blocked_id = $1
                     AND blocker_id = ANY($2)
                     )
          )",
    )
    .bind(initiator.to_uuid())
    .bind(&callee_ids)
    .fetch_one(&mut **tx)
    .await
}

#[allow(clippy::too_many_arguments)]
async fn insert_call_aggregate(
    tx: &mut Transaction<'_, Postgres>,
    id: CallId,
    room: RoomId,
    initiator: ParticipantId,
    kind: CallKind,
    mode: CallMode,
    callees: &[ParticipantId],
    started_at: time::OffsetDateTime,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r"INSERT INTO call_sessions
              (id, room_id, initiator, kind, mode, started_at)
          VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id.to_uuid())
    .bind(room.to_uuid())
    .bind(initiator.to_uuid())
    .bind(call_kind_str(kind))
    .bind(call_mode_str(mode))
    .bind(started_at)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        r"INSERT INTO call_participants
              (call_id, participant_id, role, joined_at)
          VALUES ($1, $2, 'caller', $3)",
    )
    .bind(id.to_uuid())
    .bind(initiator.to_uuid())
    .bind(started_at)
    .execute(&mut **tx)
    .await?;

    for callee in callees {
        sqlx::query(
            r"INSERT INTO call_participants
                  (call_id, participant_id, role, joined_at)
              VALUES ($1, $2, 'callee', $3)
         ON CONFLICT DO NOTHING",
        )
        .bind(id.to_uuid())
        .bind(callee.to_uuid())
        .bind(started_at)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn lock_authorized_joinable_call(
    tx: &mut Transaction<'_, Postgres>,
    id: CallId,
    actor: ParticipantId,
    expected_room: RoomId,
    required_mode: CallMode,
) -> aero_common::Result<CallSession> {
    let resolved_room =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT room_id FROM call_sessions WHERE id = $1")
            .bind(id.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| Error::NotFound(format!("call {id}")))?;
    if resolved_room != expected_room.to_uuid() {
        return Err(Error::Forbidden(
            "call does not belong to the claimed room".into(),
        ));
    }
    lock_effective_call_room(tx, expected_room, actor).await?;
    let row = sqlx::query_as::<_, CallRow>(
        r"SELECT id, room_id, initiator, kind, mode, started_at, ended_at, end_reason
            FROM call_sessions
           WHERE id = $1
             FOR UPDATE",
    )
    .bind(id.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::NotFound(format!("call {id}")))?;
    let call = CallSession::from(row);
    validate_locked_call(&call, expected_room, Some(required_mode))?;
    Ok(call)
}

pub(crate) async fn lock_authorized_active_call(
    tx: &mut Transaction<'_, Postgres>,
    id: CallId,
    actor: ParticipantId,
    expected_room: RoomId,
    required_mode: Option<CallMode>,
    recipient: Option<ParticipantId>,
) -> aero_common::Result<CallSession> {
    let resolved_room =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT room_id FROM call_sessions WHERE id = $1")
            .bind(id.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| Error::NotFound(format!("call {id}")))?;
    if resolved_room != expected_room.to_uuid() {
        return Err(Error::Forbidden(
            "call does not belong to the claimed room".into(),
        ));
    }

    let mut authorized_participants = vec![actor];
    if let Some(recipient) = recipient {
        if recipient != actor {
            authorized_participants.push(recipient);
        }
    }
    authorized_participants.sort_unstable_by_key(aero_common::ParticipantId::to_uuid);
    for participant in &authorized_participants {
        lock_effective_call_room(tx, expected_room, *participant).await?;
    }

    let row = sqlx::query_as::<_, CallRow>(
        r"SELECT id, room_id, initiator, kind, mode, started_at, ended_at, end_reason
            FROM call_sessions
           WHERE id = $1
             FOR UPDATE",
    )
    .bind(id.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::NotFound(format!("call {id}")))?;
    let call = CallSession::from(row);
    validate_locked_call(&call, expected_room, required_mode)?;

    let participant_ids = authorized_participants
        .iter()
        .map(aero_common::ParticipantId::to_uuid)
        .collect::<Vec<_>>();
    let active_legs = sqlx::query_as::<_, (uuid::Uuid, Option<time::OffsetDateTime>)>(
        r"SELECT participant_id, left_at
            FROM call_participants
           WHERE call_id = $1
             AND participant_id = ANY($2)
           ORDER BY participant_id
             FOR UPDATE",
    )
    .bind(id.to_uuid())
    .bind(&participant_ids)
    .fetch_all(&mut **tx)
    .await?;
    if active_legs.len() != participant_ids.len()
        || active_legs.iter().any(|(_, left_at)| left_at.is_some())
    {
        return Err(Error::Forbidden("not an active call participant".into()));
    }
    Ok(call)
}

fn validate_locked_call(
    call: &CallSession,
    expected_room: RoomId,
    required_mode: Option<CallMode>,
) -> aero_common::Result<()> {
    if call.room_id != expected_room {
        return Err(Error::Forbidden(
            "call does not belong to the claimed room".into(),
        ));
    }
    if call.ended_at.is_some() {
        return Err(Error::Conflict("call has already ended".into()));
    }
    if required_mode.is_some_and(|mode| call.mode != mode) {
        return Err(Error::Conflict(
            "call mode does not support this operation".into(),
        ));
    }
    Ok(())
}

const fn call_kind_str(kind: CallKind) -> &'static str {
    match kind {
        CallKind::Audio => "audio",
        CallKind::Video => "video",
    }
}

const fn call_mode_str(mode: CallMode) -> &'static str {
    match mode {
        CallMode::P2p => "p2p",
        CallMode::Sfu => "sfu",
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
        let kind = if r.kind == "video" {
            CallKind::Video
        } else {
            CallKind::Audio
        };
        let mode = if r.mode == "sfu" {
            CallMode::Sfu
        } else {
            CallMode::P2p
        };
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

/// PG-gated integration tests live in a submodule to keep production call
/// repository code below the project file-size hard limit.
#[cfg(test)]
#[path = "call/db_tests.rs"]
mod db_tests;

#[cfg(test)]
#[path = "call/security_tests.rs"]
mod security_tests;
