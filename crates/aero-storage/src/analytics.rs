//! Workspace analytics — read-only aggregate stats over existing tables.
//!
//! Admin-facing dashboard numbers for one tenant: how many messages/rooms/members
//! it has, how active it has been this week, which channels are busiest, and a
//! per-day message timeline. There is **no** new table and **no** new id — every
//! method is a parameterized aggregate SELECT scoped to a single workspace via
//! `rooms.workspace_id` (the tenant boundary introduced by
//! `migrations/0006_workspaces.sql`).
//!
//! Message aggregates filter `deleted_at IS NULL`, matching the dominant
//! convention in [`MessageRepo`](crate::MessageRepo) — analytics reflect live
//! content, not soft-deleted tombstones. Purely additive: a NEW
//! [`AnalyticsRepo`]; no existing repo is touched. The model structs
//! ([`WorkspaceAnalytics`], [`ChannelMessageCount`], [`DayCount`]) live here (and
//! are re-exported from the crate root) since they are storage-layer projections,
//! not domain types.

use aero_common::{RoomId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// Headline aggregate counts for one workspace — the analytics "overview" card.
///
/// A storage-layer projection (not a domain type); `Serialize` so a handler can
/// hand it straight back as JSON.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceAnalytics {
    /// All non-deleted messages in the workspace's rooms.
    pub total_messages: i64,
    /// Non-deleted messages created in the trailing 7 days.
    pub messages_last_7d: i64,
    /// Rooms (channels/DMs) that belong to the workspace.
    pub total_rooms: i64,
    /// Distinct members enrolled in the workspace.
    pub total_members: i64,
    /// Distinct message authors active in the workspace in the trailing 7 days.
    pub active_members_7d: i64,
}

/// One row of the "busiest channels" ranking: a room and how many (non-deleted)
/// messages it holds.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelMessageCount {
    /// The room the count is for.
    pub room_id: RoomId,
    /// Number of non-deleted messages in that room.
    pub message_count: i64,
}

/// One bucket of the per-day message timeline: a calendar day (`YYYY-MM-DD`, UTC)
/// and how many (non-deleted) messages were created that day.
#[derive(Debug, Clone, Serialize)]
pub struct DayCount {
    /// The day, truncated via `date_trunc('day', …)` and rendered `YYYY-MM-DD`.
    pub day: String,
    /// Number of non-deleted messages created that day.
    pub count: i64,
}

/// Per-channel aggregate stats for a single room: total and 30-day message counts
/// plus distinct sender counts. A storage-layer projection (`Serialize` so a handler
/// can return it directly as JSON).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ChannelAnalytics {
    pub room_id: Uuid,
    pub total_messages: i64,
    pub unique_senders: i64,
    pub messages_last_30_days: i64,
    pub active_members_last_30_days: i64,
}

/// One row of the per-workspace (or per-room) reaction-frequency ranking:
/// an emoji and how many times it has been used.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ReactionStat {
    pub emoji: String,
    pub count: i64,
}

/// 30-day workspace activity summary — a member-accessible projection (not
/// admin-only like [`WorkspaceAnalytics`]). Returned by
/// `GET /api/workspaces/:id/analytics/summary`.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceSummary {
    /// All non-deleted messages across the workspace's rooms (all time).
    pub total_messages: i64,
    /// Non-deleted messages created in the trailing 30 days.
    pub messages_30d: i64,
    /// Distinct message authors active in the workspace in the trailing 30 days.
    pub active_users_30d: i64,
    /// Distinct members enrolled in the workspace.
    pub total_members: i64,
}

/// Largest number of channels [`AnalyticsRepo::top_channels`] returns. A caller's
/// requested `limit` is clamped into `[1, MAX_TOP_CHANNELS]` so an outsized (or
/// non-positive) request can neither be empty nor unbounded.
pub const MAX_TOP_CHANNELS: i64 = 100;

/// Largest timeline window [`AnalyticsRepo::messages_per_day`] looks back over, in
/// days. A caller's requested `days` is clamped into `[1, MAX_TIMELINE_DAYS]`.
pub const MAX_TIMELINE_DAYS: i64 = 365;

/// Clamp a caller-supplied channel limit into `[1, MAX_TOP_CHANNELS]`. Pure, so
/// the bound is unit-tested without a database.
#[inline]
#[must_use]
pub fn clamp_limit(limit: i64) -> i64 {
    limit.clamp(1, MAX_TOP_CHANNELS)
}

/// Clamp a caller-supplied timeline window into `[1, MAX_TIMELINE_DAYS]` days.
/// Pure, so the bound is unit-tested without a database.
#[inline]
#[must_use]
pub fn clamp_days(days: i64) -> i64 {
    days.clamp(1, MAX_TIMELINE_DAYS)
}

