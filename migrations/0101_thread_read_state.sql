-- Per-participant, per-thread read cursor: the last time a participant read
-- the replies of a thread (identified by the root message id).
-- Backs aero_storage::ThreadReadStateRepo.
CREATE TABLE thread_read_state (
    participant_id  UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    root_message_id UUID        NOT NULL,
    last_read_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (participant_id, root_message_id)
);
