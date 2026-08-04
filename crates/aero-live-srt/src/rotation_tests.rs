use super::*;
use bytes::BytesMut;

const PASSPHRASE: &[u8] = b"deterministic-rotation-secret";
const SALT: [u8; 16] = [0x31; 16];

fn crypto(byte: u8) -> SrtCrypto {
    SrtCrypto::from_raw_sek(PASSPHRASE, &SALT, [byte; 16])
}

fn data_packet(seq_no: u32, flag: KkFlag, key: &SrtCrypto) -> Vec<u8> {
    // A syntactically complete null TS packet keeps these tests focused on
    // crypto/reliability state rather than PAT/PMT assembly.
    let mut payload = vec![0xff; TS_PACKET_SIZE];
    payload[..4].copy_from_slice(&[TS_SYNC_BYTE, 0x1f, 0xff, 0x10]);
    key.encrypt_packet(seq_no, &mut payload);
    let mut packet = BytesMut::new();
    SrtHeader {
        kind: PacketKind::Data {
            seq_no,
            msg_word: flag.set_in_msg_word(0),
        },
        timestamp: 0,
        dest_socket_id: 1,
    }
    .write_to(&mut packet);
    packet.extend_from_slice(&payload);
    packet.to_vec()
}

async fn session(even: SrtCrypto) -> (tempfile::TempDir, SrtSession) {
    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let mut session = SrtSession::with_crypto(hls, Some(even));
    session.enable_key_rotation(PASSPHRASE.to_vec());
    (dir, session)
}

#[test]
fn km_codec_models_odd_and_dual_key_material() {
    let even = crypto(0x11);
    let odd = crypto(0x22);

    let odd_message = odd
        .build_km_message_for(PASSPHRASE, KkFlag::OddKey)
        .unwrap();
    assert_eq!(odd_message.key_flags, KmKeyFlags::Odd);
    assert_eq!(odd_message.encode().len(), 56);
    let decoded_odd = KmMessage::decode(&odd_message.encode()).unwrap();
    assert_eq!(decoded_odd, odd_message);
    assert_eq!(
        SrtCrypto::from_km_message(&decoded_odd, PASSPHRASE)
            .unwrap()
            .sek(),
        odd.sek()
    );

    let dual = SrtCrypto::build_dual_km_message(&even, &odd, PASSPHRASE).unwrap();
    assert_eq!(dual.key_flags, KmKeyFlags::EvenAndOdd);
    assert_eq!(dual.wrapped_sek.len(), 40);
    assert_eq!(dual.encode().len(), 72);
    assert_eq!(KmMessage::decode(&dual.encode()).unwrap(), dual);
    assert!(
        SrtCrypto::from_km_message(&dual, PASSPHRASE).is_err(),
        "single-key compatibility constructor must not choose one key from KK=11"
    );
}

#[test]
fn established_km_control_uses_user_defined_subtypes_and_echoes_exactly() {
    let request =
        SrtCrypto::build_dual_km_message(&crypto(0x11), &crypto(0x22), PASSPHRASE).unwrap();
    let wire = protocol::encode_key_material_control(&request, 0xCAFE_BABE, 0x0102_0304);
    assert_eq!(
        &wire[..4],
        &0xffff_0003_u32.to_be_bytes(),
        "UserDefined 0x7fff + KMREQ subtype 3"
    );
    let header = SrtHeader::parse(&wire).unwrap();
    assert_eq!(
        header.kind,
        PacketKind::Control {
            control_type: ControlType::UserDefined,
            subtype: protocol::SRT_CMD_KMREQ,
            type_specific: 0,
        }
    );
    assert_eq!(header.timestamp, 0x0102_0304);
    assert_eq!(header.dest_socket_id, 0xCAFE_BABE);
    assert_eq!(
        protocol::decode_key_material_control(&wire).unwrap(),
        Some(request.clone())
    );

    let response = request.as_response();
    let response_wire = protocol::encode_key_material_control(&response, 7, 8);
    assert_eq!(&response_wire[..4], &0xffff_0004_u32.to_be_bytes());
    assert_eq!(
        protocol::decode_key_material_control(&response_wire).unwrap(),
        Some(response)
    );
}

