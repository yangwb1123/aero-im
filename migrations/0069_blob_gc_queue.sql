-- Blob garbage-collection queue.
--
-- When a participant's account is deleted the server enqueues all blobs they
-- own so a background job can delete the actual storage objects (GDPR
-- right-to-erasure for binary content). The row is deleted after the store
-- object has been successfully removed.
--
-- `blob_id` intentionally uses ON DELETE CASCADE so that a hard-delete of a
-- blob row (e.g. admin purge) automatically removes the pending GC entry.

CREATE TABLE IF NOT EXISTS blob_gc_queue (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    blob_id      UUID        NOT NULL REFERENCES blobs(id) ON DELETE CASCADE,
    enqueued_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (blob_id)
);

CREATE INDEX IF NOT EXISTS blob_gc_queue_enqueued_at ON blob_gc_queue (enqueued_at ASC);
