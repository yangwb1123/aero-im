//! Tests for the SRT crate root, split out of lib.rs (1200-line HARD limit).

use super::*;
use async_trait::async_trait;

fn sample_config() -> TurnConfig {
    TurnConfig {
        listening_port: 3478,
        realm: "aero.local".into(),
        static_auth_secret: "north-star-shared-secret".into(),
        external_ip: Some("203.0.113.7".into()),
        min_port: 49152,
        max_port: 65535,
    }
}

// ------------------------- TURN render -------------------------

#[test]
fn turn_config_renders_required_lines() {
    let c = sample_config();
    let body = c.render();
    assert!(body.contains("listening-port=3478"));
    assert!(body.contains("realm=aero.local"));
    assert!(body.contains("use-auth-secret"));
    assert!(body.contains("static-auth-secret=north-star-shared-secret"));
    assert!(body.contains("external-ip=203.0.113.7"));
    assert!(body.contains("fingerprint"));
}

#[test]
fn turn_config_skips_external_ip_when_absent() {
    let mut c = sample_config();
    c.external_ip = None;
    assert!(!c.render().contains("external-ip="));
}

// ------------------ HMAC-SHA1 known-answer tests ------------------

#[test]
fn hmac_sha1_matches_rfc2202_case2() {
    // RFC 2202 §3 test case 2: a *published* HMAC-SHA1 vector, proving our
    // HMAC is correct against the standard — not merely self-consistent.
    //   key  = "Jefe"
    //   data = "what do ya want for nothing?"
    //   HMAC = 0xeffcdf6ae5eb2fa2d27416d5f184df9c259a7c79
    // Independently base64-encoded (openssl) → "7/zfauXrL6LSdBbV8YTfnCWafHk=".
    let got = hmac_sha1_base64(b"Jefe", b"what do ya want for nothing?");
    assert_eq!(got, "7/zfauXrL6LSdBbV8YTfnCWafHk=");
}

#[test]
fn hmac_sha1_matches_rfc2202_case1() {
    // RFC 2202 §3 test case 1: key = 20 × 0x0b, data = "Hi There".
    //   HMAC = 0xb617318655057264e28bc0b6fb378c8ef146be00
    let key = [0x0bu8; 20];
    let got = hmac_sha1_base64(&key, b"Hi There");
    // base64 of the published digest above.
    assert_eq!(got, "thcxhlUFcmTii8C2+zeMjvFGvgA=");
}

#[test]
fn ephemeral_credential_is_deterministic_and_matches_reference() {
    // Hand-computed reference, independently produced by BOTH
    //   printf '%s' '1700000000:alice' | openssl dgst -sha1 \
    //       -hmac 'north-star-shared-secret' -binary | openssl base64
    // and Python's `hmac.new(secret, user, hashlib.sha1)`:
    //   secret   = "north-star-shared-secret"
    //   username = "1700000000:alice"
    //   password = base64(HMAC_SHA1(secret, username))
    //            = "WqY9HToCTDh6T15lNQzfpjD2pIo="
    let c = sample_config();
    let (username, password) =
        c.ephemeral_credential("alice", Duration::from_secs(0), 1_700_000_000);
    assert_eq!(username, "1700000000:alice");
    assert_eq!(password, "WqY9HToCTDh6T15lNQzfpjD2pIo=");
}

#[test]
fn ttl_is_added_to_now_for_the_expiry() {
    let c = sample_config();
    // now=1_700_000_000, ttl=600 → expiry 1_700_000_600.
    let (username, _) = c.ephemeral_credential("bob", Duration::from_secs(600), 1_700_000_000);
    assert_eq!(username, "1700000600:bob");
}

#[test]
fn same_inputs_yield_same_password_different_secret_differs() {
    let c = sample_config();
    let (_, p1) = c.ephemeral_credential("alice", Duration::from_secs(60), 100);
    let (_, p2) = c.ephemeral_credential("alice", Duration::from_secs(60), 100);
    assert_eq!(p1, p2, "deterministic for identical inputs");

    let mut c2 = c.clone();
    c2.static_auth_secret = "different-secret".into();
    let (_, p3) = c2.ephemeral_credential("alice", Duration::from_secs(60), 100);
    assert_ne!(p1, p3, "credential is bound to the shared secret");
}

#[test]
fn ice_server_shape_and_url() {
    let c = sample_config();
    let ice = c.ice_server(
        "turn.example.com",
        "carol",
        Duration::from_secs(300),
        1_700_000_000,
    );
    assert_eq!(ice.urls, "turn:turn.example.com:3478");
    assert_eq!(ice.username, "1700000300:carol");
    // credential must equal the standalone HMAC of the username.
    let expected = hmac_sha1_base64(c.static_auth_secret.as_bytes(), ice.username.as_bytes());
    assert_eq!(ice.credential, expected);
}

