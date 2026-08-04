-- 0184_scheduled_delivery_state.sql
--
-- One-shot scheduled messages used to set delivered_at while claiming work.
-- A transient send failure therefore made the message disappear permanently.
-- Model delivery as a leased, fenced state machine instead:
--
--   pending -> claimed -> delivered
--                    \-> pending (retry with backoff)
--                    \-> dead
--
-- Cancellation is a terminal state too.  claim_token + attempts fence stale
-- workers after lease expiry/reclaim; the application derives a stable
-- client_message_id from scheduled_messages.id to close the
-- send-success/confirm-crash ambiguity.

ALTER TABLE scheduled_messages
    ADD COLUMN IF NOT EXISTS delivery_status TEXT,
    ADD COLUMN IF NOT EXISTS available_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS claim_token UUID,
    ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS lease_expires_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS attempts INTEGER,
    ADD COLUMN IF NOT EXISTS delivery_generation INTEGER,
    ADD COLUMN IF NOT EXISTS last_error TEXT,
    ADD COLUMN IF NOT EXISTS dead_at TIMESTAMPTZ;

UPDATE scheduled_messages
   SET delivery_status = CASE
           WHEN canceled_at IS NOT NULL THEN 'canceled'
           WHEN delivered_at IS NOT NULL THEN 'delivered'
           ELSE 'pending'
       END
 WHERE delivery_status IS NULL;

UPDATE scheduled_messages
   SET available_at = scheduled_at
 WHERE available_at IS NULL;

UPDATE scheduled_messages
   SET attempts = 0
 WHERE attempts IS NULL;

UPDATE scheduled_messages
   SET delivery_generation = 0
 WHERE delivery_generation IS NULL;

ALTER TABLE scheduled_messages
    ALTER COLUMN delivery_status SET DEFAULT 'pending',
    ALTER COLUMN delivery_status SET NOT NULL,
    ALTER COLUMN available_at SET DEFAULT now(),
    ALTER COLUMN available_at SET NOT NULL,
    ALTER COLUMN attempts SET DEFAULT 0,
    ALTER COLUMN attempts SET NOT NULL,
    ALTER COLUMN delivery_generation SET DEFAULT 0,
    ALTER COLUMN delivery_generation SET NOT NULL;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'scheduled_messages_delivery_status_check'
           AND conrelid = 'scheduled_messages'::regclass
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT scheduled_messages_delivery_status_check
            CHECK (delivery_status IN ('pending', 'claimed', 'delivered', 'dead', 'canceled'));
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'scheduled_messages_attempts_nonnegative'
           AND conrelid = 'scheduled_messages'::regclass
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT scheduled_messages_attempts_nonnegative
            CHECK (attempts >= 0);
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'scheduled_messages_delivery_generation_nonnegative'
           AND conrelid = 'scheduled_messages'::regclass
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT scheduled_messages_delivery_generation_nonnegative
            CHECK (delivery_generation >= 0);
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'scheduled_messages_claim_shape'
           AND conrelid = 'scheduled_messages'::regclass
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT scheduled_messages_claim_shape
            CHECK (
                (delivery_status = 'claimed'
                    AND claim_token IS NOT NULL
                    AND claimed_at IS NOT NULL
                    AND lease_expires_at IS NOT NULL)
                OR
                (delivery_status <> 'claimed'
                    AND claim_token IS NULL
                    AND claimed_at IS NULL
                    AND lease_expires_at IS NULL)
            );
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'scheduled_messages_terminal_shape'
           AND conrelid = 'scheduled_messages'::regclass
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT scheduled_messages_terminal_shape
            CHECK (
                (delivery_status = 'delivered' AND delivered_at IS NOT NULL)
                OR (delivery_status <> 'delivered' AND delivered_at IS NULL)
            )
            NOT VALID;
    END IF;

    ALTER TABLE scheduled_messages
        DROP CONSTRAINT IF EXISTS scheduled_messages_cancel_before_attempt;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'scheduled_messages_cancel_shape'
           AND conrelid = 'scheduled_messages'::regclass
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT scheduled_messages_cancel_shape
            CHECK (
                (delivery_status = 'canceled' AND canceled_at IS NOT NULL)
                OR (delivery_status <> 'canceled' AND canceled_at IS NULL)
            )
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'scheduled_messages_dead_shape'
           AND conrelid = 'scheduled_messages'::regclass
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT scheduled_messages_dead_shape
            CHECK (
                (delivery_status = 'dead' AND dead_at IS NOT NULL)
                OR (delivery_status <> 'dead' AND dead_at IS NULL)
            )
            NOT VALID;
    END IF;
