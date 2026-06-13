-- Tiered rollup for the stream_viewer_samples 30s firehose (ROADMAP5 方向四).
--
-- stream_viewer_samples (migration 0072) is an append-only 30s-granularity
-- firehose with NO retention: a 4h stream is ~480 rows, at scale millions/day,
-- and nothing ever deleted it — the highest-cardinality append-only table in
-- the schema. This rollup aggregates raw samples into per-minute buckets so the
-- raw rows can be downsampled (deleted past a short retention window) while the
-- coarse aggregate history is preserved cheaply (1 row / stream / minute).
--
-- The composite PRIMARY KEY (stream_id, bucket_start) doubles as the ON CONFLICT
-- arbiter for the idempotent INSERT … SELECT … GROUP BY rollup, so re-running the
-- sweep over a minute whose raw rows still exist recomputes the same aggregate.
--
-- Idempotent: re-running this migration is a no-op (IF NOT EXISTS throughout).
CREATE TABLE IF NOT EXISTS stream_viewer_rollups (
    stream_id    UUID             NOT NULL,
    bucket_start TIMESTAMPTZ      NOT NULL,  -- minute-floored sample time
    avg_viewers  DOUBLE PRECISION NOT NULL,
    peak_viewers INTEGER          NOT NULL,
    sample_count BIGINT           NOT NULL,
    PRIMARY KEY (stream_id, bucket_start)
);

-- The downsample sweep deletes rollups by age, and per-stream rollup reads scan
-- by time; both key on bucket_start.
CREATE INDEX IF NOT EXISTS stream_viewer_rollups_bucket_idx
    ON stream_viewer_rollups (bucket_start);
