-- 0071 Live-stream chat modes: slow mode + follower-only + subscriber-only.
--
-- Twitch-style chat restrictions a stream owner toggles for their stream's
-- danmaku chat. One row per stream (PK = stream_id, FK to streams, cascade on
-- stream delete). Absent row ⇒ all defaults (no restrictions), so a stream that
-- never configured chat modes behaves exactly as before this migration.
--
--   * slow_mode_secs  — minimum seconds between two posts from the same viewer
--                       (0 disables slow mode).
--   * follower_only   — only viewers following the creator may post.
--   * subscriber_only — only active creator-subscribers may post.
--
-- Idempotent: re-running is a no-op (IF NOT EXISTS).
CREATE TABLE IF NOT EXISTS stream_chat_settings (
  stream_id uuid PRIMARY KEY REFERENCES streams(id) ON DELETE CASCADE,
  slow_mode_secs int NOT NULL DEFAULT 0,
  follower_only boolean NOT NULL DEFAULT false,
  subscriber_only boolean NOT NULL DEFAULT false,
  updated_at timestamptz NOT NULL DEFAULT now()
);
