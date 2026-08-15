-- Migration 0244: DLQ for fail-open audit pairs (auth B5-1 slice, D9).
-- No triggers, no FKs. Connector never claims this table.
-- IMMEDIATE-constraint pin (D4): 0239/0236 CHECK/RAISE constraints are all
-- IMMEDIATE — a future DEFERRABLE change would defer errors to COMMIT and
-- break the SAVEPOINT fail-open branch (whole-tx fail-closed, violates R7).
CREATE TABLE IF NOT EXISTS audit_governance_failed_pairs (
    id              BIGSERIAL   PRIMARY KEY,
    workspace_id    UUID        NOT NULL,
    actor_id        UUID,
    action          TEXT        NOT NULL,
    target          TEXT,
    detail          JSONB       NOT NULL,
    outbound_action TEXT        NOT NULL,
    error_sqlstate  TEXT        NOT NULL,
    error_message   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    replayed_at     TIMESTAMPTZ
);
