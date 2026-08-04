use super::*;

// ──────────── RFC 3394 AES Key Wrap test vector ─────────────
// From RFC 3394 §4.1: "Wrap 128 bits of Key Data with a 128-bit KEK"
//
//   KEK      = 000102030405060708090A0B0C0D0E0F
//   Key Data = 00112233445566778899AABBCCDDEEFF
//   Wrapped  = 1FA68B0A8112B447AEF34BD8FB5A7B829D3E862371D2CFE5

#[test]
fn aes_key_wrap_rfc3394_vector() {
    let kek: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
        0x0F,
    ];
    let key_data: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
        0xFF,
    ];
    let expected: [u8; 24] = [
        0x1F, 0xA6, 0x8B, 0x0A, 0x81, 0x12, 0xB4, 0x47, 0xAE, 0xF3, 0x4B, 0xD8, 0xFB, 0x5A, 0x7B,
        0x82, 0x9D, 0x3E, 0x86, 0x23, 0x71, 0xD2, 0xCF, 0xE5,
    ];
    let wrapped = aes_key_wrap(&kek, &key_data);
    assert_eq!(
        wrapped.as_slice(),
        expected.as_slice(),
        "RFC 3394 §4.1 wrap vector"
    );
}

#[test]
fn aes_key_unwrap_rfc3394_vector() {
    let kek: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
        0x0F,
    ];
    let wrapped: [u8; 24] = [
        0x1F, 0xA6, 0x8B, 0x0A, 0x81, 0x12, 0xB4, 0x47, 0xAE, 0xF3, 0x4B, 0xD8, 0xFB, 0x5A, 0x7B,
        0x82, 0x9D, 0x3E, 0x86, 0x23, 0x71, 0xD2, 0xCF, 0xE5,
    ];
    let expected: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
        0xFF,
    ];
    let unwrapped = aes_key_unwrap(&kek, &wrapped).expect("RFC 3394 unwrap must succeed");
    assert_eq!(
        unwrapped.as_slice(),
        expected.as_slice(),
        "RFC 3394 §4.1 unwrap vector"
    );
}

#[test]
fn aes_key_unwrap_bad_integrity_is_detected() {
    let kek = [0u8; 16];
    let mut wrapped = aes_key_wrap(&kek, &[0u8; 16]);
    // Corrupt one byte of the wrapped material.
    wrapped[5] ^= 0xFF;
    assert_eq!(
        aes_key_unwrap(&kek, &wrapped),
        Err(KeyUnwrapError::IntegrityCheckFailed)
    );
}

#[test]
fn aes_key_unwrap_bad_length_rejected() {
    let kek = [0u8; 16];
    // 7 bytes is too short (need at least 24).
    assert_eq!(
        aes_key_unwrap(&kek, &[0u8; 7]),
        Err(KeyUnwrapError::BadLength)
    );
    // 25 bytes is not a multiple of 8.
    assert_eq!(
        aes_key_unwrap(&kek, &[0u8; 25]),
        Err(KeyUnwrapError::BadLength)
    );
}

// ──────────── PBKDF2 KEK determinism ─────────────

#[test]
fn pbkdf2_kek_is_deterministic() {
    let pass = b"srt-test-passphrase";
    let salt = [0xABu8; 16];
    let k1 = pbkdf2_kek(pass, &salt);
    let k2 = pbkdf2_kek(pass, &salt);
    assert_eq!(k1, k2, "PBKDF2 must be deterministic");
}

#[test]
fn pbkdf2_kek_changes_with_passphrase_and_salt() {
    let k1 = pbkdf2_kek(b"pass1", &[0u8; 16]);
    let k2 = pbkdf2_kek(b"pass2", &[0u8; 16]);
    let mut salt = [0u8; 16];
    salt[15] = 1;
    let k3 = pbkdf2_kek(b"pass1", &salt);
    assert_ne!(k1, k2, "different passphrases produce different KEKs");
    assert_ne!(k1, k3, "different salts produce different KEKs");
}

