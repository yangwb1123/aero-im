-- Make creator goals finite, owner-bound stream resources. The stream row is
-- the serialization point for the per-stream active quota, including raw SQL
-- and rolling-upgrade writers.

CREATE OR REPLACE FUNCTION goal_write_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    actual_owner uuid;
    actual_room uuid;
    locked_owner uuid;
    locked_room uuid;
    active_count bigint;
BEGIN
    IF TG_OP = 'INSERT' THEN
        -- Resolve the authorization route without taking the aggregate lock.
        -- Room membership governance takes workspace/room/identity locks, so it
        -- must run before the stream row lock to match the rest of live ingest.
        SELECT owner_id, room_id
          INTO actual_owner, actual_room
          FROM streams
         WHERE id = NEW.stream_id;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'goal stream does not exist'
                USING ERRCODE = '23503',
                      CONSTRAINT = 'goals_stream_fkey';
        END IF;
        IF actual_owner IS DISTINCT FROM NEW.creator_id THEN
            RAISE EXCEPTION 'goal creator must own the stream'
                USING ERRCODE = '42501',
                      CONSTRAINT = 'goals_stream_owner';
        END IF;
        IF actual_room IS NOT NULL THEN
            IF NOT aero_effective_room_access(
                actual_room,
                NEW.creator_id,
                NULL
            ) THEN
                RAISE EXCEPTION 'goal creator cannot access the stream room'
                    USING ERRCODE = '42501',
                          CONSTRAINT = 'goals_creator_effective_access';
            END IF;
        ELSE
            PERFORM 1
              FROM participants
             WHERE id = NEW.creator_id
               AND deleted_at IS NULL
               FOR SHARE;
            IF NOT FOUND THEN
                RAISE EXCEPTION 'goal creator is not active'
                    USING ERRCODE = '42501',
                          CONSTRAINT = 'goals_creator_effective_access';
            END IF;
        END IF;

        SELECT owner_id, room_id
          INTO locked_owner, locked_room
          FROM streams
         WHERE id = NEW.stream_id
           FOR UPDATE;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'goal stream does not exist'
                USING ERRCODE = '23503',
                      CONSTRAINT = 'goals_stream_fkey';
        END IF;
        IF locked_owner IS DISTINCT FROM actual_owner
           OR locked_room IS DISTINCT FROM actual_room THEN
            RAISE EXCEPTION 'goal stream authorization scope changed'
                USING ERRCODE = '42501',
                      CONSTRAINT = 'goals_stream_scope_changed';
        END IF;
        IF char_length(btrim(NEW.title)) NOT BETWEEN 1 AND 120
           OR (NEW.description IS NOT NULL AND char_length(NEW.description) > 1000)
           OR NEW.target < 1
           OR NEW.current_tally < 0
           OR (NEW.status = 'active' AND NEW.current_tally >= NEW.target)
           OR (NEW.status = 'reached' AND NEW.current_tally < NEW.target)
           OR (
               NEW.expires_at IS NOT NULL
               AND NEW.expires_at <= clock_timestamp()
           )
        THEN
            RAISE EXCEPTION 'goal fields are outside their allowed bounds'
                USING ERRCODE = '22023',
                      CONSTRAINT = 'goals_input_bounds';
        END IF;

        SELECT count(*)
          INTO active_count
         FROM goals
         WHERE stream_id = NEW.stream_id
           AND status = 'active'
           AND (expires_at IS NULL OR expires_at > clock_timestamp());
        IF active_count >= 10 THEN
            RAISE EXCEPTION 'active goal limit reached'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'goals_active_limit';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.stream_id IS DISTINCT FROM OLD.stream_id
       OR NEW.creator_id IS DISTINCT FROM OLD.creator_id
       OR NEW.metric_type IS DISTINCT FROM OLD.metric_type
       OR NEW.target IS DISTINCT FROM OLD.target
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
       OR NEW.expires_at IS DISTINCT FROM OLD.expires_at
    THEN
        RAISE EXCEPTION 'goal identity and definition are immutable'
            USING ERRCODE = '23514',
                      CONSTRAINT = 'goals_identity_immutable';
    END IF;
    -- Rolling compatibility: historical rows may predate these text bounds.
    -- Cursor/lifecycle/tally writes must remain possible until a client
    -- explicitly edits the legacy field.
    IF (
           NEW.title IS DISTINCT FROM OLD.title
           AND char_length(btrim(NEW.title)) NOT BETWEEN 1 AND 120
       )
       OR (
           NEW.description IS DISTINCT FROM OLD.description
           AND NEW.description IS NOT NULL
           AND char_length(NEW.description) > 1000
       )
    THEN
        RAISE EXCEPTION 'goal fields are outside their allowed bounds'
            USING ERRCODE = '22023',
                  CONSTRAINT = 'goals_input_bounds';
    END IF;
    IF NEW.current_tally < OLD.current_tally OR NEW.current_tally < 0 THEN
        RAISE EXCEPTION 'goal tally cannot decrease'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'goals_tally_monotonic';
    END IF;
    IF NEW.current_tally IS DISTINCT FROM OLD.current_tally
       AND NEW.expires_at IS NOT NULL
       AND NEW.expires_at <= clock_timestamp()
    THEN
        RAISE EXCEPTION 'expired goals cannot accept progress'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'goals_expired_progress';
    END IF;
    IF (OLD.status = 'cancelled' AND NEW.status IS DISTINCT FROM OLD.status)
       OR (OLD.status = 'reached' AND NEW.status = 'active')
    THEN
        RAISE EXCEPTION 'goal terminal status cannot return to active'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'goals_terminal_status';
    END IF;
    IF (NEW.status = 'active' AND NEW.current_tally >= NEW.target)
       OR (NEW.status = 'reached' AND NEW.current_tally < NEW.target)
    THEN
        RAISE EXCEPTION 'goal status does not match its tally'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'goals_status_tally';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS goal_write_fence_trigger ON goals;
