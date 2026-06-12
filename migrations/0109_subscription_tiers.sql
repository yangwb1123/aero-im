CREATE TABLE subscription_tiers (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    creator_id UUID NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    price_cents INT NOT NULL DEFAULT 0,
    position INT NOT NULL DEFAULT 0,
    benefits JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_subscription_tiers_creator ON subscription_tiers (creator_id, position);

ALTER TABLE creator_subscriptions ADD COLUMN tier_id UUID REFERENCES subscription_tiers(id) ON DELETE SET NULL;
