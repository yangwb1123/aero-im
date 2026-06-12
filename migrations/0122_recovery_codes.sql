-- 0122_recovery_codes.sql — 2FA recovery codes (one-time bypass codes).
--
-- A participant with TOTP 2FA activated can generate a set of one-time
-- recovery codes.  Each code is a random plaintext string stored here; it is
-- consumed (used_at stamped) on a successful 2FA-bypass login so it cannot be
-- reused.  Generating a new set deletes all existing unused codes for the
-- participant, so there is at most one active batch at a time.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS recovery_codes (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    -- The plaintext recovery code (random, one-time).  Not a password; storing
    -- plaintext is acceptable because the code is high-entropy, single-use, and
    -- the user's 2FA secret is the real credential.
    code           TEXT        NOT NULL,
    -- NULL = unused; timestamp set when the code is consumed.
    used_at        TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_recovery_codes_participant
    ON recovery_codes (participant_id);