#[tokio::test]
async fn rotation_switches_slots_and_rejects_stale_or_replayed_data() {
    let even_0 = crypto(0x10);
    let odd_1 = crypto(0x21);
    let (_dir, mut receiver) = session(even_0.clone()).await;

    receiver
        .feed_packet(&data_packet(0, KkFlag::EvenKey, &even_0))
        .await
        .unwrap();
    let first = SrtCrypto::build_dual_km_message(&even_0, &odd_1, PASSPHRASE).unwrap();
    let request_wire = protocol::encode_key_material_control(&first, 1, 2);
    let expected_response = protocol::encode_key_material_control(&first.as_response(), 77, 99);

    let response = receiver
        .handle_key_material_control(&request_wire, 77, 99)
        .unwrap()
        .unwrap();
    assert_eq!(response, expected_response);
    // Lost KMRSP is recovered by an idempotent duplicate KMREQ.
    assert_eq!(
        receiver
            .handle_key_material_control(&request_wire, 77, 99)
            .unwrap()
            .unwrap(),
        expected_response
    );

    receiver
        .feed_packet(&data_packet(1, KkFlag::OddKey, &odd_1))
        .await
        .unwrap();
    // A late packet from before the observed switch remains decryptable.
    receiver
        .feed_packet(&data_packet(0, KkFlag::EvenKey, &even_0))
        .await
        .unwrap();
    assert!(
        receiver
            .feed_packet(&data_packet(2, KkFlag::EvenKey, &even_0))
            .await
            .is_err(),
        "old slot cannot decrypt a sequence after the switch boundary"
    );
    assert!(
        receiver
            .handle_key_material_control(&request_wire, 77, 99)
            .is_err(),
        "a completed rotation cannot be replayed as the next rotation"
    );

    let even_2 = crypto(0x32);
    let second = SrtCrypto::build_dual_km_message(&even_2, &odd_1, PASSPHRASE).unwrap();
    let second_wire = protocol::encode_key_material_control(&second, 1, 3);
    receiver
        .handle_key_material_control(&second_wire, 77, 100)
        .unwrap()
        .unwrap();
    receiver
        .feed_packet(&data_packet(2, KkFlag::EvenKey, &even_2))
        .await
        .unwrap();
    receiver
        .feed_packet(&data_packet(1, KkFlag::OddKey, &odd_1))
        .await
        .unwrap();
    assert!(receiver
        .feed_packet(&data_packet(3, KkFlag::OddKey, &odd_1))
        .await
        .is_err());
}

#[tokio::test]
async fn missing_or_invalid_kmreq_never_arms_the_new_slot() {
    let even = crypto(0x41);
    let odd = crypto(0x52);
    let (_dir, mut receiver) = session(even.clone()).await;
    let odd_packet = data_packet(0, KkFlag::OddKey, &odd);

    assert!(receiver.feed_packet(&odd_packet).await.is_err());
    assert!(
        receiver.drain_actions().is_empty(),
        "rejected ciphertext must not poison reliability state"
    );

    let wrong_secret = b"wrong-rotation-secret";
    let bad_request = SrtCrypto::build_dual_km_message(&even, &odd, wrong_secret).unwrap();
    let bad_wire = protocol::encode_key_material_control(&bad_request, 1, 0);
    assert!(receiver
        .handle_key_material_control(&bad_wire, 1, 0)
        .is_err());
    assert!(receiver.feed_packet(&odd_packet).await.is_err());

    let good_request = SrtCrypto::build_dual_km_message(&even, &odd, PASSPHRASE).unwrap();
    let good_wire = protocol::encode_key_material_control(&good_request, 1, 0);
    receiver
        .handle_key_material_control(&good_wire, 1, 0)
        .unwrap()
        .unwrap();

    let conflicting_odd = crypto(0x63);
    let conflicting =
        SrtCrypto::build_dual_km_message(&even, &conflicting_odd, PASSPHRASE).unwrap();
    assert!(
        receiver
            .handle_key_material_control(
                &protocol::encode_key_material_control(&conflicting, 1, 0),
                1,
                0,
            )
            .is_err(),
        "a different KMREQ cannot replace a pending acknowledged slot"
    );
    receiver.feed_packet(&odd_packet).await.unwrap();
}

#[tokio::test]
async fn malformed_km_control_and_unsolicited_response_fail_closed() {
    let even = crypto(0x71);
    let odd = crypto(0x82);
    let (_dir, mut receiver) = session(even.clone()).await;
    let request = SrtCrypto::build_dual_km_message(&even, &odd, PASSPHRASE).unwrap();

    let mut truncated = protocol::encode_key_material_control(&request, 1, 0).to_vec();
    truncated.pop();
    assert!(receiver
        .handle_key_material_control(&truncated, 1, 0)
        .is_err());

    let unsolicited = protocol::encode_key_material_control(&request.as_response(), 1, 0);
    assert!(receiver
        .handle_key_material_control(&unsolicited, 1, 0)
        .is_err());
    assert!(receiver
        .feed_packet(&data_packet(0, KkFlag::OddKey, &odd))
        .await
        .is_err());
}
