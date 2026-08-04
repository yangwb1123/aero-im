-- AI profiles are tenant-scoped content derived from workspace messages.
--
-- Migration 0145 recorded workspace_id but keyed/read profiles by participant
-- alone. A participant who belongs to multiple workspaces could therefore have
-- a profile extracted in workspace A reused while asking in workspace B. Make
-- the tenant boundary structural: one row per (participant, workspace), a
-- non-null workspace FK, and cascade cleanup when either subject or workspace is
-- removed.

-- NULL/orphan rows have no safe tenant attribution. They must not be guessed or
-- retained as a global profile because their text was derived from private
-- workspace messages.
DELETE FROM participant_ai_profiles p
 WHERE p.workspace_id IS NULL
    OR NOT EXISTS (
        SELECT 1 FROM workspaces w WHERE w.id = p.workspace_id
    );

-- A participant tombstoned before this migration will never cross the
-- NULL -> non-NULL transition observed by the trigger installed below. Remove
-- that already-erased subject's derived PII during the shape change itself.
DELETE FROM participant_ai_profiles AS profile
USING participants AS subject
 WHERE profile.participant_id = subject.id
   AND subject.deleted_at IS NOT NULL;

ALTER TABLE participant_ai_profiles
    ALTER COLUMN workspace_id SET NOT NULL;

ALTER TABLE participant_ai_profiles
    DROP CONSTRAINT IF EXISTS participant_ai_profiles_pkey;

ALTER TABLE participant_ai_profiles
    ADD CONSTRAINT participant_ai_profiles_pkey
    PRIMARY KEY (participant_id, workspace_id);

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'participant_ai_profiles_workspace_fkey'
           AND conrelid = 'participant_ai_profiles'::regclass
    ) THEN
        ALTER TABLE participant_ai_profiles
            ADD CONSTRAINT participant_ai_profiles_workspace_fkey
            FOREIGN KEY (workspace_id)
            REFERENCES workspaces(id)
            ON DELETE CASCADE;
    END IF;
END
$$;

-- Rolling-upgrade compatibility ------------------------------------------------
--
-- A previous binary reads/upserts by participant_id alone. Once multiple
-- workspace rows are legal, allowing that query to hit the scoped table would
-- either disclose an arbitrary tenant profile or let an old write clobber one.
-- Move tenant data behind a new relation name in this same migration
-- transaction and leave the legacy name as a deliberately empty,
-- write-rejecting view.
ALTER TABLE participant_ai_profiles
    RENAME TO participant_ai_profiles_scoped;

CREATE VIEW participant_ai_profiles AS
SELECT participant_id, workspace_id, topics, preferences, summary, updated_at
  FROM participant_ai_profiles_scoped
 WHERE FALSE
WITH LOCAL CHECK OPTION;

COMMENT ON VIEW participant_ai_profiles IS
    'Fail-closed rolling-upgrade fence for pre-tenant AI-profile binaries.';

-- The table rename retains its grants; reproduce its explicit DML grants on
-- the compatibility view so old pods fail because of the safe view contract,
-- not because rollout happened to change their role privileges.
DO $$
DECLARE
    table_grant RECORD;
    grantee_sql TEXT;
BEGIN
    FOR table_grant IN
        SELECT grantee, privilege_type
          FROM information_schema.role_table_grants
         WHERE table_schema = current_schema()
           AND table_name = 'participant_ai_profiles_scoped'
           AND privilege_type IN ('SELECT', 'INSERT', 'UPDATE', 'DELETE')
    LOOP
        grantee_sql := CASE
            WHEN table_grant.grantee = 'PUBLIC' THEN 'PUBLIC'
            ELSE format('%I', table_grant.grantee)
        END;
        EXECUTE format(
            'GRANT %s ON participant_ai_profiles TO %s',
            table_grant.privilege_type,
            grantee_sql
        );
    END LOOP;
END
$$;

-- Old GDPR code tombstones participants and then DELETEs through the legacy
-- relation. The compatibility view intentionally affects zero rows, so the
-- database trigger keeps erasure load-bearing throughout the mixed-version
-- window.
CREATE OR REPLACE FUNCTION erase_scoped_ai_profiles_on_participant_tombstone()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        DELETE FROM participant_ai_profiles_scoped
         WHERE participant_id = NEW.id;
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS participants_erase_scoped_ai_profiles ON participants;
CREATE TRIGGER participants_erase_scoped_ai_profiles
    AFTER UPDATE OF deleted_at ON participants
    FOR EACH ROW
    EXECUTE FUNCTION erase_scoped_ai_profiles_on_participant_tombstone();
