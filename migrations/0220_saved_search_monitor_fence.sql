-- Bound persisted saved-search inputs and prevent standing monitors from being
-- enabled outside the owner's current effective workspace membership.
--
-- Historical oversized rows remain readable and their cursors remain movable;
-- the trigger applies text bounds only to inserts or explicit text edits.

ALTER TABLE saved_searches
    ADD COLUMN IF NOT EXISTS monitor_enabled_at TIMESTAMPTZ;

UPDATE saved_searches
   SET monitor_enabled_at = COALESCE(
           monitor_cursor_at,
           last_run_at,
           created_at
       )
 WHERE notify_new
   AND monitor_enabled_at IS NULL;

CREATE OR REPLACE FUNCTION saved_search_write_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        NEW.monitor_enabled_at := CASE
            WHEN NEW.notify_new THEN clock_timestamp()
            ELSE NULL
        END;
        IF char_length(btrim(NEW.name)) NOT BETWEEN 1 AND 256 THEN
            RAISE EXCEPTION 'saved-search name must contain 1..256 characters'
                USING ERRCODE = '22023',
                      CONSTRAINT = 'saved_search_name_bounds';
        END IF;
        IF char_length(btrim(NEW.query)) NOT BETWEEN 1 AND 2000 THEN
            RAISE EXCEPTION 'saved-search query must contain 1..2000 characters'
                USING ERRCODE = '22023',
                      CONSTRAINT = 'saved_search_query_bounds';
        END IF;
        IF NOT aero_effective_workspace_access(
            NEW.workspace_id,
            NEW.participant_id
        ) THEN
            RAISE EXCEPTION 'saved-search owner lacks effective workspace access'
                USING ERRCODE = '42501',
                      CONSTRAINT = 'saved_search_effective_workspace_access';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.notify_new AND (
        OLD.notify_new IS NOT TRUE
        OR OLD.monitor_enabled_at IS NULL
    ) THEN
        NEW.monitor_enabled_at := clock_timestamp();
    ELSIF NOT NEW.notify_new THEN
        NEW.monitor_enabled_at := NULL;
    ELSIF NEW.monitor_enabled_at IS DISTINCT FROM OLD.monitor_enabled_at THEN
        RAISE EXCEPTION 'monitor enable time is database-managed'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'saved_search_monitor_enabled_at_immutable';
    END IF;

    IF NEW.name IS DISTINCT FROM OLD.name
       AND char_length(btrim(NEW.name)) NOT BETWEEN 1 AND 256
    THEN
        RAISE EXCEPTION 'saved-search name must contain 1..256 characters'
            USING ERRCODE = '22023',
                  CONSTRAINT = 'saved_search_name_bounds';
    END IF;
    IF NEW.query IS DISTINCT FROM OLD.query
       AND char_length(btrim(NEW.query)) NOT BETWEEN 1 AND 2000
    THEN
        RAISE EXCEPTION 'saved-search query must contain 1..2000 characters'
            USING ERRCODE = '22023',
                  CONSTRAINT = 'saved_search_query_bounds';
    END IF;

    IF (
        NEW.workspace_id IS DISTINCT FROM OLD.workspace_id
        OR NEW.participant_id IS DISTINCT FROM OLD.participant_id
        OR (NEW.notify_new AND OLD.notify_new IS NOT TRUE)
    ) AND NOT aero_effective_workspace_access(
        NEW.workspace_id,
        NEW.participant_id
    ) THEN
        RAISE EXCEPTION 'saved-search owner lacks effective workspace access'
            USING ERRCODE = '42501',
                  CONSTRAINT = 'saved_search_effective_workspace_access';
    END IF;

    RETURN NEW;
END
$$;

-- UPDATE obtains target-row locks before row triggers run.  Enter the global
-- membership-governance order at statement start so direct/rolling writers
-- cannot invert saved-search -> workspace against the monitor dispatcher's
-- global -> workspace -> saved-search order.
DROP TRIGGER IF EXISTS saved_search_governance_statement_fence
    ON saved_searches;
CREATE TRIGGER saved_search_governance_statement_fence
    BEFORE INSERT OR UPDATE OF participant_id, workspace_id, notify_new
    ON saved_searches
    FOR EACH STATEMENT
    EXECUTE FUNCTION membership_governance_statement_fence();

DROP TRIGGER IF EXISTS saved_search_write_fence_trigger
    ON saved_searches;
CREATE TRIGGER saved_search_write_fence_trigger
    BEFORE INSERT OR UPDATE
    ON saved_searches
    FOR EACH ROW
    EXECUTE FUNCTION saved_search_write_fence();

CREATE INDEX IF NOT EXISTS saved_searches_owner_page_idx
    ON saved_searches (
        participant_id,
        workspace_id,
        created_at DESC,
        id DESC
    );

COMMENT ON COLUMN saved_searches.monitor_enabled_at IS
    'Database-managed monitor enable generation used to freeze each dispatcher tick scan';

COMMENT ON FUNCTION saved_search_write_fence() IS
    'Bounds saved-search text and enforces effective workspace access for ownership and monitor enablement';
