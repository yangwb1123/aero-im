-- A workspace owner must be able to exercise workspace governance, not merely
-- retain a raw `role = 'owner'` edge. Migration 0200 made raw ownership a
-- commit-time invariant, but an older tombstone, a mandatory-2FA transition,
-- or a direct identity/TOTP write could still leave a tenant with no effective
-- owner.
--
-- Effective here matches workspace administration:
--   * a non-guest owner membership;
--   * an active participant;
--   * no workspace deactivation; and
--   * when mandatory 2FA is enabled, activated TOTP for humans (service
--     identities retain the existing 0195 exemption).
--
-- Legacy repair never invents a tenant reader. It may promote only an existing
-- effective ordinary member, preferring the creator and then the oldest
-- membership. An unrepairable non-default tenant fails the migration loudly.

CREATE OR REPLACE FUNCTION aero_workspace_has_effective_owner(
    requested_workspace uuid
) RETURNS boolean
LANGUAGE sql
VOLATILE
AS $$
    SELECT EXISTS (
        SELECT 1
          FROM workspaces workspace
          JOIN workspace_members membership
            ON membership.workspace_id = workspace.id
           AND membership.role = 'owner'
           AND NOT membership.is_guest
          JOIN participants participant
            ON participant.id = membership.participant_id
           AND participant.deleted_at IS NULL
          LEFT JOIN workspace_deactivations deactivated
            ON deactivated.workspace_id = workspace.id
           AND deactivated.participant_id = membership.participant_id
          LEFT JOIN totp_secrets totp
            ON totp.participant_id = membership.participant_id
         WHERE workspace.id = requested_workspace
           AND deactivated.participant_id IS NULL
           AND (
               participant.kind <> 'human'
               OR NOT workspace.require_2fa
               OR COALESCE(totp.activated, false)
           )
    )
$$;

CREATE OR REPLACE FUNCTION aero_workspace_has_effective_member(
    requested_workspace uuid
) RETURNS boolean
LANGUAGE sql
VOLATILE
AS $$
    SELECT EXISTS (
        SELECT 1
          FROM workspaces workspace
          JOIN workspace_members membership
            ON membership.workspace_id = workspace.id
          JOIN participants participant
            ON participant.id = membership.participant_id
           AND participant.deleted_at IS NULL
          LEFT JOIN workspace_deactivations deactivated
            ON deactivated.workspace_id = workspace.id
           AND deactivated.participant_id = membership.participant_id
          LEFT JOIN totp_secrets totp
            ON totp.participant_id = membership.participant_id
         WHERE workspace.id = requested_workspace
           AND deactivated.participant_id IS NULL
           AND (
               participant.kind <> 'human'
               OR NOT workspace.require_2fa
               OR COALESCE(totp.activated, false)
           )
    )
$$;

-- Repair only from a membership that already has effective access. The
-- historical creator wins when eligible; otherwise use joined_at + UUID for a
-- deterministic result.
WITH ownerless AS (
    SELECT workspace.id, workspace.created_by
      FROM workspaces workspace
     WHERE NOT aero_workspace_has_effective_owner(workspace.id)
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
      JOIN workspaces workspace
        ON workspace.id = ownerless.id
      JOIN workspace_members membership
        ON membership.workspace_id = ownerless.id
       AND membership.role <> 'guest'
       AND NOT membership.is_guest
      JOIN participants participant
        ON participant.id = membership.participant_id
       AND participant.deleted_at IS NULL
      LEFT JOIN workspace_deactivations deactivated
        ON deactivated.workspace_id = ownerless.id
       AND deactivated.participant_id = membership.participant_id
      LEFT JOIN totp_secrets totp
        ON totp.participant_id = membership.participant_id
     WHERE deactivated.participant_id IS NULL
       AND (
           participant.kind <> 'human'
           OR NOT workspace.require_2fa
           OR COALESCE(totp.activated, false)
       )
)
UPDATE workspace_members membership
   SET role = 'owner',
       is_guest = false
  FROM ranked_candidates candidate
 WHERE candidate.candidate_rank = 1
   AND membership.workspace_id = candidate.workspace_id
   AND membership.participant_id = candidate.participant_id;

-- Do not delete legacy identity edges: workspace_members participates in
-- governance/organisation foreign keys. Once a successor exists, downgrade
-- only the ineffective raw owner role so ordinary role checks cannot mistake
-- a tombstone for a tenant governor.
UPDATE workspace_members membership
   SET role = CASE WHEN membership.is_guest THEN 'guest' ELSE 'member' END
 WHERE membership.role = 'owner'
   AND NOT EXISTS (
       SELECT 1
         FROM workspaces workspace
         JOIN participants participant
           ON participant.id = membership.participant_id
          AND participant.deleted_at IS NULL
         LEFT JOIN workspace_deactivations deactivated
           ON deactivated.workspace_id = membership.workspace_id
          AND deactivated.participant_id = membership.participant_id
         LEFT JOIN totp_secrets totp
           ON totp.participant_id = membership.participant_id
        WHERE workspace.id = membership.workspace_id
          AND NOT membership.is_guest
          AND deactivated.participant_id IS NULL
          AND (
              participant.kind <> 'human'
              OR NOT workspace.require_2fa
              OR COALESCE(totp.activated, false)
          )
   );

