-- 0099_gift_subscriptions.sql — gift creator subscriptions.
--
-- Extends `creator_subscriptions` so a third party (the `gifter`) can create a
-- subscription on behalf of a `recipient` (the subscriber). A gifted subscription
-- carries two new nullable columns:
--   - `gifter_id`       — the participant who paid for the gift (NULL for self-subs)
--   - `gift_expires_at` — when the gift lapses (NULL for self-subs)
--
-- Both are nullable so existing rows are unaffected and the upsert in the normal
-- subscribe path never needs to supply them.
ALTER TABLE creator_subscriptions ADD COLUMN gifter_id UUID REFERENCES participants(id);
ALTER TABLE creator_subscriptions ADD COLUMN gift_expires_at TIMESTAMPTZ;
CREATE INDEX creator_subscriptions_gifter_idx ON creator_subscriptions(gifter_id) WHERE gifter_id IS NOT NULL;
