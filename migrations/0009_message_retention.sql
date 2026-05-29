-- 0009_message_retention.sql — per-workspace message retention policy
-- (ROADMAP 方向一 合规 — "按组织的留存策略").
--
-- Each workspace may declare a retention window in whole days. A periodic
-- server-side sweep soft-deletes messages whose room belongs to a workspace
-- with a non-NULL `retention_days` and whose `created_at` is older than that
-- window — applying the SAME soft-delete mutation the message edit/delete path
-- uses (deleted_at + blocks='[]'::jsonb + cleared searchable_text/embedding), so
-- a swept message is indistinguishable from any other deletion.
--
-- NULL = keep forever (the default for every existing and new workspace), so
-- this migration changes no behavior until an admin opts a tenant in. Purely
-- additive and idempotent (`IF NOT EXISTS`).

ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS retention_days INTEGER;
