-- 0063 Missed-call detection.
--
-- Record when a call was first answered (NULL = never answered). A call that
-- ends while `answered_at IS NULL` was never picked up, so the server drops a
-- durable "missed call" notice into each callee's activity feed (Wave 21's
-- activity_feed). Idempotent: re-running is a no-op.
ALTER TABLE call_sessions ADD COLUMN IF NOT EXISTS answered_at timestamptz;
