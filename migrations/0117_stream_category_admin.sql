-- ROADMAP11 Feature 4: Stream Category Tagging — admin creation endpoint
-- stream_categories and stream_category_assignments already exist from migration 0050.
-- This migration adds the admin-created flag to distinguish admin-seeded vs dynamically added.
ALTER TABLE stream_categories ADD COLUMN IF NOT EXISTS created_by UUID REFERENCES participants(id) ON DELETE SET NULL;
ALTER TABLE stream_categories ADD COLUMN IF NOT EXISTS is_admin_created BOOLEAN NOT NULL DEFAULT FALSE;