#[test]
fn pbkdf2_uses_only_least_significant_64_salt_bits() {
    let mut salt_a = [0u8; 16];
    salt_a[8..].copy_from_slice(&[8, 9, 10, 11, 12, 13, 14, 15]);
    let mut salt_b = [0xFFu8; 16];
    salt_b[8..].copy_from_slice(&salt_a[8..]);
    assert_eq!(
        pbkdf2_kek(b"standard-passphrase", &salt_a),
        pbkdf2_kek(b"standard-passphrase", &salt_b)
    );
    assert_eq!(
        pbkdf2_kek(b"standard-passphrase", &salt_a),
        [
            0xE2, 0x35, 0x51, 0x26, 0x35, 0x53, 0x3A, 0xED, 0x81, 0x05, 0x81, 0x17, 0x3D, 0x6B,
            0x24, 0x1E,
        ],
        "PBKDF2-HMAC-SHA1 fixture independently generated with OpenSSL"
    );
}

// ──────────── AES-CTR encrypt → decrypt round-trip ─────────────

#[test]
fn encrypt_decrypt_roundtrip_recovers_plaintext() {
    let sek = [0x42u8; 16];
    let salt = [0x55u8; 16];
    let crypto = SrtCrypto::from_raw_sek(b"passphrase", &salt, sek);
    let plaintext = b"Hello, SRT world! This is 32byte";
    let mut data = plaintext.to_vec();
    crypto.encrypt_packet(42, &mut data);
    assert_ne!(
        data.as_slice(),
        plaintext.as_slice(),
        "ciphertext differs from plaintext"
    );
    crypto.decrypt_packet(42, &mut data);
    assert_eq!(
        data.as_slice(),
        plaintext.as_slice(),
        "decrypt recovers plaintext"
    );
}

#[test]
fn encrypt_different_seq_nos_produce_different_ciphertext() {
    let sek = [0x11u8; 16];
    let salt = [0x22u8; 16];
    let crypto = SrtCrypto::from_raw_sek(b"pass", &salt, sek);
    let mut d1 = b"test payload data".to_vec();
    let mut d2 = d1.clone();
    crypto.encrypt_packet(1, &mut d1);
    crypto.encrypt_packet(2, &mut d2);
    assert_ne!(d1, d2, "different seq nos → different ciphertext");
}

#[test]
fn ctr_iv_matches_haivision_layout() {
    let salt = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
        0x0F,
    ];
    let crypto = SrtCrypto::from_raw_sek(b"passphrase", &salt, [0u8; 16]);
    assert_eq!(
        crypto.build_ctr_iv(0x1020_3040),
        [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x1A, 0x2B, 0x3C, 0x4D,
            0x00, 0x00,
        ]
    );
}

#[test]
fn ctr_is_self_inverse() {
    // Encrypt twice should recover the original (CTR is self-inverse).
    let sek = [0xCCu8; 16];
    let salt = [0xDDu8; 16];
    let crypto = SrtCrypto::from_raw_sek(b"x", &salt, sek);
    let original = b"data that wraps around 16 bytes!!".to_vec();
    let mut buf = original.clone();
    crypto.encrypt_packet(100, &mut buf);
    crypto.encrypt_packet(100, &mut buf);
    assert_eq!(buf, original, "double CTR encryption is identity");
}

#[test]
fn encrypt_arbitrary_length_payload() {
    let sek = [0x01u8; 16];
    let salt = [0x02u8; 16];
    let crypto = SrtCrypto::from_raw_sek(b"p", &salt, sek);
    for len in [0usize, 1, 15, 16, 17, 32, 188] {
        let original: Vec<u8> = (0..len).map(|i| u8::try_from(i % 256).unwrap()).collect();
        let mut buf = original.clone();
        crypto.encrypt_packet(0, &mut buf);
        crypto.decrypt_packet(0, &mut buf);
        assert_eq!(buf, original, "round-trip for length {len}");
    }
}

