-- Workspace ownership is a database invariant, not only an application RBAC
-- convention.  Every live workspace must retain at least one non-guest owner.
-- The reserved nil workspace may remain empty on a fresh deployment, but once
-- it has a member it follows the same ownership rule.
--
-- This migration also closes lock-order inversions in the existing row
-- triggers.  A membership/deactivation UPDATE or DELETE used to lock its child
-- row before the row trigger requested workspace/channel governance locks,
-- while canonical application transactions use workspace -> channel -> child.
-- BEFORE STATEMENT fences now establish the aggregate order first.
-- Multi-statement governance writers that explicitly lock an authorization
-- edge before their eventual mutation call the same global fence at transaction
-- entry; a statement trigger cannot retroactively precede an earlier query.

-- Repair legacy ownerless workspaces without inventing a new tenant reader.
-- Prefer the creator when they are already an ordinary member, then the oldest
-- retained ordinary member, with UUID as a deterministic final tie-break.
WITH ownerless AS (
    SELECT workspace.id, workspace.created_by
      FROM workspaces workspace
     WHERE NOT EXISTS (
               SELECT 1
                 FROM workspace_members owner_membership
                WHERE owner_membership.workspace_id = workspace.id
                  AND owner_membership.role = 'owner'
                  AND NOT owner_membership.is_guest
           )
),
ranked_candidates AS (
    SELECT ownerless.id AS workspace_id,
           membership.participant_id,
           row_number() OVER (
               PARTITION BY ownerless.id
               ORDER BY
                   (membership.participant_id = ownerless.created_by) DESC,
                   membership.joined_at ASC,
                   membership.participant_id ASC
           ) AS candidate_rank
      FROM ownerless
      JOIN workspace_members membership
        ON membership.workspace_id = ownerless.id
       AND NOT membership.is_guest
       AND membership.role <> 'guest'
)
UPDATE workspace_members membership
   SET role = 'owner',
       is_guest = false
  FROM ranked_candidates candidate
 WHERE candidate.candidate_rank = 1
   AND membership.workspace_id = candidate.workspace_id
   AND membership.participant_id = candidate.participant_id;

-- A non-default tenant with no eligible retained member cannot be repaired
-- safely.  Likewise, the default tenant may be ownerless only while empty.
DO $$
DECLARE
    invalid_workspace uuid;
BEGIN
    SELECT workspace.id
      INTO invalid_workspace
      FROM workspaces workspace
     WHERE NOT EXISTS (
               SELECT 1
                 FROM workspace_members owner_membership
                WHERE owner_membership.workspace_id = workspace.id
                  AND owner_membership.role = 'owner'
                  AND NOT owner_membership.is_guest
           )
       AND (
           workspace.id <> '00000000-0000-0000-0000-000000000000'::uuid
           OR EXISTS (
               SELECT 1
                 FROM workspace_members membership
                WHERE membership.workspace_id = workspace.id
           )
       )
     ORDER BY workspace.id
     LIMIT 1;

    IF invalid_workspace IS NOT NULL THEN
        RAISE EXCEPTION
            'workspace % has no retained ordinary member eligible for ownership; explicitly repair membership before retrying migration 0200',
            invalid_workspace
            USING ERRCODE = '23514',
                  CONSTRAINT = 'workspace_owner_required';
    END IF;
END
$$;

-- Membership and deactivation mutations are rare governance writes. Lock all
-- existing aggregate roots in one deterministic order before PostgreSQL visits
-- any target child row. Canonical multi-statement writers call this function
-- before their first explicit row lock; standalone SQL gets the same fence from
-- the statement triggers below. No table SHARE lock is needed: every affected
-- child row already references an existing workspace.
CREATE OR REPLACE FUNCTION aero_lock_all_membership_governance()
RETURNS void
LANGUAGE plpgsql
VOLATILE
AS $$
BEGIN
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

CREATE OR REPLACE FUNCTION membership_governance_statement_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM aero_lock_all_membership_governance();
    RETURN NULL;
END
$$;

DROP TRIGGER IF EXISTS workspace_member_governance_statement_fence
    ON workspace_members;
CREATE TRIGGER workspace_member_governance_statement_fence
    BEFORE DELETE OR UPDATE OF workspace_id, participant_id, role, is_guest
    ON workspace_members
    FOR EACH STATEMENT
    EXECUTE FUNCTION membership_governance_statement_fence();

DROP TRIGGER IF EXISTS room_member_governance_statement_fence
    ON room_members;
CREATE TRIGGER room_member_governance_statement_fence
    BEFORE DELETE OR UPDATE OF room_id, participant_id, role
    ON room_members
    FOR EACH STATEMENT
    EXECUTE FUNCTION membership_governance_statement_fence();

DROP TRIGGER IF EXISTS workspace_deactivation_governance_statement_fence
    ON workspace_deactivations;
CREATE TRIGGER workspace_deactivation_governance_statement_fence
    BEFORE UPDATE OF workspace_id, participant_id
    ON workspace_deactivations
    FOR EACH STATEMENT
    EXECUTE FUNCTION membership_governance_statement_fence();

