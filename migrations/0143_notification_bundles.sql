-- Notification bundling (ROADMAP7 方向三: 通知聚合).
--
-- Pending notification rows that are held for a short delay so multiple replies
-- to the same thread can be folded into one AggregateReply notification instead
-- of N individual Reply rows. A background sweep (every 60s) flushes bundles
-- older than `AERO_BUNDLE_DEADLINE_SECS` (default 30s, min 10s).
--
-- When a sweep finds >1 bundle for the same (participant, room, thread_root),
-- it inserts a single AggregateReply with `aggregate_count = N`; singles insert
-- a normal Reply. Mentions are never bundled.

CREATE TABLE IF NOT EXISTS notification_bundles (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    participant_id  UUID NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    room_id         UUID NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    message_id      UUID NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    kind            TEXT NOT NULL,  -- 'reply' | 'mention'
    actor_id        UUID REFERENCES participants(id) ON DELETE SET NULL,
    thread_root     UUID,           -- reply_to > root when kind='reply', else NULL
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Composite index for the flush query: group by (participant_id, room, thread_root).
CREATE INDEX idx_bundles_flush
    ON notification_bundles (participant_id, room_id, thread_root, created_at);

-- Sweep can also target the oldest bundles regardless of participant.
CREATE INDEX idx_bundles_created_at ON notification_bundles (created_at);
