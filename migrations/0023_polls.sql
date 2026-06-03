-- 0023_polls.sql — in-room polls (create / vote / tally / close)
--
-- A room member creates a poll (a question plus 2..=10 options, single- or
-- multi-choice); members vote; everyone sees the live tally; the creator closes
-- it. A poll belongs to a room (cascade-deleted with it); votes belong to a poll
-- (cascade-deleted with it). Purely additive — no existing table is touched.

CREATE TABLE IF NOT EXISTS polls (
    id         UUID        PRIMARY KEY,
    room_id    UUID        NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    created_by UUID        NOT NULL REFERENCES participants(id),
    question   TEXT        NOT NULL,
    -- JSON array of option label strings; a vote references one by its index.
    options    JSONB       NOT NULL,
    -- true ⇒ a participant may pick several options; false ⇒ exactly one.
    multi      BOOLEAN     NOT NULL DEFAULT false,
    -- Set once the creator closes the poll; NULL while it accepts votes.
    closed_at  TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row per (poll, participant, chosen option). A single-choice poll keeps at
-- most one row per participant (the repo deletes-then-inserts on a re-vote); a
-- multi-choice poll keeps one row per selected option. The composite primary key
-- makes a repeated identical multi-vote idempotent.
CREATE TABLE IF NOT EXISTS poll_votes (
    poll_id        UUID        NOT NULL REFERENCES polls(id) ON DELETE CASCADE,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    option_idx     INT         NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (poll_id, participant_id, option_idx)
);

-- Tally + "did this participant vote" are both keyed on poll_id.
CREATE INDEX IF NOT EXISTS poll_votes_poll_idx
    ON poll_votes (poll_id);
