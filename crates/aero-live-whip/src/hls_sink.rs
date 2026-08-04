//! Hand-off from depacketized H.264 access units toward HLS.
//!
//! The [`H264Depacketizer`](crate::depacketize::H264Depacketizer) emits
//! **Annex-B** access units (NAL units prefixed with `00 00 00 01`). The TS
//! muxer in [`aero_live_hls`] (`FlvToTsConverter`) consumes the **AVCC**
//! length-prefixed form that RTMP/FLV delivers, then converts *back* to Annex-B
//! internally. To reuse that battle-tested muxer without forking it, this
//! module provides:
//!
//! - [`MediaSink`] — the boundary the session pushes finished access units to.
//! - [`annex_b_to_avcc`] — repackages an Annex-B AU into the AVCC payload that
//!   `FlvToTsConverter::push_video_tag` expects (4-byte length prefixes).
//! - [`split_annex_b`] / [`classify_nal`] — the parsing primitives, unit-tested.
//! - [`LoggingSink`] — a no-op default sink used as a fallback / for tests.
//! - [`HlsSink`] + [`HlsSegmentWriter`] — the real glue: a `MediaSink` that
//!   buffers SPS/PPS, synthesizes an `AVCDecoderConfigurationRecord` from the
//!   first keyframe, drives `aero_live_hls::FlvToTsConverter` to mux MPEG-TS,
//!   and cuts a segment on every IDR keyframe — handing finished `.ts` bytes to
//!   an async [`HlsSegmentWriter`] that drives `aero_live_hls::HlsWriter` to
//!   persist `.ts` + `index.m3u8`.
//!
//! ## Why a sync sink + async writer split
//!
//! [`MediaSink`] is **synchronous** because it is called from inside
//! [`WhipSession::run`](crate::session::WhipSession::run)'s str0m sans-IO event
//! loop, which must never block on disk I/O (blocking there stalls ICE/DTLS
//! keepalives and RTCP). So [`HlsSink`] does the *pure-CPU* work synchronously —
//! NAL parsing, avcC synthesis, FLV→TS muxing, keyframe segment cutting — and
//! ships each finished segment over an `mpsc` channel to [`HlsSegmentWriter`],
//! whose async [`run`](HlsSegmentWriter::run) loop owns the `HlsWriter` and does
//! the filesystem writes off the hot path. Construct the pair with [`hls_sink`].
//!
//! ## What is wired & tested vs. still needs a browser
//!
//! **Wired + tested at the byte level (no browser):** RTP → Annex-B
//! (depacketizer) → AVCC → MPEG-TS (`FlvToTsConverter`) → `.ts` + `index.m3u8`
//! on disk (`HlsWriter`). The integration test in this crate synthesizes real
//! H.264 RTP packets (incl. FU-A) and asserts real segment files + a well-formed
//! manifest are produced.
//!
//! **Still needs a live browser (not runtime-verifiable here):** the ICE/DTLS/
//! SRTP handshake that delivers those RTP packets in the first place. Everything
//! *downstream* of "an RTP packet arrived" is now exercised; only the WebRTC
//! transport that feeds [`WhipSession::run`](crate::session::WhipSession::run)
//! remains browser-gated.

use std::path::PathBuf;

use aero_live_hls::{FlvToTsConverter, HlsWriter};
use bytes::{BufMut, Bytes, BytesMut};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

/// Sink for media produced by a [`WhipSession`](crate::session::WhipSession).
///
/// Implementors forward access units to HLS (e.g. via
/// [`aero_live_hls::FlvToTsConverter`]) or to any other consumer. Methods take
/// 90 kHz presentation timestamps to match the MPEG-TS clock.
pub trait MediaSink: Send {
    /// A complete H.264 access unit in **Annex-B** form (one or more NAL units,
    /// each `00 00 00 01`-prefixed). `pts_90k` is the presentation time in
    /// 90 kHz ticks.
    fn on_video_au(&mut self, annex_b: Bytes, pts_90k: u64) -> anyhow::Result<()>;

    /// A decoded audio payload (e.g. Opus). Default: ignored — the P5 path
    /// targets H.264 video into HLS; audio plumbing is future work.
    fn on_audio(&mut self, _payload: Bytes, _pts_90k: u64) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Default sink that just records counters and logs. Useful as a placeholder
/// until the server provides a `FlvToTsConverter`-backed sink, and handy for
/// tests/observability.
#[derive(Debug, Default)]
pub struct LoggingSink {
    pub video_aus: u64,
    pub video_bytes: u64,
    pub audio_packets: u64,
}

impl MediaSink for LoggingSink {
    fn on_video_au(&mut self, annex_b: Bytes, pts_90k: u64) -> anyhow::Result<()> {
        self.video_aus += 1;
        self.video_bytes += annex_b.len() as u64;
        tracing::trace!(
            au = self.video_aus,
            len = annex_b.len(),
            pts_90k,
            "whip: video access unit"
        );
        Ok(())
    }

