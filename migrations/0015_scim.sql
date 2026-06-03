-- 0015_scim.sql — SCIM 2.0 provisioning (RFC 7643/7644) Users + Groups.
--
-- Lets an external IdP (Okta / Azure AD / OneLogin) provision and de-provision
-- workspace members over the SCIM protocol, authenticated by a per-workspace
-- bearer token (NOT a participant JWT). A SCIM "User" maps to a GLOBAL
-- `participant` plus a `workspace_member` of the token's workspace; a SCIM
-- "Group" maps (read-only) to the workspace itself.
--
-- This migration is ADDITIVE and IDEMPOTENT: every statement is guarded
-- (`IF NOT EXISTS`) so re-running it is a no-op. No existing table is altered.
--
-- Why a separate `scim_users` table (vs. columns on `workspace_members`):
--   * Keeps SCIM-only attributes (userName, externalId, active, timestamps) off
--     the hot `workspace_members` table that many queries touch.
--   * The SCIM `active` flag is distinct from membership: a deprovisioned user is
--     marked `active = false` AND has their membership removed, but the row is
--     retained so the IdP can still GET/reactivate by the same id.
--   * `participants` stay global; tenancy + SCIM identity are membership edges,
--     so SCIM identity is naturally workspace-scoped here.

-- SCIM bearer tokens (per-workspace) ---------------------------------------

CREATE TABLE IF NOT EXISTS scim_tokens (
    id           UUID        PRIMARY KEY,
    workspace_id UUID        NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- Only the SHA-256 hash of the token is stored; the plaintext is shown once
    -- at mint time and never persisted (mirrors password-hash handling).
    token_hash   TEXT        NOT NULL UNIQUE,
    label        TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- NULL = active; set on revoke. Resolution filters on `revoked_at IS NULL`.
    revoked_at   TIMESTAMPTZ
);

-- Hot path: resolve an incoming bearer token's SHA-256 → its workspace.
CREATE INDEX IF NOT EXISTS scim_tokens_token_hash_idx
    ON scim_tokens (token_hash);

-- "Which tokens does this workspace have?" (management list / revoke).
CREATE INDEX IF NOT EXISTS scim_tokens_workspace_idx
    ON scim_tokens (workspace_id);

-- SCIM user mapping (workspace-scoped identity over a global participant) ----

CREATE TABLE IF NOT EXISTS scim_users (
    workspace_id   UUID        NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    -- SCIM `userName` — unique within a workspace (the IdP's login handle).
    user_name      TEXT        NOT NULL,
    -- SCIM `externalId` — the IdP's opaque id for the user (optional).
    external_id    TEXT,
    active         BOOLEAN     NOT NULL DEFAULT true,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, participant_id)
);

-- `userName eq "x"` is THE common Okta/Azure provisioning query; it must be
-- unique per workspace (RFC 7643 §4.1.1) and fast to look up.
CREATE UNIQUE INDEX IF NOT EXISTS scim_users_workspace_user_name_idx
    ON scim_users (workspace_id, user_name);

-- `externalId` lookups (the IdP correlates by its own id); not unique because it
-- is optional and not guaranteed unique across IdP configurations.
CREATE INDEX IF NOT EXISTS scim_users_workspace_external_id_idx
    ON scim_users (workspace_id, external_id)
    WHERE external_id IS NOT NULL;
