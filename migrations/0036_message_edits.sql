-- 0036 Message edit history (prior versions of an edited message).
--
-- Each time a message is edited, its OLD block content is captured here BEFORE
-- the live `messages` row is overwritten, so members can review what a message
-- said previously. One row per prior version; the most recent edit sorts first
-- by `recorded_at`. The capture-on-edit hook lives in the edit path
-- (`ImService::edit_message`) — this store only owns the append-only history and
-- the membership-gated read.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS message_edits (
  id          uuid PRIMARY KEY,
  message_id  uuid NOT NULL,
  editor_id   uuid NOT NULL,
  blocks      jsonb NOT NULL,
  recorded_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS message_edits_msg_idx ON message_edits (message_id, recorded_at DESC);
