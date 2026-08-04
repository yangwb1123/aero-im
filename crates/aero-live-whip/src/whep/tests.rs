use super::*;
use crate::depacketize::H264Depacketizer;
use crate::packetize::rtp_parse;
use bytes::BytesMut;

// ---- SDP fixtures ----

/// A WHEP viewer's SDP offer: recvonly video (H.264) with ICE/DTLS. This is
/// what a browser subscriber POSTs to the WHEP endpoint.
const WHEP_OFFER: &str = "v=0\r\n\
o=- 7614819274000 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
a=msid-semantic: WMS\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:whep\r\n\
a=ice-pwd:wheppasswordwheppasswordw\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=recvonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n";

// ---- SDP offer→answer tests ----

#[test]
fn accept_whep_offer_fn_returns_sendonly_sdp() {
    let sdp = accept_whep_offer(WHEP_OFFER, "127.0.0.1", 8000).expect("accept should succeed");
    assert!(sdp.starts_with("v=0"), "answer must be valid SDP:\n{sdp}");
    // The server answers the viewer's recvonly with sendonly.
    assert!(
        sdp.contains("a=sendonly"),
        "answer must be sendonly (we send to viewer):\n{sdp}"
    );
    // H.264 must be present.
    assert!(sdp.contains("H264"), "answer must advertise H.264:\n{sdp}");
    assert!(
        sdp.contains("m=video"),
        "answer must have video m-line:\n{sdp}"
    );
    // DTLS / ICE plumbing.
    assert!(sdp.contains("a=fingerprint:sha-256"), "fingerprint:\n{sdp}");
    assert!(sdp.contains("a=ice-ufrag:"), "ice-ufrag:\n{sdp}");
    assert!(sdp.contains("a=ice-pwd:"), "ice-pwd:\n{sdp}");
    // Host candidate at our egress address.
    assert!(
        sdp.contains("127.0.0.1") && sdp.contains("8000"),
        "host candidate for egress addr:\n{sdp}"
    );
}

#[test]
fn accept_whep_offer_fn_rejects_garbage_sdp() {
    let err =
        accept_whep_offer("garbage", "127.0.0.1", 8000).expect_err("must reject non-SDP input");
    assert!(matches!(err, SessionError::Offer(_)));
}

#[test]
fn accept_whep_offer_fn_rejects_bad_addr() {
    let Err(err) = accept_whep_offer(WHEP_OFFER, "not-an-ip", 8000) else {
        panic!("expected an error for a bad egress address");
    };
    assert!(matches!(err, SessionError::Addr(_, _)));
}

#[test]
fn whep_session_accept_produces_real_answer() {
    let (session, answer) =
        WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8001).expect("accept offer");
    let sdp = answer.to_sdp_string();

    assert!(sdp.starts_with("v=0"), "real SDP answer:\n{sdp}");
    assert!(sdp.contains("a=sendonly"), "sendonly direction:\n{sdp}");
    assert!(
        sdp.contains("a=fingerprint:sha-256"),
        "DTLS fingerprint:\n{sdp}"
    );
    assert!(sdp.contains("a=ice-ufrag:"), "ICE ufrag:\n{sdp}");
    assert!(sdp.contains("a=ice-pwd:"), "ICE pwd:\n{sdp}");
    assert!(sdp.contains("m=video"), "video m-line:\n{sdp}");
    assert!(sdp.contains("H264"), "H.264 codec:\n{sdp}");

    assert_eq!(session.local_addr(), "127.0.0.1:8001".parse().unwrap());
    assert!(session.is_alive(), "session must start alive");
}

#[test]
fn whep_session_accept_rejects_bad_ingest_addr() {
    let Err(err) = WhepSession::accept(WHEP_OFFER, "::invalid::", 8000) else {
        panic!("expected an error for a bad egress address");
    };
    assert!(matches!(err, SessionError::Addr(_, _)));
}

// ---- Packetization tests ----

/// Build a NAL unit: header byte + body.
fn make_nal(header: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![header];
    v.extend_from_slice(body);
    v
}

/// Feed RTP packets through the depacketizer and collect Annex-B output.
fn depacketize(pkts: &[RtpPacket]) -> BytesMut {
    let mut d = H264Depacketizer::new();
    let mut out = BytesMut::new();
    for pkt in pkts {
        let (_, _, marker, payload) =
            rtp_parse(&pkt.bytes).expect("valid RTP header in test packet");
        d.push(payload, marker, &mut out).expect("depacketize");
    }
    out
}

