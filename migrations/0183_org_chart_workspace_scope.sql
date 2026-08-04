-- Reporting lines are tenant-local. The original table used a global
-- participant primary key, which allowed cross-workspace reads and edges.

ALTER TABLE org_reports
    ADD COLUMN IF NOT EXISTS workspace_id uuid;

UPDATE org_reports
   SET workspace_id = '00000000-0000-0000-0000-000000000000'::uuid
 WHERE workspace_id IS NULL;

-- Keep only legacy edges whose actor, participant and manager all belong to the
-- default workspace used by the historical unscoped API.
DELETE FROM org_reports reports
 WHERE NOT EXISTS (
       SELECT 1
         FROM workspace_members membership
        WHERE membership.workspace_id = reports.workspace_id
          AND membership.participant_id = reports.participant_id
   )
    OR NOT EXISTS (
       SELECT 1
         FROM workspace_members membership
        WHERE membership.workspace_id = reports.workspace_id
          AND membership.participant_id = reports.manager_id
   )
    OR NOT EXISTS (
       SELECT 1
         FROM workspace_members membership
        WHERE membership.workspace_id = reports.workspace_id
          AND membership.participant_id = reports.set_by
   );

ALTER TABLE org_reports
    ALTER COLUMN workspace_id SET NOT NULL;

DO $$
DECLARE
    primary_key_name text;
BEGIN
    SELECT constraint_name
      INTO primary_key_name
      FROM information_schema.table_constraints
     WHERE table_schema = current_schema()
       AND table_name = 'org_reports'
       AND constraint_type = 'PRIMARY KEY'
     LIMIT 1;

    IF primary_key_name IS NOT NULL THEN
        EXECUTE format(
            'ALTER TABLE org_reports DROP CONSTRAINT %I',
            primary_key_name
        );
    END IF;

    ALTER TABLE org_reports
        ADD CONSTRAINT org_reports_pkey
        PRIMARY KEY (workspace_id, participant_id);

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'org_reports_workspace_id_fkey'
           AND conrelid = 'org_reports'::regclass
    ) THEN
        ALTER TABLE org_reports
            ADD CONSTRAINT org_reports_workspace_id_fkey
            FOREIGN KEY (workspace_id) REFERENCES workspaces(id)
            ON DELETE CASCADE;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'org_reports_target_membership_fkey'
           AND conrelid = 'org_reports'::regclass
    ) THEN
        ALTER TABLE org_reports
            ADD CONSTRAINT org_reports_target_membership_fkey
            FOREIGN KEY (workspace_id, participant_id)
            REFERENCES workspace_members(workspace_id, participant_id)
            ON DELETE CASCADE;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'org_reports_manager_membership_fkey'
           AND conrelid = 'org_reports'::regclass
    ) THEN
        ALTER TABLE org_reports
            ADD CONSTRAINT org_reports_manager_membership_fkey
            FOREIGN KEY (workspace_id, manager_id)
            REFERENCES workspace_members(workspace_id, participant_id)
            ON DELETE CASCADE;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'org_reports_participant_id_fkey'
           AND conrelid = 'org_reports'::regclass
    ) THEN
        ALTER TABLE org_reports
            ADD CONSTRAINT org_reports_participant_id_fkey
            FOREIGN KEY (participant_id) REFERENCES participants(id)
            ON DELETE CASCADE;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'org_reports_manager_id_fkey'
           AND conrelid = 'org_reports'::regclass
    ) THEN
        ALTER TABLE org_reports
            ADD CONSTRAINT org_reports_manager_id_fkey
            FOREIGN KEY (manager_id) REFERENCES participants(id)
            ON DELETE CASCADE;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'org_reports_set_by_fkey'
           AND conrelid = 'org_reports'::regclass
    ) THEN
        ALTER TABLE org_reports
            ADD CONSTRAINT org_reports_set_by_fkey
            FOREIGN KEY (set_by) REFERENCES participants(id);
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'org_reports_not_self_check'
           AND conrelid = 'org_reports'::regclass
    ) THEN
        ALTER TABLE org_reports
            ADD CONSTRAINT org_reports_not_self_check
            CHECK (participant_id <> manager_id);
    END IF;
END
$$;

DROP INDEX IF EXISTS org_reports_manager_idx;
CREATE INDEX IF NOT EXISTS org_reports_workspace_manager_idx
    ON org_reports (workspace_id, manager_id, participant_id);
