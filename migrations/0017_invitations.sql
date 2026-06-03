-- 0017_invitations.sql — workspace invitations / shareable invite links.
--
-- Lets a workspace admin/owner invite people to a workspace either by email or
-- via a shareable link. A logged-in invitee accepts an invite (by its token) and
-- becomes a `workspace_member` with the invite's role. A single invite may be a
-- one-shot email invite or an open multi-use link, bounded by an optional
-- `max_uses` and/or `expires_at`.
--
-- Only the SHA-256 *hash* of the invite token is stored; the plaintext token is
-- shown to the creator exactly once (embedded in the returned `invite_url`) and
-- is never persisted — mirroring password / provisioning-token handling.
--
-- This migration is ADDITIVE and IDEMPOTENT: every statement is guarded
-- (`IF NOT EXISTS`) so re-running it is a no-op. No existing table is altered.

CREATE TABLE IF NOT EXISTS invitations (
    id           UUID        PRIMARY KEY,
    workspace_id UUID        NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- SHA-256 hash of the plaintext token; the token itself is never stored.
    token_hash   TEXT        NOT NULL UNIQUE,
    -- NULL = open shareable link (anyone with the token); otherwise an
    -- informational hint of who the invite was addressed to.
    email        TEXT,
    -- The workspace role granted on accept. Tokens: guest/member/admin/owner.
    role         TEXT        NOT NULL DEFAULT 'member',
    -- The admin/owner who created the invite (NULL if the participant is gone).
    created_by   UUID        REFERENCES participants(id),
    -- NULL = unlimited uses; otherwise the invite is exhausted once
    -- `use_count >= max_uses`.
    max_uses     INT,
    use_count    INT         NOT NULL DEFAULT 0,
    -- NULL = never expires.
    expires_at   TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- NULL = active; set on revoke. Active lookups filter on `revoked_at IS NULL`.
    revoked_at   TIMESTAMPTZ
);

-- Hot path: resolve an incoming invite token's SHA-256 → its invitation row.
CREATE INDEX IF NOT EXISTS invitations_token_hash_idx
    ON invitations (token_hash);

-- "Which invites does this workspace have?" (admin listing / revoke).
CREATE INDEX IF NOT EXISTS invitations_workspace_idx
    ON invitations (workspace_id);
