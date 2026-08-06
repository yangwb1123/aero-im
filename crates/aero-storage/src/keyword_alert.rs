//! Keyword / highlight-alert repository (per-user keyword subscriptions).
//!
//! Backs `migrations/0037_keyword_alerts.sql`. A user subscribes to a keyword in
//! a workspace; when a message whose text contains that keyword is sent, the
//! subscriber is notified. This repo owns the subscription CRUD plus the
//! [`matching_subscribers`](KeywordAlertRepo::matching_subscribers) match query —
//! the *dispatch* hook that turns a match into a notification is wired separately
//! into `ImService::dispatch_notifications` (it calls `matching_subscribers`).
//!
//! Keywords are normalized (trimmed + lowercased) by [`normalize_keyword`] before
//! insert, and the match query lowercases the message text, so matching is
//! case-insensitive. Production CRUD owns effective-workspace authorization and
//! the owner-scoped mutation in one transaction. The match query is a current
//! snapshot: revoked/deleted/mandatory-2FA-ineligible subscribers are excluded,
//! while a retained alert re-enters only after access is restored. The
//! [`KeywordAlert`] model lives here (and is re-exported from the crate root)
//! rather than in `aero-common`, since it is a storage-layer projection.

use aero_common::{Error, KeywordAlertId, ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

/// Maximum normalized keyword length, in Unicode scalar values.
pub const MAX_KEYWORD_CHARS: usize = 64;

/// One keyword alert — a per-user, workspace-scoped keyword subscription.
///
/// A storage-layer projection of a `keyword_alerts` row. `Serialize` so a handler
/// can hand the row straight back as JSON; `created_at` renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct KeywordAlert {
    /// The keyword alert's unique id.
    pub id: KeywordAlertId,
    /// The subscriber the alert belongs to (and is scoped to).
    pub participant_id: ParticipantId,
    /// The tenant the alert is scoped to (only messages here can trigger it).
    pub workspace_id: WorkspaceId,
    /// The normalized keyword (trimmed + lowercased) to match against messages.
    pub keyword: String,
    /// When the alert was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// Normalize a keyword for storage and matching: trim surrounding whitespace and
/// lowercase it. Storing the canonical form (and lowercasing the message text at
/// match time) makes keyword matching case-insensitive and whitespace-tolerant.
///
/// Pure (no I/O), so the normalization rule is unit-tested offline.
#[must_use]
pub fn normalize_keyword(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// The columns a [`KeywordAlert`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "id, participant_id, workspace_id, keyword, created_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    participant_id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    keyword: String,
    created_at: time::OffsetDateTime,
}

fn row_to_model(r: Row) -> KeywordAlert {
    KeywordAlert {
        id: KeywordAlertId::from_uuid(r.id),
        participant_id: ParticipantId::from_uuid(r.participant_id),
        workspace_id: WorkspaceId::from_uuid(r.workspace_id),
        keyword: r.keyword,
        created_at: r.created_at,
    }
}

/// Repository over the `keyword_alerts` table (per-user keyword subscriptions).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`KeywordAlertRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct KeywordAlertRepo {
    pool: PgPool,
}

