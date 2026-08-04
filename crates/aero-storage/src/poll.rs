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
use std::collections::HashSet;

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
    /// A ballot repeated the same option index.
    #[error("duplicate option index")]
    DuplicateOption,
    /// A ballot is empty.
    #[error("ballot is empty")]
    EmptyBallot,
    /// A ballot exceeds the poll's option count or the global poll option cap.
    #[error("too many options in ballot")]
    TooManyOptions,
    /// A single-choice poll received more than one selection.
    #[error("single-choice poll requires exactly one option")]
    SingleChoiceMultiple,
    /// The voter no longer has effective access to the poll's room.
    #[error("voter cannot access poll room")]
    Forbidden,
    /// The poll does not exist.
    #[error("poll not found")]
    NotFound,
    /// A storage error occurred.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Why an atomic creator-only close was rejected.
#[derive(Debug, thiserror::Error)]
pub enum ClosePollError {
    /// The poll does not exist (or no longer belongs to the expected room).
    #[error("poll not found")]
    NotFound,
    /// The actor lost effective access to the poll room before the close commit.
    #[error("actor cannot access poll room")]
    Forbidden,
    /// The actor has room access but did not create the poll.
    #[error("only the poll creator may close it")]
    NotCreator,
    /// A storage error occurred.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Why a transaction-owned poll creation was rejected.
#[derive(Debug, thiserror::Error)]
pub enum CreatePollError {
    /// The room disappeared before the write could acquire its aggregate lock.
    #[error("room not found")]
    NotFound,
    /// The creator lost effective room access before the insert committed.
    #[error("creator cannot access poll room")]
    Forbidden,
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

fn validate_ballot(
    option_idxs: &[usize],
    option_count: usize,
    multi: bool,
) -> Result<Vec<i32>, VoteError> {
    if option_idxs.is_empty() {
        return Err(VoteError::EmptyBallot);
    }
    if option_idxs.len() > MAX_OPTIONS || option_idxs.len() > option_count {
        return Err(VoteError::TooManyOptions);
    }
    if !multi && option_idxs.len() != 1 {
        return Err(VoteError::SingleChoiceMultiple);
    }

    let mut seen = HashSet::with_capacity(option_idxs.len());
    let mut validated = Vec::with_capacity(option_idxs.len());
    for &option_idx in option_idxs {
        if !option_in_range(option_idx, option_count) {
            return Err(VoteError::OutOfRange);
        }
        if !seen.insert(option_idx) {
            return Err(VoteError::DuplicateOption);
        }
        validated.push(i32::try_from(option_idx).map_err(|_| VoteError::OutOfRange)?);
    }
    Ok(validated)
}

#[derive(Clone)]
pub struct PollRepo {
    pool: PgPool,
}

impl PollRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a poll in a room, returning its generated id. Effective room access
    /// is rechecked under the same locks as the insert.
    pub async fn create(
        &self,
        room: RoomId,
        created_by: ParticipantId,
        question: &str,
        options: &[String],
        multi: bool,
    ) -> Result<PollId, CreatePollError> {
        self.create_with_opts(room, created_by, question, options, multi, false)
            .await
    }