END
$$;

-- Existing rows were normalized above, so validation is safe on both fresh and
-- upgraded databases.  The NOT VALID declaration keeps the ALTER lock brief.
ALTER TABLE scheduled_messages
    VALIDATE CONSTRAINT scheduled_messages_terminal_shape,
    VALIDATE CONSTRAINT scheduled_messages_cancel_shape,
    VALIDATE CONSTRAINT scheduled_messages_dead_shape;

-- Preserve the idempotency fingerprint even while pre-0184 application
-- instances are draining. Their UPDATE statement does not know about attempts,
-- so enforce immutability and monotonic generations at the database boundary.
CREATE OR REPLACE FUNCTION scheduled_messages_guard_delivery_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- Pre-0184 callers only set canceled_at. Preserve that safe rolling-upgrade
    -- operation by normalizing it into the new terminal state; their unsafe
    -- delivered_at-at-claim update remains rejected by terminal_shape.
    IF OLD.canceled_at IS NULL
       AND NEW.canceled_at IS NOT NULL
       AND NEW.delivery_status = 'pending'
       AND NEW.attempts = 0 THEN
        NEW.delivery_status := 'canceled';
    END IF;

    IF NEW.delivery_generation < OLD.delivery_generation THEN
        RAISE EXCEPTION 'scheduled message delivery generation cannot move backwards'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.delivery_generation <> OLD.delivery_generation AND NOT (
        OLD.delivery_status = 'dead'
        AND NEW.delivery_status = 'pending'
        AND NEW.delivery_generation = OLD.delivery_generation + 1
        AND NEW.attempts = 0
    ) THEN
        RAISE EXCEPTION 'scheduled message delivery generation may only advance on dead retry'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.attempts < OLD.attempts AND NOT (
        OLD.delivery_status = 'dead'
        AND NEW.delivery_status = 'pending'
        AND NEW.delivery_generation = OLD.delivery_generation + 1
        AND NEW.attempts = 0
    ) THEN
        RAISE EXCEPTION 'scheduled message attempts cannot move backwards'
            USING ERRCODE = '23514';
    END IF;

    IF (OLD.attempts > 0 OR NEW.attempts > 0) AND (
        NEW.room_id IS DISTINCT FROM OLD.room_id
        OR NEW.sender_id IS DISTINCT FROM OLD.sender_id
        OR NEW.blocks IS DISTINCT FROM OLD.blocks
        OR NEW.reply_to IS DISTINCT FROM OLD.reply_to
        OR NEW.scheduled_at IS DISTINCT FROM OLD.scheduled_at
    ) THEN
        RAISE EXCEPTION 'scheduled message payload is immutable after delivery starts'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS scheduled_messages_delivery_fence ON scheduled_messages;
CREATE TRIGGER scheduled_messages_delivery_fence
    BEFORE UPDATE ON scheduled_messages
    FOR EACH ROW
    EXECUTE FUNCTION scheduled_messages_guard_delivery_fence();

DROP INDEX IF EXISTS scheduled_messages_due_idx;
DROP INDEX IF EXISTS scheduled_messages_sender_pending_idx;

CREATE INDEX IF NOT EXISTS scheduled_messages_delivery_due_idx
    ON scheduled_messages (
        (CASE
            WHEN attempts = 0 AND delivery_generation = 0
                THEN scheduled_at
            ELSE available_at
        END),
        scheduled_at,
        id
    )
    WHERE delivery_status = 'pending' AND canceled_at IS NULL;

CREATE INDEX IF NOT EXISTS scheduled_messages_delivery_reclaim_idx
    ON scheduled_messages (lease_expires_at, id)
    WHERE delivery_status = 'claimed';

CREATE INDEX IF NOT EXISTS scheduled_messages_sender_pending_idx
    ON scheduled_messages (sender_id, room_id, scheduled_at)
    WHERE delivery_status = 'pending'
      AND attempts = 0
      AND delivery_generation = 0
      AND canceled_at IS NULL;

CREATE INDEX IF NOT EXISTS scheduled_messages_dead_idx
    ON scheduled_messages (dead_at, id)
    WHERE delivery_status = 'dead';
