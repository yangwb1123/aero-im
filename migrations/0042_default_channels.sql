-- 0042 Workspace default channels (admin-curated auto-join set).
--
-- An administrator marks channels (rooms) in a workspace as "defaults": every new
-- member auto-joins them on enrollment. Each default is a single
-- (workspace, room) pair — the composite primary key makes marking idempotent and
-- needs no surrogate id. Pure organizational metadata over existing rooms: no
-- message/room data is touched, and the room must belong to the workspace (the
-- HTTP layer enforces that before inserting). The register/enroll path reads this
-- set (`DefaultChannelRepo::list`) to auto-join a new member.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS workspace_default_channels (
  workspace_id uuid NOT NULL,
  room_id      uuid NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (workspace_id, room_id)
);
CREATE INDEX IF NOT EXISTS workspace_default_channels_ws_idx ON workspace_default_channels (workspace_id);
