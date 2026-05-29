-- 0007_audit.sql — workspace audit trail (ROADMAP 方向一 合规)
--
-- An append-only log of security-relevant workspace administration: who did
-- what, to whom, in which tenant. Scoped by workspace_id so each tenant only
-- ever sees its own trail (enforced in the query layer + the GET route's
-- admin gate). Purely additive — no existing table is touched.

CREATE TABLE IF NOT EXISTS audit_events (
    id           UUID        PRIMARY KEY,
    workspace_id UUID        NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    actor_id     UUID        REFERENCES participants(id),
    action       TEXT        NOT NULL,
    target       TEXT,
    detail       JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Listing is "this workspace's events, newest first". The id is a ULID stored as
-- UUID (time-sortable byte order), so a keyset cursor walks `id` descending and
-- this index serves both the tenant filter and the ordering.
CREATE INDEX IF NOT EXISTS audit_events_workspace_idx
    ON audit_events (workspace_id, id DESC);
