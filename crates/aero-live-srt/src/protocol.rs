//! Minimal, hand-rolled SRT (Secure Reliable Transport) wire protocol.
//!
//! This implements *just enough* of SRT (HSv5) to accept an **unencrypted**
//! caller→listener push of an MPEG-TS stream and hand the payload to the tested
//! [`crate::segmenter::MpegTsSegmenter`]. It is deliberately dependency-free
//! beyond `bytes`/`std` (no `srt-tokio`/`srt-protocol`), so the wire structs are
//! parsed and serialized by hand here.
//!
//! ## What is modelled
//!
//! - The 16-byte SRT [`SrtHeader`] common to every packet (control vs. data
//!   flag, control type/subtype, timestamp, destination socket id).
//! - The 48-byte [`Handshake`] control-information field (CIF): version,
//!   encryption field, extension field, initial sequence number, MTU, flow
//!   window, handshake type, SRT socket id, SYN cookie and peer IP.
//! - The handshake **extension** framing and, specifically, the **StreamID
//!   (SID)** extension that carries `streamid=` from
//!   `srt://host:port?streamid=KEY` — including the per-32-bit-word byte
//!   reversal mandated by the SRT spec.
//! - A small [`HandshakeMachine`] for the listener side of the
//!   INDUCTION → CONCLUSION exchange, including SYN-cookie mint/validate.
//!
//! ## What is intentionally NOT modelled (pending)
//!
//! - Encryption (AES / `KMREQ`/`KMRSP`): out of scope — unencrypted only.
//! - Reliability: ACK/ACKACK/NAK retransmission, RTT estimation and congestion
//!   control. The v1 data plane assumes best-effort, in-order arrival.
//! - Packet reordering / loss recovery and the periodic keep-alive timers.
//!
//! References (consulted for the exact byte layouts, not guessed):
//! - SRT protocol Internet-Draft (`draft-sharabayko-srt`).
//! - The Haivision SRT reference implementation (`srtcore/handshake.h`,
//!   `srtcore/core.cpp` — `fillHsExtConfigString` / `HtoILA`).

// This module is dense with SRT protocol acronyms (HSv5, StreamID, HSRSP,
// TSBPD, …) that read naturally in prose; backticking every one hurts
// readability more than it helps, so we opt out of the stylistic `doc_markdown`
// pedantic lint here (the workspace already disables several pedantic lints).
#![allow(clippy::doc_markdown)]

use bytes::{Buf, BufMut, Bytes, BytesMut};

/// Size of the common SRT packet header in bytes (4 × 32-bit words).
pub const SRT_HEADER_LEN: usize = 16;

/// Size of the handshake control-information field (CIF) in bytes, excluding
/// any trailing extensions. Matches `CHandShake::m_iContentSize` (48).
pub const HANDSHAKE_CIF_LEN: usize = 48;

/// SRT handshake version advertised for HSv5.
pub const SRT_VERSION_HSV5: u32 = 5;

/// Legacy UDT4 handshake version a caller sends in its *initial* INDUCTION.
pub const SRT_VERSION_UDT4: u32 = 4;

/// `SRT_MAGIC_CODE` placed in the **extension field** of the listener's
/// INDUCTION response to advertise HSv5/SRT (distinguishes SRT from plain UDT).
pub const SRT_MAGIC_CODE: u16 = 0x4A17;

/// Encryption field value meaning "no encryption" (`HS_ENC_CLEAR`).
pub const HS_ENC_CLEAR: u16 = 0;

/// Default MTU SRT negotiates (`m_iMSS`), in bytes.
pub const SRT_DEFAULT_MTU: u32 = 1500;

/// Default flow-window size SRT advertises, in packets.
pub const SRT_DEFAULT_FLOW_WINDOW: u32 = 8192;

/// Extension-field flag: an `HSREQ`/`HSRSP` block is present (`HS_EXT_HSREQ`).
pub const HS_EXT_HSREQ: u16 = 0x0001;
/// Extension-field flag: a `KMREQ`/`KMRSP` block is present (`HS_EXT_KMREQ`).
pub const HS_EXT_KMREQ: u16 = 0x0002;
/// Extension-field flag: config blocks (e.g. the StreamID) are present
/// (`HS_EXT_CONFIG`).
pub const HS_EXT_CONFIG: u16 = 0x0004;

