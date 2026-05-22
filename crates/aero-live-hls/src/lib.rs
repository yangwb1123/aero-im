//! HLS segment generation and serving.
//!
//! Owns the on-disk layout `{hls_dir}/{stream_id}/{segment_index}.ts` plus the
//! rolling `index.m3u8` manifest. The implementation here is deliberately
//! transport-agnostic: callers push raw segment bytes (TS or, with the
//! passthrough placeholder mode, whatever the RTMP encoder gave us) and we
//! manage the manifest math.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use thiserror::Error;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::debug;

/// How many segments a live playlist keeps before discarding the oldest from the manifest.
/// Disk cleanup of evicted files happens opportunistically.
pub const LIVE_WINDOW_SEGMENTS: usize = 6;

/// Default extension for segment files. Even in passthrough mode we keep `.ts`
/// so browser HLS players will accept the manifest — content may still be
/// FLV-tagged or fMP4-like, but operators are warned in logs.
pub const DEFAULT_SEGMENT_EXT: &str = "ts";

#[derive(Debug, Error)]
pub enum HlsError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("manifest already finalized")]
    Finalized,

    #[error("invalid argument: {0}")]
    Invalid(String),
}

pub type HlsResult<T> = Result<T, HlsError>;

/// Stateful HLS writer for a single stream.
///
/// Maintains a rolling sliding window of segments and writes an HLS v3 manifest
/// every time a segment is pushed. Call [`HlsWriter::finish`] to append
/// `#EXT-X-ENDLIST` and convert the playlist from live to VOD shape.
pub struct HlsWriter {
    dir: PathBuf,
    manifest_path: PathBuf,
    segment_ext: String,
    /// Monotonic index of the next segment to write (only ever increases).
    segment_index: u64,
    /// Sequence number of the oldest segment in the manifest (`EXT-X-MEDIA-SEQUENCE`).
    media_sequence: u64,
    /// Targetduration claim made in the manifest. Real per-segment durations are tracked per entry.
    target_duration_secs: u32,
    /// Rolling window of segments currently advertised in the manifest.
    window: Vec<SegmentEntry>,
    finalized: bool,
}

#[derive(Debug, Clone)]
struct SegmentEntry {
    /// Just the file name (e.g. `0.ts`), not an absolute path — the manifest is relative.
    file_name: String,
    duration_secs: f32,
}

impl HlsWriter {
    /// Creates a writer rooted at `dir`, ensuring the directory exists and an
    /// initial (empty live) manifest is on disk.
    pub async fn new(dir: PathBuf, target_duration_secs: u32) -> HlsResult<Self> {
        if target_duration_secs == 0 {
            return Err(HlsError::Invalid(
                "target_duration_secs must be > 0".to_string(),
            ));
        }
        fs::create_dir_all(&dir).await?;
        let manifest_path = dir.join("index.m3u8");
        let writer = Self {
            dir,
            manifest_path,
            segment_ext: DEFAULT_SEGMENT_EXT.to_string(),
            segment_index: 0,
            media_sequence: 0,
            target_duration_secs,
            window: Vec::with_capacity(LIVE_WINDOW_SEGMENTS),
            finalized: false,
        };
        writer.write_manifest(false).await?;
        Ok(writer)
    }

    /// Override the segment file extension. Useful for passthrough placeholder
    /// mode where operators may want `.bin` for debugging.
    pub fn with_segment_ext(mut self, ext: impl Into<String>) -> Self {
        self.segment_ext = ext.into();
        self
    }

    /// Returns the on-disk directory this writer manages.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the absolute manifest path.
    #[must_use]
    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    /// Returns the index of the next segment that will be written.
    #[must_use]
    pub fn next_segment_index(&self) -> u64 {
        self.segment_index
    }

    /// Persists a new segment to disk and updates the rolling manifest.
    ///
    /// Returns the absolute path of the segment file just written.
    pub async fn push_segment(
        &mut self,
        bytes: Bytes,
        duration_secs: f32,
    ) -> HlsResult<PathBuf> {
        if self.finalized {
            return Err(HlsError::Finalized);
        }
        if !(duration_secs > 0.0) {
            return Err(HlsError::Invalid(
                "duration_secs must be > 0".to_string(),
            ));
        }

        let file_name = format!("{}.{}", self.segment_index, self.segment_ext);
        let seg_path = self.dir.join(&file_name);
        let mut f = fs::File::create(&seg_path).await?;
        f.write_all(&bytes).await?;
        f.flush().await?;
        drop(f);

        self.segment_index += 1;
        self.window.push(SegmentEntry {
            file_name,
            duration_secs,
        });

        while self.window.len() > LIVE_WINDOW_SEGMENTS {
            let evicted = self.window.remove(0);
            self.media_sequence += 1;
            let evicted_path = self.dir.join(&evicted.file_name);
            if let Err(e) = fs::remove_file(&evicted_path).await {
                debug!(
                    path = %evicted_path.display(),
                    error = %e,
                    "failed to remove evicted segment (continuing)"
                );
            }
        }

        self.write_manifest(false).await?;
        Ok(seg_path)
    }

    /// Marks the manifest as finalized by appending `#EXT-X-ENDLIST`.
    /// Subsequent calls to [`Self::push_segment`] will fail.
    pub async fn finish(&mut self) -> HlsResult<()> {
        if self.finalized {
            return Ok(());
        }
        self.finalized = true;
        self.write_manifest(true).await?;
        Ok(())
    }