    fn on_audio(&mut self, payload: Bytes, _pts_90k: u64) -> anyhow::Result<()> {
        self.audio_packets += 1;
        let _ = payload;
        Ok(())
    }
}

/// Classification of a NAL unit by its header type (RFC 6184 / H.264 §7.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalClass {
    /// Sequence parameter set (type 7).
    Sps,
    /// Picture parameter set (type 8).
    Pps,
    /// Instantaneous decoder refresh slice — a keyframe (type 5).
    IdrSlice,
    /// Any other NAL (non-IDR slice, SEI, AUD, ...).
    Other(u8),
}

/// Classify a NAL unit from its first byte (the NAL header).
#[must_use]
pub fn classify_nal(nal_header: u8) -> NalClass {
    match nal_header & 0x1F {
        5 => NalClass::IdrSlice,
        7 => NalClass::Sps,
        8 => NalClass::Pps,
        other => NalClass::Other(other),
    }
}

/// Split an Annex-B byte run into its constituent NAL units (each returned
/// slice excludes the start code). Handles both 3- and 4-byte start codes.
#[must_use]
pub fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    // Position just past the first start code; if there is none, there are no
    // NAL units to report.
    let Some((pos, len)) = find_start_code(data, 0) else {
        return nals;
    };
    let mut start = pos + len;
    let mut i = start;
    while let Some((next_pos, next_len)) = find_start_code(data, i) {
        if next_pos > start {
            nals.push(&data[start..next_pos]);
        }
        start = next_pos + next_len;
        i = start;
    }
    if data.len() > start {
        nals.push(&data[start..]);
    }
    nals
}

/// Returns `(position, length)` of the next Annex-B start code at or after
/// `from`. A 4-byte code (`00 00 00 01`) is preferred over the 3-byte form
/// (`00 00 01`) when both match at the same offset.
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                return Some((i, 3));
            }
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some((i, 4));
            }
        }
        i += 1;
    }
    None
}

/// Repackage an Annex-B access unit into the **AVCC** length-prefixed payload
/// (4-byte big-endian length per NAL) that
/// `aero_live_hls::FlvToTsConverter::push_video_tag` expects as its NALU body.
///
/// This is the concrete bridge into the existing TS muxer: the server builds an
/// FLV-shaped video tag as `[frame|codec][avc_pkt_type=1][cts:i24] ++ avcc` and
/// calls `push_video_tag`. (The muxer re-derives Annex-B internally, which is a
/// small redundancy we accept to avoid forking the muxer.)
#[must_use]
pub fn annex_b_to_avcc(annex_b: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(annex_b.len() + 16);
    for nal in split_annex_b(annex_b) {
        // AVCDecoderConfigurationRecord with lengthSizeMinusOne=3 → 4-byte len.
        // A NAL delivered over RTP can never approach 4 GiB, so saturate
        // defensively instead of risking a panic.
        let len = u32::try_from(nal.len()).unwrap_or(u32::MAX);
        out.put_u32(len);
        out.extend_from_slice(nal);
    }
    out.freeze()
}

/// True if the access unit contains an IDR (keyframe) slice — the segmenter
/// must start HLS segments on these.
#[must_use]
pub fn au_is_keyframe(annex_b: &[u8]) -> bool {
    split_annex_b(annex_b)
        .iter()
        .filter_map(|n| n.first())
        .any(|&h| classify_nal(h) == NalClass::IdrSlice)
}

