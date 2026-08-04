-- A channel must always retain at least one owner who can actually govern it.
--
-- A bare room_members.role = 'owner' row is not sufficient when that
-- participant was deleted, removed/deactivated in the workspace, or does not
-- satisfy the workspace's mandatory-2FA policy.  Repair historical rows first,
-- then install database guards so application bugs, SCIM/direct SQL mutations
-- of existing owners, and concurrent demotions cannot silently leave an
-- ungovernable channel.

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

CREATE OR REPLACE FUNCTION aero_channel_has_other_effective_owner(
    requested_room uuid,
    excluded_participant uuid
) RETURNS boolean
LANGUAGE sql
STABLE
AS $$
    SELECT EXISTS (
        SELECT 1
          FROM room_members owner_membership
          JOIN rooms room
            ON room.id = owner_membership.room_id
         WHERE owner_membership.room_id = requested_room
           AND owner_membership.participant_id <> excluded_participant
           AND owner_membership.role = 'owner'
           AND aero_participant_has_effective_workspace_access(
                   room.workspace_id,
                   owner_membership.participant_id
               )
    )
$$;

-- Repair channels that have no EFFECTIVE owner.  Candidates are deliberately
-- restricted to existing room_members: auto-enrolling a workspace administrator
-- into a private channel would disclose its history.  Prefer the creator when
-- still eligible, then a current room admin, then a workspace administrator,
-- then the earliest existing effective member.
WITH channels_without_effective_owner AS (
    SELECT room.id
      FROM rooms room
     WHERE room.kind = 'channel'
       AND NOT EXISTS (
           SELECT 1
             FROM room_members owner_membership
            WHERE owner_membership.room_id = room.id
              AND owner_membership.role = 'owner'
              AND aero_participant_has_effective_workspace_access(
                      room.workspace_id,
                      owner_membership.participant_id
                  )
       )
),
ranked_candidates AS (
    SELECT room.id AS room_id,
           membership.participant_id,
           row_number() OVER (
               PARTITION BY room.id
               ORDER BY
                   (membership.participant_id = room.created_by) DESC,
                   (membership.role = 'admin') DESC,
                   (workspace_membership.role IN ('owner', 'admin')) DESC,
                   membership.joined_at ASC,
                   membership.participant_id ASC
           ) AS candidate_rank
      FROM channels_without_effective_owner missing
      JOIN rooms room
        ON room.id = missing.id
      JOIN room_members membership
        ON membership.room_id = room.id
      JOIN workspace_members workspace_membership
        ON workspace_membership.workspace_id = room.workspace_id
       AND workspace_membership.participant_id = membership.participant_id
     WHERE aero_participant_has_effective_workspace_access(
               room.workspace_id,
               membership.participant_id
           )
)
UPDATE room_members membership
   SET role = 'owner'
  FROM ranked_candidates candidate
 WHERE candidate.candidate_rank = 1
   AND membership.room_id = candidate.room_id
   AND membership.participant_id = candidate.participant_id;

-- A channel with no existing effective member cannot be repaired without
-- guessing a new reader for potentially private history.  Fail loudly and let
-- an operator explicitly resolve that room before retrying the migration.
DO $$
DECLARE
    invalid_room uuid;
BEGIN
    SELECT room.id
      INTO invalid_room
      FROM rooms room
     WHERE room.kind = 'channel'
       AND NOT EXISTS (
           SELECT 1
             FROM room_members owner_membership
            WHERE owner_membership.room_id = room.id
              AND owner_membership.role = 'owner'
              AND aero_participant_has_effective_workspace_access(
                      room.workspace_id,
                      owner_membership.participant_id
                  )
       )
     ORDER BY room.id
     LIMIT 1;

    IF invalid_room IS NOT NULL THEN
        RAISE EXCEPTION
            'channel % has no existing effective member eligible for ownership; explicitly repair membership before retrying migration 0194',
            invalid_room
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_effective_owner_required';
    END IF;
END
$$;

-- Channel creation must acquire the workspace fence before INSERT's foreign-key
-- check takes KEY SHARE on that same workspace row.  Otherwise two concurrent
-- creators can each retain KEY SHARE and then deadlock when their owner-edge
-- triggers try to upgrade to FOR UPDATE.  The room kind/workspace pair defines
-- aggregate identity and has no supported mutation path; freezing it also
-- prevents a direct UPDATE from turning an unrelated room into an ownerless
-- channel or moving a channel away from its owners' tenant.
--
-- Migration 0198 adds the deferred "a channel must be born with an owner"
-- constraint once the two-statement creation contract is established.  This
-- trigger remains responsible for taking the workspace fence before INSERT's
-- foreign-key lock; the later constraint verifies the owner edge at commit.
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
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS room_channel_aggregate_identity_guard ON rooms;
CREATE TRIGGER room_channel_aggregate_identity_guard
    BEFORE INSERT OR UPDATE OF workspace_id, kind
    ON rooms
    FOR EACH ROW
    EXECUTE FUNCTION room_channel_aggregate_identity_guard();

