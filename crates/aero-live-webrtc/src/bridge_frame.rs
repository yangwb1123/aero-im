//! Node-to-node bridge wire format for cross-node call RTP (ROADMAP 方向五).
//!
//! When a call spans nodes, the node that **owns** a publisher relays that
//! participant's RTP to the node pulling it. Each datagram is a small,
//! self-describing frame so the pulling node can attribute the packet without a
//! separate SSRC-mapping channel:
//!
//! Current pullers emit a v4 envelope; v3 remains the generation-bound rolling
//! compatibility format:
//!
//! ```text
//! +------+----------+-----------------+-------+-----------------+----------+
//! |ver(1)|call(16)  |subscription(16) |flags  |participant(16) | RTP meta |
//! +------+----------+-----------------+-------+-----------------+----------+
//! |                         RTP media payload …                         |
//! +---------------------------------------------------------------------+
//! ```
//!
//! The call and subscription UUID fence delayed datagrams when an ephemeral UDP
//! port is reused. Version 2, which starts directly with `flags`, remains only
//! for a new owner sending to a pre-upgrade puller.
//!
//! Backend node-to-node links are trusted (private network), so the media itself
//! is **plain RTP** — DTLS-SRTP is only required on the client-facing leg. This
//! codec is transport-free and fully unit-tested; the UDP socket that carries the
//! frames lives in the server (`UdpRtpUpstream`), where the receive path is
//! exercised over localhost.

use aero_common::{CallId, ParticipantId};
use bytes::Bytes;
use str0m::media::{Mid, Pt, Rid};
use str0m::rtp::{SeqNo, Ssrc};
use ulid::Ulid;
use uuid::Uuid;

use crate::call_bridge::BridgeRtp;

/// Legacy frame format used only while an old puller is still registered.
const LEGACY_VERSION: u8 = 2;
/// Previous call- and pull-incarnation-bound format accepted during rolling
/// upgrades. It has the v4 layout but no media-aware gate flag.
pub const BOUND_BRIDGE_COMPAT_VERSION: u8 = 3;
/// Current call- and pull-incarnation-bound frame format.
///
/// Version 4 adds an explicit video-keyframe-gating bit. A version bump makes
/// the rolling-upgrade behavior fail closed instead of letting an older puller
/// reinterpret that bit.
pub const BOUND_BRIDGE_VERSION: u8 = 4;
const FLAG_KEYFRAME: u8 = 1 << 0;
const FLAG_MARKER: u8 = 1 << 1;
const FLAG_RID: u8 = 1 << 2;
const FLAG_REQUIRES_KEYFRAME: u8 = 1 << 3;
const LEGACY_KNOWN_FLAGS: u8 = FLAG_KEYFRAME | FLAG_MARKER | FLAG_RID;
const BOUND_KNOWN_FLAGS: u8 = LEGACY_KNOWN_FLAGS | FLAG_REQUIRES_KEYFRAME;
const BODY_HEADER_LEN: usize = 36;
const LEGACY_BODY_OFFSET: usize = 1;
const BOUND_BODY_OFFSET: usize = 33;
/// Fixed legacy header size through `rid_len`.
const HEADER_MIN: usize = LEGACY_BODY_OFFSET + BODY_HEADER_LEN;
/// Fixed bound header size through `rid_len`.
const BOUND_HEADER_MIN: usize = BOUND_BODY_OFFSET + BODY_HEADER_LEN;
const MAX_MID_LEN: usize = 16;
const MAX_RID_LEN: usize = 8;

/// A generation-bound bridge frame together with the wire version it used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundBridgeFrame {
    pub wire_version: u8,
    pub call: CallId,
    pub subscription_id: Uuid,
    pub packet: BridgeRtp,
}

/// Encode a [`BridgeRtp`] into a node-to-node datagram. The owning node calls
/// this only for a pre-upgrade puller that did not advertise a generation.
#[must_use]
pub fn encode_bridge_frame(pkt: &BridgeRtp) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_MIN + pkt.payload.len());
    out.push(LEGACY_VERSION);
    encode_body(&mut out, pkt, false);
    out
}