/// SRT control packet types (the 15-bit control-type field). Only the values
/// this minimal implementation needs to recognise are enumerated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ControlType {
    /// Handshake (induction/conclusion). `0x0000`.
    Handshake,
    /// Keep-alive. `0x0001`.
    KeepAlive,
    /// Acknowledgement. `0x0002`.
    Ack,
    /// Negative acknowledgement (loss report). `0x0003`.
    Nak,
    /// Shutdown. `0x0005`.
    Shutdown,
    /// Acknowledgement of an ACK. `0x0006`.
    AckAck,
    /// Any control type we don't special-case, preserved verbatim.
    Other(u16),
}

impl ControlType {
    /// The 15-bit on-wire control-type code.
    #[must_use]
    pub fn code(self) -> u16 {
        match self {
            ControlType::Handshake => 0x0000,
            ControlType::KeepAlive => 0x0001,
            ControlType::Ack => 0x0002,
            ControlType::Nak => 0x0003,
            ControlType::Shutdown => 0x0005,
            ControlType::AckAck => 0x0006,
            ControlType::Other(v) => v & 0x7FFF,
        }
    }

    /// Decode a 15-bit control-type code.
    #[must_use]
    pub fn from_code(code: u16) -> Self {
        match code & 0x7FFF {
            0x0000 => ControlType::Handshake,
            0x0001 => ControlType::KeepAlive,
            0x0002 => ControlType::Ack,
            0x0003 => ControlType::Nak,
            0x0005 => ControlType::Shutdown,
            0x0006 => ControlType::AckAck,
            other => ControlType::Other(other),
        }
    }
}

/// The kind of packet a parsed [`SrtHeader`] introduces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
    /// A control packet, carrying a control type, subtype and type-specific
    /// info word.
    Control {
        control_type: ControlType,
        subtype: u16,
        /// Word 1 ("type-specific information"), preserved verbatim.
        type_specific: u32,
    },
    /// A data packet, carrying the 31-bit packet sequence number. The second
    /// header word (PP/O/KK/R/message-number) is preserved verbatim so callers
    /// that don't care about reliability can ignore it without losing it.
    Data {
        /// 31-bit packet sequence number.
        seq_no: u32,
        /// Word 1 (`PP|O|KK|R|message-number`), preserved verbatim.
        msg_word: u32,
    },
}

/// The common 16-byte SRT header shared by every packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SrtHeader {
    /// Whether this is a control packet (vs. a data packet).
    pub kind: PacketKind,
    /// Packet timestamp in microseconds since connection start.
    pub timestamp: u32,
    /// Destination SRT socket id.
    pub dest_socket_id: u32,
}

impl SrtHeader {
    /// True for a control packet.
    #[must_use]
    pub fn is_control(&self) -> bool {
        matches!(self.kind, PacketKind::Control { .. })
    }

    /// Parse a 16-byte SRT header from the front of `buf`.
    ///
    /// Returns `None` if fewer than [`SRT_HEADER_LEN`] bytes are available.
    #[must_use]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < SRT_HEADER_LEN {
            return None;
        }
        let w0 = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let w1 = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let timestamp = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let dest_socket_id = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);

        let kind = if w0 & 0x8000_0000 != 0 {
            // Control: bit 31 set. Bits 30..16 = control type, 15..0 = subtype.
            let control_type = ControlType::from_code(((w0 >> 16) & 0x7FFF) as u16);
            let subtype = (w0 & 0xFFFF) as u16;
            PacketKind::Control {
                control_type,
                subtype,
                type_specific: w1,
            }
        } else {
            // Data: bit 31 clear, bits 30..0 = packet sequence number.
            PacketKind::Data {
                seq_no: w0 & 0x7FFF_FFFF,
                msg_word: w1,
            }
        };

        Some(Self {
            kind,
            timestamp,
            dest_socket_id,
        })
    }

    /// Serialize this header (16 bytes) into `out`.
    pub fn write_to(&self, out: &mut BytesMut) {
        let (w0, w1) = match self.kind {
            PacketKind::Control {
                control_type,
                subtype,
                type_specific,
            } => {
                let w0 =
                    0x8000_0000 | (u32::from(control_type.code() & 0x7FFF) << 16) | u32::from(subtype);
                (w0, type_specific)
            }
            PacketKind::Data { seq_no, msg_word } => (seq_no & 0x7FFF_FFFF, msg_word),
        };
        out.put_u32(w0);
        out.put_u32(w1);
        out.put_u32(self.timestamp);
        out.put_u32(self.dest_socket_id);
    }

    /// Serialize this header to a fresh [`Bytes`].
    #[must_use]
    pub fn to_bytes(&self) -> Bytes {
        let mut b = BytesMut::with_capacity(SRT_HEADER_LEN);
        self.write_to(&mut b);
        b.freeze()
    }
}

