//! Call-transcript repository — persisted caption lines + post-call AI recap.
//!
//! Backs `migrations/0064_call_transcripts.sql`. Two concerns over a single
//! repo:
//! - `call_transcripts` rows: the *final* caption lines spoken during a call,
//!   appended one-per-line off the existing P3 caption relay (see the
//!   `CallCaption` handler in `aero-server`'s `ws.rs`).
//!   [`CallTranscriptRepo::append_authorized`] is the write path;
//!   [`CallTranscriptRepo::lines`] reads them back in spoken order for display
//!   or for grounding an AI recap.
//! - `call_sessions.recap`: the post-call AI summary grounded in the joined
//!   transcript, written once when the call ends ([`CallTranscriptRepo::set_recap`])
//!   and read back via [`CallTranscriptRepo::recap`].
//!
//! No new ID type: a transcript line's primary key is generated inline from a
//! fresh ULID (`uuid::Uuid::from_u128(ulid::Ulid::new().0)`), matching how the
//! repos that own opaque uuid pks derive them. `call_id` / `speaker_id` are plain,
//! foreign-key-free uuid columns, so a transcript can outlive the call/participant
//! rows it references. Purely additive: a NEW [`CallTranscriptRepo`]; no existing
//! repo is touched. The [`TranscriptLine`] model lives here (and is re-exported
//! from the crate root) rather than in `aero-common`, since it is a storage-layer
//! projection.

use aero_common::{CallId, ParticipantId, RoomId};
use serde::Serialize;
use sqlx::PgPool;

/// One persisted transcript line — a single final caption spoken during a call.
///
/// A storage-layer projection of a `call_transcripts` row. `Serialize` so a
/// handler can hand the row straight back as JSON; `created_at` renders as
/// RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptLine {
    /// Who spoke the line.
    pub speaker_id: ParticipantId,
    /// The caption text (the translated text when one was produced, else the
    /// original — the writer decides which).
    pub text: String,
    /// When the line was captured (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

type Row = (uuid::Uuid, String, time::OffsetDateTime);

fn row_to_model(r: Row) -> TranscriptLine {
    let (speaker_id, text, created_at) = r;
    TranscriptLine {
        speaker_id: ParticipantId::from_uuid(speaker_id),
        text,
        created_at,
    }
}

