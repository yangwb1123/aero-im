-- 0021_custom_emoji.sql — workspace custom emoji (`:shipit:`)
--
-- A workspace defines named custom emoji backed by an already-uploaded image
-- blob (clients upload via POST /api/blobs, then register the emoji by blob_id).
-- Members reference them by name in messages/reactions; the client resolves
-- name → image URL (served by the existing GET /api/blobs/:id). Purely additive,
-- scoped by workspace_id so one tenant's emoji never leak into another's.

CREATE TABLE IF NOT EXISTS custom_emoji (
    id           UUID        PRIMARY KEY,
    workspace_id UUID        NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name         TEXT        NOT NULL,
    blob_id      UUID        NOT NULL REFERENCES blobs(id),
    created_by   UUID        REFERENCES participants(id),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One image per name per workspace; the same name may exist in two
    -- workspaces. The repo relies on this constraint to reject duplicates.
    UNIQUE (workspace_id, name)
);

-- Listing is "this workspace's emoji"; the unique constraint above already
-- indexes (workspace_id, name), but an explicit workspace_id index keeps the
-- common "all emoji for a workspace" scan cheap.
CREATE INDEX IF NOT EXISTS custom_emoji_workspace_idx
    ON custom_emoji (workspace_id);
