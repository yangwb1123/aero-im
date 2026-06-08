-- Asynchronous personal-data export jobs (ROADMAP 方向四 — GDPR Art. 20).
--
-- The synchronous `GET /api/me/export` returns a capped snapshot (500 messages /
-- 200 blobs). A *complete* export of a heavy account can be large and slow, so it
-- runs as a background job: enqueue here, a worker assembles the full JSON archive
-- (ALL the participant's messages + uploaded files + profile), stores it as a blob
-- owned by the participant, and the participant downloads it via the existing
-- `/api/blobs/:id` endpoint. The download link is valid for 24h after completion;
-- after that the archive blob is garbage-collected via `blob_gc_queue`.
CREATE TABLE export_jobs (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    -- queued ⇒ processing ⇒ done | failed
    status         TEXT        NOT NULL DEFAULT 'queued'
                   CHECK (status IN ('queued', 'processing', 'done', 'failed')),
    -- The produced archive blob (NULL until done). SET NULL if the blob is purged.
    blob_id        UUID        NULL REFERENCES blobs(id) ON DELETE SET NULL,
    error          TEXT        NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at   TIMESTAMPTZ NULL
);

-- Worker claim path: oldest queued first.
CREATE INDEX export_jobs_queued ON export_jobs (created_at) WHERE status = 'queued';
CREATE INDEX export_jobs_participant ON export_jobs (participant_id);