/// SRT handshake "request type" (the 32-bit Handshake Type field).
///
/// The negative UDT values are represented by their two's-complement `u32`
/// encoding, exactly as they appear on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeType {
    /// `URQ_WAVEAHAND` (0) — rendezvous initial wave.
    Waveahand,
    /// `URQ_INDUCTION` (1) — phase 1 of the caller-listener handshake.
    Induction,
    /// `URQ_CONCLUSION` (-1 = `0xFFFF_FFFF`) — phase 2 (final) of the handshake.
    Conclusion,
    /// `URQ_AGREEMENT` (-2 = `0xFFFF_FFFE`) — rendezvous agreement.
    Agreement,
    /// `URQ_DONE` (-3 = `0xFFFF_FFFD`).
    Done,
    /// A rejection / error code (`URQ_FAILURE_TYPES` and above), or any value
    /// not otherwise recognised, preserved verbatim.
    Other(u32),
}

impl HandshakeType {
    /// The on-wire 32-bit code.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            HandshakeType::Waveahand => 0,
            HandshakeType::Induction => 1,
            HandshakeType::Conclusion => 0xFFFF_FFFF,
            HandshakeType::Agreement => 0xFFFF_FFFE,
            HandshakeType::Done => 0xFFFF_FFFD,
            HandshakeType::Other(v) => v,
        }
    }

    /// Decode a 32-bit handshake-type code.
    #[must_use]
    pub fn from_code(code: u32) -> Self {
        match code {
            0 => HandshakeType::Waveahand,
            1 => HandshakeType::Induction,
            0xFFFF_FFFF => HandshakeType::Conclusion,
            0xFFFF_FFFE => HandshakeType::Agreement,
            0xFFFF_FFFD => HandshakeType::Done,
            other => HandshakeType::Other(other),
        }
    }
}

/// A parsed SRT handshake control-information field (CIF), plus any extensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub version: u32,
    pub encryption_field: u16,
    pub extension_field: u16,
    pub initial_seq_no: u32,
    pub mtu: u32,
    pub flow_window: u32,
    pub handshake_type: HandshakeType,
    /// The peer's SRT socket id.
    pub srt_socket_id: u32,
    pub syn_cookie: u32,
    /// Peer IP address as the raw 16-byte field (IPv4 sits in the first 4
    /// bytes, the rest zero).
    pub peer_ip: [u8; 16],
    /// Decoded handshake extensions (HSREQ / SID / …) in wire order.
    pub extensions: Vec<HsExtension>,
}

impl Default for Handshake {
    fn default() -> Self {
        Self {
            version: SRT_VERSION_HSV5,
            encryption_field: HS_ENC_CLEAR,
            extension_field: 0,
            initial_seq_no: 0,
            mtu: SRT_DEFAULT_MTU,
            flow_window: SRT_DEFAULT_FLOW_WINDOW,
            handshake_type: HandshakeType::Induction,
            srt_socket_id: 0,
            syn_cookie: 0,
            peer_ip: [0u8; 16],
            extensions: Vec::new(),
        }
    }
}

