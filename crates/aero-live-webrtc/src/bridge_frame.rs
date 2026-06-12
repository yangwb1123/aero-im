//! Node-to-node bridge wire format for cross-node call RTP (ROADMAP 方向五).
//!
//! When a call spans nodes, the node that **owns** a publisher relays that
//! participant's RTP to the node pulling it. Each datagram is a small,
//! self-describing frame so the pulling node can attribute the packet without a
//! separate SSRC-mapping channel:
//!
//! ```text
//! +--------+--------+------------------+--------+----------+-------------+
//! | ver(1) | kf(1)  | participant(16)  |mlen(1) | mid(mlen)| RTP payload |
//! +--------+--------+------------------+--------+----------+-------------+
//! ```
//!
//! Backend node-to-node links are trusted (private network), so the media itself
//! is **plain RTP** — DTLS-SRTP is only required on the client-facing leg. This
//! codec is transport-free and fully unit-tested; the UDP socket that carries the
//! frames lives in the server (`UdpRtpUpstream`), where the receive path is
//! exercised over localhost.

use aero_common::ParticipantId;
use bytes::Bytes;
use ulid::Ulid;

use crate::call_bridge::BridgeRtp;

/// Frame format version (byte 0). Bump on any layout change.
const VERSION: u8 = 1;
/// Fixed header size: version(1) + keyframe(1) + participant(16) + mid_len(1).
const HEADER_MIN: usize = 19;

/// Encode a [`BridgeRtp`] into a node-to-node datagram. The owning node calls
/// this to relay a packet to a pulling node.
#[must_use]
pub fn encode_bridge_frame(pkt: &BridgeRtp) -> Vec<u8> {
    let mid_full = pkt.mid.as_bytes();
    let mid_len = u8::try_from(mid_full.len()).unwrap_or(u8::MAX);
    let mid = &mid_full[..mid_len as usize];
    let mut out = Vec::with_capacity(HEADER_MIN + mid.len() + pkt.payload.len());
    out.push(VERSION);
    out.push(u8::from(pkt.keyframe));
    out.extend_from_slice(&pkt.participant.as_ulid().0.to_be_bytes());
    out.push(mid_len);
    out.extend_from_slice(mid);
    out.extend_from_slice(&pkt.payload);
    out
}

/// Decode a node-to-node datagram back into a [`BridgeRtp`]. Returns `None` for a
/// truncated frame or unknown version (never panics), so a malformed datagram is
/// simply dropped by the receiver.
#[must_use]
pub fn decode_bridge_frame(buf: &[u8]) -> Option<BridgeRtp> {
    if buf.len() < HEADER_MIN || buf[0] != VERSION {
        return None;
    }
    let keyframe = buf[1] != 0;
    let mut pid = [0u8; 16];
    pid.copy_from_slice(&buf[2..18]);
    let participant = ParticipantId::from_ulid(Ulid(u128::from_be_bytes(pid)));
    let mid_len = buf[18] as usize;
    let mid_end = HEADER_MIN.checked_add(mid_len)?;
    let mid = std::str::from_utf8(buf.get(HEADER_MIN..mid_end)?).ok()?.to_owned();
    let payload = Bytes::copy_from_slice(buf.get(mid_end..)?);
    Some(BridgeRtp::new(participant, mid, payload, keyframe))
}

#[cfg(test)]
mod tests {
    use super::{decode_bridge_frame, encode_bridge_frame, HEADER_MIN};
    use crate::call_bridge::BridgeRtp;
    use aero_common::ParticipantId;
    use bytes::Bytes;

    #[test]
    fn roundtrips_a_keyframe_packet() {
        let pkt = BridgeRtp::new(
            ParticipantId::new(),
            "video0",
            Bytes::from_static(&[0x80, 0x60, 0x01, 0x02, 0xAA, 0xBB]),
            true,
        );
        assert_eq!(decode_bridge_frame(&encode_bridge_frame(&pkt)), Some(pkt));
    }

    #[test]
    fn roundtrips_empty_payload_and_long_mid() {
        let pkt = BridgeRtp::new(ParticipantId::new(), "a-fairly-long-mid-name", Bytes::new(), false);
        assert_eq!(decode_bridge_frame(&encode_bridge_frame(&pkt)), Some(pkt));
    }

    #[test]
    fn malformed_frames_decode_to_none() {
        assert!(decode_bridge_frame(&[]).is_none());
        assert!(decode_bridge_frame(&[0u8; HEADER_MIN]).is_none(), "wrong version byte");
        assert!(decode_bridge_frame(&[1u8; 5]).is_none(), "truncated header");
        // mid_len claims more bytes than are present.
        let mut f = encode_bridge_frame(&BridgeRtp::new(ParticipantId::new(), "x", Bytes::new(), false));
        f[18] = 200;
        assert!(decode_bridge_frame(&f).is_none(), "mid_len overruns the buffer");
    }
}
