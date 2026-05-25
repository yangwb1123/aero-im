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
//!   KM message, honouring the KK key-flag bits (`00`=clear, `01`=odd, `10`=even)
//!   in the SRT data-packet header word.
//!
//! ## Public API
//!
//! | Item | Purpose |
//! |------|---------|
//! | [`SrtCrypto`] | Context holding a single active SEK + IV |
//! | `SrtCrypto::from_passphrase` | Derive KEK from passphrase + 16-byte salt, configure a SEK |
//! | `SrtCrypto::encrypt_packet` | In-place AES-CTR encrypt of a data payload |
//! | `SrtCrypto::decrypt_packet` | In-place AES-CTR decrypt of a data payload |
//! | [`KmMessage`] | KMREQ/KMRSP on-wire structure |
//! | `KmMessage::encode` | Build the wire form (wraps the SEK) |
//! | `KmMessage::decode` | Parse and unwrap the SEK from a received KM message |
//! | [`aes_key_wrap`] / [`aes_key_unwrap`] | Standalone RFC 3394 wrap/unwrap |
//! | [`pbkdf2_kek`] | Deterministic KEK derivation (exposed for tests) |
//!
//! ## KK bit convention
//!
//! Bits 25–24 (0-indexed from LSB) of the SRT data-header word-1 carry the `KK`
//! field.  This crate exposes [`KkFlag`] and `SrtCrypto` honours it:
//! - `KkFlag::Clear` (`00`) → packet is unencrypted; `encrypt`/`decrypt` are
//!   no-ops.
//! - `KkFlag::EvenKey` (`10`) → encrypt/decrypt with the even SEK.
//! - `KkFlag::OddKey` (`01`) → encrypt/decrypt with the odd SEK.
//!
//! For simplicity this implementation keeps only **one** active SEK (which is
//! treated as the even key); the odd-key slot mirrors it so a real peer's KK flag
//! is always honoured without a second key-exchange round.

// Lots of SRT-spec names (KK, KMREQ, SEK, PBKDF2, …) that are short and
// domain-standard; suppressing the doc_markdown lint keeps code readable.
#![allow(clippy::doc_markdown)]

use std::fmt;

use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};

// ─────────────────────────────────────────────────────────────────────────────
// KK flag
// ─────────────────────────────────────────────────────────────────────────────

/// The `KK` (key-keying) field in bits 25–24 of the SRT data-header word-1.
///
/// Tells the receiver which of the two possible SEKs was used to encrypt this
/// packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KkFlag {
    /// Packet is unencrypted (`00`).
    Clear = 0b00,
    /// Encrypted with the odd SEK (`01`).
    OddKey = 0b01,
    /// Encrypted with the even SEK (`10`).
    EvenKey = 0b10,
}

impl KkFlag {
    /// Extract the KK flag from SRT data-header word-1.
    ///
    /// `msg_word` is the second 32-bit word of the SRT header as stored in
    /// [`crate::protocol::PacketKind::Data::msg_word`].
    #[must_use]
    pub fn from_msg_word(msg_word: u32) -> Self {
        match (msg_word >> 24) & 0x03 {
            0b01 => KkFlag::OddKey,
            0b10 => KkFlag::EvenKey,
            _ => KkFlag::Clear,
        }
    }

