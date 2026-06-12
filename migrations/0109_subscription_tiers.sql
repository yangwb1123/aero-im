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

-- NOTE: an earlier draft added `creator_subscriptions.tier_id` here, but that
-- collides with the column 0051 already defines (NOT NULL, -> creator_tiers) and
-- broke the migration chain on any fresh DB. No code references such a link from
-- creator_subscriptions to subscription_tiers (the tier CRUD touches only the
-- table above), so the column was unused dead weight — removed. Re-add via a new
-- migration with a distinct name (e.g. subscription_tier_id) if such a link is
-- ever needed. This file never applied successfully anywhere, so editing it in
-- place is safe.
