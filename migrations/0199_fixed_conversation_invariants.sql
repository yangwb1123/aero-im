-- Seal fixed-membership conversations at the database boundary.
--
-- Migration 0196 protected only membership INSERTs.  It still allowed:
--
-- * a `direct` room to commit with zero or one member;
-- * either fixed conversation kind to lose a member through DELETE;
-- * a marked group DM to be unmarked and later treated as an ordinary group; and
-- * a new pod to acquire the legacy compatibility advisory lock after an old pod
--   had inserted its nameless group room but before it wrote the first member.
--
-- The durable construction protocol is now:
--
-- * direct: insert the room and exactly two member edges in one transaction;
-- * group DM: assemble an unmarked `group` room, then make the one-way
--   `is_group_dm = false -> true` transition after the complete 3..=8 member set
--   (including `created_by`) exists.
--
-- The marker transition is the group-DM seal.  Once a fixed aggregate is
-- published, no member edge may be inserted or deleted.  Deleting the parent
-- room first remains legal: PostgreSQL's ON DELETE CASCADE runs after the parent
-- row is no longer visible to this transaction, and the child guard deliberately
-- treats that as whole-aggregate deletion rather than membership mutation.

-- Refuse to hide pre-existing corruption.  There is no safe participant that a
-- migration can invent for an incomplete conversation, so an operator must
-- repair or remove the aggregate explicitly before retrying.
DO $$
DECLARE
    invalid_room uuid;
BEGIN
    SELECT room.id
      INTO invalid_room
      FROM rooms room
     WHERE room.kind = 'direct'
       AND (
           (SELECT count(*)
              FROM room_members member
             WHERE member.room_id = room.id) <> 2
           OR NOT EXISTS (
               SELECT 1
                 FROM room_members creator_member
                WHERE creator_member.room_id = room.id
                  AND creator_member.participant_id = room.created_by
           )
       )
     ORDER BY room.id
     LIMIT 1;

    IF invalid_room IS NOT NULL THEN
        RAISE EXCEPTION
            'direct room % is not a legal two-member aggregate; repair or remove it before migration 0199',
            invalid_room
            USING ERRCODE = '23514',
                  CONSTRAINT = 'fixed_conversation_legal_membership';
    END IF;

    SELECT room.id
      INTO invalid_room
      FROM rooms room
     WHERE room.is_group_dm
       AND (
           room.kind <> 'group'
           OR (SELECT count(*)
                 FROM room_members member
                WHERE member.room_id = room.id) NOT BETWEEN 3 AND 8
           OR NOT EXISTS (
               SELECT 1
                 FROM room_members creator_member
                WHERE creator_member.room_id = room.id
                  AND creator_member.participant_id = room.created_by
           )
       )
     ORDER BY room.id
     LIMIT 1;

    IF invalid_room IS NOT NULL THEN
        RAISE EXCEPTION
            'group DM % is not a legal 3..8-member aggregate containing its creator; repair or remove it before migration 0199',
            invalid_room
            USING ERRCODE = '23514',
                  CONSTRAINT = 'fixed_conversation_legal_membership';
    END IF;
END
$$;

-- Extend the room aggregate-identity trigger from migration 0194.  An old pod
-- inserts an unmarked, nameless group room before its first room_members write.
-- Taking the same workspace -> compatibility-advisory locks here closes the
-- former birth window: a new pod cannot complete its exact-set lookup until the
-- old transaction has either committed the complete legacy room or rolled back.
--
-- This also runs before INSERT's workspace foreign-key check takes KEY SHARE,
-- preserving the canonical workspace-first lock order.
CREATE OR REPLACE FUNCTION room_channel_aggregate_identity_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.workspace_id IS DISTINCT FROM OLD.workspace_id
           OR NEW.kind IS DISTINCT FROM OLD.kind
       ) THEN
        RAISE EXCEPTION
            'room workspace and kind are immutable aggregate identity'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'room_aggregate_identity_immutable';
    END IF;

    IF TG_OP = 'INSERT' AND NEW.kind = 'channel' THEN
        PERFORM 1
          FROM workspaces workspace
         WHERE workspace.id = NEW.workspace_id
           FOR UPDATE;
        -- A missing workspace remains the foreign key's responsibility.
    ELSIF TG_OP = 'INSERT'
       AND NEW.kind = 'group'
       AND NOT NEW.is_group_dm
       AND NEW.name IS NULL THEN
        PERFORM 1
          FROM workspaces workspace
         WHERE workspace.id = NEW.workspace_id
           FOR SHARE;
        -- A missing workspace remains the foreign key's responsibility.
        PERFORM pg_advisory_xact_lock(
            hashtextextended(
                format('aero:group-dm-legacy:%s', NEW.workspace_id),
                0
            )
        );
    END IF;

    RETURN NEW;
