-- 0056 Approvals workflow (Lark 审批 / approvals-lite, single-approver MVP).
--
-- A requester opens an approval request addressed to a single approver within a
-- workspace (title + optional details). The named approver then approves or
-- denies it with an optional decision note. Each row starts `pending` and
-- transitions exactly once to `approved`/`denied`, stamping `decided_at`.
--
-- Approver-scoped decisions: only the named approver can decide, and only while
-- the request is still pending. Workspace membership of both parties is enforced
-- at the API layer. Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS approvals (
  id uuid PRIMARY KEY,
  workspace_id uuid NOT NULL,
  requester_id uuid NOT NULL,
  approver_id uuid NOT NULL,
  title text NOT NULL,
  details text,
  status text NOT NULL DEFAULT 'pending',
  decision_note text,
  decided_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS approvals_approver_idx ON approvals (approver_id, status);
CREATE INDEX IF NOT EXISTS approvals_requester_idx ON approvals (requester_id);
