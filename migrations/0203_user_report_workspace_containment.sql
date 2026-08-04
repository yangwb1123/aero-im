-- A workspace-scoped user report must be filed by and about participants who
-- actually belong to that tenant. Historical rows remain valid after later
-- membership removal, but legacy rows whose workspace context is not backed by
-- both retained membership edges are moved to the global scope so they cannot
-- pollute an unrelated tenant's moderation queue.
UPDATE user_reports report
   SET workspace_id = NULL
 WHERE report.workspace_id IS NOT NULL
   AND (
       NOT EXISTS (
           SELECT 1
             FROM workspace_members membership
            WHERE membership.workspace_id = report.workspace_id
              AND membership.participant_id = report.reporter_id
       )
       OR NOT EXISTS (
           SELECT 1
             FROM workspace_members membership
            WHERE membership.workspace_id = report.workspace_id
              AND membership.participant_id = report.reported_id
       )
   );

-- PostgreSQL UNIQUE constraints treat NULL values as distinct. Keep the oldest
-- global report for each pair before adding the partial key that makes the
-- documented global idempotency rule real.
WITH duplicate_global AS (
    SELECT id,
           row_number() OVER (
               PARTITION BY reporter_id, reported_id
               ORDER BY created_at, id
           ) AS duplicate_rank
      FROM user_reports
     WHERE workspace_id IS NULL
)
DELETE FROM user_reports report
 USING duplicate_global duplicate
 WHERE duplicate.id = report.id
   AND duplicate.duplicate_rank > 1;

CREATE UNIQUE INDEX IF NOT EXISTS user_reports_global_pair_key
    ON user_reports (reporter_id, reported_id)
    WHERE workspace_id IS NULL;

CREATE OR REPLACE FUNCTION user_report_workspace_membership_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    reporter_allowed boolean;
    reported_allowed boolean;
BEGIN
    IF NEW.workspace_id IS NULL THEN
        RETURN NEW;
    END IF;

    SELECT aero_effective_workspace_access(NEW.workspace_id, NEW.reporter_id)
      INTO reporter_allowed;
    SELECT aero_effective_workspace_access(NEW.workspace_id, NEW.reported_id)
      INTO reported_allowed;

    IF NOT reporter_allowed OR NOT reported_allowed THEN
        RAISE EXCEPTION
            'workspace-scoped report participants must have effective access to workspace %',
            NEW.workspace_id
            USING ERRCODE = '23514',
                  CONSTRAINT = 'user_reports_workspace_membership_chk';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS user_report_workspace_membership_guard
    ON user_reports;
CREATE TRIGGER user_report_workspace_membership_guard
    BEFORE INSERT OR UPDATE OF reporter_id, reported_id, workspace_id
    ON user_reports
    FOR EACH ROW
    EXECUTE FUNCTION user_report_workspace_membership_guard();

COMMENT ON TRIGGER user_report_workspace_membership_guard ON user_reports IS
    'Binds new workspace-scoped reports to two currently effective tenant members; later membership changes preserve the historical row.';
