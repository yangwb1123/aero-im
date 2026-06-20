-- Bot event subscriptions (方向三 — 开放平台).
-- Each row is one event type a bot has subscribed to, with optional filters.

CREATE TABLE IF NOT EXISTS bot_event_subscriptions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    bot_id          UUID NOT NULL REFERENCES bots(id) ON DELETE CASCADE,
    -- e.g. "message", "reaction", "member_join", "interaction"
    event_type      TEXT NOT NULL,
    -- Optional JSON filter: {"room_id": "...", "workspace_id": "...", "action_id": "..."}
    filters         JSONB DEFAULT '{}',
    -- Delivery URL (webhook). NULL means the bot receives the event via WS.
    webhook_url     TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS bot_sub_bot_idx ON bot_event_subscriptions (bot_id);
CREATE INDEX IF NOT EXISTS bot_sub_event_type_idx ON bot_event_subscriptions (event_type);
