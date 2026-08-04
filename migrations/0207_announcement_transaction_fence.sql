-- Workspace announcement authorization/identity backstop.
--
-- Canonical writes are transaction-owned by AnnouncementRepo: they lock the
-- workspace, recheck effective Owner/Admin status, mutate the announcement, and
-- append an audit row in one commit. These constraints protect direct SQL and
-- future writers from forging a creator in another tenant or rewriting a
-- retained announcement's tenant identity.

-- Canonicalize the permissive 0038-era body shape before validating the
-- production input contract. Whitespace-only historical banners have no useful
-- visible content and are removed; other rows preserve their first 2000
-- characters after trimming.
DELETE FROM workspace_announcements
 WHERE btrim(body) = '';

UPDATE workspace_announcements
   SET body = left(btrim(body), 2000)
 WHERE body IS DISTINCT FROM left(btrim(body), 2000);

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'workspace_announcements_body_canonical_chk'
           AND conrelid = 'workspace_announcements'::regclass
    ) THEN
        ALTER TABLE workspace_announcements
            ADD CONSTRAINT workspace_announcements_body_canonical_chk
            CHECK (
                body = btrim(body)
                AND char_length(body) BETWEEN 1 AND 2000
            );
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION workspace_announcement_scope_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    creator_role text;
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.id IS DISTINCT FROM OLD.id
           OR NEW.workspace_id IS DISTINCT FROM OLD.workspace_id
           OR NEW.created_by IS DISTINCT FROM OLD.created_by
           OR NEW.created_at IS DISTINCT FROM OLD.created_at
       ) THEN
        RAISE EXCEPTION 'workspace announcement tenant identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'workspace_announcements_identity_immutable_chk';
    END IF;

    IF TG_OP = 'INSERT' THEN
        -- This lock-taking helper closes raw-SQL membership/deactivation/2FA
        -- races. AnnouncementRepo already owns the stronger workspace
        -- Owner/Admin lock; re-entrant locks are harmless there.
        IF NOT aero_effective_workspace_access(NEW.workspace_id, NEW.created_by) THEN
            RAISE EXCEPTION
                'workspace announcement creator lacks effective workspace access'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'workspace_announcements_creator_scope_chk';
        END IF;

        SELECT role
          INTO creator_role
          FROM workspace_members
         WHERE workspace_id = NEW.workspace_id
           AND participant_id = NEW.created_by;
        IF creator_role IS NULL OR creator_role NOT IN ('owner', 'admin') THEN
            RAISE EXCEPTION
                'workspace announcement creator must be a workspace administrator'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'workspace_announcements_creator_scope_chk';
        END IF;
    END IF;

    -- A retained announcement naturally becomes inactive after this instant;
    -- only creation or an explicit expiry change needs boundary validation.
    IF (
           TG_OP = 'INSERT'
           OR NEW.expires_at IS DISTINCT FROM OLD.expires_at
       )
       AND NEW.expires_at IS NOT NULL
       AND NEW.expires_at <= CURRENT_TIMESTAMP THEN
        RAISE EXCEPTION 'workspace announcement expiry must be in the future'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'workspace_announcements_expiry_future_chk';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS workspace_announcement_scope_guard
    ON workspace_announcements;
CREATE TRIGGER workspace_announcement_scope_guard
    BEFORE INSERT OR UPDATE
    ON workspace_announcements
    FOR EACH ROW
    EXECUTE FUNCTION workspace_announcement_scope_guard();

COMMENT ON TRIGGER workspace_announcement_scope_guard
    ON workspace_announcements IS
    'Backstops effective admin creator scope, immutable tenant identity, and future expiry; later creator membership removal preserves historical rows.';