    async fn write_manifest(&self, end: bool) -> HlsResult<()> {
        let mut buf = String::with_capacity(256 + self.window.len() * 64);
        buf.push_str("#EXTM3U\n");
        buf.push_str("#EXT-X-VERSION:3\n");
        buf.push_str(&format!(
            "#EXT-X-TARGETDURATION:{}\n",
            self.target_duration_secs
        ));
        buf.push_str(&format!(
            "#EXT-X-MEDIA-SEQUENCE:{}\n",
            self.media_sequence
        ));
        if !end {
            // Live: hint to players that this is a sliding window.
            buf.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");
        } else {
            buf.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
        }
        for entry in &self.window {
            buf.push_str(&format!("#EXTINF:{:.3},\n", entry.duration_secs));
            buf.push_str(&entry.file_name);
            buf.push('\n');
        }
        if end {
            buf.push_str("#EXT-X-ENDLIST\n");
        }

        let tmp = self.manifest_path.with_extension("m3u8.tmp");
        let mut f = fs::File::create(&tmp).await?;
        f.write_all(buf.as_bytes()).await?;
        f.flush().await?;
        drop(f);
        fs::rename(&tmp, &self.manifest_path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    async fn read_manifest(writer: &HlsWriter) -> String {
        fs::read_to_string(writer.manifest_path()).await.unwrap()
    }

    #[tokio::test]
    async fn initial_manifest_has_no_segments() {
        let dir = tempfile::tempdir().unwrap();
        let writer = HlsWriter::new(dir.path().to_path_buf(), 2).await.unwrap();
        let m = read_manifest(&writer).await;
        assert!(m.contains("#EXTM3U"));
        assert!(m.contains("#EXT-X-VERSION:3"));
        assert!(m.contains("#EXT-X-TARGETDURATION:2"));
        assert!(m.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(!m.contains("#EXTINF"));
        assert!(!m.contains("#EXT-X-ENDLIST"));
    }

    #[tokio::test]
    async fn rejects_zero_target_duration() {
        let dir = tempfile::tempdir().unwrap();
        let err = HlsWriter::new(dir.path().to_path_buf(), 0).await.err().unwrap();
        assert!(matches!(err, HlsError::Invalid(_)));
    }

    #[tokio::test]
    async fn push_segment_writes_file_and_extinf() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = HlsWriter::new(dir.path().to_path_buf(), 2).await.unwrap();
        let p = writer
            .push_segment(Bytes::from_static(b"hello"), 2.0)
            .await
            .unwrap();
        assert!(p.exists());
        let m = read_manifest(&writer).await;
        assert!(m.contains("#EXTINF:2.000,"));
        assert!(m.contains("0.ts"));
        // Confirm the bytes round-tripped.
        let on_disk = std::fs::read(p).unwrap();
        assert_eq!(on_disk, b"hello");
    }

    #[tokio::test]
    async fn manifest_rolls_window_after_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = HlsWriter::new(dir.path().to_path_buf(), 2).await.unwrap();

        // Push more than the live window worth of segments.
        for i in 0..(LIVE_WINDOW_SEGMENTS as u64 + 3) {
            writer
                .push_segment(Bytes::from(vec![i as u8; 4]), 2.0)
                .await
                .unwrap();
        }

        let m = read_manifest(&writer).await;
        let expected_seq = (LIVE_WINDOW_SEGMENTS as u64 + 3) - LIVE_WINDOW_SEGMENTS as u64;
        assert!(
            m.contains(&format!("#EXT-X-MEDIA-SEQUENCE:{expected_seq}")),
            "manifest missing media sequence; got:\n{m}"
        );
        // The oldest segments should have been evicted from disk and from the manifest.
        for evicted in 0..expected_seq {
            let p = dir.path().join(format!("{evicted}.ts"));
            assert!(!p.exists(), "segment {evicted} should have been removed");
            assert!(
                !m.contains(&format!("{evicted}.ts\n")),
                "manifest still references {evicted}.ts:\n{m}"
            );
        }
        // The newest LIVE_WINDOW_SEGMENTS segments should still be present.
        let total_pushed = LIVE_WINDOW_SEGMENTS as u64 + 3;
        for kept in (total_pushed - LIVE_WINDOW_SEGMENTS as u64)..total_pushed {
            assert!(
                m.contains(&format!("{kept}.ts")),
                "manifest missing kept segment {kept}:\n{m}"
            );
        }
    }

    #[tokio::test]
    async fn finish_appends_endlist_and_blocks_future_pushes() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = HlsWriter::new(dir.path().to_path_buf(), 2).await.unwrap();
        writer
            .push_segment(Bytes::from_static(b"a"), 2.0)
            .await
            .unwrap();
        writer.finish().await.unwrap();
        let m = read_manifest(&writer).await;
        assert!(m.contains("#EXT-X-ENDLIST"));
        let err = writer
            .push_segment(Bytes::from_static(b"b"), 2.0)
            .await
            .err()
            .unwrap();
        assert!(matches!(err, HlsError::Finalized));
    }

    #[tokio::test]
    async fn custom_segment_extension_used_in_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = HlsWriter::new(dir.path().to_path_buf(), 2)
            .await
            .unwrap()
            .with_segment_ext("bin");
        writer
            .push_segment(Bytes::from_static(b"a"), 1.5)
            .await
            .unwrap();
        let m = read_manifest(&writer).await;
        assert!(m.contains("0.bin"), "manifest missing custom ext entry:\n{m}");
    }

    #[tokio::test]
    async fn rejects_non_positive_duration() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = HlsWriter::new(dir.path().to_path_buf(), 2).await.unwrap();
        let err = writer
            .push_segment(Bytes::from_static(b"a"), 0.0)
            .await
            .err()
            .unwrap();
        assert!(matches!(err, HlsError::Invalid(_)));
    }
}
