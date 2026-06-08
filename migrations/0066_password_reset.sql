-- Password-reset tokens: one row per issued reset request.
--
-- Only the SHA-256 hash of the plaintext token is stored (mirrors the
-- revoked_tokens / auth_sessions / PAT pattern). A token is single-use:
-- `used_at` is stamped atomically on consumption so a replay returns nothing.
-- Rows expire (expires_at) and are cleaned up by the consuming query; a
-- background sweep can also prune expired+used rows periodically.

CREATE TABLE password_reset_tokens (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    token_hash     TEXT        NOT NULL UNIQUE,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    expires_at     TIMESTAMPTZ NOT NULL,
    used_at        TIMESTAMPTZ
);

CREATE INDEX password_reset_tokens_participant_idx
    ON password_reset_tokens (participant_id);
