-- Password reuse history (ROADMAP 方向五 — enterprise hardening).
--
-- On every password change / reset we record the REPLACED hash here, so a future
-- change can reject reuse of a recent password (verified by argon2 against each
-- stored hash). Pruned to the most recent N per participant by the repo. Tied to
-- the participant (passwords are a global credential, not per-workspace).
CREATE TABLE password_history (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    password_hash  TEXT        NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX password_history_participant ON password_history (participant_id, created_at DESC);
