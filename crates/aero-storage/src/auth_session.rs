//! Active-session repository (login-session inventory + remote / global sign-out).
//!
//! Backs `migrations/0061_sessions.sql`. Records one row per active
//! refresh-token-backed login ("device"), so a user (or an admin) can list their
//! active sessions and revoke one — or all-others ("sign out everywhere else").
//! Integrates with the EXISTING refresh/logout machinery: every row's
//! `token_hash` is the SHA-256 of the REFRESH token, computed with the SAME
//! [`hash_token`](crate::revoked_token::hash_token) used by `revoked_tokens`, so
//! a session row lines up exactly with the revoked-token check that gates
//! `POST /api/auth/refresh`. Revoking a session here returns the matching hash so
//! the caller can ALSO add it to `revoked_tokens`, making a refresh with the
//! revoked token `401`.
//!
//! Only the hash is stored (mirrors PAT / SCIM / webhook / revoked-token
//! handling), so the table is useless if it leaks; the [`AuthSession`] model never
//! exposes the full hash — only a short [`AuthSession::token_prefix`] for the UI.
//! Every read/mutate method is owner-scoped (`participant_id` in the `WHERE`), so
//! a caller can only ever see or revoke their own sessions. Purely additive: a NEW
//! [`SessionRepo`]; no existing repo is touched.

use aero_common::{ParticipantId, SessionId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

mod admin_revoke;
pub use admin_revoke::AdminSessionRevokeError;

/// How many leading hex chars of a session's token hash to surface to clients.
///
/// Enough to disambiguate a device in the UI, while never exposing the full hash
/// (a bearer-equivalent lookup key into both this table and `revoked_tokens`).
const PREFIX_LEN: usize = 12;

/// One active login session / device — a storage-layer projection of an
/// `auth_sessions` row.
///
/// `Serialize` so a handler can hand the row straight back as JSON; the full
/// `token_hash` is deliberately **never** a field (only [`Self::token_prefix`] is),
/// so it cannot leak through the API. Timestamps render as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct AuthSession {
    /// The session's unique id (used to address it for revocation).
    pub id: SessionId,
    /// The participant the session belongs to (and is scoped to).
    pub participant_id: ParticipantId,
    /// The first [`PREFIX_LEN`] hex chars of the session's refresh-token hash —
    /// a non-sensitive disambiguator for the UI, never the full hash.
    pub token_prefix: String,
    /// The `User-Agent` the session was recorded with, when one was sent.
    pub user_agent: Option<String>,
    /// When the session was first recorded (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// When the session was last refreshed/seen (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: time::OffsetDateTime,
    /// When the session was revoked, or `None` while active (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<time::OffsetDateTime>,
}

/// Result of an atomic refresh-token rotation attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshRotation {
    /// The old active session was retired and the new one recorded.
    Rotated,
    /// The old token has already been claimed/revoked.
    AlreadyRevoked,
    /// The signed token has no active session row and is therefore not
    /// refreshable (fail-closed for a prior session-recording failure).
    UnknownSession,
}

/// The columns an [`AuthSession`] is built from, in select order. `token_hash` is
/// selected (so the prefix can be derived) but never serialized. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str =
    "id, participant_id, token_hash, user_agent, created_at, last_seen_at, revoked_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    participant_id: uuid::Uuid,
    token_hash: String,
    user_agent: Option<String>,
    created_at: time::OffsetDateTime,
    last_seen_at: time::OffsetDateTime,
    revoked_at: Option<time::OffsetDateTime>,
}

fn row_to_model(r: Row) -> AuthSession {
    AuthSession {
        id: SessionId::from_uuid(r.id),
        participant_id: ParticipantId::from_uuid(r.participant_id),
        token_prefix: r.token_hash.chars().take(PREFIX_LEN).collect(),
        user_agent: r.user_agent,
        created_at: r.created_at,
        last_seen_at: r.last_seen_at,
        revoked_at: r.revoked_at,
    }
}

/// Repository over the `auth_sessions` table (active-session inventory).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`SessionRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct SessionRepo {
    pool: PgPool,
}

