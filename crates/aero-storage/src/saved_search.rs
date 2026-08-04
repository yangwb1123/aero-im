//! Saved-search repository (per-user, workspace-scoped named queries).
//!
//! Backs `migrations/0032_saved_searches.sql`. A user saves a named search query
//! within a workspace, then lists, re-runs, or deletes it. The query string is
//! stored verbatim; *running* a saved search reuses the existing
//! membership-scoped cross-room search
//! ([`MessageRepo::search_all_rooms_in_workspace`](crate::MessageRepo)). Monitored
//! searches additionally use the atomic delivery path in this repository.
//!
//! Every read/mutate method is owner-scoped (`participant_id` in the `WHERE`), so
//! a caller can only ever see or delete their own saved searches. Purely
//! additive: a NEW [`SavedSearchRepo`]; no existing repo is touched. The
//! [`SavedSearch`] model lives here (and is re-exported from the crate root)
//! rather than in `aero-common`, since it is a storage-layer projection.

use aero_common::{
    MessageId, NotificationId, NotificationKind, ParticipantId, SavedSearchId, WorkspaceId,
};
use serde::Serialize;
use sqlx::PgPool;

mod authorization;
mod monitor_key;
pub use authorization::SavedSearchMonitorUpdate;
use monitor_key::{delivery_id as monitor_delivery_id, lower_bound as monitor_lower_bound};

/// Maximum number of user-facing saved searches returned in one page.
pub const MAX_SAVED_SEARCH_LIST_PAGE: i64 = 200;
/// Maximum number of standing monitors materialized by one dispatcher query.
pub const MAX_MONITORED_SEARCH_SCAN_PAGE: i64 = 200;

/// One saved search — a per-user, workspace-scoped named query.
///
/// A storage-layer projection of a `saved_searches` row. `Serialize` so a handler
/// can hand the row straight back as JSON; `created_at` renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct SavedSearch {
    /// The saved search's unique id.
    pub id: SavedSearchId,
    /// The owner the saved search belongs to (and is scoped to).
    pub participant_id: ParticipantId,
    /// The tenant the saved search is scoped to (its results stay in this one).
    pub workspace_id: WorkspaceId,
    /// Human-readable name the owner gave the saved search.
    pub name: String,
    /// The raw query string, run through the membership-scoped search on demand.
    pub query: String,
    /// When the saved search was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// A monitored saved search — the work-item the periodic digest dispatcher
/// processes (one per `notify_new = true` row). Carries just what the dispatcher
/// needs to schedule the atomic monitor transaction.
#[derive(Debug, Clone)]
pub struct MonitoredSearch {
    /// The saved search's id.
    pub id: SavedSearchId,
    /// The owner — the search runs membership-scoped as them, and they receive the
    /// new-match notifications.
    pub owner: ParticipantId,
    /// The tenant the search is scoped to.
    pub workspace: WorkspaceId,
    /// The raw query string.
    pub query: String,
    /// The monitor's independent timestamp cursor.
    ///
    /// This is deliberately separate from the user-facing `last_run_at`: manually
    /// running a saved search must not skip pending background notifications.
    pub cursor_at: Option<time::OffsetDateTime>,
    /// UUID tiebreaker for messages sharing [`Self::cursor_at`]. `None` means a
    /// strict timestamp boundary (used only by migrated legacy cursors); new
    /// baselines use the nil UUID so later rows at the same timestamp remain visible.
    pub cursor_message_id: Option<MessageId>,
}

/// Immutable upper boundary for one complete dispatcher tick.
#[derive(Debug, Clone, Copy)]
pub struct MonitoredSearchScan {
    enabled_before: time::OffsetDateTime,
    through: SavedSearchId,
}

/// The columns a [`SavedSearch`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "id, participant_id, workspace_id, name, query, created_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    participant_id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    name: String,
    query: String,
    created_at: time::OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct LockedMonitor {
    workspace_id: uuid::Uuid,
    query: String,
    cursor_at: Option<time::OffsetDateTime>,
    cursor_message_id: Option<uuid::Uuid>,
    floor_at: Option<time::OffsetDateTime>,
    floor_message_id: Option<uuid::Uuid>,
}

fn row_to_model(r: Row) -> SavedSearch {
    SavedSearch {
        id: SavedSearchId::from_uuid(r.id),
        participant_id: ParticipantId::from_uuid(r.participant_id),
        workspace_id: WorkspaceId::from_uuid(r.workspace_id),
        name: r.name,
        query: r.query,
        created_at: r.created_at,
    }
}

