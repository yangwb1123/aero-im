-- Durable, idempotent saved-search monitor delivery.
--
-- `last_run_at` is also advanced by the user-facing "run saved search" route,
-- so it cannot safely serve as the background monitor's cursor: a manual run
-- could otherwise skip proactive notifications.  The monitor gets an
-- independent composite cursor.  The UUID tiebreaker is load-bearing because
-- several messages may share the same `created_at`.  A fixed floor records the
-- enable-time baseline; workers may safely look behind the moving cursor without
-- backfilling pre-enable history.
--
-- Existing enabled monitors inherit their old timestamp cursor.  A NULL
-- message id intentionally preserves the legacy strict `created_at > cursor`
-- boundary.  Newly-enabled monitors initialize their baseline on their first
-- worker run.

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = 'saved_searches'
           AND column_name = 'monitor_cursor_at'
    ) THEN
        ALTER TABLE saved_searches
            ADD COLUMN monitor_cursor_at TIMESTAMPTZ;

        ALTER TABLE saved_searches
            ADD COLUMN monitor_floor_at TIMESTAMPTZ;

        UPDATE saved_searches
           SET monitor_cursor_at = last_run_at,
               monitor_floor_at = last_run_at
         WHERE notify_new
           AND last_run_at IS NOT NULL;
    END IF;
END
$$;

ALTER TABLE saved_searches
    ADD COLUMN IF NOT EXISTS monitor_cursor_message_id UUID;

ALTER TABLE saved_searches
    ADD COLUMN IF NOT EXISTS monitor_floor_at TIMESTAMPTZ;

ALTER TABLE saved_searches
    ADD COLUMN IF NOT EXISTS monitor_floor_message_id UUID;

-- Accommodate a database that briefly saw an earlier development version of
-- this migration: preserve its cursor as the no-backfill floor.
UPDATE saved_searches
   SET monitor_floor_at = monitor_cursor_at
 WHERE notify_new
   AND monitor_floor_at IS NULL
   AND monitor_cursor_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS saved_search_monitor_deliveries (
    saved_search_id UUID        NOT NULL
                                REFERENCES saved_searches(id) ON DELETE CASCADE,
    message_id      UUID        NOT NULL
                                REFERENCES messages(id) ON DELETE CASCADE,
    participant_id  UUID        NOT NULL
                                REFERENCES participants(id) ON DELETE CASCADE,
    delivery_id     UUID        NOT NULL,
    delivered_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (saved_search_id, message_id, participant_id),
    UNIQUE (delivery_id, participant_id)
);

CREATE INDEX IF NOT EXISTS saved_search_monitor_deliveries_retention_idx
    ON saved_search_monitor_deliveries (saved_search_id, delivered_at);

COMMENT ON COLUMN saved_searches.monitor_cursor_at IS
    'Background notification cursor timestamp; independent of manual saved-search runs.';
COMMENT ON COLUMN saved_searches.monitor_cursor_message_id IS
    'Message UUID tiebreaker at monitor_cursor_at; NULL means a strict timestamp boundary.';
COMMENT ON COLUMN saved_searches.monitor_floor_at IS
    'Fixed monitor enable-time floor; bounded cursor lookback never crosses it.';
COMMENT ON COLUMN saved_searches.monitor_floor_message_id IS
    'Floor UUID tiebreaker; NULL preserves the strict legacy timestamp boundary.';
