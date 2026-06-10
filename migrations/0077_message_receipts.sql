-- 0077_message_receipts.sql — per-message read receipts ("Seen by")
--
-- Today read state is per-ROOM only (read_receipts = one row per (room,
-- participant) holding the highest message id seen). That powers unread badges
-- but cannot answer "who has seen THIS specific message" (Slack/Teams/Lark
-- "Seen by …"). This table records an explicit acknowledgement of an individual
-- message: one row per (message, participant), set when the participant marks
-- the message seen, with the timestamp of first acknowledgement.
--
-- Distinct from `read_receipts`: that is a monotonic cursor for unread counts;
-- this is a precise per-message reader set. Purely additive — no existing table
-- is touched.

CREATE TABLE IF NOT EXISTS message_receipts (
    message_id     UUID        NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    read_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One acknowledgement per (message, participant); re-marking is idempotent
    -- and keeps the original read_at (provenance of first-seen).
    PRIMARY KEY (message_id, participant_id)
);

-- Listing is "who has seen this message" (the reader list for one message),
-- ordered by when they first saw it.
CREATE INDEX IF NOT EXISTS message_receipts_message_idx
    ON message_receipts (message_id, read_at ASC);