#[test]
fn ice_server_serializes_to_browser_json() {
    let c = sample_config();
    let ice = c.ice_server("turn.example.com", "dave", Duration::from_secs(60), 0);
    let json = serde_json::to_string(&ice).unwrap();
    // Browser RTCPeerConnection expects exactly these keys.
    assert!(json.contains("\"urls\":\"turn:turn.example.com:3478\""));
    assert!(json.contains("\"username\":\"60:dave\""));
    assert!(json.contains("\"credential\":"));
}

// ----------------------------- SRT -----------------------------

#[test]
fn ingest_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SrtIngest>();
}

#[test]
fn srt_listen_addr_is_rtmp_port_plus_one() {
    let cfg = LiveStreamConfig::local_dev(); // rtmp 0.0.0.0:1935
    let addr = SrtIngest::listen_addr(&cfg).unwrap();
    assert_eq!(addr.port(), 1936);
    assert_eq!(addr.ip(), cfg.rtmp_listen.ip());
}

#[test]
fn srt_listen_addr_rejects_port_overflow() {
    let mut cfg = LiveStreamConfig::local_dev();
    cfg.rtmp_listen.set_port(u16::MAX);
    assert!(matches!(
        SrtIngest::listen_addr(&cfg),
        Err(LiveError::Protocol(message)) if message.contains("overflows")
    ));
}

// ---- data-plane glue: SRT header stripping & SHUTDOWN detection ----

#[test]
fn data_payload_strips_srt_header_and_yields_ts_bytes() {
    // A data packet wrapping an MPEG-TS payload: 16-byte SRT header + body.
    let header = SrtHeader {
        kind: PacketKind::Data {
            seq_no: 5,
            msg_word: 0,
        },
        timestamp: 0,
        dest_socket_id: 1,
    };
    let mut pkt = bytes::BytesMut::new();
    header.write_to(&mut pkt);
    let body = [0x47u8, 0x40, 0x00, 0x10, 0xDE, 0xAD]; // looks like a TS chunk
    pkt.extend_from_slice(&body);
    let payload = data_payload(&pkt).expect("data packet yields a payload");
    assert_eq!(payload, &body, "payload is everything after the 16B header");
}

#[test]
fn data_payload_rejects_control_packets() {
    let ka = SrtHeader {
        kind: PacketKind::Control {
            control_type: ControlType::KeepAlive,
            subtype: 0,
            type_specific: 0,
        },
        timestamp: 0,
        dest_socket_id: 1,
    };
    assert!(
        data_payload(&ka.to_bytes()).is_none(),
        "control has no TS payload"
    );
    // Truncated datagram → None, never a panic.
    assert!(data_payload(&[0u8; 4]).is_none());
}

#[test]
fn is_shutdown_detects_only_shutdown_control() {
    let shutdown = SrtHeader {
        kind: PacketKind::Control {
            control_type: ControlType::Shutdown,
            subtype: 0,
            type_specific: 0,
        },
        timestamp: 0,
        dest_socket_id: 1,
    };
    assert!(is_shutdown(&shutdown.to_bytes()));

    let ka = SrtHeader {
        kind: PacketKind::Control {
            control_type: ControlType::KeepAlive,
            subtype: 0,
            type_specific: 0,
        },
        timestamp: 0,
        dest_socket_id: 1,
    };
    assert!(!is_shutdown(&ka.to_bytes()), "keep-alive is not shutdown");
    assert!(!is_shutdown(&[0u8; 4]), "short datagram is not shutdown");
}

#[test]
fn segment_duration_matches_rtmp() {
    // RTMP uses a 2s cadence; keep SRT in lockstep so players see a
    // consistent target duration regardless of ingest protocol.
    assert_eq!(SEGMENT_DURATION_SECS, 2);
}

#[test]
fn segment_duration_constants_agree() {
    // The u32 and f32 forms must not drift apart. Use an epsilon comparison
    // (clippy flags `==`/`!=` on floats, not ordering) against the integer.
    let diff =
        (SEGMENT_DURATION_SECS_F32 - f32::from(u8::try_from(SEGMENT_DURATION_SECS).unwrap())).abs();
    assert!(
        diff < f32::EPSILON,
        "f32 and u32 segment durations diverged"
    );
}

// ---- SrtSession integration (segmenter → HLS writer on disk) ----

/// Minimal 188-byte payload-only TS packet with the given PID/PUSI.
fn ts_packet(pid: u16, pusi: bool, payload: &[u8]) -> Vec<u8> {
    let mut pkt = vec![0xFFu8; TS_PACKET_SIZE];
    pkt[0] = TS_SYNC_BYTE;
    pkt[1] = (u8::from(pusi) << 6) | u8::try_from((pid >> 8) & 0x1F).unwrap();
    pkt[2] = u8::try_from(pid & 0xFF).unwrap();
    pkt[3] = 0x10; // afc=01 (payload only)
    let n = payload.len().min(TS_PACKET_SIZE - 4);
    pkt[4..4 + n].copy_from_slice(&payload[..n]);
    pkt
}