/// Encode a v4 datagram bound to one call and subscription generation.
#[must_use]
pub fn encode_bound_bridge_frame(call: CallId, subscription_id: Uuid, pkt: &BridgeRtp) -> Vec<u8> {
    encode_bound_bridge_frame_version(call, subscription_id, pkt, BOUND_BRIDGE_VERSION)
        .expect("the current bridge wire version is supported")
}

/// Encode a generation-bound datagram for a negotiated v3/v4 puller.
///
/// Returns `None` for unsupported versions so an untrusted control-plane value
/// can never select an ambiguous wire layout.
#[must_use]
pub fn encode_bound_bridge_frame_version(
    call: CallId,
    subscription_id: Uuid,
    pkt: &BridgeRtp,
    wire_version: u8,
) -> Option<Vec<u8>> {
    if !matches!(
        wire_version,
        BOUND_BRIDGE_COMPAT_VERSION | BOUND_BRIDGE_VERSION
    ) {
        return None;
    }
    let mut out = Vec::with_capacity(BOUND_HEADER_MIN + pkt.payload.len());
    out.push(wire_version);
    out.extend_from_slice(call.to_uuid().as_bytes());
    out.extend_from_slice(subscription_id.as_bytes());
    encode_body(&mut out, pkt, wire_version == BOUND_BRIDGE_VERSION);
    Some(out)
}

fn encode_body(out: &mut Vec<u8>, pkt: &BridgeRtp, include_keyframe_requirement: bool) {
    let mid = pkt.mid.as_bytes();
    let rid = pkt.rid.as_ref().map_or(&[][..], |rid| rid.as_bytes());
    debug_assert!(mid.len() <= MAX_MID_LEN, "str0m Mid is at most 16 bytes");
    debug_assert!(rid.len() <= MAX_RID_LEN, "str0m Rid is at most 8 bytes");
    let mut flags = 0;
    // v2/v3 pullers gate every track because their envelope has no media-kind
    // bit. Mark audio/unknown packets as random-access for those peers so a
    // rolling upgrade cannot strand an Opus track behind a video-only gate.
    if pkt.keyframe || (!include_keyframe_requirement && !pkt.requires_keyframe) {
        flags |= FLAG_KEYFRAME;
    }
    if pkt.marker {
        flags |= FLAG_MARKER;
    }
    if pkt.rid.is_some() {
        flags |= FLAG_RID;
    }
    if include_keyframe_requirement && pkt.requires_keyframe {
        flags |= FLAG_REQUIRES_KEYFRAME;
    }
    out.push(flags);
    out.extend_from_slice(&pkt.participant.as_ulid().0.to_be_bytes());
    out.push(*pkt.pt);
    out.extend_from_slice(&(*pkt.seq_no).to_be_bytes());
    out.extend_from_slice(&pkt.rtp_time.to_be_bytes());
    out.extend_from_slice(&(*pkt.ssrc).to_be_bytes());
    out.push(u8::try_from(mid.len()).expect("Mid length is bounded"));
    out.push(u8::try_from(rid.len()).expect("Rid length is bounded"));
    out.extend_from_slice(mid);
    out.extend_from_slice(rid);
    out.extend_from_slice(&pkt.payload);
}

/// Decode a legacy v2 datagram. Upgraded pullers deliberately do not call this:
/// an unbound datagram cannot be proven to belong to their socket incarnation.
#[must_use]
pub fn decode_bridge_frame(buf: &[u8]) -> Option<BridgeRtp> {
    if buf.len() < HEADER_MIN || buf[0] != LEGACY_VERSION {
        return None;
    }
    decode_body(buf, LEGACY_BODY_OFFSET, KeyframeRequirement::LegacyAll)
}

