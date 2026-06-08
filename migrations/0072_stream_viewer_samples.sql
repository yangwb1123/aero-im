-- Concurrent-viewer history sampling (peak / average concurrent viewers).
--
-- The live viewer COUNT is an ephemeral Redis sorted set
-- (`StreamViewerStore::count`, see crates/aero-storage/src/live_presence.rs) — it
-- only knows "right now". To report PEAK and AVERAGE concurrent viewers over a
-- stream's lifetime we periodically snapshot that count into this table (one row
-- per sample), then aggregate with MAX / AVG / COUNT.
--
-- A background sampler (run_viewer_sampler) writes one row every ~30s for each
-- currently-live stream (StreamRepo::list_live). `stream_id` is the stream's
-- ULID-as-UUID, matching the `streams.id` convention; no FK so a sampled row
-- survives an eventual hard-delete of the stream row (history is append-only).
CREATE TABLE IF NOT EXISTS stream_viewer_samples (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    stream_id   UUID        NOT NULL,
    sampled_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    viewers     INT         NOT NULL
);

-- Aggregates and any time-window scan are keyed on (stream_id, sampled_at).
CREATE INDEX IF NOT EXISTS stream_viewer_samples_stream_sampled_idx
    ON stream_viewer_samples (stream_id, sampled_at);