impl SessionRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record (or refresh) the active session identified by `token_hash` for
    /// `participant`, returning its id.
    ///
    /// Upsert keyed on the unique `token_hash`: a first sight inserts a new active
    /// row; a repeat (the same refresh token re-recorded) refreshes `last_seen_at`,
    /// clears any `revoked_at`, and fills in a `user_agent` if one is now known
    /// (without clobbering a previously-recorded one). The caller is responsible
    /// for hashing the plaintext refresh token via
    /// [`hash_token`](crate::revoked_token::hash_token) first. Best-effort by
    /// convention: callers (login) should not fail the request if this errors.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn record(
        &self,
        participant: ParticipantId,
        token_hash: &str,
        user_agent: Option<&str>,
    ) -> Result<SessionId, sqlx::Error> {
        let id = SessionId::new();
        let row: (uuid::Uuid,) = sqlx::query_as(
            r"INSERT INTO auth_sessions (id, participant_id, token_hash, user_agent)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (token_hash) DO UPDATE
                 SET last_seen_at = now(),
                     revoked_at = NULL,
                     user_agent = COALESCE(EXCLUDED.user_agent, auth_sessions.user_agent)
               RETURNING id",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(token_hash)
        .bind(user_agent)
        .fetch_one(&self.pool)
        .await?;
        Ok(SessionId::from_uuid(row.0))
    }

    /// Record a session under the caller-supplied stable id embedded in its JWT
    /// pair. Unlike [`Self::record`], this never invents or replaces an id: an
    /// idempotent retry may refresh only the same `(id, participant, token_hash)`
    /// row, and a revoked row is never reactivated.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`]. A conflicting id/token owned by another
    /// session fails closed instead of silently changing the JWT-to-row binding.
    pub async fn record_with_id(
        &self,
        participant: ParticipantId,
        id: SessionId,
        token_hash: &str,
        user_agent: Option<&str>,
    ) -> Result<SessionId, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        // Linearize session creation against GDPR erasure. `delete_participant`
        // updates this same row and then revokes every session before commit.
        // Whichever transaction wins the row lock determines the safe outcome:
        // either this insert commits first and erasure revokes it, or erasure
        // commits first and this lookup fails because `deleted_at` is populated.
        sqlx::query_scalar::<_, bool>(
            r"SELECT true
                FROM participants
               WHERE id = $1 AND deleted_at IS NULL
               FOR UPDATE",
        )
        .bind(participant.to_uuid())
        .fetch_one(&mut *tx)
        .await?;

        let row: (uuid::Uuid,) = sqlx::query_as(
            r"INSERT INTO auth_sessions (id, participant_id, token_hash, user_agent)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (id) DO UPDATE
                 SET last_seen_at = now(),
                     user_agent = COALESCE(EXCLUDED.user_agent, auth_sessions.user_agent)
               WHERE auth_sessions.participant_id = EXCLUDED.participant_id
                 AND auth_sessions.token_hash = EXCLUDED.token_hash
                 AND auth_sessions.revoked_at IS NULL
               RETURNING id",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(token_hash)
        .bind(user_agent)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(SessionId::from_uuid(row.0))
    }

    /// Whether `id` is an active, non-blacklisted session owned by `participant`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn is_active(
        &self,
        id: SessionId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let row: (bool,) = sqlx::query_as(
            r"SELECT EXISTS (
                   SELECT 1
                     FROM auth_sessions s
                     JOIN participants p
                       ON p.id = s.participant_id
                      AND p.deleted_at IS NULL
                    WHERE s.id = $1
                      AND s.participant_id = $2
                      AND s.revoked_at IS NULL
                      AND NOT EXISTS (
                          SELECT 1
                            FROM revoked_tokens r
                           WHERE r.token_hash = s.token_hash
                      )
               )",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Resolve an active refresh-token hash to its stable session id, scoped to
    /// the participant derived from the token's signed `sub` claim.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn active_id_by_hash(
        &self,
        participant: ParticipantId,
        token_hash: &str,
    ) -> Result<Option<SessionId>, sqlx::Error> {
        let row: Option<(uuid::Uuid,)> = sqlx::query_as(
            r"SELECT s.id
                 FROM auth_sessions s
                 JOIN participants p
                   ON p.id = s.participant_id
                  AND p.deleted_at IS NULL
                WHERE s.participant_id = $1
                  AND s.token_hash = $2
                  AND s.revoked_at IS NULL
                  AND NOT EXISTS (
                      SELECT 1
                        FROM revoked_tokens r
                       WHERE r.token_hash = s.token_hash
                  )",
        )
        .bind(participant.to_uuid())
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id,)| SessionId::from_uuid(id)))
    }

    /// Atomically claim an old refresh token, retire its session row, and record
    /// the freshly-rotated token as a new active session.
    ///
    /// This is the concurrency boundary for refresh rotation: only
    /// [`RefreshRotation::Rotated`] may return the new token pair.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`]. The transaction rolls back the revocation
    /// claim if retiring or recording the session fails.
    pub async fn rotate_refresh_token(
        &self,
        participant: ParticipantId,
        old_token_hash: &str,
        new_token_hash: &str,
        user_agent: Option<&str>,
    ) -> Result<RefreshRotation, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // Lock/retire the active inventory row first. Concurrent rotations
        // serialize on this UPDATE; the loser observes zero rows after the
        // winner commits.
        let retired = sqlx::query(
            r"UPDATE auth_sessions
                 SET revoked_at = now()
               WHERE token_hash = $1 AND participant_id = $2 AND revoked_at IS NULL",
        )
        .bind(old_token_hash)
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if !retired {
            let already_revoked: Option<(i32,)> =
                sqlx::query_as("SELECT 1 FROM revoked_tokens WHERE token_hash = $1")
                    .bind(old_token_hash)
                    .fetch_optional(&mut *tx)
                    .await?;
            tx.rollback().await?;
            return Ok(if already_revoked.is_some() {
                RefreshRotation::AlreadyRevoked
            } else {
                RefreshRotation::UnknownSession
            });
        }

        let claimed = sqlx::query(
            r"INSERT INTO revoked_tokens (token_hash, participant_id)
               VALUES ($1, $2)
               ON CONFLICT (token_hash) DO NOTHING",
        )
        .bind(old_token_hash)
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if !claimed {
            tx.rollback().await?;
            return Ok(RefreshRotation::AlreadyRevoked);
        }

        sqlx::query(
            r"INSERT INTO auth_sessions (id, participant_id, token_hash, user_agent)
               VALUES ($1, $2, $3, $4)",
        )
        .bind(SessionId::new().to_uuid())
        .bind(participant.to_uuid())
        .bind(new_token_hash)
        .bind(user_agent)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(RefreshRotation::Rotated)
    }

    /// Atomically rotate a refresh-token hash in place while preserving the
    /// stable session id embedded in both JWTs.
    ///
    /// The update is guarded by all three identity dimensions — `id`,
    /// `participant`, and `old_token_hash`. The row update is the concurrency
    /// claim: concurrent callers serialize on it and exactly one can observe the
    /// old hash. The winner blacklists that old hash in the same transaction.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`]. The transaction rolls back the row update
    /// if blacklisting fails.
    pub async fn rotate_refresh_token_in_place(
        &self,
        participant: ParticipantId,
        id: SessionId,
        old_token_hash: &str,
        new_token_hash: &str,
        user_agent: Option<&str>,
    ) -> Result<RefreshRotation, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let rotated: Option<(uuid::Uuid,)> = sqlx::query_as(
            r"UPDATE auth_sessions
                 SET token_hash = $4,
                     last_seen_at = now(),
                     user_agent = COALESCE($5, user_agent)
               WHERE id = $1
                 AND participant_id = $2
                 AND token_hash = $3
                 AND revoked_at IS NULL
                 AND NOT EXISTS (
                     SELECT 1 FROM revoked_tokens WHERE token_hash = $3
                 )
           RETURNING id",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(old_token_hash)
        .bind(new_token_hash)
        .bind(user_agent)
        .fetch_optional(&mut *tx)
        .await?;

        if rotated.is_none() {
            let already_revoked: Option<(i32,)> =
                sqlx::query_as("SELECT 1 FROM revoked_tokens WHERE token_hash = $1")
                    .bind(old_token_hash)
                    .fetch_optional(&mut *tx)
                    .await?;
            tx.rollback().await?;
            return Ok(if already_revoked.is_some() {
                RefreshRotation::AlreadyRevoked
            } else {
                RefreshRotation::UnknownSession
            });
        }

        let claimed = sqlx::query(
            r"INSERT INTO revoked_tokens (token_hash, participant_id)
               VALUES ($1, $2)
               ON CONFLICT (token_hash) DO NOTHING",
        )
        .bind(old_token_hash)
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if !claimed {
            tx.rollback().await?;
            return Ok(RefreshRotation::AlreadyRevoked);
        }

        tx.commit().await?;
        Ok(RefreshRotation::Rotated)
    }

    /// List `participant`'s currently-active (non-revoked) sessions, most recently
    /// seen first. Owner-scoped — only the caller's own rows are returned, and the
    /// full token hash is never surfaced.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_active(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<AuthSession>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM auth_sessions
              WHERE participant_id = $1 AND revoked_at IS NULL
              ORDER BY last_seen_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Refresh the `last_seen_at` of the active session identified by `token_hash`.
    /// A no-op when no active row matches (an unknown or already-revoked hash).
    /// Best-effort by convention: callers (refresh) should not fail on an error.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn touch(&self, token_hash: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"UPDATE auth_sessions
                 SET last_seen_at = now()
               WHERE token_hash = $1 AND revoked_at IS NULL",
        )
        .bind(token_hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Revoke one of `participant`'s active sessions by id, returning its
    /// `token_hash` so the caller can also add it to `revoked_tokens` (so a refresh
    /// with that token then `401`s). Owner-scoped: a stranger's (or unknown, or
    /// already-revoked) id resolves to `None`, so a caller can never revoke another
    /// user's session and a second revoke is a no-op.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn revoke(
        &self,
        id: SessionId,
        participant: ParticipantId,
    ) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(String,)> = sqlx::query_as(
            r"UPDATE auth_sessions
                 SET revoked_at = now()
               WHERE id = $1 AND participant_id = $2 AND revoked_at IS NULL
           RETURNING token_hash",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.0))
    }

    /// Revoke one owner-scoped session and blacklist its refresh token in the
    /// same database statement.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn revoke_and_blacklist(
        &self,
        id: SessionId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let revoked = Self::revoke_and_blacklist_in_tx(&mut tx, id, participant).await?;
        tx.commit().await?;
        Ok(revoked)
    }

    /// Transaction-scoped [`revoke_and_blacklist`](Self::revoke_and_blacklist)
    /// (B5-1 route path: the B5-1 route appends the `session.revoked` governance
    /// pair on top of this before committing).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn revoke_and_blacklist_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        id: SessionId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let row: Option<(String,)> = sqlx::query_as(
            r"WITH revoked AS (
                   UPDATE auth_sessions
                      SET revoked_at = now()
                    WHERE id = $1 AND participant_id = $2 AND revoked_at IS NULL
                RETURNING token_hash
               ),
               blacklisted AS (
                   INSERT INTO revoked_tokens (token_hash, participant_id)
                   SELECT token_hash, $2 FROM revoked
                   ON CONFLICT (token_hash) DO NOTHING
                   RETURNING token_hash
               )
               SELECT token_hash FROM revoked",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&mut **tx)
        .await?;
        Ok(row.is_some())
    }

    /// Revoke the active session identified by `token_hash` for `participant`,
    /// returning `true` iff a row was revoked. Owner-scoped; used by logout to
    /// retire the session row alongside blacklisting the refresh token. A no-op
    /// (`false`) for an unknown / already-revoked / non-owned hash. The caller is
    /// responsible for hashing the plaintext refresh token via
    /// [`hash_token`](crate::revoked_token::hash_token) first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn revoke_by_hash(
        &self,
        token_hash: &str,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE auth_sessions
                 SET revoked_at = now()
               WHERE token_hash = $1 AND participant_id = $2 AND revoked_at IS NULL",
        )
        .bind(token_hash)
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Blacklist a verified refresh token and retire its matching owner-scoped
    /// session row in one transaction. The blacklist insert is idempotent, so a
    /// repeated logout remains successful.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn revoke_by_hash_and_blacklist(
        &self,
        token_hash: &str,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let revoked =
            Self::revoke_by_hash_and_blacklist_in_tx(&mut tx, token_hash, participant).await?;
        tx.commit().await?;
        Ok(revoked)
    }

    /// Transaction-scoped revoke-by-hash (B5-1 logout route path): the UPDATE
    /// only — no blacklist, no begin/commit. The route appends the
    /// `session.revoked` governance pair on top of this before committing.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn revoke_by_hash_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        token_hash: &str,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE auth_sessions
                 SET revoked_at = now()
               WHERE token_hash = $1 AND participant_id = $2 AND revoked_at IS NULL",
        )
        .bind(token_hash)
        .bind(participant.to_uuid())
        .execute(&mut **tx)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Transaction-scoped blacklist + revoke (B5-1 logout route path: the route
    /// keeps the blacklist in the SAME tx as the pair). The blacklist insert is
    /// idempotent, so a repeated logout remains successful.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn revoke_by_hash_and_blacklist_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        token_hash: &str,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query(
            r"INSERT INTO revoked_tokens (token_hash, participant_id)
               VALUES ($1, $2)
               ON CONFLICT (token_hash) DO NOTHING",
        )
        .bind(token_hash)
        .bind(participant.to_uuid())
        .execute(&mut **tx)
        .await?;
        let revoked = sqlx::query(
            r"UPDATE auth_sessions
                 SET revoked_at = now()
               WHERE token_hash = $1 AND participant_id = $2 AND revoked_at IS NULL",
        )
        .bind(token_hash)
        .bind(participant.to_uuid())
        .execute(&mut **tx)
        .await?
        .rows_affected()
            > 0;
        Ok(revoked)
    }

    /// Revoke all of `participant`'s active sessions EXCEPT the one identified by
    /// `keep_token_hash` (the caller's current session), returning the
    /// `token_hash` of every session revoked so the caller can add each to
    /// `revoked_tokens` ("sign out everywhere else"). Owner-scoped.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    /// Revoke **all** active sessions for `participant`, including the current one.
    ///
    /// Returns the token hashes of every row that was revoked so callers can
    /// blacklist them in `revoked_tokens`. Used by the password-change flow to
    /// force re-authentication on every device after a credential rotation.
    pub async fn revoke_all_for_participant(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<String>, sqlx::Error> {
        let rows: Vec<(String,)> = sqlx::query_as(
            r"UPDATE auth_sessions
                 SET revoked_at = now()
               WHERE participant_id = $1
                 AND revoked_at IS NULL
           RETURNING token_hash",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// Revoke and blacklist every active refresh session for `participant`
    /// atomically. Returns the number of session rows transitioned.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn revoke_all_and_blacklist(
        &self,
        participant: ParticipantId,
    ) -> Result<u64, sqlx::Error> {
        let row: (i64, i64) = sqlx::query_as(
            r"WITH revoked AS (
                   UPDATE auth_sessions
                      SET revoked_at = now()
                    WHERE participant_id = $1 AND revoked_at IS NULL
                RETURNING token_hash
               ),
               blacklisted AS (
                   INSERT INTO revoked_tokens (token_hash, participant_id)
                   SELECT token_hash, $1 FROM revoked
                   ON CONFLICT (token_hash) DO NOTHING
                   RETURNING token_hash
               )
               SELECT (SELECT COUNT(*) FROM revoked),
                      (SELECT COUNT(*) FROM blacklisted)",
        )
        .bind(participant.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(u64::try_from(row.0).unwrap_or(0))
    }

    pub async fn revoke_others(
        &self,
        participant: ParticipantId,
        keep_token_hash: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        let rows: Vec<(String,)> = sqlx::query_as(
            r"UPDATE auth_sessions
                 SET revoked_at = now()
               WHERE participant_id = $1
                 AND revoked_at IS NULL
                 AND token_hash <> $2
           RETURNING token_hash",
        )
        .bind(participant.to_uuid())
        .bind(keep_token_hash)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// Atomically verify that `keep_token_hash` is an active, non-blacklisted
    /// session owned by `participant`, then revoke and blacklist every other
    /// active session. `None` means the proposed keep-session is not valid and
    /// no row was changed.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn revoke_others_and_blacklist(
        &self,
        participant: ParticipantId,
        keep_token_hash: &str,
    ) -> Result<Option<u64>, sqlx::Error> {
        let row: (bool, i64, i64) = sqlx::query_as(
            r"WITH keep AS (
                   SELECT 1
                     FROM auth_sessions s
                    WHERE s.participant_id = $1
                      AND s.token_hash = $2
                      AND s.revoked_at IS NULL
                      AND NOT EXISTS (
                          SELECT 1 FROM revoked_tokens r
                           WHERE r.token_hash = s.token_hash
                      )
               ),
               revoked AS (
                   UPDATE auth_sessions
                      SET revoked_at = now()
                    WHERE participant_id = $1
                      AND revoked_at IS NULL
                      AND token_hash <> $2
                      AND EXISTS (SELECT 1 FROM keep)
                RETURNING token_hash
               ),
               blacklisted AS (
                   INSERT INTO revoked_tokens (token_hash, participant_id)
                   SELECT token_hash, $1 FROM revoked
                   ON CONFLICT (token_hash) DO NOTHING
                   RETURNING token_hash
               )
               SELECT EXISTS (SELECT 1 FROM keep),
                      (SELECT COUNT(*) FROM revoked),
                      (SELECT COUNT(*) FROM blacklisted)",
        )
        .bind(participant.to_uuid())
        .bind(keep_token_hash)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0.then(|| u64::try_from(row.1).unwrap_or(0)))
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored auth_session
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::revoked_token::hash_token;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway owner participant so the test is self-contained.
    async fn owner(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("auth-session-owner-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn record_list_touch_revoke_owner_scoped() {
        let p = pool();
        let repo = SessionRepo::new(p.clone());
        let owner = owner(&p).await;
        let stranger = ParticipantId::new();

        // Unique per-run hashes so reruns stay self-contained.
        let h1 = hash_token(&format!("auth-session-a-{owner}"));
        let h2 = hash_token(&format!("auth-session-b-{owner}"));

        // record → two active sessions; the prefix never exposes the full hash.
        let id1 = repo.record(owner, &h1, Some("Firefox")).await.unwrap();
        let _id2 = repo.record(owner, &h2, None).await.unwrap();
        let active = repo.list_active(owner).await.unwrap();
        assert!(active.iter().any(|s| s.id == id1), "session present");
        let s1 = active.iter().find(|s| s.id == id1).expect("present");
        assert_eq!(
            s1.token_prefix,
            h1.chars().take(PREFIX_LEN).collect::<String>()
        );
        assert!(
            s1.token_prefix.len() < h1.len(),
            "prefix is shorter than the hash"
        );
        assert_eq!(s1.user_agent.as_deref(), Some("Firefox"));

        // record is an upsert: re-recording h1 refreshes the SAME row.
        let id1_again = repo.record(owner, &h1, Some("Firefox")).await.unwrap();
        assert_eq!(id1_again, id1, "upsert returns the existing row id");

        // touch is a no-op error-wise on an unknown hash.
        repo.touch(&hash_token("never")).await.unwrap();
        repo.touch(&h1).await.unwrap();

        // A stranger cannot revoke or blacklist the owner's session.
        assert!(
            !repo.revoke_and_blacklist(id1, stranger).await.unwrap(),
            "stranger cannot revoke another user's session"
        );

        // revoke_others keeps h1 and atomically blacklists h2.
        let revoked = repo.revoke_others_and_blacklist(owner, &h1).await.unwrap();
        assert_eq!(revoked, Some(1), "all-but-current revoked");
        let active = repo.list_active(owner).await.unwrap();
        assert!(
            active.iter().any(|s| s.id == id1),
            "kept session still active"
        );
        assert_eq!(active.len(), 1, "only the kept session remains active");
        assert!(
            crate::RevokedTokenRepo::new(p.clone())
                .is_revoked(&h2)
                .await
                .unwrap(),
            "revoked peer session is blacklisted"
        );

        // Revoke the remaining session; a second revoke is a no-op.
        assert!(repo.revoke_and_blacklist(id1, owner).await.unwrap());
        assert!(!repo.revoke_and_blacklist(id1, owner).await.unwrap());
        assert!(
            repo.list_active(owner).await.unwrap().is_empty(),
            "no active sessions left"
        );
        assert!(
            crate::RevokedTokenRepo::new(p.clone())
                .is_revoked(&h1)
                .await
                .unwrap(),
            "explicitly revoked session is blacklisted"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM auth_sessions WHERE participant_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM revoked_tokens WHERE token_hash = ANY($1)")
            .bind(&[h1, h2])
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn concurrent_refresh_rotation_has_exactly_one_winner() {
        let p = pool();
        let repo = SessionRepo::new(p.clone());
        let owner = owner(&p).await;
        let old = hash_token(&format!("refresh-old-{owner}"));
        let first_new = hash_token(&format!("refresh-new-a-{owner}"));
        let second_new = hash_token(&format!("refresh-new-b-{owner}"));
        repo.record(owner, &old, Some("test")).await.unwrap();

        let (first, second) = tokio::join!(
            repo.rotate_refresh_token(owner, &old, &first_new, Some("first")),
            repo.rotate_refresh_token(owner, &old, &second_new, Some("second"))
        );
        let winners = [first.unwrap(), second.unwrap()]
            .into_iter()
            .filter(|outcome| *outcome == RefreshRotation::Rotated)
            .count();
        assert_eq!(winners, 1);
        let active = repo.list_active(owner).await.unwrap();
        assert_eq!(
            active.len(),
            1,
            "only the winning rotated session is active"
        );
        assert!(
            crate::RevokedTokenRepo::new(p.clone())
                .is_revoked(&old)
                .await
                .unwrap(),
            "the old refresh token is claimed exactly once"
        );

        sqlx::query("DELETE FROM auth_sessions WHERE participant_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM revoked_tokens WHERE token_hash = ANY($1)")
            .bind(&[old, first_new, second_new])
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn concurrent_in_place_rotation_preserves_stable_session_id() {
        let p = pool();
        let repo = SessionRepo::new(p.clone());
        let owner = owner(&p).await;
        let session = SessionId::new();
        let old = hash_token(&format!("stable-refresh-old-{owner}"));
        let first_new = hash_token(&format!("stable-refresh-new-a-{owner}"));
        let second_new = hash_token(&format!("stable-refresh-new-b-{owner}"));
        assert_eq!(
            repo.record_with_id(owner, session, &old, Some("test"))
                .await
                .unwrap(),
            session
        );
        assert!(repo.is_active(session, owner).await.unwrap());
        assert_eq!(
            repo.active_id_by_hash(owner, &old).await.unwrap(),
            Some(session)
        );

        let (first, second) = tokio::join!(
            repo.rotate_refresh_token_in_place(owner, session, &old, &first_new, Some("first")),
            repo.rotate_refresh_token_in_place(owner, session, &old, &second_new, Some("second"))
        );
        let outcomes = [first.unwrap(), second.unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == RefreshRotation::Rotated)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == RefreshRotation::AlreadyRevoked)
                .count(),
            1
        );

        let active = repo.list_active(owner).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, session, "rotation must not replace the sid");
        assert!(repo.is_active(session, owner).await.unwrap());
        assert_eq!(repo.active_id_by_hash(owner, &old).await.unwrap(), None);
        let winning_hash = if outcomes[0] == RefreshRotation::Rotated {
            &first_new
        } else {
            &second_new
        };
        assert_eq!(
            repo.active_id_by_hash(owner, winning_hash).await.unwrap(),
            Some(session)
        );

        sqlx::query("DELETE FROM auth_sessions WHERE participant_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM revoked_tokens WHERE token_hash = ANY($1)")
            .bind(&[old, first_new, second_new])
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn record_with_id_racing_erasure_cannot_leave_an_active_session() {
        let p = pool();
        let repo = SessionRepo::new(p.clone());
        let participants = crate::ParticipantRepo::new(p.clone());
        let owner = owner(&p).await;
        let session = SessionId::new();
        let hash = hash_token(&format!("session-erasure-race-{owner}"));

        let (recorded, erased) = tokio::join!(
            repo.record_with_id(owner, session, &hash, Some("race")),
            participants.delete_participant(owner),
        );
        assert!(erased.expect("erase participant"));
        // Both serial orders are valid: record first (then erasure revokes it),
        // or erasure first (then record fails closed with RowNotFound).
        if let Err(error) = recorded {
            assert!(
                matches!(error, sqlx::Error::RowNotFound),
                "only a deleted participant may reject recording: {error}"
            );
        }
        assert!(
            !repo.is_active(session, owner).await.unwrap(),
            "no session may remain active after erasure commits"
        );
        assert_eq!(
            repo.active_id_by_hash(owner, &hash).await.unwrap(),
            None,
            "a tombstoned participant cannot resolve an active refresh session"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn active_session_queries_fail_closed_for_tombstoned_participant() {
        let p = pool();
        let repo = SessionRepo::new(p.clone());
        let owner = owner(&p).await;
        let session = SessionId::new();
        let hash = hash_token(&format!("session-tombstone-{owner}"));
        repo.record_with_id(owner, session, &hash, Some("test"))
            .await
            .expect("record live participant session");

        // Deliberately bypass delete_participant's session revocation to prove
        // both read paths independently enforce participant liveness.
        sqlx::query("UPDATE participants SET deleted_at = now() WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .expect("tombstone participant");
        assert!(!repo.is_active(session, owner).await.unwrap());
        assert_eq!(repo.active_id_by_hash(owner, &hash).await.unwrap(), None);

        let rejected = repo
            .record_with_id(owner, SessionId::new(), &hash_token("new"), None)
            .await;
        assert!(
            matches!(rejected, Err(sqlx::Error::RowNotFound)),
            "a tombstoned participant cannot create another session"
        );
    }
}
