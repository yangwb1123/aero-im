//! Tests for the SRT protocol module, split out of protocol.rs.

    use super::*;
    use std::net::SocketAddr;

    fn peer() -> SocketAddr {
        "203.0.113.7:9000".parse().unwrap()
    }

    // ----------------------- SRT header round-trips -----------------------

    #[test]
    fn control_header_round_trips() {
        let h = SrtHeader {
            kind: PacketKind::Control {
                control_type: ControlType::Handshake,
                subtype: 0,
                type_specific: 0xDEAD_BEEF,
            },
            timestamp: 0x0102_0304,
            dest_socket_id: 0xCAFE_BABE,
        };
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), SRT_HEADER_LEN);
        // Control flag is the top bit of the first byte.
        assert_eq!(bytes[0] & 0x80, 0x80, "control bit set");
        let back = SrtHeader::parse(&bytes).unwrap();
        assert_eq!(back, h);
        assert!(back.is_control());
    }

    #[test]
    fn data_header_round_trips_and_masks_seq() {
        let h = SrtHeader {
            kind: PacketKind::Data {
                seq_no: 0x1234_5678,
                msg_word: 0xAABB_CCDD,
            },
            timestamp: 42,
            dest_socket_id: 7,
        };
        let bytes = h.to_bytes();
        assert_eq!(bytes[0] & 0x80, 0x00, "data bit clear");
        let back = SrtHeader::parse(&bytes).unwrap();
        assert_eq!(back, h);
        assert!(!back.is_control());
    }

    #[test]
    fn data_seq_high_bit_never_collides_with_control_flag() {
        // A sequence number with bit 31 set must still serialize as a *data*
        // packet (we mask to 31 bits), not be misread as control.
        let h = SrtHeader {
            kind: PacketKind::Data {
                seq_no: 0x7FFF_FFFF,
                msg_word: 0,
            },
            timestamp: 0,
            dest_socket_id: 0,
        };
        let bytes = h.to_bytes();
        assert_eq!(bytes[0] & 0x80, 0x00);
        let back = SrtHeader::parse(&bytes).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn header_parse_rejects_short_buffer() {
        assert!(SrtHeader::parse(&[0u8; 15]).is_none());
        assert!(SrtHeader::parse(&[]).is_none());
    }

    #[test]
    fn control_type_codes_round_trip() {
        for ct in [
            ControlType::Handshake,
            ControlType::KeepAlive,
            ControlType::Ack,
            ControlType::Nak,
            ControlType::Shutdown,
            ControlType::AckAck,
            ControlType::Other(0x1234),
        ] {
            assert_eq!(ControlType::from_code(ct.code()), ct);
        }
    }

    #[test]
    fn handshake_type_codes_round_trip() {
        for ht in [
            HandshakeType::Waveahand,
            HandshakeType::Induction,
            HandshakeType::Conclusion,
            HandshakeType::Agreement,
            HandshakeType::Done,
            HandshakeType::Other(1234),
        ] {
            assert_eq!(HandshakeType::from_code(ht.code()), ht);
        }
        // Spot-check the exact wire codes.
        assert_eq!(HandshakeType::Induction.code(), 1);
        assert_eq!(HandshakeType::Conclusion.code(), 0xFFFF_FFFF);
        assert_eq!(HandshakeType::Agreement.code(), 0xFFFF_FFFE);
        assert_eq!(HandshakeType::Waveahand.code(), 0);
    }

    // --------------------------- StreamID swizzle ---------------------------

    #[test]
    fn stream_id_encodes_with_per_word_byte_reversal() {
        // The canonical SRT-doc example: "STREAM" → pad to "STREAM\0\0" → reverse
        // each 4-byte block → "ERTS\0\0MA".
        let encoded = encode_stream_id("STREAM");
        assert_eq!(encoded, b"ERTS\0\0MA");
    }

    #[test]
    fn stream_id_four_byte_block_is_exactly_reversed() {
        // "test" = [74 65 73 74] → reversed [74 73 65 74].
        let encoded = encode_stream_id("test");
        assert_eq!(encoded, vec![0x74, 0x73, 0x65, 0x74]);
        assert_eq!(decode_stream_id(&encoded), "test");
    }

    #[test]
    fn stream_id_round_trips_for_various_lengths() {
        for s in [
            "",
            "a",
            "ab",
            "abc",
            "abcd",
            "abcde",
            "publisher/main",
            "#!::r=live/abc,m=publish",
        ] {
            let encoded = encode_stream_id(s);
            assert_eq!(encoded.len() % 4, 0, "padded to whole words for {s:?}");
            assert_eq!(decode_stream_id(&encoded), s, "round-trip for {s:?}");
        }
    }

    #[test]
    fn stream_id_decode_strips_trailing_nuls_only() {
        // Encode "abc" → one word, reversed: [00 'c' 'b' 'a'] = [00,63,62,61].
        let encoded = encode_stream_id("abc");
        assert_eq!(encoded, vec![0x00, 0x63, 0x62, 0x61]);
        assert_eq!(decode_stream_id(&encoded), "abc");
    }

    // ----------------------- Handshake CIF round-trips -----------------------

    #[test]
    fn handshake_cif_round_trips_without_extensions() {
        let hs = Handshake {
            version: SRT_VERSION_HSV5,
            encryption_field: HS_ENC_CLEAR,
            extension_field: SRT_MAGIC_CODE,
            initial_seq_no: 0x0011_2233,
            mtu: 1500,
            flow_window: 8192,
            handshake_type: HandshakeType::Induction,
            srt_socket_id: 0xABCD_1234,
            syn_cookie: 0x5566_7788,
            peer_ip: {
                let mut ip = [0u8; 16];
                ip[..4].copy_from_slice(&[203, 0, 113, 7]);
                ip
            },
            extensions: Vec::new(),
        };
        let bytes = hs.to_bytes();
        assert_eq!(bytes.len(), HANDSHAKE_CIF_LEN, "no-extension CIF is 48 bytes");
        let back = Handshake::parse(&bytes).unwrap();
        assert_eq!(back, hs);
        // The magic survived in the extension field (it isn't an ext block).
        assert_eq!(back.extension_field, SRT_MAGIC_CODE);
    }

    #[test]
    fn handshake_with_sid_and_hsreq_round_trips() {
        let hs = Handshake {
            handshake_type: HandshakeType::Conclusion,
            srt_socket_id: 99,
            syn_cookie: 0x1357_9BDF,
            extensions: vec![
                HsExtension::HsReq {
                    is_response: false,
                    srt_version: 0x0001_0500,
                    srt_flags: 0x0000_00BF,
                    recv_tsbpd_delay: 120,
                    send_tsbpd_delay: 0,
                },
                HsExtension::StreamId("publisher/main".to_string()),
            ],
            ..Default::default()
        };
        let bytes = hs.to_bytes();
        let back = Handshake::parse(&bytes).unwrap();
        assert_eq!(back.stream_id(), Some("publisher/main"));
        assert_eq!(back.extensions, hs.extensions);
        // Extension flags must reflect both blocks.
        assert_eq!(
            back.extension_field & (HS_EXT_HSREQ | HS_EXT_CONFIG),
            HS_EXT_HSREQ | HS_EXT_CONFIG
        );
    }

    #[test]
    fn handshake_parse_rejects_truncated_cif() {
        assert!(Handshake::parse(&[0u8; 47]).is_none());
    }

    #[test]
    fn handshake_tolerates_truncated_trailing_extension() {
        // Build a valid CIF then append a bogus extension header claiming more
        // content than is present; parsing must keep the CIF and drop the ext.
        let mut bytes = BytesMut::from(&Handshake::default().to_bytes()[..]);
        bytes.put_u16(SRT_CMD_SID);
        bytes.put_u16(4); // claims 16 bytes of content...
        bytes.put_slice(b"xx"); // ...but only 2 are present
        let back = Handshake::parse(&bytes).unwrap();
        assert!(back.extensions.is_empty(), "garbled extension dropped");
    }

    #[test]
    fn unknown_extension_round_trips_verbatim() {
        let hs = Handshake {
            handshake_type: HandshakeType::Conclusion,
            extensions: vec![HsExtension::Other {
                ext_type: 0x00FF,
                contents: Bytes::from_static(&[1, 2, 3, 4, 5, 6, 7, 8]),
            }],
            ..Default::default()
        };
        let back = Handshake::parse(&hs.to_bytes()).unwrap();
        assert_eq!(back.extensions, hs.extensions);
    }

    // ----------------------------- SYN cookie -----------------------------

    #[test]
    fn cookie_is_deterministic_per_peer_and_minute() {
        let m = HandshakeMachine::new(1, 0xABCD);
        let c1 = m.make_cookie(peer(), 1_700_000_000);
        let c2 = m.make_cookie(peer(), 1_700_000_000 + 30); // same minute
        assert_eq!(c1, c2, "cookie stable within a minute");
        let c3 = m.make_cookie(peer(), 1_700_000_000 + 120); // +2 minutes
        assert_ne!(c1, c3, "cookie changes across minutes");
    }

    #[test]
    fn cookie_differs_by_peer_and_seed() {
        let m = HandshakeMachine::new(1, 0xABCD);
        let other: SocketAddr = "198.51.100.9:9000".parse().unwrap();
        assert_ne!(m.make_cookie(peer(), 100), m.make_cookie(other, 100));

        let m2 = HandshakeMachine::new(1, 0x9999);
        assert_ne!(m.make_cookie(peer(), 100), m2.make_cookie(peer(), 100));
    }

    #[test]
    fn cookie_validates_within_and_across_minute_boundary() {
        let m = HandshakeMachine::new(1, 0xABCD);
        let now = 1_700_000_000;
        let c = m.make_cookie(peer(), now);
        assert!(m.validate_cookie(c, peer(), now));
        // A cookie minted last minute still validates this minute (rollover).
        assert!(m.validate_cookie(c, peer(), now + 60));
        // Two minutes later it no longer validates.
        assert!(!m.validate_cookie(c, peer(), now + 120));
        // A wrong cookie never validates.
        assert!(!m.validate_cookie(c.wrapping_add(1), peer(), now));
    }

    // ----------------------- Handshake state machine -----------------------

    /// Build a caller INDUCTION packet (version 4, cookie 0).
    fn caller_induction(caller_socket: u32) -> Bytes {
        let hs = Handshake {
            version: SRT_VERSION_UDT4,
            encryption_field: HS_ENC_CLEAR,
            extension_field: 2, // caller sets ext-field=2 in induction
            handshake_type: HandshakeType::Induction,
            srt_socket_id: caller_socket,
            syn_cookie: 0,
            ..Default::default()
        };
        let header = SrtHeader {
            kind: PacketKind::Control {
                control_type: ControlType::Handshake,
                subtype: 0,
                type_specific: 0,
            },
            timestamp: 0,
            dest_socket_id: 0,
        };
        let mut out = BytesMut::new();
        header.write_to(&mut out);
        hs.write_to(&mut out);
        out.freeze()
    }

    /// Build a caller CONCLUSION echoing `cookie` and carrying `sid`.
    fn caller_conclusion(caller_socket: u32, cookie: u32, sid: &str) -> Bytes {
        let hs = Handshake {
            version: SRT_VERSION_HSV5,
            handshake_type: HandshakeType::Conclusion,
            srt_socket_id: caller_socket,
            syn_cookie: cookie,
            extensions: vec![
                HsExtension::HsReq {
                    is_response: false,
                    srt_version: 0x0001_0500,
                    srt_flags: 0x00BF,
                    recv_tsbpd_delay: 120,
                    send_tsbpd_delay: 0,
                },
                HsExtension::StreamId(sid.to_string()),
            ],
            ..Default::default()
        };
        let header = SrtHeader {
            kind: PacketKind::Control {
                control_type: ControlType::Handshake,
                subtype: 0,
                type_specific: 0,
            },
            timestamp: 0,
            dest_socket_id: 7,
        };
        let mut out = BytesMut::new();
        header.write_to(&mut out);
        hs.write_to(&mut out);
        out.freeze()
    }

    #[test]
    fn full_handshake_drives_to_established_and_extracts_sid() {
        let mut m = HandshakeMachine::new(0xABCD_0001, 0xFEED);
        let now = 1_700_000_000;
        assert_eq!(m.state(), HsState::Init);

        // 1) Caller INDUCTION → listener INDUCTION response.
        let action = m
            .handle_packet(&caller_induction(0x1111_2222), peer(), now)
            .unwrap();
        let HsAction::Reply(resp) = action else {
            panic!("expected an INDUCTION reply, got {action:?}");
        };
        assert_eq!(m.state(), HsState::InductionSent);
        assert_eq!(m.peer_socket_id(), 0x1111_2222);

        // The reply must be a version-5 INDUCTION carrying the SRT magic and a
        // cookie, addressed back to the caller's socket id.
        let resp_hdr = SrtHeader::parse(&resp).unwrap();
        assert_eq!(resp_hdr.dest_socket_id, 0x1111_2222);
        let resp_hs = Handshake::parse(&resp[SRT_HEADER_LEN..]).unwrap();
        assert_eq!(resp_hs.version, SRT_VERSION_HSV5);
        assert_eq!(resp_hs.handshake_type, HandshakeType::Induction);
        assert_eq!(resp_hs.extension_field, SRT_MAGIC_CODE);
        assert_eq!(resp_hs.srt_socket_id, 0xABCD_0001);
        let cookie = resp_hs.syn_cookie;
        assert_ne!(cookie, 0, "listener minted a non-zero cookie");

        // 2) Caller CONCLUSION (echoing the cookie) → Established + SID.
        let action = m
            .handle_packet(
                &caller_conclusion(0x1111_2222, cookie, "publisher/main"),
                peer(),
                now,
            )
            .unwrap();
        let HsAction::Established {
            stream_id,
            agreement,
        } = action
        else {
            panic!("expected Established, got {action:?}");
        };
        assert_eq!(m.state(), HsState::Done);
        assert_eq!(stream_id, "publisher/main");

        // The agreement is a CONCLUSION (HSRSP) addressed back to the caller.
        let agr_hdr = SrtHeader::parse(&agreement).unwrap();
        assert_eq!(agr_hdr.dest_socket_id, 0x1111_2222);
        let agr_hs = Handshake::parse(&agreement[SRT_HEADER_LEN..]).unwrap();
        assert_eq!(agr_hs.handshake_type, HandshakeType::Conclusion);
        assert!(
            agr_hs
                .extensions
                .iter()
                .any(|e| matches!(e, HsExtension::HsReq { is_response: true, .. })),
            "conclusion response carries an HSRSP block"
        );
    }

    #[test]
    fn conclusion_with_bad_cookie_is_rejected() {
        let mut m = HandshakeMachine::new(1, 0xFEED);
        let now = 1_700_000_000;
        m.handle_packet(&caller_induction(5), peer(), now).unwrap();
        let err = m
            .handle_packet(&caller_conclusion(5, 0xDEAD_BEEF, "x"), peer(), now)
            .unwrap_err();
        assert_eq!(err, HandshakeError::BadCookie);
        // State must not advance on a bad cookie.
        assert_eq!(m.state(), HsState::InductionSent);
    }

    #[test]
    fn conclusion_without_streamid_is_rejected() {
        let mut m = HandshakeMachine::new(1, 0xFEED);
        let now = 1_700_000_000;
        m.handle_packet(&caller_induction(5), peer(), now).unwrap();
        let cookie = m.make_cookie(peer(), now);
        // CONCLUSION with a valid cookie but no SID extension.
        let hs = Handshake {
            handshake_type: HandshakeType::Conclusion,
            srt_socket_id: 5,
            syn_cookie: cookie,
            extensions: vec![HsExtension::HsReq {
                is_response: false,
                srt_version: 0x0001_0500,
                srt_flags: 0,
                recv_tsbpd_delay: 0,
                send_tsbpd_delay: 0,
            }],
            ..Default::default()
        };
        let header = SrtHeader {
            kind: PacketKind::Control {
                control_type: ControlType::Handshake,
                subtype: 0,
                type_specific: 0,
            },
            timestamp: 0,
            dest_socket_id: 7,
        };
        let mut pkt = BytesMut::new();
        header.write_to(&mut pkt);
        hs.write_to(&mut pkt);
        let err = m.handle_packet(&pkt, peer(), now).unwrap_err();
        assert_eq!(err, HandshakeError::MissingStreamId);
    }

    #[test]
    fn data_and_non_handshake_control_packets_are_ignored_by_machine() {
        let mut m = HandshakeMachine::new(1, 0xFEED);
        // A data packet.
        let data_hdr = SrtHeader {
            kind: PacketKind::Data {
                seq_no: 1,
                msg_word: 0,
            },
            timestamp: 0,
            dest_socket_id: 1,
        };
        let mut data = BytesMut::new();
        data_hdr.write_to(&mut data);
        data.put_slice(&[0u8; 188]);
        assert_eq!(
            m.handle_packet(&data, peer(), 0).unwrap(),
            HsAction::Ignore
        );

        // A keep-alive control packet (no CIF).
        let ka = SrtHeader {
            kind: PacketKind::Control {
                control_type: ControlType::KeepAlive,
                subtype: 0,
                type_specific: 0,
            },
            timestamp: 0,
            dest_socket_id: 1,
        };
        assert_eq!(
            m.handle_packet(&ka.to_bytes(), peer(), 0).unwrap(),
            HsAction::Ignore
        );
    }

    #[test]
    fn too_short_datagram_is_not_a_handshake() {
        let mut m = HandshakeMachine::new(1, 0xFEED);
        let err = m.handle_packet(&[0u8; 4], peer(), 0).unwrap_err();
        assert_eq!(err, HandshakeError::NotHandshake);
    }