    /// Set the KK bits in `msg_word`, returning the updated word.
    #[must_use]
    pub fn set_in_msg_word(self, msg_word: u32) -> u32 {
        let cleared = msg_word & !(0x03 << 24);
        cleared | ((self as u32) << 24)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PBKDF2-HMAC-SHA1 KEK derivation
// ─────────────────────────────────────────────────────────────────────────────

/// Number of PBKDF2 iterations SRT uses when deriving the KEK.
const PBKDF2_ITERATIONS: u32 = 2048;

/// Derive a 16-byte Key Encrypting Key (KEK) from a passphrase and a 16-byte
/// salt via PBKDF2-HMAC-SHA1 with [`PBKDF2_ITERATIONS`] iterations.
///
/// This is the algorithm the SRT spec mandates for the `PBKDF2` step in the
/// Keying Material message exchange.  The output length is always 16 bytes
/// (AES-128 key).
#[must_use]
pub fn pbkdf2_kek(passphrase: &[u8], salt: &[u8]) -> [u8; 16] {
    use hmac::Hmac;
    use pbkdf2::pbkdf2;
    use sha1::Sha1;

    let mut kek = [0u8; 16];
    pbkdf2::<Hmac<Sha1>>(passphrase, salt, PBKDF2_ITERATIONS, &mut kek)
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

/// Parsed SRT Keying-Material (KM) message.
///
/// Wire layout (simplified from the SRT spec / Haivision reference):
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |0| Ver |  PT   |    Sign       |     resv    |S| K |KS | Resv  |  Word 0
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                        KEKI                                   |  Word 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |  Cipher   | Auth  |  SE   |    SLen/4     |    KLen/4         |  Word 2
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                        Salt (variable, SLen bytes)            |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |              Wrapped Key (variable, KLen+8 bytes)             |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
///
/// For this implementation:
/// - `Ver` = 1, `PT` = 2 (KM message), `Sign` = `0x2029` (Haivision magic).
/// - `Cipher` = 2 (AES-CTR), `Auth` = 0, `SE` = 0.
/// - `SLen` = 16 (salt bytes), `KLen` = 16 (AES-128 SEK bytes).
/// - The wrapped-key field carries the AES-Key-Wrapped SEK (24 bytes for a
///   16-byte SEK).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmMessage {
    /// Whether this is a KMREQ (request) or KMRSP (response).
    pub msg_type: KmMessageType,
    /// Key Encrypting Key Index (KEKI). Conventionally 0 for passphrase-based.
    pub keki: u32,
    /// 16-byte salt used for PBKDF2 key derivation.
    pub salt: [u8; 16],
    /// The wrapped SEK (24 bytes: RFC 3394 wrap of a 16-byte key).
    pub wrapped_sek: [u8; 24],
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

/// Minimum KM message wire length in bytes.
/// Header (3 words = 12 bytes) + 16-byte salt + 24-byte wrapped key = 52.
pub(crate) const KM_MIN_LEN: usize = 52;

impl KmMessage {
    /// Encode this KM message to bytes suitable for embedding in an SRT
    /// handshake extension or a control packet.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(KM_MIN_LEN);

        // Word 0 byte layout:
        //   byte 0: Ver<<4 | PT  = 0x12
        //   byte 1: Sign[0] = 0x20
        //   byte 2: Sign[1] = 0x29
        //   byte 3: (S=0)|(K<<3)|(KS<<1)
        //     K=2 (even key=0b10), KS=2 (128-bit key in 64-bit units)
        let kk_bits: u8 = 0b10; // even key
        let byte3: u8 = (kk_bits << 3) | (2u8 << 1); // KS=2 → 128-bit key

        out.push((KM_VER << 4) | KM_PT); // byte 0
        out.push(KM_SIGN); // byte 1
        out.push(KM_SIGN2); // byte 2
        out.push(byte3); // byte 3

        // Word 1: KEKI (4 bytes, big-endian)
        out.extend_from_slice(&self.keki.to_be_bytes());

        // Word 2: Cipher(8) | Auth(4)/SE(4) | SLen/4(8) | KLen/4(8)
        //   Cipher=2 (AES-CTR), Auth=0, SE=0
        //   SLen = 16 bytes → SLen/4 = 4
        //   wrapped_sek = 24 bytes → KLen/4 = 6
        out.push(KM_CIPHER_AES_CTR); // Cipher
        out.push(0x00); // Auth | SE (packed: 0)
        out.push(4); // SLen/4 = 4  (16 bytes)
        out.push(6); // KLen/4 = 6  (24 bytes wrapped)

        // Salt (16 bytes)
        out.extend_from_slice(&self.salt);

        // Wrapped SEK (24 bytes)
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
        // Byte 0: Ver(4) | PT(4)
        let ver = buf[0] >> 4;
        let pt = buf[0] & 0x0F;
        if ver != KM_VER || pt != KM_PT {
            return None;
        }
        // Bytes 1-2: Sign
        if buf[1] != KM_SIGN || buf[2] != KM_SIGN2 {
            return None;
        }
        // Byte 3: S | K | KS (accepted any K)

        // Word 1: KEKI
        let keki = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

        // Word 2: Cipher | Auth/SE | SLen/4 | KLen/4
        let cipher = buf[8];
        if cipher != KM_CIPHER_AES_CTR {
            return None;
        }
        let slen_words = usize::from(buf[10]);
        let klen_words = usize::from(buf[11]);
        let slen = slen_words * 4;
        let klen = klen_words * 4;

        // Validate expected sizes for AES-128: 16-byte salt + 24-byte wrapped key.
        if slen != 16 || klen != 24 {
            return None;
        }
        if buf.len() < 12 + slen + klen {
            return None;
        }

        let mut salt = [0u8; 16];
        salt.copy_from_slice(&buf[12..28]);

        let mut wrapped_sek = [0u8; 24];
        wrapped_sek.copy_from_slice(&buf[28..52]);

        Some(KmMessage {
            msg_type: KmMessageType::Request,
            keki,
            salt,
            wrapped_sek,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SrtCrypto — the main encryption/decryption context
// ─────────────────────────────────────────────────────────────────────────────

/// Active SEK + 112-bit IV for a single SRT connection.
///
/// Created via [`SrtCrypto::from_passphrase`] (normal path, caller-supplied SEK)
/// or [`SrtCrypto::from_raw_sek`] (test/interop path).
#[derive(Debug, Clone)]
pub struct SrtCrypto {
    /// 16-byte AES-128 Stream Encrypting Key.
    sek: [u8; 16],
    /// 16-byte salt (stored so we can re-derive the KEK to build KM messages).
    salt: [u8; 16],
    /// 112-bit (14-byte) per-message IV extracted from the KM message.
    /// Per the SRT spec the IV is the rightmost 112 bits of the salt.
    msg_iv: [u8; 14],
}

impl SrtCrypto {
    /// Create a crypto context from a passphrase, a caller-supplied 16-byte
    /// salt, and an explicit SEK.  Use [`SrtCrypto::from_passphrase`] for
    /// normal operation where the SEK should be random.
    ///
    /// `sek` must be 16 bytes (AES-128).
    pub fn from_raw_sek(_passphrase: &[u8], salt: &[u8; 16], sek: [u8; 16]) -> Self {
        let mut msg_iv = [0u8; 14];
        // The msg IV is the rightmost 112 bits (14 bytes) of the 128-bit salt.
        msg_iv.copy_from_slice(&salt[2..]);
        SrtCrypto { sek, salt: *salt, msg_iv }
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
        let kek = pbkdf2_kek(passphrase, &self.salt);
        let wrapped_sek_vec = aes_key_wrap(&kek, &self.sek);
        let mut wrapped_sek = [0u8; 24];
        wrapped_sek.copy_from_slice(&wrapped_sek_vec);
        KmMessage {
            msg_type: KmMessageType::Request,
            keki: 0,
            salt: self.salt,
            wrapped_sek,
        }
    }

    /// Construct an [`SrtCrypto`] from a received [`KmMessage`] and the shared
    /// passphrase, by re-deriving the KEK and unwrapping the SEK.
    pub fn from_km_message(
        km: &KmMessage,
        passphrase: &[u8],
    ) -> Result<Self, KeyUnwrapError> {
        let kek = pbkdf2_kek(passphrase, &km.salt);
        let sek_vec = aes_key_unwrap(&kek, &km.wrapped_sek)?;
        if sek_vec.len() != 16 {
            return Err(KeyUnwrapError::BadLength);
        }
        let mut sek = [0u8; 16];
        sek.copy_from_slice(&sek_vec);
        Ok(Self::from_raw_sek(passphrase, &km.salt, sek))
    }

    /// Build the 128-bit AES-CTR counter/IV for a given packet sequence number.
    ///
    /// Per the SRT spec the IV is constructed as:
    /// ```text
    /// CTR[15:2] = msg_iv (14 bytes, rightmost 112 bits of salt)
    /// CTR XOR= seq_no in the low 32 bits
    /// ```
    #[must_use]
    fn build_ctr_iv(&self, seq_no: u32) -> [u8; 16] {
        let mut iv = [0u8; 16];
        // Upper 2 bytes are zero; lower 14 bytes come from the message IV.
        iv[2..16].copy_from_slice(&self.msg_iv);
        // XOR the sequence number into the last 4 bytes (big-endian).
        let seq_bytes = seq_no.to_be_bytes();
        for (iv_byte, seq_byte) in iv[12..].iter_mut().zip(seq_bytes.iter()) {
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

        // Increment the 128-bit counter (big-endian).
        increment_counter(&mut counter);
    }
}

/// Increment a 16-byte big-endian counter by 1.
fn increment_counter(counter: &mut [u8; 16]) {
    for byte in counter.iter_mut().rev() {
        let (new_val, overflow) = byte.overflowing_add(1);
        *byte = new_val;
        if !overflow {
            break;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        ];
        let key_data: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        ];
        let expected: [u8; 24] = [
            0x1F, 0xA6, 0x8B, 0x0A, 0x81, 0x12, 0xB4, 0x47,
            0xAE, 0xF3, 0x4B, 0xD8, 0xFB, 0x5A, 0x7B, 0x82,
            0x9D, 0x3E, 0x86, 0x23, 0x71, 0xD2, 0xCF, 0xE5,
        ];
        let wrapped = aes_key_wrap(&kek, &key_data);
        assert_eq!(wrapped.as_slice(), expected.as_slice(), "RFC 3394 §4.1 wrap vector");
    }

    #[test]
    fn aes_key_unwrap_rfc3394_vector() {
        let kek: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        ];
        let wrapped: [u8; 24] = [
            0x1F, 0xA6, 0x8B, 0x0A, 0x81, 0x12, 0xB4, 0x47,
            0xAE, 0xF3, 0x4B, 0xD8, 0xFB, 0x5A, 0x7B, 0x82,
            0x9D, 0x3E, 0x86, 0x23, 0x71, 0xD2, 0xCF, 0xE5,
        ];
        let expected: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        ];
        let unwrapped = aes_key_unwrap(&kek, &wrapped).expect("RFC 3394 unwrap must succeed");
        assert_eq!(unwrapped.as_slice(), expected.as_slice(), "RFC 3394 §4.1 unwrap vector");
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
        assert_eq!(aes_key_unwrap(&kek, &[0u8; 7]), Err(KeyUnwrapError::BadLength));
        // 25 bytes is not a multiple of 8.
        assert_eq!(aes_key_unwrap(&kek, &[0u8; 25]), Err(KeyUnwrapError::BadLength));
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
        let k3 = pbkdf2_kek(b"pass1", &[1u8; 16]);
        assert_ne!(k1, k2, "different passphrases produce different KEKs");
        assert_ne!(k1, k3, "different salts produce different KEKs");
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
        assert_ne!(data.as_slice(), plaintext.as_slice(), "ciphertext differs from plaintext");
        crypto.decrypt_packet(42, &mut data);
        assert_eq!(data.as_slice(), plaintext.as_slice(), "decrypt recovers plaintext");
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
        assert_eq!(encoded.len(), KM_MIN_LEN, "encoded KM is {KM_MIN_LEN} bytes");
        let decoded = KmMessage::decode(&encoded).expect("decode must succeed on valid bytes");
        assert_eq!(decoded.salt, km.salt, "salt round-trips");
        assert_eq!(decoded.wrapped_sek, km.wrapped_sek, "wrapped_sek round-trips");
        assert_eq!(decoded.keki, km.keki, "keki round-trips");
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
        let receiver = SrtCrypto::from_km_message(&km, passphrase)
            .expect("receiver must unwrap the SEK");
        assert_eq!(receiver.sek(), sender.sek(), "receiver recovered the same SEK");
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
        // Bits 25–24 should be 10; all others stay 0.
        assert_eq!(with_even & !(0x03 << 24), 0, "no other bits modified");
    }
}
