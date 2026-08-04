//! SRT AES encryption (per the SRT/Haivision spec).
//!
//! ## What is modelled
//!
//! - **KMREQ/KMRSP Keying-Material (KM) message codec** — encodes and decodes
//!   the KM extension that carries the wrapped Stream Encrypting Key (SEK).
//! - **KEK derivation** — the Key Encrypting Key is derived from a passphrase
//!   via PBKDF2-HMAC-SHA1 using the KM salt (2048 rounds, 16 bytes of output).
//! - **AES Key Wrap (RFC 3394)** — wrap/unwrap the SEK with the KEK.  We
//!   implement the algorithm directly (no extra dep) against the published test
//!   vectors.
//! - **AES-CTR data-plane encryption/decryption** — cipher-block counter is
//!   built from the packet sequence number and the per-message IV carried in the
//!   KM message, honouring the KK key-flag bits (`00`=clear, `01`=even, `10`=odd)
//!   in the SRT data-packet header word.
//!
//! ## KK bit convention
//!
//! Bits 28–27 (0-indexed from LSB) of the SRT data-header word-1 carry the `KK`
//! field.  This crate exposes [`KkFlag`] and `SrtCrypto` honours it:
//! - `KkFlag::Clear` (`00`) → packet is unencrypted; `encrypt`/`decrypt` are
//!   no-ops.
//! - `KkFlag::EvenKey` (`01`) → encrypt/decrypt with the even SEK.
//! - `KkFlag::OddKey` (`10`) → encrypt/decrypt with the odd SEK.
//!
//! [`SrtKeyRotation`] keeps the even/odd receive slots and only exposes a newly
//! announced slot after a valid post-handshake KMREQ has been unwrapped.

// Lots of SRT-spec names (KK, KMREQ, SEK, PBKDF2, …) that are short and
// domain-standard; suppressing the doc_markdown lint keeps code readable.
#![allow(clippy::doc_markdown)]

use std::fmt;

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;

mod rotation;

pub(crate) use rotation::SrtKeyRotation;

// ─────────────────────────────────────────────────────────────────────────────
// KK flag
// ─────────────────────────────────────────────────────────────────────────────

/// The `KK` (key-keying) field in bits 28–27 of the SRT data-header word-1.
///
/// Tells the receiver which of the two possible SEKs was used to encrypt this
/// packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KkFlag {
    /// Packet is unencrypted (`00`).
    Clear = 0b00,
    /// Encrypted with the even SEK (`01`).
    EvenKey = 0b01,
    /// Encrypted with the odd SEK (`10`).
    OddKey = 0b10,
    /// Reserved/invalid on an SRT data packet (`11`).
    Invalid = 0b11,
}

impl KkFlag {
    /// Extract the KK flag from SRT data-header word-1.
    ///
    /// `msg_word` is the second 32-bit word of the SRT header as stored in
    /// [`crate::protocol::PacketKind::Data::msg_word`].
    #[must_use]
    pub fn from_msg_word(msg_word: u32) -> Self {
        match (msg_word >> 27) & 0x03 {
            0b01 => KkFlag::EvenKey,
            0b10 => KkFlag::OddKey,
            0b11 => KkFlag::Invalid,
            _ => KkFlag::Clear,
        }
    }

