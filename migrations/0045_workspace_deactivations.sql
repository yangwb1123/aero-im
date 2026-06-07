-- 0045 Workspace user deactivation (admin revokes a member's access).
--
-- An admin/owner deactivates a member within a workspace, revoking that member's
-- access to the workspace's rooms (access-enforcement is wired separately, in
-- `ImService::assert_room_access`, via `DeactivationRepo::is_deactivated`). Each
-- deactivation is a single (workspace, participant) pair — the composite primary
-- key makes deactivating idempotent and needs no surrogate id. Reactivating a
-- member removes the row. Pure access-control metadata — no message/room data is
-- touched.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS workspace_deactivations (
  workspace_id   uuid NOT NULL,
  participant_id uuid NOT NULL,
  deactivated_at timestamptz NOT NULL DEFAULT now(),
  deactivated_by uuid,
  PRIMARY KEY (workspace_id, participant_id)
);
CREATE INDEX IF NOT EXISTS workspace_deactivations_ws_idx ON workspace_deactivations (workspace_id);