/// Synthesize an `AVCDecoderConfigurationRecord` (the "avcC" box / FLV AVC
/// sequence header) from a single SPS and a single PPS NAL unit (each *without*
/// an Annex-B start code, i.e. starting at the NAL header byte).
///
/// This is the byte layout `FlvToTsConverter::parse_avc_config` decodes (ISO/IEC
/// 14496-15 §5.2.4.1). `lengthSizeMinusOne` is fixed at `3` (4-byte NAL length
/// prefixes) to match [`annex_b_to_avcc`], which always emits 4-byte lengths.
///
/// The profile/level triplet is lifted from the SPS itself (its bytes 1..=3 are
/// `profile_idc`, `constraint_set_flags`, `level_idc`), so the record is
/// self-consistent with the stream. Returns `None` if the SPS is too short to
/// carry that triplet.
#[must_use]
pub fn build_avc_decoder_config(sps: &[u8], pps: &[u8]) -> Option<Bytes> {
    // SPS = [nal_header][profile_idc][constraint_flags][level_idc] ... — need at
    // least those 4 bytes to populate the record's profile/level fields.
    if sps.len() < 4 {
        return None;
    }
    let profile_idc = sps[1];
    let constraint_flags = sps[2];
    let level_idc = sps[3];

    let mut out = BytesMut::with_capacity(11 + sps.len() + pps.len());
    out.put_u8(1); // configurationVersion
    out.put_u8(profile_idc); // AVCProfileIndication
    out.put_u8(constraint_flags); // profile_compatibility
    out.put_u8(level_idc); // AVCLevelIndication
                           // 6 reserved bits set to 1 + lengthSizeMinusOne(2) = 3 → 0xFF.
    out.put_u8(0xFC | 0x03);
    // 3 reserved bits set to 1 + numOfSequenceParameterSets(5) = 1 → 0xE1.
    out.put_u8(0xE0 | 0x01);
    // SPS length can never approach u16::MAX over RTP; saturate defensively.
    out.put_u16(u16::try_from(sps.len()).unwrap_or(u16::MAX));
    out.extend_from_slice(sps);
    out.put_u8(1); // numOfPictureParameterSets
    out.put_u16(u16::try_from(pps.len()).unwrap_or(u16::MAX));
    out.extend_from_slice(pps);
    Some(out.freeze())
}

/// A finished HLS segment: the MPEG-TS bytes plus the wall-clock duration the
/// `#EXTINF` entry should advertise. Produced by [`HlsSink`], consumed by
/// [`HlsSegmentWriter`].
#[derive(Debug, Clone)]
pub struct HlsSegment {
    /// Complete MPEG-TS segment bytes (PAT+PMT already prepended by the muxer).
    pub bytes: Bytes,
    /// Segment duration in seconds, for the manifest `#EXTINF`.
    pub duration_secs: f32,
}

/// Smallest positive duration we will ever report to [`HlsWriter::push_segment`]
/// (which rejects non-positive durations). Synthetic/degenerate streams whose
/// access units carry near-identical PTS still produce a valid manifest entry.
const MIN_SEGMENT_DURATION_SECS: f32 = 0.001;

/// Create a wired [`HlsSink`] / [`HlsSegmentWriter`] pair for one stream.
///
/// `stream_dir` is the per-stream HLS directory (the caller composes
/// `hls_dir/{stream_id}`); `target_duration_secs` is the manifest's
/// `#EXT-X-TARGETDURATION` claim. The async writer is initialized eagerly so its
/// initial (empty) `index.m3u8` is on disk immediately.
///
/// Spawn [`HlsSegmentWriter::run`] on a task, then hand the [`HlsSink`] to
/// [`WhipSession::run`](crate::session::WhipSession::run); dropping the sink (end
/// of session) closes the channel and lets the writer finalize the manifest.
pub async fn hls_sink(
    stream_dir: PathBuf,
    target_duration_secs: u32,
) -> aero_live_hls::HlsResult<(HlsSink, HlsSegmentWriter)> {
    let writer = HlsWriter::new(stream_dir, target_duration_secs).await?;
    let (tx, rx) = mpsc::unbounded_channel();
    Ok((HlsSink::new(tx), HlsSegmentWriter { writer, rx }))
}

