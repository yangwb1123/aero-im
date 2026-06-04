-- 0028_drafts.sql — server-persisted per-room message drafts
--
-- A user's in-progress composer message for a room is saved server-side so it
-- follows them across devices/reloads (Slack drafts). A draft is PRIVATE to the
-- authoring participant and there is exactly one per (participant, room): an
-- upsert replaces the previous draft for that pair. Saving a draft does not
-- send anything — it only stages the composer's blocks (and an optional reply
-- target). Purely additive — no existing table is touched.

CREATE TABLE IF NOT EXISTS message_drafts (
  participant_id uuid NOT NULL,
  room_id        uuid NOT NULL,
  blocks         jsonb NOT NULL,
  reply_to       uuid,
  updated_at     timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (participant_id, room_id)
);

-- Listing is "this user's drafts across rooms, most-recently-edited first".
CREATE INDEX IF NOT EXISTS message_drafts_participant_idx
  ON message_drafts (participant_id, updated_at DESC);
