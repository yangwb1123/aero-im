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
//! - [`LoggingSink`] — a no-op default sink used until the server wires a real
//!   segmenter.
//!
//! ## What is wired vs. pending
//!
//! **Wired (here, tested):** depacketizer → Annex-B AU → [`MediaSink`], and the
//! Annex-B↔AVCC repackaging needed to feed `aero-live-hls`.
//!
//! **Pending live validation:** the final glue that (1) extracts SPS/PPS from
//! the first keyframe to synthesize an `AVCDecoderConfigurationRecord` and feed
//! it to `FlvToTsConverter::push_video_tag` as an AVC *sequence header*, then
//! (2) drives the `HlsWriter` segmenter on keyframe boundaries. That glue lives
//! naturally in the server's WHIP task (out of scope for this crate, which may
//! only edit `aero-live-whip`), and cannot be exercised without a real browser
//! publisher. The boundary is intentionally clean so the server can drop in a
//! `FlvToTsConverter`-backed `MediaSink` impl.

use bytes::{BufMut, Bytes, BytesMut};

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
}