-- Migration 0189's inverse deactivation guard took the workspace lock even for
-- ordinary member/admin writes.  Those transitions cannot create a deactivated
-- owner, so return before touching the aggregate.  Owner inserts/promotions
-- retain the same serialized validation.
CREATE OR REPLACE FUNCTION workspace_owner_reject_deactivated()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.role <> 'owner' THEN
        RETURN NEW;
    END IF;

    PERFORM 1
      FROM workspaces
     WHERE id = NEW.workspace_id
       FOR UPDATE;

    IF EXISTS (
        SELECT 1
          FROM workspace_deactivations deactivated
         WHERE deactivated.workspace_id = NEW.workspace_id
           AND deactivated.participant_id = NEW.participant_id
    ) THEN
        RAISE EXCEPTION 'deactivated members cannot become workspace owners'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'workspace_owner_cannot_be_deactivated';
    END IF;
    RETURN NEW;
END
$$;

-- The membership primary-key columns are aggregate identity.  Moving a row can
-- otherwise bypass both old/new workspace invariants and produces lock-order
-- ambiguity; callers must delete and insert explicitly.
CREATE OR REPLACE FUNCTION workspace_member_identity_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.workspace_id IS DISTINCT FROM OLD.workspace_id
       OR NEW.participant_id IS DISTINCT FROM OLD.participant_id THEN
        RAISE EXCEPTION
            'workspace membership identity is immutable; delete and insert instead'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'workspace_members_identity_immutable';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS workspace_member_identity_guard
    ON workspace_members;
CREATE TRIGGER workspace_member_identity_guard
    BEFORE UPDATE OF workspace_id, participant_id
    ON workspace_members
    FOR EACH ROW
    EXECUTE FUNCTION workspace_member_identity_guard();

-- The nil workspace is intentionally empty on a pristine database.  Its first
-- ordinary enrollment bootstraps ownership under the workspace row lock so
-- first-party registration/OIDC cannot create a member-bearing ownerless
-- default tenant.  Guest enrollment never receives this promotion.
CREATE OR REPLACE FUNCTION default_workspace_first_owner_bootstrap()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.workspace_id = '00000000-0000-0000-0000-000000000000'::uuid
       AND NEW.role <> 'owner'
       AND NEW.role <> 'guest'
       AND NOT NEW.is_guest THEN
        PERFORM 1
          FROM workspaces workspace
         WHERE workspace.id = NEW.workspace_id
           FOR UPDATE;

        IF NOT EXISTS (
            SELECT 1
              FROM workspace_members membership
             WHERE membership.workspace_id = NEW.workspace_id
        ) THEN
            NEW.role := 'owner';
        END IF;
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS default_workspace_first_owner_bootstrap
    ON workspace_members;
CREATE TRIGGER default_workspace_first_owner_bootstrap
    BEFORE INSERT
    ON workspace_members
    FOR EACH ROW
    EXECUTE FUNCTION default_workspace_first_owner_bootstrap();

-- Deferred validation permits the supported two-statement
-- workspace-row/owner-edge creation transaction, while rejecting a raw
-- autocommit workspace birth and every final-owner demotion/deletion.
CREATE OR REPLACE FUNCTION workspace_owner_commit_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    governed_workspace uuid;
    membership_count bigint;
BEGIN
    IF TG_TABLE_NAME = 'workspaces' THEN
        governed_workspace := NEW.id;
    ELSIF TG_OP = 'DELETE' THEN
        governed_workspace := OLD.workspace_id;
    ELSE
        governed_workspace := NEW.workspace_id;
    END IF;

    -- Workspace deletion (including its membership cascade) removes the
    -- aggregate, so there is no surviving owner invariant to validate.
    IF NOT EXISTS (
        SELECT 1
          FROM workspaces workspace
         WHERE workspace.id = governed_workspace
    ) THEN
        RETURN NULL;
    END IF;

    IF EXISTS (
        SELECT 1
          FROM workspace_members owner_membership
         WHERE owner_membership.workspace_id = governed_workspace
           AND owner_membership.role = 'owner'
           AND NOT owner_membership.is_guest
    ) THEN
        RETURN NULL;
    END IF;

    SELECT count(*)
      INTO membership_count
      FROM workspace_members membership
     WHERE membership.workspace_id = governed_workspace;

    IF governed_workspace =
           '00000000-0000-0000-0000-000000000000'::uuid
       AND membership_count = 0 THEN
        RETURN NULL;
    END IF;

    RAISE EXCEPTION
        'workspace % must retain at least one non-guest owner before commit',
        governed_workspace
        USING ERRCODE = '23514',
              CONSTRAINT = 'workspace_owner_required';
END
$$;

DROP TRIGGER IF EXISTS workspace_birth_owner_commit_guard
    ON workspaces;
CREATE CONSTRAINT TRIGGER workspace_birth_owner_commit_guard
    AFTER INSERT
    ON workspaces
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION workspace_owner_commit_guard();

DROP TRIGGER IF EXISTS workspace_membership_owner_commit_guard
    ON workspace_members;
CREATE CONSTRAINT TRIGGER workspace_membership_owner_commit_guard
    AFTER INSERT OR UPDATE OR DELETE
    ON workspace_members
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION workspace_owner_commit_guard();

COMMENT ON FUNCTION aero_lock_all_membership_governance() IS
    'Transaction-entry governance fence: workspace UUID order, then channel (workspace, UUID) order, before any membership/deactivation child lock.';

COMMENT ON TRIGGER workspace_birth_owner_commit_guard ON workspaces IS
    'Deferred birth invariant: every surviving workspace has a non-guest owner; the empty nil workspace is the sole exception.';

COMMENT ON TRIGGER workspace_membership_owner_commit_guard ON workspace_members IS
    'Deferred survivor invariant: final-owner demotion/deletion cannot commit, while workspace cascades remain valid.';
