-- Anchor each materialized Canvas snapshot to the durable operation log and
-- make operation submission safely retryable.
--
-- `snapshot_op_seq` is the highest operation already represented by `blocks`.
-- Existing snapshots predate compaction, so they start at zero and clients
-- replay the complete existing log. Snapshot writers only advance this value
-- while holding the same canvas-row lock used by op allocation.
ALTER TABLE channel_canvases
    ADD COLUMN IF NOT EXISTS snapshot_op_seq BIGINT NOT NULL DEFAULT 0;

DO $$
BEGIN
    ALTER TABLE channel_canvases
        ADD CONSTRAINT channel_canvases_snapshot_op_seq_valid
        CHECK (
            snapshot_op_seq >= 0
            AND snapshot_op_seq <= op_seq
        );
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

-- Older rows did not carry a client id. Their immutable server id is a safe,
-- already-unique backfill. New writes use the caller-supplied UUID and dedupe
-- within one (canvas, participant) namespace.
ALTER TABLE canvas_ops
    ADD COLUMN IF NOT EXISTS client_op_id UUID;

-- Migration-first rolling deploys keep previous binaries serving while this
-- DDL is applied. Those binaries omit client_op_id, so install the fallback
-- before tightening the column. The immutable server operation id is already
-- unique and gives each legacy insert a stable, non-colliding key.
CREATE OR REPLACE FUNCTION fill_legacy_canvas_op_client_id()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.client_op_id IS NULL THEN
        NEW.client_op_id := NEW.id;
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS canvas_ops_fill_legacy_client_id ON canvas_ops;
CREATE TRIGGER canvas_ops_fill_legacy_client_id
    BEFORE INSERT ON canvas_ops
    FOR EACH ROW
    EXECUTE FUNCTION fill_legacy_canvas_op_client_id();

UPDATE canvas_ops
   SET client_op_id = id
 WHERE client_op_id IS NULL;

ALTER TABLE canvas_ops
    ALTER COLUMN client_op_id SET NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS canvas_ops_client_op_id_uq
    ON canvas_ops (canvas_id, author_id, client_op_id);
