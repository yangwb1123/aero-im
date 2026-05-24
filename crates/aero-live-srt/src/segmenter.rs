//! MPEG-TS → HLS segmenter.
//!
//! SRT (like RTMP-over-TS or `ffmpeg -f mpegts`) delivers an already-muxed
//! MPEG-2 Transport Stream. Unlike the RTMP path — which receives FLV tags and
//! *builds* TS via [`aero_live_hls::FlvToTsConverter`] — here the bytes are
//! already TS, so we must **parse** them and decide where to cut HLS segments.
//!
//! ## What this does
//!
//! 1. Reassembles a contiguous byte stream into 188-byte TS packets (sync byte
//!    `0x47`), tolerating partial packets across `push` calls.
//! 2. Parses the PAT (PID `0x0000`) to discover the PMT PID, then the PMT to
//!    discover the **video elementary PID** (`stream_type == 0x1B`, H.264).
//! 3. Within video-PID packets, tracks PES boundaries (PUSI) and scans the PES
//!    payload for an H.264 **IDR** NAL unit (`nal_unit_type == 5`) using
//!    Annex-B start codes (`00 00 01` / `00 00 00 01`).
//! 4. Emits a [`SegmentCut`] *before* each keyframe-bearing access unit (after
//!    the first), so every HLS segment begins on a keyframe — the property
//!    browser players require for clean seeking and bitrate switching.
//!
//! Spec references mirror `aero-live-hls::ts`:
//! - ISO/IEC 13818-1 (MPEG-2 Systems / TS, PSI tables)
//! - ISO/IEC 14496-10 (H.264 / Annex-B NAL units)
//!
//! ## Scope
//!
//! This is the transport-agnostic, fully unit-tested core. It does not touch
//! the network or the filesystem — callers feed it bytes and act on the cut
//! points it reports. Keeping it pure makes it deterministic to test with
//! synthetic packets.

/// Size of a single MPEG-TS packet in bytes.
pub const TS_PACKET_SIZE: usize = 188;

/// MPEG-TS sync byte that prefixes every packet.
pub const TS_SYNC_BYTE: u8 = 0x47;

/// PID carrying the Program Association Table.
const PID_PAT: u16 = 0x0000;

/// PID value reserved for null/stuffing packets (never carries PSI or PES).
const PID_NULL: u16 = 0x1FFF;

/// `stream_type` for an H.264 (AVC) video elementary stream in the PMT.
const STREAM_TYPE_H264: u8 = 0x1B;

/// A decision the segmenter surfaces to its caller as packets are pushed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentEvent {
    /// The packet was buffered into the current segment; nothing else to do.
    Buffered,
    /// A new segment should start *before* this keyframe. The caller should
    /// flush everything accumulated so far (via [`MpegTsSegmenter::take_segment`])
    /// and only then append the packets that produced this event.
    ///
    /// Emitted at every keyframe boundary *except the very first*, so the first
    /// segment is opened lazily rather than producing an empty leading segment.
    CutBeforeKeyframe,
}

/// Parser state while walking the PSI tables to learn the video PID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PsiState {
    /// Haven't seen a PAT yet; PMT PID unknown.
    NeedPat,
    /// PAT parsed; waiting for the PMT on `pmt_pid`.
    NeedPmt { pmt_pid: u16 },
    /// PMT parsed; `video_pid` resolved and segmenting is active.
    Ready { video_pid: u16 },
}

/// Stateful MPEG-TS → HLS segmenter. One instance per stream.
///
/// Bytes are pushed in arbitrary chunk sizes via [`Self::push`]; the segmenter
/// reassembles whole 188-byte packets and appends them to the segment currently
/// being built. When a keyframe (other than the first) is reached it closes the
/// open segment, moves it into a "completed" slot, starts a fresh segment with
/// that keyframe's packet, and reports [`SegmentEvent::CutBeforeKeyframe`].
/// The completed segment bytes are then retrieved with [`Self::take_segment`].
///
/// This makes a cut atomic: the keyframe packet always belongs to the *new*
/// segment, so [`Self::take_segment`] never returns a segment that bleeds into
/// the next keyframe.
#[derive(Debug)]
pub struct MpegTsSegmenter {
    /// Reassembly buffer for bytes that don't yet form a whole packet.
    partial: Vec<u8>,
    /// Accumulated TS packets for the segment currently being built.
    current: Vec<u8>,
    /// A just-closed segment awaiting retrieval via [`Self::take_segment`].
    /// At most one is held; a second cut before draining is not expected
    /// because callers flush on every [`SegmentEvent::CutBeforeKeyframe`].
    completed: Option<Vec<u8>>,
    /// PSI discovery state machine.
    psi: PsiState,
    /// Whether we've already opened the first segment (i.e. seen the first
    /// keyframe). Used to suppress a spurious cut before the leading keyframe.
    saw_first_keyframe: bool,
    /// Total whole packets consumed — handy for tests and diagnostics.
    packets_seen: u64,
}

