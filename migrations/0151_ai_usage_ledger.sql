-- Per-tenant AI usage ledger (ROADMAP 第六版 · 方向一·2).
--
-- A durable, queryable record of real AI spend per workspace — the billing /
-- usage-attribution foundation the Prometheus AI_COST_MICROS_TOTAL counter cannot
-- be (counters are aggregate, scrape-window, non-historical). Every paid charge
-- (`aero_ai::metrics::charge_cost`, the single convergence point of BOTH the
-- ai_jobs worker queue AND the real-time moderation bot) is fanned to a bounded
-- channel and batch-inserted here by a boot drain task — off the hot path.
--
-- No FK to workspaces: a billing/usage record outlives the workspace (same audit
-- semantics as audit_events), and a NULL workspace_id is a legacy/system job.
CREATE TABLE IF NOT EXISTS ai_usage_ledger (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id UUID        NULL,
    kind         TEXT        NOT NULL,
    cost_micros  BIGINT      NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Drives the per-workspace, time-bounded summary query (GET /api/workspaces/:id/ai-usage).
CREATE INDEX IF NOT EXISTS ai_usage_ledger_ws_time
    ON ai_usage_ledger (workspace_id, created_at DESC);
