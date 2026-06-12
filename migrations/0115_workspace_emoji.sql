-- ROADMAP11 Feature 2: Custom Workspace Emoji
-- Per-workspace custom emoji (like Slack/Discord workspace emoji).
CREATE TABLE IF NOT EXISTS workspace_emoji (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    blob_id UUID NOT NULL REFERENCES blobs(id) ON DELETE CASCADE,
    created_by UUID REFERENCES participants(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (workspace_id, name)
);
CREATE INDEX IF NOT EXISTS idx_workspace_emoji_ws ON workspace_emoji (workspace_id);
