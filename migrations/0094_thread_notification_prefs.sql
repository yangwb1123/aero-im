-- Per-thread notification level (feature: thread-notification-prefs).
-- Mirrors the per-channel notification_level approach but keyed on the root message.
CREATE TABLE thread_notification_prefs (
    participant_id UUID NOT NULL,
    root_message_id UUID NOT NULL,
    level TEXT NOT NULL CHECK (level IN ('all','mentions','none')) DEFAULT 'all',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (participant_id, root_message_id)
);
