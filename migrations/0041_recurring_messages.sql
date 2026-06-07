-- 0041_recurring_messages.sql — Recurring scheduled messages (repeating cadence).
--
-- Complements the one-shot scheduled messages (0016): a user stages a message
-- that is re-posted on a repeating cadence (hourly / daily / weekly). A row sits
-- here with its next firing time in `next_run`; the recurring dispatcher polls
-- due rows, replays each through the normal send path, then advances `next_run`
-- to the next occurrence. Setting `active = false` cancels the series.
--
-- Idempotent: safe to re-run.

CREATE TABLE IF NOT EXISTS recurring_messages (
    id          uuid PRIMARY KEY,
    room_id     uuid NOT NULL,
    sender_id   uuid NOT NULL,
    blocks      jsonb NOT NULL,
    cadence     text NOT NULL,
    next_run    timestamptz NOT NULL,
    active      boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Due-poll index: the dispatcher claims rows ordered by next_run among those
-- still active. Partial so it stays tiny (only live series).
CREATE INDEX IF NOT EXISTS recurring_messages_due_idx
    ON recurring_messages (next_run)
    WHERE active;