END
$$;

-- `is_group_dm` is a one-way aggregate discriminator.  Existing unmarked
-- nameless rooms must remain claimable during the rolling upgrade, so false ->
-- true is supported only after the complete legal set exists.  A marked room
-- cannot be unmarked, and a brand-new marked row is rejected because member
-- edges cannot precede their room foreign key; callers must use the supported
-- assemble-then-seal transaction above.
CREATE OR REPLACE FUNCTION room_group_dm_marker_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    member_count bigint;
    creator_is_member boolean;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.is_group_dm THEN
            RAISE EXCEPTION
                'group DM must be assembled unmarked and sealed after its complete member set exists'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'rooms_group_dm_birth_protocol';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.is_group_dm IS NOT DISTINCT FROM OLD.is_group_dm THEN
        RETURN NEW;
    END IF;

    IF OLD.is_group_dm OR NOT NEW.is_group_dm THEN
        RAISE EXCEPTION
            'group-DM marker is immutable after sealing'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'rooms_group_dm_marker_immutable';
    END IF;

    -- room.kind is separately immutable, but keep this check local so the marker
    -- invariant remains self-contained under direct SQL.
    IF OLD.kind <> 'group' OR NEW.kind <> 'group' THEN
        RAISE EXCEPTION
            'only group rooms can be sealed as group DMs'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'rooms_group_dm_kind_check';
    END IF;

    SELECT count(*),
           COALESCE(bool_or(member.participant_id = OLD.created_by), false)
      INTO member_count, creator_is_member
      FROM room_members member
     WHERE member.room_id = OLD.id;

    IF member_count NOT BETWEEN 3 AND 8 OR NOT creator_is_member THEN
        RAISE EXCEPTION
            'group DM must contain 3..8 members including its creator before sealing'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'fixed_conversation_legal_membership';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS room_group_dm_marker_guard ON rooms;
CREATE TRIGGER room_group_dm_marker_guard
    BEFORE INSERT OR UPDATE OF is_group_dm
    ON rooms
    FOR EACH ROW
    EXECUTE FUNCTION room_group_dm_marker_guard();

-- A direct room has no marker transition, so validate its complete birth at the
-- transaction boundary.  The deferred trigger permits the supported room +
-- member-edge transaction while rejecting an incomplete autocommit INSERT.
CREATE OR REPLACE FUNCTION direct_room_birth_commit_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    current_kind text;
    current_creator uuid;
    member_count bigint;
    creator_is_member boolean;
BEGIN
    SELECT room.kind, room.created_by
      INTO current_kind, current_creator
      FROM rooms room
     WHERE room.id = NEW.id;

    -- The parent was deleted later in this transaction, including as part of a
    -- workspace teardown.  No live aggregate remains to validate.
    IF NOT FOUND OR current_kind <> 'direct' THEN
        RETURN NULL;
    END IF;

    SELECT count(*),
           COALESCE(bool_or(member.participant_id = current_creator), false)
      INTO member_count, creator_is_member
      FROM room_members member
     WHERE member.room_id = NEW.id;

    IF member_count <> 2 OR NOT creator_is_member THEN
        RAISE EXCEPTION
            'direct room must commit with exactly two members including its creator'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'fixed_conversation_legal_membership';
    END IF;

    RETURN NULL;
