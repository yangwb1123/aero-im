-- 0035 Custom user profile fields (per-participant side table).
--
-- Optional, self-service profile metadata a participant fills in about
-- themselves — title, pronouns, timezone, phone, and a free-form status text.
-- Stored in a SIDE table keyed by `participant_id` so the core `participants`
-- row (and its `update_me` handler) stays untouched: a profile is created lazily
-- on first upsert and every field is nullable.
--
-- Idempotent: re-running is a no-op.
CREATE TABLE IF NOT EXISTS participant_profiles (
  participant_id uuid PRIMARY KEY,
  title text,
  pronouns text,
  timezone text,
  phone text,
  status_text text,
  updated_at timestamptz NOT NULL DEFAULT now()
);
