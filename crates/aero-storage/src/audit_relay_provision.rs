//! B5-4 audit relay provisioning heartbeat repo (migration 0248).
//!
//! A singleton durable liveness row backing the settle-face freshness gate:
//! the audit relay only acknowledges deliveries (fenced settle → status 2)
//! while this row exists and is fresh. **Empty table = fail-closed
//! NotVerified** — settles stay rejected until a heartbeat lands.
//!
//! Single clock domain: the write (`record_heartbeat`), the freshness check
//! (`provision_check`), and the server samplers' ages all read the DB clock
//! (`clock_timestamp()`) — never an app-clock interpolation.
//!
//! Liveness is tick-driven (the server's 60s heartbeat tick + the one-shot
//! bootstrap arm), so a quiet period (no settle traffic) can never stale the
//! gate; this repo is deliberately traffic-independent.

use sqlx::PgPool;
use time::OffsetDateTime;

/// Three-state freshness verdict over the singleton provisioning row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionCheck {
    /// Row exists and `verified_at + freshness >= clock_timestamp()`
    /// (age == freshness still counts as fresh; age == freshness + 1 is
    /// stale — boundary pinned by the db_tests).
    Verified(OffsetDateTime),
    /// Row exists but stale: `age > freshness`.
    Stale(OffsetDateTime),
    /// No row — fail-closed default (bootstrap never landed).
    NotVerified,
}

/// Durable heartbeat over `audit_relay_provisioning` (singleton row).
#[derive(Debug, Clone)]
pub struct AuditRelayProvisionRepo {
    pool: PgPool,
}

