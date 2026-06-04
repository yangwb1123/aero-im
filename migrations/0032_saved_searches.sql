-- 0032 Saved searches (per-user, workspace-scoped).
--
-- A user saves a named search query within a workspace, then lists, re-runs, or
-- deletes it. Running a saved search reuses the existing membership-scoped
-- cross-room search (`MessageRepo::search_all_rooms_in_workspace`), so results
-- stay bounded to rooms the owner belongs to.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS saved_searches (
  id uuid PRIMARY KEY,
  participant_id uuid NOT NULL,
  workspace_id   uuid NOT NULL,
  name text NOT NULL,
  query text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS saved_searches_owner_idx ON saved_searches (participant_id, workspace_id);
