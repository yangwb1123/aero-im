//! Call-transcript repository — persisted caption lines + post-call AI recap.
//!
//! Backs `migrations/0064_call_transcripts.sql`. Two concerns over a single
//! repo:
//! - `call_transcripts` rows: the *final* caption lines spoken during a call,
//!   appended one-per-line off the existing P3 caption relay (see the
//!   `CallCaption` handler in `aero-server`'s `ws.rs`). [`CallTranscriptRepo::append`]
//!   is the write path; [`CallTranscriptRepo::lines`] reads them back in spoken
//!   order for display or for grounding an AI recap.
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

use aero_common::{CallId, ParticipantId};
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

    /// Append one transcript line for `call`, spoken by `speaker`. The primary
    /// key is generated inline from a fresh ULID. The caller is responsible for
    /// access-gating and for trimming/non-empty validation of `text`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn append(
        &self,
        call: CallId,
        speaker: ParticipantId,
        text: &str,
    ) -> Result<(), sqlx::Error> {
        let id = uuid::Uuid::from_u128(ulid::Ulid::new().0);
        sqlx::query(
            r"INSERT INTO call_transcripts (id, call_id, speaker_id, text)
               VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(call.to_uuid())
        .bind(speaker.to_uuid())
        .bind(text)
        .execute(&self.pool)
        .await?;
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
    use aero_common::{CallKind, CallMode, RoomId};

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
            .bind(format!("transcript-{tag}-{id}"))
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
        .bind(format!("transcript-room-{id}"))
        .bind(creator.to_uuid())
        .bind(uuid::Uuid::parse_str(DEFAULT_WS).expect("uuid"))
        .execute(p)
        .await
        .expect("insert room");
        id
    }

    /// `append` two lines → `lines` returns them in spoken order; `set_recap` /
    /// `recap` round-trip over `call_sessions.recap`.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn append_lines_and_recap_roundtrip() {
        let p = pool();
        let repo = CallTranscriptRepo::new(p.clone());
        let caller = participant(&p, "caller").await;
        let callee = participant(&p, "callee").await;
        let r = room(&p, caller).await;

        // A real call session so `set_recap`/`recap` (which update/select
        // `call_sessions`) have a row to hang off.
        let call_id = CallId::new();
        crate::CallRepo::new(p.clone())
            .start(call_id, r, caller, CallKind::Audio, CallMode::P2p, &[callee])
            .await
            .expect("start call");

        // Append two lines (caller then callee).
        repo.append(call_id, caller, "hello there").await.unwrap();
        repo.append(call_id, callee, "general kenobi").await.unwrap();

        let lines = repo.lines(call_id).await.unwrap();
        assert_eq!(lines.len(), 2, "both lines persisted");
        assert_eq!(lines[0].speaker_id, caller);
        assert_eq!(lines[0].text, "hello there");
        assert_eq!(lines[1].speaker_id, callee);
        assert_eq!(lines[1].text, "general kenobi");

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
        sqlx::query("DELETE FROM rooms WHERE id = $1").bind(r.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind(vec![caller.to_uuid(), callee.to_uuid()])
            .execute(&p)
            .await
            .ok();
    }
}