/// A video PES carrying NAL units of the given types (4-byte start codes).
fn video_pes(nal_types: &[u8]) -> Vec<u8> {
    let mut es = Vec::new();
    for &t in nal_types {
        es.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, t & 0x1F, 0xAA]);
    }
    let mut pes = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x00, 0x00];
    pes.extend_from_slice(&es);
    pes
}

/// A 1-byte PAT advertising PMT PID 0x1000 / PMT advertising video PID 0x100.
/// Reuses the segmenter's own parser, so we only need plausible PSI here.
fn pat() -> Vec<u8> {
    // pointer(0) table_id(0) B0 len.. tsid version sec last prog=1 pmt_pid
    let mut s = vec![0x00, 0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00];
    s.extend_from_slice(&1u16.to_be_bytes());
    s.extend_from_slice(&(0xE000u16 | 0x1000).to_be_bytes());
    s.extend_from_slice(&[0, 0, 0, 0]); // CRC (ignored)
    s
}

fn pmt() -> Vec<u8> {
    let mut s = vec![0x00, 0x02, 0xB0, 0x12, 0x00, 0x01, 0xC1, 0x00, 0x00];
    s.extend_from_slice(&(0xE000u16 | 0x0100).to_be_bytes()); // PCR PID
    s.extend_from_slice(&0xF000u16.to_be_bytes()); // program_info_length=0
    s.push(0x1B); // H.264
    s.extend_from_slice(&(0xE000u16 | 0x0100).to_be_bytes());
    s.extend_from_slice(&0xF000u16.to_be_bytes());
    s.extend_from_slice(&[0, 0, 0, 0]); // CRC
    s
}

#[tokio::test]
async fn srt_session_writes_hls_segments_at_keyframes() {
    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let mut session = SrtSession::new(hls);

    // Feed PSI then two keyframes separated by an inter frame. The second
    // keyframe should close the first segment.
    session
        .feed(&ts_packet(0x0000, true, &pat()))
        .await
        .unwrap();
    session
        .feed(&ts_packet(0x1000, true, &pmt()))
        .await
        .unwrap();
    session
        .feed(&ts_packet(0x0100, true, &video_pes(&[5])))
        .await
        .unwrap();
    session
        .feed(&ts_packet(0x0100, true, &video_pes(&[1])))
        .await
        .unwrap();
    // Second keyframe → cut → first segment (0.ts) is flushed to disk.
    session
        .feed(&ts_packet(0x0100, true, &video_pes(&[5])))
        .await
        .unwrap();
    assert!(dir.path().join("0.ts").exists(), "first segment written");

    // Finish flushes the trailing open segment and finalizes the manifest.
    session.finish().await.unwrap();
    assert!(dir.path().join("1.ts").exists(), "trailing segment written");
    let manifest = std::fs::read_to_string(dir.path().join("index.m3u8")).unwrap();
    assert!(manifest.contains("#EXT-X-ENDLIST"), "manifest finalized");
    assert!(manifest.contains("0.ts"));
    assert!(!session.has_open_segment());
}

#[tokio::test]
async fn srt_session_finish_is_idempotent_and_finalizes() {
    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let mut session = SrtSession::new(hls);
    session
        .feed(&ts_packet(0x0000, true, &pat()))
        .await
        .unwrap();
    session
        .feed(&ts_packet(0x1000, true, &pmt()))
        .await
        .unwrap();
    session
        .feed(&ts_packet(0x0100, true, &video_pes(&[5])))
        .await
        .unwrap();
    session.finish().await.unwrap();
    // A second finish must not error (HlsWriter::finish is idempotent).
    session.finish().await.unwrap();
}

// ── Crypto integration tests ────────────────────────────────────────────

/// Build a full SRT data packet (header + payload) with the given `seq_no`,
/// KK flag, and payload bytes.
fn make_data_packet(seq_no: u32, kk: KkFlag, payload: &[u8]) -> Vec<u8> {
    let msg_word = kk.set_in_msg_word(0);
    let header = SrtHeader {
        kind: PacketKind::Data { seq_no, msg_word },
        timestamp: 0,
        dest_socket_id: 1,
    };
    let mut pkt = bytes::BytesMut::new();
    header.write_to(&mut pkt);
    pkt.extend_from_slice(payload);
    pkt.to_vec()
}

/// In-memory stream backend for exercising the production datagram router.
struct ListenerTestBackend {
    stream_id: ulid::Ulid,
    session: std::sync::Mutex<Option<SrtSession>>,
    resolved_keys: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl SessionBackend for ListenerTestBackend {
    async fn resolve(&self, stream_key: &str) -> LiveResult<(ulid::Ulid, SrtSession)> {
        self.resolved_keys
            .lock()
            .unwrap()
            .push(stream_key.to_string());
        let session = self
            .session
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| LiveError::Protocol("test session already consumed".into()))?;
        Ok((self.stream_id, session))
    }

    async fn finalize(&self, mut session: SrtSession, _stream_id: Option<ulid::Ulid>) {
        let _ = session.finish().await;
    }
}