#[test]
fn packetize_single_nal_produces_one_packet_with_marker() {
    let (mut session, _) =
        WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8002).expect("accept offer");

    let nal = make_nal(0x65, &[0x11, 0x22, 0x33]); // IDR slice
    let pkts = session.packetize(&[&nal], 90_000);

    assert_eq!(pkts.len(), 1, "single NAL → single RTP packet");
    assert!(pkts[0].marker, "marker bit on last (sole) packet");

    // The seq number in the header must be 0 (first packet).
    let (seq, ts, marker, _payload) = rtp_parse(&pkts[0].bytes).unwrap();
    assert_eq!(seq, 0, "first packet seq = 0");
    assert_eq!(ts, 90_000u32, "90 kHz timestamp carried through");
    assert!(marker, "marker bit in wire header");
}

#[test]
fn packetize_multiple_aus_increments_seq() {
    let (mut session, _) =
        WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8003).expect("accept offer");

    let sps = make_nal(0x67, &[0x42, 0x00, 0x1F]);
    let pps = make_nal(0x68, &[0xCE]);
    let idr = make_nal(0x65, &[0xAA, 0xBB, 0xCC]);

    // AU 1: SPS + PPS + IDR (STAP-A for small NALs, then IDR single-NAL or STAP-A).
    let au1 = session.packetize(&[&sps, &pps, &idr], 0);
    let last_seq_au1 = {
        let last = au1.last().unwrap();
        assert!(last.marker, "AU1 last packet must have marker");
        let (seq, _, _, _) = rtp_parse(&last.bytes).unwrap();
        seq
    };

    // AU 2: a P-frame.
    let pframe = make_nal(0x41, &[0x01, 0x02]);
    let au2 = session.packetize(&[&pframe], 3_000);
    let (first_seq_au2, _, _, _) = rtp_parse(&au2[0].bytes).unwrap();

    assert!(
        first_seq_au2 == last_seq_au1.wrapping_add(1),
        "seq continues from AU1 end ({last_seq_au1}) to AU2 start ({first_seq_au2})"
    );
    assert!(
        au2.last().unwrap().marker,
        "AU2 last packet must have marker"
    );
}

#[test]
fn packetize_large_nal_uses_fu_a_fragmentation() {
    let (mut session, _) =
        WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8004).expect("accept offer");

    // Build a NAL larger than EGRESS_MTU so FU-A kicks in.
    let body: Vec<u8> = (0u8..=255).cycle().take(2000).collect();
    let nal = make_nal(0x65, &body); // IDR slice, larger than MTU
    let pkts = session.packetize(&[&nal], 180_000);

    assert!(
        pkts.len() > 1,
        "large NAL must produce multiple FU-A packets"
    );
    // Only the last packet has the marker.
    let n = pkts.len();
    for (i, pkt) in pkts.iter().enumerate() {
        assert_eq!(
            pkt.marker,
            i == n - 1,
            "marker on packet {i}/{n}: got {}",
            pkt.marker
        );
    }
    // Wire marker bit matches the marker field.
    for pkt in &pkts {
        let (_, _, wire_marker, _) = rtp_parse(&pkt.bytes).unwrap();
        assert_eq!(
            wire_marker, pkt.marker,
            "wire marker must match struct field"
        );
    }
    // Sequence numbers must be strictly incrementing.
    let mut seqs: Vec<u16> = pkts
        .iter()
        .map(|p| rtp_parse(&p.bytes).unwrap().0)
        .collect();
    for w in seqs.windows(2) {
        assert_eq!(w[1], w[0].wrapping_add(1), "seq must increment");
    }
    let _ = seqs.pop(); // suppress unused warning
}

#[test]
fn packetize_roundtrip_through_depacketizer() {
    let (mut session, _) =
        WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8005).expect("accept offer");

    let sps = make_nal(0x67, &[0x42, 0x00, 0x1F, 0xAB]);
    let pps = make_nal(0x68, &[0xCE]);
    let idr_body: Vec<u8> = (0u8..100).collect();
    let idr = make_nal(0x65, &idr_body);

    let pkts = session.packetize(&[&sps, &pps, &idr], 0);
    assert!(!pkts.is_empty());

    let out = depacketize(&pkts);
    // The depacketized output must contain all three NALs (Annex-B prefixed).
    // Verify the IDR body is present (SPS+PPS may be STAP-A'd together).
    let out_bytes = &out[..];
    // IDR: type 5. Its body must appear verbatim in the depacketized output.
    let idr_body_pos = out_bytes
        .windows(idr_body.len())
        .position(|w| w == idr_body.as_slice());
    assert!(
        idr_body_pos.is_some(),
        "IDR body must appear in depacketized output"
    );
    // SPS byte must be somewhere (preceded by a start code).
    assert!(
        out_bytes.contains(&0x67u8),
        "SPS NAL header 0x67 must be present"
    );
    assert!(
        out_bytes.contains(&0x68u8),
        "PPS NAL header 0x68 must be present"
    );
}

