-- 0086_block_interactions.sql — interactive message-block interactions
--
-- Messages may now carry interactive blocks (Slack Block Kit-lite): a `Button`
-- or a `Select` dropdown (aero_common::Block). When a participant clicks a button
-- or picks an option, the interactions endpoint records it here so the message's
-- poster — typically a bot/webhook/app integration — can collect actionable
-- replies. One row per recorded interaction (NOT deduplicated: a participant may
-- click the same button several times, or pick different Select values; each is a
-- distinct, time-ordered event). `value` is the chosen Select option's value (or
-- a button payload); NULL for a value-less button click.
--
-- Purely additive — no existing table is touched. Cascades on message/room/
-- participant deletion so interactions never outlive what they reference.

CREATE TABLE IF NOT EXISTS block_interactions (
    id             UUID        PRIMARY KEY,
    message_id     UUID        NOT NULL REFERENCES messages(id)     ON DELETE CASCADE,
    room_id        UUID        NOT NULL REFERENCES rooms(id)        ON DELETE CASCADE,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    -- The interactive block's action_id this interaction targeted.
    action_id      TEXT        NOT NULL,
    -- The chosen Select option's value / button payload; NULL for a value-less
    -- button click.
    value          TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Listing is "every interaction on this message", oldest first, so the poster
-- reads a chronological tally of who clicked/picked what.
CREATE INDEX IF NOT EXISTS block_interactions_message_idx
    ON block_interactions (message_id, created_at ASC, id ASC);
