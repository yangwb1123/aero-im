-- Normalize legacy capacity/role values before making their documented
-- invariants database-enforced. Raising max_uses to the already-consumed count
-- preserves exhausted state without discarding redemption history.
UPDATE invitations
   SET role = 'guest'
 WHERE role NOT IN ('owner', 'admin', 'member', 'guest');

UPDATE invitations
   SET max_uses = NULL
 WHERE max_uses IS NOT NULL
   AND max_uses < 1;

UPDATE invitations
   SET use_count = GREATEST(use_count, 0),
       max_uses = CASE
           WHEN max_uses IS NOT NULL AND use_count > max_uses
               THEN GREATEST(use_count, 0)
           ELSE max_uses
       END
 WHERE use_count < 0
    OR (max_uses IS NOT NULL AND use_count > max_uses);

ALTER TABLE invitations
    DROP CONSTRAINT IF EXISTS invitations_role_chk;
ALTER TABLE invitations
    ADD CONSTRAINT invitations_role_chk
    CHECK (role IN ('owner', 'admin', 'member', 'guest'));

ALTER TABLE invitations
    DROP CONSTRAINT IF EXISTS invitations_capacity_chk;
ALTER TABLE invitations
    ADD CONSTRAINT invitations_capacity_chk
    CHECK (
        use_count >= 0
        AND (max_uses IS NULL OR (max_uses >= 1 AND use_count <= max_uses))
    );

CREATE OR REPLACE FUNCTION invitation_governance_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    creator_role text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.created_by IS NULL
           OR length(NEW.token_hash) <> 64
           OR NEW.token_hash !~ '^[0-9a-f]{64}$' THEN
            RAISE EXCEPTION
                'new invitation requires an administrator and a SHA-256 token hash'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'invitations_creation_shape_chk';
        END IF;

        SELECT membership.role
          INTO creator_role
          FROM workspace_members membership
         WHERE membership.workspace_id = NEW.workspace_id
           AND membership.participant_id = NEW.created_by
           AND aero_participant_has_effective_workspace_access(
                   NEW.workspace_id,
                   NEW.created_by
               );
        IF creator_role NOT IN ('owner', 'admin')
           OR (creator_role = 'admin' AND NEW.role = 'owner') THEN
            RAISE EXCEPTION
                'invitation creator cannot grant this role in the workspace'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'invitations_creator_scope_chk';
        END IF;
    ELSE
        IF NEW.workspace_id IS DISTINCT FROM OLD.workspace_id
           OR NEW.created_by IS DISTINCT FROM OLD.created_by
           OR NEW.token_hash IS DISTINCT FROM OLD.token_hash
           OR NEW.role IS DISTINCT FROM OLD.role THEN
            RAISE EXCEPTION 'invitation tenant identity and grant are immutable'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'invitations_identity_immutable_chk';
        END IF;
        IF OLD.revoked_at IS NOT NULL
           AND NEW.revoked_at IS DISTINCT FROM OLD.revoked_at THEN
            RAISE EXCEPTION 'invitation revocation is terminal'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'invitations_revocation_terminal_chk';
        END IF;
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS invitation_governance_guard
    ON invitations;
CREATE TRIGGER invitation_governance_guard
    BEFORE INSERT OR UPDATE
    ON invitations
    FOR EACH ROW
    EXECUTE FUNCTION invitation_governance_guard();

COMMENT ON TRIGGER invitation_governance_guard ON invitations IS
    'Backstops raw SQL creator/workspace/grant containment and immutable, terminal invitation identity; repository methods own race-free actor authorization.';