/// Repository of read-only aggregate stats for a workspace.
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`AnalyticsRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct AnalyticsRepo {
    pool: PgPool,
}

impl AnalyticsRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Headline counts for `workspace` (see [`WorkspaceAnalytics`]). One round-trip:
    /// five correlated scalar subqueries, each scoped to the workspace's rooms via
    /// `rooms.workspace_id`. Message counts exclude soft-deleted rows.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn overview(
        &self,
        workspace: WorkspaceId,
    ) -> Result<WorkspaceAnalytics, sqlx::Error> {
        let row = sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
            r"SELECT
                (SELECT COUNT(*) FROM messages m
                   JOIN rooms r ON r.id = m.room_id
                  WHERE r.workspace_id = $1 AND m.deleted_at IS NULL) AS total_messages,
                (SELECT COUNT(*) FROM messages m
                   JOIN rooms r ON r.id = m.room_id
                  WHERE r.workspace_id = $1 AND m.deleted_at IS NULL
                    AND m.created_at >= now() - make_interval(days => 7)) AS messages_last_7d,
                (SELECT COUNT(*) FROM rooms WHERE workspace_id = $1) AS total_rooms,
                (SELECT COUNT(DISTINCT participant_id) FROM workspace_members
                  WHERE workspace_id = $1) AS total_members,
                (SELECT COUNT(DISTINCT m.sender_id) FROM messages m
                   JOIN rooms r ON r.id = m.room_id
                  WHERE r.workspace_id = $1 AND m.deleted_at IS NULL
                    AND m.created_at >= now() - make_interval(days => 7)) AS active_members_7d",
        )
        .bind(workspace.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        let (total_messages, messages_last_7d, total_rooms, total_members, active_members_7d) = row;
        Ok(WorkspaceAnalytics {
            total_messages,
            messages_last_7d,
            total_rooms,
            total_members,
            active_members_7d,
        })
    }

    /// The busiest channels in `workspace`: rooms ranked by non-deleted message
    /// count, descending, capped at `limit` (clamped via [`clamp_limit`]).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn top_channels(
        &self,
        workspace: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<ChannelMessageCount>, sqlx::Error> {
        let limit = clamp_limit(limit);
        let rows = sqlx::query_as::<_, (uuid::Uuid, i64)>(
            r"SELECT m.room_id, COUNT(*) AS message_count
               FROM messages m
               JOIN rooms r ON r.id = m.room_id
              WHERE r.workspace_id = $1 AND m.deleted_at IS NULL
              GROUP BY m.room_id
              ORDER BY COUNT(*) DESC, m.room_id DESC
              LIMIT $2",
        )
        .bind(workspace.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(room_id, message_count)| ChannelMessageCount {
                room_id: RoomId::from_uuid(room_id),
                message_count,
            })
            .collect())
    }

    /// Per-day message volume in `workspace` over the trailing `days`
    /// (clamped via [`clamp_days`]), oldest day first. Days with no messages do
    /// not appear (the timeline is sparse, not gap-filled). Buckets are UTC
    /// calendar days via `date_trunc('day', created_at)`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn messages_per_day(
        &self,
        workspace: WorkspaceId,
        days: i64,
    ) -> Result<Vec<DayCount>, sqlx::Error> {
        let days = clamp_days(days);
        // `make_interval` takes int4; the window is already clamped to <= 365.
        let days_i32 = i32::try_from(days).unwrap_or(i32::MAX);
        let rows = sqlx::query_as::<_, (String, i64)>(
            r"SELECT date_trunc('day', m.created_at)::date::text AS day,
                     COUNT(*) AS count
               FROM messages m
               JOIN rooms r ON r.id = m.room_id
              WHERE r.workspace_id = $1 AND m.deleted_at IS NULL
                AND m.created_at >= now() - make_interval(days => $2)
              GROUP BY date_trunc('day', m.created_at)
              ORDER BY date_trunc('day', m.created_at) ASC",
        )
        .bind(workspace.to_uuid())
        .bind(days_i32)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(day, count)| DayCount { day, count })
            .collect())
    }

    /// Aggregate stats for a single room (channel) within `workspace`.
    ///
    /// Returns total message count, unique senders, and the same two figures
    /// restricted to the trailing 30 days. All counts exclude soft-deleted rows.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the queries.
    pub async fn channel_analytics(
        &self,
        workspace: WorkspaceId,
        room: RoomId,
    ) -> Result<ChannelAnalytics, sqlx::Error> {
        let row = sqlx::query_as::<_, (i64, i64, i64, i64)>(
            r"SELECT
                COUNT(*) FILTER (WHERE deleted_at IS NULL) AS total_messages,
                COUNT(DISTINCT sender_id) FILTER (WHERE deleted_at IS NULL) AS unique_senders,
                COUNT(*) FILTER (WHERE deleted_at IS NULL
                                   AND created_at > now() - interval '30 days') AS messages_last_30_days,
                COUNT(DISTINCT sender_id) FILTER (WHERE deleted_at IS NULL
                                                   AND created_at > now() - interval '30 days') AS active_members_last_30_days
              FROM messages m
              JOIN rooms r ON r.id = m.room_id
             WHERE m.room_id = $1
               AND r.workspace_id = $2",
        )
        .bind(room.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        let (total_messages, unique_senders, messages_last_30_days, active_members_last_30_days) =
            row;
        Ok(ChannelAnalytics {
            room_id: room.to_uuid(),
            total_messages,
            unique_senders,
            messages_last_30_days,
            active_members_last_30_days,
        })
    }

    /// The most-used reactions across all messages in `workspace`, descending by
    /// frequency, limited to `limit` rows (clamped to `[1, 100]`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn top_reactions_workspace(
        &self,
        workspace: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<ReactionStat>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        sqlx::query_as::<_, ReactionStat>(
            r"SELECT rct.emoji, COUNT(*) AS count
               FROM reactions rct
               JOIN messages m ON rct.message_id = m.id
               JOIN rooms r ON r.id = m.room_id
              WHERE r.workspace_id = $1
                AND m.deleted_at IS NULL
              GROUP BY rct.emoji
              ORDER BY count DESC, rct.emoji ASC
              LIMIT $2",
        )
        .bind(workspace.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// Aggregate summary for `workspace` over a 30-day window, suitable for
    /// member-facing dashboard widgets. Returns total message count (all time),
    /// messages in the last 30 days, distinct active message authors in the last
    /// 30 days, and total distinct workspace members. One round-trip via four
    /// correlated scalar subqueries, matching the style of [`Self::overview`].
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn workspace_summary(
        &self,
        workspace: WorkspaceId,
    ) -> Result<WorkspaceSummary, sqlx::Error> {
        let row = sqlx::query_as::<_, (i64, i64, i64, i64)>(
            r"SELECT
                (SELECT COUNT(*) FROM messages m
                   JOIN rooms r ON r.id = m.room_id
                  WHERE r.workspace_id = $1 AND m.deleted_at IS NULL) AS total_messages,
                (SELECT COUNT(*) FROM messages m
                   JOIN rooms r ON r.id = m.room_id
                  WHERE r.workspace_id = $1 AND m.deleted_at IS NULL
                    AND m.created_at >= now() - INTERVAL '30 days') AS messages_30d,
                (SELECT COUNT(DISTINCT m.sender_id) FROM messages m
                   JOIN rooms r ON r.id = m.room_id
                  WHERE r.workspace_id = $1 AND m.deleted_at IS NULL
                    AND m.created_at >= now() - INTERVAL '30 days') AS active_users_30d,
                (SELECT COUNT(DISTINCT participant_id) FROM workspace_members
                  WHERE workspace_id = $1) AS total_members",
        )
        .bind(workspace.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        let (total_messages, messages_30d, active_users_30d, total_members) = row;
        Ok(WorkspaceSummary {
            total_messages,
            messages_30d,
            active_users_30d,
            total_members,
        })
    }

    /// The most-used reactions on messages in `room`, descending by frequency,
    /// limited to `limit` rows (clamped to `[1, 100]`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn top_reactions_room(
        &self,
        room: RoomId,
        limit: i64,
    ) -> Result<Vec<ReactionStat>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        sqlx::query_as::<_, ReactionStat>(
            r"SELECT rct.emoji, COUNT(*) AS count
               FROM reactions rct
               JOIN messages m ON rct.message_id = m.id
              WHERE m.room_id = $1
                AND m.deleted_at IS NULL
              GROUP BY rct.emoji
              ORDER BY count DESC, rct.emoji ASC
              LIMIT $2",
        )
        .bind(room.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::{clamp_days, clamp_limit, MAX_TIMELINE_DAYS, MAX_TOP_CHANNELS};

    #[test]
    fn limit_is_clamped_into_window() {
        assert_eq!(clamp_limit(0), 1);
        assert_eq!(clamp_limit(-5), 1);
        assert_eq!(clamp_limit(1), 1);
        assert_eq!(clamp_limit(10), 10);
        assert_eq!(clamp_limit(MAX_TOP_CHANNELS), MAX_TOP_CHANNELS);
        assert_eq!(clamp_limit(MAX_TOP_CHANNELS + 1), MAX_TOP_CHANNELS);
        assert_eq!(clamp_limit(i64::MAX), MAX_TOP_CHANNELS);
    }

    #[test]
    fn days_is_clamped_into_window() {
        assert_eq!(clamp_days(0), 1);
        assert_eq!(clamp_days(-1), 1);
        assert_eq!(clamp_days(1), 1);
        assert_eq!(clamp_days(14), 14);
        assert_eq!(clamp_days(MAX_TIMELINE_DAYS), MAX_TIMELINE_DAYS);
        assert_eq!(clamp_days(MAX_TIMELINE_DAYS + 1), MAX_TIMELINE_DAYS);
        assert_eq!(clamp_days(i64::MAX), MAX_TIMELINE_DAYS);
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored analytics
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{ParticipantId, RoomKind, WorkspaceRole};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Insert a throwaway participant so message/membership FKs are satisfiable.
    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("analytics-actor-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    /// A fresh workspace whose only member is `owner` — so the per-tenant counts
    /// are deterministic (no other workspace's data leaks in).
    async fn workspace(p: &PgPool, owner: ParticipantId) -> WorkspaceId {
        let ws = WorkspaceId::new();
        let mut tx = p.begin().await.expect("begin workspace fixture");
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_by, created_at) VALUES ($1,$2,$3,$4, now())")
            .bind(ws.to_uuid())
            .bind("Analytics Test WS")
            .bind(format!("analytics-{ws}"))
            .bind(owner.to_uuid())
            .execute(&mut *tx)
            .await
            .expect("insert workspace");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role, joined_at) VALUES ($1,$2,$3, now())",
        )
        .bind(ws.to_uuid())
        .bind(owner.to_uuid())
        .bind(WorkspaceRole::Owner.as_str())
        .execute(&mut *tx)
        .await
        .expect("insert workspace member");
        tx.commit().await.expect("commit workspace fixture");
        ws
    }

    /// Insert a room directly in `workspace`, created by `creator`.
    async fn room_in(p: &PgPool, ws: WorkspaceId, creator: ParticipantId) -> RoomId {
        crate::RoomRepo::new(p.clone())
            .create_in_workspace(
                ws,
                RoomKind::Channel,
                Some(format!("analytics-room-{}", RoomId::new())),
                creator,
            )
            .await
            .expect("insert room")
            .id
    }

    /// Insert one (now-dated) message authored by `sender` in `room`.
    async fn insert_message(p: &PgPool, room: RoomId, sender: ParticipantId) {
        let id = aero_common::MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text, created_at) VALUES ($1,$2,$3,$4,$5, now())",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .bind(serde_json::json!([{ "type": "text", "text": "analytics body" }]))
        .bind("analytics body")
        .execute(p)
        .await
        .expect("insert message");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn analytics_counts_move_with_inserted_data() {
        let p = pool();
        let repo = AnalyticsRepo::new(p.clone());
        let owner = participant(&p).await;
        let ws = workspace(&p, owner).await;

        // A fresh workspace starts empty.
        let before = repo.overview(ws).await.unwrap();
        assert_eq!(before.total_messages, 0, "fresh workspace has no messages");
        assert_eq!(before.total_rooms, 0, "fresh workspace has no rooms");
        assert_eq!(before.total_members, 1, "owner is the sole member");
        assert_eq!(before.active_members_7d, 0, "no activity yet");

        // One room, three messages from the owner.
        let room = room_in(&p, ws, owner).await;
        for _ in 0..3 {
            insert_message(&p, room, owner).await;
        }

        let after = repo.overview(ws).await.unwrap();
        assert_eq!(after.total_messages, 3, "three messages counted");
        assert_eq!(after.messages_last_7d, 3, "all three are within the week");
        assert_eq!(after.total_rooms, 1, "one room counted");
        assert_eq!(after.total_members, 1, "still one member");
        assert_eq!(
            after.active_members_7d, 1,
            "owner is the lone active author"
        );

        // Busiest channels: the seeded room with its three messages.
        let top = repo.top_channels(ws, 10).await.unwrap();
        let hit = top.iter().find(|c| c.room_id == room).expect("room ranked");
        assert_eq!(hit.message_count, 3, "ranking carries the right count");

        // Timeline: one bucket (today) with three messages.
        let timeline = repo.messages_per_day(ws, 14).await.unwrap();
        let total: i64 = timeline.iter().map(|d| d.count).sum();
        assert_eq!(total, 3, "timeline buckets sum to the message total");
        assert!(
            timeline
                .iter()
                .all(|d| d.day.len() == 10 && d.day.contains('-')),
            "each day renders as YYYY-MM-DD"
        );

        // Cleanup so reruns stay self-contained (rooms→workspaces FK has no
        // cascade, so delete the room — and its messages — before the workspace).
        sqlx::query("DELETE FROM rooms WHERE workspace_id = $1")
            .bind(ws.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(ws.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