async fn sweep_monitor_delivery_ledger(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    search: SavedSearchId,
) -> Result<(), sqlx::Error> {
    // Twenty-four hours is deliberately much wider than the 15-minute replay
    // overlap. Stable notification delivery ids remain the second dedupe fence.
    sqlx::query(
        "DELETE FROM saved_search_monitor_deliveries
          WHERE saved_search_id = $1
            AND delivered_at < CURRENT_TIMESTAMP - INTERVAL '24 hours'",
    )
    .bind(search.to_uuid())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Repository over the `saved_searches` table (per-user named queries).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`SavedSearchRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct SavedSearchRepo {
    pool: PgPool,
}

impl SavedSearchRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new saved search for `participant` in `workspace`, returning its
    /// generated id. The caller is responsible for workspace-membership and
    /// name/query validation.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        name: &str,
        query: &str,
    ) -> Result<SavedSearchId, sqlx::Error> {
        let id = SavedSearchId::new();
        sqlx::query(
            r"INSERT INTO saved_searches (id, participant_id, workspace_id, name, query)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .bind(name)
        .bind(query)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List one bounded page of `participant`'s saved searches in `workspace`,
    /// newest first. `before` is an owner/workspace-scoped keyset cursor: a
    /// missing or foreign cursor yields an empty page rather than revealing it.
    /// Owner-scoped — only the caller's own rows are returned.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        before: Option<SavedSearchId>,
        limit: i64,
    ) -> Result<Vec<SavedSearch>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM saved_searches saved
              WHERE saved.participant_id = $1
                AND saved.workspace_id = $2
                AND (
                    $3::uuid IS NULL
                    OR (saved.created_at, saved.id) < (
                        SELECT cursor.created_at, cursor.id
                          FROM saved_searches cursor
                         WHERE cursor.id = $3
                           AND cursor.participant_id = $1
                           AND cursor.workspace_id = $2
                    )
                )
              ORDER BY saved.created_at DESC, saved.id DESC
              LIMIT $4"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .bind(workspace.to_uuid())
            .bind(before.map(|id| id.to_uuid()))
            .bind(limit.clamp(1, MAX_SAVED_SEARCH_LIST_PAGE))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Fetch one of `participant`'s saved searches by id, or `None` if no such row
    /// exists *for that owner*. Owner-scoped: a stranger's id resolves to `None`,
    /// so this can never surface another user's saved search.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(
        &self,
        id: SavedSearchId,
        participant: ParticipantId,
    ) -> Result<Option<SavedSearch>, sqlx::Error> {
        let sql =
            format!("SELECT {COLUMNS} FROM saved_searches WHERE id = $1 AND participant_id = $2");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(participant.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Delete one of `participant`'s saved searches. Returns `true` iff a row was
    /// removed — owner-scoped, so a caller can never delete another user's saved
    /// search, and a second delete (or a stranger's) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(
        &self,
        id: SavedSearchId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("DELETE FROM saved_searches WHERE id = $1 AND participant_id = $2")
                .bind(id.to_uuid())
                .bind(participant.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Toggle this saved search's `notify_new` monitoring flag (owner-scoped).
    /// Returns `true` iff a row was updated. When `true`, a background dispatcher
    /// periodically re-runs the query and notifies the owner of new matches.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn set_notify_new(
        &self,
        id: SavedSearchId,
        participant: ParticipantId,
        notify_new: bool,
    ) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            r"UPDATE saved_searches
                  SET monitor_cursor_at = CASE
                        WHEN $3 AND NOT notify_new THEN NULL
                        ELSE monitor_cursor_at
                      END,
                      monitor_cursor_message_id = CASE
                        WHEN $3 AND NOT notify_new THEN NULL
                        ELSE monitor_cursor_message_id
                      END,
                      monitor_floor_at = CASE
                        WHEN $3 AND NOT notify_new THEN NULL
                        ELSE monitor_floor_at
                      END,
                      monitor_floor_message_id = CASE
                        WHEN $3 AND NOT notify_new THEN NULL
                        ELSE monitor_floor_message_id
                      END,
                      notify_new = $3
               WHERE id = $1 AND participant_id = $2",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(notify_new)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// How many of `participant`'s saved searches currently have monitoring
    /// enabled. Used to cap per-owner monitoring so a single user can't enable
    /// thousands of standing cross-room queries and amplify the dispatcher's cost.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn count_monitored(&self, participant: ParticipantId) -> Result<i64, sqlx::Error> {
        let (n,): (i64,) = sqlx::query_as(
            r"SELECT COUNT(*) FROM saved_searches
               WHERE participant_id = $1 AND notify_new",
        )
        .bind(participant.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }

    /// Freeze the upper id and database enable-time boundary for one dispatcher
    /// tick. Monitors created or re-enabled after this call are intentionally
    /// deferred to the next tick, regardless of their random UUID ordering.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn begin_monitored_scan(&self) -> Result<Option<MonitoredSearchScan>, sqlx::Error> {
        let (enabled_before, through): (time::OffsetDateTime, Option<uuid::Uuid>) = sqlx::query_as(
            r"WITH boundary AS MATERIALIZED (
                      SELECT clock_timestamp() AS enabled_before
                  )
                  SELECT boundary.enabled_before,
                         (
                           SELECT id
                             FROM saved_searches
                            WHERE notify_new
                              AND monitor_enabled_at <= boundary.enabled_before
                              AND aero_participant_has_effective_workspace_access(
                                      workspace_id,
                                      participant_id
                                  )
                            ORDER BY id DESC
                            LIMIT 1
                         )
                    FROM boundary",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(through.map(|through| MonitoredSearchScan {
            enabled_before,
            through: SavedSearchId::from_uuid(through),
        }))
    }

    /// One bounded keyset page within a frozen [`MonitoredSearchScan`], across
    /// all owners. `after` is the last id from the preceding page. Returns each
    /// search's id, owner, workspace, query, and independent composite monitor
    /// cursor. Not owner-scoped: it is a server-internal background scan, not a
    /// user-facing read.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_monitored_page(
        &self,
        scan: MonitoredSearchScan,
        after: Option<SavedSearchId>,
        limit: i64,
    ) -> Result<Vec<MonitoredSearch>, sqlx::Error> {
        let rows = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                uuid::Uuid,
                uuid::Uuid,
                String,
                Option<time::OffsetDateTime>,
                Option<uuid::Uuid>,
            ),
        >(
            r"SELECT id, participant_id, workspace_id, query,
                     monitor_cursor_at, monitor_cursor_message_id
              FROM saved_searches
              WHERE notify_new
                AND ($1::uuid IS NULL OR id > $1)
                AND id <= $2
                AND monitor_enabled_at <= $3
                AND aero_participant_has_effective_workspace_access(
                        workspace_id,
                        participant_id
                    )
              ORDER BY id
              LIMIT $4",
        )
        .bind(after.map(|id| id.to_uuid()))
        .bind(scan.through.to_uuid())
        .bind(scan.enabled_before)
        .bind(limit.clamp(1, MAX_MONITORED_SEARCH_SCAN_PAGE))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, owner, workspace, query, cursor_at, cursor_message_id)| MonitoredSearch {
                    id: SavedSearchId::from_uuid(id),
                    owner: ParticipantId::from_uuid(owner),
                    workspace: WorkspaceId::from_uuid(workspace),
                    query,
                    cursor_at,
                    cursor_message_id: cursor_message_id.map(MessageId::from_uuid),
                },
            )
            .collect())
    }

    /// Deliver one monitor batch atomically.
    ///
    /// Membership governance, the workspace, and finally the saved-search row
    /// are locked in canonical order. This both serializes server instances and
    /// gives revocation a deterministic before/after relationship with delivery.
    /// A fixed enable-time floor plus bounded cursor overlap recovers messages
    /// that commit just after a polling snapshot; the durable delivery ledger
    /// removes prior deliveries before the cap is applied. Hits are ordered by
    /// `(created_at, id)`, so equal timestamps and capped batches drain without
    /// skips.
    ///
    /// Every notification uses a stable `UUIDv5` derived from
    /// `(saved_search, message, owner)`. The existing partial unique index on
    /// `(delivery_id, participant_id)` makes retries converge. Notification,
    /// delivery-ledger record, and cursor advancement commit together. The final
    /// projection repeats the complete effective-access boundary (live account,
    /// workspace/room membership, deactivation, and mandatory 2FA).
    ///
    /// Returns the number of newly inserted notification rows. A first run only
    /// establishes the baseline and returns zero.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`]. The transaction is rolled back on error.
    pub async fn deliver_monitored_batch(
        &self,
        id: SavedSearchId,
        expected_owner: ParticipantId,
        now: time::OffsetDateTime,
        cap: usize,
    ) -> Result<usize, sqlx::Error> {
        if cap == 0 {
            return Ok(0);
        }

        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        let workspace = sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT workspace_id
                FROM saved_searches
               WHERE id = $1
                 AND participant_id = $2
                 AND notify_new",
        )
        .bind(id.to_uuid())
        .bind(expected_owner.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(workspace) = workspace else {
            tx.commit().await?;
            return Ok(0);
        };
        let workspace = WorkspaceId::from_uuid(workspace);
        let workspace_exists =
            sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
                .bind(workspace.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !workspace_exists {
            tx.commit().await?;
            return Ok(0);
        }
        let authorized =
            sqlx::query_scalar::<_, bool>("SELECT aero_effective_workspace_access($1, $2)")
                .bind(workspace.to_uuid())
                .bind(expected_owner.to_uuid())
                .fetch_one(&mut *tx)
                .await?;
        if !authorized {
            tx.commit().await?;
            return Ok(0);
        }

        let locked: Option<LockedMonitor> = sqlx::query_as(
            r"SELECT workspace_id, query,
                     monitor_cursor_at AS cursor_at,
                     monitor_cursor_message_id AS cursor_message_id,
                     monitor_floor_at AS floor_at,
                     monitor_floor_message_id AS floor_message_id
                FROM saved_searches
               WHERE id = $1
                 AND participant_id = $2
                 AND workspace_id = $3
                 AND notify_new
               FOR UPDATE",
        )
        .bind(id.to_uuid())
        .bind(expected_owner.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(locked) = locked else {
            tx.commit().await?;
            return Ok(0);
        };
        debug_assert_eq!(locked.workspace_id, workspace.to_uuid());

        let Some(cursor_at) = locked.cursor_at else {
            sqlx::query(
                r"UPDATE saved_searches
                      SET monitor_cursor_at = $3,
                          monitor_cursor_message_id = $4,
                          monitor_floor_at = $3,
                          monitor_floor_message_id = $4
                    WHERE id = $1 AND participant_id = $2",
            )
            .bind(id.to_uuid())
            .bind(expected_owner.to_uuid())
            .bind(now)
            .bind(uuid::Uuid::nil())
            .execute(&mut *tx)
            .await?;
            sweep_monitor_delivery_ledger(&mut tx, id).await?;
            tx.commit().await?;
            return Ok(0);
        };

        let high_water = now.max(cursor_at);
        let floor_at = locked.floor_at.unwrap_or(cursor_at);
        let (lower_at, lower_message_uuid) =
            monitor_lower_bound(cursor_at, floor_at, locked.floor_message_id);
        let parsed = crate::parse_search_query(&locked.query);
        let capped_limit = cap.min(100);
        let fetch_limit = i64::try_from(capped_limit.saturating_add(1)).unwrap_or(101);
        let candidates: Vec<(uuid::Uuid, uuid::Uuid, uuid::Uuid, time::OffsetDateTime)> =
            sqlx::query_as(include_str!("saved_search/monitor_candidates.sql"))
                .bind(expected_owner.to_uuid())
                .bind(workspace.to_uuid())
                .bind(parsed.from.map(|participant| participant.to_uuid()))
                .bind(parsed.in_room.map(|room| room.to_uuid()))
                .bind(parsed.before.map(|message| message.to_uuid()))
                .bind(parsed.after.map(|message| message.to_uuid()))
                .bind(parsed.after_ts)
                .bind(parsed.before_ts)
                .bind(&parsed.terms)
                .bind(lower_at)
                .bind(lower_message_uuid)
                .bind(high_water)
                .bind(id.to_uuid())
                .bind(fetch_limit)
                .fetch_all(&mut *tx)
                .await?;

        let was_capped = candidates.len() > capped_limit;
        let processed = &candidates[..candidates.len().min(capped_limit)];
        let inserted = if processed.is_empty() {
            0
        } else {
            let notification_ids: Vec<_> = processed
                .iter()
                .map(|_| NotificationId::new().to_uuid())
                .collect();
            let message_ids: Vec<_> = processed.iter().map(|row| row.0).collect();
            let delivery_ids: Vec<_> = processed
                .iter()
                .map(|row| monitor_delivery_id(id, MessageId::from_uuid(row.0), expected_owner))
                .collect();
            let created_at = time::OffsetDateTime::now_utc();
            let (inserted, _recorded): (i64, i64) =
                sqlx::query_as(include_str!("saved_search/monitor_deliver.sql"))
                    .bind(&notification_ids)
                    .bind(&message_ids)
                    .bind(&delivery_ids)
                    .bind(expected_owner.to_uuid())
                    .bind(NotificationKind::SavedSearch.as_str())
                    .bind(created_at)
                    .bind(aero_common::model::importance_for(
                        &NotificationKind::SavedSearch,
                    ))
                    .bind(workspace.to_uuid())
                    .bind(id.to_uuid())
                    .fetch_one(&mut *tx)
                    .await?;
            usize::try_from(inserted).unwrap_or(usize::MAX)
        };

        let (next_at, next_message_id) = if was_capped {
            let last = processed
                .last()
                .expect("a capped non-zero batch has a processed row");
            let advances = last.3 > cursor_at
                || (last.3 == cursor_at
                    && locked
                        .cursor_message_id
                        .is_some_and(|cursor| last.0 > cursor));
            if advances {
                (last.3, Some(last.0))
            } else {
                (cursor_at, locked.cursor_message_id)
            }
        } else {
            (high_water, Some(uuid::Uuid::nil()))
        };
        sqlx::query(
            r"UPDATE saved_searches
                  SET monitor_cursor_at = $3,
                      monitor_cursor_message_id = $4
                WHERE id = $1 AND participant_id = $2",
        )
        .bind(id.to_uuid())
        .bind(expected_owner.to_uuid())
        .bind(next_at)
        .bind(next_message_id)
        .execute(&mut *tx)
        .await?;
        sweep_monitor_delivery_ledger(&mut tx, id).await?;
        tx.commit().await?;
        Ok(inserted)
    }

    /// Stamp `now` as this saved search's `last_run_at` (owner-scoped) and return
    /// the PREVIOUS `last_run_at` — the cursor for a "new since I last ran this"
    /// delta. `None` when it had never been run before (or the row is absent).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn mark_run(
        &self,
        id: SavedSearchId,
        participant: ParticipantId,
        now: time::OffsetDateTime,
    ) -> Result<Option<time::OffsetDateTime>, sqlx::Error> {
        // The CTE captures the prior value before the UPDATE overwrites it, so a
        // single round-trip both advances the cursor and returns the old one.
        let row: Option<(Option<time::OffsetDateTime>,)> = sqlx::query_as(
            r"WITH prev AS (
                  SELECT last_run_at FROM saved_searches
                   WHERE id = $1 AND participant_id = $2
              )
              UPDATE saved_searches s
                 SET last_run_at = $3
                FROM prev
               WHERE s.id = $1 AND s.participant_id = $2
           RETURNING prev.last_run_at",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(prev,)| prev))
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored saved_search
/// ```
#[cfg(test)]
#[path = "saved_search/owner_scope_tests.rs"]
mod owner_scope_tests;

#[cfg(test)]
mod db_tests {
    use super::*;

    /// The reserved all-zero default workspace, guaranteed to exist by migration
    /// 0006's backfill — reused so the saved-search rows are well-scoped.
    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

    pub(super) fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway owner participant so the test is self-contained.
    pub(super) async fn owner(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("saved-search-owner-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        sqlx::query(
            r"INSERT INTO workspace_members
                   (workspace_id, participant_id, role)
               VALUES ($1, $2, 'member')",
        )
        .bind(default_ws().to_uuid())
        .bind(id.to_uuid())
        .execute(p)
        .await
        .expect("join default workspace");
        id
    }

    pub(super) fn default_ws() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"))
    }

    /// `mark_run` returns `None` on the first run (never run before) and the
    /// previous run's timestamp on the next — the cursor for the new-since delta.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn mark_run_returns_previous_timestamp() {
        let p = pool();
        let repo = SavedSearchRepo::new(p.clone());
        let ws = default_ws();
        let owner = owner(&p).await;
        let id = repo
            .create(owner, ws, "deploys", "deploy failed")
            .await
            .unwrap();

        let t1 = time::OffsetDateTime::now_utc();
        assert!(
            repo.mark_run(id, owner, t1).await.unwrap().is_none(),
            "first run has no previous cursor",
        );

        let t2 = t1 + time::Duration::seconds(30);
        let prev = repo
            .mark_run(id, owner, t2)
            .await
            .unwrap()
            .expect("second run sees the first");
        assert!(
            (prev - t1).abs() < time::Duration::seconds(1),
            "second run returns the first run's timestamp (got {prev}, expected ~{t1})",
        );

        // A stranger cannot advance another user's cursor (owner-scoped).
        assert!(
            repo.mark_run(id, ParticipantId::new(), t2)
                .await
                .unwrap()
                .is_none(),
            "stranger's mark_run matches no row",
        );

        sqlx::query("DELETE FROM saved_searches WHERE participant_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
