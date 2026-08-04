-- 0186_recurring_delivery_leases.sql
--
-- Recurring messages and AI digests previously listed due rows without a lease
-- and advanced the cadence even when generation/delivery failed. Multiple
-- replicas could also fire the same occurrence concurrently. Add a per-
-- occurrence delivery key, leased claim, monotonic attempt fence, retry cursor,
-- and last-success timestamp. next_run/next_run_at advances only through a
-- token-fenced success confirmation.

ALTER TABLE recurring_messages
    ADD COLUMN IF NOT EXISTS claim_token UUID,
    ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS lease_expires_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS delivery_key UUID,
    ADD COLUMN IF NOT EXISTS delivery_payload JSONB,
    ADD COLUMN IF NOT EXISTS delivery_attempts INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS retry_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS last_error TEXT,
    ADD COLUMN IF NOT EXISTS last_sent_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS dead_at TIMESTAMPTZ;

ALTER TABLE digest_subscriptions
    ADD COLUMN IF NOT EXISTS claim_token UUID,
    ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS lease_expires_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS delivery_key UUID,
    ADD COLUMN IF NOT EXISTS delivery_attempts INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS retry_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS last_error TEXT,
    ADD COLUMN IF NOT EXISTS prepared_summary TEXT,
    ADD COLUMN IF NOT EXISTS last_sent_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS dead_at TIMESTAMPTZ;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'recurring_messages_delivery_attempts_nonnegative'
           AND conrelid = 'recurring_messages'::regclass
    ) THEN
        ALTER TABLE recurring_messages
            ADD CONSTRAINT recurring_messages_delivery_attempts_nonnegative
            CHECK (delivery_attempts >= 0);
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'recurring_messages_claim_shape'
           AND conrelid = 'recurring_messages'::regclass
    ) THEN
        ALTER TABLE recurring_messages
            ADD CONSTRAINT recurring_messages_claim_shape
            CHECK (
                (claim_token IS NULL
                    AND claimed_at IS NULL
                    AND lease_expires_at IS NULL)
                OR
                (claim_token IS NOT NULL
                    AND claimed_at IS NOT NULL
                    AND lease_expires_at IS NOT NULL
                    AND delivery_key IS NOT NULL
                    AND delivery_payload IS NOT NULL)
            );
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'recurring_messages_dead_shape'
           AND conrelid = 'recurring_messages'::regclass
    ) THEN
        ALTER TABLE recurring_messages
            ADD CONSTRAINT recurring_messages_dead_shape
            CHECK (dead_at IS NULL OR (NOT active AND claim_token IS NULL));
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'digest_subscriptions_delivery_attempts_nonnegative'
           AND conrelid = 'digest_subscriptions'::regclass
    ) THEN
        ALTER TABLE digest_subscriptions
            ADD CONSTRAINT digest_subscriptions_delivery_attempts_nonnegative
            CHECK (delivery_attempts >= 0);
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'digest_subscriptions_claim_shape'
           AND conrelid = 'digest_subscriptions'::regclass
    ) THEN
        ALTER TABLE digest_subscriptions
            ADD CONSTRAINT digest_subscriptions_claim_shape
            CHECK (
                (claim_token IS NULL
                    AND claimed_at IS NULL
                    AND lease_expires_at IS NULL)
                OR
                (claim_token IS NOT NULL
                    AND claimed_at IS NOT NULL
                    AND lease_expires_at IS NOT NULL
                    AND delivery_key IS NOT NULL)
            );
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'digest_subscriptions_dead_shape'
           AND conrelid = 'digest_subscriptions'::regclass
    ) THEN
        ALTER TABLE digest_subscriptions
            ADD CONSTRAINT digest_subscriptions_dead_shape
            CHECK (dead_at IS NULL OR claim_token IS NULL);
    END IF;
END
$$;

DROP INDEX IF EXISTS recurring_messages_due_idx;
CREATE INDEX IF NOT EXISTS recurring_messages_delivery_due_idx
    ON recurring_messages (
        (COALESCE(retry_at, next_run)),
        next_run,
        id
    )
    WHERE active;

CREATE INDEX IF NOT EXISTS recurring_messages_delivery_dead_idx
    ON recurring_messages (dead_at, id)
    WHERE dead_at IS NOT NULL;

DROP INDEX IF EXISTS digest_subscriptions_due_idx;
CREATE INDEX IF NOT EXISTS digest_subscriptions_delivery_due_idx
    ON digest_subscriptions (
        (COALESCE(retry_at, next_run_at)),
        next_run_at,
        id
    )
    WHERE dead_at IS NULL;

CREATE INDEX IF NOT EXISTS digest_subscriptions_delivery_dead_idx
    ON digest_subscriptions (dead_at, id)
    WHERE dead_at IS NOT NULL;

-- Workspace digest retries use delivery_key as activity_feed.subject_id.
-- A process crash after insert but before confirmation therefore replays as a
-- no-op instead of adding a duplicate feed item.
CREATE UNIQUE INDEX IF NOT EXISTS activity_feed_digest_delivery_uniq
    ON activity_feed (participant_id, subject_id)
    WHERE kind = 'digest' AND subject_id IS NOT NULL;
