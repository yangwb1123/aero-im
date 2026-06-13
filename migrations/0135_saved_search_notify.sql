-- Opt-in periodic monitoring for saved searches (ROADMAP5 方向三).
--
-- A saved search already tracks last_run_at (mig 0127) and supports an on-demand
-- "new since I last ran this" delta. This flag turns a saved search into a
-- standing monitor: when set, a background dispatcher periodically re-runs the
-- query and notifies the owner of matches newer than last_run_at. Default false
-- (a saved search stays a passive bookmark unless the owner opts in).
--
-- Idempotent: re-running is a no-op.
ALTER TABLE saved_searches
    ADD COLUMN IF NOT EXISTS notify_new BOOLEAN NOT NULL DEFAULT false;

-- The dispatcher scans only monitored searches; a partial index keeps that scan
-- cheap as the saved-search table grows (the monitored subset is small).
CREATE INDEX IF NOT EXISTS saved_searches_monitored_idx
    ON saved_searches (id) WHERE notify_new;
