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
use sqlx::PgPool;

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

/// The columns an [`AuthSession`] is built from, in select order. `token_hash` is
/// selected (so the prefix can be derived) but never serialized. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str =
    "id, participant_id, token_hash, user_agent, created_at, last_seen_at, revoked_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    String,
    Option<String>,
    time::OffsetDateTime,
    time::OffsetDateTime,
    Option<time::OffsetDateTime>,
);

fn row_to_model(r: Row) -> AuthSession {
    let (id, participant_id, token_hash, user_agent, created_at, last_seen_at, revoked_at) = r;
    AuthSession {
        id: SessionId::from_uuid(id),
        participant_id: ParticipantId::from_uuid(participant_id),
        token_prefix: token_hash.chars().take(PREFIX_LEN).collect(),
        user_agent,
        created_at,
        last_seen_at,
        revoked_at,
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
        assert_eq!(s1.token_prefix, h1.chars().take(PREFIX_LEN).collect::<String>());
        assert!(s1.token_prefix.len() < h1.len(), "prefix is shorter than the hash");
        assert_eq!(s1.user_agent.as_deref(), Some("Firefox"));

        // record is an upsert: re-recording h1 refreshes the SAME row.
        let id1_again = repo.record(owner, &h1, Some("Firefox")).await.unwrap();
        assert_eq!(id1_again, id1, "upsert returns the existing row id");

        // touch is a no-op error-wise on an unknown hash.
        repo.touch(&hash_token("never")).await.unwrap();
        repo.touch(&h1).await.unwrap();

        // A stranger cannot revoke the owner's session.
        assert!(
            repo.revoke(id1, stranger).await.unwrap().is_none(),
            "stranger cannot revoke another user's session"
        );

        // revoke_others keeps h1, revokes h2 — returns h2's hash.
        let revoked = repo.revoke_others(owner, &h1).await.unwrap();
        assert_eq!(revoked, vec![h2.clone()], "all-but-current revoked");
        let active = repo.list_active(owner).await.unwrap();
        assert!(active.iter().any(|s| s.id == id1), "kept session still active");
        assert_eq!(active.len(), 1, "only the kept session remains active");

        // revoke the remaining session → returns its hash; second revoke is None.
        assert_eq!(repo.revoke(id1, owner).await.unwrap(), Some(h1.clone()));
        assert!(repo.revoke(id1, owner).await.unwrap().is_none(), "second revoke is a no-op");
        assert!(repo.list_active(owner).await.unwrap().is_empty(), "no active sessions left");

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM auth_sessions WHERE participant_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