/// A single decoded handshake extension block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HsExtension {
    /// `SRT_CMD_HSREQ` (1) / `SRT_CMD_HSRSP` (2): SRT capability exchange.
    HsReq {
        /// Whether this was the response form (`HSRSP`, type 2) rather than the
        /// request (`HSREQ`, type 1).
        is_response: bool,
        srt_version: u32,
        srt_flags: u32,
        recv_tsbpd_delay: u16,
        send_tsbpd_delay: u16,
    },
    /// `SRT_CMD_SID` (5): the StreamID string (the stream key for our ingest).
    StreamId(String),
    /// Any other extension, preserved as `(type, raw-contents)` so it
    /// round-trips without us having to model it.
    Other { ext_type: u16, contents: Bytes },
}

/// Extension command type: `SRT_CMD_HSREQ`.
const SRT_CMD_HSREQ: u16 = 1;
/// Extension command type: `SRT_CMD_HSRSP`.
const SRT_CMD_HSRSP: u16 = 2;
/// Extension command type: `SRT_CMD_SID` (StreamID).
const SRT_CMD_SID: u16 = 5;

impl Handshake {
    /// Parse a handshake CIF (and any trailing extensions) from `buf`.
    ///
    /// `buf` must start at the first byte *after* the 16-byte [`SrtHeader`].
    /// Returns `None` if the fixed 48-byte CIF is truncated; malformed trailing
    /// extension blocks stop extension parsing but still yield the base CIF.
    #[must_use]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < HANDSHAKE_CIF_LEN {
            return None;
        }
        let mut c = buf;
        let version = c.get_u32();
        let encryption_field = c.get_u16();
        let extension_field = c.get_u16();
        let initial_seq_no = c.get_u32();
        let mtu = c.get_u32();
        let flow_window = c.get_u32();
        let handshake_type = HandshakeType::from_code(c.get_u32());
        let srt_socket_id = c.get_u32();
        let syn_cookie = c.get_u32();
        let mut peer_ip = [0u8; 16];
        peer_ip.copy_from_slice(&c[..16]);
        c.advance(16);

        let extensions = parse_extensions(c);

        Some(Self {
            version,
            encryption_field,
            extension_field,
            initial_seq_no,
            mtu,
            flow_window,
            handshake_type,
            srt_socket_id,
            syn_cookie,
            peer_ip,
            extensions,
        })
    }

    /// Serialize the full handshake (CIF + extensions) into `out`. The
    /// extension-field flag bits are recomputed from `self.extensions` so they
    /// always agree with what is actually appended.
    pub fn write_to(&self, out: &mut BytesMut) {
        out.put_u32(self.version);
        out.put_u16(self.encryption_field);
        // Recompute the extension flags from the blocks we are about to write,
        // so a hand-built `Handshake` can leave `extension_field` at 0 and still
        // serialize correctly.
        out.put_u16(self.computed_extension_field());
        out.put_u32(self.initial_seq_no);
        out.put_u32(self.mtu);
        out.put_u32(self.flow_window);
        out.put_u32(self.handshake_type.code());
        out.put_u32(self.srt_socket_id);
        out.put_u32(self.syn_cookie);
        out.put_slice(&self.peer_ip);
        for ext in &self.extensions {
            write_extension(out, ext);
        }
    }

    /// Serialize the full handshake to a fresh [`Bytes`].
    #[must_use]
    pub fn to_bytes(&self) -> Bytes {
        let mut b = BytesMut::with_capacity(HANDSHAKE_CIF_LEN);
        self.write_to(&mut b);
        b.freeze()
    }

    /// The extension-field flags implied by `self.extensions`, OR-ed with any
    /// flags already set in `self.extension_field` (so the INDUCTION magic, which
    /// is *not* an extension block, is preserved).
    #[must_use]
    pub fn computed_extension_field(&self) -> u16 {
        let mut flags = self.extension_field;
        for ext in &self.extensions {
            match ext {
                HsExtension::HsReq { .. } => flags |= HS_EXT_HSREQ,
                HsExtension::StreamId(_) | HsExtension::Other { .. } => flags |= HS_EXT_CONFIG,
            }
        }
        flags
    }

    /// The StreamID carried by a `SRT_CMD_SID` extension, if present.
    #[must_use]
    pub fn stream_id(&self) -> Option<&str> {
        self.extensions.iter().find_map(|e| match e {
            HsExtension::StreamId(s) => Some(s.as_str()),
            _ => None,
        })
    }
}

