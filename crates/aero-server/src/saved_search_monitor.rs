//! Periodic saved-search digest dispatcher (ROADMAP5 方向三).
//!
//! A saved search with `notify_new = true` becomes a standing monitor: this
//! background loop periodically re-runs each such query (effective-access-scoped as its
//! owner) and inserts an inbox notification for every match newer than its
//! independent composite monitor cursor. So an owner who saved
//! `from:@ops deploy failed` with monitoring on is pinged when a new matching
//! message arrives — without polling the search themselves.
//!
//! The per-search work is factored into [`process_monitored_search`] (free
//! function over the storage transaction) so the new-match → notification logic
//! is db-testable without spinning up the whole server.

use aero_storage::saved_search::MAX_MONITORED_SEARCH_SCAN_PAGE;
use aero_storage::{MonitoredSearch, SavedSearchRepo};

/// How many notifications one monitored search may emit per dispatcher tick. A
/// query that suddenly matches a flood of messages (e.g. just enabled on a busy
/// term) should not bury the owner's inbox in one go; the composite cursor stops
/// at the last emitted match and subsequent ticks drain the remainder.
const MAX_NOTIFICATIONS_PER_RUN: usize = 20;

/// Re-run one monitored saved search and notify its owner of new matches.
///
/// Returns the number of newly inserted notifications. A newly-enabled search
/// first stamps a baseline and notifies nothing, so monitoring never backfills
/// history. Storage row-locks the search and commits stable-id notification
/// inserts with the `(created_at, message_id)` cursor, making concurrent instances
/// and crash retries converge.
///
/// # Errors
/// Propagates a [`sqlx::Error`] from any of the underlying repo calls. The caller
/// (the dispatcher loop) logs and swallows so one bad search never wedges the run.
pub async fn process_monitored_search(
    saved: &SavedSearchRepo,
    item: &MonitoredSearch,
    now: time::OffsetDateTime,
    cap: usize,
) -> Result<usize, sqlx::Error> {
    saved
        .deliver_monitored_batch(item.id, item.owner, now, cap)
        .await
}

/// Background loop: every `interval_secs`, process every monitored saved search.
/// The global work-list is complete but scanned in bounded id-keyset pages, so
/// one query/allocation cannot materialize the full table and higher ids are not
/// starved. Per-owner enable quotas bound each tenant's contribution, and the
/// interval's `Skip` policy prevents overlapping runs. Best-effort throughout —
/// a per-search error is logged and the loop continues; it honors the shutdown
/// token. Spawned by the server binary.
pub async fn run_saved_search_monitor(
    state: crate::state::AppState,
    interval_secs: u64,
    cancel: tokio_util::sync::CancellationToken,
) {
    let saved = SavedSearchRepo::new(state.pg.clone());
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await; // skip the immediate first tick
    tracing::info!(interval_secs, "saved-search monitor started");

    'ticks: loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("saved-search monitor shutting down");
                return;
            }
            _ = tick.tick() => {}
        }

        let now = time::OffsetDateTime::now_utc();
        let mut total = 0usize;
        let mut searches = 0usize;
        let mut after = None;
        let scan = tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("saved-search monitor shutting down");
                return;
            }
            result = saved.begin_monitored_scan() => {
                match result {
                    Ok(Some(scan)) => scan,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!(
                            error = ?e,
                            "saved-search monitor: begin_monitored_scan failed"
                        );
                        continue;
                    }
                }
            }
        };
        loop {
            let monitored = tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!("saved-search monitor shutting down");
                    return;
                }
                result = saved.list_monitored_page(
                    scan,
                    after,
                    MAX_MONITORED_SEARCH_SCAN_PAGE,
                ) => {
                    match result {
                        Ok(page) => page,
                        Err(e) => {
                            tracing::warn!(
                                error = ?e,
                                "saved-search monitor: list_monitored_page failed"
                            );
                            continue 'ticks;
                        }
                    }
                }
            };
            let Some(last_id) = monitored.last().map(|item| item.id) else {
                break;
            };
            searches += monitored.len();
            for item in &monitored {
                if cancel.is_cancelled() {
                    tracing::info!("saved-search monitor shutting down");
                    return;
                }
                match process_monitored_search(&saved, item, now, MAX_NOTIFICATIONS_PER_RUN).await {
                    Ok(n) => total += n,
                    Err(e) => {
                        tracing::warn!(error = ?e, saved_search = %item.id, "saved-search monitor: run failed");
                    }
                }
            }
            after = Some(last_id);
        }
        if total > 0 {
            tracing::info!(
                notified = total,
                searches,
                "saved-search monitor: new matches notified"
            );
        }
    }
}

