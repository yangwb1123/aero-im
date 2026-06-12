-- Migration 0119: workspace-level default notification level.
-- When a participant joins a channel in a workspace that has a default set, and
-- they have no existing channel_notification_prefs row for that channel, the
-- default level is applied automatically.
CREATE TABLE workspace_notification_defaults (
    workspace_id UUID PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    default_level TEXT NOT NULL DEFAULT 'all'
        CHECK (default_level IN ('all', 'mentions', 'none')),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
