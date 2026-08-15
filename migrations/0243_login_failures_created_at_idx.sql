-- Migration 0243: login_failures.created_at single-column index (auth B5-1 slice).
-- RENUMBERED from the original 0242 plan (connector-root design §7 D8): 0242 is
-- taken by the aero-ai L1 trigger migration; this slice unconditionally takes
-- 0243 + 0244. Serves the L1 bucket scan (aggregate_login_failure_buckets) and
-- the retention sweep. Idempotent, additive.
CREATE INDEX IF NOT EXISTS login_failures_created_at_idx
    ON login_failures (created_at);