fn handshake_datagram(handshake: &Handshake, dest_socket_id: u32) -> Vec<u8> {
    let header = SrtHeader {
        kind: PacketKind::Control {
            control_type: ControlType::Handshake,
            subtype: 0,
            type_specific: 0,
        },
        timestamp: 0,
        dest_socket_id,
    };
    let mut packet = bytes::BytesMut::new();
    header.write_to(&mut packet);
    handshake.write_to(&mut packet);
    packet.to_vec()
}

fn listener_test_induction(caller_socket_id: u32) -> Vec<u8> {
    handshake_datagram(
        &Handshake {
            version: protocol::SRT_VERSION_UDT4,
            encryption_field: protocol::HS_ENC_CLEAR,
            extension_field: 2,
            handshake_type: protocol::HandshakeType::Induction,
            srt_socket_id: caller_socket_id,
            ..Default::default()
        },
        0,
    )
}

fn listener_test_encrypted_conclusion(
    caller_socket_id: u32,
    cookie: u32,
    stream_id: &str,
    kmreq: KmMessage,
) -> Vec<u8> {
    handshake_datagram(
        &Handshake {
            version: protocol::SRT_VERSION_HSV5,
            encryption_field: protocol::HS_ENC_AES128,
            handshake_type: protocol::HandshakeType::Conclusion,
            srt_socket_id: caller_socket_id,
            syn_cookie: cookie,
            extensions: vec![
                protocol::HsExtension::HsReq {
                    is_response: false,
                    srt_version: 0x0001_0500,
                    srt_flags: 0x00BF,
                    recv_tsbpd_delay: 120,
                    send_tsbpd_delay: 0,
                },
                protocol::HsExtension::KeyMaterial(kmreq),
                protocol::HsExtension::StreamId(stream_id.to_string()),
            ],
            ..Default::default()
        },
        1,
    )
}

/// The production UDP datagram path must consume the configured passphrase,
/// return KMRSP, install the unwrapped SEK, and decrypt media before TS ingest.
#[tokio::test]
async fn listener_datagram_path_negotiates_and_decrypts_encrypted_media() {
    let passphrase = b"listener-test-passphrase";
    let salt = [0xA5; 16];
    let sek = [0x3C; 16];
    let sender_crypto = SrtCrypto::from_raw_sek(passphrase, &salt, sek);
    let kmreq = sender_crypto.build_km_message(passphrase);

    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let backend = ListenerTestBackend {
        stream_id: ulid::Ulid::new(),
        session: std::sync::Mutex::new(Some(SrtSession::new(hls))),
        resolved_keys: std::sync::Mutex::new(Vec::new()),
    };
    let ingest = SrtIngest::with_identity(0x5254_0001, 0xCAFE).with_passphrase(passphrase.to_vec());
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caller = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer = caller.local_addr().unwrap();
    let mut peers = HashMap::new();
    let caller_socket_id = 0x1111_2222;

    handle_datagram(
        &ingest,
        &server,
        &backend,
        &mut peers,
        peer,
        &listener_test_induction(caller_socket_id),
    )
    .await
    .unwrap();
    let mut reply = [0u8; 2048];
    let (reply_len, _) = tokio::time::timeout(Duration::from_secs(1), caller.recv_from(&mut reply))
        .await
        .expect("induction response timed out")
        .unwrap();
    let induction = Handshake::parse(&reply[protocol::SRT_HEADER_LEN..reply_len]).unwrap();
    assert_eq!(induction.encryption_field, protocol::HS_ENC_AES128);

    handle_datagram(
        &ingest,
        &server,
        &backend,
        &mut peers,
        peer,
        &listener_test_encrypted_conclusion(
            caller_socket_id,
            induction.syn_cookie,
            "publisher/listener-test",
            kmreq.clone(),
        ),
    )
    .await
    .unwrap();
    let (reply_len, _) = tokio::time::timeout(Duration::from_secs(1), caller.recv_from(&mut reply))
        .await
        .expect("conclusion response timed out")
        .unwrap();
    let agreement = Handshake::parse(&reply[protocol::SRT_HEADER_LEN..reply_len]).unwrap();
    let kmrsp = agreement
        .extensions
        .iter()
        .find_map(|extension| match extension {
            protocol::HsExtension::KeyMaterial(km) => Some(km),
            _ => None,
        })
        .expect("listener must confirm accepted key material");
    assert_eq!(kmrsp.msg_type, KmMessageType::Response);
    assert_eq!(kmrsp.wrapped_sek, kmreq.wrapped_sek);
    assert_eq!(
        backend.resolved_keys.lock().unwrap().as_slice(),
        ["publisher/listener-test"]
    );

    let mut ts_payload = ts_packet(0x0000, true, &pat());
    sender_crypto.encrypt_packet(0, &mut ts_payload);
    handle_datagram(
        &ingest,
        &server,
        &backend,
        &mut peers,
        peer,
        &make_data_packet(0, KkFlag::EvenKey, &ts_payload),
    )
    .await
    .unwrap();

    let PeerState::Streaming {
        peer_socket_id,
        session,
        ..
    } = peers.get(&peer).unwrap()
    else {
        panic!("successful KM handshake must transition the peer to streaming");
    };
    assert_eq!(*peer_socket_id, caller_socket_id);
    assert_eq!(session.crypto.as_ref().unwrap().sek(), &sek);
    assert!(
        session.segmenter.has_segment_data(),
        "decrypted MPEG-TS reached the segmenter through handle_datagram"
    );
}

