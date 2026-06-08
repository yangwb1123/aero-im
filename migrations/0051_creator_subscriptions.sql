-- 0051 Creator subscriptions / membership tiers (recurring creator support).
--
-- Viewers subscribe to a creator (a participant) at a named tier — like Twitch
-- subs / YouTube memberships. Creators define tiers (name, monthly price in
-- cents, optional perks text); a viewer subscribes at one tier and may later
-- unsubscribe (the row is kept but flagged inactive). This is the recurring
-- complement to the existing one-off live gifts.
--
-- `creator_subscriptions` has at most one row per (creator, subscriber): a
-- re-subscribe upserts the tier and re-activates the row, so the unique
-- constraint is the upsert conflict target. Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS creator_tiers (
  id uuid PRIMARY KEY,
  creator_id uuid NOT NULL,
  name text NOT NULL,
  price_cents int NOT NULL,
  perks text,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS creator_tiers_creator_idx ON creator_tiers (creator_id);

CREATE TABLE IF NOT EXISTS creator_subscriptions (
  id uuid PRIMARY KEY,
  creator_id uuid NOT NULL,
  subscriber_id uuid NOT NULL,
  tier_id uuid NOT NULL,
  active boolean NOT NULL DEFAULT true,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (creator_id, subscriber_id)
);
CREATE INDEX IF NOT EXISTS creator_subscriptions_creator_idx ON creator_subscriptions (creator_id);
CREATE INDEX IF NOT EXISTS creator_subscriptions_subscriber_idx ON creator_subscriptions (subscriber_id);
