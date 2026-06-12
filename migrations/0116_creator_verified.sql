-- ROADMAP11 Feature 3: Creator Verified Badge
-- Adds verified status to participants (like Twitter/Instagram verification).
ALTER TABLE participants ADD COLUMN IF NOT EXISTS is_verified BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE participants ADD COLUMN IF NOT EXISTS verified_at TIMESTAMPTZ;
