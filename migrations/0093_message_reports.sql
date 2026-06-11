-- 0093_message_reports.sql — user-initiated message reports + moderation review queue
--
-- A room member may flag a message they find abusive/spammy/etc. The report lands
-- here as `pending` and surfaces in a workspace-admin moderation review queue, where
-- an administrator decides to KEEP (dismiss the report) or REMOVE (soft-delete the
-- reported message via the existing transactional moderate-delete path). This is the
-- human-review counterpart to the AI moderation pipeline, which auto-soft-deletes
-- async with no human in the loop.
--
-- No FK to messages: a report is a HISTORICAL record that must outlive the message
-- it references — once a message is removed (by this very review, or by anything
-- else), its report row stays as an audit of the decision. Existence of the message
-- is validated in the handler at report time, NOT enforced via a foreign key (same
-- lesson as ban_appeals). workspace_id / reporter_id / reviewed_by are likewise plain
-- UUIDs (denormalized), so the queue survives membership churn.
--
-- Purely additive — no existing table is touched.

CREATE TABLE IF NOT EXISTS message_reports (
    id           UUID        PRIMARY KEY,
    -- The workspace the reported message's room belongs to (resolved at report
    -- time). Scopes the admin review queue.
    workspace_id UUID        NOT NULL,
    -- The reported message. No FK: the report outlives the message.
    message_id   UUID        NOT NULL,
    -- The participant who filed the report.
    reporter_id  UUID        NOT NULL,
    -- Free-text reason the reporter supplied.
    reason       TEXT        NOT NULL,
    -- Review lifecycle: pending -> kept | removed (terminal). 'kept' dismisses the
    -- report; 'removed' means the reviewer also soft-deleted the message.
    status       TEXT        NOT NULL DEFAULT 'pending'
                 CHECK (status IN ('pending', 'kept', 'removed')),
    -- Optional reviewer note recorded at decision time.
    note         TEXT,
    -- The administrator who reviewed it; NULL while pending.
    reviewed_by  UUID,
    -- When the decision was stamped; NULL while pending.
    reviewed_at  TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The review queue is "pending reports in this workspace, oldest first" so an
-- administrator works the backlog FIFO. (workspace_id, status, created_at ASC,
-- id ASC) covers the listing filter + order with a stable id tiebreak.
CREATE INDEX IF NOT EXISTS message_reports_queue_idx
    ON message_reports (workspace_id, status, created_at ASC, id ASC);
