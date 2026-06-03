//! In-room polls repository (create / vote / tally / close).
//!
//! Backs `migrations/0023_polls.sql`. A room member creates a poll (a question
//! plus 2..=10 options, single- or multi-choice); members vote; everyone sees the
//! live tally; the creator closes it. Purely additive: a NEW [`PollRepo`]; no
//! existing repo is touched.
//!
//! The `Poll`/`PollTally`/`PollOp` domain types live in
//! [`aero_common::model`] (re-exported), mirroring how `PinnedMessage` etc. are
//! shared; this module owns the SQL and the vote-validation helpers.

use aero_common::{ParticipantId, Poll, PollId, RoomId};
use sqlx::PgPool;

/// Minimum / maximum number of options a poll may have. A poll needs at least two
/// choices to be meaningful; the upper bound keeps the option list (and the
/// rendered ballot) bounded.
pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 10;

/// Why a vote was rejected. Kept distinct from [`sqlx::Error`] so the server can
/// map each case to a clean client error (closed ⇒ conflict, out-of-range ⇒
/// invalid) instead of a generic 500.
#[derive(Debug, thiserror::Error)]
pub enum VoteError {
    /// The poll has been closed by its creator and no longer accepts votes.
    #[error("poll is closed")]
    Closed,
    /// The chosen option index is outside `0..options.len()`.
    #[error("option index out of range")]
    OutOfRange,
    /// The poll does not exist.
    #[error("poll not found")]
    NotFound,
    /// A storage error occurred.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Whether `idx` selects a valid option of a poll with `option_count` options.
/// Pure, so the bounds rule is unit-tested without a database.
#[must_use]
pub fn option_in_range(idx: usize, option_count: usize) -> bool {
    idx < option_count
}

/// Whether `count` options is an acceptable poll size (`MIN_OPTIONS..=MAX_OPTIONS`).
/// Pure decision function, unit-tested offline.
#[must_use]
pub fn option_count_valid(count: usize) -> bool {
    (MIN_OPTIONS..=MAX_OPTIONS).contains(&count)
}

#[derive(Clone)]
pub struct PollRepo {
    pool: PgPool,
}

impl PollRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a poll in a room, returning its generated id. The caller has already
    /// validated room access and the option count (`option_count_valid`).
    pub async fn create(
        &self,
        room: RoomId,
        created_by: ParticipantId,
        question: &str,
        options: &[String],
        multi: bool,
    ) -> Result<PollId, sqlx::Error> {
        let id = PollId::new();
        sqlx::query(
            r"INSERT INTO polls (id, room_id, created_by, question, options, multi, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, now())",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(created_by.to_uuid())
        .bind(question)
        .bind(sqlx::types::Json(options))
        .bind(multi)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Fetch a poll by id, or `None` if it does not exist.
    pub async fn get(&self, poll: PollId) -> Result<Option<Poll>, sqlx::Error> {
        let row = sqlx::query_as::<_, PollRow>(
            r"SELECT id, room_id, created_by, question, options, multi, closed_at, created_at
               FROM polls WHERE id = $1",
        )
        .bind(poll.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Poll::from))
    }

    /// The room a poll belongs to, or `None` if the poll does not exist. Used by
    /// the server to resolve the room for the access check before a vote.
    pub async fn room_of(&self, poll: PollId) -> Result<Option<RoomId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(r"SELECT room_id FROM polls WHERE id = $1")
            .bind(poll.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(r,)| RoomId::from_uuid(r)))
    }

    /// Votes per option index. `counts[i]` is the number of votes for option `i`.
    /// The vector length equals the poll's option count, so an option with no
    /// votes still reports `0`. Returns an empty vector if the poll is gone.
    pub async fn tally(&self, poll: PollId) -> Result<Vec<u32>, sqlx::Error> {
        let Some(p) = self.get(poll).await? else {
            return Ok(Vec::new());
        };
        let rows = sqlx::query_as::<_, (i32, i64)>(
            r"SELECT option_idx, COUNT(*) AS n
               FROM poll_votes WHERE poll_id = $1
               GROUP BY option_idx",
        )
        .bind(poll.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        let mut counts = vec![0u32; p.options.len()];
        for (idx, n) in rows {
            if let Ok(i) = usize::try_from(idx) {
                if let Some(slot) = counts.get_mut(i) {
                    *slot = u32::try_from(n).unwrap_or(u32::MAX);
                }
            }
        }
        Ok(counts)
    }

    /// Whether `participant` has cast at least one vote in `poll`.
    pub async fn has_voted(
        &self,
        poll: PollId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*) FROM poll_votes WHERE poll_id = $1 AND participant_id = $2",
        )
        .bind(poll.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 > 0)
    }

    /// Cast a vote. For a single-choice poll (`multi == false`) any prior vote by
    /// this participant is replaced (delete-then-insert) so they end up with
    /// exactly one. For a multi-choice poll the `(poll, participant, idx)` row is
    /// upserted (idempotent). Rejects votes on a closed poll and out-of-range
    /// option indices. The `multi` flag is supplied by the caller (it has already
    /// loaded the poll) to avoid a redundant read.
    pub async fn vote(
        &self,
        poll: PollId,
        participant: ParticipantId,
        option_idx: usize,
        multi: bool,
    ) -> Result<(), VoteError> {
        // Re-read the poll under the call to honor closed-state + bounds against
        // the authoritative row (the caller's `multi` is only an optimization).
        let p = self.get(poll).await?.ok_or(VoteError::NotFound)?;
        if p.is_closed() {
            return Err(VoteError::Closed);
        }
        if !option_in_range(option_idx, p.options.len()) {
            return Err(VoteError::OutOfRange);
        }
        let idx = i32::try_from(option_idx).map_err(|_| VoteError::OutOfRange)?;

        if multi {
            // Multi-choice: add this option for the participant (idempotent).
            sqlx::query(
                r"INSERT INTO poll_votes (poll_id, participant_id, option_idx, created_at)
                   VALUES ($1, $2, $3, now())
                   ON CONFLICT (poll_id, participant_id, option_idx) DO NOTHING",
            )
            .bind(poll.to_uuid())
            .bind(participant.to_uuid())
            .bind(idx)
            .execute(&self.pool)
            .await?;
        } else {
            // Single-choice: a participant has exactly one vote. Replace any prior
            // choice atomically so a concurrent re-vote can't leave two rows.
            let mut tx = self.pool.begin().await?;
            sqlx::query(r"DELETE FROM poll_votes WHERE poll_id = $1 AND participant_id = $2")
                .bind(poll.to_uuid())
                .bind(participant.to_uuid())
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                r"INSERT INTO poll_votes (poll_id, participant_id, option_idx, created_at)
                   VALUES ($1, $2, $3, now())",
            )
            .bind(poll.to_uuid())
            .bind(participant.to_uuid())
            .bind(idx)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        }
        Ok(())
    }

    /// Close a poll — creator only. Returns `true` if THIS call closed it: false
    /// when the poll is gone, the actor is not the creator, or it was already
    /// closed. Verifying `created_by` in the `WHERE` keeps the check atomic and
    /// leaks nothing to a non-creator.
    pub async fn close(&self, poll: PollId, actor: ParticipantId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE polls SET closed_at = now()
               WHERE id = $1 AND created_by = $2 AND closed_at IS NULL",
        )
        .bind(poll.to_uuid())
        .bind(actor.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[derive(sqlx::FromRow)]
struct PollRow {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    created_by: uuid::Uuid,
    question: String,
    options: serde_json::Value,
    multi: bool,
    closed_at: Option<time::OffsetDateTime>,
    created_at: time::OffsetDateTime,
}

impl From<PollRow> for Poll {
    fn from(r: PollRow) -> Self {
        let options: Vec<String> = serde_json::from_value(r.options).unwrap_or_default();
        Self {
            id: PollId::from_uuid(r.id),
            room_id: RoomId::from_uuid(r.room_id),
            created_by: ParticipantId::from_uuid(r.created_by),
            question: r.question,
            options,
            multi: r.multi,
            closed_at: r.closed_at,
            created_at: r.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_in_range_bounds() {
        assert!(option_in_range(0, 2));
        assert!(option_in_range(1, 2));
        assert!(!option_in_range(2, 2));
        assert!(!option_in_range(0, 0));
    }

    #[test]
    fn option_count_valid_bounds() {
        assert!(!option_count_valid(0));
        assert!(!option_count_valid(1));
        assert!(option_count_valid(2));
        assert!(option_count_valid(10));
        assert!(!option_count_valid(11));
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored poll_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{ParticipantId, RoomId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn fixture(p: &PgPool) -> (RoomId, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(actor.to_uuid())
            .bind(format!("poll-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1,'channel',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("poll-room")
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        (room, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_create_vote_tally_close_roundtrip() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        let opts = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let poll = repo.create(room, actor, "pick one", &opts, false).await.unwrap();

        let got = repo.get(poll).await.unwrap().expect("poll exists");
        assert_eq!(got.options.len(), 3);
        assert!(!got.multi);
        assert!(!got.is_closed());
        assert_eq!(repo.room_of(poll).await.unwrap(), Some(room));

        // Vote for option 1.
        repo.vote(poll, actor, 1, false).await.unwrap();
        assert!(repo.has_voted(poll, actor).await.unwrap());
        let tally = repo.tally(poll).await.unwrap();
        assert_eq!(tally, vec![0, 1, 0], "one vote on option index 1");

        // Close (creator) — then a second close is a no-op.
        assert!(repo.close(poll, actor).await.unwrap(), "creator closed it");
        assert!(!repo.close(poll, actor).await.unwrap(), "already closed");
        assert!(repo.get(poll).await.unwrap().unwrap().is_closed());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_single_choice_revote_replaces() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        let opts = vec!["A".to_string(), "B".to_string()];
        let poll = repo.create(room, actor, "single", &opts, false).await.unwrap();

        repo.vote(poll, actor, 0, false).await.unwrap();
        repo.vote(poll, actor, 1, false).await.unwrap(); // re-vote replaces
        let tally = repo.tally(poll).await.unwrap();
        assert_eq!(tally, vec![0, 1], "single-choice keeps exactly the latest vote");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_multi_choice_accumulates() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        let opts = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let poll = repo.create(room, actor, "multi", &opts, true).await.unwrap();

        repo.vote(poll, actor, 0, true).await.unwrap();
        repo.vote(poll, actor, 2, true).await.unwrap();
        repo.vote(poll, actor, 2, true).await.unwrap(); // idempotent
        let tally = repo.tally(poll).await.unwrap();
        assert_eq!(tally, vec![1, 0, 1], "multi-choice keeps each distinct option");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_closed_rejects_vote_and_out_of_range() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        let opts = vec!["A".to_string(), "B".to_string()];
        let poll = repo.create(room, actor, "q", &opts, false).await.unwrap();

        // Out-of-range option is rejected.
        assert!(matches!(
            repo.vote(poll, actor, 9, false).await,
            Err(VoteError::OutOfRange)
        ));

        // Once closed, even a valid option is rejected.
        assert!(repo.close(poll, actor).await.unwrap());
        assert!(matches!(
            repo.vote(poll, actor, 0, false).await,
            Err(VoteError::Closed)
        ));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_close_is_creator_only() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, creator) = fixture(&p).await;
        let (_other_room, stranger) = fixture(&p).await;

        let opts = vec!["A".to_string(), "B".to_string()];
        let poll = repo.create(room, creator, "q", &opts, false).await.unwrap();

        // A non-creator cannot close it.
        assert!(!repo.close(poll, stranger).await.unwrap(), "stranger cannot close");
        assert!(!repo.get(poll).await.unwrap().unwrap().is_closed());
        // The creator can.
        assert!(repo.close(poll, creator).await.unwrap());
    }
}
