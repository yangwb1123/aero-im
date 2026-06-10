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
//! * **One-off snooze** — a single nullable `snooze_until` timestamp on the same
//!   `dnd_settings` row (migration 0060). While `now < snooze_until` ALL
//!   notifications are suppressed once, then it lapses on its own; `NULL` means
//!   not snoozed. This is the Slack "Pause notifications" / Teams quiet-time
//!   control, distinct from the recurring daily DND window above.
//!
//! The suppression *decision* is pure ([`in_dnd_window`] / [`should_suppress`] /
//! [`is_snoozed`]) so it is exhaustively unit-testable without a database; the
//! server consults these before persisting/pushing a notification. Purely
//! additive: a NEW [`NotificationPrefsRepo`]; no existing repo is touched.

use std::collections::{HashMap, HashSet};

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

/// Whether a one-off snooze is currently active: `snooze_until` is set and `now`
/// has not yet reached it (`now < snooze_until`). An absent (`None`) or already
/// elapsed snooze reads as inactive. Composes with [`should_suppress`] — a
/// notification is silenced when EITHER is true. Pure, unit-tested.
#[must_use]
pub fn is_snoozed(snooze_until: Option<time::OffsetDateTime>, now: time::OffsetDateTime) -> bool {
    matches!(snooze_until, Some(until) if now < until)
}

/// Minutes-of-day (`0..1440`) for an instant, computed from its UTC clock.
/// Pure helper so callers can derive `now_minute` consistently.
#[must_use]
pub fn minute_of_day_utc(at: time::OffsetDateTime) -> i32 {
    i32::from(at.hour()) * 60 + i32::from(at.minute())
}

/// Whether a notification should be DELIVERED to a recipient given their
/// (batch-fetched) preferences: NOT snoozed AND NOT suppressed by mute/DND.
/// The pure composition of [`is_snoozed`] + [`should_suppress`] (over
/// [`minute_of_day_utc`]) — it reuses those helpers rather than restating
/// their rules, so fan-out paths that read prefs in bulk decide identically
/// to the single-recipient path. Fail-open callers that could not load a
/// recipient's prefs pass `false`/`None`/`None`, which always delivers.
#[must_use]
pub fn should_deliver(
    is_muted: bool,
    dnd: Option<(i32, i32)>,
    snooze_until: Option<time::OffsetDateTime>,
    now: time::OffsetDateTime,
) -> bool {
    !is_snoozed(snooze_until, now) && !should_suppress(is_muted, dnd, minute_of_day_utc(now))
}