/// Decode a v3/v4 datagram with its call and subscription binding.
///
/// Version 2 is rejected rather than guessed, making a new puller fail closed
/// while its owner is still an old binary.
#[must_use]
pub fn decode_bound_bridge_frame(buf: &[u8]) -> Option<BoundBridgeFrame> {
    if buf.len() < BOUND_HEADER_MIN
        || !matches!(buf[0], BOUND_BRIDGE_COMPAT_VERSION | BOUND_BRIDGE_VERSION)
    {
        return None;
    }
    let wire_version = buf[0];
    let call_uuid = Uuid::from_bytes(buf.get(1..17)?.try_into().ok()?);
    let subscription_id = Uuid::from_bytes(buf.get(17..33)?.try_into().ok()?);
    Some(BoundBridgeFrame {
        wire_version,
        call: CallId::from_uuid(call_uuid),
        subscription_id,
        packet: decode_body(
            buf,
            BOUND_BODY_OFFSET,
            if wire_version == BOUND_BRIDGE_VERSION {
                KeyframeRequirement::ExplicitFlag
            } else {
                // A v3 owner cannot distinguish audio from video. Availability
                // wins during the compatibility window: accept its media as
                // passthrough instead of recreating the old permanent audio
                // black-hole. Native v4 links retain video random-access gates.
                KeyframeRequirement::CompatibilityPassthrough
            },
        )?,
    })
}

#[derive(Clone, Copy)]
enum KeyframeRequirement {
    LegacyAll,
    CompatibilityPassthrough,
    ExplicitFlag,
}