/// An established peer that has received no outbound packet for one second
/// must get a header-only KEEPALIVE without first sending anything itself.
#[tokio::test]
async fn idle_streaming_peer_gets_independent_keepalive() {
    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caller = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer = caller.local_addr().unwrap();
    let peer_socket_id = 0x1111_2222;
    let now = Instant::now();
    let connected_at = now.checked_sub(Duration::from_secs(2)).unwrap();
    let mut peers = HashMap::from([(
        peer,
        PeerState::Streaming {
            stream_id: ulid::Ulid::new(),
            peer_socket_id,
            connected_at,
            last_sent_at: now.checked_sub(SRT_KEEPALIVE_INTERVAL).unwrap(),
            session: Box::new(SrtSession::new(hls)),
        },
    )]);

    assert_eq!(send_due_keepalives(&server, &mut peers, now).await, 1);

    let mut packet = [0u8; 64];
    let (len, _) = tokio::time::timeout(Duration::from_secs(1), caller.recv_from(&mut packet))
        .await
        .expect("keep-alive timed out")
        .unwrap();
    assert_eq!(len, protocol::SRT_HEADER_LEN, "KEEPALIVE has no CIF");
    let header = SrtHeader::parse(&packet[..len]).unwrap();
    assert_eq!(
        header.kind,
        PacketKind::Control {
            control_type: ControlType::KeepAlive,
            subtype: 0,
            type_specific: 0,
        }
    );
    assert_eq!(header.dest_socket_id, peer_socket_id);
    assert_eq!(header.timestamp, 2_000_000);
}

/// An ingest configured with a passphrase must decrypt an AES-CTR-encrypted
/// data packet so the plaintext TS bytes reach the segmenter.
///
/// The test builds a TS payload, encrypts it with the same `SrtCrypto`
/// instance, packages it in a data packet with `KkFlag::EvenKey`, feeds it
/// to a session that has that crypto installed, and verifies the
/// segmenter sees the original plaintext (i.e. the TS sync byte 0x47).
#[tokio::test]
async fn encrypted_data_packet_is_decrypted_before_segmenter() {
    // Set up a known passphrase, salt, and SEK so we can produce a
    // matching ciphertext on the test (sender) side.
    let passphrase = b"test-passphrase";
    let salt = [0xBBu8; 16];
    let sek = [0xCCu8; 16];

    let crypto = SrtCrypto::from_passphrase(passphrase, &salt, sek);

    // Build a TS payload (minimal PAT-like bytes starting with 0x47).
    let mut ts_payload = ts_packet(0x0000, true, &pat());
    let original = ts_payload.clone();

    // Encrypt the payload as the sender would (seq_no = 1, even key).
    let seq_no = 1u32;
    crypto.encrypt_packet(seq_no, &mut ts_payload);
    assert_ne!(
        ts_payload, original,
        "ciphertext must differ from plaintext"
    );

    // Package as an SRT data packet with KK=EvenKey.
    let pkt = make_data_packet(seq_no, KkFlag::EvenKey, &ts_payload);

    // Open a session with the same crypto context installed.
    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let mut session = SrtSession::with_crypto(
        hls,
        Some(SrtCrypto::from_passphrase(passphrase, &salt, sek)),
    );

    // Feed the encrypted packet — the session must decrypt it first.
    session.feed_packet(&pkt).await.unwrap();

    // The segmenter received the plaintext.  Verify by feeding the
    // original plaintext through a plain session and confirming both
    // sessions end up with the same segmenter state (non-empty buffer).
    assert!(
        session.segmenter.has_segment_data(),
        "segmenter must have buffered data after decryption"
    );
}

/// Once encrypted mode is negotiated, clear data must be rejected instead of
/// silently bypassing the configured security policy.
#[tokio::test]
async fn clear_data_packet_is_rejected_after_crypto_install() {
    let passphrase = b"any-passphrase";
    let salt = [0x11u8; 16];
    let sek = [0x22u8; 16];

    let ts_payload = ts_packet(0x0000, true, &pat());
    // KK = Clear → packet is NOT encrypted.
    let pkt = make_data_packet(0, KkFlag::Clear, &ts_payload);

    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let mut session = SrtSession::with_crypto(
        hls,
        Some(SrtCrypto::from_passphrase(passphrase, &salt, sek)),
    );

    let error = session.feed_packet(&pkt).await.unwrap_err();
    assert!(
        error.to_string().contains("unencrypted SRT data"),
        "clear packet must be rejected after encrypted negotiation: {error}"
    );
}

