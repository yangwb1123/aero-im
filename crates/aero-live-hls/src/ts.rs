//! FLV (RTMP) → MPEG-TS muxer for HLS segments.
//!
//! Converts the H.264 (AVCC) + AAC (raw) payloads that `rml_rtmp` surfaces
//! into a sequence of MPEG-TS packets that browser HLS players can decode.
//!
//! Spec references:
//! - ISO/IEC 13818-1 (MPEG-2 Systems / TS)
//! - ISO/IEC 14496-15 (AVC file format — `AVCDecoderConfigurationRecord`)
//! - ISO/IEC 14496-3 (AAC `AudioSpecificConfig`)
//! - Adobe FLV File Format Specification v10 (Annex E)
//!
//! Layout notes
//! ------------
//! - PAT @ PID 0x0000, PMT @ PID 0x1000, video PES @ PID 0x100,
//!   audio PES @ PID 0x101.
//! - Video `stream_type` = 0x1B (H.264), audio = 0x0F (ADTS AAC).
//! - PCR is carried in the video adaptation field at each new keyframe.
//! - PTS/DTS in 90 kHz units.
//! - AVCC length-prefixed NALUs are converted to Annex-B start-code form;
//!   SPS+PPS are prepended to each keyframe access unit (HLS requirement).
//! - AAC raw is wrapped in ADTS (7-byte header) for `aac_es_id_3` PIDs.
//!
//! This module is a bit-packing muxer: narrowing casts into fixed-width protocol
//! fields are the intended operation (values are masked to the target width
//! first), so truncation/sign/wrap casts are allowed module-wide.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use bytes::{BufMut, Bytes, BytesMut};

const TS_PACKET_SIZE: usize = 188;
const TS_SYNC_BYTE: u8 = 0x47;

const PID_PAT: u16 = 0x0000;
const PID_PMT: u16 = 0x1000;
const PID_VIDEO: u16 = 0x100;
const PID_AUDIO: u16 = 0x101;

const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_ADTS_AAC: u8 = 0x0F;

const STREAM_ID_VIDEO: u8 = 0xE0;
const STREAM_ID_AUDIO: u8 = 0xC0;

/// Maximum value of the ADTS `aac_frame_length` field (13 bits, ISO/IEC
/// 13818-7). The whole frame — 7-byte ADTS header plus raw AAC payload —
/// must fit within this many bytes.
pub const ADTS_MAX_FRAME_LEN: usize = (1 << 13) - 1; // 8191

