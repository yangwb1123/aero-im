-- 0018_notification_prefs.sql — notification preferences (per-channel mute + per-user DND)
--
-- To-B/To-C notification control: a participant can MUTE a specific room (no
-- mention/reply pings from it) and set a daily Do-Not-Disturb (DND) window during
-- which they receive no notifications at all. Both are consulted by the server
-- *before* a notification is persisted/pushed (the suppression seam lives in
-- `ImService`). Purely additive — no existing table is touched. Idempotent.

-- Per-(participant, room) mute. Presence of a row = that participant has muted
-- that room. Idempotent upsert on mute; DELETE on unmute. CASCADEs with both the
-- participant and the room so stale prefs can never outlive their subjects.
CREATE TABLE IF NOT EXISTS channel_mutes (
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    room_id        UUID        NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (participant_id, room_id)
);

-- "Which rooms has this participant muted" reverse lookup (the PK already serves
-- the (participant, room) point check); newest-first for a stable listing.
CREATE INDEX IF NOT EXISTS channel_mutes_participant_idx
    ON channel_mutes (participant_id, created_at DESC);

-- Per-participant Do-Not-Disturb window, expressed as minutes-of-day in the
-- range 0..1440 (so 9:00 = 540, 22:30 = 1350). NULL/NULL = no DND configured.
-- A window is [start_minute, end_minute): when start <= end it is a same-day
-- window; when start > end it is an OVERNIGHT window that wraps past midnight
-- (e.g. 1320..480 = 22:00 -> 08:00). DND is evaluated in UTC for now (see
-- `ImService` suppression seam).
CREATE TABLE IF NOT EXISTS dnd_settings (
    participant_id UUID        PRIMARY KEY REFERENCES participants(id) ON DELETE CASCADE,
    start_minute   INT,
    end_minute     INT,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
