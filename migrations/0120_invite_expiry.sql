-- 0120_invite_expiry.sql — ensure the invitations table has an expires_at column.
--
-- The `invitations` table (migration 0017) already includes `expires_at
-- TIMESTAMPTZ` in the initial schema. This migration is intentionally a
-- graceful no-op: if the column is missing on an older schema variant it is
-- added; if it already exists the ALTER is skipped. Safe to re-run.
ALTER TABLE invitations ADD COLUMN IF NOT EXISTS expires_at TIMESTAMPTZ;
