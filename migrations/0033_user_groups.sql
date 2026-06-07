-- 0033 User groups / @-usergroups (workspace-scoped named groups of members).
--
-- A workspace member creates a named group (`@designers`) of participants, then
-- lists, resolves, or deletes it and manages its membership. The handle is unique
-- within a workspace, so an `@handle` mention resolves to at most one group; the
-- mention fan-out into notifications is wired separately and reuses
-- `UserGroupRepo::resolve`/`members` as its seam — this layer owns only the CRUD.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS user_groups (
  id uuid PRIMARY KEY,
  workspace_id uuid NOT NULL,
  handle text NOT NULL,
  name   text NOT NULL,
  created_by uuid NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (workspace_id, handle)
);
CREATE INDEX IF NOT EXISTS user_groups_ws_idx ON user_groups (workspace_id);

CREATE TABLE IF NOT EXISTS user_group_members (
  group_id uuid NOT NULL REFERENCES user_groups (id) ON DELETE CASCADE,
  participant_id uuid NOT NULL,
  added_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (group_id, participant_id)
);