-- Protect DELETE and role demotion at the room-membership boundary.  Every
-- owner mutation serializes workspace -> room, matching application governance.
-- During ON DELETE CASCADE from rooms the parent row is already absent, so the
-- trigger deliberately returns without blocking room deletion.
CREATE OR REPLACE FUNCTION channel_effective_owner_membership_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    governed_room uuid;
    governed_participant uuid;
    governed_workspace uuid;
    governed_kind text;
    locked_workspace uuid;
    locked_kind text;
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.room_id <> OLD.room_id
           OR NEW.participant_id <> OLD.participant_id
       ) THEN
        RAISE EXCEPTION 'room membership identity is immutable; delete and insert instead'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'room_members_identity_immutable';
    END IF;

    governed_room := CASE WHEN TG_OP = 'INSERT' THEN NEW.room_id ELSE OLD.room_id END;
    governed_participant :=
        CASE WHEN TG_OP = 'INSERT' THEN NEW.participant_id ELSE OLD.participant_id END;

    -- First classify the room without taking governance locks.  DM/GroupDM
    -- membership creation already holds the workspace through the canonical
    -- access gate; trying to upgrade that SHARE lock here can deadlock two
    -- concurrent creators.  Those room kinds have no channel-owner invariant.
    SELECT room.workspace_id, room.kind
      INTO governed_workspace, governed_kind
      FROM rooms room
     WHERE room.id = governed_room;
    IF NOT FOUND THEN
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NEW;
    END IF;
    IF governed_kind <> 'channel' THEN
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NEW;
    END IF;

    -- Ordinary channel-member edges cannot change effective ownership.  Owner
    -- insertion/promotion, deletion, and demotion continue through the
    -- serialized workspace -> room path below.
    IF (TG_OP = 'INSERT' AND NEW.role <> 'owner')
       OR (TG_OP = 'DELETE' AND OLD.role <> 'owner')
       OR (
           TG_OP = 'UPDATE'
           AND NOT (
               (OLD.role <> 'owner' AND NEW.role = 'owner')
               OR (OLD.role = 'owner' AND NEW.role <> 'owner')
           )
       ) THEN
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NEW;
    END IF;

    PERFORM 1
      FROM workspaces workspace
     WHERE workspace.id = governed_workspace
       FOR UPDATE;

    SELECT room.workspace_id, room.kind
      INTO locked_workspace, locked_kind
      FROM rooms room
     WHERE room.id = governed_room
       FOR UPDATE;
    IF NOT FOUND OR locked_kind <> 'channel' THEN
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NEW;
    END IF;
    IF locked_workspace <> governed_workspace THEN
        RAISE EXCEPTION
            'room workspace changed while channel membership was being updated; retry the transaction'
            USING ERRCODE = '40001';
    END IF;

    IF (
        (TG_OP = 'INSERT' AND NEW.role = 'owner')
        OR (TG_OP = 'UPDATE' AND OLD.role <> 'owner' AND NEW.role = 'owner')
    ) AND NOT aero_participant_has_effective_workspace_access(
        governed_workspace,
        governed_participant
    ) THEN
        RAISE EXCEPTION
            'channel owners must have current effective workspace access'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_effective_owner_required';
    END IF;

    IF (
        (TG_OP = 'DELETE' AND OLD.role = 'owner')
        OR (TG_OP = 'UPDATE' AND OLD.role = 'owner' AND NEW.role <> 'owner')
    ) AND NOT aero_channel_has_other_effective_owner(
        governed_room,
        governed_participant
    ) THEN
        RAISE EXCEPTION
            'channel must retain at least one effective owner; transfer ownership first'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_effective_owner_required';
    END IF;

    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS channel_effective_owner_membership_guard
    ON room_members;
CREATE TRIGGER channel_effective_owner_membership_guard
    BEFORE INSERT OR DELETE OR UPDATE OF role, room_id, participant_id
    ON room_members
    FOR EACH ROW
    EXECUTE FUNCTION channel_effective_owner_membership_guard();

