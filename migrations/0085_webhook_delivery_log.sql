-- Webhook delivery log + retry/DLQ (operability — outbound webhook reliability).
--
-- Today `outgoing_webhooks` delivery is one-shot best-effort: the dispatcher POSTs
-- each matching room event once via reqwest and logs a non-2xx/transport failure,
-- but never retries and keeps no per-attempt record. This table makes outbound
-- delivery durable and observable:
--
--   * Every (webhook, event) delivery has a row tracking its lifecycle —
--     pending → delivered | failed → dead — with an attempt counter and the last
--     HTTP status / error.
--   * `failed` rows carry a future `next_attempt_at` (exponential backoff) so a
--     retry loop can `claim_due(now)` and re-send.
--   * After MAX_ATTEMPTS the row is parked at `dead` (a dead-letter queue) for
--     admin inspection and manual `requeue`.
--
-- Scoped to a single outgoing webhook via FK; cascades with the hook (and thus the
-- room/workspace) so a deleted hook leaves no orphan log. Purely additive — no
-- existing table is altered.
CREATE TABLE IF NOT EXISTS webhook_delivery_log (
    id               UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    webhook_id       UUID        NOT NULL REFERENCES outgoing_webhooks(id) ON DELETE CASCADE,
    -- Opaque correlation id for the delivered event (e.g. the source message id);
    -- free-form text so any event kind can supply a stable handle. NULL allowed.
    event_id         TEXT        NULL,
    -- Lifecycle: pending (just recorded, in flight) → delivered (2xx) |
    -- failed (retryable, has a future next_attempt_at) → dead (cap reached).
    status           TEXT        NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending', 'delivered', 'failed', 'dead')),
    -- How many send attempts have been made so far.
    attempts         INTEGER     NOT NULL DEFAULT 0,
    -- HTTP status code of the last attempt (NULL on a transport-level error).
    last_status_code INTEGER     NULL,
    -- Last error string (non-2xx body excerpt or transport error). NULL on success.
    last_error       TEXT        NULL,
    -- When a `failed` row becomes eligible for retry (exponential backoff). NULL
    -- for terminal states (delivered/dead) and not-yet-failed pending rows.
    next_attempt_at  TIMESTAMPTZ NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Retry-loop claim path: due `failed` rows, oldest-eligible first.
CREATE INDEX IF NOT EXISTS webhook_delivery_due_idx
    ON webhook_delivery_log (next_attempt_at)
    WHERE status = 'failed';

-- Admin DLQ listing + per-hook delivery log: scan by hook, newest first.
CREATE INDEX IF NOT EXISTS webhook_delivery_hook_idx
    ON webhook_delivery_log (webhook_id, created_at DESC);