    /// Set the KK bits in `msg_word`, returning the updated word.
    #[must_use]
    pub fn set_in_msg_word(self, msg_word: u32) -> u32 {
        let cleared = msg_word & !(0x03 << 27);
        cleared | ((self as u32) << 27)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PBKDF2-HMAC-SHA1 KEK derivation
// ─────────────────────────────────────────────────────────────────────────────

/// Number of PBKDF2 iterations SRT uses when deriving the KEK.
const PBKDF2_ITERATIONS: u32 = 2048;

/// Derive a 16-byte Key Encrypting Key (KEK) from a passphrase and an SRT salt
/// via PBKDF2-HMAC-SHA1 with [`PBKDF2_ITERATIONS`] iterations.
///
/// This is the algorithm the SRT spec mandates for the `PBKDF2` step in the
/// Keying Material message exchange. Only the least-significant 64 bits (the
/// final eight bytes in wire order) of the 128-bit KM salt are supplied to
/// PBKDF2. The output length is always 16 bytes (AES-128 key).
#[must_use]
pub fn pbkdf2_kek(passphrase: &[u8], salt: &[u8]) -> [u8; 16] {
    use hmac::Hmac;
    use pbkdf2::pbkdf2;
    use sha1::Sha1;

    let pbkdf_salt = &salt[salt.len().saturating_sub(8)..];
    let mut kek = [0u8; 16];
    pbkdf2::<Hmac<Sha1>>(passphrase, pbkdf_salt, PBKDF2_ITERATIONS, &mut kek)
        .expect("PBKDF2 with a fixed 16-byte output never fails");
    kek
}

// ─────────────────────────────────────────────────────────────────────────────
// AES Key Wrap / Unwrap  (RFC 3394)
// ─────────────────────────────────────────────────────────────────────────────

/// The RFC 3394 default IV / integrity-check value.
const AES_KEY_WRAP_IV: [u8; 8] = [0xA6, 0xA6, 0xA6, 0xA6, 0xA6, 0xA6, 0xA6, 0xA6];

/// Wrap `key_data` (must be a multiple of 8 bytes) with `kek` using AES Key
/// Wrap (RFC 3394).
///
/// Returns the wrapped key, which is 8 bytes longer than the input.
///
/// # Panics
///
/// Panics if `key_data` is empty or not a multiple of 8 bytes.
#[must_use]
#[allow(clippy::many_single_char_names)] // RFC 3394 uses single-letter variable names (a, b, t, n, r)
pub fn aes_key_wrap(kek: &[u8; 16], key_data: &[u8]) -> Vec<u8> {
    assert!(
        !key_data.is_empty() && key_data.len() % 8 == 0,
        "key_data must be a non-empty multiple of 8 bytes"
    );

    let semi_blocks = key_data.len() / 8; // number of 8-byte semi-blocks
    let mut r: Vec<[u8; 8]> = (0..semi_blocks)
        .map(|idx| {
            let mut block = [0u8; 8];
            block.copy_from_slice(&key_data[idx * 8..(idx + 1) * 8]);
            block
        })
        .collect();
    let mut accumulator = AES_KEY_WRAP_IV;
    let cipher = Aes128::new_from_slice(kek).expect("16-byte KEK always valid for AES-128");

    for pass in 0..6u64 {
        for (idx, r_block) in r.iter_mut().enumerate() {
            let step_no = (pass * semi_blocks as u64 + (idx as u64 + 1)).to_be_bytes();
            // B = AES(accumulator || R[idx])
            let mut cipher_block = [0u8; 16];
            cipher_block[..8].copy_from_slice(&accumulator);
            cipher_block[8..].copy_from_slice(r_block);
            cipher.encrypt_block((&mut cipher_block).into());
            // accumulator = MSB(64, B) XOR step_no
            accumulator.copy_from_slice(&cipher_block[..8]);
            for (acc_byte, step_byte) in accumulator.iter_mut().zip(step_no.iter()) {
                *acc_byte ^= step_byte;
            }
            // R[idx] = LSB(64, B)
            r_block.copy_from_slice(&cipher_block[8..]);
        }
    }

    let mut out = Vec::with_capacity(8 + key_data.len());
    out.extend_from_slice(&accumulator);
    for block in &r {
        out.extend_from_slice(block);
    }
    out
}

/// Errors that can arise while unwrapping an AES-wrapped key.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyUnwrapError {
    /// The wrapped-key ciphertext is the wrong length (not a multiple of 8
    /// bytes, or fewer than 24 bytes).
    BadLength,
    /// The integrity check failed — the key, KEK, or wrapped-key material is
    /// corrupt or tampered.
    IntegrityCheckFailed,
}

impl fmt::Display for KeyUnwrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyUnwrapError::BadLength => write!(f, "wrapped key has invalid length"),
            KeyUnwrapError::IntegrityCheckFailed => {
                write!(f, "AES key-unwrap integrity check failed")
            }
        }
    }
}

