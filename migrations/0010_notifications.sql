-- 0010_notifications.sql — per-recipient notification inbox (mentions & thread replies)
--
-- To-B collaboration: when a message @-mentions a participant (a `Block::Mention`)
-- or replies to a participant's message (`reply_to`), the server persists a row
-- here so the recipient sees a durable, cross-device inbox + unread badge — not
-- just a transient realtime ping. Listing is always scoped to one participant, so
-- one user's inbox can never surface another's. Purely additive — no existing
-- table is touched.

CREATE TABLE IF NOT EXISTS notifications (
    id             UUID        PRIMARY KEY,
    -- The recipient whose inbox this lands in.
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    room_id        UUID        NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    message_id     UUID        NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- 'mention' | 'reply' — the reason the recipient was notified.
    kind           TEXT        NOT NULL,
    -- Who triggered it (the message sender); NULL for system notifications.
    actor_id       UUID        REFERENCES participants(id),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- NULL until the recipient marks it read.
    read_at        TIMESTAMPTZ
);

-- Inbox listing is "this participant's notifications, newest first". The id is a
-- ULID stored as UUID (time-sortable), so a keyset cursor walks `id` descending
-- and this index serves both the recipient filter and the ordering.
CREATE INDEX IF NOT EXISTS notifications_inbox_idx
    ON notifications (participant_id, id DESC);

-- Unread-count / unread-listing fast path: a partial index over just the unread
-- rows per recipient keeps the badge query cheap as read history grows.
CREATE INDEX IF NOT EXISTS notifications_unread_idx
    ON notifications (participant_id)
    WHERE read_at IS NULL;
