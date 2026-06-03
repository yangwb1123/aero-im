-- 0019_pat.sql — Personal Access Tokens (PAT): programmatic REST API auth.
--
-- A participant mints long-lived API tokens (`aero_pat_<random>`) to call the
-- REST API programmatically, as an alternative to a short-lived participant JWT.
-- Any `AuthUser`-gated route accepts a PAT in the `Authorization: Bearer` header
-- exactly where it accepts an access JWT.
--
-- This migration is ADDITIVE and IDEMPOTENT: every statement is guarded
-- (`IF NOT EXISTS`) so re-running it is a no-op. No existing table is altered.
--
-- Only the SHA-256 hash of a token is stored; the plaintext is shown once at mint
-- time and never persisted (mirrors password / SCIM / webhook token handling).

CREATE TABLE IF NOT EXISTS pat_tokens (
    id             UUID        PRIMARY KEY,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    -- SHA-256 hex of the plaintext token; the plaintext itself is never stored.
    token_hash     TEXT        NOT NULL UNIQUE,
    -- Optional human label so a user can tell their tokens apart.
    name           TEXT,
    -- Reserved for future fine-grained authorization; empty = full participant
    -- scope (the token acts as the participant). Stored, not yet enforced.
    scopes         TEXT[]      NOT NULL DEFAULT '{}',
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Best-effort "last seen" bump on every successful verify (analytics / audit).
    last_used_at   TIMESTAMPTZ,
    -- NULL = never expires; otherwise the token is inactive once now() >= this.
    expires_at     TIMESTAMPTZ,
    -- NULL = active; set on revoke. Resolution filters on `revoked_at IS NULL`.
    revoked_at     TIMESTAMPTZ
);

-- Hot path: resolve an incoming bearer token's SHA-256 → its owner on every
-- PAT-authenticated request. The column is UNIQUE (above) so this is a point
-- lookup; the explicit index documents the access pattern and matches the
-- convention used by the other token tables (SCIM / webhooks on live master).
CREATE INDEX IF NOT EXISTS pat_tokens_token_hash_idx
    ON pat_tokens (token_hash);

-- "Which tokens does this participant have?" (management list / revoke).
CREATE INDEX IF NOT EXISTS pat_tokens_participant_idx
    ON pat_tokens (participant_id);