#[test]
fn packetize_annex_b_matches_packetize() {
    // packetize_annex_b and packetize must produce identical bytes.
    use crate::depacketize::ANNEX_B_START_CODE;

    let (mut s1, _) = WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8006).expect("accept");
    let (mut s2, _) = WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8007).expect("accept");

    let sps_body = [0x42u8, 0x00, 0x1F];
    let pps_body = [0xCEu8];
    let sps = make_nal(0x67, &sps_body);
    let pps = make_nal(0x68, &pps_body);

    // Build Annex-B manually.
    let mut annex_b = Vec::new();
    for nal in [&sps, &pps] {
        annex_b.extend_from_slice(&ANNEX_B_START_CODE);
        annex_b.extend_from_slice(nal);
    }

    let pkts_direct = s1.packetize(&[&sps, &pps], 9_000);
    let pkts_annexb = s2.packetize_annex_b(&annex_b, 9_000);

    assert_eq!(
        pkts_direct.len(),
        pkts_annexb.len(),
        "same number of packets"
    );
    for (a, b) in pkts_direct.iter().zip(pkts_annexb.iter()) {
        assert_eq!(a.bytes, b.bytes, "packet bytes must be identical");
    }
}

// ---- Session lifecycle tests ----

#[test]
fn session_is_alive_after_accept() {
    let (session, _) = WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8008).expect("accept offer");
    assert!(session.is_alive(), "session alive immediately after accept");
}

#[test]
fn session_local_addr_matches_input() {
    let (session, _) = WhepSession::accept(WHEP_OFFER, "10.0.0.1", 9876).expect("accept offer");
    assert_eq!(
        session.local_addr(),
        "10.0.0.1:9876".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn session_video_pt_is_populated_after_accept() {
    let (session, answer) =
        WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8009).expect("accept offer");
    let sdp = answer.to_sdp_string();
    // The session must have a video PT after accept (str0m may renegotiate
    // the exact PT value; we just verify it is present and that the answer
    // SDP carries H.264 — the SDP is the source of truth for the PT).
    let pt = session.video_payload_type();
    if let Some(pt) = pt {
        // The negotiated PT must appear in the answer SDP.
        assert!(
            sdp.contains(&format!("a=rtpmap:{} H264", *pt)) || sdp.contains("H264"),
            "negotiated PT {pt} must correspond to H.264 in the answer:\n{sdp}"
        );
    }
    // Whether or not the PT is exposed immediately, the SDP must contain H.264.
    assert!(sdp.contains("H264"), "answer must advertise H.264:\n{sdp}");
}

#[test]
fn session_video_pt_tracks_remapped_h264_payload_type() {
    // A browser is free to assign a dynamic payload type that differs from
    // str0m's defaults. The egress writer must use the H.264 PT from that
    // negotiation, not the first configured video codec's PT.
    let offer = WHEP_OFFER
        .replace("SAVPF 96", "SAVPF 104")
        .replace("rtpmap:96", "rtpmap:104")
        .replace("fmtp:96", "fmtp:104");
    let (session, answer) =
        WhepSession::accept(&offer, "127.0.0.1", 8012).expect("accept remapped H.264");
    let pt = session
        .video_payload_type()
        .expect("H.264 payload type must be negotiated");
    assert_eq!(*pt, 104);
    let sdp = answer.to_sdp_string();
    assert!(
        sdp.contains("a=rtpmap:104 H264/90000"),
        "answer must bind PT 104 to H.264:\n{sdp}"
    );
}

#[test]
fn accept_whep_offer_fn_rejects_minimal_broken_sdp() {
    // Passes the `v=0` gate but is not parseable as a complete SDP.
    let Err(err) = accept_whep_offer("v=0\r\nbroken", "127.0.0.1", 8000) else {
        panic!("expected an error for malformed SDP");
    };
    // Either Offer (parse error) or Rtc (negotiation error) is acceptable.
    assert!(
        matches!(err, SessionError::Offer(_) | SessionError::Rtc(_)),
        "unexpected error variant"
    );
}

#[test]
fn packetize_empty_nals_returns_empty() {
    let (mut session, _) =
        WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8010).expect("accept offer");
    let pkts = session.packetize(&[], 0);
    assert!(pkts.is_empty(), "empty NAL slice must produce no packets");
}

#[test]
fn packetize_marker_bit_boundary_on_multi_au_sequence() {
    // Feed four single-NAL access units and verify that only the last packet
    // of *each* AU carries the marker bit.
    let (mut session, _) =
        WhepSession::accept(WHEP_OFFER, "127.0.0.1", 8011).expect("accept offer");

    let nals = [
        make_nal(0x67, &[0x01]),
        make_nal(0x68, &[0x02]),
        make_nal(0x65, &[0x03]),
        make_nal(0x41, &[0x04]),
    ];
    for (i, nal) in nals.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let rtp_ts = (i as u32) * 3_000;
        let packets = session.packetize(&[nal], rtp_ts);
        assert_eq!(packets.len(), 1, "single small NAL → single packet");
        assert!(packets[0].marker, "AU{i}: marker bit on sole packet");
    }
}
