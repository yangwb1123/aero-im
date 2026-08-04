-- Approval inbox/outbox queries and writes are workspace-scoped in the
-- repository. Add database-level ownership references and matching hot-path
-- indexes so new rows cannot point at missing global resources.

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'approvals_workspace_id_fkey'
           AND conrelid = 'approvals'::regclass
    ) THEN
        ALTER TABLE approvals
            ADD CONSTRAINT approvals_workspace_id_fkey
            FOREIGN KEY (workspace_id) REFERENCES workspaces(id)
            ON DELETE CASCADE
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'approvals_requester_id_fkey'
           AND conrelid = 'approvals'::regclass
    ) THEN
        ALTER TABLE approvals
            ADD CONSTRAINT approvals_requester_id_fkey
            FOREIGN KEY (requester_id) REFERENCES participants(id)
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'approvals_approver_id_fkey'
           AND conrelid = 'approvals'::regclass
    ) THEN
        ALTER TABLE approvals
            ADD CONSTRAINT approvals_approver_id_fkey
            FOREIGN KEY (approver_id) REFERENCES participants(id)
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'approvals_status_check'
           AND conrelid = 'approvals'::regclass
    ) THEN
        ALTER TABLE approvals
            ADD CONSTRAINT approvals_status_check
            CHECK (status IN ('pending', 'approved', 'denied'))
            NOT VALID;
    END IF;
END
$$;

CREATE INDEX IF NOT EXISTS approvals_workspace_approver_status_idx
    ON approvals (workspace_id, approver_id, status, created_at DESC);

CREATE INDEX IF NOT EXISTS approvals_workspace_requester_idx
    ON approvals (workspace_id, requester_id, created_at DESC);
