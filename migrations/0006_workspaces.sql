-- Aero IM — Workspace / Org multi-tenancy data foundation (ROADMAP 方向一).
--
-- Slack-like tenancy model:
--   * `participants` stay GLOBAL user identities (unchanged here).
--   * A `workspace` (= tenant) groups members (with roles) and channels.
--   * `rooms` (channels) now BELONG TO a workspace; messages inherit tenancy
--     transitively via their room.
--
-- This migration is ADDITIVE and IDEMPOTENT: every statement is guarded
-- (`IF NOT EXISTS`, `ON CONFLICT DO NOTHING`, conditional `ALTER`) so re-running
-- it is a no-op. Threading `workspace_id` through existing queries and enforcing
-- it in services/routes is a LATER batch — this only lays the schema + a safe
-- backfill so the column can become NOT NULL without breaking existing rows.
--
-- Backfill strategy (safe on a populated DB):
--   1. Insert one deterministic "default" workspace (all-zero UUID).
--   2. Enroll every existing participant as a member of it (the oldest
--      participant as `owner`; everyone else as `member`).
--   3. Point every existing room at the default workspace.
--   4. Only then flip `rooms.workspace_id` to NOT NULL.

-- Workspaces ----------------------------------------------------------------

CREATE TABLE IF NOT EXISTS workspaces (
    id         UUID        PRIMARY KEY,
    name       TEXT        NOT NULL,
    slug       TEXT        NOT NULL UNIQUE,
    created_by UUID        REFERENCES participants(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS workspaces_created_by_idx
    ON workspaces(created_by)
    WHERE created_by IS NOT NULL;

-- Workspace membership (tenant-scoped RBAC) --------------------------------

CREATE TABLE IF NOT EXISTS workspace_members (
    workspace_id   UUID        NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    role           TEXT        NOT NULL CHECK (role IN ('owner', 'admin', 'member', 'guest')),
    joined_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (workspace_id, participant_id)
);

-- "Which workspaces does this participant belong to?" (reverse lookup).
CREATE INDEX IF NOT EXISTS workspace_members_participant_idx
    ON workspace_members(participant_id);

-- Rooms belong to a workspace ----------------------------------------------

-- Added nullable first so the backfill below can populate it before we tighten
-- the constraint to NOT NULL.
ALTER TABLE rooms
    ADD COLUMN IF NOT EXISTS workspace_id UUID REFERENCES workspaces(id);

-- Backfill ------------------------------------------------------------------

-- 1. Deterministic default workspace. The all-zero UUID is reserved as the
--    "legacy / default" tenant that pre-tenancy data lands in.
INSERT INTO workspaces (id, name, slug, created_by, created_at)
VALUES (
    '00000000-0000-0000-0000-000000000000'::uuid,
    'Default Workspace',
    'default',
    NULL,
    NOW()
)
ON CONFLICT (id) DO NOTHING;

-- 2. Enroll every existing participant into the default workspace. We pick a
--    deterministic `owner` (oldest participant) and make the rest `member`s.
--    `ON CONFLICT DO NOTHING` keeps this idempotent and avoids clobbering any
--    role a re-run might already have written.
INSERT INTO workspace_members (workspace_id, participant_id, role, joined_at)
SELECT
    '00000000-0000-0000-0000-000000000000'::uuid,
    p.id,
    CASE
        WHEN p.id = (SELECT id FROM participants ORDER BY created_at ASC, id ASC LIMIT 1)
            THEN 'owner'
        ELSE 'member'
    END,
    NOW()
FROM participants p
ON CONFLICT (workspace_id, participant_id) DO NOTHING;

-- 3. Point every existing (and any NULL) room at the default workspace.
UPDATE rooms
   SET workspace_id = '00000000-0000-0000-0000-000000000000'::uuid
 WHERE workspace_id IS NULL;

-- 4. Now that no room has a NULL workspace, enforce the invariant. SET NOT NULL
--    has no `IF NOT EXISTS`, so guard it via catalog inspection to stay idempotent.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM information_schema.columns
         WHERE table_name = 'rooms'
           AND column_name = 'workspace_id'
           AND is_nullable = 'YES'
    ) THEN
        ALTER TABLE rooms ALTER COLUMN workspace_id SET NOT NULL;
    END IF;
END
$$;

-- Hot path: list rooms (channels) within a workspace.
CREATE INDEX IF NOT EXISTS rooms_workspace_idx
    ON rooms(workspace_id);