/// Unwrap a wrapped key produced by [`aes_key_wrap`] using `kek`.
///
/// Returns the unwrapped plaintext key on success, or [`KeyUnwrapError`] if the
/// wrapped key is malformed or the integrity check fails.
#[allow(clippy::many_single_char_names)] // RFC 3394 uses single-letter variable names
pub fn aes_key_unwrap(kek: &[u8; 16], wrapped: &[u8]) -> Result<Vec<u8>, KeyUnwrapError> {
    if wrapped.len() < 24 || wrapped.len() % 8 != 0 {
        return Err(KeyUnwrapError::BadLength);
    }

    let semi_blocks = wrapped.len() / 8 - 1;
    let mut accumulator = [0u8; 8];
    accumulator.copy_from_slice(&wrapped[..8]);
    let mut r: Vec<[u8; 8]> = (0..semi_blocks)
        .map(|idx| {
            let mut block = [0u8; 8];
            block.copy_from_slice(&wrapped[(idx + 1) * 8..(idx + 2) * 8]);
            block
        })
        .collect();

    let cipher = Aes128::new_from_slice(kek).expect("16-byte KEK always valid for AES-128");

    for pass in (0..6u64).rev() {
        for (idx, r_block) in r.iter_mut().enumerate().rev() {
            let step_no = (pass * semi_blocks as u64 + (idx as u64 + 1)).to_be_bytes();
            // accumulator XOR step_no
            for (acc_byte, step_byte) in accumulator.iter_mut().zip(step_no.iter()) {
                *acc_byte ^= step_byte;
            }
            // B = AES_inv(accumulator || R[idx])
            let mut cipher_block = [0u8; 16];
            cipher_block[..8].copy_from_slice(&accumulator);
            cipher_block[8..].copy_from_slice(r_block);
            cipher.decrypt_block((&mut cipher_block).into());
            accumulator.copy_from_slice(&cipher_block[..8]);
            r_block.copy_from_slice(&cipher_block[8..]);
        }
    }

    if accumulator != AES_KEY_WRAP_IV {
        return Err(KeyUnwrapError::IntegrityCheckFailed);
    }

    let mut out = Vec::with_capacity(semi_blocks * 8);
    for block in &r {
        out.extend_from_slice(block);
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// KM message (KMREQ / KMRSP) codec
// ─────────────────────────────────────────────────────────────────────────────

/// Key-material message type: KMREQ (initiator→responder) or KMRSP (response).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KmMessageType {
    /// Key-Material REQuest (sent by the publishing side).
    Request,
    /// Key-Material ReSPonse (echo from the receiving side confirming acceptance).
    Response,
}

/// Which SEK(s) a KM message carries in its two-bit `KK` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum KmKeyFlags {
    /// One even SEK.
    Even = 0b01,
    /// One odd SEK.
    Odd = 0b10,
    /// Both SEKs, wrapped together in even-then-odd order.
    EvenAndOdd = 0b11,
}

impl KmKeyFlags {
    fn from_wire(value: u8) -> Option<Self> {
        match value {
            0b01 => Some(Self::Even),
            0b10 => Some(Self::Odd),
            0b11 => Some(Self::EvenAndOdd),
            _ => None,
        }
    }

    /// Whether this message includes the requested encrypted data-key slot.
    #[must_use]
    pub fn contains(self, flag: KkFlag) -> bool {
        matches!(
            (self, flag),
            (Self::Even | Self::EvenAndOdd, KkFlag::EvenKey)
                | (Self::Odd | Self::EvenAndOdd, KkFlag::OddKey)
        )
    }

    fn key_count(self) -> usize {
        usize::from(matches!(self, Self::EvenAndOdd)) + 1
    }
}