/// Repository over the `call_transcripts` table plus the `call_sessions.recap`
/// column.
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`CallTranscriptRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct CallTranscriptRepo {
    pool: PgPool,
}

impl CallTranscriptRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Append one transcript line for `call`, spoken by `speaker`.
    ///
    /// The insert shares one transaction with the canonical call-room,
    /// effective room-access, active call-leg, and live-call checks. A room
    /// membership revocation or call end therefore either commits first and
    /// rejects the caption, or waits until the complete line has committed.
    /// The caller remains responsible for trimming/non-empty validation.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn append_authorized(
        &self,
        call: CallId,
        speaker: ParticipantId,
        expected_room: RoomId,
        text: &str,
    ) -> aero_common::Result<()> {
        let mut tx = self.pool.begin().await?;
        crate::call::lock_authorized_active_call(&mut tx, call, speaker, expected_room, None, None)
            .await?;
        let id = uuid::Uuid::from_u128(ulid::Ulid::new().0);
        sqlx::query(
            r"INSERT INTO call_transcripts (id, call_id, speaker_id, text)
               VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(call.to_uuid())
        .bind(speaker.to_uuid())
        .bind(text)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// All of a call's transcript lines in spoken (chronological) order.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn lines(&self, call: CallId) -> Result<Vec<TranscriptLine>, sqlx::Error> {
        let rows = sqlx::query_as::<_, Row>(
            r"SELECT speaker_id, text, created_at
               FROM call_transcripts
              WHERE call_id = $1
              ORDER BY created_at ASC, id ASC",
        )
        .bind(call.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Store (or replace) the post-call AI recap for `call`, onto the existing
    /// `call_sessions` row. A no-op for an unknown call id.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn set_recap(&self, call: CallId, recap: &str) -> Result<(), sqlx::Error> {
        sqlx::query(r"UPDATE call_sessions SET recap = $2 WHERE id = $1")
            .bind(call.to_uuid())
            .bind(recap)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The post-call AI recap for `call`, or `None` if none has been produced
    /// (or the call is unknown).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn recap(&self, call: CallId) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as(r"SELECT recap FROM call_sessions WHERE id = $1")
                .bind(call.to_uuid())
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(recap,)| recap))
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored call_transcript
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{CallKind, CallMode, RoomId, WorkspaceId};

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
            .bind(format!("transcript-{tag}-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn room(p: &PgPool, creator: ParticipantId) -> (WorkspaceId, RoomId) {
        let workspace = WorkspaceId::new();
        let id = RoomId::new();
        let mut tx = p.begin().await.expect("begin transcript fixture");
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(workspace.to_uuid())
        .bind(format!("transcript-workspace-{workspace}"))
        .bind(format!("transcript-{workspace}"))
        .bind(creator.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert transcript workspace");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(workspace.to_uuid())
        .bind(creator.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert transcript workspace owner");
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1, 'group', $2, $3, now(), $4)",
        )
        .bind(id.to_uuid())
        .bind(format!("transcript-room-{id}"))
        .bind(creator.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert room");
        tx.commit().await.expect("commit transcript fixture");
        (workspace, id)
    }

    async fn grant_access(
        p: &PgPool,
        workspace: WorkspaceId,
        room: RoomId,
        participants: &[ParticipantId],
    ) {
        for participant in participants {
            sqlx::query(
                "INSERT INTO workspace_members (workspace_id, participant_id, role)
                 VALUES ($1, $2, 'member')
                 ON CONFLICT DO NOTHING",
            )
            .bind(workspace.to_uuid())
            .bind(participant.to_uuid())
            .execute(p)
            .await
            .expect("insert workspace member");
            sqlx::query(
                "INSERT INTO room_members (room_id, participant_id, role)
                 VALUES ($1, $2, 'member')
                 ON CONFLICT DO NOTHING",
            )
            .bind(room.to_uuid())
            .bind(participant.to_uuid())
            .execute(p)
            .await
            .expect("insert room member");
        }
    }

    /// `append_authorized` two lines → `lines` returns them in spoken order;
    /// `set_recap` / `recap` round-trip over `call_sessions.recap`.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn append_lines_and_recap_roundtrip() {
        let p = pool();
        let repo = CallTranscriptRepo::new(p.clone());
        let initiator = participant(&p, "caller").await;
        let recipient = participant(&p, "callee").await;
        let (workspace, r) = room(&p, initiator).await;

        grant_access(&p, workspace, r, &[initiator, recipient]).await;

        // A real call session so transcript authorization and recap storage have
        // a canonical aggregate to bind to.
        let call_id = CallId::new();
        crate::CallRepo::new(p.clone())
            .start(
                call_id,
                r,
                initiator,
                CallKind::Audio,
                CallMode::P2p,
                &[recipient],
            )
            .await
            .expect("start call");

        // Append two lines (caller then callee).
        repo.append_authorized(call_id, initiator, r, "hello there")
            .await
            .unwrap();
        repo.append_authorized(call_id, recipient, r, "general kenobi")
            .await
            .unwrap();

        let lines = repo.lines(call_id).await.unwrap();
        assert_eq!(lines.len(), 2, "both lines persisted");
        assert_eq!(lines[0].speaker_id, initiator);
        assert_eq!(lines[0].text, "hello there");
        assert_eq!(lines[1].speaker_id, recipient);
        assert_eq!(lines[1].text, "general kenobi");

        let raw_rewrite =
            sqlx::query("UPDATE call_transcripts SET text = 'rewritten' WHERE call_id = $1")
                .bind(call_id.to_uuid())
                .execute(&p)
                .await;
        assert!(
            raw_rewrite.is_err(),
            "persisted transcript lines are append-only"
        );

        // Recap round-trips; absent before, present after.
        assert!(repo.recap(call_id).await.unwrap().is_none(), "no recap yet");
        repo.set_recap(call_id, "- 双方互相问候").await.unwrap();
        assert_eq!(
            repo.recap(call_id).await.unwrap().as_deref(),
            Some("- 双方互相问候"),
            "recap round-trips"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM call_transcripts WHERE call_id = $1")
            .bind(call_id.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM call_sessions WHERE id = $1")
            .bind(call_id.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(r.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind(vec![initiator.to_uuid(), recipient.to_uuid()])
            .execute(&p)
            .await
            .ok();
    }

    /// An in-flight caption cannot cross a concurrent room-membership
    /// revocation. The append waits on the membership fence and then rejects
    /// without leaving a transcript row.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn append_is_linearized_with_room_revocation() {
        let p = pool();
        let repo = CallTranscriptRepo::new(p.clone());
        let initiator = participant(&p, "revoked-caller").await;
        let recipient = participant(&p, "revoked-callee").await;
        let (workspace, r) = room(&p, initiator).await;
        grant_access(&p, workspace, r, &[initiator, recipient]).await;
        let call_id = CallId::new();
        crate::CallRepo::new(p.clone())
            .start(
                call_id,
                r,
                initiator,
                CallKind::Audio,
                CallMode::P2p,
                &[recipient],
            )
            .await
            .expect("start call");

        let mut revoke = p.begin().await.expect("begin revocation");
        sqlx::query(
            "DELETE FROM room_members
              WHERE room_id = $1 AND participant_id = $2",
        )
        .bind(r.to_uuid())
        .bind(initiator.to_uuid())
        .execute(&mut *revoke)
        .await
        .expect("stage revocation");

        let append = repo.append_authorized(call_id, initiator, r, "must not persist");
        tokio::pin!(append);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), &mut append)
                .await
                .is_err(),
            "caption must wait for the membership revocation transaction"
        );
        revoke.commit().await.expect("commit revocation");
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), append)
                .await
                .expect("append completes after revocation")
                .is_err(),
            "committed revocation must reject the caption"
        );

        let persisted: i64 =
            sqlx::query_scalar("SELECT count(*) FROM call_transcripts WHERE call_id = $1")
                .bind(call_id.to_uuid())
                .fetch_one(&p)
                .await
                .expect("count transcript lines");
        assert_eq!(persisted, 0);
    }
}