/// Parse the sequence of handshake extension blocks that follow the 48-byte CIF.
///
/// Each block is `[type:u16][len_words:u16][contents: len_words*4 bytes]`. A
/// truncated/garbled block ends parsing (we return what we decoded so far)
/// rather than erroring, mirroring the reference decoder's tolerance.
fn parse_extensions(mut buf: &[u8]) -> Vec<HsExtension> {
    let mut out = Vec::new();
    while buf.len() >= 4 {
        let ext_type = u16::from_be_bytes([buf[0], buf[1]]);
        let len_words = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        let content_len = len_words * 4;
        if buf.len() < 4 + content_len {
            break;
        }
        let contents = &buf[4..4 + content_len];
        match ext_type {
            SRT_CMD_HSREQ | SRT_CMD_HSRSP if content_len >= 12 => {
                let srt_version = u32::from_be_bytes([contents[0], contents[1], contents[2], contents[3]]);
                let srt_flags = u32::from_be_bytes([contents[4], contents[5], contents[6], contents[7]]);
                let recv_tsbpd_delay = u16::from_be_bytes([contents[8], contents[9]]);
                let send_tsbpd_delay = u16::from_be_bytes([contents[10], contents[11]]);
                out.push(HsExtension::HsReq {
                    is_response: ext_type == SRT_CMD_HSRSP,
                    srt_version,
                    srt_flags,
                    recv_tsbpd_delay,
                    send_tsbpd_delay,
                });
            }
            SRT_CMD_SID => {
                out.push(HsExtension::StreamId(decode_stream_id(contents)));
            }
            _ => {
                out.push(HsExtension::Other {
                    ext_type,
                    contents: Bytes::copy_from_slice(contents),
                });
            }
        }
        buf = &buf[4 + content_len..];
    }
    out
}

/// Serialize a single extension block (header + contents) into `out`.
fn write_extension(out: &mut BytesMut, ext: &HsExtension) {
    match ext {
        HsExtension::HsReq {
            is_response,
            srt_version,
            srt_flags,
            recv_tsbpd_delay,
            send_tsbpd_delay,
        } => {
            let cmd = if *is_response { SRT_CMD_HSRSP } else { SRT_CMD_HSREQ };
            out.put_u16(cmd);
            out.put_u16(3); // 3 words of contents
            out.put_u32(*srt_version);
            out.put_u32(*srt_flags);
            out.put_u16(*recv_tsbpd_delay);
            out.put_u16(*send_tsbpd_delay);
        }
        HsExtension::StreamId(sid) => {
            let encoded = encode_stream_id(sid);
            let words = encoded.len() / 4;
            out.put_u16(SRT_CMD_SID);
            out.put_u16(u16::try_from(words).unwrap_or(u16::MAX));
            out.put_slice(&encoded);
        }
        HsExtension::Other { ext_type, contents } => {
            // Pad to a 4-byte boundary, as all SRT extension contents are
            // word-aligned.
            let words = contents.len().div_ceil(4);
            out.put_u16(*ext_type);
            out.put_u16(u16::try_from(words).unwrap_or(u16::MAX));
            out.put_slice(contents);
            for _ in 0..(words * 4 - contents.len()) {
                out.put_u8(0);
            }
        }
    }
}

/// Encode a StreamID string into the SRT `SRT_CMD_SID` wire form.
///
/// Per the SRT spec the content "is stored as 32-bit little endian words": the
/// UTF-8 string is NUL-padded up to a multiple of four bytes, then **each
/// 4-byte block is byte-reversed**. (The reference implementation copies the
/// bytes verbatim then applies `HtoILA` = `htole32` per word; because every
/// other field in the packet is big-endian, the net on-wire effect is a
/// per-word reversal — e.g. `"STREAM"` → `"ERTS\0\0MA"`.)
#[must_use]
pub fn encode_stream_id(sid: &str) -> Vec<u8> {
    let bytes = sid.as_bytes();
    let words = bytes.len().div_ceil(4);
    let mut padded = vec![0u8; words * 4];
    padded[..bytes.len()].copy_from_slice(bytes);
    for block in padded.chunks_exact_mut(4) {
        block.reverse();
    }
    padded
}