/// An HLS-writing [`MediaSink`].
///
/// Receives reassembled **Annex-B** H.264 access units from the depacketizer and
/// turns them into keyframe-aligned MPEG-TS segments via
/// [`aero_live_hls::FlvToTsConverter`]:
///
/// 1. **Parameter sets:** SPS (type 7) / PPS (type 8) NAL units are cached as
///    they arrive (browsers re-send them with each IDR).
/// 2. **Sequence header:** on the *first* IDR keyframe, an
///    `AVCDecoderConfigurationRecord` is synthesized from the cached SPS+PPS and
///    fed to the muxer as an AVC sequence header (`avc_packet_type == 0`). Until
///    that lands, the muxer cannot emit and access units are buffered/dropped by
///    `FlvToTsConverter` exactly as the RTMP path does.
/// 3. **Media:** every access unit's NALs are repackaged to AVCC
///    ([`annex_b_to_avcc`]) and pushed as an FLV-shaped video tag
///    (`avc_packet_type == 1`).
/// 4. **Segmenting:** an IDR keyframe arriving *after* the current segment
///    already has TS data cuts that segment ([`FlvToTsConverter::drain_segment`])
///    and ships it to the [`HlsSegmentWriter`]; the keyframe then opens the next
///    segment, so every segment begins on a keyframe.
///
/// Disk I/O happens on the writer task, never here — see the module docs.
pub struct HlsSink {
    /// FLV→TS muxer (reused from `aero-live-hls`, not re-implemented here).
    converter: FlvToTsConverter,
    /// Cached SPS NAL units (body, no start code), latest wins.
    sps: Vec<Vec<u8>>,
    /// Cached PPS NAL units (body, no start code), latest wins.
    pps: Vec<Vec<u8>>,
    /// Whether the AVC sequence header has been handed to the muxer yet.
    config_sent: bool,
    /// PTS (90 kHz) of the first access unit in the currently-open segment, used
    /// to compute the `#EXTINF` duration when the segment is cut.
    segment_start_pts_90k: Option<u64>,
    /// Channel to the async writer that persists finished segments.
    tx: mpsc::UnboundedSender<HlsSegment>,
    /// Diagnostics: total access units accepted.
    video_aus: u64,
    /// Diagnostics: total segments cut and shipped to the writer.
    segments_emitted: u64,
}

impl HlsSink {
    /// Build a sink that ships finished segments over `tx`. Prefer the
    /// [`hls_sink`] constructor, which also creates the matching writer.
    #[must_use]
    pub fn new(tx: mpsc::UnboundedSender<HlsSegment>) -> Self {
        Self {
            converter: FlvToTsConverter::new(),
            sps: Vec::new(),
            pps: Vec::new(),
            config_sent: false,
            segment_start_pts_90k: None,
            tx,
            video_aus: 0,
            segments_emitted: 0,
        }
    }

    /// Number of access units accepted so far (diagnostics/tests).
    #[must_use]
    pub fn video_aus(&self) -> u64 {
        self.video_aus
    }

    /// Number of segments cut and shipped to the writer so far
    /// (diagnostics/tests). Does not count the trailing segment, which the
    /// writer flushes on channel close.
    #[must_use]
    pub fn segments_emitted(&self) -> u64 {
        self.segments_emitted
    }

    /// Cache any SPS/PPS carried in this access unit. Browsers prepend the
    /// parameter sets to each IDR, so this keeps them fresh.
    fn absorb_parameter_sets(&mut self, annex_b: &[u8]) {
        for nal in split_annex_b(annex_b) {
            let Some(&header) = nal.first() else { continue };
            match classify_nal(header) {
                NalClass::Sps => {
                    self.sps = vec![nal.to_vec()];
                }
                NalClass::Pps => {
                    self.pps = vec![nal.to_vec()];
                }
                _ => {}
            }
        }
    }

    /// Hand the AVC sequence header (avcC) to the muxer once SPS+PPS are known.
    /// Idempotent; a no-op until both parameter sets have been cached.
    fn ensure_config_sent(&mut self) -> anyhow::Result<()> {
        if self.config_sent {
            return Ok(());
        }
        let (Some(sps), Some(pps)) = (self.sps.first(), self.pps.first()) else {
            return Ok(());
        };
        let Some(avcc) = build_avc_decoder_config(sps, pps) else {
            warn!("whip-hls: SPS too short to synthesize avcC; deferring");
            return Ok(());
        };
        // FLV AVC sequence-header video tag: [0x17][avc_packet_type=0][cts=0]++avcC.
        // 0x17 = frame_type(1=key)<<4 | codec_id(7=AVC).
        let mut tag = Vec::with_capacity(5 + avcc.len());
        tag.extend_from_slice(&[0x17, 0x00, 0x00, 0x00, 0x00]);
        tag.extend_from_slice(&avcc);
        self.converter
            .push_video_tag(&tag, 0)
            .map_err(|e| anyhow::anyhow!("avc sequence header: {e}"))?;
        self.config_sent = true;
        debug!(
            sps_len = sps.len(),
            pps_len = pps.len(),
            "whip-hls: AVC sequence header sent to muxer"
        );
        Ok(())
    }

