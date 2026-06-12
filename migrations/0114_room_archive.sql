-- ROADMAP11 Feature 1: Room/Channel Archiving
-- Adds archived_at timestamp to rooms. is_archived already exists from 0012_channels.sql.
ALTER TABLE rooms ADD COLUMN IF NOT EXISTS archived_at TIMESTAMPTZ;
