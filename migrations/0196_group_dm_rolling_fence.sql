-- Rolling-upgrade fence for fixed-membership conversations.
--
-- Migration 0193 introduced `rooms.is_group_dm`, but an old application pod does
-- not know that column:
--
-- * it creates a group DM as an unmarked, nameless `group` room and then appends
--   members one statement at a time;
-- * its generic room-member writer can still append to a room that a new pod has
--   already marked as a group DM; and
-- * its generic writer can append a third member to a direct room.
--
-- This is the EXPAND half of the rollout.  Unmarked nameless rooms remain
-- writable so an old pod can finish constructing one.  New code recognizes an
-- exact legacy set and atomically claims it by setting `is_group_dm = true`.
-- Once claimed, this trigger fences every further membership append.  Direct
-- rooms retain the two inserts needed by the dedicated old/new DM constructors,
-- but a third distinct edge is rejected.
--
-- The workspace-scoped compatibility advisory lock serializes an old pod's
-- incremental legacy inserts with a new pod's legacy lookup/create transaction,
-- so a completed exact legacy row is observed and claimed rather than duplicated.
--
-- CONTRACT boundary: while old pods still exist, an incomplete unmarked,
-- nameless room cannot be distinguished from a completed legacy room except by
-- its current exact member set.  Keep the legacy lookup/fence until every old pod
-- is drained.  A later contract migration may then backfill/quarantine remaining
-- candidates and remove the compatibility branch/lock; this migration does not
-- claim that unmarked candidates are already immutable.

CREATE OR REPLACE FUNCTION aero_room_members_fixed_insert_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    target_kind text;
    target_is_group_dm boolean;
    target_name text;
    target_workspace uuid;
    existing_members bigint;
BEGIN
    -- Preserve ON CONFLICT idempotency.  This row cannot change the fixed member
    -- set, so let the unique constraint / caller's DO NOTHING handle it.
    IF EXISTS (
        SELECT 1
          FROM room_members
         WHERE room_id = NEW.room_id
           AND participant_id = NEW.participant_id
    ) THEN
        RETURN NEW;
    END IF;

    -- First resolve without a row lock so the legacy branch can obey the global
    -- workspace -> advisory -> room lock order.
    SELECT kind, is_group_dm, name, workspace_id
      INTO target_kind, target_is_group_dm, target_name, target_workspace
      FROM rooms
     WHERE id = NEW.room_id;

    IF NOT FOUND THEN
        -- The room FK remains authoritative for a missing target.
        RETURN NEW;
    END IF;

    IF target_kind = 'group'
       AND NOT target_is_group_dm
       AND target_name IS NULL THEN
        -- Old pods construct legacy group DMs through this shape.  Serialize
        -- their incremental writes with new-code exact lookup/create.
        PERFORM 1
          FROM workspaces
         WHERE id = target_workspace
           FOR SHARE;
        PERFORM pg_advisory_xact_lock(
            hashtextextended(
                format('aero:group-dm-legacy:%s', target_workspace),
                0
            )
        );
    END IF;

    -- Serialize fixed-set inspection and concurrent appends to this room.
    SELECT kind, is_group_dm, name, workspace_id
      INTO target_kind, target_is_group_dm, target_name, target_workspace
      FROM rooms
     WHERE id = NEW.room_id
       FOR UPDATE;

    IF target_is_group_dm THEN
        RAISE EXCEPTION 'group-DM membership is fixed'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'room_members_fixed_membership_insert';
    END IF;

    IF target_kind = 'direct' THEN
        SELECT count(*)
          INTO existing_members
          FROM room_members
         WHERE room_id = NEW.room_id;
        IF existing_members >= 2 THEN
            RAISE EXCEPTION 'direct-room membership is limited to two participants'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'room_members_fixed_membership_insert';
        END IF;
        RETURN NEW;
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS room_members_fixed_insert_guard ON room_members;

CREATE TRIGGER room_members_fixed_insert_guard
BEFORE INSERT ON room_members
FOR EACH ROW
EXECUTE FUNCTION aero_room_members_fixed_insert_guard();
