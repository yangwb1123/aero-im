-- 0008_ai_job_workspace.sql — tag AI jobs with their workspace (ROADMAP 方向三)
--
-- Lets the worker meter paid-API spend PER tenant (a per-workspace budget),
-- not just globally — so one busy/abusive workspace cannot starve others within
-- the global cap. Nullable: jobs whose workspace can't be resolved (or legacy
-- rows) fall back to the global budget only. Purely additive.

ALTER TABLE ai_jobs ADD COLUMN IF NOT EXISTS workspace_id UUID;