-- A default workspace with no effective member is intentionally dormant. Any
-- other ownerless workspace (and a member-bearing default workspace) needs an
-- operator to repair its existing membership before retrying the migration.
DO $$
DECLARE
    invalid_workspace uuid;
BEGIN
    SELECT workspace.id
      INTO invalid_workspace
      FROM workspaces workspace
     WHERE NOT aero_workspace_has_effective_owner(workspace.id)
       AND (
           workspace.id <> '00000000-0000-0000-0000-000000000000'::uuid
           OR aero_workspace_has_effective_member(workspace.id)
       )
     ORDER BY workspace.id
     LIMIT 1;

    IF invalid_workspace IS NOT NULL THEN
        RAISE EXCEPTION
            'workspace % has no existing effective member eligible for ownership; explicitly repair membership before retrying migration 0227',
            invalid_workspace
            USING ERRCODE = '23514',
                  CONSTRAINT = 'workspace_owner_required';
    END IF;
END
$$;

-- Owner roles may be staged before TOTP enrollment while another effective
-- owner remains, but an irreversible tombstone or a deactivated/guest identity
-- can never be promoted into raw ownership.
CREATE OR REPLACE FUNCTION workspace_owner_reject_deactivated()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    participant_active boolean;
BEGIN
    IF NEW.role <> 'owner' THEN
        RETURN NEW;
    END IF;

    PERFORM 1
      FROM workspaces
     WHERE id = NEW.workspace_id
       FOR UPDATE;

    SELECT deleted_at IS NULL
      INTO participant_active
      FROM participants
     WHERE id = NEW.participant_id
       FOR SHARE;

    IF NOT COALESCE(participant_active, false)
       OR NEW.is_guest
       OR EXISTS (
           SELECT 1
             FROM workspace_deactivations deactivated
            WHERE deactivated.workspace_id = NEW.workspace_id
              AND deactivated.participant_id = NEW.participant_id
       ) THEN
        RAISE EXCEPTION
            'workspace owners must be active non-guest members'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'workspace_owner_required';
    END IF;
    RETURN NEW;
END
$$;

-- Dormant tombstone/deactivation rows in the nil workspace must not prevent its
-- next effective ordinary member from becoming the bootstrap owner.
CREATE OR REPLACE FUNCTION default_workspace_first_owner_bootstrap()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    workspace_requires_2fa boolean;
    participant_kind text;
    participant_active boolean;
    participant_has_totp boolean;
BEGIN
    IF NEW.workspace_id <> '00000000-0000-0000-0000-000000000000'::uuid
       OR NEW.role = 'owner'
       OR NEW.role = 'guest'
       OR NEW.is_guest THEN
        RETURN NEW;
    END IF;

    SELECT require_2fa
      INTO workspace_requires_2fa
      FROM workspaces
     WHERE id = NEW.workspace_id
       FOR UPDATE;
    IF NOT FOUND THEN
        RETURN NEW;
    END IF;

    SELECT kind, deleted_at IS NULL
      INTO participant_kind, participant_active
      FROM participants
     WHERE id = NEW.participant_id
       FOR SHARE;
    IF NOT COALESCE(participant_active, false) THEN
        RETURN NEW;
    END IF;

    participant_has_totp := EXISTS (
        SELECT 1
          FROM totp_secrets
         WHERE participant_id = NEW.participant_id
           AND activated
    );
    IF workspace_requires_2fa
       AND participant_kind = 'human'
       AND NOT participant_has_totp THEN
        RETURN NEW;
    END IF;

    IF NOT aero_workspace_has_effective_member(NEW.workspace_id) THEN
        NEW.role := 'owner';
    END IF;
    RETURN NEW;
END
$$;

-- Replace migration 0200's raw-owner count with the effective predicate. The
-- trigger remains deferred so an explicit successor promotion and predecessor
-- demotion can share one transaction.
CREATE OR REPLACE FUNCTION workspace_owner_commit_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    governed_workspace uuid;
BEGIN
    IF TG_TABLE_NAME = 'workspaces' THEN
        governed_workspace := NEW.id;
    ELSIF TG_OP = 'DELETE' THEN
        governed_workspace := OLD.workspace_id;
    ELSE
        governed_workspace := NEW.workspace_id;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM workspaces workspace
         WHERE workspace.id = governed_workspace
    ) THEN
        RETURN NULL;
    END IF;

    IF aero_workspace_has_effective_owner(governed_workspace) THEN
        RETURN NULL;
    END IF;

    IF governed_workspace =
           '00000000-0000-0000-0000-000000000000'::uuid
       AND NOT aero_workspace_has_effective_member(governed_workspace) THEN
        RETURN NULL;
    END IF;

    RAISE EXCEPTION
        'workspace % must retain at least one effective non-guest owner before commit',
        governed_workspace
        USING ERRCODE = '23514',
              CONSTRAINT = 'workspace_owner_required';
