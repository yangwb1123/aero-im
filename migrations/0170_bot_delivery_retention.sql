-- Bound durable bot-delivery terminal rows and per-attempt observability.
--
-- Retryable outbox rows (`pending` / `failed`) are deliberately excluded from
-- every retention index and sweep. `dead` rows remain available for the
-- operator-configured DLQ window before deletion; attempt history uses that
-- same operational window.

CREATE INDEX IF NOT EXISTS bot_subscription_delivery_outbox_delivered_retention_idx
    ON bot_subscription_delivery_outbox (completed_at)
    WHERE status = 'delivered';

-- `bot_subscription_delivery_outbox_dead_idx` from migration 0168 already
-- supports the status='dead' completed_at range delete.

CREATE INDEX IF NOT EXISTS bot_subscription_delivery_attempt_retention_idx
    ON bot_subscription_deliveries (created_at);
