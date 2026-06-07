-- 0034 Channel favorites / starred channels (per-user).
--
-- A user marks a channel (room) as a favorite, then lists or unstars it. Each
-- favorite is a single (participant, room) pair — the composite primary key makes
-- favoriting idempotent and needs no surrogate id. Favorites are PRIVATE to the
-- owning participant: every read/mutate is scoped to `participant_id`, so one user
-- can never see or touch another's favorites. Pure organizational metadata over
-- existing rooms — no message/room data is touched.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS channel_favorites (
  participant_id uuid NOT NULL,
  room_id        uuid NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (participant_id, room_id)
);
CREATE INDEX IF NOT EXISTS channel_favorites_owner_idx ON channel_favorites (participant_id);
