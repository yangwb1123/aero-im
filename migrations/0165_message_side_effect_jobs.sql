-- 0165_message_side_effect_jobs.sql
--
-- Durable post-commit work for message mutations.  The business row, event
-- outbox row, and these jobs are committed together; a process crash after the
-- message commit therefore cannot permanently lose notifications, embedding,
-- or moderation work.

CREATE TABLE IF NOT EXISTS message_side_effect_jobs (
    id                UUID        PRIMARY KEY,
    message_id        UUID        NOT NULL,
    mutation_version  INTEGER     NOT NULL CHECK (mutation_version > 0),
    kind              TEXT        NOT NULL
        CHECK (kind IN ('notifications', 'embed', 'moderate')),
    attempts          INTEGER     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    available_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    claimed_at        TIMESTAMPTZ,
    completed_at      TIMESTAMPTZ,
    last_error        TEXT CHECK (
        last_error IS NULL OR char_length(last_error) <= 2048
    ),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (message_id, mutation_version, kind)
);

CREATE INDEX IF NOT EXISTS idx_message_side_effect_jobs_pending
    ON message_side_effect_jobs (available_at, created_at, id)
    WHERE completed_at IS NULL;

-- Reply bundles are another durable projection produced by the notification
-- side effect.  A deterministic delivery id makes replay after a worker crash
-- an idempotent no-op without changing legacy rows (NULL never conflicts).
ALTER TABLE notification_bundles
    ADD COLUMN IF NOT EXISTS delivery_id UUID;

CREATE UNIQUE INDEX IF NOT EXISTS notification_bundles_delivery_recipient_uidx
    ON notification_bundles (delivery_id, participant_id)
    WHERE delivery_id IS NOT NULL;
