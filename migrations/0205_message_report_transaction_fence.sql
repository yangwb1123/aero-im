-- Repair the only class of legacy queue poisoning that can still be resolved:
-- when the referenced message remains present, its room is the canonical tenant
-- boundary. Reports intentionally have no message FK and outlive hard deletion,
-- so rows whose message is already gone remain historical evidence.
UPDATE message_reports report
   SET workspace_id = room.workspace_id
  FROM messages message
  JOIN rooms room
    ON room.id = message.room_id
 WHERE report.message_id = message.id
   AND report.workspace_id <> room.workspace_id;

CREATE OR REPLACE FUNCTION message_report_scope_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    resolved_room uuid;
    reviewer_is_admin boolean;
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.workspace_id IS DISTINCT FROM OLD.workspace_id
           OR NEW.message_id IS DISTINCT FROM OLD.message_id
           OR NEW.reporter_id IS DISTINCT FROM OLD.reporter_id
       ) THEN
        RAISE EXCEPTION 'message report tenant identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'message_reports_identity_immutable_chk';
    END IF;

    IF TG_OP = 'INSERT' THEN
        -- This is a lock-free containment backstop for raw SQL. Production
        -- writes use MessageReportRepo::report_authorized, whose canonical
        -- workspace -> room -> membership -> message locks also close races.
        SELECT message.room_id
          INTO resolved_room
          FROM messages message
          JOIN rooms room
            ON room.id = message.room_id
         WHERE message.id = NEW.message_id
           AND message.deleted_at IS NULL
           AND room.workspace_id = NEW.workspace_id
           AND aero_participant_has_effective_workspace_access(
                   NEW.workspace_id,
                   NEW.reporter_id
               )
           AND EXISTS (
               SELECT 1
                 FROM room_members membership
                WHERE membership.room_id = message.room_id
                  AND membership.participant_id = NEW.reporter_id
           );
        IF NOT FOUND THEN
            RAISE EXCEPTION
                'message report must bind a live message and effective room member in the same workspace'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'message_reports_scope_containment_chk';
        END IF;
    END IF;

    IF TG_OP = 'UPDATE'
       AND OLD.status <> 'pending'
       AND NEW.status IS DISTINCT FROM OLD.status THEN
        RAISE EXCEPTION 'message report decisions are terminal'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'message_reports_terminal_status_chk';
    END IF;

    IF NEW.status = 'pending' THEN
        IF NEW.reviewed_by IS NOT NULL OR NEW.reviewed_at IS NOT NULL THEN
            RAISE EXCEPTION 'pending message report cannot have reviewer metadata'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'message_reports_review_state_chk';
        END IF;
    ELSIF TG_OP = 'INSERT'
       OR OLD.status = 'pending'
       OR NEW.reviewed_by IS DISTINCT FROM OLD.reviewed_by THEN
        IF NEW.reviewed_by IS NULL OR NEW.reviewed_at IS NULL THEN
            RAISE EXCEPTION 'decided message report requires reviewer metadata'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'message_reports_review_state_chk';
        END IF;

        SELECT (
                   member.role IN ('owner', 'admin')
                   AND aero_participant_has_effective_workspace_access(
                           NEW.workspace_id,
                           NEW.reviewed_by
                       )
               )
          INTO reviewer_is_admin
          FROM workspace_members member
         WHERE member.workspace_id = NEW.workspace_id
           AND member.participant_id = NEW.reviewed_by;
        IF NOT COALESCE(reviewer_is_admin, false) THEN
            RAISE EXCEPTION
                'message report reviewer must be an effective workspace administrator'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'message_reports_reviewer_scope_chk';
        END IF;
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS message_report_scope_guard
    ON message_reports;
CREATE TRIGGER message_report_scope_guard
    BEFORE INSERT OR UPDATE
    ON message_reports
    FOR EACH ROW
    EXECUTE FUNCTION message_report_scope_guard();

COMMENT ON TRIGGER message_report_scope_guard ON message_reports IS
    'Backstops raw SQL tenant/message containment, immutable identity, terminal decisions, and workspace-scoped reviewers; repository methods own race-free authorization.';
