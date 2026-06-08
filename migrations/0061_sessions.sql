-- 0061_sessions.sql — active login session inventory + remote / global sign-out.
--
-- Records one row per active refresh-token-backed login ("device"), so a user (or
-- an admin) can list their active sessions and revoke one — or all-others ("sign
-- out everywhere else"). Integrates with the existing refresh/logout machinery:
-- `token_hash` is the SHA-256 of the REFRESH token, computed with the SAME
-- `aero_storage::revoked_token::hash_token` used by `revoked_tokens`, so a row
-- here lines up exactly with the revoked-token check that gates `POST
-- /api/auth/refresh`. Revoking a session both flips `revoked_at` here AND adds the
-- hash to `revoked_tokens`, so a refresh with the revoked token 401s.
--
-- Only the hash is stored (mirrors PAT / SCIM / webhook / revoked-token handling),
-- so the table is useless if it leaks. `participant_id` carries no FK — a session
-- record is advisory device metadata that may outlive the participant row.
--
-- Purely additive: a NEW `auth_sessions` table; no existing table is reshaped, and
-- the whole script is idempotent (safe to re-run).
CREATE TABLE IF NOT EXISTS auth_sessions (
    id             uuid        PRIMARY KEY,
    participant_id uuid        NOT NULL,
    token_hash     text        NOT NULL UNIQUE,
    user_agent     text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    last_seen_at   timestamptz NOT NULL DEFAULT now(),
    revoked_at     timestamptz
);

-- Per-user active-session listing: "my active sessions" (the GET list path). The
-- partial predicate keeps the index small (revoked rows are excluded).
CREATE INDEX IF NOT EXISTS auth_sessions_participant_idx
    ON auth_sessions (participant_id) WHERE revoked_at IS NULL;
