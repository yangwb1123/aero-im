-- 0121_custom_status.sql — add emoji + expiry to participant_profiles.
--
-- Extends the `participant_profiles` table (migration 0035) with two new
-- optional columns:
--   status_emoji    — the emoji shorthand the user picks alongside their status
--                     text (e.g. ":palm_tree:").
--   status_expires_at — optional auto-expiry: once this instant passes the
--                     status is treated as cleared by readers.
--
-- `status_text` already exists; `status_emoji` and `status_expires_at` are
-- new this migration.  Each ALTER is guarded (IF NOT EXISTS) so re-running is
-- a no-op.
ALTER TABLE participant_profiles ADD COLUMN IF NOT EXISTS status_emoji TEXT;
ALTER TABLE participant_profiles ADD COLUMN IF NOT EXISTS status_expires_at TIMESTAMPTZ;