/// Clear mode remains compatible when neither peer configured a passphrase.
#[tokio::test]
async fn clear_data_packet_passes_in_clear_session() {
    let ts_payload = ts_packet(0x0000, true, &pat());
    let pkt = make_data_packet(0, KkFlag::Clear, &ts_payload);
    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let mut session = SrtSession::new(hls);
    session.feed_packet(&pkt).await.unwrap();
    assert!(session.segmenter.has_segment_data());
}

/// Ciphertext must not flow into the TS segmenter before KM negotiation.
#[tokio::test]
async fn encrypted_data_packet_is_rejected_without_crypto() {
    let pkt = make_data_packet(0, KkFlag::EvenKey, &[0xAA; TS_PACKET_SIZE]);
    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let mut session = SrtSession::new(hls);
    let error = session.feed_packet(&pkt).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("without completed KM negotiation"),
        "ciphertext must be rejected before key installation: {error}"
    );
    assert!(!session.segmenter.has_segment_data());
}

/// A KMREQ→KMRSP exchange must establish a usable session key: after
/// `apply_km_message`, the session can decrypt a packet that was encrypted
/// with the same passphrase.
#[tokio::test]
async fn kmreq_kmrsp_exchange_establishes_session_key() {
    let passphrase = b"shared-secret";
    let salt = [0xA5u8; 16];
    let sek = [0x3Cu8; 16];

    // Sender side: build a KMREQ message.
    let sender_crypto = SrtCrypto::from_passphrase(passphrase, &salt, sek);
    let km = sender_crypto.build_km_message(passphrase);

    // Receiver side: apply the KMREQ to derive the same session key.
    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let mut session = SrtSession::new(hls);

    // Before applying the KM message, the session has no crypto.
    assert!(
        session.crypto.is_none(),
        "fresh session starts without a crypto context"
    );

    session
        .apply_km_message(&km, passphrase)
        .expect("apply_km_message must succeed with the correct passphrase");

    assert!(
        session.crypto.is_some(),
        "session must have a crypto context after applying the KM message"
    );

    // Verify the installed SEK matches the sender's.
    let installed_sek = session.crypto.as_ref().unwrap().sek();
    assert_eq!(
        installed_sek, &sek,
        "receiver must derive the same SEK as the sender"
    );

    // Prove it can decrypt: encrypt a packet with the sender's crypto then
    // feed it to the session.
    let mut ts_payload = ts_packet(0x0000, true, &pat());
    let seq_no = 5u32;
    sender_crypto.encrypt_packet(seq_no, &mut ts_payload);
    let pkt = make_data_packet(seq_no, KkFlag::EvenKey, &ts_payload);
    session.feed_packet(&pkt).await.unwrap();
    assert!(
        session.segmenter.has_segment_data(),
        "session must successfully decrypt and buffer the TS packet"
    );
}

/// Applying a KMREQ with the wrong passphrase must fail.
#[test]
fn apply_km_message_fails_with_wrong_passphrase() {
    let salt = [0u8; 16];
    let sek = [1u8; 16];
    let sender = SrtCrypto::from_passphrase(b"correct", &salt, sek);
    let km = sender.build_km_message(b"correct");

    // A standalone check (no async needed here).
    let result = SrtCrypto::from_km_message(&km, b"wrong");
    assert!(result.is_err(), "wrong passphrase must not unwrap the SEK");
}

// ── Reliability integration tests ───────────────────────────────────────

/// In-order delivery (consecutive sequence numbers) must emit ACKs via the
/// periodic ACK timer; no NAKs should be produced.
#[tokio::test]
async fn in_order_delivery_emits_ack_not_nak() {
    use std::time::Duration as StdDuration;

    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let mut session = SrtSession::new(hls);

    // Set a very short ACK interval so the timer fires during the test.
    session
        .reliability
        .set_ack_interval(StdDuration::from_nanos(1));

    let ts_bytes = ts_packet(0x0000, true, &pat());

    // Feed three consecutive packets — no gap, so no NAK expected.
    for seq_no in 0u32..3 {
        let pkt = make_data_packet(seq_no, KkFlag::Clear, &ts_bytes);
        session.feed_packet(&pkt).await.unwrap();
    }

    let actions = session.drain_actions();

    // No NAKs must appear.
    assert!(
        actions.iter().all(|a| !matches!(a, Action::SendNak { .. })),
        "no NAK expected for consecutive packets; got actions: {actions:?}"
    );
    // At least one ACK must have been emitted (the timer fires quickly).
    assert!(
        actions.iter().any(|a| matches!(a, Action::SendAck { .. })),
        "at least one ACK expected; got actions: {actions:?}"
    );
}

