//! Failed-login attempt repository (ROADMAP5 方向五 — anomaly detection).
//!
//! Records one row per *failed* credential check (attempted account identifier +
//! source IP + user-agent + time). This is the persistent, queryable, cross-node
//! counterpart to the in-process [`LoginThrottle`](aero_auth) lockout: the
//! throttle resets on restart, lives only inside one node, and cannot be queried,
//! so a slow-rolling credential-stuffing run that stays under the per-account
//! lockout threshold leaves no durable trail. A `login_failures` row does — by
//! `account` (catches password-spraying / account enumeration: the email may not
//! even exist, which is why we key on the *attempted* identifier, not a
//! `participant_id`) and by `ip` (catches one host hammering many accounts).
//!
//! The sibling [`LoginEventRepo`](crate::login_event) records only *successful*
//! logins; together they give the full picture. Append-only; swept by the
//! data-lifecycle retention loop on the same window family as `login_events`.
//! Backs `migrations/0140_login_failures.sql`. Purely additive — a NEW repo; no
//! existing repo is touched.

use serde::Serialize;
use sqlx::PgPool;

/// One recorded failed-login attempt — a storage-layer projection of a
/// `login_failures` row. `Serialize` so an admin/security handler can return a
/// recent-failures view directly as JSON; `created_at` renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct LoginFailure {
    /// The account identifier that was attempted (typically the email). May name
    /// an account that does not exist — enumeration attempts are recorded too.
    pub account: String,
    /// Source IP (proxy-forwarded), or `None` when it could not be derived.
    pub ip: Option<String>,
    /// Client user-agent, or `None` when absent.
    pub user_agent: Option<String>,
    /// When the failed attempt happened.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// Repository over `login_failures`.
///
/// Cheap to clone — it just wraps a [`PgPool`] (an `Arc` internally).
#[derive(Clone)]
#[must_use]
pub struct LoginFailureRepo {
    pool: PgPool,
}

impl LoginFailureRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record one failed login for `account` from `ip` / `user_agent` (either may
    /// be `None` when not derivable). `account` is the attempted identifier (the
    /// email), recorded verbatim — it may not correspond to any real account.
    ///
    /// Callers on the login path should treat this as **fail-open**: a recording
    /// error is an observability gap, never a reason to fail the login flow.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn record(
        &self,
        account: &str,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO login_failures (account, ip, user_agent)
               VALUES ($1, $2, $3)",
        )
        .bind(account)
        .bind(ip)
        .bind(user_agent)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Count failed attempts against `account` at or after `since`. Powers a
    /// "this account has seen N failures in the last hour" anomaly signal that the
    /// per-node, restart-volatile [`LoginThrottle`](aero_auth) cannot give.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn recent_failures_for_account(
        &self,
        account: &str,
        since: time::OffsetDateTime,
    ) -> Result<i64, sqlx::Error> {
        let (count,): (i64,) = sqlx::query_as(
            r"SELECT COUNT(*) FROM login_failures
               WHERE account = $1 AND created_at >= $2",
        )
        .bind(account)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Count failed attempts originating from `ip` at or after `since`. Powers the
    /// "one host is spraying many accounts" signal (credential stuffing / spray),
    /// which an account-keyed view alone misses. A `None`/empty `ip` returns 0 —
    /// we never aggregate the unobservable into a single bucket.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn recent_failures_for_ip(
        &self,
        ip: Option<&str>,
        since: time::OffsetDateTime,
    ) -> Result<i64, sqlx::Error> {
        let Some(ip) = ip.filter(|s| !s.is_empty()) else {
            return Ok(0);
        };
        let (count,): (i64,) = sqlx::query_as(
            r"SELECT COUNT(*) FROM login_failures
               WHERE ip = $1 AND created_at >= $2",
        )
        .bind(ip)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// The most recent failed attempts against `account`, newest first, capped at
    /// `limit` (clamped to `[1, 100]`). For a security/admin "recent failed
    /// logins" view.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn recent_for_account(
        &self,
        account: &str,
        limit: i64,
    ) -> Result<Vec<LoginFailure>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>, time::OffsetDateTime)>(
            r"SELECT account, ip, user_agent, created_at
               FROM login_failures
              WHERE account = $1
              ORDER BY created_at DESC
              LIMIT $2",
        )
        .bind(account)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(account, ip, user_agent, created_at)| LoginFailure {
                account,
                ip,
                user_agent,
                created_at,
            })
            .collect())
    }

    /// Hard-delete failed-login rows older than `cutoff` (data-lifecycle retention
    /// sweep, ROADMAP5 方向四). Append-only; the trail is only useful while recent.
    /// Returns the number of rows deleted.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn sweep_before(
        &self,
        cutoff: time::OffsetDateTime,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(r"DELETE FROM login_failures WHERE created_at < $1")
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
///   cargo test -p aero-storage --lib -- --ignored login_failure
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn records_and_counts_failures() {
        let p = pool();
        let repo = LoginFailureRepo::new(p.clone());
        // Unique attempted account so the test is independent of other rows.
        let acct = format!("attacker-{}@example.invalid", uuid::Uuid::new_v4());
        let ip = "203.0.113.7";
        let since = time::OffsetDateTime::now_utc() - time::Duration::hours(1);

        // Nothing yet.
        assert_eq!(repo.recent_failures_for_account(&acct, since).await.expect("count empty"), 0);
        assert_eq!(repo.recent_failures_for_ip(Some(ip), since).await.expect("ip empty"), 0);
        // A missing IP never aggregates into a bucket.
        assert_eq!(repo.recent_failures_for_ip(None, since).await.expect("none ip"), 0);

        // Three failed attempts against the account, two of them from `ip`.
        repo.record(&acct, Some(ip), Some("curl/8")).await.expect("rec 1");
        repo.record(&acct, Some(ip), Some("curl/8")).await.expect("rec 2");
        repo.record(&acct, None, None).await.expect("rec 3");

        assert_eq!(
            repo.recent_failures_for_account(&acct, since).await.expect("count acct"),
            3,
            "three attempts against the account in-window"
        );
        assert_eq!(
            repo.recent_failures_for_ip(Some(ip), since).await.expect("count ip"),
            2,
            "two attempts from the ip in-window"
        );

        // A far-past `since` window excludes everything just recorded.
        let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        assert_eq!(
            repo.recent_failures_for_account(&acct, future).await.expect("count future"),
            0,
            "nothing newer than a future cutoff"
        );

        // recent_for_account returns newest-first and is account-scoped.
        let hist = repo.recent_for_account(&acct, 10).await.expect("recent");
        assert_eq!(hist.len(), 3, "three failures for this account");
        assert!(hist.iter().all(|f| f.account == acct), "account-scoped");

        // Retention sweep drops everything older than a far-future cutoff.
        let swept = repo
            .sweep_before(time::OffsetDateTime::now_utc() + time::Duration::days(1))
            .await
            .expect("sweep");
        assert!(swept >= 3, "all three rows swept, got {swept}");

        // Cleanup any rows this test might still own (sweep already removed them,
        // but be defensive against a shared DB).
        sqlx::query("DELETE FROM login_failures WHERE account = $1")
            .bind(&acct)
            .execute(&p)
            .await
            .ok();
    }
}