impl Default for MpegTsSegmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl MpegTsSegmenter {
    /// Create an empty segmenter that has not yet seen any PSI.
    #[must_use]
    pub fn new() -> Self {
        Self {
            partial: Vec::with_capacity(TS_PACKET_SIZE),
            current: Vec::new(),
            completed: None,
            psi: PsiState::NeedPat,
            saw_first_keyframe: false,
            packets_seen: 0,
        }
    }

    /// The resolved video elementary PID, once the PMT has been parsed.
    #[must_use]
    pub fn video_pid(&self) -> Option<u16> {
        match self.psi {
            PsiState::Ready { video_pid } => Some(video_pid),
            _ => None,
        }
    }

    /// Number of whole 188-byte packets processed so far.
    #[must_use]
    pub fn packets_seen(&self) -> u64 {
        self.packets_seen
    }

    /// Bytes currently buffered for the open segment (not yet drained).
    #[must_use]
    pub fn current_len(&self) -> usize {
        self.current.len()
    }

    /// True once the first keyframe has opened a segment.
    #[must_use]
    pub fn has_started(&self) -> bool {
        self.saw_first_keyframe
    }

    /// Feed a chunk of the incoming TS byte stream.
    ///
    /// Returns one [`SegmentEvent`] per whole packet consumed, in order. A
    /// [`SegmentEvent::CutBeforeKeyframe`] means a segment has just been closed
    /// and is waiting in the completed slot: the caller should drain it with
    /// [`Self::take_segment`]. The keyframe packet that triggered the cut has
    /// already been placed at the head of the *new* (now-open) segment.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SegmentEvent> {
        let mut events = Vec::new();
        self.partial.extend_from_slice(bytes);

        let mut consumed = 0usize;
        while self.partial.len() - consumed >= TS_PACKET_SIZE {
            let pkt = &self.partial[consumed..consumed + TS_PACKET_SIZE];
            // Resync: a well-formed packet starts with 0x47. If we're out of
            // alignment (lost bytes upstream), step forward one byte at a time
            // until the sync byte reappears rather than emitting garbage.
            if pkt[0] != TS_SYNC_BYTE {
                consumed += 1;
                continue;
            }
            let packet: [u8; TS_PACKET_SIZE] = pkt.try_into().expect("slice is 188 bytes");
            consumed += TS_PACKET_SIZE;
            self.packets_seen += 1;
            if let Some(ev) = self.handle_packet(&packet) {
                events.push(ev);
            } else {
                events.push(SegmentEvent::Buffered);
            }
        }