/// Parsed SRT Keying-Material (KM) message.
///
/// Wire layout (simplified from the SRT spec / Haivision reference):
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |S| Ver |  PT   |             Sign             | Resv1 |KK|  Word 0
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                        KEKI                                   |  Word 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |  Cipher   |  Auth     |    SE      |      Resv2                |  Word 2
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |             Resv3             |    SLen/4    |    KLen/4      |  Word 3
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                        Salt (variable, SLen bytes)            |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |       Wrapped Key(s) (KLen*n+8 bytes, even before odd)        |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
///
/// For this implementation:
/// - `Ver` = 1, `PT` = 2 (KM message), `Sign` = `0x2029` (Haivision magic).
/// - `KK` identifies one even key, one odd key, or both keys.
/// - `Cipher` = 2 (AES-CTR), `Auth` = 0, `SE` = 2 (MPEG-TS/SRT).
/// - `SLen` = 16 (salt bytes), `KLen` = 16 (AES-128 SEK bytes).
/// - The wrapped-key field is 24 bytes for one SEK and 40 bytes for two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmMessage {
    /// Whether this is a KMREQ (request) or KMRSP (response).
    pub msg_type: KmMessageType,
    /// Key Encrypting Key Index (KEKI). Conventionally 0 for passphrase-based.
    pub keki: u32,
    /// Even/odd slots carried by this message.
    pub key_flags: KmKeyFlags,
    /// 16-byte salt used for PBKDF2 key derivation.
    pub salt: [u8; 16],
    /// RFC 3394 wrapped SEK bytes (24 bytes for one key, 40 for both).
    pub wrapped_sek: Vec<u8>,
}

/// Haivision KM message signature bytes (`Sign` field).
const KM_SIGN: u8 = 0x20;
const KM_SIGN2: u8 = 0x29;

/// KM message payload type (PT = 2 = KM).
const KM_PT: u8 = 2;
/// KM message version.
const KM_VER: u8 = 1;
/// Cipher: AES-CTR = 2.
const KM_CIPHER_AES_CTR: u8 = 2;
/// Stream encapsulation: MPEG-TS over SRT.
const KM_SE_MPEG_TS_SRT: u8 = 2;
/// Minimum KM message wire length in bytes.
/// Header (4 words = 16 bytes) + 16-byte salt + 24-byte wrapped key = 56.
pub(crate) const KM_MIN_LEN: usize = 56;

impl KmMessage {
    /// Encode this KM message to bytes suitable for embedding in an SRT
    /// handshake extension or a control packet.
    ///
    /// # Panics
    ///
    /// Panics when `wrapped_sek` does not match the advertised `key_flags`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let expected_wrapped_len = self.key_flags.key_count() * 16 + 8;
        assert_eq!(
            self.wrapped_sek.len(),
            expected_wrapped_len,
            "KM wrapped-key length must match KK flags"
        );
        let mut out = Vec::with_capacity(32 + expected_wrapped_len);

        // Word 0 byte layout:
        //   byte 0: S(1) | Ver(3) | PT(4) = 0x12
        //   byte 1: Sign[0] = 0x20
        //   byte 2: Sign[1] = 0x29

        out.push((KM_VER << 4) | KM_PT); // byte 0
        out.push(KM_SIGN); // byte 1
        out.push(KM_SIGN2); // byte 2
        out.push(self.key_flags as u8); // byte 3

        // Word 1: KEKI (4 bytes, big-endian)
        out.extend_from_slice(&self.keki.to_be_bytes());

        // Word 2: Cipher(8) | Auth(8) | SE(8) | Resv2(8).
        out.push(KM_CIPHER_AES_CTR); // Cipher
        out.push(0); // Auth
        out.push(KM_SE_MPEG_TS_SRT); // SE
        out.push(0); // Resv2

        // Word 3: Resv3(16) | SLen/4(8) | KLen/4(8).
        out.extend_from_slice(&[0, 0]);
        out.push(4); // SLen/4 = 4 (16-byte salt)
        out.push(4); // KLen/4 = 4 (16-byte unwrapped SEK)

        // Salt (16 bytes)
        out.extend_from_slice(&self.salt);

        // Wrapped SEK(s), with even first for a dual-key message.
        out.extend_from_slice(&self.wrapped_sek);

