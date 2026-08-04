-- Keep commit-time room authorization in the global governance lock order.
--
-- The original helper locked `rooms` before `workspaces`; channel governance,
-- workspace removal, deactivation, account erasure, and 2FA governance all use
-- workspace -> room -> authorization edges.  A message send racing one of those
-- writers could therefore deadlock (room SHARE vs workspace UPDATE). Resolve the
-- immutable tenant edge first without a lock, acquire the canonical workspace
-- fence, then lock/revalidate the room and room-membership edge.
CREATE OR REPLACE FUNCTION aero_effective_room_access(
    requested_room uuid,
    requested_participant uuid,
    expected_workspace uuid DEFAULT NULL
) RETURNS boolean
LANGUAGE plpgsql
VOLATILE
AS $$
DECLARE
    resolved_workspace uuid;
    locked_workspace uuid;
BEGIN
    SELECT workspace_id
      INTO resolved_workspace
      FROM rooms
     WHERE id = requested_room;
    IF NOT FOUND
       OR (
           expected_workspace IS NOT NULL
           AND resolved_workspace <> expected_workspace
       ) THEN
        RETURN false;
    END IF;

    -- This locks workspace first and rechecks active workspace membership,
    -- account state, deactivation, and human-only mandatory TOTP.
    IF NOT aero_effective_workspace_access(
        resolved_workspace,
        requested_participant
    ) THEN
        RETURN false;
    END IF;

    SELECT workspace_id
      INTO locked_workspace
      FROM rooms
     WHERE id = requested_room
       FOR SHARE;
    IF NOT FOUND OR locked_workspace <> resolved_workspace THEN
        RETURN false;
    END IF;

    PERFORM 1
      FROM room_members
     WHERE room_id = requested_room
       AND participant_id = requested_participant
       FOR UPDATE;
    RETURN FOUND;
END
$$;

COMMENT ON FUNCTION aero_effective_room_access(uuid, uuid, uuid) IS
    'Commit-time room access fence in workspace -> room -> membership lock order';
