-- User-level report flow: any authenticated user can report another user within a
-- workspace context. The UNIQUE constraint prevents duplicate reports from the same
-- reporter/reported/workspace triple. workspace_id is nullable so a report can be
-- filed outside any specific workspace context.
CREATE TABLE user_reports (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    reporter_id UUID NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    reported_id UUID NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    workspace_id UUID REFERENCES workspaces(id) ON DELETE CASCADE,
    reason TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'reviewed', 'dismissed')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (reporter_id, reported_id, workspace_id)
);
CREATE INDEX idx_user_reports_workspace ON user_reports (workspace_id, status);