impl AuditRelayProvisionRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Refresh the heartbeat (UPSERT on the singleton, DB clock). Creates the
    /// row when absent — the bootstrap arm and the 60s tick are both creator
    /// paths (FM-B: the tick retries until the DB returns).
    pub async fn record_heartbeat(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO audit_relay_provisioning (singleton, verified_at)
              VALUES (TRUE, clock_timestamp())
              ON CONFLICT (singleton) DO UPDATE
                 SET verified_at = clock_timestamp(),
                     updated_at = clock_timestamp()",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The last verified timestamp (`None` = no row yet).
    pub async fn verified_at(&self) -> Result<Option<OffsetDateTime>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT verified_at FROM audit_relay_provisioning WHERE singleton = TRUE",
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Three-state freshness check on the DB clock. `freshness` comes from
    /// the caller (no env parsing here); DB errors propagate and the caller
    /// fails closed (a freshness query failure never acknowledges).
    pub async fn provision_check(
        &self,
        freshness: time::Duration,
    ) -> Result<ProvisionCheck, sqlx::Error> {
        let row = sqlx::query_as::<_, (OffsetDateTime, bool)>(
            r"SELECT verified_at,
                     (verified_at + make_interval(secs => $1)) >= clock_timestamp()
                FROM audit_relay_provisioning
               WHERE singleton = TRUE",
        )
        .bind(freshness.whole_seconds())
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((verified_at, true)) => ProvisionCheck::Verified(verified_at),
            Some((verified_at, false)) => ProvisionCheck::Stale(verified_at),
            None => ProvisionCheck::NotVerified,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(url: &str) -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(url)
            .expect("well-formed DATABASE_URL")
    }

    /// Freshness window used by the boundary db_tests.
    const FRESHNESS: time::Duration = time::Duration::seconds(300);

    /// Self-isolating start: the singleton row is global (shared throwaway
    /// DB) — a re-run or a crashed sibling test must never leak a stale/absent
    /// state into the boundary assertions.
    async fn self_isolate(pool: &PgPool) {
        sqlx::query("DELETE FROM audit_relay_provisioning")
            .execute(pool)
            .await
            .expect("reset the provisioning singleton");
    }

    /// R3.1/R3.3 — the UPSERT is idempotent and re-refresh advances
    /// `verified_at` on the DB clock (the record-on-fenced-settle pin).
    #[tokio::test]
    #[ignore = "requires live Postgres (DATABASE_URL)"]
    async fn record_heartbeat_is_idempotent_and_advances_verified_at() {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must point at a throwaway Postgres");
        let pool = pool(&url);
        self_isolate(&pool).await;
        let repo = AuditRelayProvisionRepo::new(pool.clone());

        repo.record_heartbeat()
            .await
            .expect("first heartbeat creates the row");
        let first = repo
            .verified_at()
            .await
            .expect("read verified_at")
            .expect("row exists after the first heartbeat");
        assert!(
            matches!(
                repo.provision_check(FRESHNESS).await.expect("provision check"),
                ProvisionCheck::Verified(_)
            ),
            "a just-recorded heartbeat must be Verified"
        );

        // Second record must NOT error (UPSERT idempotency) and must advance
        // the timestamp strictly (DB clock monotonic within the test).
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        repo.record_heartbeat()
            .await
            .expect("second heartbeat upserts, never errors");
        let second = repo
            .verified_at()
            .await
            .expect("read verified_at")
            .expect("row still exists");
        assert!(
            second > first,
            "re-refresh must advance verified_at on the DB clock"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_relay_provisioning")
                .fetch_one(&pool)
                .await
                .expect("row count"),
            1,
            "UPSERT keeps exactly one singleton row"
        );

        self_isolate(&pool).await;
    }

    /// R3.3 — three-state `provision_check` + the freshness boundary:
    /// age == freshness → Verified; age == freshness + 1 → Stale; no row →
    /// NotVerified (fail-closed).
    #[tokio::test]
    #[ignore = "requires live Postgres (DATABASE_URL)"]
    async fn provision_check_pins_three_states_and_the_freshness_boundary() {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must point at a throwaway Postgres");
        let pool = pool(&url);
        self_isolate(&pool).await;
        let repo = AuditRelayProvisionRepo::new(pool.clone());

        // No row → NotVerified (the fail-closed default).
        assert_eq!(
            repo.provision_check(FRESHNESS).await.expect("provision check"),
            ProvisionCheck::NotVerified,
            "empty table must be fail-closed NotVerified"
        );

        // Age < freshness (299s against a 300s window) → Verified. A 1s
        // margin keeps the seed→check clock race out of the assertion (the
        // exact age == freshness instant is unreachable across two
        // statements — ε always flips it stale; see the inclusive-boundary
        // probe below for the `>=` semantics itself).
        sqlx::query(
            "INSERT INTO audit_relay_provisioning (singleton, verified_at)
             VALUES (TRUE, clock_timestamp() - make_interval(secs => $1))
             ON CONFLICT (singleton) DO UPDATE
                 SET verified_at = clock_timestamp() - make_interval(secs => $1),
                     updated_at = clock_timestamp()",
        )
        .bind(FRESHNESS.whole_seconds() - 1)
        .execute(&pool)
        .await
        .expect("seed in-window heartbeat");
        let check = repo
            .provision_check(FRESHNESS)
            .await
            .expect("provision check");
        assert!(
            matches!(check, ProvisionCheck::Verified(_)),
            "age < freshness must be Verified"
        );

        // Inclusive boundary semantics: `(verified_at + freshness) >=
        // clock_timestamp()` — a row seeded at EXACTLY `freshness` in the
        // past, evaluated in the SAME statement, must read as fresh. The
        // repo face cannot observe the exact `==` instant (`clock_timestamp()`
        // advances even within one statement — age is always freshness + ε
        // by check time), so this probe pins the SQL OPERATOR using the
        // statement-stable transaction timestamp (`now()`): if anyone
        // narrows `>=` to `>`, this reds.
        let boundary_fresh: bool = sqlx::query_scalar(
            r"WITH seeded AS (
                 INSERT INTO audit_relay_provisioning (singleton, verified_at)
                 VALUES (TRUE, now() - make_interval(secs => $1))
                 ON CONFLICT (singleton) DO UPDATE
                     SET verified_at = now() - make_interval(secs => $1)
                 RETURNING verified_at
             )
             SELECT (verified_at + make_interval(secs => $2)) >= now()
               FROM seeded",
        )
        .bind(FRESHNESS.whole_seconds())
        .bind(FRESHNESS.whole_seconds())
        .fetch_one(&pool)
        .await
        .expect("inclusive boundary probe");
        assert!(
            boundary_fresh,
            "age == freshness must count as Verified (inclusive `>=` boundary)"
        );

        // Age == freshness + 1 → Stale.
        sqlx::query(
            "UPDATE audit_relay_provisioning
                SET verified_at = clock_timestamp() - make_interval(secs => $1),
                    updated_at = clock_timestamp()
              WHERE singleton",
        )
        .bind(FRESHNESS.whole_seconds() + 1)
        .execute(&pool)
        .await
        .expect("seed stale heartbeat");
        let check = repo
            .provision_check(FRESHNESS)
            .await
            .expect("provision check");
        assert!(
            matches!(check, ProvisionCheck::Stale(_)),
            "age == freshness + 1 must be Stale"
        );

        // Age == 0 (fresh) → Verified again.
        repo.record_heartbeat().await.expect("fresh heartbeat");
        assert!(
            matches!(
                repo.provision_check(FRESHNESS).await.expect("provision check"),
                ProvisionCheck::Verified(_)
            ),
            "a fresh heartbeat must be Verified"
        );

        self_isolate(&pool).await;
    }
}
