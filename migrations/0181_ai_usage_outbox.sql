-- Crash-recoverable AI usage accounting.
--
-- Every paid provider call first creates a fenced reservation with a
-- conservative estimated charge. Provider success finalizes the actual charge;
-- an ordinary provider failure cancels it. If the caller disappears while the
-- outcome is ambiguous, the expired reservation becomes a ready estimated
-- charge. A separate leased relay moves ready charges into ai_usage_ledger.
-- Successful finalization stores the versioned minimal provider result in the
-- same row, so a stable retry can resume its business write without a second
-- paid call. Keeping terminal rows provides the durable idempotency key across
-- process restarts and ambiguous client retries.
ALTER TABLE ai_usage_ledger
    ADD COLUMN IF NOT EXISTS usage_id UUID;

CREATE UNIQUE INDEX IF NOT EXISTS ai_usage_ledger_usage_id_unique
    ON ai_usage_ledger (usage_id)
    WHERE usage_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS ai_usage_outbox (
    usage_id        UUID        PRIMARY KEY,
    workspace_id    UUID,
    kind            TEXT        NOT NULL CHECK (length(kind) BETWEEN 1 AND 64),
    cost_micros     BIGINT      NOT NULL CHECK (cost_micros > 0),
    -- Expected schema and the minimal provider result needed to resume after
    -- accounting committed but its caller disappeared. Ambiguously expired
    -- reservations deliberately have no outcome payload.
    outcome_kind    TEXT        CHECK (
                                     outcome_kind IS NULL
                                     OR length(outcome_kind) BETWEEN 1 AND 64
                                 ),
    outcome_json    JSONB,
    status          TEXT        NOT NULL DEFAULT 'reserved'
                                 CHECK (status IN (
                                     'reserved', 'ready', 'completed', 'cancelled'
                                 )),
    reservation_token UUID,
    reservation_expires_at TIMESTAMPTZ,
    finalized_at    TIMESTAMPTZ,
    cancelled_at    TIMESTAMPTZ,
    attempts        INTEGER     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    available_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    claim_token     UUID,
    lease_expires_at TIMESTAMPTZ,
    completed_at    TIMESTAMPTZ,
    last_error      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (
        (claim_token IS NULL AND lease_expires_at IS NULL)
        OR (claim_token IS NOT NULL AND lease_expires_at IS NOT NULL)
    ),
    CHECK (outcome_json IS NULL OR outcome_kind IS NOT NULL),
    CHECK (
        (
            status = 'reserved'
            AND reservation_token IS NOT NULL
            AND reservation_expires_at IS NOT NULL
            AND finalized_at IS NULL
            AND cancelled_at IS NULL
            AND completed_at IS NULL
            AND claim_token IS NULL
            AND lease_expires_at IS NULL
            AND outcome_json IS NULL
        )
        OR (
            status = 'ready'
            AND reservation_token IS NULL
            AND reservation_expires_at IS NULL
            AND finalized_at IS NOT NULL
            AND cancelled_at IS NULL
            AND completed_at IS NULL
        )
        OR (
            status = 'completed'
            AND reservation_token IS NULL
            AND reservation_expires_at IS NULL
            AND finalized_at IS NOT NULL
            AND cancelled_at IS NULL
            AND completed_at IS NOT NULL
            AND claim_token IS NULL
            AND lease_expires_at IS NULL
        )
        OR (
            status = 'cancelled'
            AND reservation_token IS NULL
            AND reservation_expires_at IS NULL
            AND finalized_at IS NULL
            AND cancelled_at IS NOT NULL
            AND completed_at IS NULL
            AND claim_token IS NULL
            AND lease_expires_at IS NULL
            AND outcome_json IS NULL
        )
    )
);

CREATE INDEX IF NOT EXISTS ai_usage_outbox_due_idx
    ON ai_usage_outbox (available_at, created_at, usage_id)
    WHERE status = 'ready';

CREATE INDEX IF NOT EXISTS ai_usage_outbox_reservation_expiry_idx
    ON ai_usage_outbox (reservation_expires_at, usage_id)
    WHERE status = 'reserved';

CREATE INDEX IF NOT EXISTS ai_usage_outbox_completed_idx
    ON ai_usage_outbox (completed_at)
    WHERE status = 'completed';

CREATE INDEX IF NOT EXISTS ai_usage_outbox_cancelled_idx
    ON ai_usage_outbox (cancelled_at)
    WHERE status = 'cancelled';

COMMENT ON TABLE ai_usage_outbox IS
    'Fenced provider reservations, replayable minimal outcomes, and finalized paid AI charges awaiting idempotent ledger insertion';
COMMENT ON COLUMN ai_usage_outbox.reservation_token IS
    'Random provider-call fence; an expired reservation is conservatively charged';
COMMENT ON COLUMN ai_usage_outbox.outcome_json IS
    'Versioned minimal provider result for stable-operation replay; NULL for ambiguous recovery and legacy cost-only rows';
COMMENT ON COLUMN ai_usage_outbox.claim_token IS
    'Random ledger-relay generation fence rotated on every crash-recoverable lease claim';
