-- Preserve the exact outbound request across webhook retries and give bot
-- subscriptions a real per-endpoint signing credential.

ALTER TABLE webhook_delivery_log
    ADD COLUMN IF NOT EXISTS request_body BYTEA,
    ADD COLUMN IF NOT EXISTS request_headers JSONB NOT NULL DEFAULT '[]'::JSONB;

-- Pre-0164 rows did not retain the original payload. Retrying them with a
-- fabricated body would violate the receiver's event contract, so park any
-- still-active legacy rows instead. Terminal history remains readable.
UPDATE webhook_delivery_log
   SET status = 'dead',
       last_error = 'legacy delivery lacks immutable request body',
       next_attempt_at = NULL,
       updated_at = now()
 WHERE request_body IS NULL
   AND status IN ('pending', 'failed');

DO $$
BEGIN
    ALTER TABLE webhook_delivery_log
        ADD CONSTRAINT webhook_delivery_request_body_state_chk
        CHECK (
            request_body IS NOT NULL
            OR status IN ('delivered', 'dead')
        );
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

DO $$
BEGIN
    ALTER TABLE webhook_delivery_log
        ADD CONSTRAINT webhook_delivery_request_headers_shape_chk
        CHECK (jsonb_typeof(request_headers) = 'array');
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

CREATE OR REPLACE FUNCTION aero_webhook_delivery_request_immutable()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.request_body IS DISTINCT FROM OLD.request_body
       OR NEW.request_headers IS DISTINCT FROM OLD.request_headers THEN
        RAISE EXCEPTION 'webhook delivery request is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;

DO $$
BEGIN
    CREATE TRIGGER webhook_delivery_request_immutable_trg
        BEFORE UPDATE OF request_body, request_headers
        ON webhook_delivery_log
        FOR EACH ROW
        EXECUTE FUNCTION aero_webhook_delivery_request_immutable();
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

ALTER TABLE bot_event_subscriptions
    ADD COLUMN IF NOT EXISTS webhook_secret TEXT;

-- Existing webhook-bound subscriptions receive a strong secret. It cannot be
-- shown retroactively; owners can use the rotate endpoint to obtain a new
-- plaintext value exactly once.
UPDATE bot_event_subscriptions
   SET webhook_secret = encode(gen_random_bytes(32), 'hex')
 WHERE webhook_url IS NOT NULL
   AND (webhook_secret IS NULL OR webhook_secret = '');

DO $$
BEGIN
    ALTER TABLE bot_event_subscriptions
        ADD CONSTRAINT bot_event_subscription_webhook_secret_chk
        CHECK (
            webhook_url IS NULL
            OR (
                webhook_secret IS NOT NULL
                AND length(webhook_secret) >= 32
            )
        );
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

COMMENT ON COLUMN webhook_delivery_log.request_body IS
    'Exact immutable HTTP request body captured before the first webhook attempt';
COMMENT ON COLUMN webhook_delivery_log.request_headers IS
    'Unsigned request headers replayed on retry; signature and timestamp are regenerated';
COMMENT ON COLUMN bot_event_subscriptions.webhook_secret IS
    'Per-subscription HMAC secret; returned only on create or rotate';