        // Drop the consumed prefix, keep any trailing partial packet bytes.
        if consumed > 0 {
            self.partial.drain(0..consumed);
        }
        events
    }

    /// Retrieve a closed segment.
    ///
    /// After a [`SegmentEvent::CutBeforeKeyframe`] this returns the segment that
    /// was just closed (the keyframe that triggered the cut is *not* included —
    /// it heads the next segment). When no segment has been closed, it flushes
    /// whatever is currently open instead — used by [`Self::finish`]-style
    /// drains at end-of-stream. Returns an empty `Vec` if nothing is buffered.
    pub fn take_segment(&mut self) -> Vec<u8> {
        if let Some(done) = self.completed.take() {
            return done;
        }
        std::mem::take(&mut self.current)
    }

    /// Whether any bytes are buffered — either a closed segment awaiting
    /// retrieval or an open one still accumulating.
    #[must_use]
    pub fn has_segment_data(&self) -> bool {
        self.completed.as_ref().is_some_and(|c| !c.is_empty()) || !self.current.is_empty()
    }

    // ---------------- internals ----------------

    /// Process one whole, sync-aligned 188-byte packet. Returns `Some(event)`
    /// only for a keyframe cut; `None` means "buffered normally" (the caller
    /// maps that to [`SegmentEvent::Buffered`]).
    fn handle_packet(&mut self, packet: &[u8; TS_PACKET_SIZE]) -> Option<SegmentEvent> {
        let pusi = packet[1] & 0x40 != 0;
        let pid = (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]);
        let afc = (packet[3] >> 4) & 0x03;

        if pid == PID_NULL {
            // Null packets are pure padding — don't even buffer them.
            return None;
        }

        // Compute where the TS payload begins (after any adaptation field).
        let payload = ts_payload(packet, afc);

        match self.psi {
            PsiState::NeedPat if pid == PID_PAT => {
                if let Some(pmt_pid) = parse_pat(payload, pusi) {
                    self.psi = PsiState::NeedPmt { pmt_pid };
                }
                self.current.extend_from_slice(packet);
                None
            }
            PsiState::NeedPmt { pmt_pid } if pid == pmt_pid => {
                if let Some(video_pid) = parse_pmt(payload, pusi) {
                    self.psi = PsiState::Ready { video_pid };
                }
                self.current.extend_from_slice(packet);
                None
            }
            PsiState::Ready { video_pid } if pid == video_pid => {
                self.handle_video_packet(packet, payload, pusi)
            }
            _ => {
                // PAT/PMT carriage that arrives again, audio packets, or any
                // other PID: keep them in the segment verbatim so the muxed
                // stream stays intact.
                self.current.extend_from_slice(packet);
                None
            }
        }
    }

    /// Handle a packet on the video PID, detecting keyframe-aligned cut points.
    ///
    /// A PUSI marks the start of a new access unit (PES). We classify it by
    /// scanning the ES payload after the PES header for an IDR NAL. On a
    /// keyframe other than the first, we close the open segment into `completed`
    /// and begin a fresh one *with this keyframe packet*, so the cut is atomic.
    fn handle_video_packet(
        &mut self,
        packet: &[u8; TS_PACKET_SIZE],
        payload: &[u8],
        pusi: bool,
    ) -> Option<SegmentEvent> {
        if pusi && pes_payload_has_idr(payload) {
            if self.saw_first_keyframe {
                // Close the open segment and start a new one headed by this
                // keyframe packet.
                self.completed = Some(std::mem::take(&mut self.current));
                self.current.extend_from_slice(packet);
                return Some(SegmentEvent::CutBeforeKeyframe);
            }
            // First keyframe ever: open the first segment here, no cut.
            self.saw_first_keyframe = true;
        }
        // Non-keyframe PUSI, continuation packets, and the first keyframe all
        // append to the currently-open segment.
        self.current.extend_from_slice(packet);
        None
    }
}

/// Slice the TS payload out of a packet given its `adaptation_field_control`.
///
/// `afc`: `01` = payload only, `10` = adaptation only (no payload),
/// `11` = adaptation field followed by payload. The first payload byte sits at
/// offset `4 + 1 + adaptation_field_length` when an adaptation field is present.
fn ts_payload(packet: &[u8; TS_PACKET_SIZE], afc: u8) -> &[u8] {
    match afc {
        0b01 => &packet[4..],
        0b11 => {
            let af_len = packet[4] as usize;
            let start = 5 + af_len;
            if start <= TS_PACKET_SIZE {
                &packet[start..]
            } else {
                &packet[TS_PACKET_SIZE..] // malformed: empty payload
            }
        }
        // 0b10 (adaptation only) or 0b00 (reserved): no payload.
        _ => &packet[TS_PACKET_SIZE..],
    }
}

/// Strip the `pointer_field` that precedes a PSI section in a PUSI packet.
fn psi_section(payload: &[u8], pusi: bool) -> Option<&[u8]> {
    if !pusi || payload.is_empty() {
        return None;
    }
    let pointer = payload[0] as usize;
    let start = 1 + pointer;
    payload.get(start..)
}