END
$$;

-- Match ParticipantRepo: a live raw workspace owner must first transfer and
-- demote/remove its role before account tombstone or hard deletion. A legacy
-- participant that was already tombstoned may be hard-deleted once 0227 has
-- repaired the workspace.
CREATE OR REPLACE FUNCTION participant_workspace_raw_owner_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    loses_active_identity boolean;
    owned_workspace uuid;
BEGIN
    loses_active_identity :=
        CASE
            WHEN TG_OP = 'DELETE' THEN OLD.deleted_at IS NULL
            ELSE OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL
        END;
    IF NOT loses_active_identity THEN
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NEW;
    END IF;

    SELECT membership.workspace_id
      INTO owned_workspace
      FROM workspace_members membership
     WHERE membership.participant_id = OLD.id
       AND membership.role = 'owner'
       AND NOT membership.is_guest
     ORDER BY membership.workspace_id
     LIMIT 1;
    IF owned_workspace IS NOT NULL THEN
        RAISE EXCEPTION
            'workspace owner must transfer and demote ownership before account deletion'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'workspace_owner_required';
    END IF;

    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS participant_workspace_raw_owner_guard
    ON participants;
CREATE TRIGGER participant_workspace_raw_owner_guard
    BEFORE DELETE OR UPDATE OF deleted_at
    ON participants
    FOR EACH ROW
    EXECUTE FUNCTION participant_workspace_raw_owner_guard();

-- Identity-kind and TOTP updates can affect several owner rows in one SQL
-- statement. Existing 0198 row guards protect channels; this AFTER STATEMENT
-- final scan closes the workspace-level multi-row snapshot gap.
CREATE OR REPLACE FUNCTION workspace_effective_owner_statement_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    invalid_workspace uuid;
BEGIN
    SELECT workspace.id
      INTO invalid_workspace
      FROM workspaces workspace
     WHERE NOT aero_workspace_has_effective_owner(workspace.id)
       AND (
           workspace.id <> '00000000-0000-0000-0000-000000000000'::uuid
           OR aero_workspace_has_effective_member(workspace.id)
       )
     ORDER BY workspace.id
     LIMIT 1;

    IF invalid_workspace IS NOT NULL THEN
        RAISE EXCEPTION
            'identity policy change would leave workspace % without an effective owner',
            invalid_workspace
            USING ERRCODE = '23514',
                  CONSTRAINT = 'workspace_owner_required';
    END IF;
    RETURN NULL;
END
$$;

DROP TRIGGER IF EXISTS participant_workspace_effective_owner_statement_guard
    ON participants;
CREATE TRIGGER participant_workspace_effective_owner_statement_guard
    AFTER DELETE OR UPDATE OF deleted_at, kind
    ON participants
    FOR EACH STATEMENT
    EXECUTE FUNCTION workspace_effective_owner_statement_guard();

DROP TRIGGER IF EXISTS totp_workspace_effective_owner_statement_guard
    ON totp_secrets;
CREATE TRIGGER totp_workspace_effective_owner_statement_guard
    AFTER DELETE OR UPDATE OF participant_id, activated
    ON totp_secrets
    FOR EACH STATEMENT
    EXECUTE FUNCTION workspace_effective_owner_statement_guard();

-- Enabling mandatory 2FA changes workspace ownership and channel ownership at
-- the same instant. Extend the existing channel guard so one workspace-row
-- lock and one channel lock sequence validates both invariants.
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

    IF NOT EXISTS (
        SELECT 1
          FROM workspace_members owner_membership
          JOIN participants participant
            ON participant.id = owner_membership.participant_id
           AND participant.deleted_at IS NULL
          LEFT JOIN workspace_deactivations deactivated
            ON deactivated.workspace_id = NEW.id
           AND deactivated.participant_id =
               owner_membership.participant_id
          LEFT JOIN totp_secrets totp
            ON totp.participant_id = owner_membership.participant_id
         WHERE owner_membership.workspace_id = NEW.id
           AND owner_membership.role = 'owner'
           AND NOT owner_membership.is_guest
           AND deactivated.participant_id IS NULL
           AND (
               participant.kind <> 'human'
               OR COALESCE(totp.activated, false)
           )
    ) THEN
        RAISE EXCEPTION
            'mandatory 2FA would leave the workspace without an effective owner'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'workspace_owner_required';
    END IF;

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

COMMENT ON FUNCTION aero_workspace_has_effective_owner(uuid) IS
    'True when a workspace has a non-guest owner with current effective administration access';
COMMENT ON FUNCTION workspace_owner_commit_guard() IS
    'Deferred invariant requiring a surviving workspace to retain an effective owner; dormant nil workspace is the sole exception';
COMMENT ON FUNCTION workspace_effective_owner_statement_guard() IS
    'Final-state backstop for multi-row participant and TOTP changes that affect workspace ownership';
COMMENT ON TRIGGER participant_workspace_raw_owner_guard ON participants IS
    'Requires explicit owner transfer/demotion before a live account can be tombstoned or hard-deleted';
