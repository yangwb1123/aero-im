//! Notification-preferences repository: per-channel mute + per-user Do-Not-Disturb.
//!
//! Backs `migrations/0018_notification_prefs.sql`. Two independent controls a
//! participant uses to quiet their notifications:
//!
//! * **Channel mute** — a row in `channel_mutes` means "do not notify me about
//!   this room". Muting is an idempotent upsert; unmuting deletes the row.
//! * **Do-Not-Disturb (DND)** — a single daily window per participant in
//!   `dnd_settings`, stored as minutes-of-day (`0..1440`). While the current
//!   time falls inside the window, no notifications are delivered. `None/None`
//!   means no DND configured. A window with `start > end` wraps past midnight
//!   (an *overnight* window).
//!
//! The suppression *decision* is pure ([`in_dnd_window`] / [`should_suppress`])
//! so it is exhaustively unit-testable without a database; the server consults
//! these before persisting/pushing a notification. Purely additive: a NEW
//! [`NotificationPrefsRepo`]; no existing repo is touched.

use aero_common::{ParticipantId, RoomId};
use sqlx::PgPool;

/// Number of minutes in a day — the modulus for a minutes-of-day clock. Public
/// so callers validating a DND window can bound minutes to `0..MINUTES_PER_DAY`.
pub const MINUTES_PER_DAY: i32 = 1440;

/// Whether `now_minute` (minutes-of-day) falls inside the DND window
/// `[start, end)`, handling both same-day and overnight (wrap-around) windows.
///
/// * Same-day (`start <= end`): inside when `start <= now < end`. An empty
///   window (`start == end`) is never "inside" — DND is effectively off.
/// * Overnight (`start > end`): the window wraps past midnight, so it is the
///   UNION `[start, 1440) ∪ [0, end)` — inside when `now >= start` OR `now < end`.
///
/// Pure so the boundary/wrap behaviour is unit-tested offline.
#[must_use]
pub fn in_dnd_window(now_minute: i32, start: i32, end: i32) -> bool {
    if start <= end {
        // Same-day window (empty when start == end => always false).
        now_minute >= start && now_minute < end
    } else {
        // Overnight window: from `start` to midnight, then midnight to `end`.
        now_minute >= start || now_minute < end
    }
}

/// Whether a notification to a recipient should be suppressed: muted on the room
/// OR currently inside the recipient's DND window. `dnd` is `Some((start, end))`
/// only when the recipient has a window configured. Pure, unit-tested.
#[must_use]
pub fn should_suppress(is_muted: bool, dnd: Option<(i32, i32)>, now_minute: i32) -> bool {
    if is_muted {
        return true;
    }
    matches!(dnd, Some((start, end)) if in_dnd_window(now_minute, start, end))
}

/// Minutes-of-day (`0..1440`) for an instant, computed from its UTC clock.
/// Pure helper so callers can derive `now_minute` consistently.
#[must_use]
pub fn minute_of_day_utc(at: time::OffsetDateTime) -> i32 {
    i32::from(at.hour()) * 60 + i32::from(at.minute())
}

#[derive(Clone)]
pub struct NotificationPrefsRepo {
    pool: PgPool,
}