    /// Like [`create`](Self::create) but accepts the `anonymous` flag controlling
    /// whether voter identities are hidden in the tally. Backed by
    /// `migrations/0096_polls_anonymous.sql`.
    pub async fn create_with_opts(
        &self,
        room: RoomId,
        created_by: ParticipantId,
        question: &str,
        options: &[String],
        multi: bool,
        anonymous: bool,
    ) -> Result<PollId, CreatePollError> {
        let mut tx = self.pool.begin().await?;
        if !lock_effective_room_access(&mut tx, room, created_by).await? {
            let room_exists = sqlx::query_scalar::<_, bool>("SELECT true FROM rooms WHERE id = $1")
                .bind(room.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
            return if room_exists {
                Err(CreatePollError::Forbidden)
            } else {
                Err(CreatePollError::NotFound)
            };
        }
        let id = PollId::new();
        sqlx::query(
            r"INSERT INTO polls (id, room_id, created_by, question, options, multi, anonymous, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, now())",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(created_by.to_uuid())
        .bind(question)
        .bind(sqlx::types::Json(options))
        .bind(multi)
        .bind(anonymous)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Fetch a poll by id, or `None` if it does not exist.
    pub async fn get(&self, poll: PollId) -> Result<Option<Poll>, sqlx::Error> {
        let row = sqlx::query_as::<_, PollRow>(
            r"SELECT id, room_id, created_by, question, options, multi, anonymous, closed_at, created_at
               FROM polls WHERE id = $1",
        )
        .bind(poll.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Poll::from))
    }

    /// Polls in a room, newest first (capped). `open_only` restricts to polls
    /// still accepting votes (`closed_at IS NULL`). Backs the room poll list so a
    /// client can enumerate polls instead of needing each poll id out of band.
    pub async fn list_for_room(
        &self,
        room: RoomId,
        open_only: bool,
    ) -> Result<Vec<Poll>, sqlx::Error> {
        let rows = sqlx::query_as::<_, PollRow>(
            r"SELECT id, room_id, created_by, question, options, multi, anonymous, closed_at, created_at
               FROM polls
              WHERE room_id = $1 AND ($2 = false OR closed_at IS NULL)
              ORDER BY id DESC
              LIMIT 100",
        )
        .bind(room.to_uuid())
        .bind(open_only)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Poll::from).collect())
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

    /// Atomically apply one complete request ballot.
    ///
    /// Effective authorization edges are locked in the global workspace -> room
    /// order before the poll row is locked and its closed state, choice mode, and
    /// options are decoded. Every index is validated before any vote row changes.
    /// Single-choice ballots replace the prior choice; multi-choice ballots add
    /// all requested distinct choices idempotently, preserving the existing
    /// incremental multi-vote behavior. A stale HTTP authorization therefore
    /// cannot create a vote after revocation.
    pub async fn vote_ballot(
        &self,
        poll: PollId,
        participant: ParticipantId,
        expected_room: RoomId,
        option_idxs: &[usize],
    ) -> Result<(), VoteError> {
        if option_idxs.len() > MAX_OPTIONS {
            return Err(VoteError::TooManyOptions);
        }
        let mut tx = self.pool.begin().await?;

        // Global authorization lock order is workspace -> room -> membership
        // rows -> aggregate row. Resolve the immutable poll->room edge without
        // a lock first, then acquire the authorization locks before locking the
        // poll. Workspace/room revocation writers use the same order, so either
        // the vote commits first or the revocation commits first and this
        // transaction observes Forbidden; a stale HTTP preflight cannot commit
        // after a completed revocation.
        let resolved_room =
            sqlx::query_scalar::<_, uuid::Uuid>("SELECT room_id FROM polls WHERE id = $1")
                .bind(poll.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(VoteError::NotFound)?;
        if resolved_room != expected_room.to_uuid() {
            return Err(VoteError::NotFound);
        }
        if !lock_effective_room_access(&mut tx, expected_room, participant).await? {
            return Err(VoteError::Forbidden);
        }

        let locked = sqlx::query_as::<_, BallotPollRow>(
            r"SELECT room_id, options, multi, closed_at
                 FROM polls
                WHERE id = $1
                FOR UPDATE",
        )
        .bind(poll.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(VoteError::NotFound)?;
        if RoomId::from_uuid(locked.room_id) != expected_room {
            return Err(VoteError::NotFound);
        }
        if locked.closed_at.is_some() {
            return Err(VoteError::Closed);
        }
        let option_idxs = validate_ballot(option_idxs, locked.options.0.len(), locked.multi)?;

        if !locked.multi {
            sqlx::query(r"DELETE FROM poll_votes WHERE poll_id = $1 AND participant_id = $2")
                .bind(poll.to_uuid())
                .bind(participant.to_uuid())
                .execute(&mut *tx)
                .await?;
        }

        sqlx::query(
            r"INSERT INTO poll_votes
                  (poll_id, participant_id, option_idx, created_at)
               SELECT $1, $2, choice.option_idx, now()
                 FROM UNNEST($3::int[]) AS choice(option_idx)
          ON CONFLICT (poll_id, participant_id, option_idx) DO NOTHING",
        )
        .bind(poll.to_uuid())
        .bind(participant.to_uuid())
        .bind(&option_idxs)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Close a poll with creator identity and effective room access checked under
    /// the same locks as the update. Returns `true` when this call closed it and
    /// `false` when the creator had already closed it.
    pub async fn close_authorized(
        &self,
        poll: PollId,
        actor: ParticipantId,
        expected_room: RoomId,
    ) -> Result<bool, ClosePollError> {
        let mut tx = self.pool.begin().await?;
        let resolved_room =
            sqlx::query_scalar::<_, uuid::Uuid>("SELECT room_id FROM polls WHERE id = $1")
                .bind(poll.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(ClosePollError::NotFound)?;
        if resolved_room != expected_room.to_uuid() {
            return Err(ClosePollError::NotFound);
        }
        if !lock_effective_room_access(&mut tx, expected_room, actor).await? {
            return Err(ClosePollError::Forbidden);
        }

        let locked = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, Option<time::OffsetDateTime>)>(
            r"SELECT room_id, created_by, closed_at
                 FROM polls
                WHERE id = $1
                FOR UPDATE",
        )
        .bind(poll.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ClosePollError::NotFound)?;
        if locked.0 != expected_room.to_uuid() {
            return Err(ClosePollError::NotFound);
        }
        if locked.1 != actor.to_uuid() {
            return Err(ClosePollError::NotCreator);
        }
        if locked.2.is_some() {
            tx.commit().await?;
            return Ok(false);
        }
        sqlx::query("UPDATE polls SET closed_at = now() WHERE id = $1")
            .bind(poll.to_uuid())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }
}

/// Hold every mutable authorization edge needed by a room write.
///
/// The workspace row serializes deactivation, workspace removal, and policy
/// changes; the room and membership rows serialize room removal/leave; the
/// participant and TOTP rows serialize account deletion and 2FA disable. Bots
/// and service principals are exempt from human mandatory-2FA enrollment, while
/// every other access gate remains mandatory.
async fn lock_effective_room_access(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    room: RoomId,
    participant: ParticipantId,
) -> Result<bool, sqlx::Error> {
    let workspace =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT workspace_id FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .fetch_optional(&mut **tx)
            .await?;
    let Some(workspace) = workspace else {
        return Ok(false);
    };

    let require_2fa =
        sqlx::query_scalar::<_, bool>("SELECT require_2fa FROM workspaces WHERE id = $1 FOR SHARE")
            .bind(workspace)
            .fetch_optional(&mut **tx)
            .await?;
    let Some(require_2fa) = require_2fa else {
        return Ok(false);
    };

    let locked_workspace = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT workspace_id FROM rooms WHERE id = $1 FOR SHARE",
    )
    .bind(room.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    if locked_workspace != Some(workspace) {
        return Ok(false);
    }

    let workspace_member = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM workspace_members
           WHERE workspace_id = $1 AND participant_id = $2
           FOR SHARE",
    )
    .bind(workspace)
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .is_some();
    if !workspace_member {
        return Ok(false);
    }

    let room_member = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM room_members
           WHERE room_id = $1 AND participant_id = $2
           FOR SHARE",
    )
    .bind(room.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .is_some();
    if !room_member {
        return Ok(false);
    }

    let participant_kind = sqlx::query_scalar::<_, String>(
        r"SELECT kind
            FROM participants
           WHERE id = $1 AND deleted_at IS NULL
           FOR SHARE",
    )
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    let Some(participant_kind) = participant_kind else {
        return Ok(false);
    };

    let deactivated = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM workspace_deactivations
           WHERE workspace_id = $1 AND participant_id = $2
           FOR SHARE",
    )
    .bind(workspace)
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .is_some();
    if deactivated {
        return Ok(false);
    }

    if participant_kind != "human" || !require_2fa {
        return Ok(true);
    }
    let totp_activated = sqlx::query_scalar::<_, bool>(
        r"SELECT activated
            FROM totp_secrets
           WHERE participant_id = $1
           FOR SHARE",
    )
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false);
    Ok(totp_activated)
}

#[derive(sqlx::FromRow)]
struct PollRow {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    created_by: uuid::Uuid,
    question: String,
    options: serde_json::Value,
    multi: bool,
    anonymous: bool,
    closed_at: Option<time::OffsetDateTime>,
    created_at: time::OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct BallotPollRow {
    room_id: uuid::Uuid,
    options: sqlx::types::Json<Vec<String>>,
    multi: bool,
    closed_at: Option<time::OffsetDateTime>,
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
            anonymous: r.anonymous,
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

    #[test]
    fn ballot_validation_rejects_duplicates_bounds_and_wrong_choice_mode() {
        assert_eq!(validate_ballot(&[0, 2], 3, true).unwrap(), vec![0, 2]);
        assert!(matches!(
            validate_ballot(&[0, 0], 3, true),
            Err(VoteError::DuplicateOption)
        ));
        assert!(matches!(
            validate_ballot(&[], 3, true),
            Err(VoteError::EmptyBallot)
        ));
        assert!(matches!(
            validate_ballot(&[0, 1], 3, false),
            Err(VoteError::SingleChoiceMultiple)
        ));
        assert!(matches!(
            validate_ballot(&[3], 3, true),
            Err(VoteError::OutOfRange)
        ));
        let oversized = vec![0; MAX_OPTIONS + 1];
        assert!(matches!(
            validate_ballot(&oversized, MAX_OPTIONS + 1, true),
            Err(VoteError::TooManyOptions)
        ));
        assert!(matches!(
            validate_ballot(&[0, 1, 2], 2, true),
            Err(VoteError::TooManyOptions)
        ));
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
            .max_connections(4)
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
             VALUES ($1,'group',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("poll-room")
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        sqlx::query(
            "INSERT INTO workspace_members
                 (workspace_id, participant_id, role)
             VALUES ('00000000-0000-0000-0000-000000000000', $1, 'member')",
        )
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("join workspace");
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("join room");
        (room, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_create_vote_tally_close_roundtrip() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        let opts = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let poll = repo
            .create(room, actor, "pick one", &opts, false)
            .await
            .unwrap();

        let got = repo.get(poll).await.unwrap().expect("poll exists");
        assert_eq!(got.options.len(), 3);
        assert!(!got.multi);
        assert!(!got.is_closed());
        assert_eq!(repo.room_of(poll).await.unwrap(), Some(room));

        // Vote for option 1.
        repo.vote_ballot(poll, actor, room, &[1]).await.unwrap();
        assert!(repo.has_voted(poll, actor).await.unwrap());
        let tally = repo.tally(poll).await.unwrap();
        assert_eq!(tally, vec![0, 1, 0], "one vote on option index 1");

        // Close (creator) — then a second close is a no-op.
        assert!(
            repo.close_authorized(poll, actor, room).await.unwrap(),
            "creator closed it"
        );
        assert!(
            !repo.close_authorized(poll, actor, room).await.unwrap(),
            "already closed"
        );
        assert!(repo.get(poll).await.unwrap().unwrap().is_closed());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_single_choice_revote_replaces() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        let opts = vec!["A".to_string(), "B".to_string()];
        let poll = repo
            .create(room, actor, "single", &opts, false)
            .await
            .unwrap();

        repo.vote_ballot(poll, actor, room, &[0]).await.unwrap();
        repo.vote_ballot(poll, actor, room, &[1]).await.unwrap(); // re-vote replaces
        let tally = repo.tally(poll).await.unwrap();
        assert_eq!(
            tally,
            vec![0, 1],
            "single-choice keeps exactly the latest vote"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_multi_choice_accumulates() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        let opts = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let poll = repo
            .create(room, actor, "multi", &opts, true)
            .await
            .unwrap();

        repo.vote_ballot(poll, actor, room, &[0, 2]).await.unwrap();
        repo.vote_ballot(poll, actor, room, &[2]).await.unwrap(); // idempotent
        let tally = repo.tally(poll).await.unwrap();
        assert_eq!(
            tally,
            vec![1, 0, 1],
            "multi-choice keeps each distinct option"
        );
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
            repo.vote_ballot(poll, actor, room, &[9]).await,
            Err(VoteError::OutOfRange)
        ));

        // Once closed, even a valid option is rejected.
        assert!(repo.close_authorized(poll, actor, room).await.unwrap());
        assert!(matches!(
            repo.vote_ballot(poll, actor, room, &[0]).await,
            Err(VoteError::Closed)
        ));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_ballot_validation_is_atomic_without_prefix_writes() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;
        let opts = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let multi = repo
            .create(room, actor, "multi", &opts, true)
            .await
            .unwrap();

        assert!(matches!(
            repo.vote_ballot(multi, actor, room, &[0, 99]).await,
            Err(VoteError::OutOfRange)
        ));
        assert!(matches!(
            repo.vote_ballot(multi, actor, room, &[0, 0]).await,
            Err(VoteError::DuplicateOption)
        ));
        let oversized = vec![0; MAX_OPTIONS + 1];
        assert!(matches!(
            repo.vote_ballot(multi, actor, room, &oversized).await,
            Err(VoteError::TooManyOptions)
        ));
        assert_eq!(
            repo.tally(multi).await.unwrap(),
            vec![0, 0, 0],
            "an invalid batch must not commit its valid prefix"
        );

        let single = repo
            .create(room, actor, "single", &opts, false)
            .await
            .unwrap();
        repo.vote_ballot(single, actor, room, &[1]).await.unwrap();
        assert!(matches!(
            repo.vote_ballot(single, actor, room, &[0, 2]).await,
            Err(VoteError::SingleChoiceMultiple)
        ));
        assert!(matches!(
            repo.vote_ballot(single, actor, room, &[99]).await,
            Err(VoteError::OutOfRange)
        ));
        assert_eq!(
            repo.tally(single).await.unwrap(),
            vec![0, 1, 0],
            "invalid replacement ballots must preserve the old single choice"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_vote_access_revocation_rolls_back_single_choice_delete() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;
        let opts = vec!["A".to_string(), "B".to_string()];
        let poll = repo
            .create(room, actor, "single", &opts, false)
            .await
            .unwrap();
        repo.vote_ballot(poll, actor, room, &[1]).await.unwrap();

        sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
            .bind(room.to_uuid())
            .bind(actor.to_uuid())
            .execute(&p)
            .await
            .unwrap();
        assert!(matches!(
            repo.vote_ballot(poll, actor, room, &[0]).await,
            Err(VoteError::Forbidden)
        ));
        assert_eq!(
            repo.tally(poll).await.unwrap(),
            vec![0, 1],
            "authorization failure must roll back the replacement delete"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_create_rechecks_access_in_insert_transaction() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
            .bind(room.to_uuid())
            .bind(actor.to_uuid())
            .execute(&p)
            .await
            .unwrap();
        assert!(matches!(
            repo.create(
                room,
                actor,
                "must not persist",
                &["A".to_owned(), "B".to_owned()],
                false,
            )
            .await,
            Err(CreatePollError::Forbidden)
        ));
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM polls WHERE room_id = $1")
            .bind(room.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_membership_revocation_wins_before_vote_commit() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;
        let poll = repo
            .create(
                room,
                actor,
                "revocation race",
                &["A".to_owned(), "B".to_owned()],
                false,
            )
            .await
            .unwrap();

        // Hold the mutable authorization edge exactly as a revocation writer
        // would. The vote cannot pass its FOR SHARE authorization fence until
        // this transaction commits the delete.
        let mut revoker = p.begin().await.unwrap();
        crate::ownership::lock_membership_governance(&mut revoker)
            .await
            .unwrap();
        sqlx::query(
            r"SELECT participant_id
                FROM room_members
               WHERE room_id = $1 AND participant_id = $2
               FOR UPDATE",
        )
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .fetch_one(&mut *revoker)
        .await
        .unwrap();
        let racing_repo = repo.clone();
        let vote =
            tokio::spawn(async move { racing_repo.vote_ballot(poll, actor, room, &[0]).await });
        tokio::task::yield_now().await;
        sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
            .bind(room.to_uuid())
            .bind(actor.to_uuid())
            .execute(&mut *revoker)
            .await
            .unwrap();
        revoker.commit().await.unwrap();

        assert!(matches!(vote.await.unwrap(), Err(VoteError::Forbidden)));
        assert_eq!(
            repo.tally(poll).await.unwrap(),
            vec![0, 0],
            "a vote waiting behind a committed revocation must not persist"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_membership_revocation_wins_before_close_commit() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;
        let poll = repo
            .create(
                room,
                actor,
                "close revocation race",
                &["A".to_owned(), "B".to_owned()],
                false,
            )
            .await
            .unwrap();

        let mut revoker = p.begin().await.unwrap();
        crate::ownership::lock_membership_governance(&mut revoker)
            .await
            .unwrap();
        sqlx::query(
            r"SELECT participant_id
                FROM room_members
               WHERE room_id = $1 AND participant_id = $2
               FOR UPDATE",
        )
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .fetch_one(&mut *revoker)
        .await
        .unwrap();
        let racing_repo = repo.clone();
        let close =
            tokio::spawn(async move { racing_repo.close_authorized(poll, actor, room).await });
        tokio::task::yield_now().await;
        sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
            .bind(room.to_uuid())
            .bind(actor.to_uuid())
            .execute(&mut *revoker)
            .await
            .unwrap();
        revoker.commit().await.unwrap();

        assert!(matches!(
            close.await.unwrap(),
            Err(ClosePollError::Forbidden)
        ));
        assert!(
            !repo.get(poll).await.unwrap().unwrap().is_closed(),
            "a close waiting behind a committed revocation must not persist"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn poll_close_wins_row_lock_race_and_rejects_whole_ballot() {
        let p = pool();
        let repo = PollRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;
        let opts = vec!["A".to_string(), "B".to_string()];
        let poll = repo
            .create(room, actor, "race", &opts, false)
            .await
            .unwrap();

        let mut closer = p.begin().await.unwrap();
        sqlx::query("SELECT id FROM polls WHERE id = $1 FOR UPDATE")
            .bind(poll.to_uuid())
            .fetch_one(&mut *closer)
            .await
            .unwrap();
        let racing_repo = repo.clone();
        let vote =
            tokio::spawn(async move { racing_repo.vote_ballot(poll, actor, room, &[0]).await });
        sqlx::query("UPDATE polls SET closed_at = now() WHERE id = $1")
            .bind(poll.to_uuid())
            .execute(&mut *closer)
            .await
            .unwrap();
        closer.commit().await.unwrap();

        assert!(matches!(vote.await.unwrap(), Err(VoteError::Closed)));
        assert_eq!(repo.tally(poll).await.unwrap(), vec![0, 0]);
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

        // Give the stranger legitimate room access so this exercises the
        // creator-only branch rather than the earlier access fence.
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(room.to_uuid())
        .bind(stranger.to_uuid())
        .execute(&p)
        .await
        .unwrap();
        assert!(matches!(
            repo.close_authorized(poll, stranger, room).await,
            Err(ClosePollError::NotCreator)
        ));
        assert!(!repo.get(poll).await.unwrap().unwrap().is_closed());
        // The creator can.
        assert!(repo.close_authorized(poll, creator, room).await.unwrap());
    }
}