/// Decode a `SRT_CMD_SID` wire payload back into the StreamID string.
///
/// Reverses each 4-byte block (undoing [`encode_stream_id`]) and trims trailing
/// NUL padding. Bytes are interpreted as UTF-8 lossily so a malformed SID never
/// panics the listener.
#[must_use]
pub fn decode_stream_id(contents: &[u8]) -> String {
    let mut bytes = contents.to_vec();
    for block in bytes.chunks_exact_mut(4) {
        block.reverse();
    }
    // Trim trailing NUL padding only (interior NULs are preserved).
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

// ============================ Handshake machine ============================

/// Errors the listener-side handshake can surface.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HandshakeError {
    /// The datagram was too short or not a handshake control packet.
    #[error("not a handshake packet")]
    NotHandshake,
    /// A CONCLUSION arrived whose SYN cookie did not match what we minted.
    #[error("invalid SYN cookie")]
    BadCookie,
    /// A CONCLUSION carried no StreamID extension, so we can't resolve a stream.
    #[error("conclusion missing streamid extension")]
    MissingStreamId,
    /// An unexpected handshake type for the current state.
    #[error("unexpected handshake type for current state")]
    UnexpectedType,
}

/// State of the listener-side handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HsState {
    /// Waiting for the caller's INDUCTION.
    Init,
    /// Sent the INDUCTION response (with our cookie); awaiting CONCLUSION.
    InductionSent,
    /// Connection established (CONCLUSION validated, SID extracted).
    Done,
}

/// What the listener should do after feeding a packet to the [`HandshakeMachine`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HsAction {
    /// Send these bytes back to the caller (a full SRT packet), then keep going.
    Reply(Bytes),
    /// Handshake complete. The connection is established and `stream_id` is the
    /// `streamid=` the caller pushed (our stream key). `agreement` is the
    /// CONCLUSION response to send back to the caller to confirm.
    Established {
        stream_id: String,
        agreement: Bytes,
    },
    /// Nothing to do (e.g. a duplicate/unknown packet we safely ignore).
    Ignore,
}

/// Minimal listener-side SRT (HSv5) handshake driver for the unencrypted case.
///
/// Drives the INDUCTION → CONCLUSION exchange:
/// 1. Caller → INDUCTION (version 4, cookie 0).
/// 2. Listener → INDUCTION response (version 5, magic `0x4A17`, fresh cookie).
/// 3. Caller → CONCLUSION (version 5, echoes the cookie, carries HSREQ + SID).
/// 4. Listener → CONCLUSION response (HSRSP); connection established.
///
/// SYN cookies are minted from the peer address + a coarse (1-minute) clock so a
/// stale or spoofed CONCLUSION is rejected. The cookie secret/seed and clock are
/// injected so the machine is deterministic under test.
#[derive(Debug, Clone)]
pub struct HandshakeMachine {
    /// Our (listener) SRT socket id, echoed to the caller.
    listener_socket_id: u32,
    /// Seed mixed into the SYN cookie (a per-listener secret).
    cookie_seed: u32,
    state: HsState,
    /// The cookie we minted in the INDUCTION response, to validate against.
    issued_cookie: Option<u32>,
    /// The caller's SRT socket id, learned from its INDUCTION.
    peer_socket_id: u32,
}

impl HandshakeMachine {
    /// Create a listener handshake machine.
    ///
    /// `listener_socket_id` is the id we present to callers; `cookie_seed` is a
    /// per-process secret folded into SYN cookies.
    #[must_use]
    pub fn new(listener_socket_id: u32, cookie_seed: u32) -> Self {
        Self {
            listener_socket_id,
            cookie_seed,
            state: HsState::Init,
            issued_cookie: None,
            peer_socket_id: 0,
        }
    }

    /// Current state of the handshake.
    #[must_use]
    pub fn state(&self) -> HsState {
        self.state
    }