    /// Push one access unit's NAL units into the muxer as an FLV video tag.
    fn push_au_to_muxer(
        &mut self,
        annex_b: &[u8],
        pts_90k: u64,
        keyframe: bool,
    ) -> anyhow::Result<()> {
        let avcc = annex_b_to_avcc(annex_b);
        // [frame_codec][avc_packet_type=1][cts:i24=0] ++ avcc-nalus.
        // frame_codec: keyframe → 0x17, inter → 0x27 (frame_type<<4 | AVC=7).
        let frame_codec = if keyframe { 0x17 } else { 0x27 };
        let mut tag = Vec::with_capacity(5 + avcc.len());
        tag.extend_from_slice(&[frame_codec, 0x01, 0x00, 0x00, 0x00]);
        tag.extend_from_slice(&avcc);
        // The muxer's clock is milliseconds; our PTS is 90 kHz ticks.
        let ts_ms = u32::try_from(pts_90k / 90).unwrap_or(u32::MAX);
        self.converter
            .push_video_tag(&tag, ts_ms)
            .map_err(|e| anyhow::anyhow!("avc nalus: {e}"))?;
        Ok(())
    }

    /// Flush the final, still-open segment at end-of-stream (no following
    /// keyframe to trigger a cut). The closing PTS is the last DTS the muxer
    /// wrote, so the trailing `#EXTINF` reflects the real GOP span. Called from
    /// [`Drop`] so the wiring in
    /// [`WhipSession::run`](crate::session::WhipSession::run) — which moves the
    /// sink in and drops it on return — never loses the last GOP.
    fn flush_final(&mut self) {
        let end_pts_90k = self
            .converter
            .last_video_dts_90k()
            .or(self.segment_start_pts_90k)
            .unwrap_or(0);
        self.cut_segment(end_pts_90k);
    }

    /// Cut the open segment (if it holds data) and ship it to the writer.
    /// `end_pts_90k` is the PTS that closes the segment (the next keyframe's, or
    /// the last AU's at end-of-stream) and is used to compute the duration.
    fn cut_segment(&mut self, end_pts_90k: u64) {
        if !self.converter.has_segment_data() {
            return;
        }
        let bytes = self.converter.drain_segment();
        let duration_secs = self.segment_duration(end_pts_90k);
        self.segment_start_pts_90k = None;
        self.segments_emitted += 1;
        // An error here means the writer task is gone; the session is ending, so
        // just trace it rather than failing the media path.
        if self
            .tx
            .send(HlsSegment {
                bytes,
                duration_secs,
            })
            .is_err()
        {
            trace!("whip-hls: segment writer dropped; discarding finished segment");
        }
    }

    /// Duration (seconds) of the open segment given the PTS that closes it,
    /// floored at [`MIN_SEGMENT_DURATION_SECS`] so it is always strictly
    /// positive (the writer rejects non-positive durations).
    fn segment_duration(&self, end_pts_90k: u64) -> f32 {
        let span = self
            .segment_start_pts_90k
            .map_or(0, |start| end_pts_90k.saturating_sub(start));
        // 90 kHz ticks → seconds. The cast is lossy only for absurdly long
        // segments; HLS segments are seconds, so this is exact in practice.
        #[allow(clippy::cast_precision_loss)]
        let secs = span as f32 / 90_000.0;
        if secs > MIN_SEGMENT_DURATION_SECS {
            secs
        } else {
            MIN_SEGMENT_DURATION_SECS
        }
    }
}

impl MediaSink for HlsSink {
    fn on_video_au(&mut self, annex_b: Bytes, pts_90k: u64) -> anyhow::Result<()> {
        self.video_aus += 1;
        let keyframe = au_is_keyframe(&annex_b);

        // Keep parameter sets current and (re)try sending the sequence header.
        self.absorb_parameter_sets(&annex_b);
        if keyframe {
            self.ensure_config_sent()?;
        }

        // The muxer rejects NALU tags before its AVC sequence header. We only
        // send that header on the first keyframe, so until then we must drop
        // access units here (this also matches the muxer's own "no inter frames
        // before the first keyframe" rule — we just enforce it one level up so
        // we never hit its error path).
        if !self.config_sent {
            return Ok(());
        }

        // A keyframe that arrives once the current segment already has TS data
        // closes that segment *before* this keyframe is muxed, so every segment
        // begins on a keyframe (mirrors the SRT/RTMP segmenters' invariant).
        if keyframe && self.converter.has_segment_data() {
            self.cut_segment(pts_90k);
        }

        self.push_au_to_muxer(&annex_b, pts_90k, keyframe)?;

        // Remember when this segment started (first AU that produced TS bytes).
        if self.segment_start_pts_90k.is_none() && self.converter.has_segment_data() {
            self.segment_start_pts_90k = Some(pts_90k);
        }
        Ok(())
    }
}

