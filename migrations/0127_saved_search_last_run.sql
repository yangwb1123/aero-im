-- "New since I last ran this" tracking for saved searches (ROADMAP5 方向三).
-- Running a saved search stamps last_run_at; the previous value is the cursor a
-- `?only_new=true` run uses to return only matches created since the last run.
ALTER TABLE saved_searches ADD COLUMN IF NOT EXISTS last_run_at TIMESTAMPTZ;