    /// Mint a SYN cookie for `peer` at `now_unix` (seconds). The clock is folded
    /// in with 1-minute granularity, matching SRT's anti-flood design: a cookie
    /// is reproducible within the same minute for the same peer, and changes
    /// across minutes.
    #[must_use]
    pub fn make_cookie(&self, peer: std::net::SocketAddr, now_unix: i64) -> u32 {
        // A small, dependency-free FNV-1a hash over the peer address, port,
        // coarse time and the per-listener seed. Not cryptographic, but
        // sufficient to bind a CONCLUSION to the INDUCTION that issued it and to
        // resist blind SYN floods (the attacker must echo the exact cookie).
        const FNV_OFFSET: u32 = 0x811c_9dc5;
        const FNV_PRIME: u32 = 0x0100_0193;
        let mut h = FNV_OFFSET;
        let mut mix = |b: u8| {
            h ^= u32::from(b);
            h = h.wrapping_mul(FNV_PRIME);
        };
        match peer.ip() {
            std::net::IpAddr::V4(v4) => {
                for b in v4.octets() {
                    mix(b);
                }
            }
            std::net::IpAddr::V6(v6) => {
                for b in v6.octets() {
                    mix(b);
                }
            }
        }
        for b in peer.port().to_be_bytes() {
            mix(b);
        }
        // Coarse minute counter (anti-flood: 1-minute accuracy).
        let minute = now_unix / 60;
        for b in minute.to_be_bytes() {
            mix(b);
        }
        for b in self.cookie_seed.to_be_bytes() {
            mix(b);
        }
        h
    }

    /// Validate that `cookie` matches one we'd mint for `peer` in either the
    /// current or the immediately-preceding minute (tolerating a minute rollover
    /// between our INDUCTION response and the caller's CONCLUSION).
    #[must_use]
    pub fn validate_cookie(&self, cookie: u32, peer: std::net::SocketAddr, now_unix: i64) -> bool {
        // Prefer the exact cookie we issued this session if we have one.
        if self.issued_cookie == Some(cookie) {
            return true;
        }
        cookie == self.make_cookie(peer, now_unix)
            || cookie == self.make_cookie(peer, now_unix.saturating_sub(60))
    }

    /// Feed a received datagram (a full SRT packet) and advance the handshake.
    ///
    /// `peer` is the UDP source address; `now_unix` the current wall clock in
    /// seconds (injected for determinism). Non-handshake control packets and
    /// data packets are reported as [`HsAction::Ignore`] so the caller can route
    /// data packets to the data plane once [`HsState::Done`] is reached.
    pub fn handle_packet(
        &mut self,
        datagram: &[u8],
        peer: std::net::SocketAddr,
        now_unix: i64,
    ) -> Result<HsAction, HandshakeError> {
        let header = SrtHeader::parse(datagram).ok_or(HandshakeError::NotHandshake)?;
        let PacketKind::Control { control_type, .. } = header.kind else {
            return Ok(HsAction::Ignore);
        };
        if control_type != ControlType::Handshake {
            return Ok(HsAction::Ignore);
        }
        let hs = Handshake::parse(&datagram[SRT_HEADER_LEN..]).ok_or(HandshakeError::NotHandshake)?;

        match hs.handshake_type {
            HandshakeType::Induction => self.on_induction(&hs, peer, now_unix),
            HandshakeType::Conclusion => self.on_conclusion(&hs, peer, now_unix),
            // Rendezvous / agreement / anything else isn't part of the
            // caller-listener push we support.
            _ => Ok(HsAction::Ignore),
        }
    }

    /// Handle the caller's INDUCTION: mint a cookie and build the version-5
    /// INDUCTION response (magic `0x4A17` in the extension field).
    fn on_induction(
        &mut self,
        caller: &Handshake,
        peer: std::net::SocketAddr,
        now_unix: i64,
    ) -> Result<HsAction, HandshakeError> {
        self.peer_socket_id = caller.srt_socket_id;
        let cookie = self.make_cookie(peer, now_unix);
        self.issued_cookie = Some(cookie);
        self.state = HsState::InductionSent;

        let hs = Handshake {
            version: SRT_VERSION_HSV5,
            encryption_field: HS_ENC_CLEAR,
            // The INDUCTION-response magic lives in the extension field and is
            // NOT an extension block, so set it directly.
            extension_field: SRT_MAGIC_CODE,
            initial_seq_no: caller.initial_seq_no,
            mtu: caller.mtu.min(SRT_DEFAULT_MTU),
            flow_window: caller.flow_window.min(SRT_DEFAULT_FLOW_WINDOW),
            handshake_type: HandshakeType::Induction,
            srt_socket_id: self.listener_socket_id,
            syn_cookie: cookie,
            peer_ip: caller.peer_ip,
            extensions: Vec::new(),
        };
        Ok(HsAction::Reply(self.wrap_control(&hs)))
    }

