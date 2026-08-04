-- Mandatory workspace 2FA applies to interactive human accounts only.
--
-- Bots and agents have no credentials/TOTP enrollment surface. They must still
-- retain an active participant row plus current workspace membership and must
-- not be workspace-deactivated. Room-level callers additionally enforce room
-- membership through aero_effective_room_access or their equivalent joins.
CREATE OR REPLACE FUNCTION aero_effective_workspace_access(
    requested_workspace uuid,
    requested_participant uuid
) RETURNS boolean
LANGUAGE plpgsql
VOLATILE
AS $$
DECLARE
    workspace_requires_2fa boolean;
    participant_kind text;
    participant_deleted_at timestamptz;
    totp_activated boolean;
BEGIN
    SELECT require_2fa
      INTO workspace_requires_2fa
      FROM workspaces
     WHERE id = requested_workspace
       FOR SHARE;
    IF NOT FOUND THEN
        RETURN false;
    END IF;

    PERFORM 1
      FROM workspace_members
     WHERE workspace_id = requested_workspace
       AND participant_id = requested_participant
       FOR UPDATE;
    IF NOT FOUND THEN
        RETURN false;
    END IF;

    SELECT kind, deleted_at
      INTO participant_kind, participant_deleted_at
      FROM participants
     WHERE id = requested_participant
       FOR SHARE;
    IF NOT FOUND OR participant_deleted_at IS NOT NULL THEN
        RETURN false;
    END IF;

    IF EXISTS (
        SELECT 1
          FROM workspace_deactivations
         WHERE workspace_id = requested_workspace
           AND participant_id = requested_participant
    ) THEN
        RETURN false;
    END IF;

    IF workspace_requires_2fa AND participant_kind = 'human' THEN
        SELECT activated
          INTO totp_activated
          FROM totp_secrets
         WHERE participant_id = requested_participant
           FOR SHARE;
        IF NOT FOUND OR NOT totp_activated THEN
            RETURN false;
        END IF;
    END IF;

    RETURN true;
END
$$;

COMMENT ON FUNCTION aero_effective_workspace_access(uuid, uuid) IS
    'Effective workspace access: active membership, not deactivated, and mandatory TOTP for human participants only';

-- Migration 0194 introduced a lock-free predicate for trigger/read contexts.
-- Replace it here as well: an already-upgraded database will not rerun 0194
-- even if the historical migration is also corrected for fresh deployments.
CREATE OR REPLACE FUNCTION aero_participant_has_effective_workspace_access(
    requested_workspace uuid,
    requested_participant uuid
) RETURNS boolean
LANGUAGE sql
STABLE
AS $$
    SELECT EXISTS (
        SELECT 1
          FROM workspaces workspace
          JOIN workspace_members membership
            ON membership.workspace_id = workspace.id
           AND membership.participant_id = requested_participant
          JOIN participants participant
            ON participant.id = requested_participant
           AND participant.deleted_at IS NULL
          LEFT JOIN workspace_deactivations deactivated
            ON deactivated.workspace_id = workspace.id
           AND deactivated.participant_id = requested_participant
          LEFT JOIN totp_secrets totp
            ON totp.participant_id = requested_participant
         WHERE workspace.id = requested_workspace
           AND deactivated.participant_id IS NULL
           AND (
               participant.kind <> 'human'
               OR NOT workspace.require_2fa
               OR COALESCE(totp.activated, false)
           )
    )
$$;

COMMENT ON FUNCTION aero_participant_has_effective_workspace_access(uuid, uuid) IS
    'Lock-free effective workspace predicate with mandatory TOTP for human participants only';

-- This BEFORE UPDATE trigger evaluates the proposed require_2fa=true value, so
-- it cannot call a helper that still observes the row's old value. Keep its
-- owner predicate explicit and aligned with both effective-access helpers.
CREATE OR REPLACE FUNCTION workspace_require_2fa_effective_channel_owner_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    stranded_room uuid;
BEGIN
    IF NEW.require_2fa IS NOT TRUE OR OLD.require_2fa IS TRUE THEN
        RETURN NEW;
    END IF;

    PERFORM room.id
      FROM rooms room
     WHERE room.workspace_id = NEW.id
       AND room.kind = 'channel'
     ORDER BY room.id
       FOR UPDATE;

    SELECT room.id
      INTO stranded_room
      FROM rooms room
     WHERE room.workspace_id = NEW.id
       AND room.kind = 'channel'
       AND NOT EXISTS (
           SELECT 1
             FROM room_members owner_membership
             JOIN participants participant
               ON participant.id = owner_membership.participant_id
              AND participant.deleted_at IS NULL
             JOIN workspace_members workspace_membership
               ON workspace_membership.workspace_id = room.workspace_id
              AND workspace_membership.participant_id =
                  owner_membership.participant_id
             LEFT JOIN workspace_deactivations deactivated
               ON deactivated.workspace_id = room.workspace_id
              AND deactivated.participant_id =
                  owner_membership.participant_id
             LEFT JOIN totp_secrets totp
               ON totp.participant_id = owner_membership.participant_id
            WHERE owner_membership.room_id = room.id
              AND owner_membership.role = 'owner'
              AND deactivated.participant_id IS NULL
              AND (
                  participant.kind <> 'human'
                  OR COALESCE(totp.activated, false)
              )
       )
     ORDER BY room.id
     LIMIT 1;

    IF stranded_room IS NOT NULL THEN
        RAISE EXCEPTION
            'mandatory 2FA would leave channel % without an effective owner; enroll or transfer ownership first',
            stranded_room
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_effective_owner_required';
    END IF;

    RETURN NEW;
END
$$;