CREATE TRIGGER goal_write_fence_trigger
    BEFORE INSERT OR UPDATE
    ON goals
    FOR EACH ROW
    EXECUTE FUNCTION goal_write_fence();

ALTER TABLE goal_events
    DROP CONSTRAINT IF EXISTS goal_events_positive_delta;
ALTER TABLE goal_events
    ADD CONSTRAINT goal_events_positive_delta
    CHECK (delta > 0) NOT VALID;

CREATE OR REPLACE FUNCTION goal_validate_legacy_audit()
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    invalid_event uuid;
    invalid_goal uuid;
    invalid_tally bigint;
    invalid_total numeric;
BEGIN
    SELECT id
      INTO invalid_event
      FROM goal_events
     WHERE delta <= 0
     ORDER BY id
     LIMIT 1;
    IF invalid_event IS NOT NULL THEN
        RAISE EXCEPTION
            'legacy goal event % has a non-positive delta; repair it explicitly before migrating',
            invalid_event
            USING ERRCODE = '23514',
                  CONSTRAINT = 'goal_events_legacy_invalid_delta';
    END IF;

    SELECT id, current_tally
      INTO invalid_goal, invalid_tally
      FROM goals
     WHERE current_tally < 0
     ORDER BY id
     LIMIT 1;
    IF invalid_goal IS NOT NULL THEN
        RAISE EXCEPTION
            'legacy goal % has negative tally %; repair it explicitly before migrating',
            invalid_goal,
            invalid_tally
            USING ERRCODE = '23514',
                  CONSTRAINT = 'goals_legacy_negative_tally';
    END IF;

    SELECT goal.id,
           goal.current_tally,
           COALESCE(SUM(event.delta::numeric), 0::numeric)
      INTO invalid_goal, invalid_tally, invalid_total
      FROM goals AS goal
      LEFT JOIN goal_events AS event ON event.goal_id = goal.id
     GROUP BY goal.id, goal.current_tally
    HAVING COALESCE(SUM(event.delta::numeric), 0::numeric)
           > goal.current_tally::numeric
     ORDER BY goal.id
     LIMIT 1;
    IF invalid_goal IS NOT NULL THEN
        RAISE EXCEPTION
            'legacy goal % has tally % below immutable event total %; repair it explicitly before migrating',
            invalid_goal,
            invalid_tally,
            invalid_total
            USING ERRCODE = '23514',
                  CONSTRAINT = 'goal_events_legacy_total_exceeds_tally';
    END IF;
END
$$;

SELECT goal_validate_legacy_audit();

