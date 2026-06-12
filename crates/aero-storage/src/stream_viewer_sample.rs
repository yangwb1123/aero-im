//! Concurrent-viewer history sampling — peak / average concurrent viewers.
//!
//! The live viewer COUNT lives in Redis as an ephemeral sorted set
//! ([`StreamViewerStore::count`](crate::live_presence::StreamViewerStore::count)):
//! it only ever knows the audience size *right now*. To report PEAK and AVERAGE
//! concurrent viewers over a stream's lifetime, a background sampler periodically
//! snapshots that count into `stream_viewer_samples` (migration 0072), one row
//! per sample. This repo writes those snapshots ([`StreamViewerSampleRepo::record`])
//! and aggregates them ([`StreamViewerSampleRepo::stats`] → MAX / AVG / COUNT).
//!
//! This closes the seam called out in [`crate::stream_stats`] (peak/avg concurrent
//! viewers were previously "not sampled"). Purely additive: a NEW repo over a NEW
//! table; no existing repo, table, or signature is touched.
//!
//! Stream ids are [`Ulid`]s stored as UUID — `uuid::Uuid::from_u128(ulid.0)` —
//! matching [`StreamRepo`](crate::StreamRepo) and [`StreamStatsRepo`](crate::StreamStatsRepo).

use serde::Serialize;
use sqlx::PgPool;
use ulid::Ulid;
use uuid::Uuid;

/// Aggregate of a stream's recorded concurrent-viewer samples.
///
/// `Serialize` so a handler can hand it straight back as JSON. `peak` is the
/// largest single sample (0 when no samples), `avg` the mean concurrent viewers
/// across all samples (0.0 when none), and `samples` the number of snapshots
/// taken (0 for a stream that was never sampled — e.g. never went live).
#[derive(Debug, Clone, Serialize)]
pub struct ViewerStats {
    /// Highest concurrent viewer count observed across all samples (`MAX`).
    pub peak: i64,
    /// Mean concurrent viewers across all samples (`AVG`). `0.0` when no samples.
    pub avg: f64,
    /// Number of samples taken for this stream (`COUNT`).
    pub samples: i64,
}

/// Repository for concurrent-viewer history sampling.
///
/// Cheap to clone — it just wraps a [`PgPool`] (an `Arc` internally), so feature
/// modules / the background sampler build one inline via
/// [`StreamViewerSampleRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct StreamViewerSampleRepo {
    pool: PgPool,
}

impl StreamViewerSampleRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record one concurrent-viewer snapshot for `stream`. `sampled_at` defaults
    /// to `now()` server-side. `viewers` is clamped at the type boundary into an
    /// `i32` (the column type) — a count beyond `i32::MAX` is implausible for a
    /// viewer audience and saturates rather than overflowing.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn record(&self, stream: Ulid, viewers: u64) -> Result<(), sqlx::Error> {
        let sid = Uuid::from_u128(stream.0);
        let v = i32::try_from(viewers).unwrap_or(i32::MAX);
        sqlx::query("INSERT INTO stream_viewer_samples (stream_id, viewers) VALUES ($1, $2)")
            .bind(sid)
            .bind(v)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Aggregate this stream's recorded samples into a [`ViewerStats`].
    ///
    /// Returns `peak = 0`, `avg = 0.0`, `samples = 0` for a stream with no
    /// samples (the `COALESCE`s make an empty set zeroed rather than null), so the
    /// caller never has to special-case "never sampled".
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the aggregate query.
    pub async fn stats(&self, stream: Ulid) -> Result<ViewerStats, sqlx::Error> {
        let sid = Uuid::from_u128(stream.0);
        let (peak, avg, samples) = sqlx::query_as::<_, (i64, f64, i64)>(
            r"SELECT
                COALESCE(MAX(viewers), 0)::bigint,
                COALESCE(AVG(viewers), 0)::double precision,
                COUNT(*)::bigint
              FROM stream_viewer_samples WHERE stream_id = $1",
        )
        .bind(sid)
        .fetch_one(&self.pool)
        .await?;
        Ok(ViewerStats { peak, avg, samples })
    }
}

/// One point in a stream's viewer retention curve: the offset from the start of
/// the stream, the average viewer count in that bucket, and the retention
/// percentage relative to the peak at stream start.
///
/// `Serialize` so the handler can return it directly as JSON.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RetentionPoint {
    /// Offset from stream start in seconds (0 = the opening bucket).
    pub offset_secs: i64,
    /// Average concurrent viewers in the bucket.
    pub viewers: i64,
    /// Viewers as a percentage of the peak at the start of the stream.
    pub retention_pct: f64,
}