    /// Handle the caller's CONCLUSION: validate the echoed cookie, extract the
    /// StreamID and build the CONCLUSION response (HSRSP), completing the
    /// handshake.
    fn on_conclusion(
        &mut self,
        caller: &Handshake,
        peer: std::net::SocketAddr,
        now_unix: i64,
    ) -> Result<HsAction, HandshakeError> {
        if !self.validate_cookie(caller.syn_cookie, peer, now_unix) {
            return Err(HandshakeError::BadCookie);
        }
        let stream_id = caller
            .stream_id()
            .ok_or(HandshakeError::MissingStreamId)?
            .to_string();
        self.peer_socket_id = caller.srt_socket_id;

        // Build the CONCLUSION response: mirror the caller, reply with an HSRSP
        // capability block (unencrypted, so no KMRSP). We don't echo the SID.
        let hsrsp = HsExtension::HsReq {
            is_response: true,
            srt_version: srt_version_word(),
            srt_flags: caller_hs_flags(caller),
            recv_tsbpd_delay: caller_recv_delay(caller),
            send_tsbpd_delay: 0,
        };
        let hs = Handshake {
            version: SRT_VERSION_HSV5,
            encryption_field: HS_ENC_CLEAR,
            extension_field: 0, // recomputed from `extensions` on serialize
            initial_seq_no: caller.initial_seq_no,
            mtu: caller.mtu.min(SRT_DEFAULT_MTU),
            flow_window: caller.flow_window.min(SRT_DEFAULT_FLOW_WINDOW),
            handshake_type: HandshakeType::Conclusion,
            srt_socket_id: self.listener_socket_id,
            syn_cookie: caller.syn_cookie,
            peer_ip: caller.peer_ip,
            extensions: vec![hsrsp],
        };
        let agreement = self.wrap_control(&hs);
        self.state = HsState::Done;
        Ok(HsAction::Established {
            stream_id,
            agreement,
        })
    }

    /// Wrap a handshake CIF in a full SRT control packet (header + CIF), aimed
    /// at the caller's socket id.
    fn wrap_control(&self, hs: &Handshake) -> Bytes {
        let header = SrtHeader {
            kind: PacketKind::Control {
                control_type: ControlType::Handshake,
                subtype: 0,
                type_specific: 0,
            },
            timestamp: 0,
            dest_socket_id: self.peer_socket_id,
        };
        let mut out = BytesMut::with_capacity(SRT_HEADER_LEN + HANDSHAKE_CIF_LEN);
        header.write_to(&mut out);
        hs.write_to(&mut out);
        out.freeze()
    }

    /// The caller's SRT socket id (valid once an INDUCTION/CONCLUSION arrived).
    #[must_use]
    pub fn peer_socket_id(&self) -> u32 {
        self.peer_socket_id
    }
}

/// SRT library version word `major*0x10000 + minor*0x100 + patch`. We advertise
/// a plausible 1.5.0 so a peer that inspects it sees a modern SRT.
fn srt_version_word() -> u32 {
    0x0001_0500
}

/// Echo the caller's negotiated HS flags in our HSRSP (a real implementation
/// would intersect capabilities; for the unencrypted MPEG-TS path mirroring is
/// adequate).
fn caller_hs_flags(caller: &Handshake) -> u32 {
    caller
        .extensions
        .iter()
        .find_map(|e| match e {
            HsExtension::HsReq { srt_flags, .. } => Some(*srt_flags),
            _ => None,
        })
        .unwrap_or(0)
}

/// Mirror the caller's receiver TSBPD delay so timestamps line up.
fn caller_recv_delay(caller: &Handshake) -> u16 {
    caller
        .extensions
        .iter()
        .find_map(|e| match e {
            HsExtension::HsReq {
                recv_tsbpd_delay, ..
            } => Some(*recv_tsbpd_delay),
            _ => None,
        })
        .unwrap_or(0)
}


#[cfg(test)]
mod tests;
