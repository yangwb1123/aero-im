-- Workspace-wide mute: suppress ALL notifications from every channel in a workspace.
CREATE TABLE workspace_mutes (
    participant_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (participant_id, workspace_id)
);
