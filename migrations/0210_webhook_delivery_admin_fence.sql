-- Fence outbound-webhook delivery administration to its immutable tenant and
-- make every non-terminal delivery state recoverable.
--
-- The owning tenant is deliberately not copied into webhook_delivery_log:
-- `delivery -> outgoing_webhook -> room -> workspace` remains the single source
-- of truth.  Instead, freeze both identity edges so raw SQL cannot move a hook
-- or delivery after an administrator has resolved it.  The triggers cover
-- UPDATE only; the existing ON DELETE CASCADE chain remains unchanged.

CREATE OR REPLACE FUNCTION aero_outgoing_webhook_room_identity_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.room_id IS DISTINCT FROM OLD.room_id THEN
        RAISE EXCEPTION 'outgoing webhook room identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'outgoing_webhook_room_identity_immutable';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS outgoing_webhook_room_identity_immutable_trg
    ON outgoing_webhooks;
CREATE TRIGGER outgoing_webhook_room_identity_immutable_trg
    BEFORE UPDATE OF room_id
    ON outgoing_webhooks
    FOR EACH ROW
    EXECUTE FUNCTION aero_outgoing_webhook_room_identity_immutable();

CREATE OR REPLACE FUNCTION aero_webhook_delivery_identity_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.webhook_id IS DISTINCT FROM OLD.webhook_id THEN
        RAISE EXCEPTION 'webhook delivery webhook identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'webhook_delivery_webhook_identity_immutable';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS webhook_delivery_identity_immutable_trg
    ON webhook_delivery_log;
CREATE TRIGGER webhook_delivery_identity_immutable_trg
    BEFORE UPDATE OF webhook_id
    ON webhook_delivery_log
    FOR EACH ROW
    EXECUTE FUNCTION aero_webhook_delivery_identity_immutable();

-- Normalize legacy rows before installing the state-shape constraints.  A
-- failed row without a deadline is otherwise invisible to claim_due forever;
-- a pending row is recovered by updated_at and must not retain an obsolete due
-- time from its previous failed generation.
UPDATE webhook_delivery_log
   SET attempts = GREATEST(0, LEAST(attempts, 6)),
       status = CASE
                    WHEN (status = 'failed' AND attempts >= 6)
                         OR (status = 'pending' AND attempts > 6)
                        THEN 'dead'
                    ELSE status
                END,
       last_error = CASE
                        WHEN (status = 'failed' AND attempts >= 6)
                             OR (status = 'pending' AND attempts > 6)
                            THEN COALESCE(
                                last_error,
                                'legacy delivery exceeded retry budget'
                            )
                        ELSE last_error
                    END,
       next_attempt_at = CASE
                             WHEN (status = 'failed' AND attempts >= 6)
                                  OR (status = 'pending' AND attempts > 6)
                                 THEN NULL
                             WHEN status = 'failed'
                                 THEN COALESCE(next_attempt_at, now())
                             ELSE NULL
                         END,
       updated_at = CASE
                        WHEN attempts < 0
                             OR attempts > 6
                             OR (status = 'failed' AND attempts >= 6)
                             OR (status = 'failed' AND next_attempt_at IS NULL)
                             OR (status <> 'failed' AND next_attempt_at IS NOT NULL)
                            THEN now()
                        ELSE updated_at
                    END
 WHERE attempts < 0
    OR attempts > 6
    OR (status = 'failed' AND attempts >= 6)
    OR (status = 'failed' AND next_attempt_at IS NULL)
    OR (status <> 'failed' AND next_attempt_at IS NOT NULL);

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'webhook_delivery_attempt_budget_chk'
           AND conrelid = 'webhook_delivery_log'::regclass
    ) THEN
        ALTER TABLE webhook_delivery_log
            ADD CONSTRAINT webhook_delivery_attempt_budget_chk
            CHECK (attempts BETWEEN 0 AND 6);
    END IF;
END
$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'webhook_delivery_failed_budget_chk'
           AND conrelid = 'webhook_delivery_log'::regclass
    ) THEN
        ALTER TABLE webhook_delivery_log
            ADD CONSTRAINT webhook_delivery_failed_budget_chk
            CHECK (status <> 'failed' OR attempts < 6);
    END IF;
END
$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'webhook_delivery_retry_deadline_chk'
           AND conrelid = 'webhook_delivery_log'::regclass
    ) THEN
        ALTER TABLE webhook_delivery_log
            ADD CONSTRAINT webhook_delivery_retry_deadline_chk
            CHECK (
                (status = 'failed' AND next_attempt_at IS NOT NULL)
                OR (status <> 'failed' AND next_attempt_at IS NULL)
            );
    END IF;
END
$$;

COMMENT ON FUNCTION aero_outgoing_webhook_room_identity_immutable() IS
    'Freeze outgoing webhook -> room tenant identity while preserving DELETE cascades';
COMMENT ON FUNCTION aero_webhook_delivery_identity_immutable() IS
    'Freeze delivery -> outgoing webhook identity while preserving DELETE cascades';
