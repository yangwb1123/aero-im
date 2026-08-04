-- Keep optional live/scheduled-stream room links inside the creator's current
-- effective tenant boundary. Repository transactions call the functions below
-- before writes; triggers are the final guard for direct SQL and future writers.

-- A deactivation must serialize with writes that lock the corresponding
-- workspace membership. The composite FK also removes orphan deactivations left
-- by legacy member-removal paths.
DELETE FROM workspace_deactivations deactivation
 WHERE NOT EXISTS (
           SELECT 1
             FROM workspace_members membership
            WHERE membership.workspace_id = deactivation.workspace_id
              AND membership.participant_id = deactivation.participant_id
       );

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'workspace_deactivations_membership_fk'
           AND conrelid = 'workspace_deactivations'::regclass
    ) THEN
        ALTER TABLE workspace_deactivations
            ADD CONSTRAINT workspace_deactivations_membership_fk
            FOREIGN KEY (workspace_id, participant_id)
            REFERENCES workspace_members (workspace_id, participant_id)
            ON DELETE CASCADE;
    END IF;
END
$$;

-- This function intentionally takes row locks in separate statements. If a
-- concurrent revocation already holds a lock, the later statements receive a
-- fresh READ COMMITTED snapshot after waiting and observe that revocation.
CREATE OR REPLACE FUNCTION aero_effective_workspace_access(
    requested_workspace uuid,
    requested_participant uuid
) RETURNS boolean
LANGUAGE plpgsql
VOLATILE
AS $$
DECLARE
    workspace_requires_2fa boolean;
    participant_deleted_at timestamptz;
    totp_activated boolean;
BEGIN
    PERFORM 1
      FROM workspace_members
     WHERE workspace_id = requested_workspace
       AND participant_id = requested_participant
       FOR UPDATE;
    IF NOT FOUND THEN
        RETURN false;
    END IF;

    SELECT require_2fa
      INTO workspace_requires_2fa
      FROM workspaces
     WHERE id = requested_workspace
       FOR SHARE;
    IF NOT FOUND THEN
        RETURN false;
    END IF;

    SELECT deleted_at
      INTO participant_deleted_at
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

    IF workspace_requires_2fa THEN
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

CREATE OR REPLACE FUNCTION aero_effective_room_access(
    requested_room uuid,
    requested_participant uuid,
    expected_workspace uuid DEFAULT NULL
) RETURNS boolean
LANGUAGE plpgsql
VOLATILE
AS $$
DECLARE
    actual_workspace uuid;
BEGIN
    SELECT workspace_id
      INTO actual_workspace
      FROM rooms
     WHERE id = requested_room
       FOR SHARE;
    IF NOT FOUND
       OR (expected_workspace IS NOT NULL AND actual_workspace <> expected_workspace) THEN
        RETURN false;
    END IF;

    IF NOT aero_effective_workspace_access(actual_workspace, requested_participant) THEN
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

-- Existing invalid optional links are detached; invalid scheduled rows whose
-- creator no longer belongs to their workspace are removed rather than exposed
-- through a tenant they cannot access.
UPDATE streams stream
   SET room_id = NULL
 WHERE stream.room_id IS NOT NULL
   AND NOT aero_effective_room_access(stream.room_id, stream.owner_id, NULL);

DELETE FROM scheduled_streams scheduled
 WHERE NOT aero_effective_workspace_access(
               scheduled.workspace_id,
               scheduled.created_by
           );

UPDATE scheduled_streams scheduled
   SET room_id = NULL
 WHERE scheduled.room_id IS NOT NULL
   AND NOT aero_effective_room_access(
               scheduled.room_id,
               scheduled.created_by,
               scheduled.workspace_id
           );

-- The original scheduled-stream table predated tenancy foreign keys. The
-- composite key makes `workspace_id + room_id` a database-level containment
-- invariant in addition to the effective-membership trigger.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'rooms_workspace_id_id_unique'
           AND conrelid = 'rooms'::regclass
    ) THEN
        ALTER TABLE rooms
            ADD CONSTRAINT rooms_workspace_id_id_unique
            UNIQUE (workspace_id, id);
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'scheduled_streams_workspace_fk'
           AND conrelid = 'scheduled_streams'::regclass
    ) THEN
        ALTER TABLE scheduled_streams
            ADD CONSTRAINT scheduled_streams_workspace_fk
            FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'scheduled_streams_room_fk'
           AND conrelid = 'scheduled_streams'::regclass
    ) THEN
        ALTER TABLE scheduled_streams
            ADD CONSTRAINT scheduled_streams_room_fk
            FOREIGN KEY (room_id) REFERENCES rooms(id) ON DELETE SET NULL;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'scheduled_streams_workspace_room_fk'
           AND conrelid = 'scheduled_streams'::regclass
    ) THEN
        ALTER TABLE scheduled_streams
            ADD CONSTRAINT scheduled_streams_workspace_room_fk
            FOREIGN KEY (workspace_id, room_id)
            REFERENCES rooms(workspace_id, id)
            ON DELETE SET NULL (room_id);
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'scheduled_streams_creator_fk'
           AND conrelid = 'scheduled_streams'::regclass
    ) THEN
        ALTER TABLE scheduled_streams
            ADD CONSTRAINT scheduled_streams_creator_fk
            FOREIGN KEY (created_by) REFERENCES participants(id) ON DELETE CASCADE;
    END IF;
END
$$;

CREATE INDEX IF NOT EXISTS scheduled_streams_room_idx
    ON scheduled_streams (room_id)
    WHERE room_id IS NOT NULL;

CREATE OR REPLACE FUNCTION aero_validate_stream_room_containment()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.room_id IS NOT NULL
       AND NOT aero_effective_room_access(NEW.room_id, NEW.owner_id, NULL) THEN
        RAISE EXCEPTION 'stream owner cannot access requested room'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'streams_room_containment';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS streams_room_containment_trigger ON streams;
CREATE TRIGGER streams_room_containment_trigger
BEFORE INSERT OR UPDATE OF owner_id, room_id ON streams
FOR EACH ROW EXECUTE FUNCTION aero_validate_stream_room_containment();

CREATE OR REPLACE FUNCTION aero_validate_scheduled_stream_containment()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.room_id IS NULL THEN
        IF NOT aero_effective_workspace_access(NEW.workspace_id, NEW.created_by) THEN
            RAISE EXCEPTION 'scheduled-stream creator cannot access workspace'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'scheduled_streams_workspace_containment';
        END IF;
    ELSIF NOT aero_effective_room_access(
        NEW.room_id,
        NEW.created_by,
        NEW.workspace_id
    ) THEN
        RAISE EXCEPTION 'scheduled-stream creator cannot access requested room'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'scheduled_streams_room_containment';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS scheduled_streams_containment_trigger ON scheduled_streams;
CREATE TRIGGER scheduled_streams_containment_trigger
BEFORE INSERT OR UPDATE OF workspace_id, room_id, created_by ON scheduled_streams
FOR EACH ROW EXECUTE FUNCTION aero_validate_scheduled_stream_containment();