// ──────────── KM message encode → decode round-trip ─────────────

#[test]
fn km_message_encode_decode_roundtrip() {
    let sek = [0x77u8; 16];
    let salt = [0x88u8; 16];
    let crypto = SrtCrypto::from_raw_sek(b"my-passphrase", &salt, sek);
    let km = crypto.build_km_message(b"my-passphrase");
    let encoded = km.encode();
    assert_eq!(
        encoded.len(),
        KM_MIN_LEN,
        "encoded KM is {KM_MIN_LEN} bytes"
    );
    let decoded = KmMessage::decode(&encoded).expect("decode must succeed on valid bytes");
    assert_eq!(decoded.salt, km.salt, "salt round-trips");
    assert_eq!(
        decoded.wrapped_sek, km.wrapped_sek,
        "wrapped_sek round-trips"
    );
    assert_eq!(decoded.keki, km.keki, "keki round-trips");
    assert_eq!(
        &encoded[..16],
        &[0x12, 0x20, 0x29, 0x01, 0, 0, 0, 0, 2, 0, 2, 0, 0, 0, 4, 4],
        "KM fixed header matches the SRT KM message layout"
    );
}

#[test]
fn km_message_decode_rejects_short_buffer() {
    assert!(KmMessage::decode(&[0u8; KM_MIN_LEN - 1]).is_none());
    assert!(KmMessage::decode(&[]).is_none());
}

#[test]
fn km_message_decode_rejects_bad_signature() {
    let sek = [0u8; 16];
    let salt = [0u8; 16];
    let crypto = SrtCrypto::from_raw_sek(b"p", &salt, sek);
    let km = crypto.build_km_message(b"p");
    let mut encoded = km.encode();
    encoded[1] ^= 0xFF; // corrupt the signature byte
    assert!(KmMessage::decode(&encoded).is_none());
}

// ──────────── Full passphrase → KM → unwrap SEK round-trip ─────────────

#[test]
fn from_km_message_recovers_sek() {
    let passphrase = b"secret-passphrase";
    let salt = [0xABu8; 16];
    let sek = [0x5Au8; 16];

    // Sender side: build KM message.
    let sender = SrtCrypto::from_passphrase(passphrase, &salt, sek);
    let km = sender.build_km_message(passphrase);

    // Receiver side: reconstruct crypto from the KM message.
    let receiver =
        SrtCrypto::from_km_message(&km, passphrase).expect("receiver must unwrap the SEK");
    assert_eq!(
        receiver.sek(),
        sender.sek(),
        "receiver recovered the same SEK"
    );
}

#[test]
fn from_km_message_fails_with_wrong_passphrase() {
    let salt = [0u8; 16];
    let sek = [1u8; 16];
    let sender = SrtCrypto::from_passphrase(b"correct", &salt, sek);
    let km = sender.build_km_message(b"correct");
    // Using the wrong passphrase should fail the integrity check.
    let result = SrtCrypto::from_km_message(&km, b"wrong");
    assert!(result.is_err(), "wrong passphrase must fail unwrap");
}

// ──────────── KK flag bit manipulation ─────────────

#[test]
fn kk_flag_round_trips_in_msg_word() {
    for kk in [KkFlag::Clear, KkFlag::EvenKey, KkFlag::OddKey] {
        let word = kk.set_in_msg_word(0xDEAD_BEEF);
        let back = KkFlag::from_msg_word(word);
        assert_eq!(back, kk, "KkFlag {kk:?} must round-trip in msg_word");
    }
}

#[test]
fn kk_flag_does_not_disturb_other_bits() {
    let original: u32 = 0x0000_0000;
    let with_even = KkFlag::EvenKey.set_in_msg_word(original);
    // Bits 28–27 should be 01; all others stay 0.
    assert_eq!(with_even, 0x0800_0000);
    assert_eq!(with_even & !(0x03 << 27), 0, "no other bits modified");
}
