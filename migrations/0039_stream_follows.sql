-- 0039 Stream / creator follows (per-user).
--
-- A participant follows a creator (another participant); when the followed
-- creator goes live, the orchestrator notifies every follower. Each follow is a
-- single (follower, streamer) pair — the composite primary key makes following
-- idempotent and needs no surrogate id. A self-follow (follower = streamer) is
-- the caller's concern (the HTTP layer rejects it); the table itself stores
-- opaque participant uuids with no FK. The streamer index backs the notify
-- fan-out (`followers(streamer)`), which lists everyone to alert on go-live.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS stream_follows (
  follower_id uuid NOT NULL,
  streamer_id uuid NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (follower_id, streamer_id)
);
CREATE INDEX IF NOT EXISTS stream_follows_streamer_idx ON stream_follows (streamer_id);
