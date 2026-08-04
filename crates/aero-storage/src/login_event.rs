//! Login IP/device history repository (ROADMAP5 方向五).
//!
//! Records one row per successful login (source IP + user-agent) so the platform
//! can show a user their recent login activity and flag a login from an IP that
//! account has never used before — the canonical account-takeover signal that
//! the in-process failed-login lockout ([`login_throttle`](aero_auth)) cannot
//! see. Backs `migrations/0134_login_events.sql`.
//!
//! `is_known_ip` is checked BEFORE recording the new event, so "is this a new
//! location?" is answered against the account's prior history, not including the
//! login in progress. Append-only; swept by the data-lifecycle retention loop.
//! Purely additive — a NEW repo; no existing repo is touched.

use aero_common::ParticipantId;
use serde::Serialize;
use sqlx::PgPool;

/// One recorded login event — a storage-layer projection of a `login_events`
/// row. `Serialize` so a handler can return a user's history directly as JSON;
/// `created_at` renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct LoginEvent {
    /// Source IP (proxy-forwarded), or `None` when it could not be derived.
    pub ip: Option<String>,
    /// Client user-agent, or `None` when absent.
    pub user_agent: Option<String>,
    /// When the login happened.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// Repository over `login_events`.
///
/// Cheap to clone — it just wraps a [`PgPool`] (an `Arc` internally).
#[derive(Clone)]
#[must_use]
pub struct LoginEventRepo {
    pool: PgPool,
}

impl LoginEventRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record one successful login for `participant` from `ip` / `user_agent`
    /// (either may be `None` when not derivable).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn record(
        &self,
        participant: ParticipantId,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO login_events (participant_id, ip, user_agent)
               VALUES ($1, $2, $3)",
        )
        .bind(participant.to_uuid())
        .bind(ip)
        .bind(user_agent)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Whether `participant` has EVER logged in from `ip` before. Used to detect a
    /// login from a new location: call this BEFORE [`record`](Self::record) so the
    /// in-progress login isn't counted. A `None`/empty `ip` is treated as "known"
    /// (we can't flag what we can't observe — never raise a false new-IP alert on
    /// a request whose source IP is simply unavailable).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_known_ip(
        &self,
        participant: ParticipantId,
        ip: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let Some(ip) = ip.filter(|s| !s.is_empty()) else {
            return Ok(true);
        };
        let (exists,): (bool,) = sqlx::query_as(
            r"SELECT EXISTS (
                 SELECT 1 FROM login_events
                  WHERE participant_id = $1 AND ip = $2)",
        )
        .bind(participant.to_uuid())
        .bind(ip)
        .fetch_one(&self.pool)
        .await?;
        Ok(exists)
    }

    /// Whether `participant` has any prior login on record at all. A first-ever
    /// login should NOT be flagged as "from a new IP" (every IP is new then), so
    /// the new-IP alert is gated on this being true.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn has_any(&self, participant: ParticipantId) -> Result<bool, sqlx::Error> {
        let (exists,): (bool,) =
            sqlx::query_as(r"SELECT EXISTS (SELECT 1 FROM login_events WHERE participant_id = $1)")
                .bind(participant.to_uuid())
                .fetch_one(&self.pool)
                .await?;
        Ok(exists)
    }

    /// The participant's most recent login events, newest first (owner-scoped),
    /// capped at `limit` (clamped to `[1, 100]`). For a "recent login activity"
    /// view.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn recent(
        &self,
        participant: ParticipantId,
        limit: i64,
    ) -> Result<Vec<LoginEvent>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let rows = sqlx::query_as::<_, (Option<String>, Option<String>, time::OffsetDateTime)>(
            r"SELECT ip, user_agent, created_at
               FROM login_events
              WHERE participant_id = $1
              ORDER BY created_at DESC
              LIMIT $2",
        )
        .bind(participant.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(ip, user_agent, created_at)| LoginEvent {
                ip,
                user_agent,
                created_at,
            })
            .collect())
    }

    /// Hard-delete login events older than `cutoff` (data-lifecycle retention
    /// sweep, ROADMAP5 方向四). Append-only; the history is only useful while
    /// recent. Returns the number of rows deleted.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn sweep_before(&self, cutoff: time::OffsetDateTime) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(r"DELETE FROM login_events WHERE created_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }
}

/// PG-gated integration tests:
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored login_event
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::ParticipantId;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("login-evt-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn records_history_and_detects_new_ip() {
        let p = pool();
        let repo = LoginEventRepo::new(p.clone());
        let me = participant(&p).await;

        // No history yet: has_any is false (so the caller suppresses the new-IP
        // alert on a first-ever login), while a specific unseen IP is correctly
        // "not known". A missing IP is always "known" (can't flag the unobservable).
        assert!(!repo.has_any(me).await.expect("has_any empty"));
        assert!(!repo
            .is_known_ip(me, Some("1.2.3.4"))
            .await
            .expect("specific unseen ip"));
        assert!(repo
            .is_known_ip(me, None)
            .await
            .expect("missing ip is known"));

        // First login from 1.2.3.4.
        repo.record(me, Some("1.2.3.4"), Some("Firefox"))
            .await
            .expect("record 1");
        assert!(repo.has_any(me).await.expect("has_any"));
        assert!(repo
            .is_known_ip(me, Some("1.2.3.4"))
            .await
            .expect("known same"));
        // A different IP is NOT yet known — the new-location signal.
        assert!(!repo
            .is_known_ip(me, Some("9.9.9.9"))
            .await
            .expect("known new"));
        // A missing IP is treated as known (can't flag the unobservable).
        assert!(repo.is_known_ip(me, None).await.expect("known none"));

        // Second login from the new IP; now it becomes known.
        repo.record(me, Some("9.9.9.9"), Some("Safari"))
            .await
            .expect("record 2");
        assert!(repo
            .is_known_ip(me, Some("9.9.9.9"))
            .await
            .expect("known after record"));

        // recent() returns newest-first.
        let hist = repo.recent(me, 10).await.expect("recent");
        assert_eq!(hist.len(), 2, "two logins recorded");
        assert_eq!(hist[0].ip.as_deref(), Some("9.9.9.9"), "newest first");
        assert_eq!(hist[1].ip.as_deref(), Some("1.2.3.4"));

        // Retention sweep drops everything older than a far-future cutoff.
        let swept = repo
            .sweep_before(time::OffsetDateTime::now_utc() + time::Duration::days(1))
            .await
            .expect("sweep");
        assert!(swept >= 2, "both rows swept, got {swept}");

        // Cleanup (sweep removed events; the participant FK-cascades on delete).
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(me.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