END
$$;

DROP TRIGGER IF EXISTS direct_room_birth_commit_guard ON rooms;
CREATE CONSTRAINT TRIGGER direct_room_birth_commit_guard
    AFTER INSERT
    ON rooms
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (NEW.kind = 'direct')
    EXECUTE FUNCTION direct_room_birth_commit_guard();

-- Keep migration 0196's INSERT fence, now paired with the DELETE fence below.
-- The room row serializes concurrent append/seal operations.  Unmarked nameless
-- legacy group rooms remain writable only until a new pod claims them.
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
    -- Preserve ON CONFLICT idempotency.  This row cannot change the member set.
    IF EXISTS (
        SELECT 1
          FROM room_members
         WHERE room_id = NEW.room_id
           AND participant_id = NEW.participant_id
    ) THEN
        RETURN NEW;
    END IF;

    -- Resolve before locking so the legacy branch obeys workspace -> advisory ->
    -- room.  The room-birth trigger already owns the advisory for old-pod
    -- construction; this repeated acquisition is transaction-reentrant.
    SELECT kind, is_group_dm, name, workspace_id
      INTO target_kind, target_is_group_dm, target_name, target_workspace
      FROM rooms
     WHERE id = NEW.room_id;

    IF NOT FOUND THEN
        RETURN NEW;
    END IF;

    IF target_kind = 'group'
       AND NOT target_is_group_dm
       AND target_name IS NULL THEN
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
            RAISE EXCEPTION
                'direct-room membership is limited to two participants'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'room_members_fixed_membership_insert';
        END IF;
    END IF;

    RETURN NEW;
END
$$;

-- Removing one edge from a live fixed aggregate is never a supported mutation.
-- We first classify without a parent lock so a parent-first cascade can return
-- immediately.  A standalone child-first delete then takes the room NOWAIT:
-- this avoids a child -> parent wait cycle with a concurrent whole-room delete;
-- callers receive a retryable serialization failure instead of a deadlock.
CREATE OR REPLACE FUNCTION aero_room_members_fixed_delete_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    target_kind text;
    target_is_group_dm boolean;
BEGIN
    SELECT room.kind, room.is_group_dm
      INTO target_kind, target_is_group_dm
      FROM rooms room
     WHERE room.id = OLD.room_id;

    IF NOT FOUND THEN
        RETURN OLD;
    END IF;
    IF target_kind <> 'direct' AND NOT target_is_group_dm THEN
        RETURN OLD;
    END IF;

    BEGIN
        SELECT room.kind, room.is_group_dm
          INTO target_kind, target_is_group_dm
          FROM rooms room
         WHERE room.id = OLD.room_id
           FOR UPDATE NOWAIT;
    EXCEPTION
        WHEN lock_not_available THEN
            RAISE EXCEPTION
                'fixed conversation is concurrently changing; retry'
                USING ERRCODE = '40001';
    END;

    IF NOT FOUND THEN
        RETURN OLD;
    END IF;
    IF target_kind = 'direct' OR target_is_group_dm THEN
        RAISE EXCEPTION 'fixed-conversation membership cannot be removed'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'room_members_fixed_membership_delete';
    END IF;

    RETURN OLD;
END
$$;

DROP TRIGGER IF EXISTS room_members_fixed_delete_guard ON room_members;
CREATE TRIGGER room_members_fixed_delete_guard
    BEFORE DELETE
    ON room_members
    FOR EACH ROW
    EXECUTE FUNCTION aero_room_members_fixed_delete_guard();

COMMENT ON FUNCTION room_group_dm_marker_guard() IS
    'One-way group-DM seal: false->true requires 3..8 members including created_by; true->false is forbidden.';
COMMENT ON TRIGGER direct_room_birth_commit_guard ON rooms IS
    'Deferred birth invariant: every surviving direct room commits with exactly two members including created_by.';
COMMENT ON TRIGGER room_members_fixed_delete_guard ON room_members IS
    'Reject member removal from live direct/marked-group-DM aggregates while allowing parent-first room cascades.';