        out
    }

    /// Decode a KM message from raw bytes.
    ///
    /// Returns `None` if the bytes are too short, have a bad signature, or
    /// carry an unsupported cipher or key-length.
    #[must_use]
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < KM_MIN_LEN {
            return None;
        }
        // Byte 0: S(1) | Ver(3) | PT(4)
        let ver = (buf[0] >> 4) & 0x07;
        let pt = buf[0] & 0x0F;
        if buf[0] & 0x80 != 0 || ver != KM_VER || pt != KM_PT {
            return None;
        }
        // Bytes 1-2: Sign
        if buf[1] != KM_SIGN || buf[2] != KM_SIGN2 {
            return None;
        }
        // Byte 3: Resv1(6) | KK(2).
        if buf[3] & 0xFC != 0 {
            return None;
        }
        let key_flags = KmKeyFlags::from_wire(buf[3] & 0x03)?;

        // Word 1: KEKI
        let keki = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

        // Word 2: Cipher | Auth | SE | Resv2.
        if buf[8] != KM_CIPHER_AES_CTR
            || buf[9] != 0
            || buf[10] != KM_SE_MPEG_TS_SRT
            || buf[11] != 0
        {
            return None;
        }
        // Word 3: Resv3 | SLen/4 | KLen/4.
        if buf[12] != 0 || buf[13] != 0 {
            return None;
        }
        let slen_words = usize::from(buf[14]);
        let klen_words = usize::from(buf[15]);
        let slen = slen_words * 4;
        let klen = klen_words * 4;

        // AES-128 uses a 16-byte salt and a 16-byte SEK. RFC 3394 adds an
        // eight-byte integrity value to the wrapped key.
        if slen != 16 || klen != 16 {
            return None;
        }
        let wrapped_len = key_flags.key_count() * klen + 8;
        if buf.len() != 16 + slen + wrapped_len {
            return None;
        }

        let mut salt = [0u8; 16];
        salt.copy_from_slice(&buf[16..32]);

        Some(KmMessage {
            msg_type: KmMessageType::Request,
            keki,
            key_flags,
            salt,
            wrapped_sek: buf[32..].to_vec(),
        })
    }

    /// Return a successful KMRSP that echoes this request's exact key material.
    #[must_use]
    pub fn as_response(&self) -> Self {
        let mut response = self.clone();
        response.msg_type = KmMessageType::Response;
        response
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SrtCrypto — the main encryption/decryption context
// ─────────────────────────────────────────────────────────────────────────────

/// Active SEK + 112-bit IV for a single SRT connection.
///
/// Created via [`SrtCrypto::from_passphrase`] (normal path, caller-supplied SEK)
/// or [`SrtCrypto::from_raw_sek`] (test/interop path).
#[derive(Clone, PartialEq, Eq)]
pub struct SrtCrypto {
    /// 16-byte AES-128 Stream Encrypting Key.
    sek: [u8; 16],
    /// 16-byte salt (stored so we can re-derive the KEK to build KM messages).
    salt: [u8; 16],
    /// 112-bit (14-byte) per-message IV extracted from the KM message.
    /// Per the SRT spec the IV is the most-significant 112 bits of the salt.
    msg_iv: [u8; 14],
}

impl fmt::Debug for SrtCrypto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SrtCrypto")
            .field("sek", &"[redacted]")
            .field("salt", &self.salt)
            .finish_non_exhaustive()
    }
}

impl SrtCrypto {
    /// Create a crypto context from a passphrase, a caller-supplied 16-byte
    /// salt, and an explicit SEK.  Use [`SrtCrypto::from_passphrase`] for
    /// normal operation where the SEK should be random.
    ///
    /// `sek` must be 16 bytes (AES-128).
    pub fn from_raw_sek(_passphrase: &[u8], salt: &[u8; 16], sek: [u8; 16]) -> Self {
        let mut msg_iv = [0u8; 14];
        // The msg IV is the most-significant 112 bits (14 bytes) of the salt.
        msg_iv.copy_from_slice(&salt[..14]);
        SrtCrypto {
            sek,
            salt: *salt,
            msg_iv,
        }
    }

    /// Create a crypto context by configuring the SEK to use for this session.
    /// The KEK derived from `passphrase` + `salt` is used only when building
    /// or parsing KM messages.  Also extracts the 112-bit per-message IV from
    /// the salt.
    ///
    /// For tests, pass a known SEK.  In production, generate a random SEK
    /// externally (e.g. via `rand::random::<[u8; 16]>()`).
    pub fn from_passphrase(passphrase: &[u8], salt: &[u8; 16], sek: [u8; 16]) -> Self {
        Self::from_raw_sek(passphrase, salt, sek)
    }