impl StreamViewerSampleRepo {
    /// Build a retention curve for `stream`, grouping samples into
    /// `bucket_secs`-wide windows (default: 60s) and computing the retention
    /// percentage relative to the peak viewer count seen in the first bucket.
    ///
    /// All samples for the stream are fetched in chronological order and
    /// grouped in Rust — no complex SQL windowing is required.
    ///
    /// Returns an empty `Vec` when the stream has no samples.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn retention_curve(
        &self,
        stream: Ulid,
        bucket_secs: i64,
    ) -> Result<Vec<RetentionPoint>, sqlx::Error> {
        let bucket_secs = bucket_secs.max(1);
        let sid = Uuid::from_u128(stream.0);

        // Fetch all samples for this stream in chronological order.
        let rows = sqlx::query_as::<_, (i32, time::OffsetDateTime)>(
            r"SELECT viewers, sampled_at
               FROM stream_viewer_samples
              WHERE stream_id = $1
              ORDER BY sampled_at ASC",
        )
        .bind(sid)
        .fetch_all(&self.pool)
        .await?;

        if rows.is_empty() {
            return Ok(vec![]);
        }

        // Use the timestamp of the very first sample as the stream start anchor.
        let start_ts = rows[0].1;

        // Group into bucket_secs-wide windows, computing avg viewers per bucket.
        // Bucket index = floor((sampled_at - start) / bucket_secs).
        let mut buckets: std::collections::BTreeMap<i64, (i64, i64)> =
            std::collections::BTreeMap::new(); // bucket_idx -> (sum, count)
        for (viewers, sampled_at) in &rows {
            let offset = ((*sampled_at) - start_ts).whole_seconds().max(0);
            let bucket_idx = offset / bucket_secs;
            let entry = buckets.entry(bucket_idx).or_insert((0, 0));
            entry.0 += i64::from(*viewers);
            entry.1 += 1;
        }

        // The peak is the avg viewers in the FIRST bucket (offset 0).
        let first_avg = {
            let (sum, cnt) = buckets.get(&0).copied().unwrap_or((0, 1));
            if cnt == 0 { 1.0 } else { sum as f64 / cnt as f64 }
        };
        let peak_viewers = first_avg.max(1.0);

        let curve: Vec<RetentionPoint> = buckets
            .into_iter()
            .map(|(bucket_idx, (sum, cnt))| {
                let avg_viewers = if cnt > 0 { sum / cnt } else { 0 };
                let retention_pct = avg_viewers as f64 / peak_viewers * 100.0;
                RetentionPoint {
                    offset_secs: bucket_idx * bucket_secs,
                    viewers: avg_viewers,
                    retention_pct,
                }
            })
            .collect();

        Ok(curve)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored stream_viewer_sample
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_viewer_sample_records_and_aggregates_peak_avg() {
        let p = pool();
        let repo = StreamViewerSampleRepo::new(p.clone());
        // A fresh, never-sampled stream id: no FK on stream_viewer_samples, so the
        // sample rows stand alone — no need to insert a streams/participant row.
        let stream = Ulid::new();
        let sid = Uuid::from_u128(stream.0);

        // Before any samples: everything zeroed (the COALESCEs hold).
        let empty = repo.stats(stream).await.expect("stats empty");
        assert_eq!(empty.peak, 0, "no samples ⇒ peak 0");
        assert!(empty.avg.abs() < f64::EPSILON, "no samples ⇒ avg 0.0");
        assert_eq!(empty.samples, 0, "no samples ⇒ count 0");

        // Three samples: 2, 10, 6 ⇒ peak 10, avg 6.0, samples 3.
        for v in [2u64, 10, 6] {
            repo.record(stream, v).await.expect("record sample");
        }
        let s = repo.stats(stream).await.expect("stats");
        assert_eq!(s.peak, 10, "peak is the max sample");
        assert!((s.avg - 6.0).abs() < 1e-9, "avg is the mean: (2+10+6)/3 = 6.0");
        assert_eq!(s.samples, 3, "three samples recorded");

        // Cleanup: remove the throwaway samples (no FK cascade to lean on).
        sqlx::query("DELETE FROM stream_viewer_samples WHERE stream_id = $1")
            .bind(sid)
            .execute(&p)
            .await
            .ok();
    }
}