fn decode_body(
    buf: &[u8],
    offset: usize,
    keyframe_requirement: KeyframeRequirement,
) -> Option<BridgeRtp> {
    let header_end = offset.checked_add(BODY_HEADER_LEN)?;
    if buf.len() < header_end {
        return None;
    }
    let flags = buf[offset];
    let known_flags = if matches!(keyframe_requirement, KeyframeRequirement::ExplicitFlag) {
        BOUND_KNOWN_FLAGS
    } else {
        LEGACY_KNOWN_FLAGS
    };
    if flags & !known_flags != 0 {
        return None;
    }
    let keyframe = flags & FLAG_KEYFRAME != 0;
    let marker = flags & FLAG_MARKER != 0;
    let has_rid = flags & FLAG_RID != 0;
    let requires_keyframe = match keyframe_requirement {
        KeyframeRequirement::LegacyAll => true,
        KeyframeRequirement::CompatibilityPassthrough => false,
        KeyframeRequirement::ExplicitFlag => flags & FLAG_REQUIRES_KEYFRAME != 0,
    };
    let mut pid = [0u8; 16];
    pid.copy_from_slice(buf.get(offset + 1..offset + 17)?);
    let participant = ParticipantId::from_ulid(Ulid(u128::from_be_bytes(pid)));
    let pt = buf[offset + 17];
    if pt > 127 {
        return None;
    }
    let seq_no = u64::from_be_bytes(buf.get(offset + 18..offset + 26)?.try_into().ok()?);
    let rtp_time = u32::from_be_bytes(buf.get(offset + 26..offset + 30)?.try_into().ok()?);
    let ssrc = u32::from_be_bytes(buf.get(offset + 30..offset + 34)?.try_into().ok()?);
    let mid_len = buf[offset + 34] as usize;
    let rid_len = buf[offset + 35] as usize;
    if mid_len == 0 || mid_len > MAX_MID_LEN || rid_len > MAX_RID_LEN || has_rid != (rid_len > 0) {
        return None;
    }
    let mid_end = header_end.checked_add(mid_len)?;
    let rid_end = mid_end.checked_add(rid_len)?;
    let mid = std::str::from_utf8(buf.get(header_end..mid_end)?).ok()?;
    let rid = if has_rid {
        Some(Rid::from(
            std::str::from_utf8(buf.get(mid_end..rid_end)?).ok()?,
        ))
    } else {
        None
    };
    let payload = Bytes::copy_from_slice(buf.get(rid_end..)?);
    Some(BridgeRtp {
        participant,
        mid: Mid::from(mid),
        rid,
        pt: Pt::from(pt),
        seq_no: SeqNo::from(seq_no),
        rtp_time,
        ssrc: Ssrc::from(ssrc),
        marker,
        payload,
        keyframe,
        requires_keyframe,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        decode_bound_bridge_frame, decode_bridge_frame, encode_bound_bridge_frame,
        encode_bound_bridge_frame_version, encode_bridge_frame, BoundBridgeFrame,
        BOUND_BODY_OFFSET, BOUND_BRIDGE_COMPAT_VERSION, BOUND_BRIDGE_VERSION, FLAG_KEYFRAME,
        FLAG_MARKER, FLAG_REQUIRES_KEYFRAME, FLAG_RID, HEADER_MIN,
    };
    use crate::call_bridge::BridgeRtp;
    use aero_common::{CallId, ParticipantId};
    use bytes::Bytes;
    use str0m::media::Rid;
    use ulid::Ulid;
    use uuid::Uuid;

    #[test]
    fn roundtrips_a_keyframe_packet() {
        let mut pkt = BridgeRtp::new(
            ParticipantId::new(),
            "video0",
            Bytes::from_static(&[0x80, 0x60, 0x01, 0x02, 0xAA, 0xBB]),
            true,
        );
        pkt.rid = Some(Rid::from("high"));
        pkt.pt = 127u8.into();
        pkt.seq_no = 0x1_0001u64.into();
        pkt.rtp_time = 0xAABB_CCDD;
        pkt.ssrc = 0x1122_3344u32.into();
        pkt.marker = true;
        assert_eq!(decode_bridge_frame(&encode_bridge_frame(&pkt)), Some(pkt));
    }

    #[test]
    fn roundtrips_empty_payload_and_long_mid() {
        let pkt = BridgeRtp::new(
            ParticipantId::new(),
            "a-fairly-long-mid-name",
            Bytes::new(),
            false,
        );
        assert_eq!(decode_bridge_frame(&encode_bridge_frame(&pkt)), Some(pkt));
    }

    #[test]
    fn bound_frame_roundtrips_call_generation_and_packet() {
        let call = CallId::new();
        let subscription_id = Uuid::new_v4();
        let packet = BridgeRtp::new(
            ParticipantId::new(),
            "video0",
            Bytes::from_static(b"bound-media"),
            true,
        );
        let encoded = encode_bound_bridge_frame(call, subscription_id, &packet);

        assert_eq!(
            decode_bound_bridge_frame(&encoded),
            Some(BoundBridgeFrame {
                wire_version: BOUND_BRIDGE_VERSION,
                call,
                subscription_id,
                packet,
            })
        );
        assert!(
            decode_bridge_frame(&encoded).is_none(),
            "the compatibility decoder must not erase a v4 binding"
        );
    }

    #[test]
    fn bound_frame_preserves_audio_gate_bypass() {
        let call = CallId::new();
        let subscription_id = Uuid::new_v4();
        let packet = BridgeRtp::new(
            ParticipantId::new(),
            "audio0",
            Bytes::from_static(b"opus"),
            false,
        )
        .with_keyframe_requirement(false);

        let decoded =
            decode_bound_bridge_frame(&encode_bound_bridge_frame(call, subscription_id, &packet))
                .expect("v4 frame");
        assert_eq!(decoded.packet, packet);
        assert!(!decoded.packet.requires_keyframe);
        assert_eq!(
            encoded_flag(&encode_bound_bridge_frame(
                call,
                subscription_id,
                &decoded.packet
            )) & FLAG_REQUIRES_KEYFRAME,
            0
        );
    }

    fn encoded_flag(frame: &[u8]) -> u8 {
        frame[BOUND_BODY_OFFSET]
    }

    #[test]
    fn v3_golden_layout_and_v4_decoder_compatibility() {
        let call = CallId::from_uuid(Uuid::from_u128(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff));
        let subscription_id = Uuid::from_u128(0xffee_ddcc_bbaa_9988_7766_5544_3322_1100);
        let participant = ParticipantId::from_ulid(Ulid(0x0123_4567_89ab_cdef_0011_2233_4455_6677));
        let mut packet = BridgeRtp::new(participant, "v0", Bytes::from_static(&[0xde, 0xad]), true);
        packet.rid = Some(Rid::from("hi"));
        packet.pt = 96u8.into();
        packet.seq_no = 0x0102_0304_0506_0708u64.into();
        packet.rtp_time = 0x1122_3344;
        packet.ssrc = 0x5566_7788u32.into();
        packet.marker = true;

        // Hand-built v3 fixture fixes every offset independently of the
        // encoder under test.
        let mut golden = vec![BOUND_BRIDGE_COMPAT_VERSION];
        golden.extend_from_slice(call.to_uuid().as_bytes());
        golden.extend_from_slice(subscription_id.as_bytes());
        golden.push(FLAG_KEYFRAME | FLAG_MARKER | FLAG_RID);
        golden.extend_from_slice(&participant.as_ulid().0.to_be_bytes());
        golden.push(96);
        golden.extend_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
        golden.extend_from_slice(&0x1122_3344u32.to_be_bytes());
        golden.extend_from_slice(&0x5566_7788u32.to_be_bytes());
        golden.push(2);
        golden.push(2);
        golden.extend_from_slice(b"v0");
        golden.extend_from_slice(b"hi");
        golden.extend_from_slice(&[0xde, 0xad]);

        assert_eq!(
            encode_bound_bridge_frame_version(
                call,
                subscription_id,
                &packet,
                BOUND_BRIDGE_COMPAT_VERSION
            ),
            Some(golden.clone())
        );
        let decoded = decode_bound_bridge_frame(&golden).expect("new pullers accept v3");
        assert_eq!(decoded.wire_version, BOUND_BRIDGE_COMPAT_VERSION);
        assert_eq!(decoded.call, call);
        assert_eq!(decoded.subscription_id, subscription_id);
        assert_eq!(
            decoded.packet,
            packet.with_keyframe_requirement(false),
            "v3 compatibility media bypasses the old all-track startup gate"
        );

        golden[BOUND_BODY_OFFSET] |= FLAG_REQUIRES_KEYFRAME;
        assert!(
            decode_bound_bridge_frame(&golden).is_none(),
            "v3 must reject the v4-only flag"
        );
    }

    #[test]
    fn compatibility_encoder_opens_old_audio_gate() {
        let packet = BridgeRtp::new(
            ParticipantId::new(),
            "audio0",
            Bytes::from_static(b"opus"),
            false,
        )
        .with_keyframe_requirement(false);
        let frame = encode_bound_bridge_frame_version(
            CallId::new(),
            Uuid::new_v4(),
            &packet,
            BOUND_BRIDGE_COMPAT_VERSION,
        )
        .expect("v3");
        assert_ne!(
            encoded_flag(&frame) & FLAG_KEYFRAME,
            0,
            "old pullers see non-video media as immediately decodable"
        );
        assert_eq!(encoded_flag(&frame) & FLAG_REQUIRES_KEYFRAME, 0);
    }

    #[test]
    fn bound_decoder_rejects_unbound_legacy_frame() {
        let packet = BridgeRtp::new(ParticipantId::new(), "audio0", Bytes::new(), false);
        assert!(decode_bound_bridge_frame(&encode_bridge_frame(&packet)).is_none());
    }

    #[test]
    fn malformed_frames_decode_to_none() {
        assert!(decode_bridge_frame(&[]).is_none());
        assert!(
            decode_bridge_frame(&[0u8; HEADER_MIN]).is_none(),
            "wrong version byte"
        );
        assert!(decode_bridge_frame(&[1u8; 5]).is_none(), "truncated header");
        // mid_len claims more bytes than are present.
        let mut f = encode_bridge_frame(&BridgeRtp::new(
            ParticipantId::new(),
            "x",
            Bytes::new(),
            false,
        ));
        f[35] = 200;
        assert!(
            decode_bridge_frame(&f).is_none(),
            "mid_len overruns the buffer"
        );

        let mut bad_pt = encode_bridge_frame(&BridgeRtp::new(
            ParticipantId::new(),
            "x",
            Bytes::new(),
            false,
        ));
        bad_pt[18] = 128;
        assert!(
            decode_bridge_frame(&bad_pt).is_none(),
            "payload type is only seven bits"
        );

        let mut missing_rid = encode_bridge_frame(&BridgeRtp::new(
            ParticipantId::new(),
            "x",
            Bytes::new(),
            false,
        ));
        missing_rid[1] |= FLAG_RID;
        assert!(
            decode_bridge_frame(&missing_rid).is_none(),
            "RID-present flag requires non-empty RID bytes"
        );
    }
}
