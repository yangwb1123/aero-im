-- Push token registry for mobile push notifications (ROADMAP 方向二).
--
-- Stores FCM (Android/Web) and APNs (iOS) device tokens per participant so the
-- notification dispatch layer can push alerts to offline devices. A physical
-- device registers its current token when the app starts; the (platform, token)
-- pair is globally unique (one device can only be owned by one participant at a
-- time — the UPSERT in register() handles token transfer on account switch).
CREATE TABLE push_tokens (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    -- 'fcm'  = Firebase Cloud Messaging (Android + Web)
    -- 'apns' = Apple Push Notification service (iOS + macOS)
    platform       TEXT        NOT NULL CHECK (platform IN ('fcm', 'apns')),
    token          TEXT        NOT NULL,
    registered_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- A token belongs to exactly one participant at a time; re-registration on a
    -- new account moves the token without leaving an orphan row.
    UNIQUE (platform, token)
);

CREATE INDEX push_tokens_participant_id_idx ON push_tokens (participant_id);