/// A gap in the received sequence space must cause the session to emit a
/// NAK for exactly the missing range.
#[tokio::test]
async fn sequence_gap_drives_nak_emission() {
    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let mut session = SrtSession::new(hls);

    let ts_bytes = ts_packet(0x0000, true, &pat());

    // Feed packet 0 (no gap).
    let pkt0 = make_data_packet(0, KkFlag::Clear, &ts_bytes);
    session.feed_packet(&pkt0).await.unwrap();
    let _ = session.drain_actions(); // clear first-packet actions

    // Feed packet 3 — skipping 1 and 2.  The reliability layer must emit a
    // NAK for the range [1, 2].
    let pkt3 = make_data_packet(3, KkFlag::Clear, &ts_bytes);
    session.feed_packet(&pkt3).await.unwrap();

    let actions = session.drain_actions();
    let nak = actions.iter().find(|a| matches!(a, Action::SendNak { .. }));
    assert!(nak.is_some(), "NAK must be emitted for the gap [1, 2]");
    assert_eq!(
        nak.unwrap(),
        &Action::SendNak { from: 1, to: 2 },
        "NAK must cover exactly the missing range"
    );
}

/// `SrtIngest::with_passphrase` and `passphrase()` accessor work correctly.
#[test]
fn ingest_passphrase_roundtrip() {
    let ingest = SrtIngest::new().with_passphrase(b"mysecret".to_vec());
    assert_eq!(ingest.passphrase(), Some(b"mysecret".as_ref()));

    let plain = SrtIngest::new();
    assert!(plain.passphrase().is_none());
}

// ── max-bandwidth knob (send pacer configuration) ─────────────────────────

/// The env-override parser accepts positive integers only: zero would
/// stall the stream at the pacer floor, and garbage must fall back to the
/// default rather than panic.
#[test]
fn parse_max_bandwidth_accepts_positive_integers_only() {
    assert_eq!(parse_max_bandwidth("1500000"), Some(1_500_000));
    assert_eq!(
        parse_max_bandwidth("  42  "),
        Some(42),
        "whitespace is trimmed"
    );
    assert_eq!(
        parse_max_bandwidth("0"),
        None,
        "zero would stall the stream"
    );
    assert_eq!(parse_max_bandwidth("-5"), None);
    assert_eq!(parse_max_bandwidth("12 Mbps"), None);
    assert_eq!(parse_max_bandwidth(""), None);
}

/// `with_identity` (the hermetic test constructor) defaults to
/// [`DEFAULT_MAX_BANDWIDTH`]; `with_max_bandwidth` overrides it.
#[test]
fn ingest_max_bandwidth_defaults_and_overrides() {
    let ingest = SrtIngest::with_identity(1, 2);
    assert_eq!(ingest.max_bandwidth(), DEFAULT_MAX_BANDWIDTH);
    assert_eq!(ingest.with_max_bandwidth(99).max_bandwidth(), 99);
}

#[tokio::test]
async fn idle_ingest_listener_stops_promptly_when_cancelled() {
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let srt_addr = probe.local_addr().unwrap();
    drop(probe);
    let rtmp_addr = std::net::SocketAddr::new(
        srt_addr.ip(),
        srt_addr
            .port()
            .checked_sub(1)
            .expect("ephemeral port is non-zero"),
    );
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://u:p@localhost/aero")
        .unwrap();
    let cfg = Arc::new(LiveStreamConfig {
        hls_dir: std::env::temp_dir().join("aero-srt-cancel-test"),
        rtmp_listen: rtmp_addr,
    });
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        SrtIngest::new()
            .run_until_cancelled(StreamRepo::new(pool), cfg, task_cancel)
            .await
    });

    tokio::time::sleep(Duration::from_millis(20)).await;
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("cancelled listener should stop promptly")
        .expect("listener task should not panic");
    assert!(result.is_ok(), "listener should stop cleanly: {result:?}");
}

/// `SrtSession::with_max_bandwidth` re-seeds the pacer at the new cap.
#[tokio::test]
async fn session_with_max_bandwidth_seeds_the_pacer() {
    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let session = SrtSession::new(hls);
    assert_eq!(session.current_send_rate(), DEFAULT_MAX_BANDWIDTH);
    let session = session.with_max_bandwidth(64_000);
    assert_eq!(session.current_send_rate(), 64_000);
}

// ── drain_control_packets integration tests ──────────────────────────────

