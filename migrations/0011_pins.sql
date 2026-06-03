-- 0011_pins.sql — pinned messages (To-B collaboration)
--
-- A room may pin a small set of important messages (announcements, decisions,
-- links) so members can find them without scrolling history. A pin is a
-- (room, message) pair with provenance (who pinned it, when). Unpinning deletes
-- the row. Purely additive — no existing table is touched.

CREATE TABLE IF NOT EXISTS pins (
    room_id    UUID        NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    message_id UUID        NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    pinned_by  UUID        NOT NULL REFERENCES participants(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One pin per message per room; re-pinning is idempotent.
    PRIMARY KEY (room_id, message_id)
);

-- Listing is "this room's pins, newest first".
CREATE INDEX IF NOT EXISTS pins_room_idx
    ON pins (room_id, created_at DESC);