impl NotificationPrefsRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Mute `room` for `participant`. Idempotent: re-muting keeps the original
    /// `created_at`.
    pub async fn mute(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO channel_mutes (participant_id, room_id, created_at)
               VALUES ($1, $2, now())
               ON CONFLICT (participant_id, room_id) DO NOTHING",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Unmute `room` for `participant`. Returns `true` if a mute was removed
    /// (idempotent: unmuting a non-muted room is a no-op `false`).
    pub async fn unmute(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query(r"DELETE FROM channel_mutes WHERE participant_id = $1 AND room_id = $2")
                .bind(participant.to_uuid())
                .bind(room.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Whether `participant` has muted `room`.
    pub async fn is_muted(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*) FROM channel_mutes WHERE participant_id = $1 AND room_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 > 0)
    }

    /// All rooms `participant` has muted, newest mute first.
    pub async fn muted_rooms(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<RoomId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT room_id FROM channel_mutes
               WHERE participant_id = $1
               ORDER BY created_at DESC",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(r,)| RoomId::from_uuid(r)).collect())
    }

    /// Set (or clear) `participant`'s DND window. Pass `Some` minutes-of-day for
    /// both bounds to arm it, or `None`/`None` to clear it. An idempotent upsert
    /// keyed on the participant.
    pub async fn set_dnd(
        &self,
        participant: ParticipantId,
        start_minute: Option<i32>,
        end_minute: Option<i32>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO dnd_settings (participant_id, start_minute, end_minute, updated_at)
               VALUES ($1, $2, $3, now())
               ON CONFLICT (participant_id)
               DO UPDATE SET start_minute = EXCLUDED.start_minute,
                             end_minute   = EXCLUDED.end_minute,
                             updated_at   = now()",
        )
        .bind(participant.to_uuid())
        .bind(start_minute)
        .bind(end_minute)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `participant`'s configured DND window, or `None` when unset (no row, or a
    /// row with NULL bounds). A partially-set window (one bound NULL) is treated
    /// as unset.
    pub async fn get_dnd(
        &self,
        participant: ParticipantId,
    ) -> Result<Option<(i32, i32)>, sqlx::Error> {
        let row = sqlx::query_as::<_, (Option<i32>, Option<i32>)>(
            r"SELECT start_minute, end_minute FROM dnd_settings WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((Some(start), Some(end))) => Some((start, end)),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dnd_same_day_window_inside_outside_and_boundary() {
        // 09:00 (540) .. 17:00 (1020): a normal working-hours quiet window.
        let (start, end) = (540, 1020);
        // Inside.
        assert!(in_dnd_window(540, start, end), "start is inclusive");
        assert!(in_dnd_window(700, start, end), "midday is inside");
        assert!(in_dnd_window(1019, start, end), "just before end is inside");
        // Outside.
        assert!(!in_dnd_window(539, start, end), "one minute before start");
        assert!(!in_dnd_window(1020, start, end), "end is exclusive");
        assert!(!in_dnd_window(0, start, end), "midnight is outside");
        assert!(!in_dnd_window(1439, start, end), "last minute of day outside");
    }

    #[test]
    fn dnd_overnight_window_crosses_midnight() {
        // 22:00 (1320) .. 08:00 (480): wraps past midnight.
        let (start, end) = (1320, 480);
        // Inside: late evening, across midnight, and early morning.
        assert!(in_dnd_window(1320, start, end), "start is inclusive");
        assert!(in_dnd_window(1380, start, end), "23:00 inside");
        assert!(in_dnd_window(0, start, end), "midnight inside");
        assert!(in_dnd_window(479, start, end), "07:59 inside");
        // Outside: daytime gap between end and start.
        assert!(!in_dnd_window(480, start, end), "08:00 end is exclusive");
        assert!(!in_dnd_window(720, start, end), "noon is outside");
        assert!(!in_dnd_window(1319, start, end), "one minute before start outside");
    }

    #[test]
    fn dnd_empty_window_is_never_inside() {
        // start == end is a zero-length window: DND effectively off.
        for now in [0, 540, 720, 1439] {
            assert!(!in_dnd_window(now, 600, 600), "empty window never matches at {now}");
        }
    }

    #[test]
    fn dnd_full_day_window_is_always_inside() {
        // [0, 1440): every minute of the day is covered (same-day branch).
        for now in [0, 1, 540, 1439] {
            assert!(in_dnd_window(now, 0, MINUTES_PER_DAY), "full-day window covers {now}");
        }
    }

    #[test]
    fn should_suppress_truth_table() {
        // Muted always suppresses, regardless of DND / time.
        assert!(should_suppress(true, None, 0));
        assert!(should_suppress(true, Some((540, 1020)), 700));
        assert!(should_suppress(true, Some((540, 1020)), 100));

        // Not muted, no DND: never suppressed.
        assert!(!should_suppress(false, None, 0));
        assert!(!should_suppress(false, None, 720));

        // Not muted, DND configured: suppressed iff inside the window.
        assert!(should_suppress(false, Some((540, 1020)), 700), "inside same-day window");
        assert!(!should_suppress(false, Some((540, 1020)), 100), "outside same-day window");
        assert!(should_suppress(false, Some((1320, 480)), 0), "inside overnight window");
        assert!(!should_suppress(false, Some((1320, 480)), 720), "outside overnight window");
    }

    #[test]
    fn minute_of_day_from_utc_clock() {
        let midnight = time::OffsetDateTime::UNIX_EPOCH; // 1970-01-01 00:00:00 UTC
        assert_eq!(minute_of_day_utc(midnight), 0);
        // 1970-01-01 00:00:00 + 9h30m = 09:30 => 570.
        let nine_thirty = midnight + time::Duration::minutes(570);
        assert_eq!(minute_of_day_utc(nine_thirty), 570);
        // Wrap into the next day stays minutes-of-day (24h later == 00:00).
        let next_midnight = midnight + time::Duration::hours(24);
        assert_eq!(minute_of_day_utc(next_midnight), 0);
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored notif_prefs_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{ParticipantId, RoomId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a participant + a room (in the default workspace) so the FKs are
    /// satisfied. Returns (participant, room).
    async fn fixture(p: &PgPool) -> (ParticipantId, RoomId) {
        let participant = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(participant.to_uuid())
            .bind(format!("prefs-participant-{participant}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1,'channel',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("prefs-room")
        .bind(participant.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        (participant, room)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn notif_prefs_mute_is_muted_muted_rooms_unmute_roundtrip() {
        let p = pool();
        let repo = NotificationPrefsRepo::new(p.clone());
        let (participant, room) = fixture(&p).await;

        assert!(!repo.is_muted(participant, room).await.unwrap(), "not muted initially");

        repo.mute(participant, room).await.unwrap();
        repo.mute(participant, room).await.unwrap(); // idempotent
        assert!(repo.is_muted(participant, room).await.unwrap(), "muted after mute");

        let muted = repo.muted_rooms(participant).await.unwrap();
        assert!(muted.contains(&room), "muted_rooms lists the room");

        assert!(repo.unmute(participant, room).await.unwrap(), "unmute removed it");
        assert!(!repo.unmute(participant, room).await.unwrap(), "second unmute is a no-op");
        assert!(!repo.is_muted(participant, room).await.unwrap(), "not muted after unmute");
        assert!(!repo.muted_rooms(participant).await.unwrap().contains(&room));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn notif_prefs_set_get_and_clear_dnd() {
        let p = pool();
        let repo = NotificationPrefsRepo::new(p.clone());
        let (participant, _room) = fixture(&p).await;

        assert!(repo.get_dnd(participant).await.unwrap().is_none(), "no DND initially");

        repo.set_dnd(participant, Some(1320), Some(480)).await.unwrap();
        assert_eq!(repo.get_dnd(participant).await.unwrap(), Some((1320, 480)), "overnight window");

        // Overwrite with a same-day window (upsert path).
        repo.set_dnd(participant, Some(540), Some(1020)).await.unwrap();
        assert_eq!(repo.get_dnd(participant).await.unwrap(), Some((540, 1020)));

        // Clear it.
        repo.set_dnd(participant, None, None).await.unwrap();
        assert!(repo.get_dnd(participant).await.unwrap().is_none(), "DND cleared");
    }
}
