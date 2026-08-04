//! Transaction-owned saved-search monitor authorization and quota.

use aero_common::{ParticipantId, SavedSearchId, WorkspaceId};

use super::SavedSearchRepo;

/// Deterministic outcome of a monitor toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedSearchMonitorUpdate {
    Updated,
    NotFound,
    LimitReached,
    Forbidden,
}

impl SavedSearchRepo {
    /// Enable or disable a standing saved-search monitor under the same
    /// membership-governance transaction that owns revocation.
    ///
    /// Enabling requires current effective workspace access and consumes one of
    /// the owner's globally bounded monitor slots. Disabling remains available
    /// after revocation so a user can always remove background state.
    pub async fn set_notify_new_capped_authorized(
        &self,
        id: SavedSearchId,
        participant: ParticipantId,
        notify_new: bool,
        max_monitored: i64,
    ) -> Result<SavedSearchMonitorUpdate, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        // The owner lock serializes concurrent enables across all workspaces,
        // making the global per-owner count a real quota.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(participant.to_uuid().to_string())
            .execute(&mut *tx)
            .await?;

        let workspace = sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT workspace_id
                FROM saved_searches
               WHERE id = $1 AND participant_id = $2",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(workspace) = workspace else {
            tx.commit().await?;
            return Ok(SavedSearchMonitorUpdate::NotFound);
        };
        let workspace = WorkspaceId::from_uuid(workspace);

        // Match global governance order: global fence -> workspace -> resource.
        let workspace_exists =
            sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
                .bind(workspace.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !workspace_exists {
            tx.commit().await?;
            return Ok(SavedSearchMonitorUpdate::NotFound);
        }
        let currently_enabled = sqlx::query_scalar::<_, bool>(
            r"SELECT notify_new
                FROM saved_searches
               WHERE id = $1
                 AND participant_id = $2
                 AND workspace_id = $3
               FOR UPDATE",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(currently_enabled) = currently_enabled else {
            tx.commit().await?;
            return Ok(SavedSearchMonitorUpdate::NotFound);
        };

        if notify_new
            && !crate::workspace::members::effective_workspace_access_in_tx(
                &mut tx,
                workspace,
                participant,
            )
            .await?
        {
            tx.commit().await?;
            return Ok(SavedSearchMonitorUpdate::Forbidden);
        }
        if notify_new && !currently_enabled {
            let count = sqlx::query_scalar::<_, i64>(
                r"SELECT count(*)
                    FROM saved_searches
                   WHERE participant_id = $1 AND notify_new",
            )
            .bind(participant.to_uuid())
            .fetch_one(&mut *tx)
            .await?;
            if count >= max_monitored {
                tx.commit().await?;
                return Ok(SavedSearchMonitorUpdate::LimitReached);
            }
        }

        sqlx::query(
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
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(SavedSearchMonitorUpdate::Updated)
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::saved_search::MAX_MONITORED_SEARCH_SCAN_PAGE;
    use aero_common::{WorkspaceId, WorkspaceRole};
    use sqlx::PgPool;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
        let participant = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(participant.to_uuid())
            .bind(format!("saved-monitor-{label}-{participant}"))
            .execute(pool)
            .await
            .unwrap();
        participant
    }

    async fn fixture(
        pool: &PgPool,
        label: &str,
    ) -> (WorkspaceId, ParticipantId, ParticipantId, SavedSearchId) {
        let owner = participant(pool, "owner").await;
        let member = participant(pool, "member").await;
        let workspace = crate::WorkspaceRepo::new(pool.clone())
            .create(
                format!("Saved monitor {label}"),
                format!("saved-monitor-{label}-{}", WorkspaceId::new()),
                owner,
            )
            .await
            .unwrap()
            .id;
        crate::WorkspaceRepo::new(pool.clone())
            .add_member_authorized(workspace, owner, member, WorkspaceRole::Member)
            .await
            .unwrap();
        let search = SavedSearchRepo::new(pool.clone())
            .create(member, workspace, "deploys", "deploy failed")
            .await
            .unwrap();
        (workspace, owner, member, search)
    }

    async fn monitored_ids(repo: &SavedSearchRepo) -> Vec<SavedSearchId> {
        let Some(scan) = repo.begin_monitored_scan().await.unwrap() else {
            return Vec::new();
        };
        repo.list_monitored_page(scan, None, MAX_MONITORED_SEARCH_SCAN_PAGE)
            .await
            .unwrap()
            .into_iter()
            .map(|item| item.id)
            .collect()
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations through 0220"]
    async fn monitor_enable_requires_effective_access_and_dispatch_list_filters_revoked_rows() {
        let pool = pool();
        let repo = SavedSearchRepo::new(pool.clone());
        let (workspace, owner, member, search) = fixture(&pool, "access").await;

        assert_eq!(
            repo.set_notify_new_capped_authorized(search, member, true, 50)
                .await
                .unwrap(),
            SavedSearchMonitorUpdate::Updated
        );
        assert!(monitored_ids(&repo).await.contains(&search));

        crate::WorkspaceRepo::new(pool.clone())
            .remove_member_authorized(workspace, owner, member)
            .await
            .unwrap();
        assert!(!monitored_ids(&repo).await.contains(&search));
        assert_eq!(
            repo.set_notify_new_capped_authorized(search, member, true, 50)
                .await
                .unwrap(),
            SavedSearchMonitorUpdate::Forbidden
        );

        sqlx::query("UPDATE saved_searches SET notify_new = false WHERE id = $1")
            .bind(search.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let raw_enable = sqlx::query("UPDATE saved_searches SET notify_new = true WHERE id = $1")
            .bind(search.to_uuid())
            .execute(&pool)
            .await
            .expect_err("raw monitor enable after revocation must be rejected");
        assert_eq!(
            raw_enable
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("42501")
        );
        assert!(
            sqlx::query("UPDATE saved_searches SET query = $2 WHERE id = $1")
                .bind(search.to_uuid())
                .bind("x".repeat(2_001))
                .execute(&pool)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations through 0220"]
    async fn concurrent_membership_revocation_wins_before_monitor_enable() {
        let pool = pool();
        let repo = SavedSearchRepo::new(pool.clone());
        let (workspace, _owner, member, search) = fixture(&pool, "race").await;

        let mut revoke = pool.begin().await.unwrap();
        sqlx::query("SELECT aero_lock_all_membership_governance()")
            .execute(&mut *revoke)
            .await
            .unwrap();
        sqlx::query("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .execute(&mut *revoke)
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM workspace_members WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(member.to_uuid())
        .execute(&mut *revoke)
        .await
        .unwrap();

        let contender = tokio::spawn(async move {
            repo.set_notify_new_capped_authorized(search, member, true, 50)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
        assert!(
            !contender.is_finished(),
            "monitor enable must wait behind membership governance"
        );
        revoke.commit().await.unwrap();
        assert_eq!(
            contender.await.unwrap().unwrap(),
            SavedSearchMonitorUpdate::Forbidden
        );
        assert!(!sqlx::query_scalar::<_, bool>(
            "SELECT notify_new FROM saved_searches WHERE id = $1",
        )
        .bind(search.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap());
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations through 0220"]
    async fn monitored_work_list_keyset_pages_are_bounded_and_complete() {
        let pool = pool();
        let repo = SavedSearchRepo::new(pool.clone());
        let (workspace, _owner, member, first) = fixture(&pool, "scan-pages").await;
        let mut expected = vec![first];
        for index in 0..4 {
            expected.push(
                repo.create(
                    member,
                    workspace,
                    &format!("monitor-{index}"),
                    &format!("needle-{index}"),
                )
                .await
                .unwrap(),
            );
        }
        for search in &expected {
            assert_eq!(
                repo.set_notify_new_capped_authorized(*search, member, true, 50)
                    .await
                    .unwrap(),
                SavedSearchMonitorUpdate::Updated
            );
        }

        let scan = repo
            .begin_monitored_scan()
            .await
            .unwrap()
            .expect("initial monitored set");
        // Create and enable an id below the frozen high-water after the scan
        // starts. The enable-time bound, not UUID ordering alone, must defer it.
        let deferred_uuid = uuid::Uuid::from_u128(scan.through.to_uuid().as_u128() / 2);
        let deferred = SavedSearchId::from_uuid(deferred_uuid);
        sqlx::query(
            r"INSERT INTO saved_searches
                  (id, participant_id, workspace_id, name, query)
               VALUES ($1, $2, $3, 'deferred', 'deferred needle')",
        )
        .bind(deferred.to_uuid())
        .bind(member.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        assert_eq!(
            repo.set_notify_new_capped_authorized(deferred, member, true, 50)
                .await
                .unwrap(),
            SavedSearchMonitorUpdate::Updated
        );

        let mut after = None;
        let mut scanned: Vec<SavedSearchId> = Vec::new();
        let mut terminated = false;
        for _ in 0..1_000 {
            let page = repo.list_monitored_page(scan, after, 2).await.unwrap();
            assert!(page.len() <= 2, "one query never exceeds its page cap");
            let Some(last) = page.last().map(|item| item.id) else {
                terminated = true;
                break;
            };
            if let (Some(previous), Some(first_in_page)) =
                (scanned.last().copied(), page.first().map(|item| item.id))
            {
                assert!(
                    first_in_page.to_uuid() > previous.to_uuid(),
                    "id keyset advances strictly"
                );
            }
            scanned.extend(page.iter().map(|item| item.id));
            after = Some(last);
        }
        assert!(terminated, "a frozen scan reaches an empty terminal page");
        for search in expected {
            assert!(
                scanned.contains(&search),
                "paged dispatcher scan includes enabled search {search}"
            );
        }
        assert!(
            !scanned.contains(&deferred),
            "newly enabled rows wait for the next scan"
        );
        assert!(
            monitored_ids(&repo).await.contains(&deferred),
            "the next scan includes the deferred monitor"
        );
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations through 0220"]
    async fn monitor_delivery_holds_governance_before_saved_search_row() {
        let pool = pool();
        let repo = SavedSearchRepo::new(pool.clone());
        let (workspace, _owner, member, search) = fixture(&pool, "delivery-order").await;
        assert_eq!(
            repo.set_notify_new_capped_authorized(search, member, true, 50)
                .await
                .unwrap(),
            SavedSearchMonitorUpdate::Updated
        );

        // Hold only the aggregate row. A correctly ordered delivery acquires
        // membership governance + workspace first, then waits here.
        let mut aggregate_guard = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM saved_searches WHERE id = $1 FOR UPDATE")
            .bind(search.to_uuid())
            .execute(&mut *aggregate_guard)
            .await
            .unwrap();

        let delivery_repo = repo.clone();
        let delivery = tokio::spawn(async move {
            delivery_repo
                .deliver_monitored_batch(search, member, time::OffsetDateTime::now_utc(), 20)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !delivery.is_finished(),
            "delivery waits for the saved-search aggregate"
        );
        let workspace_lock =
            sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE NOWAIT")
                .bind(workspace.to_uuid())
                .execute(&pool)
                .await
                .expect_err("delivery must already hold the workspace lock");
        assert_eq!(
            workspace_lock
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("55P03")
        );

        let revoke_pool = pool.clone();
        let mut revoke = tokio::spawn(async move {
            sqlx::query(
                "DELETE FROM workspace_members
                  WHERE workspace_id = $1 AND participant_id = $2",
            )
            .bind(workspace.to_uuid())
            .bind(member.to_uuid())
            .execute(&revoke_pool)
            .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut revoke)
                .await
                .is_err(),
            "membership revocation waits behind the in-flight delivery"
        );

        aggregate_guard.commit().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), delivery)
            .await
            .expect("delivery unblocked")
            .expect("delivery task")
            .expect("delivery transaction");
        tokio::time::timeout(std::time::Duration::from_secs(3), revoke)
            .await
            .expect("revocation unblocked")
            .expect("revocation task")
            .expect("revocation transaction");
        assert!(!monitored_ids(&repo).await.contains(&search));
    }
}
