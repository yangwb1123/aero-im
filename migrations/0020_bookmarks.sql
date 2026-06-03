-- 0020_bookmarks.sql — saved items / bookmarks (personal save-for-later)
--
-- A user privately saves messages to revisit later (Slack "Saved items"). Unlike
-- pins (which are per-room and visible to every member), a bookmark is per-user
-- and the saved list crosses rooms. A bookmark is a (participant, message) pair
-- with the owning room captured for access checks + rendering, an optional note,
-- and when it was saved. Unsaving deletes the row. Purely additive — no existing
-- table is touched.

CREATE TABLE IF NOT EXISTS bookmarks (
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    message_id     UUID        NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    room_id        UUID        NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    note           TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One bookmark per message per user; re-saving is idempotent.
    PRIMARY KEY (participant_id, message_id)
);

-- Listing is "this user's saved items, newest-saved first" (cross-room).
CREATE INDEX IF NOT EXISTS bookmarks_participant_idx
    ON bookmarks (participant_id, created_at DESC);