/// One participant's `dnd_settings` row as returned by
/// [`NotificationPrefsRepo::dnd_snooze_many`], decoded with the SAME semantics
/// as the single-recipient reads: `dnd` is `Some((start, end))` only when both
/// bounds are set (a partially-set window reads as unset, mirroring
/// [`get_dnd`](NotificationPrefsRepo::get_dnd)), and `snooze_until` is the raw
/// nullable instant (callers decide "still active" via [`is_snoozed`], mirroring
/// [`get_snooze`](NotificationPrefsRepo::get_snooze)). `Default` is the
/// no-row/no-prefs state: no DND, no snooze.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DndSnooze {
    /// Daily DND window in minutes-of-day, `None` when unset.
    pub dnd: Option<(i32, i32)>,
    /// One-off snooze instant, `None` when unset.
    pub snooze_until: Option<time::OffsetDateTime>,
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

    /// Which of `participants` have muted `room`, in ONE round-trip
    /// (`participant_id = ANY($2)`), as a set for O(1) membership tests. The
    /// batched counterpart of [`is_muted`](Self::is_muted) for notification
    /// fan-out (ROADMAP 第三版 方向四 — a large-room `@everyone` previously did
    /// one mute lookup per recipient). Empty input short-circuits to an empty
    /// set without touching the database.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn muted_set(
        &self,
        room: RoomId,
        participants: &[ParticipantId],
    ) -> Result<HashSet<ParticipantId>, sqlx::Error> {
        if participants.is_empty() {
            return Ok(HashSet::new());
        }
        let ids: Vec<uuid::Uuid> = participants.iter().map(ParticipantId::to_uuid).collect();
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT participant_id FROM channel_mutes
               WHERE room_id = $1 AND participant_id = ANY($2)",
        )
        .bind(room.to_uuid())
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(p,)| ParticipantId::from_uuid(p)).collect())
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

    /// Arm (or clear) `participant`'s one-off notification snooze. Pass `Some`
    /// future instant to pause ALL notifications until then, or `None` to clear
    /// it. An idempotent upsert keyed on the participant that touches ONLY
    /// `snooze_until`, so it preserves any configured daily DND window on the
    /// same row. See [`clear_snooze`](Self::clear_snooze) for the clearing alias.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn set_snooze(
        &self,
        participant: ParticipantId,
        until: Option<time::OffsetDateTime>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO dnd_settings (participant_id, snooze_until, updated_at)
               VALUES ($1, $2, now())
               ON CONFLICT (participant_id)
               DO UPDATE SET snooze_until = EXCLUDED.snooze_until,
                             updated_at   = now()",
        )
        .bind(participant.to_uuid())
        .bind(until)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Clear `participant`'s one-off snooze (sets `snooze_until` back to `NULL`),
    /// leaving any daily DND window intact. Idempotent — clearing an unset snooze
    /// is a no-op. A thin alias for [`set_snooze`](Self::set_snooze) with `None`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the underlying upsert.
    pub async fn clear_snooze(&self, participant: ParticipantId) -> Result<(), sqlx::Error> {
        self.set_snooze(participant, None).await
    }

    /// `participant`'s one-off snooze instant, or `None` when unset (no row, or a
    /// row with a NULL `snooze_until`). Read alongside the DND row. A value in the
    /// past is returned verbatim; callers decide "is it still active" via
    /// [`is_snoozed`].
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get_snooze(
        &self,
        participant: ParticipantId,
    ) -> Result<Option<time::OffsetDateTime>, sqlx::Error> {
        let row = sqlx::query_as::<_, (Option<time::OffsetDateTime>,)>(
            r"SELECT snooze_until FROM dnd_settings WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(snooze,)| snooze))
    }

    /// DND window + snooze for many participants in ONE round-trip
    /// (`participant_id = ANY($1)`) — the batched counterpart of
    /// [`get_dnd`](Self::get_dnd) + [`get_snooze`](Self::get_snooze) for
    /// notification fan-out (ROADMAP 第三版 方向四). Participants with no
    /// `dnd_settings` row are simply absent from the map (callers treat a miss
    /// as [`DndSnooze::default`]: no DND, no snooze). Empty input
    /// short-circuits to an empty map without touching the database.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn dnd_snooze_many(
        &self,
        participants: &[ParticipantId],
    ) -> Result<HashMap<ParticipantId, DndSnooze>, sqlx::Error> {
        type Row = (uuid::Uuid, Option<i32>, Option<i32>, Option<time::OffsetDateTime>);
        if participants.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<uuid::Uuid> = participants.iter().map(ParticipantId::to_uuid).collect();
        let rows = sqlx::query_as::<_, Row>(
            r"SELECT participant_id, start_minute, end_minute, snooze_until
               FROM dnd_settings WHERE participant_id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(p, start, end, snooze_until)| {
                let dnd = match (start, end) {
                    (Some(s), Some(e)) => Some((s, e)),
                    _ => None,
                };
                (ParticipantId::from_uuid(p), DndSnooze { dnd, snooze_until })
            })
            .collect())
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
    fn snooze_active_only_while_now_is_before_until() {
        let now = time::OffsetDateTime::UNIX_EPOCH;
        // Not snoozed when unset.
        assert!(!is_snoozed(None, now), "no snooze set => inactive");
        // Active while now is strictly before the target instant.
        let future = now + time::Duration::hours(1);
        assert!(is_snoozed(Some(future), now), "future snooze is active");
        assert!(
            is_snoozed(Some(future), future - time::Duration::seconds(1)),
            "active one second before it lapses"
        );
        // Boundary is exclusive: at/after the instant it has lapsed.
        assert!(!is_snoozed(Some(future), future), "lapses exactly at snooze_until");
        assert!(
            !is_snoozed(Some(future), future + time::Duration::seconds(1)),
            "inactive after it lapses"
        );
        // A snooze already in the past reads as inactive.
        let past = now - time::Duration::hours(1);
        assert!(!is_snoozed(Some(past), now), "elapsed snooze is inactive");
    }

    #[test]
    fn should_deliver_composes_snooze_mute_and_dnd() {
        // 12:00 UTC on a fixed date => now_minute = 720.
        let now = time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(12);
        let future = now + time::Duration::hours(1);
        let past = now - time::Duration::hours(1);

        // No prefs at all (the fail-open / no-row shape): always delivers.
        assert!(should_deliver(false, None, None, now));

        // Each suppressor alone blocks delivery.
        assert!(!should_deliver(true, None, None, now), "muted");
        assert!(!should_deliver(false, Some((600, 900)), None, now), "inside DND window");
        assert!(!should_deliver(false, None, Some(future), now), "active snooze");

        // Inactive variants of each do NOT block.
        assert!(should_deliver(false, Some((900, 1020)), None, now), "outside DND window");
        assert!(should_deliver(false, None, Some(past), now), "elapsed snooze");
        assert!(should_deliver(false, None, Some(now), now), "snooze lapses at its instant");

        // Overnight DND window: noon is outside, so it delivers.
        assert!(should_deliver(false, Some((1320, 480)), None, now));

        // Suppressors are independent: any one of them is enough.
        assert!(!should_deliver(true, Some((900, 1020)), Some(past), now), "mute wins");
        assert!(!should_deliver(false, Some((600, 900)), Some(past), now), "DND wins");
        assert!(!should_deliver(false, Some((900, 1020)), Some(future), now), "snooze wins");
        assert!(!should_deliver(true, Some((600, 900)), Some(future), now), "all three");
    }

    /// `should_deliver` must agree with the single-recipient decision the
    /// service derives from `is_snoozed` + `should_suppress` for EVERY input
    /// combination — it is a composition, not a reimplementation.
    #[test]
    fn should_deliver_matches_helper_composition_exhaustively() {
        let now = time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(12);
        let snoozes = [None, Some(now - time::Duration::hours(1)), Some(now + time::Duration::hours(1))];
        let dnds = [None, Some((600, 900)), Some((900, 1020)), Some((1320, 480)), Some((0, MINUTES_PER_DAY))];
        for muted in [false, true] {
            for dnd in dnds {
                for snooze in snoozes {
                    let expected = !is_snoozed(snooze, now)
                        && !should_suppress(muted, dnd, minute_of_day_utc(now));
                    assert_eq!(
                        should_deliver(muted, dnd, snooze, now),
                        expected,
                        "muted={muted} dnd={dnd:?} snooze={snooze:?}"
                    );
                }
            }
        }
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

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn notif_prefs_set_clear_and_read_snooze() {
        let p = pool();
        let repo = NotificationPrefsRepo::new(p.clone());
        let (participant, _room) = fixture(&p).await;

        assert!(repo.get_snooze(participant).await.unwrap().is_none(), "no snooze initially");

        // Arm a future snooze; it round-trips (compare to whole seconds to dodge
        // any sub-second precision difference on the wire).
        let until = time::OffsetDateTime::now_utc() + time::Duration::hours(2);
        repo.set_snooze(participant, Some(until)).await.unwrap();
        let got = repo.get_snooze(participant).await.unwrap().expect("snooze set");
        assert_eq!(
            got.unix_timestamp(),
            until.unix_timestamp(),
            "snooze_until round-trips"
        );
        assert!(is_snoozed(Some(got), time::OffsetDateTime::now_utc()), "future snooze is active");

        // Setting a DND window must not disturb the snooze (independent columns).
        repo.set_dnd(participant, Some(540), Some(1020)).await.unwrap();
        assert!(
            repo.get_snooze(participant).await.unwrap().is_some(),
            "set_dnd leaves snooze intact"
        );

        // Clear the snooze; the DND window must survive.
        repo.clear_snooze(participant).await.unwrap();
        assert!(repo.get_snooze(participant).await.unwrap().is_none(), "snooze cleared");
        assert_eq!(
            repo.get_dnd(participant).await.unwrap(),
            Some((540, 1020)),
            "clear_snooze leaves DND intact"
        );

        // Clearing again is idempotent.
        repo.clear_snooze(participant).await.unwrap();
        assert!(repo.get_snooze(participant).await.unwrap().is_none(), "second clear is a no-op");
    }

    /// The batched fan-out reads must agree with the single-recipient fns for a
    /// mixed population: muted / DND-only / snoozed / partial-NULL window /
    /// no-row participants all decode identically either way.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn notif_prefs_batched_reads_match_single_recipient_fns() {
        let p = pool();
        let repo = NotificationPrefsRepo::new(p.clone());
        let (owner, room) = fixture(&p).await;

        // Extra participants sharing the fixture room (FKs only need the rows).
        let mut population = vec![owner];
        for i in 0..5 {
            let extra = ParticipantId::new();
            sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
                .bind(extra.to_uuid())
                .bind(format!("prefs-batch-{i}-{extra}"))
                .execute(&p)
                .await
                .expect("insert participant");
            population.push(extra);
        }
        let [a, b, c, d, e, f]: [ParticipantId; 6] =
            population.clone().try_into().expect("six participants");

        // a: muted only. b: DND only. c: muted + snoozed. d: elapsed snooze.
        // e: partial window (one bound NULL => reads as unset). f: no row at all.
        let now = time::OffsetDateTime::now_utc();
        repo.mute(a, room).await.unwrap();
        repo.set_dnd(b, Some(1320), Some(480)).await.unwrap();
        repo.mute(c, room).await.unwrap();
        repo.set_snooze(c, Some(now + time::Duration::hours(2))).await.unwrap();
        repo.set_snooze(d, Some(now - time::Duration::hours(2))).await.unwrap();
        sqlx::query(
            "INSERT INTO dnd_settings (participant_id, start_minute, end_minute, updated_at)
             VALUES ($1, $2, NULL, now())",
        )
        .bind(e.to_uuid())
        .bind(540)
        .execute(&p)
        .await
        .expect("insert partial DND row");
        let _ = f; // no prefs rows: must be absent from both batch results.

        let muted = repo.muted_set(room, &population).await.unwrap();
        let rows = repo.dnd_snooze_many(&population).await.unwrap();

        for who in &population {
            assert_eq!(
                muted.contains(who),
                repo.is_muted(*who, room).await.unwrap(),
                "muted_set vs is_muted for {who}"
            );
            let row = rows.get(who).copied().unwrap_or_default();
            assert_eq!(row.dnd, repo.get_dnd(*who).await.unwrap(), "dnd_snooze_many vs get_dnd for {who}");
            assert_eq!(
                row.snooze_until.map(|t| t.unix_timestamp()),
                repo.get_snooze(*who).await.unwrap().map(|t| t.unix_timestamp()),
                "dnd_snooze_many vs get_snooze for {who}"
            );
        }
        assert!(!rows.contains_key(&f), "no dnd_settings row => absent from the map");

        // Empty input short-circuits (no DB round-trip, trivially consistent).
        assert!(repo.muted_set(room, &[]).await.unwrap().is_empty());
        assert!(repo.dnd_snooze_many(&[]).await.unwrap().is_empty());
    }
}