-- Older rolling writers may have advanced the tally without its event. Repair
-- only the unambiguous positive gap before enforcing commit-time consistency.
WITH event_totals AS (
    SELECT goal_id, SUM(delta::numeric) AS total
      FROM goal_events
     GROUP BY goal_id
)
INSERT INTO goal_events (id, goal_id, delta, created_at)
SELECT gen_random_uuid(),
       goal.id,
       (
           goal.current_tally::numeric
           - COALESCE(event_totals.total, 0::numeric)
       )::bigint,
       goal.created_at
  FROM goals AS goal
 LEFT JOIN event_totals ON event_totals.goal_id = goal.id
 WHERE goal.current_tally::numeric > COALESCE(event_totals.total, 0::numeric);

DO $$
DECLARE
    inconsistent_goal uuid;
BEGIN
    SELECT goal.id
      INTO inconsistent_goal
      FROM goals AS goal
      LEFT JOIN goal_events AS event ON event.goal_id = goal.id
     GROUP BY goal.id, goal.current_tally
    HAVING COALESCE(SUM(event.delta::numeric), 0::numeric)
           <> goal.current_tally::numeric
     ORDER BY goal.id
     LIMIT 1;
    IF inconsistent_goal IS NOT NULL THEN
        RAISE EXCEPTION
            'goal audit backfill did not converge for goal %',
            inconsistent_goal
            USING ERRCODE = '23514',
                  CONSTRAINT = 'goal_audit_backfill_incomplete';
    END IF;
END
$$;

ALTER TABLE goal_events
    VALIDATE CONSTRAINT goal_events_positive_delta;

CREATE OR REPLACE FUNCTION goal_event_write_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP <> 'INSERT' THEN
        RAISE EXCEPTION 'goal events are append-only'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'goal_events_append_only';
    END IF;
    IF NEW.delta <= 0 THEN
        RAISE EXCEPTION 'goal event delta must be positive'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'goal_events_positive_delta';
    END IF;
    PERFORM 1 FROM goals WHERE id = NEW.goal_id FOR KEY SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'goal event requires a live goal row'
            USING ERRCODE = '23503',
                  CONSTRAINT = 'goal_events_goal_fkey';
    END IF;
    -- Audit time is database-owned; callers cannot backdate new contributions.
    NEW.created_at := clock_timestamp();
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS goal_event_write_fence_trigger ON goal_events;
CREATE TRIGGER goal_event_write_fence_trigger
    BEFORE INSERT OR UPDATE OR DELETE
    ON goal_events
    FOR EACH ROW
    EXECUTE FUNCTION goal_event_write_fence();

CREATE OR REPLACE FUNCTION goal_audit_consistency_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    guarded_goal uuid;
    tally numeric;
    event_total numeric;
BEGIN
    IF TG_TABLE_NAME = 'goals' THEN
        guarded_goal := NEW.id;
    ELSE
        guarded_goal := NEW.goal_id;
    END IF;

    SELECT current_tally::numeric
      INTO tally
      FROM goals
     WHERE id = guarded_goal;
    IF NOT FOUND THEN
        -- Goal rows intentionally may be pruned while their audit survives.
        RETURN NULL;
    END IF;

    SELECT COALESCE(SUM(delta::numeric), 0::numeric)
      INTO event_total
      FROM goal_events
     WHERE goal_id = guarded_goal;
    IF event_total <> tally THEN
        RAISE EXCEPTION
            'goal tally % does not match immutable event total % for goal %',
            tally,
            event_total,
            guarded_goal
            USING ERRCODE = '23514',
                  CONSTRAINT = 'goal_audit_tally_consistency';
    END IF;
    RETURN NULL;
END
$$;

DROP TRIGGER IF EXISTS goal_tally_audit_consistency ON goals;
CREATE CONSTRAINT TRIGGER goal_tally_audit_consistency
    AFTER INSERT OR UPDATE OF current_tally
    ON goals
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION goal_audit_consistency_guard();

DROP TRIGGER IF EXISTS goal_event_audit_consistency ON goal_events;
CREATE CONSTRAINT TRIGGER goal_event_audit_consistency
    AFTER INSERT
    ON goal_events
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION goal_audit_consistency_guard();

COMMENT ON FUNCTION goal_write_fence() IS
    'Locks effective creator access before the canonical stream, verifies bounds, and caps active goals at ten';

COMMENT ON FUNCTION goal_event_write_fence() IS
    'Makes goal contribution events database-timestamped and append-only';

COMMENT ON FUNCTION goal_audit_consistency_guard() IS
    'At commit, requires each retained goal tally to equal its immutable event total';

COMMENT ON FUNCTION goal_validate_legacy_audit() IS
    'Fail-closed migration diagnostic for non-positive deltas, negative tallies, or event totals above their goal tally';
