-- 0038 Workspace announcements / banners (admin-posted, workspace-wide).
--
-- A workspace admin posts a short banner that every member of the workspace
-- reads (e.g. "Office closed Friday"). A banner is active until its optional
-- `expires_at` passes; an admin can also delete one early. Reads are scoped to
-- the workspace and filtered to active rows, so members only ever see live
-- banners for their own tenant.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS workspace_announcements (
  id uuid PRIMARY KEY,
  workspace_id uuid NOT NULL,
  body text NOT NULL,
  created_by uuid NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  expires_at timestamptz NULL
);
CREATE INDEX IF NOT EXISTS workspace_announcements_ws_idx
  ON workspace_announcements (workspace_id, created_at DESC);