/// Parse a PAT section and return the PMT PID for the first program with a
/// non-zero `program_number` (`program_number` 0 is the network PID, skipped).
fn parse_pat(payload: &[u8], pusi: bool) -> Option<u16> {
    let section = psi_section(payload, pusi)?;
    // table_id(1) syntax+len(2) tsid(2) ver(1) sec_no(1) last_sec(1) = 8 byte
    // header before the program loop; then 4 trailing CRC bytes.
    if section.len() < 12 || section[0] != 0x00 {
        return None;
    }
    let section_length = (usize::from(section[1] & 0x0F) << 8) | usize::from(section[2]);
    // section_length counts bytes after byte index 2, including the 4-byte CRC.
    let body_end = (3 + section_length).min(section.len());
    let mut p = 8usize;
    while p + 4 <= body_end.saturating_sub(4) {
        let program_number = (u16::from(section[p]) << 8) | u16::from(section[p + 1]);
        let pid = (u16::from(section[p + 2] & 0x1F) << 8) | u16::from(section[p + 3]);
        p += 4;
        if program_number != 0 {
            return Some(pid);
        }
    }
    None
}

/// Parse a PMT section and return the elementary PID of the first H.264 stream.
fn parse_pmt(payload: &[u8], pusi: bool) -> Option<u16> {
    let section = psi_section(payload, pusi)?;
    if section.len() < 16 || section[0] != 0x02 {
        return None;
    }
    let section_length = (usize::from(section[1] & 0x0F) << 8) | usize::from(section[2]);
    let body_end = (3 + section_length).min(section.len());
    // Fixed PMT header is 12 bytes (through program_info_length); CRC is last 4.
    let program_info_length =
        (usize::from(section[10] & 0x0F) << 8) | usize::from(section[11]);
    let mut p = 12 + program_info_length;
    let loop_end = body_end.saturating_sub(4); // stop before CRC
    while p + 5 <= loop_end {
        let stream_type = section[p];
        let elementary_pid =
            (u16::from(section[p + 1] & 0x1F) << 8) | u16::from(section[p + 2]);
        let es_info_length =
            (usize::from(section[p + 3] & 0x0F) << 8) | usize::from(section[p + 4]);
        if stream_type == STREAM_TYPE_H264 {
            return Some(elementary_pid);
        }
        p += 5 + es_info_length;
    }
    None
}

/// Scan a video PES packet payload for an H.264 IDR (key) NAL unit.
///
/// The payload begins with a PES header (`00 00 01 <stream_id> ...`). We skip
/// the header using `PES_header_data_length`, then walk Annex-B start codes in
/// the elementary stream, returning `true` if any NAL unit has
/// `nal_unit_type == 5` (coded slice of an IDR picture).
fn pes_payload_has_idr(payload: &[u8]) -> bool {
    let es = strip_pes_header(payload).unwrap_or(payload);
    annexb_has_idr(es)
}

/// Remove the PES header, returning the elementary-stream bytes that follow.
fn strip_pes_header(payload: &[u8]) -> Option<&[u8]> {
    // packet_start_code_prefix(3) = 00 00 01, then stream_id(1).
    if payload.len() < 9 || payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
        return None;
    }
    // Optional-header form is present when the next 2 bits are '10'.
    if payload[6] & 0xC0 != 0x80 {
        return None;
    }
    let pes_header_data_length = payload[8] as usize;
    let es_start = 9 + pes_header_data_length;
    payload.get(es_start..)
}

