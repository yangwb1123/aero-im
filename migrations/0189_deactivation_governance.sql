-- Workspace deactivation is a tenant-governance mutation, not a free-standing
-- deny-list write. Contain every row to a current membership, protect workspace
-- owners at the database boundary, and retain a valid global actor reference.

-- Legacy owner deactivations conflict with the ownership invariant used by role
-- management and SCIM. Reactivate them before installing the hard guard.
DELETE FROM workspace_deactivations deactivated
 USING workspace_members membership
 WHERE membership.workspace_id = deactivated.workspace_id
   AND membership.participant_id = deactivated.participant_id
   AND membership.role = 'owner';

DELETE FROM workspace_deactivations deactivated
 WHERE NOT EXISTS (
       SELECT 1
         FROM workspace_members membership
        WHERE membership.workspace_id = deactivated.workspace_id
          AND membership.participant_id = deactivated.participant_id
 );

UPDATE workspace_deactivations deactivated
   SET deactivated_by = NULL
 WHERE deactivated.deactivated_by IS NOT NULL
   AND NOT EXISTS (
       SELECT 1
         FROM participants actor
        WHERE actor.id = deactivated.deactivated_by
   );

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname IN (
                   'workspace_deactivations_membership_fk',
                   'workspace_deactivations_member_fkey'
               )
           AND conrelid = 'workspace_deactivations'::regclass
    ) THEN
        ALTER TABLE workspace_deactivations
            ADD CONSTRAINT workspace_deactivations_member_fkey
            FOREIGN KEY (workspace_id, participant_id)
            REFERENCES workspace_members (workspace_id, participant_id)
            ON DELETE CASCADE;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'workspace_deactivations_actor_fkey'
           AND conrelid = 'workspace_deactivations'::regclass
    ) THEN
        ALTER TABLE workspace_deactivations
            ADD CONSTRAINT workspace_deactivations_actor_fkey
            FOREIGN KEY (deactivated_by)
            REFERENCES participants (id)
            ON DELETE SET NULL;
    END IF;
END
$$;

-- Migration 0185 introduced the effective-access helper with membership before
-- workspace locking. Governance writes consistently lock workspace before
-- membership, so replace the helper with that global order to avoid a
-- deactivation/access-check deadlock while retaining fresh READ COMMITTED
-- snapshots after waits.
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

CREATE OR REPLACE FUNCTION workspace_deactivation_reject_owner()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- Serialize against role changes, including legacy/direct SQL writers. The
    -- application governance paths take the same workspace-row lock.
    PERFORM 1
      FROM workspaces
     WHERE id = NEW.workspace_id
     FOR UPDATE;

    IF EXISTS (
        SELECT 1
          FROM workspace_members membership
         WHERE membership.workspace_id = NEW.workspace_id
           AND membership.participant_id = NEW.participant_id
           AND membership.role = 'owner'
    ) THEN
        RAISE EXCEPTION 'workspace owners cannot be deactivated; transfer or demote ownership first'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS workspace_deactivations_owner_guard
    ON workspace_deactivations;
CREATE TRIGGER workspace_deactivations_owner_guard
    BEFORE INSERT OR UPDATE OF workspace_id, participant_id
    ON workspace_deactivations
    FOR EACH ROW
    EXECUTE FUNCTION workspace_deactivation_reject_owner();

-- The inverse transition is equally important: a deactivated member cannot be
-- promoted into an owner while the deny row still exists.
CREATE OR REPLACE FUNCTION workspace_owner_reject_deactivated()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- Serialize the inverse transition with deactivation inserts.
    PERFORM 1
      FROM workspaces
     WHERE id = NEW.workspace_id
     FOR UPDATE;

    IF NEW.role = 'owner'
       AND EXISTS (
           SELECT 1
             FROM workspace_deactivations deactivated
            WHERE deactivated.workspace_id = NEW.workspace_id
              AND deactivated.participant_id = NEW.participant_id
       ) THEN
        RAISE EXCEPTION 'deactivated members cannot become workspace owners'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS workspace_members_active_owner_guard
    ON workspace_members;
CREATE TRIGGER workspace_members_active_owner_guard
    BEFORE INSERT OR UPDATE OF role
    ON workspace_members
    FOR EACH ROW
    EXECUTE FUNCTION workspace_owner_reject_deactivated();

CREATE INDEX IF NOT EXISTS workspace_deactivations_actor_idx
    ON workspace_deactivations (deactivated_by)
    WHERE deactivated_by IS NOT NULL;
