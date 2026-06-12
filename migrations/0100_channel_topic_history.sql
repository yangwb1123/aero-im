-- Channel topic change history: one row per topic mutation on a channel room.
-- Backs aero_storage::TopicHistoryRepo.
CREATE TABLE channel_topic_history (
    id         UUID          PRIMARY KEY DEFAULT gen_random_uuid(),
    room_id    UUID          NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    changed_by UUID          NOT NULL REFERENCES participants(id),
    old_topic  TEXT,
    new_topic  TEXT,
    changed_at TIMESTAMPTZ   NOT NULL DEFAULT now()
);

CREATE INDEX channel_topic_history_room_idx
    ON channel_topic_history(room_id, changed_at DESC);