/// Walk Annex-B start codes and report whether any NAL is an IDR slice.
fn annexb_has_idr(es: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 3 < es.len() {
        // Match 00 00 01 (3-byte) — a 4-byte start code is just a 3-byte one
        // preceded by an extra 0x00, so this catches both.
        if es[i] == 0x00 && es[i + 1] == 0x00 && es[i + 2] == 0x01 {
            let nal_header = es[i + 3];
            let nal_unit_type = nal_header & 0x1F;
            if nal_unit_type == 5 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 188-byte TS packet with the given PID/PUSI and a payload that is
    /// copied verbatim after the 4-byte header (payload-only, afc=01). The
    /// payload is truncated/padded to fit the 184-byte payload region.
    fn ts_packet(pid: u16, pusi: bool, cc: u8, payload: &[u8]) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = ((u8::from(pusi)) << 6) | ((pid >> 8) & 0x1F) as u8;
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10 | (cc & 0x0F); // afc=01 (payload only)
        let room = TS_PACKET_SIZE - 4;
        let n = payload.len().min(room);
        pkt[4..4 + n].copy_from_slice(&payload[..n]);
        pkt
    }

    /// Patch the 12-bit `section_length` (over the placeholder at `s[2..4]`) to
    /// span every byte after the length field, including the 4-byte CRC. The
    /// value is masked to 12 bits, so the byte writes are exact (no truncation).
    fn patch_section_length(s: &mut [u8]) {
        // Bytes after the length field = total - (pointer + table_id + len(2)),
        // then +4 for the CRC appended afterwards.
        let section_len = ((s.len() - 4) + 4) & 0x0FFF;
        // Masked to 12 bits above, so both bytes fit a u8 exactly.
        s[2] = 0xB0 | u8::try_from(section_len >> 8).unwrap();
        s[3] = u8::try_from(section_len & 0xFF).unwrap();
    }

    /// Minimal PAT payload (with `pointer_field`) advertising program 1 → PMT PID.
    fn pat_payload(pmt_pid: u16) -> Vec<u8> {
        let mut s = vec![
            0x00, // pointer_field
            0x00, // table_id = PAT
            0xB0, // syntax indicator + reserved + length hi (filled below)
            0x00, // length lo placeholder
        ];
        s.extend_from_slice(&1u16.to_be_bytes()); // transport_stream_id
        s.push(0xC1); // reserved/version/current_next
        s.push(0x00); // section_number
        s.push(0x00); // last_section_number
        s.extend_from_slice(&1u16.to_be_bytes()); // program_number = 1
        s.extend_from_slice(&(0xE000 | (pmt_pid & 0x1FFF)).to_be_bytes());
        patch_section_length(&mut s);
        s.extend_from_slice(&[0, 0, 0, 0]); // dummy CRC (parser ignores it)
        s
    }

    /// Minimal PMT payload (with `pointer_field`) advertising one H.264 stream on
    /// `video_pid` and one AAC stream on `audio_pid`.
    fn pmt_payload(video_pid: u16, audio_pid: u16) -> Vec<u8> {
        let mut s = vec![
            0x00, // pointer_field
            0x02, // table_id = PMT
            0xB0, // syntax indicator + reserved + length hi
            0x00, // length lo placeholder
        ];
        s.extend_from_slice(&1u16.to_be_bytes()); // program_number
        s.push(0xC1);
        s.push(0x00);
        s.push(0x00);
        s.extend_from_slice(&(0xE000 | (video_pid & 0x1FFF)).to_be_bytes()); // PCR PID
        s.extend_from_slice(&0xF000u16.to_be_bytes()); // program_info_length = 0
        // Video ES entry.
        s.push(STREAM_TYPE_H264);
        s.extend_from_slice(&(0xE000 | (video_pid & 0x1FFF)).to_be_bytes());
        s.extend_from_slice(&0xF000u16.to_be_bytes()); // ES_info_length = 0
        // Audio ES entry (AAC, stream_type 0x0F) — should be ignored.
        s.push(0x0F);
        s.extend_from_slice(&(0xE000 | (audio_pid & 0x1FFF)).to_be_bytes());
        s.extend_from_slice(&0xF000u16.to_be_bytes());
        patch_section_length(&mut s);
        s.extend_from_slice(&[0, 0, 0, 0]); // dummy CRC
        s
    }

    /// Build a video PES payload whose elementary stream contains the given NAL
    /// unit types, each prefixed with a 4-byte Annex-B start code.
    fn video_pes_payload(nal_types: &[u8]) -> Vec<u8> {
        let mut es = Vec::new();
        for &t in nal_types {
            es.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            es.push(t & 0x1F); // forbidden_zero_bit=0, nal_ref_idc=0
            es.extend_from_slice(&[0xAA, 0xBB]); // some RBSP bytes
        }
        // PES header: start code + stream_id(0xE0) + len(0) + flags + hdr_len(0).
        let mut pes = Vec::new();
        pes.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0]);
        pes.extend_from_slice(&0u16.to_be_bytes()); // PES_packet_length = 0 (unbounded video)
        pes.push(0x80); // '10' marker + flags
        pes.push(0x00); // no PTS/DTS flags
        pes.push(0x00); // PES_header_data_length = 0
        pes.extend_from_slice(&es);
        pes
    }

    #[test]
    fn parses_pat_then_pmt_to_find_video_pid() {
        let mut seg = MpegTsSegmenter::new();
        assert_eq!(seg.video_pid(), None);
        seg.push(&ts_packet(PID_PAT, true, 0, &pat_payload(0x1000)));
        assert_eq!(seg.video_pid(), None, "PMT not seen yet");
        seg.push(&ts_packet(0x1000, true, 0, &pmt_payload(0x0100, 0x0101)));
        assert_eq!(seg.video_pid(), Some(0x0100));
    }

    #[test]
    fn reassembles_packets_split_across_pushes() {
        let mut seg = MpegTsSegmenter::new();
        let pkt = ts_packet(PID_PAT, true, 0, &pat_payload(0x1000));
        // Feed the packet in two halves.
        let ev1 = seg.push(&pkt[..100]);
        assert!(ev1.is_empty(), "no whole packet yet");
        assert_eq!(seg.packets_seen(), 0);
        let ev2 = seg.push(&pkt[100..]);
        assert_eq!(ev2.len(), 1);
        assert_eq!(seg.packets_seen(), 1);
    }

    #[test]
    fn concatenated_packets_are_split_at_188_boundaries() {
        let mut seg = MpegTsSegmenter::new();
        let mut stream = Vec::new();
        stream.extend_from_slice(&ts_packet(PID_PAT, true, 0, &pat_payload(0x1000)));
        stream.extend_from_slice(&ts_packet(0x1000, true, 0, &pmt_payload(0x0100, 0x0101)));
        stream.extend_from_slice(&ts_packet(0x0101, true, 0, b"audio")); // non-video
        let events = seg.push(&stream);
        assert_eq!(events.len(), 3, "three whole packets");
        assert_eq!(seg.packets_seen(), 3);
        assert_eq!(seg.video_pid(), Some(0x0100));
    }

    #[test]
    fn first_keyframe_opens_segment_without_cut() {
        let mut seg = MpegTsSegmenter::new();
        seg.push(&ts_packet(PID_PAT, true, 0, &pat_payload(0x1000)));
        seg.push(&ts_packet(0x1000, true, 0, &pmt_payload(0x0100, 0x0101)));
        // First video PES carrying an IDR (nal type 5).
        let events = seg.push(&ts_packet(0x0100, true, 0, &video_pes_payload(&[7, 8, 5])));
        assert_eq!(events, vec![SegmentEvent::Buffered]);
        assert!(seg.has_started(), "first keyframe should open a segment");
    }

    #[test]
    fn second_keyframe_triggers_cut() {
        let mut seg = MpegTsSegmenter::new();
        seg.push(&ts_packet(PID_PAT, true, 0, &pat_payload(0x1000)));
        seg.push(&ts_packet(0x1000, true, 0, &pmt_payload(0x0100, 0x0101)));
        // First keyframe (opens segment 0). Segment so far: PAT, PMT, kf0 = 3.
        seg.push(&ts_packet(0x0100, true, 0, &video_pes_payload(&[5])));
        // A non-keyframe inter frame (nal type 1) — buffered, no cut. Now 4.
        let inter = seg.push(&ts_packet(0x0100, true, 1, &video_pes_payload(&[1])));
        assert_eq!(inter, vec![SegmentEvent::Buffered]);
        // Second keyframe — should close the segment and request a cut.
        let kf = seg.push(&ts_packet(0x0100, true, 2, &video_pes_payload(&[5])));
        assert_eq!(kf, vec![SegmentEvent::CutBeforeKeyframe]);

        // The closed segment is exactly the 4 packets that preceded the cut;
        // the keyframe that triggered the cut must NOT be in it (it heads the
        // next segment) — the property that keeps every HLS segment starting on
        // a keyframe.
        let closed = seg.take_segment();
        assert_eq!(closed.len(), 4 * TS_PACKET_SIZE);
        // After draining the completed segment, the open one holds just kf2.
        assert_eq!(seg.current_len(), TS_PACKET_SIZE);
        // A subsequent take (no completed slot) flushes that open keyframe.
        let open = seg.take_segment();
        assert_eq!(open.len(), TS_PACKET_SIZE);
    }

    #[test]
    fn idr_split_across_two_video_packets_still_cuts() {
        // The IDR NAL's start code lands in the PUSI packet, so detection keys
        // off the PES header in that first packet — verify the classifier reads
        // the IDR even when the access unit is large.
        let mut seg = MpegTsSegmenter::new();
        seg.push(&ts_packet(PID_PAT, true, 0, &pat_payload(0x1000)));
        seg.push(&ts_packet(0x1000, true, 0, &pmt_payload(0x0100, 0x0101)));
        seg.push(&ts_packet(0x0100, true, 0, &video_pes_payload(&[5]))); // segment 0 opens
        // Continuation packet of an inter frame (no PUSI) then a fresh keyframe.
        seg.push(&ts_packet(0x0100, false, 1, &[0x11, 0x22, 0x33]));
        let kf = seg.push(&ts_packet(0x0100, true, 2, &video_pes_payload(&[6, 5])));
        assert_eq!(kf, vec![SegmentEvent::CutBeforeKeyframe]);
    }

    #[test]
    fn take_segment_drains_and_resets() {
        let mut seg = MpegTsSegmenter::new();
        seg.push(&ts_packet(PID_PAT, true, 0, &pat_payload(0x1000)));
        seg.push(&ts_packet(0x1000, true, 0, &pmt_payload(0x0100, 0x0101)));
        seg.push(&ts_packet(0x0100, true, 0, &video_pes_payload(&[5])));
        assert!(seg.has_segment_data());
        let bytes = seg.take_segment();
        // Three packets buffered → multiple of 188.
        assert_eq!(bytes.len() % TS_PACKET_SIZE, 0);
        assert_eq!(bytes.len(), 3 * TS_PACKET_SIZE);
        assert_eq!(bytes[0], TS_SYNC_BYTE);
        assert!(!seg.has_segment_data(), "buffer reset after take");
    }

    #[test]
    fn resyncs_after_misaligned_garbage_byte() {
        let mut seg = MpegTsSegmenter::new();
        let pat = ts_packet(PID_PAT, true, 0, &pat_payload(0x1000));
        let mut stream = vec![0x00]; // one stray byte before the real packet
        stream.extend_from_slice(&pat);
        // 1 garbage byte + 188 packet = 189 bytes. The segmenter should skip the
        // stray byte and still parse exactly one packet.
        let events = seg.push(&stream);
        assert_eq!(seg.packets_seen(), 1, "resynced to the sync byte");
        assert!(!events.is_empty());
        assert_eq!(seg.video_pid(), None); // only PAT seen; PMT PID recorded internally
    }

    #[test]
    fn null_packets_are_not_buffered() {
        let mut seg = MpegTsSegmenter::new();
        seg.push(&ts_packet(PID_PAT, true, 0, &pat_payload(0x1000)));
        seg.push(&ts_packet(0x1000, true, 0, &pmt_payload(0x0100, 0x0101)));
        let before = seg.current_len();
        seg.push(&ts_packet(PID_NULL, false, 0, &[0xDE, 0xAD]));
        assert_eq!(seg.current_len(), before, "null packet should be dropped");
        assert_eq!(seg.packets_seen(), 3, "but still counted as seen");
    }

    #[test]
    fn annexb_idr_detection_handles_3_and_4_byte_start_codes() {
        // 3-byte start code form.
        let es3 = [0x00, 0x00, 0x01, 0x65, 0xAA];
        assert!(annexb_has_idr(&es3));
        // 4-byte form.
        let es4 = [0x00, 0x00, 0x00, 0x01, 0x65, 0xAA];
        assert!(annexb_has_idr(&es4));
        // Non-IDR (nal type 1) must not match.
        let non = [0x00, 0x00, 0x00, 0x01, 0x41, 0xAA];
        assert!(!annexb_has_idr(&non));
    }
}