-- Removing a workspace-membership row directly also makes all of that
-- participant's channel-owner rows ineffective, even if a caller forgets to
-- delete room_members first.  Lock the owned channel rows in UUID order and
-- reject any transition that would strand one.
CREATE OR REPLACE FUNCTION workspace_member_effective_channel_owner_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    stranded_room uuid;
BEGIN
    IF TG_OP = 'UPDATE'
       AND NEW.workspace_id = OLD.workspace_id
       AND NEW.participant_id = OLD.participant_id THEN
        RETURN NEW;
    END IF;

    -- A workspace cascade is deleting the whole tenant, not orphaning a live
    -- channel.  The missing parent also avoids taking locks in reverse order.
    PERFORM 1
      FROM workspaces workspace
     WHERE workspace.id = OLD.workspace_id
       FOR UPDATE;
    IF NOT FOUND THEN
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NEW;
    END IF;

    PERFORM room.id
      FROM rooms room
      JOIN room_members owner_membership
        ON owner_membership.room_id = room.id
       AND owner_membership.participant_id = OLD.participant_id
       AND owner_membership.role = 'owner'
     WHERE room.workspace_id = OLD.workspace_id
       AND room.kind = 'channel'
     ORDER BY room.id
       FOR UPDATE OF room;

    SELECT room.id
      INTO stranded_room
      FROM rooms room
      JOIN room_members owner_membership
        ON owner_membership.room_id = room.id
       AND owner_membership.participant_id = OLD.participant_id
       AND owner_membership.role = 'owner'
     WHERE room.workspace_id = OLD.workspace_id
       AND room.kind = 'channel'
       AND NOT aero_channel_has_other_effective_owner(
                   room.id,
                   OLD.participant_id
               )
     ORDER BY room.id
     LIMIT 1;

    IF stranded_room IS NOT NULL THEN
        RAISE EXCEPTION
            'workspace membership removal would leave channel % without an effective owner; transfer ownership first',
            stranded_room
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_effective_owner_required';
    END IF;

    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS workspace_member_effective_channel_owner_guard
    ON workspace_members;
CREATE TRIGGER workspace_member_effective_channel_owner_guard
    BEFORE DELETE OR UPDATE OF workspace_id, participant_id
    ON workspace_members
    FOR EACH ROW
    EXECUTE FUNCTION workspace_member_effective_channel_owner_guard();

-- Replace migration 0189's deactivation guard.  Workspace owners remain
-- protected, and a room owner can now be deactivated only when another
-- effective owner will remain in every owned channel.
CREATE OR REPLACE FUNCTION workspace_deactivation_reject_owner()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    stranded_room uuid;
BEGIN
    PERFORM 1
      FROM workspaces workspace
     WHERE workspace.id = NEW.workspace_id
       FOR UPDATE;

    IF EXISTS (
        SELECT 1
          FROM workspace_members membership
         WHERE membership.workspace_id = NEW.workspace_id
           AND membership.participant_id = NEW.participant_id
           AND membership.role = 'owner'
    ) THEN
        RAISE EXCEPTION
            'workspace owners cannot be deactivated; transfer or demote ownership first'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'workspace_owner_cannot_be_deactivated';
    END IF;

    PERFORM room.id
      FROM rooms room
      JOIN room_members owner_membership
        ON owner_membership.room_id = room.id
       AND owner_membership.participant_id = NEW.participant_id
       AND owner_membership.role = 'owner'
     WHERE room.workspace_id = NEW.workspace_id
       AND room.kind = 'channel'
     ORDER BY room.id
       FOR UPDATE OF room;

    SELECT room.id
      INTO stranded_room
      FROM rooms room
      JOIN room_members owner_membership
        ON owner_membership.room_id = room.id
       AND owner_membership.participant_id = NEW.participant_id
       AND owner_membership.role = 'owner'
     WHERE room.workspace_id = NEW.workspace_id
       AND room.kind = 'channel'
       AND NOT aero_channel_has_other_effective_owner(
                   room.id,
                   NEW.participant_id
               )
     ORDER BY room.id
     LIMIT 1;

    IF stranded_room IS NOT NULL THEN
        RAISE EXCEPTION
            'deactivation would leave channel % without an effective owner; transfer ownership first',
            stranded_room
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_effective_owner_required';
    END IF;

    RETURN NEW;
END
$$;

-- Enabling mandatory 2FA can itself make every current owner ineffective.
-- Validate the proposed true policy under the workspace -> room lock order.
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

DROP TRIGGER IF EXISTS workspace_require_2fa_effective_channel_owner_guard
    ON workspaces;
CREATE TRIGGER workspace_require_2fa_effective_channel_owner_guard
    BEFORE UPDATE OF require_2fa
    ON workspaces
    FOR EACH ROW
    EXECUTE FUNCTION workspace_require_2fa_effective_channel_owner_guard();

-- Final explicit assertion (stronger than a raw owner count): every channel now
-- has at least one owner satisfying the canonical effective-access predicate.
DO $$
DECLARE
    invalid_room uuid;
BEGIN
    SELECT room.id
      INTO invalid_room
      FROM rooms room
     WHERE room.kind = 'channel'
       AND NOT EXISTS (
           SELECT 1
             FROM room_members owner_membership
            WHERE owner_membership.room_id = room.id
              AND owner_membership.role = 'owner'
              AND aero_participant_has_effective_workspace_access(
                      room.workspace_id,
                      owner_membership.participant_id
                  )
       )
     ORDER BY room.id
     LIMIT 1;

    IF invalid_room IS NOT NULL THEN
        RAISE EXCEPTION
            'channel % failed the effective-owner invariant after migration 0194',
            invalid_room
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_effective_owner_required';
    END IF;
END
$$;
