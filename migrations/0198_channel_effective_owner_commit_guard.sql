-- Close the remaining database-boundary gaps in the effective channel-owner
-- invariant introduced by migration 0194.
--
-- There are two distinct problems:
--
-- 1. A channel row and its first owner edge are necessarily separate writes.
--    The supported repositories put both writes in one transaction, but a raw
--    autocommit INSERT could previously publish an ownerless channel.
-- 2. Effective access also depends on participants.deleted_at/kind and, for a
--    human in a mandatory-2FA workspace, totp_secrets.activated.  Application
--    repositories fenced those transitions, but direct SQL did not.
--
-- Participant/TOTP row triggers run after PostgreSQL has locked their target
-- row.  Taking workspace locks only from such a row trigger would invert the
-- canonical workspace -> room -> identity lock order used by access checks.
-- The statement trigger below therefore acquires the governance fence before
-- PostgreSQL visits any participant/TOTP target row.  These writes are rare;
-- serializing them across tenants is an intentional safety trade-off.

CREATE OR REPLACE FUNCTION aero_lock_all_channel_governance()
RETURNS void
LANGUAGE plpgsql
VOLATILE
AS $$
BEGIN
    -- Prevent a concurrent INSERT from creating a previously invisible
    -- workspace after the row scan.  Workspace DML takes ROW EXCLUSIVE, which
    -- conflicts with SHARE before either side can acquire aggregate row locks.
    LOCK TABLE workspaces IN SHARE MODE;

    -- Every workspace is included, not only one that already has a channel:
    -- a concurrent first-channel INSERT locks its workspace row and must
    -- serialize with an account/TOTP downgrade.
    PERFORM workspace.id
      FROM workspaces workspace
     ORDER BY workspace.id
       FOR UPDATE;

    PERFORM room.id
      FROM rooms room
     WHERE room.kind = 'channel'
     ORDER BY room.workspace_id, room.id
       FOR UPDATE;
END
$$;

CREATE OR REPLACE FUNCTION channel_effective_identity_statement_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM aero_lock_all_channel_governance();
    RETURN NULL;
END
$$;

DROP TRIGGER IF EXISTS participant_channel_effective_identity_statement_fence
    ON participants;
CREATE TRIGGER participant_channel_effective_identity_statement_fence
    BEFORE DELETE OR UPDATE OF deleted_at, kind
    ON participants
    FOR EACH STATEMENT
    EXECUTE FUNCTION channel_effective_identity_statement_fence();

DROP TRIGGER IF EXISTS totp_channel_effective_identity_statement_fence
    ON totp_secrets;
CREATE TRIGGER totp_channel_effective_identity_statement_fence
    BEFORE DELETE OR UPDATE OF participant_id, activated
    ON totp_secrets
    FOR EACH STATEMENT
    EXECUTE FUNCTION channel_effective_identity_statement_fence();

CREATE OR REPLACE FUNCTION participant_effective_channel_owner_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    governed_participant uuid;
    loses_all_access boolean := false;
    loses_human_2fa_bypass boolean := false;
    has_active_totp boolean := false;
    stranded_room uuid;
BEGIN
    governed_participant :=
        CASE WHEN TG_OP = 'DELETE' THEN OLD.id ELSE NEW.id END;

    IF TG_OP = 'DELETE' THEN
        -- A participant already tombstoned was not effective before this hard
        -- delete, so removing the retained identity cannot newly strand a room.
        loses_all_access := OLD.deleted_at IS NULL;
    ELSE
        loses_all_access :=
            OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL;

        IF NOT loses_all_access
           AND OLD.deleted_at IS NULL
           AND NEW.deleted_at IS NULL
           AND OLD.kind <> 'human'
           AND NEW.kind = 'human' THEN
            SELECT COALESCE(totp.activated, false)
              INTO has_active_totp
              FROM totp_secrets totp
             WHERE totp.participant_id = governed_participant;
            loses_human_2fa_bypass := NOT COALESCE(has_active_totp, false);
        END IF;
    END IF;

    IF NOT loses_all_access AND NOT loses_human_2fa_bypass THEN
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NEW;
    END IF;

    SELECT room.id
      INTO stranded_room
      FROM rooms room
      JOIN room_members owner_membership
        ON owner_membership.room_id = room.id
       AND owner_membership.participant_id = governed_participant
       AND owner_membership.role = 'owner'
      JOIN workspaces workspace
        ON workspace.id = room.workspace_id
     WHERE room.kind = 'channel'
       AND (loses_all_access OR workspace.require_2fa)
       AND NOT aero_channel_has_other_effective_owner(
                   room.id,
                   governed_participant
               )
     ORDER BY room.workspace_id, room.id
     LIMIT 1;

    IF stranded_room IS NOT NULL THEN
        RAISE EXCEPTION
            'participant effective-access downgrade would leave channel % without an effective owner; transfer ownership first',
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