impl Drop for HlsSink {
    /// Ship the trailing open segment (if any) before the channel closes, so the
    /// last GOP of a stream is never dropped on the floor. The writer then
    /// finalizes the manifest once it observes the closed channel.
    fn drop(&mut self) {
        self.flush_final();
    }
}

/// Async consumer that persists [`HlsSegment`]s produced by an [`HlsSink`].
///
/// Owns the `aero_live_hls::HlsWriter` and drains the channel on its own task so
/// the str0m loop never blocks on disk. [`run`](Self::run) returns once the sink
/// (and thus the sender) is dropped, after finalizing the manifest.
pub struct HlsSegmentWriter {
    writer: HlsWriter,
    rx: mpsc::UnboundedReceiver<HlsSegment>,
}

impl HlsSegmentWriter {
    /// The on-disk directory this writer manages (its `index.m3u8` lives here).
    #[must_use]
    pub fn dir(&self) -> &std::path::Path {
        self.writer.dir()
    }

    /// Persist every segment the sink ships, then finalize the manifest when the
    /// channel closes (the sink was dropped, i.e. the session ended).
    ///
    /// Returns the number of segments written. Disk errors on an individual
    /// segment are logged and skipped rather than tearing down the stream.
    pub async fn run(mut self) -> aero_live_hls::HlsResult<u64> {
        let mut written = 0u64;
        while let Some(segment) = self.rx.recv().await {
            match self
                .writer
                .push_segment(segment.bytes, segment.duration_secs)
                .await
            {
                Ok(path) => {
                    written += 1;
                    trace!(path = %path.display(), "whip-hls: wrote segment");
                }
                Err(e) => warn!(error = %e, "whip-hls: failed to write segment; skipping"),
            }
        }
        self.writer.finish().await?;
        debug!(
            segments = written,
            "whip-hls: writer finished, manifest finalized"
        );
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(start4: bool, header: u8, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        if start4 {
            v.extend_from_slice(&[0, 0, 0, 1]);
        } else {
            v.extend_from_slice(&[0, 0, 1]);
        }
        v.push(header);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn split_single_4byte_nal() {
        let buf = nal(true, 0x65, &[0xAA, 0xBB]);
        let nals = split_annex_b(&buf);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0], &[0x65, 0xAA, 0xBB]);
    }

    #[test]
    fn split_multiple_nals_mixed_start_codes() {
        // 4-byte SPS, 3-byte PPS, 4-byte IDR.
        let mut buf = nal(true, 0x67, &[0x42, 0x00]);
        buf.extend_from_slice(&nal(false, 0x68, &[0xCE]));
        buf.extend_from_slice(&nal(true, 0x65, &[0x01, 0x02, 0x03]));
        let nals = split_annex_b(&buf);
        assert_eq!(nals.len(), 3, "got {nals:?}");
        assert_eq!(nals[0][0] & 0x1F, 7); // SPS
        assert_eq!(nals[1][0] & 0x1F, 8); // PPS
        assert_eq!(nals[2][0] & 0x1F, 5); // IDR
        assert_eq!(nals[2], &[0x65, 0x01, 0x02, 0x03]);
    }

    #[test]
    fn split_empty_or_no_start_code() {
        assert!(split_annex_b(&[]).is_empty());
        assert!(split_annex_b(&[0x65, 0xAA]).is_empty());
    }

    #[test]
    fn classify_recognizes_sps_pps_idr() {
        assert_eq!(classify_nal(0x67), NalClass::Sps);
        assert_eq!(classify_nal(0x68), NalClass::Pps);
        assert_eq!(classify_nal(0x65), NalClass::IdrSlice);
        assert_eq!(classify_nal(0x61), NalClass::Other(1));
        assert_eq!(classify_nal(0x06), NalClass::Other(6)); // SEI
    }

    #[test]
    fn keyframe_detection() {
        let mut kf = nal(true, 0x67, &[0x42]); // SPS
        kf.extend_from_slice(&nal(true, 0x68, &[0xCE])); // PPS
        kf.extend_from_slice(&nal(true, 0x65, &[0x01])); // IDR
        assert!(au_is_keyframe(&kf));

        let inter = nal(true, 0x61, &[0x01]); // non-IDR slice
        assert!(!au_is_keyframe(&inter));
    }

