-- B5-4: audit relay provisioning heartbeat + dead-row index.
--
-- 1. `audit_relay_provisioning` — a singleton liveness row backing the
--    settle-face freshness gate (fail-closed: NO row = NotVerified = settles
--    rejected). `verified_at` is written on the DB clock
--    (`clock_timestamp()`, single clock domain — every freshness check and
--    sampler age reads the same domain). `provision_state` is a future seam,
--    never read or written here. 0247 was consumed by
--    `0247_governance_failed_pairs_replay_caps.sql`, hence 0248.
--
-- 2. `audit_governance_status3_idx` — partial index on (status) WHERE
--    status = 3 for the Tier-1 QP2 `EXISTS(status = 3)` probe: without it the
--    healthy-state (zero dead rows) EXISTS is a full-table seq scan every 30s
--    per instance; with it the all-healthy case is an O(1) index lookup
--    (perf P-1). Additive and idempotent.
CREATE TABLE IF NOT EXISTS audit_relay_provisioning (
    singleton       BOOLEAN      PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    verified_at     TIMESTAMPTZ  NOT NULL,
    provision_state TEXT         NOT NULL DEFAULT 'provisioned',
    updated_at      TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX IF NOT EXISTS audit_governance_status3_idx
    ON audit_governance_outbox (status) WHERE status = 3;
