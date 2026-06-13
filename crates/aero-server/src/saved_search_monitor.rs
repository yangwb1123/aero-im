//! Periodic saved-search digest dispatcher (ROADMAP5 方向三).
//!
//! A saved search with `notify_new = true` becomes a standing monitor: this
//! background loop periodically re-runs each such query (membership-scoped as its
//! owner) and inserts an inbox notification for every match newer than the
//! search's `last_run_at`, then advances that cursor. So an owner who saved
//! `from:@ops deploy failed` with monitoring on is pinged when a new matching
//! message arrives — without polling the search themselves.
//!
//! The per-search work is factored into [`process_monitored_search`] (free
//! function over the three repos) so the new-match → notification logic is
//! db-testable without spinning up the whole server.

use aero_common::NotificationKind;
use aero_storage::{
    AdvancedSearchRepo, MonitoredSearch, NotificationRepo, SavedSearchRepo,
};

/// How many notifications one monitored search may emit per dispatcher tick. A
/// query that suddenly matches a flood of messages (e.g. just enabled on a busy
/// term) shouldn't bury the owner's inbox in one go; the cursor still advances
/// past them, so the excess is simply not notified (the matches remain findable
/// via the search itself).
const MAX_NOTIFICATIONS_PER_RUN: usize = 20;

/// How many top hits to pull from the search before filtering to the new ones.
const SEARCH_LIMIT: i64 = 100;

/// Re-run one monitored saved search and notify its owner of new matches.
///
/// Returns the number of notifications inserted. On a search whose `last_run_at`
/// is `None` (monitoring was just enabled), it stamps the baseline and notifies
/// nothing — so turning monitoring on never backfills the entire history.
/// Best-effort callers ignore the count; it exists for tests/metrics.
///
/// Cursor semantics & a known best-effort edge: the cursor is `created_at`-based.
/// A message whose inserting transaction sets `created_at` before this run but
/// COMMITS after the search's MVCC snapshot (a long write transaction) could be
/// missed when the cursor advances past its timestamp. Chat messages commit
/// promptly, so this is a rare, accepted limitation of a 5-minute-poll digest —
/// the match remains findable via the search itself; only the proactive ping is
/// best-effort. A monotonic-on-commit cursor (message seq) would close it but is
/// out of scope here.
///
/// # Errors
/// Propagates a [`sqlx::Error`] from any of the underlying repo calls. The caller
/// (the dispatcher loop) logs and swallows so one bad search never wedges the run.
pub async fn process_monitored_search(
    search_repo: &AdvancedSearchRepo,
    notifications: &NotificationRepo,
    saved: &SavedSearchRepo,
    item: &MonitoredSearch,
    now: time::OffsetDateTime,
    cap: usize,
) -> Result<usize, sqlx::Error> {
    // First-ever run: set the baseline cursor and notify nothing.
    let Some(since) = item.last_run_at else {
        saved.mark_run(item.id, item.owner, now).await?;
        return Ok(0);
    };

    // Scope the search to messages created after the cursor by injecting `since`
    // as the query's `after_ts` lower bound (taking the LATER of the cursor and
    // any `since:` operator the user already wrote). This is what makes the
    // monitor correct under load: without it the relevance-ordered top-N could be
    // entirely OLD high-score matches, hiding a genuinely new one behind the cap.
    // With it, every hit the search returns is already new, so the cap only ever
    // trims among new matches.
    let mut parsed = aero_storage::parse_search_query(&item.query);
    parsed.after_ts = Some(parsed.after_ts.map_or(since, |user| user.max(since)));
    let hits = search_repo
        .search(item.owner, item.workspace, &parsed, SEARCH_LIMIT)
        .await?;

    // `after_ts` is inclusive (`>= since`); exclude the exact boundary so a
    // message created at the previous run's instant is never re-notified. Sort
    // oldest-first so the inbox reads chronologically and the cap (if hit) keeps
    // the OLDEST unseen — the rest are caught on the next tick (see the cursor
    // logic below), never silently dropped.
    let mut fresh: Vec<_> = hits.into_iter().filter(|h| h.message.created_at > since).collect();
    fresh.sort_by_key(|h| h.message.created_at);

    let capped = fresh.len() > cap;
    let to_notify: Vec<_> = fresh.into_iter().take(cap).collect();
    let mut last_notified_at = None;
    for hit in &to_notify {
        notifications
            .insert(
                item.owner,
                hit.message.room_id,
                hit.message.id,
                NotificationKind::SavedSearch,
                Some(hit.message.sender_id),
            )
            .await?;
        last_notified_at = Some(hit.message.created_at);
    }

    // Advance the cursor. Normally to `now` (all new matches drained). But when we
    // hit the per-run cap, advance ONLY to the last message we actually notified —
    // so the not-yet-notified newer matches are picked up next tick rather than
    // skipped over. (`now` would jump past them; the un-notified flood would be
    // lost.) `last_notified_at` is always Some here when `capped`.
    let cursor = if capped { last_notified_at.unwrap_or(now) } else { now };
    saved.mark_run(item.id, item.owner, cursor).await?;
    Ok(to_notify.len())
}

