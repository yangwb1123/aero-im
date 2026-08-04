-- Bound scheduled-stream text, future work, and list amplification. A workspace
-- row lock serializes the per-creator quota for both new and rolling writers.

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'scheduled_streams_status_domain'
           AND conrelid = 'scheduled_streams'::regclass
    ) THEN
        ALTER TABLE scheduled_streams
            ADD CONSTRAINT scheduled_streams_status_domain
            CHECK (status IN ('scheduled', 'live', 'canceled', 'ended'))
            NOT VALID;
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION scheduled_stream_resource_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    active_count bigint;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.status <> 'scheduled' THEN
            RAISE EXCEPTION 'scheduled-stream must start in scheduled status'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'scheduled_streams_initial_status';
        END IF;
        IF char_length(btrim(NEW.title)) NOT BETWEEN 1 AND 256
           OR (
               NEW.description IS NOT NULL
               AND char_length(NEW.description) > 4000
           )
        THEN
            RAISE EXCEPTION 'scheduled-stream text is outside allowed bounds'
                USING ERRCODE = '22023',
                      CONSTRAINT = 'scheduled_streams_text_bounds';
        END IF;
        PERFORM 1 FROM workspaces WHERE id = NEW.workspace_id FOR UPDATE;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'scheduled-stream workspace does not exist'
                USING ERRCODE = '23503',
                      CONSTRAINT = 'scheduled_streams_workspace_fk';
        END IF;
        IF NEW.scheduled_for <= clock_timestamp() THEN
            RAISE EXCEPTION 'scheduled_for must be in the future'
                USING ERRCODE = '22023',
                      CONSTRAINT = 'scheduled_streams_future_time';
        END IF;
        SELECT count(*)
          INTO active_count
          FROM scheduled_streams
         WHERE workspace_id = NEW.workspace_id
           AND created_by = NEW.created_by
           AND status = 'scheduled'
           AND scheduled_for > clock_timestamp();
        IF active_count >= 100 THEN
            RAISE EXCEPTION 'scheduled-stream creator limit reached'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'scheduled_streams_creator_limit';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.workspace_id IS DISTINCT FROM OLD.workspace_id
       OR NEW.room_id IS DISTINCT FROM OLD.room_id
       OR NEW.created_by IS DISTINCT FROM OLD.created_by
       OR NEW.scheduled_for IS DISTINCT FROM OLD.scheduled_for
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION 'scheduled-stream identity and schedule are immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'scheduled_streams_identity_immutable';
    END IF;
    -- Do not strand rows written before this migration.  Historical oversized
    -- text remains readable and lifecycle-mutable, but any explicit text edit
    -- must satisfy the new bounds.
    IF (
           NEW.title IS DISTINCT FROM OLD.title
           AND char_length(btrim(NEW.title)) NOT BETWEEN 1 AND 256
       )
       OR (
           NEW.description IS DISTINCT FROM OLD.description
           AND NEW.description IS NOT NULL
           AND char_length(NEW.description) > 4000
       )
    THEN
        RAISE EXCEPTION 'scheduled-stream text is outside allowed bounds'
            USING ERRCODE = '22023',
                  CONSTRAINT = 'scheduled_streams_text_bounds';
    END IF;
    IF NOT (
           (OLD.status = 'scheduled'
            AND NEW.status IN ('scheduled', 'live', 'canceled', 'ended'))
        OR (OLD.status = 'live' AND NEW.status IN ('live', 'ended'))
        OR (OLD.status = 'canceled' AND NEW.status = 'canceled')
        OR (OLD.status = 'ended' AND NEW.status = 'ended')
    ) THEN
        RAISE EXCEPTION 'scheduled-stream lifecycle transition is invalid'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'scheduled_streams_status_transition';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS scheduled_stream_resource_fence_trigger
    ON scheduled_streams;
CREATE TRIGGER scheduled_stream_resource_fence_trigger
    BEFORE INSERT OR UPDATE
    ON scheduled_streams
    FOR EACH ROW
    EXECUTE FUNCTION scheduled_stream_resource_fence();

COMMENT ON FUNCTION scheduled_stream_resource_fence() IS
    'Enforces the scheduled-stream status machine, bounds text/future time, and caps per-creator rows';
