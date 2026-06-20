-- Bots (方向三 — 开放平台).
-- Each bot is also a participant (kind='bot') with its own JWT identity.
-- Bot tokens are opaque bearer tokens the bot uses to authenticate as itself;
-- they are distinct from Personal Access Tokens (pat_tokens).

CREATE TABLE IF NOT EXISTS bots (
    id              UUID PRIMARY KEY,
    owner_id        UUID NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    icon_url        TEXT,
    -- Scoped to the workspace the bot operates in (NULL = system-wide).
    workspace_id    UUID REFERENCES workspaces(id) ON DELETE SET NULL,
    -- SHA-256 hash of the current bot token; NULL when no token has been issued.
    token_hash      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS bots_owner_idx ON bots (owner_id);
