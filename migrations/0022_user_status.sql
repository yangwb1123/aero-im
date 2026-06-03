-- 0022_user_status.sql — durable user-set custom status + presence preference.
--
-- This is the DURABLE counterpart to the ephemeral Redis `PresenceStore`
-- (online-in-room heartbeat, `crates/aero-storage/src/presence.rs`). A user sets
-- a custom status — an emoji shorthand (e.g. `:palm_tree:`), free text (e.g.
-- "On vacation"), and an optional auto-expiry — plus a coarse presence
-- preference (active/away). Other users read it on a profile / member list.
--
-- One row per participant (keyed on participant_id) so setting a status is an
-- idempotent upsert; clearing deletes the row. Purely additive — no existing
-- table is touched. Idempotent: safe to re-run (IF NOT EXISTS).

CREATE TABLE IF NOT EXISTS user_status (
    participant_id UUID        PRIMARY KEY REFERENCES participants(id) ON DELETE CASCADE,
    -- Emoji shorthand (`:palm_tree:`) and free text are both optional: a user
    -- may set only a presence preference with no custom status decoration.
    emoji          TEXT,
    text           TEXT,
    -- Coarse presence preference, a lowercase token ('active'/'away'; 'offline'
    -- accepted for completeness). NOT NULL with a sensible default so a row
    -- always carries a presence even when emoji/text are absent.
    presence       TEXT        NOT NULL DEFAULT 'active',
    -- Optional auto-expiry: once this instant passes, the custom status
    -- (emoji/text) is treated as CLEARED by readers (the presence preference is
    -- kept). NULL means the custom status never expires on its own.
    expires_at     TIMESTAMPTZ,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