/// AAC sampling-frequency-index table from ISO/IEC 14496-3 §1.6.3.4.
const AAC_SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("flv: {0}")]
    Flv(&'static str),
    #[error("avc config: {0}")]
    AvcConfig(&'static str),
    #[error("aac config: {0}")]
    AacConfig(&'static str),
    /// An AAC raw frame is too large to be represented in an ADTS header.
    ///
    /// The ADTS `aac_frame_length` field is 13 bits wide (ISO/IEC 13818-7),
    /// so the total frame including the 7-byte header cannot exceed
    /// [`ADTS_MAX_FRAME_LEN`] bytes. We reject oversized frames instead of
    /// silently truncating the length field, which would desync every
    /// downstream AAC decoder for the remainder of the stream.
    #[error("aac frame: {0} bytes exceeds the 13-bit ADTS frame length limit")]
    AacFrameTooLarge(usize),
}

#[derive(Debug, Default, Clone)]
struct AvcCodec {
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
    length_size_minus_one: u8,
}

#[derive(Debug, Default, Clone, Copy)]
struct AacCodec {
    object_type: u8,
    sample_rate_index: u8,
    channel_config: u8,
}

/// Stateful converter; one per stream. Sequence headers (AVC config, AAC ASC)
/// are absorbed on receipt; subsequent media tags produce TS bytes.
#[derive(Default)]
pub struct FlvToTsConverter {
    avc: Option<AvcCodec>,
    aac: Option<AacCodec>,

    /// Pending TS bytes for the current segment. PAT/PMT have NOT been
    /// emitted yet for this segment when this is empty.
    pending: BytesMut,
    /// Continuity counters per PID (4-bit wrapping).
    cc_pat: u8,
    cc_pmt: u8,
    cc_video: u8,
    cc_audio: u8,
    /// True once we've seen the very first IDR frame; needed to keep segments
    /// starting on a keyframe.
    saw_first_keyframe: bool,

    /// Track the last DTS we wrote so the segmenter can compute durations.
    last_video_dts_90k: Option<u64>,
}

impl FlvToTsConverter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True once both AVC and AAC configs have been received.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.avc.is_some()
    }

    /// True if the segmenter has TS bytes ready to flush. For video streams,
    /// this implies we've already received a keyframe (inter frames before the
    /// first keyframe are dropped); audio-only streams flush as soon as a
    /// frame is ready.
    #[must_use]
    pub fn has_segment_data(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Process a single FLV video tag body.
    pub fn push_video_tag(&mut self, body: &[u8], timestamp_ms: u32) -> Result<(), MuxError> {
        if body.len() < 5 {
            return Err(MuxError::Flv("video tag too short"));
        }
        let frame_codec = body[0];
        let frame_type = frame_codec >> 4;
        let codec_id = frame_codec & 0x0F;
        if codec_id != 7 {
            return Ok(()); // only AVC supported
        }
        let avc_packet_type = body[1];
        let cts = read_i24_be(&body[2..5]);
        let payload = &body[5..];
        match avc_packet_type {
            0 => self.parse_avc_config(payload),
            1 => self.push_avc_nalus(payload, timestamp_ms, cts, frame_type == 1),
            2 => Ok(()), // end of sequence
            _ => Err(MuxError::Flv("unknown AVC packet type")),
        }
    }

    /// Process a single FLV audio tag body.
    pub fn push_audio_tag(&mut self, body: &[u8], timestamp_ms: u32) -> Result<(), MuxError> {
        if body.len() < 2 {
            return Err(MuxError::Flv("audio tag too short"));
        }
        let sound_format = body[0] >> 4;
        if sound_format != 10 {
            return Ok(()); // only AAC supported
        }
        let aac_packet_type = body[1];
        let payload = &body[2..];
        match aac_packet_type {
            0 => self.parse_aac_config(payload),
            1 => self.push_aac_raw(payload, timestamp_ms),
            _ => Err(MuxError::Flv("unknown AAC packet type")),
        }
    }

    /// Take the current segment's TS bytes. Prepends a fresh PAT+PMT pair.
    /// The internal buffer is reset so the next segment starts clean.
    pub fn drain_segment(&mut self) -> Bytes {
        let mut out = BytesMut::with_capacity(self.pending.len() + 2 * TS_PACKET_SIZE);
        out.extend_from_slice(&write_pat_packet(&mut self.cc_pat));
        out.extend_from_slice(&write_pmt_packet(&mut self.cc_pmt));
        out.extend_from_slice(&self.pending);
        self.pending.clear();
        // Carry the saw_first_keyframe flag forward — next segment still
        // needs to start with one but the AVC SPS/PPS reinsertion happens
        // per-keyframe anyway.
        out.freeze()
    }

    // ---------------- internals ----------------

    fn parse_avc_config(&mut self, body: &[u8]) -> Result<(), MuxError> {
        // AVCDecoderConfigurationRecord
        if body.len() < 7 {
            return Err(MuxError::AvcConfig("config too short"));
        }
        // skip configurationVersion(1) profile(1) profile_compat(1) level(1)
        let length_size_minus_one = body[4] & 0x03;
        let sps_count = body[5] & 0x1F;
        let mut p = 6;
        let mut sps = Vec::with_capacity(sps_count as usize);
        for _ in 0..sps_count {
            if p + 2 > body.len() {
                return Err(MuxError::AvcConfig("truncated sps len"));
            }
            let len = u16::from_be_bytes([body[p], body[p + 1]]) as usize;
            p += 2;
            if p + len > body.len() {
                return Err(MuxError::AvcConfig("truncated sps"));
            }
            sps.push(body[p..p + len].to_vec());
            p += len;
        }
        if p >= body.len() {
            return Err(MuxError::AvcConfig("missing pps count"));
        }
        let pps_count = body[p];
        p += 1;
        let mut pps = Vec::with_capacity(pps_count as usize);
        for _ in 0..pps_count {
            if p + 2 > body.len() {
                return Err(MuxError::AvcConfig("truncated pps len"));
            }
            let len = u16::from_be_bytes([body[p], body[p + 1]]) as usize;
            p += 2;
            if p + len > body.len() {
                return Err(MuxError::AvcConfig("truncated pps"));
            }
            pps.push(body[p..p + len].to_vec());
            p += len;
        }
        self.avc = Some(AvcCodec {
            sps,
            pps,
            length_size_minus_one,
        });
        Ok(())
    }

    fn parse_aac_config(&mut self, body: &[u8]) -> Result<(), MuxError> {
        // AudioSpecificConfig — first 2 bytes carry AOT(5) + SamplingFrequencyIndex(4) + ChannelConfig(4).
        if body.len() < 2 {
            return Err(MuxError::AacConfig("ASC too short"));
        }
        let b0 = body[0];
        let b1 = body[1];
        let object_type = b0 >> 3;
        let sample_rate_index = ((b0 & 0x07) << 1) | (b1 >> 7);
        let channel_config = (b1 >> 3) & 0x0F;
        if (sample_rate_index as usize) >= AAC_SAMPLE_RATES.len() {
            return Err(MuxError::AacConfig("unsupported sample rate index"));
        }
        self.aac = Some(AacCodec {
            object_type,
            sample_rate_index,
            channel_config,
        });
        Ok(())
    }

    fn push_avc_nalus(
        &mut self,
        body: &[u8],
        ts_ms: u32,
        cts_ms: i32,
        is_keyframe: bool,
    ) -> Result<(), MuxError> {
        let avc = self
            .avc
            .as_ref()
            .ok_or(MuxError::Flv("AVC NALUs without seq header"))?;
        let ls = (avc.length_size_minus_one + 1) as usize; // 1, 2, or 4
        let mut nalus: Vec<&[u8]> = Vec::new();
        let mut p = 0usize;
        while p + ls <= body.len() {
            let len = match ls {
                1 => body[p] as usize,
                2 => u16::from_be_bytes([body[p], body[p + 1]]) as usize,
                4 => u32::from_be_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]) as usize,
                _ => return Err(MuxError::Flv("unsupported AVC length size")),
            };
            p += ls;
            if p + len > body.len() {
                return Err(MuxError::Flv("truncated AVC NALU"));
            }
            nalus.push(&body[p..p + len]);
            p += len;
        }
        if !is_keyframe && !self.saw_first_keyframe {
            // Drop inter frames before the first keyframe.
            return Ok(());
        }
        let dts_90k = u64::from(ts_ms) * 90;
        let pts_90k = (i64::from(ts_ms) + i64::from(cts_ms)).max(0) as u64 * 90;
        let mut access_unit = BytesMut::new();
        // AUD NAL unit (delimiter) — required at the start of each Annex-B access unit.
        access_unit.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x09, 0xF0]);
        if is_keyframe {
            for sps in &avc.sps {
                access_unit.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                access_unit.extend_from_slice(sps);
            }
            for pps in &avc.pps {
                access_unit.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                access_unit.extend_from_slice(pps);
            }
        }
        for nalu in &nalus {
            access_unit.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            access_unit.extend_from_slice(nalu);
        }
        let pes = build_pes(STREAM_ID_VIDEO, Some(pts_90k), Some(dts_90k), &access_unit);
        let pcr = if is_keyframe { Some(dts_90k) } else { None };
        let packets = write_pes_into_ts(PID_VIDEO, &mut self.cc_video, &pes, pcr);
        self.pending.extend_from_slice(&packets);

        if is_keyframe {
            self.saw_first_keyframe = true;
        }
        self.last_video_dts_90k = Some(dts_90k);
        Ok(())
    }

    fn push_aac_raw(&mut self, body: &[u8], ts_ms: u32) -> Result<(), MuxError> {
        let aac = self
            .aac
            .as_ref()
            .ok_or(MuxError::Flv("AAC raw without seq header"))?;
        // Wrap raw AAC in ADTS.
        let frame_len = 7 + body.len();
        // The ADTS `aac_frame_length` field is only 13 bits wide. Encoding a
        // larger value would silently truncate it (e.g. 8192 -> 0), producing a
        // header that lies about the frame size and desyncs the decoder for the
        // rest of the stream. Reject rather than corrupt.
        if frame_len > ADTS_MAX_FRAME_LEN {
            return Err(MuxError::AacFrameTooLarge(frame_len));
        }
        let mut adts = BytesMut::with_capacity(frame_len);
        let profile_minus_1 = aac.object_type.saturating_sub(1) & 0x03;
        let sri = aac.sample_rate_index & 0x0F;
        let chc = aac.channel_config & 0x07;
        adts.put_u8(0xFF);
        adts.put_u8(0xF1); // MPEG-4, layer 0, protection_absent=1
        adts.put_u8((profile_minus_1 << 6) | (sri << 2) | (chc >> 2));
        adts.put_u8(((chc & 0x03) << 6) | (((frame_len >> 11) & 0x03) as u8));
        adts.put_u8(((frame_len >> 3) & 0xFF) as u8);
        adts.put_u8((((frame_len & 0x07) << 5) as u8) | 0x1F);
        adts.put_u8(0xFC);
        adts.extend_from_slice(body);

        let pts_90k = u64::from(ts_ms) * 90;
        let pes = build_pes(STREAM_ID_AUDIO, Some(pts_90k), None, &adts);
        let packets = write_pes_into_ts(PID_AUDIO, &mut self.cc_audio, &pes, None);
        self.pending.extend_from_slice(&packets);
        Ok(())
    }
}

