-- 0088_thread_mutes.sql — per-user thread MUTING (the inverse of thread-follow).
--
-- A user mutes a thread — identified by its root message — so they STOP receiving
-- reply notifications for it, even if they would otherwise be a thread subscriber
-- (0040 `thread_subscriptions`) or the root author. This is the inverse of the
-- existing thread-follow: a follow ADDS a recipient to the reply fan-out, a mute
-- SUBTRACTS one. Muting only suppresses the NOTIFICATION — the room broadcast of
-- the reply is unaffected, so a muted thread still updates live in an open view.
--
-- Each mute is a single `(participant_id, root_message_id)` pair; the composite
-- primary key makes muting idempotent and needs no surrogate id. Root message ids
-- are opaque uuids (no FK to `messages`), mirroring `thread_subscriptions`.
--
-- Idempotent: safe to re-run.

CREATE TABLE IF NOT EXISTS thread_mutes (
    participant_id  uuid NOT NULL,
    root_message_id uuid NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (participant_id, root_message_id)
);

-- Reply fan-out: the dispatcher subtracts the muted set for a reply's root via
-- `root_message_id = $1`, so index that leading column for the scan.
CREATE INDEX IF NOT EXISTS thread_mutes_root_idx
    ON thread_mutes (root_message_id);