    /// Return the active 16-byte SEK.
    #[must_use]
    pub fn sek(&self) -> &[u8; 16] {
        &self.sek
    }

    /// Return the 16-byte salt.
    #[must_use]
    pub fn salt(&self) -> &[u8; 16] {
        &self.salt
    }

    /// Build a KMREQ [`KmMessage`] wrapping the SEK, ready to send to the peer.
    ///
    /// `passphrase` is the shared secret used to derive the KEK.
    #[must_use]
    pub fn build_km_message(&self, passphrase: &[u8]) -> KmMessage {
        self.build_km_message_for(passphrase, KkFlag::EvenKey)
            .expect("the even flag is valid key material")
    }

    /// Build a single-slot KMREQ for either the even or odd SEK slot.
    ///
    /// Returns `None` for `Clear` and the data-plane-only invalid `KK=11`
    /// representation.
    #[must_use]
    pub fn build_km_message_for(&self, passphrase: &[u8], key_flag: KkFlag) -> Option<KmMessage> {
        let key_flags = match key_flag {
            KkFlag::EvenKey => KmKeyFlags::Even,
            KkFlag::OddKey => KmKeyFlags::Odd,
            KkFlag::Clear | KkFlag::Invalid => return None,
        };
        let kek = pbkdf2_kek(passphrase, &self.salt);
        Some(KmMessage {
            msg_type: KmMessageType::Request,
            keki: 0,
            key_flags,
            salt: self.salt,
            wrapped_sek: aes_key_wrap(&kek, &self.sek),
        })
    }

    /// Build the dual-key KMREQ used during a libsrt pre-announcement window.
    ///
    /// Both contexts must share the same salt/KEK. The unwrapped order is
    /// always even SEK followed by odd SEK, independent of the current active
    /// slot.
    #[must_use]
    pub fn build_dual_km_message(even: &Self, odd: &Self, passphrase: &[u8]) -> Option<KmMessage> {
        if even.salt != odd.salt {
            return None;
        }
        let mut keys = Vec::with_capacity(32);
        keys.extend_from_slice(&even.sek);
        keys.extend_from_slice(&odd.sek);
        let kek = pbkdf2_kek(passphrase, &even.salt);
        Some(KmMessage {
            msg_type: KmMessageType::Request,
            keki: 0,
            key_flags: KmKeyFlags::EvenAndOdd,
            salt: even.salt,
            wrapped_sek: aes_key_wrap(&kek, &keys),
        })
    }

    /// Construct an [`SrtCrypto`] from a received [`KmMessage`] and the shared
    /// passphrase, by re-deriving the KEK and unwrapping the SEK.
    pub fn from_km_message(km: &KmMessage, passphrase: &[u8]) -> Result<Self, KeyUnwrapError> {
        let keys = UnwrappedKmKeys::from_message(km, passphrase)?;
        match (keys.even, keys.odd) {
            (Some(key), None) | (None, Some(key)) => Ok(key),
            _ => Err(KeyUnwrapError::BadLength),
        }
    }

    /// Build the 128-bit AES-CTR counter/IV for a given packet sequence number.
    ///
    /// Per the SRT spec the IV is constructed as:
    /// ```text
    /// CTR = MSB(112, salt) XOR (packet-index in bytes 10..14), with the
    /// least-significant 16 bits reserved for the per-packet block counter.
    /// ```
    #[must_use]
    fn build_ctr_iv(&self, seq_no: u32) -> [u8; 16] {
        let mut iv = [0u8; 16];
        iv[..14].copy_from_slice(&self.msg_iv);
        // SRT places the 32-bit packet index immediately above the 16-bit
        // block counter, then XORs the upper 112 bits with the salt IV.
        let seq_bytes = seq_no.to_be_bytes();
        for (iv_byte, seq_byte) in iv[10..14].iter_mut().zip(seq_bytes.iter()) {
            *iv_byte ^= seq_byte;
        }
        iv
    }