// ---------------- TS packet helpers ----------------

fn write_pes_into_ts(pid: u16, cc: &mut u8, pes: &[u8], pcr_90k: Option<u64>) -> Vec<u8> {
    let mut out = Vec::with_capacity(pes.len().div_ceil(184) * TS_PACKET_SIZE);
    let mut offset = 0;
    let mut first = true;
    while offset < pes.len() {
        let mut pkt = [0u8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = ((u8::from(first) & 1) << 6) | ((pid >> 8) & 0x1F) as u8;
        pkt[2] = (pid & 0xFF) as u8;
        let cc_val = *cc & 0x0F;
        *cc = cc.wrapping_add(1) & 0x0F;

        let want_pcr = first && pcr_90k.is_some();
        let mut adaptation: Vec<u8> = Vec::new();
        if want_pcr {
            let pcr = pcr_90k.unwrap();
            // adaptation_field_length will be 7 (1 flag byte + 6 PCR bytes)
            adaptation.push(7);
            adaptation.push(0x10); // PCR_flag = 1
                                   // 33-bit PCR base + 6 reserved bits + 9-bit extension
            let base = pcr & ((1u64 << 33) - 1);
            let ext = 0u16; // 9-bit, in 27 MHz / 300 units; 0 is fine
            adaptation.push(((base >> 25) & 0xFF) as u8);
            adaptation.push(((base >> 17) & 0xFF) as u8);
            adaptation.push(((base >> 9) & 0xFF) as u8);
            adaptation.push(((base >> 1) & 0xFF) as u8);
            adaptation.push((((base & 1) << 7) as u8) | 0x7E | (((ext >> 8) & 0x01) as u8));
            adaptation.push((ext & 0xFF) as u8);
        }
        let max_payload = 184 - adaptation.len();
        let remaining = pes.len() - offset;
        let chunk_len = remaining.min(max_payload);
        let need_stuffing = max_payload - chunk_len;
        // afc: 01 = payload only, 10 = adaptation only, 11 = adaptation + payload
        let mut afc: u8 = 0b01;
        if !adaptation.is_empty() {
            afc = 0b11;
        }
        if need_stuffing > 0 {
            // Add an adaptation field (or extend existing) to absorb stuffing bytes.
            afc = 0b11;
            if adaptation.is_empty() {
                // adaptation_field_length = need_stuffing - 1 (for the flag byte)
                // but length must be >= 1; if we need 0 stuffing past the flag, length=1 + 0 flags.
                if need_stuffing == 1 {
                    adaptation.push(0); // length 0; no flag byte
                } else {
                    adaptation.push((need_stuffing - 1) as u8);
                    adaptation.push(0x00); // no flags
                    adaptation.resize(adaptation.len() + (need_stuffing - 2), 0xFF);
                }
            } else {
                // Extend existing adaptation by adding stuffing bytes.
                if let Some(first_len) = adaptation.get_mut(0) {
                    *first_len = first_len.wrapping_add(need_stuffing as u8);
                }
                adaptation.resize(adaptation.len() + need_stuffing, 0xFF);
            }
        }
        pkt[3] = (afc << 4) | cc_val;
        let mut wp = 4usize;
        for b in &adaptation {
            pkt[wp] = *b;
            wp += 1;
        }
        let payload_start = wp;
        let take = TS_PACKET_SIZE - payload_start;
        let chunk_len = chunk_len.min(take); // safety
        pkt[payload_start..payload_start + chunk_len]
            .copy_from_slice(&pes[offset..offset + chunk_len]);
        offset += chunk_len;
        out.extend_from_slice(&pkt);
        first = false;
    }
    out
}

fn build_pes(stream_id: u8, pts_90k: Option<u64>, dts_90k: Option<u64>, payload: &[u8]) -> Vec<u8> {
    let pts_dts_flags: u8 = match (pts_90k.is_some(), dts_90k.is_some()) {
        (true, true) => 0b11,
        (true, false) => 0b10,
        _ => 0b00,
    };
    let header_data_len = match pts_dts_flags {
        0b11 => 10,
        0b10 => 5,
        _ => 0,
    };
    let total_header = 9 + header_data_len;
    let mut buf = Vec::with_capacity(total_header + payload.len());
    buf.extend_from_slice(&[0x00, 0x00, 0x01, stream_id]);
    // PES_packet_length = total bytes following this field, OR 0 for video to mean unbounded.
    // Use 0 for video (allows packet to exceed 65535). For audio, set length when small enough.
    let length_field = if stream_id == STREAM_ID_VIDEO {
        0u16
    } else {
        let n = 3 + header_data_len + payload.len();
        if n > 0xFFFF {
            0
        } else {
            n as u16
        }
    };
    buf.extend_from_slice(&length_field.to_be_bytes());
    buf.push(0x80); // marker bits + flags (no scrambling, no priority, etc.)
    buf.push(pts_dts_flags << 6); // PTS_DTS_flags + other flags
    buf.push(header_data_len as u8);
    match pts_dts_flags {
        0b11 => {
            buf.extend_from_slice(&encode_pts_dts(0b0011, pts_90k.unwrap()));
            buf.extend_from_slice(&encode_pts_dts(0b0001, dts_90k.unwrap()));
        }
        0b10 => buf.extend_from_slice(&encode_pts_dts(0b0010, pts_90k.unwrap())),
        _ => {}
    }
    buf.extend_from_slice(payload);
    buf
}

fn encode_pts_dts(prefix: u8, ts_90k: u64) -> [u8; 5] {
    let t = ts_90k & ((1u64 << 33) - 1);
    let mut o = [0u8; 5];
    o[0] = (prefix << 4) | (((t >> 30) & 0x07) as u8) << 1 | 1;
    o[1] = ((t >> 22) & 0xFF) as u8;
    o[2] = ((((t >> 15) & 0x7F) as u8) << 1) | 1;
    o[3] = ((t >> 7) & 0xFF) as u8;
    o[4] = (((t & 0x7F) as u8) << 1) | 1;
    o
}

fn write_pat_packet(cc: &mut u8) -> [u8; TS_PACKET_SIZE] {
    let mut pkt = [0xFFu8; TS_PACKET_SIZE];
    pkt[0] = TS_SYNC_BYTE;
    pkt[1] = 0x40 | ((PID_PAT >> 8) & 0x1F) as u8; // PUSI=1, PID hi
    pkt[2] = (PID_PAT & 0xFF) as u8;
    let cc_val = *cc & 0x0F;
    *cc = cc.wrapping_add(1) & 0x0F;
    pkt[3] = 0x10 | cc_val; // afc=01, cc
    pkt[4] = 0x00; // pointer field

    // PAT section
    let mut section = Vec::with_capacity(20);
    section.push(0x00); // table_id
                        // section_syntax_indicator=1, '0', reserved '11', section_length(12) — fill later
    section.push(0xB0);
    section.push(0x00); // section_length lo placeholder
    section.extend_from_slice(&1u16.to_be_bytes()); // transport_stream_id
    section.push(0xC1); // reserved(2)=11, version_number(5)=0, current_next=1
    section.push(0x00); // section_number
    section.push(0x00); // last_section_number
                        // program_number=1, program_map_PID=PID_PMT
    section.extend_from_slice(&1u16.to_be_bytes());
    section.extend_from_slice(&((0xE000 | (PID_PMT & 0x1FFF)).to_be_bytes()));
    // section_length = bytes from section_length_lo onward (i.e. after the first 3 bytes)
    let section_len = section.len() + 4 - 3; // +4 for CRC
    section[1] = 0xB0 | (((section_len >> 8) & 0x0F) as u8);
    section[2] = (section_len & 0xFF) as u8;
    let crc = mpeg2_crc32(&section);
    section.extend_from_slice(&crc.to_be_bytes());

    // Copy section into TS payload.
    let payload_start = 5;
    for (i, b) in section.iter().enumerate() {
        if payload_start + i >= TS_PACKET_SIZE {
            break;
        }
        pkt[payload_start + i] = *b;
    }
    pkt
}

fn write_pmt_packet(cc: &mut u8) -> [u8; TS_PACKET_SIZE] {
    let mut pkt = [0xFFu8; TS_PACKET_SIZE];
    pkt[0] = TS_SYNC_BYTE;
    pkt[1] = 0x40 | ((PID_PMT >> 8) & 0x1F) as u8;
    pkt[2] = (PID_PMT & 0xFF) as u8;
    let cc_val = *cc & 0x0F;
    *cc = cc.wrapping_add(1) & 0x0F;
    pkt[3] = 0x10 | cc_val;
    pkt[4] = 0x00; // pointer field

    let mut section = Vec::with_capacity(32);
    section.push(0x02); // table_id PMT
    section.push(0xB0); // section_syntax + reserved
    section.push(0x00); // length lo placeholder
    section.extend_from_slice(&1u16.to_be_bytes()); // program_number
    section.push(0xC1);
    section.push(0x00);
    section.push(0x00);
    // PCR_PID = PID_VIDEO
    section.extend_from_slice(&((0xE000 | (PID_VIDEO & 0x1FFF)).to_be_bytes()));
    // program_info_length = 0
    section.extend_from_slice(&0xF000u16.to_be_bytes());
    // Video stream
    section.push(STREAM_TYPE_H264);
    section.extend_from_slice(&((0xE000 | (PID_VIDEO & 0x1FFF)).to_be_bytes()));
    section.extend_from_slice(&0xF000u16.to_be_bytes()); // ES_info_length=0
                                                         // Audio stream
    section.push(STREAM_TYPE_ADTS_AAC);
    section.extend_from_slice(&((0xE000 | (PID_AUDIO & 0x1FFF)).to_be_bytes()));
    section.extend_from_slice(&0xF000u16.to_be_bytes());

    let section_len = section.len() + 4 - 3;
    section[1] = 0xB0 | (((section_len >> 8) & 0x0F) as u8);
    section[2] = (section_len & 0xFF) as u8;
    let crc = mpeg2_crc32(&section);
    section.extend_from_slice(&crc.to_be_bytes());

    let payload_start = 5;
    for (i, b) in section.iter().enumerate() {
        if payload_start + i >= TS_PACKET_SIZE {
            break;
        }
        pkt[payload_start + i] = *b;
    }
    pkt
}

fn read_i24_be(b: &[u8]) -> i32 {
    let v = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
    // sign-extend from 24 bits
    if v & 0x0080_0000 != 0 {
        (v | 0xFF00_0000) as i32
    } else {
        v as i32
    }
}

/// MPEG-2 CRC32 (polynomial 0x04C11DB7, initial value 0xFFFFFFFF, no reflection).
fn mpeg2_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        let mut x = u32::from(b) << 24;
        for _ in 0..8 {
            let bit = (crc ^ x) & 0x8000_0000;
            crc <<= 1;
            x <<= 1;
            if bit != 0 {
                crc ^= 0x04C1_1DB7;
            }
        }
    }
    crc
}

