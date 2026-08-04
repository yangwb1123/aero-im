-- Distinguish mandatory erasure/expiry from reference-aware attachment cleanup.
--
-- Historical queue rows predate this distinction and were always intended to
-- delete their object, so backfill them as forced. New ordinary message cleanup
-- rows opt into `force_delete = false` explicitly.
ALTER TABLE blob_gc_queue
    ADD COLUMN IF NOT EXISTS force_delete BOOLEAN;

UPDATE blob_gc_queue
   SET force_delete = TRUE
 WHERE force_delete IS NULL;

ALTER TABLE blob_gc_queue
    ALTER COLUMN force_delete SET NOT NULL,
    ALTER COLUMN force_delete SET DEFAULT FALSE;

COMMENT ON COLUMN blob_gc_queue.force_delete IS
    'true for GDPR/expiry/reservation erasure; false for reference-aware message attachment cleanup';