/// Background loop: every `interval_secs`, process every monitored saved search.
/// Best-effort throughout — a per-search error is logged and the loop continues;
/// it honors the shutdown token. Spawned by the server binary.
pub async fn run_saved_search_monitor(
    state: crate::state::AppState,
    interval_secs: u64,
    cancel: tokio_util::sync::CancellationToken,
) {
    let saved = SavedSearchRepo::new(state.pg.clone());
    let search_repo = AdvancedSearchRepo::new(state.pg.clone());
    let notifications = NotificationRepo::new(state.pg.clone());
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await; // skip the immediate first tick
    tracing::info!(interval_secs, "saved-search monitor started");

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("saved-search monitor shutting down");
                return;
            }
            _ = tick.tick() => {}
        }

        let monitored = match saved.list_monitored().await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = ?e, "saved-search monitor: list_monitored failed");
                continue;
            }
        };
        if monitored.is_empty() {
            continue;
        }

        let now = time::OffsetDateTime::now_utc();
        let mut total = 0usize;
        for item in &monitored {
            match process_monitored_search(
                &search_repo,
                &notifications,
                &saved,
                item,
                now,
                MAX_NOTIFICATIONS_PER_RUN,
            )
            .await
            {
                Ok(n) => total += n,
                Err(e) => {
                    tracing::warn!(error = ?e, saved_search = %item.id, "saved-search monitor: run failed");
                }
            }
        }
        if total > 0 {
            tracing::info!(notified = total, searches = monitored.len(), "saved-search monitor: new matches notified");
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
mod db_tests {
    use super::process_monitored_search;
    use aero_common::{Block, ParticipantId, RoomId, WorkspaceId};
    use aero_storage::{
        AdvancedSearchRepo, MessageRepo, MonitoredSearch, NewMessage, NotificationRepo, PgPool,
        SavedSearchRepo,
    };

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
            .bind(format!("ssm-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn room(p: &PgPool, creator: ParticipantId, ws: WorkspaceId) -> RoomId {
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
        sqlx::query("INSERT INTO room_members (room_id, participant_id, role) VALUES ($1,$2,'member')")
            .bind(id.to_uuid())
            .bind(creator.to_uuid())
            .execute(p)
            .await
            .expect("join");
        id
    }

    /// A monitored search with a prior `last_run_at` notifies the owner of matches
    /// created AFTER that cursor (and nothing for the first-ever run, which only
    /// sets the baseline). Verifies the new-match → SavedSearch-notification path.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn notifies_owner_of_new_matches_since_last_run() {
        let p = pool();
        let ws = WorkspaceId(ulid::Ulid(0)); // the default workspace
        let search_repo = AdvancedSearchRepo::new(p.clone());
        let notifications = NotificationRepo::new(p.clone());
        let saved = SavedSearchRepo::new(p.clone());
        let msgs = MessageRepo::new(p.clone());

        let owner = participant(&p).await;
        let r = room(&p, owner, ws).await;
        let needle = format!("zqssmtoken{}", ParticipantId::new());

        // A matching message exists already (BEFORE the cursor → must NOT notify).
        let old = msgs
            .insert(NewMessage {
                room_id: r,
                sender_id: owner,
                blocks: vec![Block::text(format!("{needle} old one"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert old");

        let sid = saved.create(owner, ws, "mon", &needle).await.expect("create");

        // First-ever run: baseline only, zero notifications even though `old` matches.
        let item0 = MonitoredSearch {
            id: sid,
            owner,
            workspace: ws,
            query: needle.clone(),
            last_run_at: None,
        };
        let now0 = time::OffsetDateTime::now_utc();
        let n0 = process_monitored_search(&search_repo, &notifications, &saved, &item0, now0, 20)
            .await
            .expect("baseline run");
        assert_eq!(n0, 0, "first-ever run only sets the baseline, notifies nothing");

        // A NEW matching message arrives after the baseline.
        let fresh = msgs
            .insert(NewMessage {
                room_id: r,
                sender_id: owner,
                blocks: vec![Block::text(format!("{needle} brand new"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert fresh");

        // Second run with the baseline as the cursor: notifies for the new match only.
        let item1 = MonitoredSearch {
            id: sid,
            owner,
            workspace: ws,
            query: needle.clone(),
            last_run_at: Some(now0),
        };
        let now1 = now0 + time::Duration::seconds(60);
        let n1 = process_monitored_search(&search_repo, &notifications, &saved, &item1, now1, 20)
            .await
            .expect("delta run");
        assert_eq!(n1, 1, "exactly the one new-since-baseline match notifies");

        // The notification is a SavedSearch kind pointing at the fresh message.
        let inbox = notifications.list(owner, None, false, Some(50)).await.expect("inbox");
        assert!(
            inbox.iter().any(|n| n.message_id == fresh.id
                && matches!(n.kind, aero_common::NotificationKind::SavedSearch)),
            "a SavedSearch notification for the fresh match exists",
        );
        assert!(
            !inbox.iter().any(|n| n.message_id == old.id),
            "the pre-baseline match was never notified",
        );

        // Cleanup.
        sqlx::query("DELETE FROM notifications WHERE participant_id = $1").bind(owner.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM saved_searches WHERE participant_id = $1").bind(owner.to_uuid()).execute(&p).await.ok();
        for m in [old.id, fresh.id] {
            sqlx::query("DELETE FROM messages WHERE id = $1").bind(m.to_uuid()).execute(&p).await.ok();
        }
        sqlx::query("DELETE FROM room_members WHERE room_id = $1").bind(r.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1").bind(r.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM participants WHERE id = $1").bind(owner.to_uuid()).execute(&p).await.ok();
    }

    /// When more new matches exist than the per-run cap, the run notifies `cap` of
    /// them (oldest-first) and advances the cursor ONLY past those — so the rest
    /// are picked up on the NEXT run rather than silently skipped. Guards the
    /// no-data-loss property of the capped path.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn cap_does_not_silently_drop_matches_drains_over_runs() {
        let p = pool();
        let ws = WorkspaceId(ulid::Ulid(0));
        let search_repo = AdvancedSearchRepo::new(p.clone());
        let notifications = NotificationRepo::new(p.clone());
        let saved = SavedSearchRepo::new(p.clone());
        let msgs = MessageRepo::new(p.clone());

        let owner = participant(&p).await;
        let r = room(&p, owner, ws).await;
        let needle = format!("zqcaptoken{}", ParticipantId::new());

        // Baseline cursor: everything below is "new" relative to it.
        let base = time::OffsetDateTime::now_utc();

        // Insert 3 matching messages with strictly increasing created_at (explicit,
        // so the oldest-first ordering and cursor math are deterministic).
        let mut ids = Vec::new();
        for i in 0..3 {
            let ts = base + time::Duration::seconds(i64::from(i) + 1);
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

        let sid = saved.create(owner, ws, "cap", &needle).await.expect("create");
        // Enable monitoring so the re-read via list_monitored() below finds it.
        saved.set_notify_new(sid, owner, true).await.expect("enable monitoring");
        let item = MonitoredSearch { id: sid, owner, workspace: ws, query: needle.clone(), last_run_at: Some(base) };

        // Run 1 with cap=2: notifies the 2 OLDEST, cursor advances only to the 2nd.
        let r1 = process_monitored_search(&search_repo, &notifications, &saved, &item, base + time::Duration::seconds(100), 2)
            .await
            .expect("run1");
        assert_eq!(r1, 2, "first capped run notifies exactly the cap");

        // Re-read the cursor and run again: the 3rd (un-notified) match is caught.
        let monitored = saved.list_monitored().await.expect("list");
        let item2 = monitored.into_iter().find(|m| m.id == sid).expect("still monitored");
        let r2 = process_monitored_search(&search_repo, &notifications, &saved, &item2, base + time::Duration::seconds(200), 2)
            .await
            .expect("run2");
        assert_eq!(r2, 1, "the next run drains the remaining match — none dropped");

        // All 3 matches ended up notified across the two runs (no silent loss).
        let inbox = notifications.list(owner, None, false, Some(50)).await.expect("inbox");
        for id in &ids {
            assert!(
                inbox.iter().any(|n| n.message_id == *id
                    && matches!(n.kind, aero_common::NotificationKind::SavedSearch)),
                "match {id} was eventually notified",
            );
        }

        // Cleanup.
        sqlx::query("DELETE FROM notifications WHERE participant_id = $1").bind(owner.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM saved_searches WHERE participant_id = $1").bind(owner.to_uuid()).execute(&p).await.ok();
        for id in &ids {
            sqlx::query("DELETE FROM messages WHERE id = $1").bind(id.to_uuid()).execute(&p).await.ok();
        }
        sqlx::query("DELETE FROM room_members WHERE room_id = $1").bind(r.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1").bind(r.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM participants WHERE id = $1").bind(owner.to_uuid()).execute(&p).await.ok();
    }
}