    /// Encrypt `payload` in-place using AES-CTR with the IV derived from
    /// `seq_no`.  Operates on the even SEK (KkFlag::EvenKey).
    ///
    /// For unencrypted traffic (e.g. when the key flag is `Clear`) simply do
    /// not call this function — nothing prevents decryption of an already-clear
    /// packet.
    pub fn encrypt_packet(&self, seq_no: u32, payload: &mut [u8]) {
        let iv = self.build_ctr_iv(seq_no);
        aes_ctr_xor(&self.sek, &iv, payload);
    }

    /// Decrypt `payload` in-place (AES-CTR is self-inverse: encryption and
    /// decryption are identical operations).
    pub fn decrypt_packet(&self, seq_no: u32, payload: &mut [u8]) {
        self.encrypt_packet(seq_no, payload); // CTR mode: same operation
    }
}

#[derive(Debug)]
pub(crate) struct UnwrappedKmKeys {
    even: Option<SrtCrypto>,
    odd: Option<SrtCrypto>,
}

impl UnwrappedKmKeys {
    fn from_message(km: &KmMessage, passphrase: &[u8]) -> Result<Self, KeyUnwrapError> {
        let kek = pbkdf2_kek(passphrase, &km.salt);
        let unwrapped = aes_key_unwrap(&kek, &km.wrapped_sek)?;
        if unwrapped.len() != km.key_flags.key_count() * 16 {
            return Err(KeyUnwrapError::BadLength);
        }
        let context = |bytes: &[u8]| {
            let mut sek = [0u8; 16];
            sek.copy_from_slice(bytes);
            SrtCrypto::from_raw_sek(passphrase, &km.salt, sek)
        };
        Ok(match km.key_flags {
            KmKeyFlags::Even => Self {
                even: Some(context(&unwrapped[..16])),
                odd: None,
            },
            KmKeyFlags::Odd => Self {
                even: None,
                odd: Some(context(&unwrapped[..16])),
            },
            KmKeyFlags::EvenAndOdd => Self {
                even: Some(context(&unwrapped[..16])),
                odd: Some(context(&unwrapped[16..32])),
            },
        })
    }

    fn get(&self, flag: KkFlag) -> Option<&SrtCrypto> {
        match flag {
            KkFlag::EvenKey => self.even.as_ref(),
            KkFlag::OddKey => self.odd.as_ref(),
            KkFlag::Clear | KkFlag::Invalid => None,
        }
    }

    fn take(&mut self, flag: KkFlag) -> Option<SrtCrypto> {
        match flag {
            KkFlag::EvenKey => self.even.take(),
            KkFlag::OddKey => self.odd.take(),
            KkFlag::Clear | KkFlag::Invalid => None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AES-CTR keystream helper
// ─────────────────────────────────────────────────────────────────────────────

/// XOR `data` with the AES-CTR keystream generated from `key` and starting IV
/// `iv`.  The counter increments on each 16-byte block.
///
/// Uses the `aes` crate directly (no `ctr` mode wrapper) to stay unsafe-free
/// and avoid bringing in the additional cipher abstraction crate.
fn aes_ctr_xor(key: &[u8; 16], iv: &[u8; 16], data: &mut [u8]) {
    let cipher = Aes128::new_from_slice(key).expect("16-byte key is always valid for AES-128");

    let mut counter = *iv;
    let mut pos = 0usize;

    while pos < data.len() {
        // Encrypt the counter block to get one block of keystream.
        let mut keystream = counter;
        cipher.encrypt_block((&mut keystream).into());

        let chunk_end = (pos + 16).min(data.len());
        for (data_byte, ks_byte) in data[pos..chunk_end].iter_mut().zip(keystream.iter()) {
            *data_byte ^= ks_byte;
        }
        pos += 16;

        // SRT reserves only the least-significant 16 bits for the block
        // counter. Packet payloads are bounded by the MTU, so it cannot wrap.
        increment_block_counter(&mut counter);
    }
}

/// Increment the least-significant 16-bit SRT block counter by one.
fn increment_block_counter(counter: &mut [u8; 16]) {
    let block = u16::from_be_bytes([counter[14], counter[15]]).wrapping_add(1);
    counter[14..].copy_from_slice(&block.to_be_bytes());
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
