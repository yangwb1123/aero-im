-- Per-workspace rate-limit tier (ROADMAP3 方向五 — 租户公平).
--
-- 'standard' | 'premium' | 'unlimited'. The tier selects which per-minute
-- request ceiling applies to the whole tenant (limits themselves come from the
-- server environment: AERO_WS_RATE_STANDARD_PER_MIN / AERO_WS_RATE_PREMIUM_PER_MIN;
-- 'unlimited' skips the check). Unknown tokens are treated as 'standard' by the
-- reader, so a bad value degrades to the tightest paid-nothing tier, never to
-- "no limit".
ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS rate_tier TEXT NOT NULL DEFAULT 'standard';