// ---------------- public re-exports ----------------

impl FlvToTsConverter {
    /// Read the last DTS we wrote (90 kHz). Useful for the segmenter to know
    /// how long the current segment has been growing.
    #[must_use]
    pub fn last_video_dts_90k(&self) -> Option<u64> {
        self.last_video_dts_90k
    }
}

/// Convenience: produce an empty MPEG-TS segment containing only PAT+PMT.
/// Used by the segmenter when it has no media yet but wants a valid file on disk.
#[must_use]
pub fn empty_ts_segment() -> Bytes {
    let mut pat_cc = 0u8;
    let mut pmt_counter = 0u8;
    let mut out = BytesMut::with_capacity(2 * TS_PACKET_SIZE);
    out.extend_from_slice(&write_pat_packet(&mut pat_cc));
    out.extend_from_slice(&write_pmt_packet(&mut pmt_counter));
    out.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_self_consistent() {
        // Pin a known-good output of this MPEG-2 CRC32 implementation so later
        // refactors that change behavior are caught immediately.
        assert_eq!(mpeg2_crc32(b""), 0xFFFF_FFFF);
        assert_eq!(mpeg2_crc32(b"test"), 0xCC74_3053);
    }

    #[test]
    fn pat_packet_starts_with_sync_byte() {
        let mut cc = 0;
        let pat = write_pat_packet(&mut cc);
        assert_eq!(pat[0], 0x47);
        assert_eq!(pat[1] & 0x40, 0x40); // PUSI set
        assert_eq!(pat[3] & 0x30, 0x10); // payload only
    }

    #[test]
    fn pmt_packet_starts_with_sync_byte() {
        let mut cc = 0;
        let pmt = write_pmt_packet(&mut cc);
        assert_eq!(pmt[0], 0x47);
    }

    #[test]
    fn drain_segment_starts_with_pat_pmt() {
        let mut c = FlvToTsConverter::new();
        let seg = c.drain_segment();
        assert_eq!(seg.len(), 2 * TS_PACKET_SIZE);
        assert_eq!(seg[0], 0x47); // PAT
        assert_eq!(seg[TS_PACKET_SIZE], 0x47); // PMT
    }

    #[test]
    fn empty_ts_segment_is_pat_plus_pmt() {
        let s = empty_ts_segment();
        assert_eq!(s.len(), 2 * TS_PACKET_SIZE);
    }

    #[test]
    fn pes_encodes_pts_dts_when_both_present() {
        let pes = build_pes(STREAM_ID_VIDEO, Some(9000), Some(0), &[0xAA, 0xBB]);
        // start_code + stream_id + length(2) + flags(2) + header_data_len(1) + 10 PTS/DTS + payload(2)
        assert!(pes.len() >= 9 + 10 + 2);
        assert_eq!(pes[0..4], [0x00, 0x00, 0x01, STREAM_ID_VIDEO]);
        // PTS_DTS_flags = 0b11 → byte 7 has top 2 bits set.
        assert_eq!(pes[7] >> 6, 0b11);
        assert_eq!(pes[8], 10);
    }

    #[test]
    fn aac_adts_wrap_has_correct_sync() {
        let mut c = FlvToTsConverter::new();
        // Pretend ASC: AOT=2, sample_rate_idx=4 (44100), channel_config=2 (stereo)
        let asc = [(2u8 << 3) | (4u8 >> 1), (2u8 << 3)];
        c.aac = Some(AacCodec {
            object_type: 2,
            sample_rate_index: 4,
            channel_config: 2,
        });
        let _ = asc;
        c.push_aac_raw(&[0x21, 0x10, 0x05], 0).unwrap();
        // Expect at least one TS packet containing ADTS sync 0xFFF
        let any = c
            .pending
            .windows(2)
            .any(|w| w[0] == 0xFF && (w[1] & 0xF0) == 0xF0);
        assert!(any, "ADTS sync not found");
    }

    #[test]
    fn read_i24_be_handles_negative() {
        assert_eq!(read_i24_be(&[0xFF, 0xFF, 0xFF]), -1);
        assert_eq!(read_i24_be(&[0x00, 0x00, 0x01]), 1);
        assert_eq!(read_i24_be(&[0x80, 0x00, 0x00]), -8_388_608);
    }

    /// Reconstruct the 13-bit `aac_frame_length` field from the three ADTS
    /// header bytes that carry it, exactly as a decoder would.
    fn adts_frame_length(adts: &[u8]) -> usize {
        ((adts[3] as usize & 0x03) << 11)
            | ((adts[4] as usize) << 3)
            | ((adts[5] as usize >> 5) & 0x07)
    }

    #[test]
    fn aac_frame_length_at_13bit_boundary_is_exact() {
        let mut c = FlvToTsConverter::new();
        c.aac = Some(AacCodec {
            object_type: 2,
            sample_rate_index: 4,
            channel_config: 2,
        });
        // Largest payload that still fits in the 13-bit ADTS length field.
        let payload = vec![0u8; ADTS_MAX_FRAME_LEN - 7];
        c.push_aac_raw(&payload, 0).unwrap();
        // The first ADTS header lives at the start of the PES payload; locate
        // the 0xFFF sync word and decode its declared frame length.
        let sync = c
            .pending
            .windows(2)
            .position(|w| w[0] == 0xFF && (w[1] & 0xF0) == 0xF0)
            .expect("ADTS sync not found");
        let header = &c.pending[sync..sync + 6];
        assert_eq!(
            adts_frame_length(header),
            ADTS_MAX_FRAME_LEN,
            "frame length at the boundary must round-trip exactly"
        );
    }

    #[test]
    fn aac_frame_exceeding_13bit_limit_is_rejected() {
        let mut c = FlvToTsConverter::new();
        c.aac = Some(AacCodec {
            object_type: 2,
            sample_rate_index: 4,
            channel_config: 2,
        });
        // One byte past the boundary: total frame_len == 8192, which would
        // truncate to 0 in the 13-bit field if written naively.
        let payload = vec![0u8; ADTS_MAX_FRAME_LEN - 7 + 1];
        let err = c.push_aac_raw(&payload, 0).unwrap_err();
        match err {
            MuxError::AacFrameTooLarge(n) => assert_eq!(n, ADTS_MAX_FRAME_LEN + 1),
            other => panic!("expected AacFrameTooLarge, got {other:?}"),
        }
        // Nothing should have been written to the pending buffer on rejection.
        assert!(
            c.pending.is_empty(),
            "rejected frame must not emit partial TS bytes"
        );
    }
}
