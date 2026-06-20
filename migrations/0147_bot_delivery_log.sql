-- Bot subscription delivery log (方向三 — 开放平台 / observability).
--
-- The bot event dispatcher (`crates/aero-server/src/bot_dispatch.rs`, Wave9) fans
-- room events out to user bots' `bot_event_subscriptions.webhook_url`, reusing the
-- outbound webhook signing seam (`build_delivery` + `WebhookSender`). But it kept
-- NO delivery record: the existing `webhook_delivery_log` (migration 0085) is
-- FK-bound to `outgoing_webhooks(id)`, so a bot-subscription delivery — which lives
-- in the *different* `bot_event_subscriptions` table — cannot be logged there.
-- Bot delivery was therefore best-effort and invisible (warn-and-continue).
--
-- This table closes that observability gap: one row per dispatcher delivery
-- attempt, recording the subscription it was for, the event type, whether it
-- succeeded (2xx) or failed, the HTTP status (NULL on a transport-level error),
-- and a short error excerpt. It is a write-and-read log (no retry/DLQ lifecycle —
-- bot delivery stays fire-and-log like the other built-in bus bots); the
-- dispatcher never blocks on it (fail-open: a logging error is itself swallowed).
--
-- Scoped to a single subscription via FK; cascades with the subscription (and thus
-- the bot, and thus the participant) so a deleted subscription leaves no orphan
-- delivery rows. Purely additive — no existing table is altered.
CREATE TABLE IF NOT EXISTS bot_subscription_deliveries (
    id               UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    -- The subscription this delivery was for. Cascades on subscription delete.
    subscription_id  UUID        NOT NULL
                     REFERENCES bot_event_subscriptions(id) ON DELETE CASCADE,
    -- The owning bot's participant id, denormalized so the owner-scoped listing
    -- (GET /api/bots/:id/deliveries) is a single index probe without re-joining
    -- through bot_event_subscriptions (whose row may already be gone).
    bot_id           UUID        NOT NULL,
    -- The RoomEvent kind that was delivered (matches the subscription's
    -- event_type, e.g. "message", "reaction", "interaction").
    event_type       TEXT        NOT NULL,
    -- Outcome: delivered (2xx) | failed (non-2xx or transport error).
    status           TEXT        NOT NULL
                     CHECK (status IN ('delivered', 'failed')),
    -- HTTP status code of the attempt (NULL on a transport-level error).
    http_status      INTEGER     NULL,
    -- Short error excerpt on failure (non-2xx note or transport error). NULL on
    -- success.
    error            TEXT        NULL,
    -- How many send attempts this row represents (always 1 today — bot delivery is
    -- one-shot, not retried — but kept for shape parity / future retry).
    attempts         INTEGER     NOT NULL DEFAULT 1,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Per-subscription delivery history: scan by subscription, newest first.
CREATE INDEX IF NOT EXISTS bot_delivery_sub_idx
    ON bot_subscription_deliveries (subscription_id, created_at DESC);

-- Owner-scoped delivery history (GET /api/bots/:id/deliveries): scan by bot,
-- newest first.
CREATE INDEX IF NOT EXISTS bot_delivery_bot_idx
    ON bot_subscription_deliveries (bot_id, created_at DESC);