    #[test]
    fn avcc_roundtrips_lengths_and_payload() {
        // Annex-B AU: SPS(2 body) + IDR(3 body).
        let mut au = nal(true, 0x67, &[0x42, 0x00]);
        au.extend_from_slice(&nal(true, 0x65, &[0x01, 0x02, 0x03]));

        let avcc = annex_b_to_avcc(&au);
        // First NAL: len = 1(header)+2 = 3 → 4-byte BE prefix.
        assert_eq!(&avcc[0..4], &3u32.to_be_bytes());
        assert_eq!(&avcc[4..7], &[0x67, 0x42, 0x00]);
        // Second NAL: len = 1+3 = 4.
        assert_eq!(&avcc[7..11], &4u32.to_be_bytes());
        assert_eq!(&avcc[11..15], &[0x65, 0x01, 0x02, 0x03]);
        assert_eq!(avcc.len(), 4 + 3 + 4 + 4);
    }

    #[test]
    fn avcc_uses_4byte_start_code_const() {
        // Guard against ANNEX_B_START_CODE drift breaking the bridge contract.
        assert_eq!(
            crate::depacketize::ANNEX_B_START_CODE,
            [0x00, 0x00, 0x00, 0x01]
        );
    }

    #[test]
    fn logging_sink_counts() {
        let mut s = LoggingSink::default();
        s.on_video_au(Bytes::from_static(&[0, 0, 0, 1, 0x65, 0xAA]), 9000)
            .unwrap();
        s.on_audio(Bytes::from_static(&[1, 2, 3]), 9000).unwrap();
        assert_eq!(s.video_aus, 1);
        assert_eq!(s.video_bytes, 6);
        assert_eq!(s.audio_packets, 1);
    }

    // --------------------- avcC synthesis ---------------------

    #[test]
    fn avc_decoder_config_layout_is_well_formed() {
        // SPS NAL: header 0x67, profile_idc=0x42, constraint=0x00, level=0x1F.
        let sps = [0x67u8, 0x42, 0x00, 0x1F, 0xAB, 0xCD];
        let pps = [0x68u8, 0xCE, 0x3C];
        let cfg = build_avc_decoder_config(&sps, &pps).expect("config");

        assert_eq!(cfg[0], 1, "configurationVersion");
        assert_eq!(cfg[1], 0x42, "AVCProfileIndication lifted from SPS");
        assert_eq!(cfg[2], 0x00, "profile_compatibility lifted from SPS");
        assert_eq!(cfg[3], 0x1F, "AVCLevelIndication lifted from SPS");
        assert_eq!(cfg[4] & 0x03, 0x03, "lengthSizeMinusOne = 3 (4-byte NALs)");
        assert_eq!(cfg[5] & 0x1F, 1, "numOfSequenceParameterSets = 1");
        let sps_len = u16::from_be_bytes([cfg[6], cfg[7]]) as usize;
        assert_eq!(sps_len, sps.len());
        assert_eq!(&cfg[8..8 + sps_len], &sps);
        let p = 8 + sps_len;
        assert_eq!(cfg[p], 1, "numOfPictureParameterSets = 1");
        let pps_len = u16::from_be_bytes([cfg[p + 1], cfg[p + 2]]) as usize;
        assert_eq!(pps_len, pps.len());
        assert_eq!(&cfg[p + 3..p + 3 + pps_len], &pps);
    }

    #[test]
    fn avc_decoder_config_rejects_short_sps() {
        // Fewer than 4 bytes → no profile/level triplet → None.
        assert!(build_avc_decoder_config(&[0x67, 0x42], &[0x68, 0xCE]).is_none());
    }

    #[test]
    fn synthesized_avcc_is_accepted_by_real_muxer() {
        // The whole point of synthesizing the record is that the *real*
        // FlvToTsConverter parses it back. Wrap it as an FLV AVC sequence header
        // and confirm the muxer becomes ready (no fork, no re-mux).
        let sps = [0x67u8, 0x42, 0x00, 0x1F, 0xAB];
        let pps = [0x68u8, 0xCE];
        let avcc = build_avc_decoder_config(&sps, &pps).unwrap();
        let mut tag = vec![0x17u8, 0x00, 0x00, 0x00, 0x00];
        tag.extend_from_slice(&avcc);

        let mut conv = aero_live_hls::FlvToTsConverter::new();
        assert!(!conv.is_ready());
        conv.push_video_tag(&tag, 0)
            .expect("muxer accepts our avcC");
        assert!(
            conv.is_ready(),
            "muxer should be ready after the seq header"
        );
    }

    // ------------------ HlsSink (channel level) ------------------