/// PG-gated integration tests:
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-server --lib -- --ignored saved_search_monitor
/// ```
#[cfg(test)]
#[path = "saved_search_monitor/notification_tests.rs"]
mod notification_tests;

#[cfg(test)]
mod db_tests {
    use super::{process_monitored_search, MAX_MONITORED_SEARCH_SCAN_PAGE};
    use aero_common::{ParticipantId, RoomId, WorkspaceId};
    use aero_storage::{MonitoredSearch, NotificationRepo, PgPool, SavedSearchRepo};

    pub(super) fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    pub(super) async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("ssm-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    pub(super) async fn workspace(p: &PgPool, creator: ParticipantId) -> WorkspaceId {
        let id = WorkspaceId::new();
        let mut tx = p.begin().await.expect("begin workspace fixture");
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id.to_uuid())
        .bind(format!("ssm-workspace-{id}"))
        .bind(format!("ssm-{id}"))
        .bind(creator.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace");
        sqlx::query(
            "INSERT INTO workspace_members
                 (workspace_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(id.to_uuid())
        .bind(creator.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace owner");
        tx.commit().await.expect("commit workspace fixture");
        id
    }

    pub(super) async fn room(p: &PgPool, creator: ParticipantId, ws: WorkspaceId) -> RoomId {
        sqlx::query(
            "INSERT INTO workspace_members
                 (workspace_id, participant_id, role)
             VALUES ($1, $2, 'member')
             ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(ws.to_uuid())
        .bind(creator.to_uuid())
        .execute(p)
        .await
        .expect("join workspace");
        let id = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id) VALUES ($1,'group',$2,$3,$4)",
        )
        .bind(id.to_uuid())
        .bind(format!("ssm-room-{id}"))
        .bind(creator.to_uuid())
        .bind(ws.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role) VALUES ($1,$2,'member')",
        )
        .bind(id.to_uuid())
        .bind(creator.to_uuid())
        .execute(p)
        .await
        .expect("join");
        id
    }

    pub(super) async fn insert_match(
        p: &PgPool,
        room: RoomId,
        sender: ParticipantId,
        needle: &str,
        created_at: time::OffsetDateTime,
    ) -> aero_common::MessageId {
        let id = aero_common::MessageId::new();
        let text = format!("{needle} match");
        sqlx::query(
            "INSERT INTO messages
                 (id, room_id, sender_id, blocks, searchable_text, created_at)
             VALUES ($1, $2, $3, $4::jsonb, $5, $6)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .bind(serde_json::json!([{ "type": "text", "content": text }]).to_string())
        .bind(&text)
        .bind(created_at)
        .execute(p)
        .await
        .expect("insert matching message");
        id
    }

    async fn monitored_page(saved: &SavedSearchRepo) -> Vec<MonitoredSearch> {
        let scan = saved
            .begin_monitored_scan()
            .await
            .expect("begin monitored scan")
            .expect("at least one monitored search");
        saved
            .list_monitored_page(scan, None, MAX_MONITORED_SEARCH_SCAN_PAGE)
            .await
            .expect("list monitored page")
    }

    async fn monitored(saved: &SavedSearchRepo, id: aero_common::SavedSearchId) -> MonitoredSearch {
        monitored_page(saved)
            .await
            .into_iter()
            .find(|item| item.id == id)
            .expect("saved search is monitored")
    }

    async fn rewind(p: &PgPool, id: aero_common::SavedSearchId, cursor: time::OffsetDateTime) {
        sqlx::query(
            "UPDATE saved_searches
                SET monitor_cursor_at = $2,
                    monitor_cursor_message_id = NULL
              WHERE id = $1",
        )
        .bind(id.to_uuid())
        .bind(cursor)
        .execute(p)
        .await
        .expect("rewind monitor cursor");
    }

    /// When more new matches exist than the per-run cap, the run notifies `cap` of
    /// them (oldest-first) and advances the cursor ONLY past those — so the rest
    /// are picked up on the NEXT run rather than silently skipped. Guards the
    /// no-data-loss property of the capped path.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn cap_does_not_silently_drop_matches_drains_over_runs() {
        let p = pool();
        let notifications = NotificationRepo::new(p.clone());
        let saved = SavedSearchRepo::new(p.clone());

        let owner = participant(&p).await;
        let ws = workspace(&p, owner).await;
        let r = room(&p, owner, ws).await;
        let needle = format!("zqcaptoken{}", ParticipantId::new());

        // Baseline cursor: everything below is "new" relative to it.
        let base = time::OffsetDateTime::now_utc();

        // Insert 3 matching messages at the same timestamp. The UUID tiebreaker
        // must carry the capped cursor across all three without skipping one.
        let mut ids = Vec::new();
        for i in 0..3 {
            let ts = base + time::Duration::seconds(1);
            let id = aero_common::MessageId::new();
            sqlx::query(
                "INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text, created_at) \
                 VALUES ($1,$2,$3,$4::jsonb,$5,$6)",
            )
            .bind(id.to_uuid())
            .bind(r.to_uuid())
            .bind(owner.to_uuid())
            .bind(serde_json::json!([{ "type": "text", "content": format!("{needle} m{i}") }]).to_string())
            .bind(format!("{needle} m{i}"))
            .bind(ts)
            .execute(&p)
            .await
            .expect("insert match");
            ids.push(id);
        }

        let sid = saved
            .create(owner, ws, "cap", &needle)
            .await
            .expect("create");
        // Enable monitoring so the paged re-read below finds it.
        saved
            .set_notify_new(sid, owner, true)
            .await
            .expect("enable monitoring");
        let baseline_item = monitored_page(&saved)
            .await
            .into_iter()
            .find(|candidate| candidate.id == sid)
            .expect("monitored");
        assert_eq!(
            process_monitored_search(&saved, &baseline_item, base, 2)
                .await
                .expect("baseline"),
            0
        );
        let item = MonitoredSearch {
            id: sid,
            owner,
            workspace: ws,
            query: needle.clone(),
            cursor_at: Some(base),
            cursor_message_id: None,
        };

        // Run 1 with cap=2: notifies the 2 OLDEST, cursor advances only to the 2nd.
        let r1 = process_monitored_search(&saved, &item, base + time::Duration::seconds(100), 2)
            .await
            .expect("run1");
        assert_eq!(r1, 2, "first capped run notifies exactly the cap");

        // Re-read the cursor and run again: the 3rd (un-notified) match is caught.
        let monitored = monitored_page(&saved).await;
        let item2 = monitored
            .into_iter()
            .find(|m| m.id == sid)
            .expect("still monitored");
        let r2 = process_monitored_search(&saved, &item2, base + time::Duration::seconds(200), 2)
            .await
            .expect("run2");
        assert_eq!(
            r2, 1,
            "the next run drains the remaining match — none dropped"
        );

        // All 3 matches ended up notified across the two runs (no silent loss).
        let inbox = notifications
            .list(owner, None, false, Some(50))
            .await
            .expect("inbox");
        for id in &ids {
            assert!(
                inbox.iter().any(|n| n.message_id == *id
                    && matches!(n.kind, aero_common::NotificationKind::SavedSearch)),
                "match {id} was eventually notified",
            );
        }
    }

    /// Two server instances may race the same stale work-list row. The saved
    /// search row lock serializes them, while the stable delivery id also makes a
    /// deliberately rewound (crash-style stale) cursor converge without another
    /// inbox row.
    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0188"]
    async fn concurrent_workers_and_stale_cursor_retry_are_idempotent() {
        let p = pool();
        let saved = SavedSearchRepo::new(p.clone());
        let owner = participant(&p).await;
        let ws = workspace(&p, owner).await;
        let room = room(&p, owner, ws).await;
        let needle = format!("zqconcurrent{}", ParticipantId::new());
        let sid = saved.create(owner, ws, "race", &needle).await.unwrap();
        saved.set_notify_new(sid, owner, true).await.unwrap();

        let base = time::OffsetDateTime::now_utc();
        let item = monitored(&saved, sid).await;
        assert_eq!(
            process_monitored_search(&saved, &item, base, 20)
                .await
                .unwrap(),
            0
        );
        let message =
            insert_match(&p, room, owner, &needle, base + time::Duration::seconds(1)).await;
        let stale = monitored(&saved, sid).await;
        let repo_a = saved.clone();
        let repo_b = saved.clone();
        let item_a = stale.clone();
        let item_b = stale;
        let high_water = base + time::Duration::seconds(30);
        let (first, second) = tokio::join!(
            process_monitored_search(&repo_a, &item_a, high_water, 20),
            process_monitored_search(&repo_b, &item_b, high_water, 20),
        );
        assert_eq!(
            first.unwrap() + second.unwrap(),
            1,
            "only one racing worker materializes the notification"
        );

        // Simulate a pre-fix crash window: the notification survived, but the
        // cursor appears stale. UUIDv5 + the notification unique index must make
        // the replay a no-op while still repairing cursor progress.
        rewind(&p, sid, base).await;
        assert_eq!(
            process_monitored_search(&saved, &item_a, high_water + time::Duration::seconds(1), 20,)
                .await
                .unwrap(),
            0
        );
        let row: (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COUNT(delivery_id)
               FROM notifications
              WHERE participant_id = $1
                AND message_id = $2
                AND kind = 'saved_search'",
        )
        .bind(owner.to_uuid())
        .bind(message.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(row, (1, 1), "one durable, stable-id notification remains");

        sqlx::query(
            "UPDATE saved_search_monitor_deliveries
                SET delivered_at = CURRENT_TIMESTAMP - INTERVAL '2 days'
              WHERE saved_search_id = $1",
        )
        .bind(sid.to_uuid())
        .execute(&p)
        .await
        .unwrap();
        process_monitored_search(&saved, &item_a, high_water + time::Duration::seconds(2), 20)
            .await
            .unwrap();
        let ledger_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
               FROM saved_search_monitor_deliveries
              WHERE saved_search_id = $1",
        )
        .bind(sid.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(ledger_rows, 0, "entries older than 24 hours are swept");
    }

    /// A message transaction may allocate `created_at` before the monitor's
    /// snapshot and commit only after the monitor advances its high-water mark.
    /// The bounded overlap must recover it on the next tick.
    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0188"]
    async fn overlap_recovers_message_committed_after_monitor_snapshot() {
        let p = pool();
        let saved = SavedSearchRepo::new(p.clone());
        let owner = participant(&p).await;
        let ws = workspace(&p, owner).await;
        let room = room(&p, owner, ws).await;
        let needle = format!("zqlatecommit{}", ParticipantId::new());
        let sid = saved.create(owner, ws, "late", &needle).await.unwrap();
        saved.set_notify_new(sid, owner, true).await.unwrap();
        let base = time::OffsetDateTime::now_utc();
        let item = monitored(&saved, sid).await;
        process_monitored_search(&saved, &item, base, 20)
            .await
            .unwrap();

        let message = aero_common::MessageId::new();
        let text = format!("{needle} delayed");
        let mut writer = p.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO messages
                 (id, room_id, sender_id, blocks, searchable_text, created_at)
             VALUES ($1, $2, $3, $4::jsonb, $5, $6)",
        )
        .bind(message.to_uuid())
        .bind(room.to_uuid())
        .bind(owner.to_uuid())
        .bind(serde_json::json!([{ "type": "text", "content": text }]).to_string())
        .bind(&text)
        .bind(base + time::Duration::seconds(1))
        .execute(&mut *writer)
        .await
        .unwrap();

        let high_water = base + time::Duration::seconds(30);
        assert_eq!(
            process_monitored_search(&saved, &item, high_water, 20)
                .await
                .unwrap(),
            0,
            "the uncommitted message is invisible to this snapshot"
        );
        writer.commit().await.unwrap();

        assert_eq!(
            process_monitored_search(&saved, &item, high_water + time::Duration::seconds(1), 20,)
                .await
                .unwrap(),
            1,
            "lookback recovers the late commit behind the high-water cursor"
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
               FROM notifications
              WHERE participant_id = $1
                AND message_id = $2
                AND kind = 'saved_search'",
        )
        .bind(owner.to_uuid())
        .bind(message.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(count, 1);
    }

    /// The monitor's search and its final insert both use effective access. Every
    /// revocation dimension suppresses a pending notification even though the
    /// saved search and matching message still exist.
    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0188"]
    async fn final_insert_rechecks_complete_effective_access() {
        let p = pool();
        let saved = SavedSearchRepo::new(p.clone());
        let manager = participant(&p).await;
        let owner = participant(&p).await;
        let ws = workspace(&p, manager).await;
        let room = room(&p, owner, ws).await;
        let needle = format!("zqrevoked{}", ParticipantId::new());
        let sid = saved.create(owner, ws, "revoked", &needle).await.unwrap();
        saved.set_notify_new(sid, owner, true).await.unwrap();
        let base = time::OffsetDateTime::now_utc();
        let item = monitored(&saved, sid).await;
        process_monitored_search(&saved, &item, base, 20)
            .await
            .unwrap();
        insert_match(&p, room, owner, &needle, base + time::Duration::seconds(1)).await;
        let high_water = base + time::Duration::seconds(30);

        sqlx::query(
            "DELETE FROM room_members
              WHERE room_id = $1 AND participant_id = $2",
        )
        .bind(room.to_uuid())
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .unwrap();
        assert_eq!(
            process_monitored_search(&saved, &item, high_water, 20)
                .await
                .unwrap(),
            0,
            "room revocation suppresses delivery"
        );
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(room.to_uuid())
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .unwrap();

        rewind(&p, sid, base).await;
        sqlx::query(
            "INSERT INTO workspace_deactivations
                 (workspace_id, participant_id, deactivated_by)
             VALUES ($1, $2, $3)",
        )
        .bind(ws.to_uuid())
        .bind(owner.to_uuid())
        .bind(manager.to_uuid())
        .execute(&p)
        .await
        .unwrap();
        assert_eq!(
            process_monitored_search(&saved, &item, high_water, 20)
                .await
                .unwrap(),
            0,
            "workspace deactivation suppresses delivery"
        );
        sqlx::query(
            "DELETE FROM workspace_deactivations
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(ws.to_uuid())
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .unwrap();

        rewind(&p, sid, base).await;
        sqlx::query(
            "DELETE FROM workspace_members
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(ws.to_uuid())
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .unwrap();
        assert_eq!(
            process_monitored_search(&saved, &item, high_water, 20)
                .await
                .unwrap(),
            0,
            "workspace membership revocation suppresses delivery"
        );
        sqlx::query(
            "INSERT INTO workspace_members
                 (workspace_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(ws.to_uuid())
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .unwrap();

        rewind(&p, sid, base).await;
        sqlx::query(
            "INSERT INTO totp_secrets (participant_id, secret, activated)
             VALUES ($1, $2, true)",
        )
        .bind(manager.to_uuid())
        .bind(format!("saved-search-manager-{manager}"))
        .execute(&p)
        .await
        .unwrap();
        sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
            .bind(ws.to_uuid())
            .execute(&p)
            .await
            .unwrap();
        assert_eq!(
            process_monitored_search(&saved, &item, high_water, 20)
                .await
                .unwrap(),
            0,
            "mandatory 2FA suppresses the unenrolled saved-search owner"
        );
        sqlx::query("UPDATE workspaces SET require_2fa = false WHERE id = $1")
            .bind(ws.to_uuid())
            .execute(&p)
            .await
            .unwrap();

        rewind(&p, sid, base).await;
        sqlx::query("UPDATE participants SET deleted_at = now() WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .unwrap();
        assert_eq!(
            process_monitored_search(&saved, &item, high_water, 20)
                .await
                .unwrap(),
            0,
            "deleted account suppresses delivery"
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
               FROM notifications
              WHERE participant_id = $1 AND kind = 'saved_search'",
        )
        .bind(owner.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(count, 0);
    }
}
