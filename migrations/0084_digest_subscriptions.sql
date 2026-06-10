-- 0084_digest_subscriptions.sql — Scheduled / recurring AI digest subscriptions.
--
-- A participant subscribes to a recurring AI digest of a ROOM or a WORKSPACE on a
-- `daily` / `weekly` cadence. The digest dispatcher polls due rows
-- (`next_run_at <= now`), summarizes the target via the AI service, delivers the
-- summary (into the room as a message for a room subscription, or into the
-- participant's activity feed for a workspace subscription), then advances
-- `next_run_at` by the frequency.
--
-- Exactly one of (room_id, workspace_id) is set — the CHECK enforces the XOR so a
-- row is unambiguously a room digest or a workspace digest.
--
-- Idempotent: safe to re-run.

CREATE TABLE IF NOT EXISTS digest_subscriptions (
    id             uuid PRIMARY KEY,
    participant_id uuid NOT NULL,
    room_id        uuid,
    workspace_id   uuid,
    frequency      text NOT NULL,
    next_run_at    timestamptz NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    -- Exactly one target: a room digest XOR a workspace digest.
    CONSTRAINT digest_subscriptions_one_target_chk
        CHECK ((room_id IS NULL) <> (workspace_id IS NULL))
);

-- Due-poll index: the dispatcher claims rows ordered by next_run_at.
CREATE INDEX IF NOT EXISTS digest_subscriptions_due_idx
    ON digest_subscriptions (next_run_at);

-- Owner lookup: list a participant's subscriptions, delete by owner.
CREATE INDEX IF NOT EXISTS digest_subscriptions_participant_idx
    ON digest_subscriptions (participant_id);
