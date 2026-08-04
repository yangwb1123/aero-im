-- Separate webhook delivery ownership from the HTTP-attempt counter.
--
-- `attempts` is business state (how many HTTP calls were started), so it cannot
-- safely double as a reusable claim generation: an admin requeue intentionally
-- resets attempts to zero. A random token is minted for every durable claim and
-- fences begin/settlement/defer/release operations against stale workers.
ALTER TABLE webhook_delivery_log
    ADD COLUMN IF NOT EXISTS claim_token UUID NOT NULL DEFAULT gen_random_uuid();

COMMENT ON COLUMN webhook_delivery_log.claim_token IS
    'Unforgeable, non-reusable owner token rotated for every delivery claim';
