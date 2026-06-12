-- Idempotent gift sends. A retried gift RPC (network blip, double-click, client
-- resend) must record the ledger row, advance goal bars, and broadcast the
-- StreamEvent exactly once. The client supplies a stable key (REST
-- `Idempotency-Key` header / WS `stream_gift` frame `nonce`); this partial unique
-- index dedups per sender. Keyless gifts (idempotency_key IS NULL) keep the
-- legacy always-insert behaviour, so existing clients are unaffected.
ALTER TABLE stream_gifts ADD COLUMN IF NOT EXISTS idempotency_key TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS uq_stream_gifts_sender_idem
    ON stream_gifts (sender_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;
