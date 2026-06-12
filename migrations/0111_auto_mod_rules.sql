-- Auto-moderation rules engine: workspace admins define text-matching rules that
-- the IM service checks at send time. match_type is one of 'contains' (default),
-- 'exact', or 'prefix' — pure string operations, no regex dependency. action is
-- one of 'block' (reject the message), 'delete' (reserved for future post-send
-- sweep), or 'warn' (reserved for future client-side hint).
CREATE TABLE auto_mod_rules (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    pattern TEXT NOT NULL,
    match_type TEXT NOT NULL DEFAULT 'contains'
        CHECK (match_type IN ('contains', 'exact', 'prefix')),
    action TEXT NOT NULL DEFAULT 'block'
        CHECK (action IN ('block', 'delete', 'warn')),
    created_by UUID REFERENCES participants(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_auto_mod_rules_workspace ON auto_mod_rules (workspace_id);
