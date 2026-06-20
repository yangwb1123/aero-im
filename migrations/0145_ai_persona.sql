-- Persistent cross-room AI user profile (持久跨房 AI 用户画像).
--
-- The AI memory layer keeps only a short, per-(participant, room) rolling
-- conversation in Redis (`ai_context`). There was no DURABLE, cross-room view of
-- a participant — the topics they engage with, their stated preferences, a short
-- summary — that an answer / recommendation path could read to personalise.
--
-- This table holds exactly one optional profile row per participant. It is a
-- PRIVACY-SENSITIVE store, so it ships with strict gates baked into the design:
--
--   * OPT-IN: a row is only ever written when the operator sets
--     `AERO_AI_CROSS_ROOM_PROFILE` (default OFF) — the extraction entrypoint is a
--     no-op otherwise, so a fresh deploy never accumulates profiles.
--   * GDPR-ERASABLE: keyed by participant_id; the right-to-erasure path
--     (`participant.rs::delete_participant`) DELETEs the row explicitly. The FK is
--     `ON DELETE CASCADE`, but erasure TOMBSTONES the participant (an UPDATE), so
--     the cascade never fires — the explicit DELETE is the load-bearing one. This
--     mirrors the project invariant for every participant-keyed PII table.
--   * TRANSPARENT: `topics` / `preferences` / `summary` are plain readable data,
--     not opaque vectors, so the subject can be shown exactly what is stored.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS participant_ai_profiles (
    -- One profile per participant. CASCADE is a backstop; the explicit erasure
    -- DELETE in delete_participant is what actually removes it (tombstone, not
    -- hard-delete, so the cascade is never reached).
    participant_id UUID        PRIMARY KEY REFERENCES participants(id) ON DELETE CASCADE,
    -- The workspace the profile was last extracted within (per-tenant scoping for
    -- analytics / future per-workspace purge). NULLable for safety on backfill.
    workspace_id   UUID,
    -- Extracted recurring topics — a JSON array of short strings, e.g.
    -- ["rust", "postgres", "oncall"]. Defaults to an empty array.
    topics         JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- Extracted stated preferences — a JSON object of free-form key/value hints,
    -- e.g. {"tone": "concise", "language": "zh"}. Defaults to an empty object.
    preferences    JSONB       NOT NULL DEFAULT '{}'::jsonb,
    -- A short human-readable summary of the participant for the LLM to ground on.
    summary        TEXT        NOT NULL DEFAULT '',
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Per-tenant scan (future per-workspace analytics / purge).
CREATE INDEX IF NOT EXISTS participant_ai_profiles_ws_idx
    ON participant_ai_profiles (workspace_id);