DROP TRIGGER IF EXISTS participant_effective_channel_owner_guard
    ON participants;
CREATE TRIGGER participant_effective_channel_owner_guard
    BEFORE DELETE OR UPDATE OF deleted_at, kind
    ON participants
    FOR EACH ROW
    EXECUTE FUNCTION participant_effective_channel_owner_guard();

CREATE OR REPLACE FUNCTION totp_effective_channel_owner_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    governed_participant uuid;
    participant_is_effective_human boolean;
    stranded_room uuid;
BEGIN
    governed_participant := OLD.participant_id;

    -- Only removing an active enrollment from OLD can reduce access.  Moving an
    -- active row to another participant also removes OLD's enrollment.
    IF NOT OLD.activated
       OR (
           TG_OP = 'UPDATE'
           AND NEW.activated
           AND NEW.participant_id = OLD.participant_id
       ) THEN
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NEW;
    END IF;

    SELECT EXISTS (
        SELECT 1
          FROM participants participant
         WHERE participant.id = governed_participant
           AND participant.deleted_at IS NULL
           AND participant.kind = 'human'
    )
      INTO participant_is_effective_human;

    IF NOT participant_is_effective_human THEN
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NEW;
    END IF;

    SELECT room.id
      INTO stranded_room
      FROM rooms room
      JOIN workspaces workspace
        ON workspace.id = room.workspace_id
       AND workspace.require_2fa
      JOIN room_members owner_membership
        ON owner_membership.room_id = room.id
       AND owner_membership.participant_id = governed_participant
       AND owner_membership.role = 'owner'
     WHERE room.kind = 'channel'
       AND NOT aero_channel_has_other_effective_owner(
                   room.id,
                   governed_participant
               )
     ORDER BY room.workspace_id, room.id
     LIMIT 1;

    IF stranded_room IS NOT NULL THEN
        RAISE EXCEPTION
            'TOTP downgrade would leave channel % without an effective owner; transfer ownership first',
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

DROP TRIGGER IF EXISTS totp_effective_channel_owner_guard
    ON totp_secrets;
CREATE TRIGGER totp_effective_channel_owner_guard
    BEFORE DELETE OR UPDATE OF participant_id, activated
    ON totp_secrets
    FOR EACH ROW
    EXECUTE FUNCTION totp_effective_channel_owner_guard();

-- The first owner edge may be inserted after the room row, but it must exist by
-- transaction commit.  A deferred constraint trigger closes the autocommit
-- birth gap without rejecting the supported two-statement transaction.
CREATE OR REPLACE FUNCTION channel_effective_owner_commit_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- The room may have been deleted later in this transaction, including by a
    -- workspace cascade.  In that case there is no live aggregate to govern.
    IF NOT EXISTS (
        SELECT 1
          FROM rooms room
         WHERE room.id = NEW.id
           AND room.kind = 'channel'
    ) THEN
        RETURN NULL;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM rooms room
          JOIN room_members owner_membership
            ON owner_membership.room_id = room.id
           AND owner_membership.role = 'owner'
         WHERE room.id = NEW.id
           AND room.kind = 'channel'
           AND aero_participant_has_effective_workspace_access(
                   room.workspace_id,
                   owner_membership.participant_id
               )
    ) THEN
        RAISE EXCEPTION
            'channel % must have an effective owner before commit',
            NEW.id
            USING ERRCODE = '23514',
                  CONSTRAINT = 'channel_effective_owner_required';
    END IF;

    RETURN NULL;
END
$$;

DROP TRIGGER IF EXISTS channel_effective_owner_commit_guard
    ON rooms;
CREATE CONSTRAINT TRIGGER channel_effective_owner_commit_guard
    AFTER INSERT
    ON rooms
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (NEW.kind = 'channel')
    EXECUTE FUNCTION channel_effective_owner_commit_guard();

COMMENT ON FUNCTION aero_lock_all_channel_governance() IS
    'Low-frequency effective-identity fence: SHARE-lock the workspace table, then lock every workspace and channel in UUID order before participant/TOTP downgrade writes.';

COMMENT ON TRIGGER channel_effective_owner_commit_guard ON rooms IS
    'Deferred birth invariant: every channel that survives the transaction must have at least one effective owner at commit.';