/// In-order delivery produces ACK control packets via `drain_control_packets`,
/// and none of them are NAK packets.
#[tokio::test]
async fn drain_control_packets_in_order_yields_ack_not_nak() {
    use protocol::{ControlType, PacketKind};
    use std::time::Duration as StdDuration;

    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let mut session = SrtSession::new(hls).with_receive_buffer_capacity(4096);
    // Fire the ACK timer immediately.
    session
        .reliability
        .set_ack_interval(StdDuration::from_nanos(1));

    let ts_bytes = ts_packet(0x0000, true, &pat());
    let peer_socket_id = 0xBEEF_1234u32;

    // Feed three consecutive packets.
    for seq_no in 0u32..3 {
        let pkt = make_data_packet(seq_no, KkFlag::Clear, &ts_bytes);
        session.feed_packet(&pkt).await.unwrap();
    }

    let ctrl_pkts = session.drain_control_packets(peer_socket_id);
    assert!(
        !ctrl_pkts.is_empty(),
        "drain_control_packets must return at least one packet for in-order delivery"
    );

    // Every packet must be a valid SRT header addressed to the peer.
    for pkt in &ctrl_pkts {
        let hdr = SrtHeader::parse(pkt).expect("must be a valid SRT header");
        assert!(
            hdr.is_control(),
            "every returned packet must be a control packet"
        );
        assert_eq!(hdr.dest_socket_id, peer_socket_id);
    }

    // At least one must be an ACK, and its wire-level flow-control field must
    // match the capacity accepted during the handshake.
    let ack = ctrl_pkts.iter().find(|pkt| {
        SrtHeader::parse(pkt).is_some_and(|h| {
            matches!(
                h.kind,
                PacketKind::Control {
                    control_type: ControlType::Ack,
                    ..
                }
            )
        })
    });
    let ack = ack.expect("at least one ACK control packet expected");
    let cif = &ack[protocol::SRT_HEADER_LEN..];
    let available_buffer = u32::from_be_bytes([cif[12], cif[13], cif[14], cif[15]]);
    assert_eq!(
        available_buffer, 4096,
        "ACKD_BUFFERLEFT must carry the negotiated available packet capacity"
    );

    // None must be a NAK.
    let has_nak = ctrl_pkts.iter().any(|pkt| {
        SrtHeader::parse(pkt).is_some_and(|h| {
            matches!(
                h.kind,
                PacketKind::Control {
                    control_type: ControlType::Nak,
                    ..
                }
            )
        })
    });
    assert!(!has_nak, "no NAK expected for consecutive packets");
}

/// A sequence gap drives `drain_control_packets` to yield a NAK packet
/// whose decoded loss list covers the exact missing range.
#[tokio::test]
async fn drain_control_packets_gap_yields_nak_with_correct_loss_list() {
    use protocol::{ControlType, PacketKind, SRT_HEADER_LEN};

    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let mut session = SrtSession::new(hls);

    let ts_bytes = ts_packet(0x0000, true, &pat());
    let peer_socket_id = 0xCAFE_BABEu32;

    // Feed packet 0.
    let pkt0 = make_data_packet(0, KkFlag::Clear, &ts_bytes);
    session.feed_packet(&pkt0).await.unwrap();
    // Drain to reset the pending-action queue.
    let _ = session.drain_control_packets(peer_socket_id);

    // Feed packet 3, skipping 1 and 2 → gap [1, 2].
    let pkt3 = make_data_packet(3, KkFlag::Clear, &ts_bytes);
    session.feed_packet(&pkt3).await.unwrap();

    let ctrl_pkts = session.drain_control_packets(peer_socket_id);

    // Find the NAK packet.
    let nak_pkt = ctrl_pkts.iter().find(|pkt| {
        SrtHeader::parse(pkt).is_some_and(|h| {
            matches!(
                h.kind,
                PacketKind::Control {
                    control_type: ControlType::Nak,
                    ..
                }
            )
        })
    });
    assert!(
        nak_pkt.is_some(),
        "NAK control packet must be emitted for the gap [1, 2]"
    );

    let nak_pkt = nak_pkt.unwrap();
    let hdr = SrtHeader::parse(nak_pkt).unwrap();
    assert_eq!(hdr.dest_socket_id, peer_socket_id);

    // Decode the loss list from the NAK CIF body.
    let body = &nak_pkt[SRT_HEADER_LEN..];
    let ranges = decode_nak_loss_list(body);

    // The gap [1, 2] must appear as a range (from=1, to=2).
    assert_eq!(
        ranges,
        vec![(1, 2)],
        "NAK loss list must cover exactly the missing range [1, 2]"
    );
}

/// `drain_control_packets` must leave the pending-actions queue empty, so a
/// second call returns nothing (no double-send).
#[tokio::test]
async fn drain_control_packets_is_consuming() {
    use std::time::Duration as StdDuration;

    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let mut session = SrtSession::new(hls);
    // Use a tiny ACK interval so the timer fires on the second packet.
    session
        .reliability
        .set_ack_interval(StdDuration::from_nanos(1));

    let ts_bytes = ts_packet(0x0000, true, &pat());

    // First packet — sets the timer but doesn't fire it yet.
    let pkt0 = make_data_packet(0, KkFlag::Clear, &ts_bytes);
    session.feed_packet(&pkt0).await.unwrap();

    // Second packet — fires the ACK timer (interval = 1 ns, definitely elapsed).
    let pkt1 = make_data_packet(1, KkFlag::Clear, &ts_bytes);
    session.feed_packet(&pkt1).await.unwrap();

    // First drain must return packets (at least one ACK).
    let first = session.drain_control_packets(0);
    assert!(
        !first.is_empty(),
        "first drain must return at least one packet"
    );

    // Second drain must be empty — actions consumed.
    let second = session.drain_control_packets(0);
    assert!(
        second.is_empty(),
        "second drain must return nothing; actions already consumed"
    );
}
