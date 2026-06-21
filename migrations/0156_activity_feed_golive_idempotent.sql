-- 0156_activity_feed_golive_idempotent.sql — dedup go-live activity fan-out
-- (gap-scan round 3 · idempotency).
--
-- golive_bot fans a "stream_live" activity_feed row out to every follower, then
-- acks the NATS event. A consumer crash between the fan-out and the ack causes an
-- at-least-once redelivery → the whole fan-out repeats → every follower gets a
-- DUPLICATE "X is live" feed entry. `ActivityFeedRepo::insert` always inserts
-- (no dedup), so nothing stops it.
--
-- Each broadcast has a fresh `stream_id` (streams are soft-ended, never reused),
-- so `(participant_id, subject_id)` uniquely identifies one follower's go-live
-- notice for one broadcast — a partial unique index lets the fan-out upsert with
-- ON CONFLICT DO NOTHING, making redelivery a no-op while a genuine later
-- broadcast (new stream_id) still notifies. Scoped to `kind = 'stream_live'` so no
-- other feed kind's legitimate repeats are affected.

CREATE UNIQUE INDEX IF NOT EXISTS activity_feed_stream_live_uniq
    ON activity_feed (participant_id, subject_id)
    WHERE kind = 'stream_live' AND subject_id IS NOT NULL;