impl KeywordAlertRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Subscribe `participant` to `keyword` while effective workspace access is
    /// locked through the insert and canonical row read.
    ///
    /// Re-subscribing is idempotent and returns the exact existing row from this
    /// transaction; callers never need a second unguarded list query.
    pub async fn add_authorized(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        keyword: &str,
    ) -> Result<KeywordAlert, Error> {
        let normalized = validate_keyword(keyword)?;
        let mut tx = self.pool.begin().await?;
        assert_effective_member_in_tx(&mut tx, workspace, participant).await?;

        let id = KeywordAlertId::new();
        let insert_sql = format!(
            "INSERT INTO keyword_alerts (id, participant_id, workspace_id, keyword)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (participant_id, workspace_id, keyword) DO NOTHING
             RETURNING {COLUMNS}"
        );
        let inserted = sqlx::query_as::<_, Row>(&insert_sql)
            .bind(id.to_uuid())
            .bind(participant.to_uuid())
            .bind(workspace.to_uuid())
            .bind(&normalized)
            .fetch_optional(&mut *tx)
            .await?;
        let row = if let Some(row) = inserted { row } else {
            let select_sql = format!(
                "SELECT {COLUMNS}
                   FROM keyword_alerts
                  WHERE participant_id = $1
                    AND workspace_id = $2
                    AND keyword = $3"
            );
            sqlx::query_as::<_, Row>(&select_sql)
                .bind(participant.to_uuid())
                .bind(workspace.to_uuid())
                .bind(&normalized)
                .fetch_one(&mut *tx)
                .await?
        };
        let alert = row_to_model(row);
        tx.commit().await?;
        Ok(alert)
    }

    /// List the caller's alerts while current effective workspace access is held
    /// through the tenant-scoped read.
    pub async fn list_authorized(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
    ) -> Result<Vec<KeywordAlert>, Error> {
        let mut tx = self.pool.begin().await?;
        assert_effective_member_in_tx(&mut tx, workspace, participant).await?;
        let sql = format!(
            "SELECT {COLUMNS}
               FROM keyword_alerts
              WHERE participant_id = $1 AND workspace_id = $2
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .bind(workspace.to_uuid())
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Delete an alert while its canonical tenant and current owner access are
    /// held in one transaction.
    ///
    /// Unknown ids and ids belonging to a different participant are deliberately
    /// indistinguishable (`NotFound`). The alert row is locked only after the
    /// workspace authorization boundary, preserving the global lock order.
    pub async fn delete_authorized(
        &self,
        id: KeywordAlertId,
        participant: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        let resolved = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid)>(
            "SELECT participant_id, workspace_id
               FROM keyword_alerts
              WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((owner, workspace)) = resolved else {
            return Err(Error::NotFound(format!("keyword alert {id}")));
        };
        if owner != participant.to_uuid() {
            return Err(Error::NotFound(format!("keyword alert {id}")));
        }
        let workspace = WorkspaceId::from_uuid(workspace);
        assert_effective_member_in_tx(&mut tx, workspace, participant).await?;

        let removed = sqlx::query(
            "DELETE FROM keyword_alerts
              WHERE id = $1 AND participant_id = $2 AND workspace_id = $3",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&mut *tx)
        .await?;
        if removed.rows_affected() != 1 {
            return Err(Error::NotFound(format!("keyword alert {id}")));
        }
        tx.commit().await?;
        Ok(())
    }

    /// Low-level compatibility seam used only by storage tests.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert or the conflict lookup.
    #[cfg(test)]
    pub async fn add(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        keyword: &str,
    ) -> Result<KeywordAlertId, sqlx::Error> {
        let normalized = normalize_keyword(keyword);
        let id = KeywordAlertId::new();
        // Insert; on conflict (same participant+workspace+keyword) DO NOTHING and
        // RETURNING yields no row, so fall back to SELECTing the existing id.
        let inserted = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"INSERT INTO keyword_alerts (id, participant_id, workspace_id, keyword)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (participant_id, workspace_id, keyword) DO NOTHING
               RETURNING id",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .bind(&normalized)
        .fetch_optional(&self.pool)
        .await?;
        if let Some((existing,)) = inserted {
            return Ok(KeywordAlertId::from_uuid(existing));
        }
        // Conflict: the row already existed — return its id.
        let (existing,) = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT id FROM keyword_alerts
               WHERE participant_id = $1 AND workspace_id = $2 AND keyword = $3",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .bind(&normalized)
        .fetch_one(&self.pool)
        .await?;
        Ok(KeywordAlertId::from_uuid(existing))
    }

    /// Low-level compatibility seam used only by storage tests.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    #[cfg(test)]
    pub async fn list_for(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
    ) -> Result<Vec<KeywordAlert>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM keyword_alerts
              WHERE participant_id = $1 AND workspace_id = $2
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .bind(workspace.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Low-level compatibility seam used only by storage tests.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    #[cfg(test)]
    pub async fn delete(
        &self,
        id: KeywordAlertId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("DELETE FROM keyword_alerts WHERE id = $1 AND participant_id = $2")
                .bind(id.to_uuid())
                .bind(participant.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The distinct participants in `workspace` whose subscribed keyword appears in
    /// `text` (case-insensitively). This is the dispatch seam: the notification
    /// path calls it for each new message to find keyword subscribers to notify.
    /// The match uses `position(keyword IN lower($text)) > 0`; stored keywords are
    /// already normalized (lowercased), so the comparison is case-insensitive.
    ///
    /// Effective access is evaluated on every invocation. A revoked subscriber's
    /// retained row is skipped; restoring access makes it eligible again on a
    /// later invocation. The caller must apply its own dedupe (against
    /// already-notified mention/reply recipients) and never self-notify the
    /// sender.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn matching_subscribers(
        &self,
        workspace: WorkspaceId,
        text: &str,
    ) -> Result<Vec<ParticipantId>, sqlx::Error> {
        let lowered = text.to_lowercase();
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT DISTINCT participant_id
               FROM keyword_alerts
              WHERE workspace_id = $1
                AND position(keyword IN $2) > 0
                AND aero_participant_has_effective_workspace_access(
                        workspace_id,
                        participant_id
                    )",
        )
        .bind(workspace.to_uuid())
        .bind(&lowered)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(p,)| ParticipantId::from_uuid(p))
            .collect())
    }
}

fn validate_keyword(raw: &str) -> Result<String, Error> {
    let normalized = normalize_keyword(raw);
    if normalized.is_empty() {
        return Err(Error::Invalid("keyword must not be empty".into()));
    }
    if normalized.chars().count() > MAX_KEYWORD_CHARS {
        return Err(Error::Invalid(format!(
            "keyword too long (max {MAX_KEYWORD_CHARS} chars)"
        )));
    }
    Ok(normalized)
}

async fn assert_effective_member_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<(), Error> {
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR SHARE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
    if !exists {
        return Err(Error::NotFound(format!("workspace {workspace}")));
    }
    if !crate::workspace::members::effective_workspace_access_in_tx(tx, workspace, participant)
        .await?
    {
        return Err(Error::Forbidden("workspace member required".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keyword_trims_and_lowercases() {
        assert_eq!(normalize_keyword("  Deploy  "), "deploy");
        assert_eq!(normalize_keyword("ALERT"), "alert");
        assert_eq!(normalize_keyword("On-Call"), "on-call");
        assert_eq!(normalize_keyword("already"), "already");
        // Inner whitespace is preserved; only the ends are trimmed.
        assert_eq!(normalize_keyword("\tBuild Failed\n"), "build failed");
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored keyword_alert
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::WorkspaceRole;

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
            .bind(format!("keyword-alert-owner-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn workspace(p: &PgPool, owner: ParticipantId, label: &str) -> WorkspaceId {
        crate::WorkspaceRepo::new(p.clone())
            .create(
                format!("{label}-{owner}"),
                format!("{label}-{owner}").to_ascii_lowercase(),
                owner,
            )
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn keyword_alert_add_idempotent_list_delete_owner_scoped() {
        let p = pool();
        let repo = KeywordAlertRepo::new(p.clone());
        let owner = owner(&p).await;
        let ws = workspace(&p, owner, "keyword-alert-crud").await;
        let stranger = ParticipantId::new();

        // add → list shows it; the keyword is normalized (lowercased).
        let id = repo.add(owner, ws, "  Deploy  ").await.unwrap();
        let listed = repo.list_for(owner, ws).await.unwrap();
        let found = listed.iter().find(|a| a.id == id).expect("present");
        assert_eq!(found.keyword, "deploy", "keyword stored normalized");

        // add is idempotent: re-subscribing (even with different case/whitespace)
        // returns the SAME id and does not create a duplicate row.
        let again = repo.add(owner, ws, "DEPLOY").await.unwrap();
        assert_eq!(again, id, "re-subscribe returns the existing id");
        let after = repo.list_for(owner, ws).await.unwrap();
        assert_eq!(
            after.iter().filter(|a| a.keyword == "deploy").count(),
            1,
            "no duplicate keyword row"
        );

        // A stranger's delete is a no-op; the owner's first delete succeeds, the
        // second is a no-op.
        assert!(
            !repo.delete(id, stranger).await.unwrap(),
            "stranger cannot delete another user's alert"
        );
        assert!(repo.delete(id, owner).await.unwrap(), "owner deletes");
        assert!(
            !repo.delete(id, owner).await.unwrap(),
            "second delete is a no-op"
        );
        assert!(
            !repo
                .list_for(owner, ws)
                .await
                .unwrap()
                .iter()
                .any(|a| a.id == id),
            "deleted alert leaves the list"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM keyword_alerts WHERE participant_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn matching_subscribers_finds_match_excludes_non_match() {
        let p = pool();
        let repo = KeywordAlertRepo::new(p.clone());
        let subscriber = owner(&p).await;
        let other = owner(&p).await;
        let ws = workspace(&p, subscriber, "keyword-alert-match").await;
        crate::WorkspaceRepo::new(p.clone())
            .add_member(ws, other, WorkspaceRole::Member)
            .await
            .unwrap();

        repo.add(subscriber, ws, "incident").await.unwrap();
        repo.add(other, ws, "vacation").await.unwrap();

        // A message text containing "incident" (any case) matches the subscriber
        // but not the unrelated keyword owner.
        let hits = repo
            .matching_subscribers(ws, "We have an INCIDENT in prod")
            .await
            .unwrap();
        assert!(hits.contains(&subscriber), "keyword match found subscriber");
        assert!(
            !hits.contains(&other),
            "non-matching keyword owner excluded"
        );

        // A text with neither keyword matches nobody we inserted.
        let none = repo
            .matching_subscribers(ws, "all systems nominal")
            .await
            .unwrap();
        assert!(!none.contains(&subscriber), "no spurious match");
        assert!(!none.contains(&other), "no spurious match");

        // Cleanup.
        for o in [subscriber, other] {
            sqlx::query("DELETE FROM keyword_alerts WHERE participant_id = $1")
                .bind(o.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
    }
}

#[cfg(test)]
#[path = "keyword_alert/security_tests.rs"]
mod security_tests;
