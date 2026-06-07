-- 0040 Thread subscriptions / follow (per-user).
--
-- A user follows a thread (its root message) to be notified of new replies, even
-- when they are not @-mentioned. Each subscription is a single
-- (participant, root_message) pair — the composite primary key makes following
-- idempotent and needs no surrogate id. Subscriptions are PRIVATE to the owning
-- participant: the caller's reads/mutates are scoped to `participant_id`, while
-- the fan-out lookup (`subscribers`) scans by `root_message_id`. Pure
-- notification-routing metadata over existing messages — no message/room data is
-- touched. The HTTP layer is responsible for the room-access check before
-- subscribing.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS thread_subscriptions (
  participant_id  uuid NOT NULL,
  root_message_id uuid NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (participant_id, root_message_id)
);
-- Reply fan-out: given a reply's root, list every subscriber to notify.
CREATE INDEX IF NOT EXISTS thread_subscriptions_root_idx ON thread_subscriptions (root_message_id);