    /// Build a minimal Annex-B access unit from `(nal_header, body)` pairs.
    fn annex_b_au(nals: &[(u8, &[u8])]) -> Bytes {
        let mut out = BytesMut::new();
        for (header, body) in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.put_u8(*header);
            out.extend_from_slice(body);
        }
        out.freeze()
    }

    #[test]
    fn hls_sink_emits_segment_on_second_keyframe() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sink = HlsSink::new(tx);

        // Keyframe AU #1: SPS + PPS + IDR. Opens the first (still-open) segment;
        // nothing cut yet.
        let kf1 = annex_b_au(&[
            (0x67, &[0x42, 0x00, 0x1F]),
            (0x68, &[0xCE]),
            (0x65, &[0x10, 0x20, 0x30]),
        ]);
        sink.on_video_au(kf1, 0).unwrap();
        assert!(
            rx.try_recv().is_err(),
            "first keyframe must not cut a segment"
        );

        // A P-frame extends the open segment.
        let p = annex_b_au(&[(0x41, &[0x11, 0x22])]);
        sink.on_video_au(p, 3_000).unwrap();

        // Keyframe AU #2 closes the first segment (it has TS data) before muxing.
        let kf2 = annex_b_au(&[
            (0x67, &[0x42, 0x00, 0x1F]),
            (0x68, &[0xCE]),
            (0x65, &[0x40, 0x50]),
        ]);
        sink.on_video_au(kf2, 6_000).unwrap();

        let seg = rx.try_recv().expect("one segment cut at the 2nd keyframe");
        assert_eq!(sink.segments_emitted(), 1);
        assert_eq!(sink.video_aus(), 3);
        // Real MPEG-TS: starts with PAT then PMT (each a 188-byte sync packet).
        assert_eq!(seg.bytes[0], 0x47, "TS sync byte (PAT)");
        assert_eq!(seg.bytes[188], 0x47, "TS sync byte (PMT)");
        assert_eq!(seg.bytes.len() % 188, 0, "whole TS packets");
        // Duration spans PTS 0 → 6000 ticks = 1/15 s, strictly positive.
        let expected = 6_000.0f32 / 90_000.0;
        assert!(
            (seg.duration_secs - expected).abs() < 1e-4,
            "got {}",
            seg.duration_secs
        );
    }

    #[test]
    fn hls_sink_drops_inter_frames_before_first_keyframe() {
        // Mirrors FlvToTsConverter's own rule: no segment data until a keyframe.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sink = HlsSink::new(tx);
        sink.on_video_au(annex_b_au(&[(0x41, &[0x01])]), 0).unwrap();
        sink.on_video_au(annex_b_au(&[(0x41, &[0x02])]), 3_000)
            .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "no segment without a leading keyframe"
        );
        assert_eq!(sink.segments_emitted(), 0);
    }

    #[tokio::test]
    async fn hls_sink_writer_pair_persists_files() {
        let dir = crate::testutil::TempDir::new().unwrap();
        let stream_dir = dir.path().join("stream-xyz");
        let (mut sink, writer) = hls_sink(stream_dir.clone(), 2).await.unwrap();
        let writer_task = tokio::spawn(writer.run());

        // Two GOPs so the first segment is cut at the second keyframe; dropping
        // the sink flushes the trailing one and finalizes the manifest.
        let kf = |pts: u64| {
            let au = annex_b_au(&[
                (0x67, &[0x42, 0x00, 0x1F]),
                (0x68, &[0xCE]),
                (0x65, &[0xAA, 0xBB, 0xCC]),
            ]);
            (au, pts)
        };
        let (a, ap) = kf(0);
        sink.on_video_au(a, ap).unwrap();
        sink.on_video_au(annex_b_au(&[(0x41, &[0x01])]), 3_000)
            .unwrap();
        let (b, bp) = kf(6_000);
        sink.on_video_au(b, bp).unwrap();
        drop(sink); // closes the channel → writer finalizes

        let segments = writer_task.await.unwrap().unwrap();
        assert!(segments >= 1, "at least the first GOP segment persisted");
        assert!(stream_dir.join("0.ts").exists(), "0.ts on disk");
        let manifest = std::fs::read_to_string(stream_dir.join("index.m3u8")).unwrap();
        assert!(manifest.contains("#EXTM3U"));
        assert!(manifest.contains("#EXTINF"));
        assert!(manifest.contains("0.ts"));
        assert!(
            manifest.contains("#EXT-X-ENDLIST"),
            "finalized on sink drop"
        );
    }
}
