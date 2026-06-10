//! RTCP bandwidth feedback — REMB and TWCC (transport-cc) parse/encode.
//!
//! Complements [`crate::rtcp_feedback`] (PLI/FIR keyframe requests) with the
//! two feedback formats that drive adaptive bitrate:
//!
//! - **REMB** (Receiver Estimated Maximum Bitrate,
//!   `draft-alvestrand-rmcat-remb`): a PSFB (PT=206) packet with FMT=15
//!   (application-layer feedback) whose FCI starts with the ASCII identifier
//!   `REMB`. Carries the receiver's total estimated bitrate as a
//!   mantissa/exponent pair plus the SSRCs it applies to.
//! - **TWCC** (Transport-Wide Congestion Control feedback,
//!   `draft-holmer-rmcat-transport-wide-cc-extensions-01`): an RTPFB (PT=205)
//!   packet with FMT=15. Carries per-packet arrival info (base sequence,
//!   packet-status chunks in run-length and status-vector form, and receive
//!   deltas in 250 µs units) from which loss and delay trend are derived.
//!
//! All parsers are total: malformed or truncated input yields `None` (or is
//! skipped by the compound walker), never a panic.

// ── Wire constants ────────────────────────────────────────────────────────────

/// RTCP PT for Transport-layer Feedback (RFC 4585 §6.1).
const PT_RTPFB: u8 = 205;
/// RTCP PT for Payload-Specific Feedback (RFC 4585 §6.1).
const PT_PSFB: u8 = 206;
/// FMT for application-layer feedback within PSFB (REMB lives here).
const FMT_ALFB: u8 = 15;
/// FMT for transport-cc feedback within RTPFB.
const FMT_TWCC: u8 = 15;
/// ASCII unique identifier at the start of the REMB FCI.
const REMB_ID: [u8; 4] = *b"REMB";
/// REMB mantissa is 18 bits wide.
const REMB_MANTISSA_BITS: u32 = 18;

// ── REMB ──────────────────────────────────────────────────────────────────────

/// A parsed REMB packet: the receiver's total estimated max bitrate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remb {
    /// SSRC of the packet sender (the receiver of the media).
    pub sender_ssrc: u32,
    /// Estimated maximum total bitrate in bits/second (mantissa << exponent,
    /// saturating on overflow).
    pub bitrate_bps: u64,
    /// The media SSRCs this estimate applies to.
    pub ssrcs: Vec<u32>,
}

/// Encode a REMB packet into a fresh buffer.
///
/// The bitrate is encoded as an 18-bit mantissa with a 6-bit exponent; values
/// that cannot be represented exactly are rounded down (mantissa truncation),
/// matching browser behaviour.
#[must_use]
pub fn encode_remb(sender_ssrc: u32, bitrate_bps: u64, ssrcs: &[u32]) -> Vec<u8> {
    let mut exp = 0u32;
    while exp < 63 && (bitrate_bps >> exp) >= (1 << REMB_MANTISSA_BITS) {
        exp += 1;
    }
    let mantissa = (bitrate_bps >> exp) & ((1 << REMB_MANTISSA_BITS) - 1);

    let len_bytes = 12 + 8 + ssrcs.len() * 4;
    let mut buf = Vec::with_capacity(len_bytes);
    buf.push(0x80 | FMT_ALFB); // V=2, P=0, FMT=15
    buf.push(PT_PSFB);
    // length in 32-bit words − 1
    #[allow(clippy::cast_possible_truncation)]
    let words_less_one = (len_bytes / 4 - 1) as u16;
    buf.extend_from_slice(&words_less_one.to_be_bytes());
    buf.extend_from_slice(&sender_ssrc.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes()); // media SSRC = 0 for REMB
    buf.extend_from_slice(&REMB_ID);
    #[allow(clippy::cast_possible_truncation)]
    buf.push(ssrcs.len() as u8);
    // BR Exp (6 bits) | top 2 bits of the 18-bit mantissa
    #[allow(clippy::cast_possible_truncation)]
    buf.push(((exp as u8) << 2) | ((mantissa >> 16) as u8 & 0x03));
    #[allow(clippy::cast_possible_truncation)]
    buf.extend_from_slice(&((mantissa & 0xFFFF) as u16).to_be_bytes());
    for ssrc in ssrcs {
        buf.extend_from_slice(&ssrc.to_be_bytes());
    }
    buf
}

/// Parse a REMB from one complete RTCP packet (header included).
///
/// Returns `None` unless the packet is a well-formed PSFB FMT=15 whose FCI
/// carries the `REMB` identifier.
#[must_use]
pub fn parse_remb(pkt: &[u8]) -> Option<Remb> {
    if pkt.len() < 20 {
        return None;
    }
    if pkt[0] & 0b1100_0000 != 0x80 || pkt[0] & 0b0001_1111 != FMT_ALFB || pkt[1] != PT_PSFB {
        return None;
    }
    let sender_ssrc = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
    // pkt[8..12] = media source SSRC, always 0 for REMB; not validated (some
    // stacks set it anyway).
    if pkt[12..16] != REMB_ID {
        return None;
    }
    let num_ssrc = usize::from(pkt[16]);
    let exp = u32::from(pkt[17] >> 2);
    let mantissa =
        (u64::from(pkt[17] & 0x03) << 16) | (u64::from(pkt[18]) << 8) | u64::from(pkt[19]);
    let bitrate_bps = mantissa.checked_shl(exp).unwrap_or(u64::MAX);

    let ssrc_bytes = pkt.get(20..20 + num_ssrc * 4)?;
    let ssrcs = ssrc_bytes
        .chunks_exact(4)
        .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Some(Remb {
        sender_ssrc,
        bitrate_bps,
        ssrcs,
    })
}

// ── TWCC ──────────────────────────────────────────────────────────────────────

/// Receive status of one packet in a TWCC feedback report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwccStatus {
    /// The packet did not arrive (or arrived without a usable timestamp).
    NotReceived,
    /// The packet arrived; `delta_us` is the receive-time delta to the
    /// previous received packet in microseconds (multiples of 250 µs on the
    /// wire; may be negative for the 2-byte "large delta" form).
    Received {
        /// Receive delta in microseconds.
        delta_us: i32,
    },
}

/// A parsed TWCC (transport-cc) feedback packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwccFeedback {
    /// SSRC of the packet sender.
    pub sender_ssrc: u32,
    /// SSRC of the media source the feedback refers to.
    pub media_ssrc: u32,
    /// Transport-wide sequence number of the first packet in this report.
    pub base_seq: u16,
    /// Reference time in multiples of 64 ms (24-bit signed).
    pub reference_time_64ms: i32,
    /// Feedback packet counter (wraps; used to detect feedback loss).
    pub fb_pkt_count: u8,
    /// One status per packet starting at `base_seq`.
    pub statuses: Vec<TwccStatus>,
}

/// Loss/delay summary extracted from one [`TwccFeedback`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TwccSummary {
    /// Number of packets reported as received.
    pub received: u32,
    /// Number of packets reported as lost (not received).
    pub lost: u32,
    /// One-way-delay trend in µs: mean receive delta of the second half of
    /// received packets minus the first half. Positive = inter-arrival
    /// spacing growing = queue building up on the path. Zero when fewer than
    /// four packets were received (not enough signal).
    pub delay_trend_us: i64,
}

impl TwccFeedback {
    /// Derive the (received, lost, delay-trend) summary for this report.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    pub fn summary(&self) -> TwccSummary {
        let mut deltas: Vec<i64> = Vec::with_capacity(self.statuses.len());
        let mut lost = 0u32;
        for s in &self.statuses {
            match s {
                TwccStatus::Received { delta_us } => deltas.push(i64::from(*delta_us)),
                TwccStatus::NotReceived => lost += 1,
            }
        }
        let received = deltas.len() as u32;
        let delay_trend_us = if deltas.len() >= 4 {
            let half = deltas.len() / 2;
            let first: i64 = deltas[..half].iter().sum();
            let second: i64 = deltas[deltas.len() - half..].iter().sum();
            (second - first) / half as i64
        } else {
            0
        };
        TwccSummary {
            received,
            lost,
            delay_trend_us,
        }
    }
}

/// Parse a TWCC feedback from one complete RTCP packet (header included).
///
/// Returns `None` unless the packet is a well-formed RTPFB FMT=15 with
/// consistent chunk/delta lengths. Trailing padding after the deltas is
/// tolerated (packets are padded to 32-bit boundaries).
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn parse_twcc(pkt: &[u8]) -> Option<TwccFeedback> {
    if pkt.len() < 20 {
        return None;
    }
    if pkt[0] & 0b1100_0000 != 0x80 || pkt[0] & 0b0001_1111 != FMT_TWCC || pkt[1] != PT_RTPFB {
        return None;
    }
    let sender_ssrc = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
    let media_ssrc = u32::from_be_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]);
    let base_seq = u16::from_be_bytes([pkt[12], pkt[13]]);
    let status_count = usize::from(u16::from_be_bytes([pkt[14], pkt[15]]));
    // 24-bit signed reference time, sign-extended via an i32 shift.
    let reference_time_64ms = (i32::from(pkt[16]) << 24
        | i32::from(pkt[17]) << 16
        | i32::from(pkt[18]) << 8)
        >> 8;
    let fb_pkt_count = pkt[19];

    // Walk the packet-status chunks until `status_count` symbols are decoded.
    // Symbol values: 0 = not received, 1 = received (small delta, 1 byte),
    // 2 = received (large delta, 2 bytes signed), 3 = reserved (no delta).
    let mut symbols: Vec<u8> = Vec::with_capacity(status_count);
    let mut pos = 20usize;
    while symbols.len() < status_count {
        let chunk_bytes = pkt.get(pos..pos + 2)?;
        let chunk = u16::from_be_bytes([chunk_bytes[0], chunk_bytes[1]]);
        pos += 2;
        if chunk & 0x8000 == 0 {
            // Run-length chunk: S(2) | run(13)
            let symbol = ((chunk >> 13) & 0x03) as u8;
            let run = usize::from(chunk & 0x1FFF);
            if run == 0 {
                return None; // zero-length run is malformed
            }
            for _ in 0..run.min(status_count - symbols.len()) {
                symbols.push(symbol);
            }
        } else if chunk & 0x4000 == 0 {
            // Status-vector chunk, 1-bit symbols (14 of them, MSB first).
            for i in (0..14).rev() {
                if symbols.len() >= status_count {
                    break;
                }
                symbols.push(((chunk >> i) & 0x01) as u8);
            }
        } else {
            // Status-vector chunk, 2-bit symbols (7 of them, MSB first).
            for i in (0..7).rev() {
                if symbols.len() >= status_count {
                    break;
                }
                symbols.push(((chunk >> (i * 2)) & 0x03) as u8);
            }
        }
    }

    // Consume receive deltas in symbol order.
    let mut statuses = Vec::with_capacity(status_count);
    for symbol in symbols {
        match symbol {
            1 => {
                let d = *pkt.get(pos)?;
                pos += 1;
                statuses.push(TwccStatus::Received {
                    delta_us: i32::from(d) * 250,
                });
            }
            2 => {
                let d = pkt.get(pos..pos + 2)?;
                pos += 2;
                let raw = i16::from_be_bytes([d[0], d[1]]);
                statuses.push(TwccStatus::Received {
                    delta_us: i32::from(raw) * 250,
                });
            }
            // 0 = not received; 3 = reserved/received-without-timestamp —
            // neither carries a delta, both are treated as not received.
            _ => statuses.push(TwccStatus::NotReceived),
        }
    }

    Some(TwccFeedback {
        sender_ssrc,
        media_ssrc,
        base_seq,
        reference_time_64ms,
        fb_pkt_count,
        statuses,
    })
}

// ── Compound walker ───────────────────────────────────────────────────────────

/// A bandwidth-relevant feedback message found in a compound RTCP buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BandwidthFeedback {
    /// Receiver Estimated Maximum Bitrate.
    Remb(Remb),
    /// Transport-wide congestion-control feedback.
    Twcc(TwccFeedback),
}

/// Walk a compound RTCP buffer and return every REMB and TWCC found.
///
/// Mirrors [`crate::rtcp_feedback::parse_keyframe_requests`]: sub-packets the
/// walker does not recognise (or cannot parse) are skipped, truncation stops
/// the walk.
#[must_use]
pub fn parse_bandwidth_feedback(buf: &[u8]) -> Vec<BandwidthFeedback> {
    let mut out = Vec::new();
    let mut remaining = buf;
    loop {
        if remaining.len() < 4 {
            break;
        }
        let version = (remaining[0] & 0b1100_0000) >> 6;
        if version != 2 {
            break;
        }
        let fmt = remaining[0] & 0b0001_1111;
        let pt = remaining[1];
        let words_less_one = usize::from(u16::from_be_bytes([remaining[2], remaining[3]]));
        let pkt_bytes = (words_less_one + 1) * 4;
        if pkt_bytes > remaining.len() {
            break;
        }
        let pkt = &remaining[..pkt_bytes];

        if pt == PT_PSFB && fmt == FMT_ALFB {
            if let Some(remb) = parse_remb(pkt) {
                out.push(BandwidthFeedback::Remb(remb));
            }
        } else if pt == PT_RTPFB && fmt == FMT_TWCC {
            if let Some(twcc) = parse_twcc(pkt) {
                out.push(BandwidthFeedback::Twcc(twcc));
            }
        }

        remaining = &remaining[pkt_bytes..];
    }
    out
}

// ── Publisher-facing REMB aggregation ─────────────────────────────────────────

/// Tunables for [`PublisherRembAggregator`]. [`RembAggregatorConfig::default`]
/// matches common SFU practice.
#[derive(Debug, Clone)]
pub struct RembAggregatorConfig {
    /// Floor for the emitted REMB (bps). A publisher should never be asked to
    /// drop below the lowest viable encoding; this also avoids emitting a
    /// near-zero REMB when a single subscriber's estimator briefly bottoms out.
    pub min_bps: u64,
    /// EWMA weight given to each new aggregate sample (0..1]. Smooths the
    /// min-across-subscribers target so a transient dip on one subscriber does
    /// not snap the publisher's encoder down and back up (REMB flapping).
    pub alpha: f64,
    /// Relative change (fraction of the last *emitted* value) the smoothed
    /// aggregate must move before a non-periodic emission fires. 0.10 = 10 %.
    pub hysteresis: f64,
}

impl Default for RembAggregatorConfig {
    fn default() -> Self {
        Self {
            min_bps: 100_000,
            alpha: 0.3,
            hysteresis: 0.10,
        }
    }
}

/// Per-published-track aggregation of subscriber bandwidth estimates into a
/// single REMB target for the publisher.
///
/// The slowest subscriber bounds the publisher, so the aggregate is the **MIN**
/// across the current per-subscriber estimates, floored at
/// [`RembAggregatorConfig::min_bps`] and EWMA-smoothed
/// ([`RembAggregatorConfig::alpha`]) to avoid flapping. Time is injected
/// (`now_ms`) — no clock is read here.
///
/// Emission is gated by either a hysteresis threshold on the smoothed value
/// ([`Self::update`]) or a periodic tick ([`Self::tick`]); both return
/// `Some(bitrate_bps)` only when the publisher should be sent a fresh REMB,
/// which the SFU then encodes with [`encode_remb`] and relays upstream.
#[derive(Debug, Clone)]
pub struct PublisherRembAggregator {
    cfg: RembAggregatorConfig,
    /// `subscriber → latest estimate (bps)`. Keyed by an opaque token the
    /// caller chooses (the SFU uses a participant-derived `u64`).
    estimates: std::collections::HashMap<u64, u64>,
    /// EWMA-smoothed aggregate (`None` until the first subscriber estimate).
    smoothed_bps: Option<f64>,
    /// Last value actually emitted toward the publisher (`None` until the first
    /// emission); the hysteresis comparison is against this, not the raw
    /// aggregate, so emissions are self-rate-limited.
    last_emitted_bps: Option<u64>,
}

impl Default for PublisherRembAggregator {
    fn default() -> Self {
        Self::new(RembAggregatorConfig::default())
    }
}

impl PublisherRembAggregator {
    /// Create an aggregator with the given tunables.
    #[must_use]
    pub fn new(cfg: RembAggregatorConfig) -> Self {
        Self {
            cfg,
            estimates: std::collections::HashMap::new(),
            smoothed_bps: None,
            last_emitted_bps: None,
        }
    }

    /// Record a subscriber's latest estimate, re-fold the smoothed aggregate,
    /// and return `Some(bitrate)` when the change crosses the hysteresis
    /// threshold (i.e. the publisher should be sent a fresh REMB now).
    ///
    /// The first ever sample always emits (there is no prior REMB on the wire).
    pub fn update(&mut self, subscriber: u64, estimate_bps: u64) -> Option<u64> {
        self.estimates.insert(subscriber, estimate_bps);
        self.refold();
        self.maybe_emit()
    }

    /// Forget a subscriber's estimate (they left / unsubscribed) and re-fold.
    ///
    /// Returns `Some(bitrate)` if dropping the slowest subscriber lifts the
    /// aggregate past the hysteresis threshold (the publisher can speed up).
    pub fn remove(&mut self, subscriber: u64) -> Option<u64> {
        self.estimates.remove(&subscriber)?;
        self.refold();
        self.maybe_emit()
    }

    /// Periodic-tick emission: re-emit the current smoothed aggregate
    /// unconditionally (subject only to having any subscribers), so the
    /// publisher keeps a live REMB even when subscriber feedback is steady.
    /// Returns `None` while no subscriber estimate exists yet.
    pub fn tick(&mut self) -> Option<u64> {
        let target = self.target_bps()?;
        self.last_emitted_bps = Some(target);
        Some(target)
    }

    /// The current floored, smoothed aggregate target (bps), or `None` when no
    /// subscriber estimate has been recorded.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn target_bps(&self) -> Option<u64> {
        self.smoothed_bps
            .map(|v| (v.max(0.0) as u64).max(self.cfg.min_bps))
    }

    /// Number of subscribers currently contributing an estimate.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.estimates.len()
    }

    /// Fold the raw MIN-across-subscribers into the EWMA-smoothed aggregate.
    #[allow(clippy::cast_precision_loss)]
    fn refold(&mut self) {
        let Some(raw_min) = self.estimates.values().copied().min() else {
            // No subscribers left — drop the smoothed history so a future
            // subscriber starts clean rather than inheriting a stale value.
            self.smoothed_bps = None;
            return;
        };
        let sample = raw_min as f64;
        self.smoothed_bps = Some(match self.smoothed_bps {
            None => sample,
            Some(prev) => self.cfg.alpha * sample + (1.0 - self.cfg.alpha) * prev,
        });
    }

    /// Decide whether the current target warrants an emission under hysteresis.
    fn maybe_emit(&mut self) -> Option<u64> {
        let target = self.target_bps()?;
        let emit = match self.last_emitted_bps {
            None => true, // first emission: nothing on the wire yet
            Some(last) => {
                let delta = target.abs_diff(last);
                #[allow(clippy::cast_precision_loss)]
                let rel = delta as f64 / last.max(1) as f64;
                rel >= self.cfg.hysteresis
            }
        };
        if emit {
            self.last_emitted_bps = Some(target);
            Some(target)
        } else {
            None
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
mod tests {
    use super::*;

    // ── REMB ──────────────────────────────────────────────────────────────────

    #[test]
    fn remb_encode_parse_round_trip() {
        let buf = encode_remb(0xAABB_CCDD, 1_250_000, &[0x1122_3344, 0x5566_7788]);
        // Header sanity: V=2, FMT=15, PT=206.
        assert_eq!(buf[0], 0x8F);
        assert_eq!(buf[1], 206);
        // length = 28 bytes = 7 words → words-1 = 6
        assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), 6);
        assert_eq!(buf.len(), 28);

        let remb = parse_remb(&buf).expect("round trip parse");
        assert_eq!(remb.sender_ssrc, 0xAABB_CCDD);
        assert_eq!(remb.ssrcs, vec![0x1122_3344, 0x5566_7788]);
        // 1_250_000 needs exp ≥ 3 (mantissa max 262143); 1_250_000 >> 3 = 156_250,
        // 156_250 << 3 = 1_250_000 exactly.
        assert_eq!(remb.bitrate_bps, 1_250_000);
    }

    #[test]
    fn remb_small_bitrate_uses_zero_exponent() {
        let buf = encode_remb(1, 200_000, &[42]);
        let remb = parse_remb(&buf).unwrap();
        assert_eq!(remb.bitrate_bps, 200_000);
        // exp stored in top 6 bits of byte 17
        assert_eq!(buf[17] >> 2, 0, "200k fits the 18-bit mantissa directly");
    }

    #[test]
    fn remb_large_bitrate_rounds_down_within_mantissa_precision() {
        let bps = 50_000_001u64; // needs exp=8; loses the low 8 bits
        let buf = encode_remb(1, bps, &[]);
        let remb = parse_remb(&buf).unwrap();
        assert!(remb.bitrate_bps <= bps);
        assert!(bps - remb.bitrate_bps < 256, "error bounded by 2^exp");
    }

    #[test]
    fn remb_real_world_shaped_bytes_parse() {
        // Hand-built packet mirroring what libwebrtc emits for ~2.5 Mbps,
        // one media SSRC. 2_500_000 = 0b1001100010010110100000;
        // exp=4, mantissa=156_250 (0x2625A): 156_250 << 4 = 2_500_000.
        let pkt: Vec<u8> = vec![
            0x8F, 0xCE, 0x00, 0x05, // V=2 FMT=15, PT=206, len=5 words-1 (24 bytes)
            0x12, 0x34, 0x56, 0x78, // sender SSRC
            0x00, 0x00, 0x00, 0x00, // media SSRC (0)
            b'R', b'E', b'M', b'B', // identifier
            0x01, // num ssrc = 1
            0x12, // exp=4 (000100<<2) | mantissa[17:16]=0b10
            0x62, 0x5A, // mantissa low 16 bits
            0xDE, 0xAD, 0xBE, 0xEF, // ssrc[0]
        ];
        let remb = parse_remb(&pkt).expect("real-world REMB must parse");
        assert_eq!(remb.bitrate_bps, 2_500_000);
        assert_eq!(remb.ssrcs, vec![0xDEAD_BEEF]);
    }

    #[test]
    fn remb_rejects_wrong_identifier() {
        let mut buf = encode_remb(1, 500_000, &[2]);
        buf[12..16].copy_from_slice(b"GOOG"); // not REMB (e.g. goog-remb sibling)
        assert!(parse_remb(&buf).is_none());
    }

    #[test]
    fn remb_rejects_truncated_ssrc_list() {
        let mut buf = encode_remb(1, 500_000, &[2]);
        buf.truncate(22); // chop into the SSRC list
        assert!(parse_remb(&buf).is_none());
    }

    #[test]
    fn remb_rejects_short_or_wrong_type_packets() {
        assert!(parse_remb(&[]).is_none());
        assert!(parse_remb(&[0x8F, 0xCE, 0x00]).is_none());
        // PLI (PT=206, FMT=1) is not a REMB.
        let pli = [
            0x81, 0xCE, 0x00, 0x02, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(parse_remb(&pli).is_none());
    }

    // ── TWCC ──────────────────────────────────────────────────────────────────

    /// Build a TWCC packet from raw chunk/delta bytes (pads to 32-bit words).
    fn build_twcc(
        base_seq: u16,
        status_count: u16,
        ref_time: i32,
        fb_count: u8,
        chunks: &[u16],
        deltas: &[u8],
    ) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&0x0102_0304u32.to_be_bytes()); // sender ssrc
        body.extend_from_slice(&0x0506_0708u32.to_be_bytes()); // media ssrc
        body.extend_from_slice(&base_seq.to_be_bytes());
        body.extend_from_slice(&status_count.to_be_bytes());
        let rt = (ref_time & 0x00FF_FFFF) as u32;
        body.extend_from_slice(&[(rt >> 16) as u8, (rt >> 8) as u8, rt as u8]);
        body.push(fb_count);
        for c in chunks {
            body.extend_from_slice(&c.to_be_bytes());
        }
        body.extend_from_slice(deltas);
        while (body.len() + 4) % 4 != 0 {
            body.push(0); // pad to word boundary
        }
        let words_less_one = ((body.len() + 4) / 4 - 1) as u16;
        let mut pkt = vec![0x8F, 0xCD];
        pkt.extend_from_slice(&words_less_one.to_be_bytes());
        pkt.extend_from_slice(&body);
        pkt
    }

    #[test]
    fn twcc_run_length_all_received_small_deltas() {
        // 4 packets, all received with small deltas: run-length chunk
        // S=1, run=4 → 0b0_01_0000000000100 = 0x2004.
        let pkt = build_twcc(100, 4, 5, 0, &[0x2004], &[4, 8, 12, 16]);
        let fb = parse_twcc(&pkt).expect("must parse");
        assert_eq!(fb.base_seq, 100);
        assert_eq!(fb.reference_time_64ms, 5);
        assert_eq!(fb.statuses.len(), 4);
        assert_eq!(fb.statuses[0], TwccStatus::Received { delta_us: 1000 });
        assert_eq!(fb.statuses[3], TwccStatus::Received { delta_us: 4000 });

        let s = fb.summary();
        assert_eq!(s.received, 4);
        assert_eq!(s.lost, 0);
        // halves: [1000,2000] vs [3000,4000] → trend = (7000-3000)/2 = 2000
        assert_eq!(s.delay_trend_us, 2000);
    }

    #[test]
    fn twcc_run_length_not_received_counts_loss() {
        // 3 received (deltas), then run of 5 not-received:
        // chunk1: S=1 run=3 → 0x2003; chunk2: S=0 run=5 → 0x0005.
        let pkt = build_twcc(7, 8, 0, 1, &[0x2003, 0x0005], &[1, 1, 1]);
        let fb = parse_twcc(&pkt).expect("must parse");
        let s = fb.summary();
        assert_eq!(s.received, 3);
        assert_eq!(s.lost, 5);
        assert_eq!(s.delay_trend_us, 0, "under 4 received → no trend signal");
    }

    #[test]
    fn twcc_one_bit_status_vector() {
        // Status-vector chunk, 1-bit symbols: 0b10_10101010101010 = 0xAAAA
        // (type=1, S=0, then symbols 1,0,1,0,1,0,1,0,1,0,1,0,1,0).
        // status_count=14: 7 received → 7 small deltas.
        let deltas = [2u8; 7];
        let pkt = build_twcc(0, 14, -1, 2, &[0xAAAA], &deltas);
        let fb = parse_twcc(&pkt).expect("must parse");
        assert_eq!(fb.reference_time_64ms, -1, "24-bit sign extension");
        let s = fb.summary();
        assert_eq!(s.received, 7);
        assert_eq!(s.lost, 7);
        assert_eq!(s.delay_trend_us, 0, "constant deltas → flat trend");
    }

    #[test]
    fn twcc_two_bit_status_vector_with_large_delta() {
        // Status-vector chunk, 2-bit symbols: type=1, S=1, symbols
        // [1, 2, 0, 1, 0, 0, 0] → 0b11_01_10_00_01_00_00_00 = 0xD840.
        // status_count=7. Deltas: small(1B), large(2B signed), small(1B).
        let deltas = [40u8, 0xFF, 0x38, 8]; // large = -200 → -50_000 µs
        let pkt = build_twcc(500, 7, 100, 3, &[0xD840], &deltas);
        let fb = parse_twcc(&pkt).expect("must parse");
        assert_eq!(fb.statuses[0], TwccStatus::Received { delta_us: 10_000 });
        assert_eq!(fb.statuses[1], TwccStatus::Received { delta_us: -50_000 });
        assert_eq!(fb.statuses[2], TwccStatus::NotReceived);
        assert_eq!(fb.statuses[3], TwccStatus::Received { delta_us: 2000 });
        assert_eq!(fb.statuses[4], TwccStatus::NotReceived);
        let s = fb.summary();
        assert_eq!(s.received, 3);
        assert_eq!(s.lost, 4);
    }

    #[test]
    fn twcc_real_world_shaped_mixed_chunks() {
        // Mirrors a libwebrtc-style report: a 1-bit vector chunk followed by a
        // run-length of received. 14 + 6 = 20 statuses, base 0x1234.
        // chunk1 = 0xBFFE → 1-bit vector: 1111111111111 0 (13 recv, 1 lost)
        // chunk2 = 0x2006 → run-length S=1 run=6.
        let deltas: Vec<u8> = (1..=19).collect(); // 13 + 6 received
        let pkt = build_twcc(0x1234, 20, 1023, 9, &[0xBFFE, 0x2006], &deltas);
        let fb = parse_twcc(&pkt).expect("must parse");
        assert_eq!(fb.base_seq, 0x1234);
        assert_eq!(fb.fb_pkt_count, 9);
        let s = fb.summary();
        assert_eq!(s.received, 19);
        assert_eq!(s.lost, 1);
        assert!(s.delay_trend_us > 0, "monotonically growing deltas → rising");
    }

    #[test]
    fn twcc_symbol3_treated_as_not_received_no_delta() {
        // Run-length chunk S=3 run=4 → 0b0_11_0000000000100 = 0x6004.
        // No deltas follow symbol 3.
        let pkt = build_twcc(1, 4, 0, 0, &[0x6004], &[]);
        let fb = parse_twcc(&pkt).expect("must parse");
        assert!(fb.statuses.iter().all(|s| *s == TwccStatus::NotReceived));
    }

    #[test]
    fn twcc_malformed_inputs_are_rejected() {
        // Too short.
        assert!(parse_twcc(&[0x8F, 0xCD, 0x00, 0x01]).is_none());
        // Missing chunk bytes: status_count says 4 but the chunk was cut off.
        let mut pkt = build_twcc(0, 4, 0, 0, &[0x2004], &[1, 2, 3, 4]);
        pkt.truncate(20);
        assert!(parse_twcc(&pkt).is_none());
        // Missing delta bytes: 4 received but only 2 deltas present.
        let pkt = build_twcc(0, 4, 0, 0, &[0x2004], &[1, 2]);
        assert!(parse_twcc(&pkt).is_none());
        // Zero-length run chunk is malformed.
        let pkt = build_twcc(0, 4, 0, 0, &[0x2000, 0x2004], &[1, 2, 3, 4]);
        assert!(parse_twcc(&pkt).is_none());
        // Wrong PT (PSFB) is not TWCC.
        let mut pkt = build_twcc(0, 1, 0, 0, &[0x2001], &[1]);
        pkt[1] = 206;
        assert!(parse_twcc(&pkt).is_none());
    }

    // ── Compound walker ───────────────────────────────────────────────────────

    #[test]
    fn walker_extracts_remb_and_twcc_skipping_others() {
        let mut buf = Vec::new();
        // Leading receiver report (PT=201, RC=0, len=1 word: header+ssrc).
        buf.extend_from_slice(&[0x80, 201, 0x00, 0x01, 0, 0, 0, 9]);
        buf.extend_from_slice(&encode_remb(1, 750_000, &[2]));
        buf.extend_from_slice(&build_twcc(10, 2, 0, 0, &[0x2002], &[4, 4]));

        let found = parse_bandwidth_feedback(&buf);
        assert_eq!(found.len(), 2);
        match &found[0] {
            BandwidthFeedback::Remb(r) => assert_eq!(r.bitrate_bps, 750_000),
            other @ BandwidthFeedback::Twcc(_) => panic!("expected REMB first, got {other:?}"),
        }
        match &found[1] {
            BandwidthFeedback::Twcc(t) => assert_eq!(t.summary().received, 2),
            other @ BandwidthFeedback::Remb(_) => panic!("expected TWCC second, got {other:?}"),
        }
    }

    #[test]
    fn walker_stops_on_truncated_or_garbage_input() {
        assert!(parse_bandwidth_feedback(&[]).is_empty());
        assert!(parse_bandwidth_feedback(&[0x8F]).is_empty());
        // Version != 2.
        assert!(parse_bandwidth_feedback(&[0x4F, 0xCD, 0x00, 0x01, 0, 0, 0, 0]).is_empty());
        // Length field exceeds buffer.
        assert!(parse_bandwidth_feedback(&[0x8F, 0xCD, 0x00, 0x20, 0, 0, 0, 0]).is_empty());
        // A malformed REMB inside an otherwise valid envelope is skipped.
        let mut buf = encode_remb(1, 500_000, &[2]);
        buf[12] = b'X';
        assert!(parse_bandwidth_feedback(&buf).is_empty());
    }

    // ── PublisherRembAggregator ─────────────────────────────────────────────────

    /// A non-smoothing, no-floor config so tests can assert exact MIN values.
    fn raw_agg() -> PublisherRembAggregator {
        PublisherRembAggregator::new(RembAggregatorConfig {
            min_bps: 1,
            alpha: 1.0,
            hysteresis: 0.10,
        })
    }

    #[test]
    fn aggregate_is_min_across_subscribers() {
        let mut a = raw_agg();
        // First subscriber: first sample always emits.
        assert_eq!(a.update(1, 800_000), Some(800_000));
        // A faster second subscriber does not raise the floor; the slower one
        // (800k) still bounds the publisher → no change → no emission.
        assert_eq!(a.update(2, 2_000_000), None);
        assert_eq!(a.target_bps(), Some(800_000));
        // The slow subscriber gets even slower → emit the new, lower MIN.
        assert_eq!(a.update(1, 400_000), Some(400_000));
    }

    #[test]
    fn hysteresis_suppresses_small_changes() {
        let mut a = raw_agg();
        assert_eq!(a.update(1, 1_000_000), Some(1_000_000));
        // 5 % drop (< 10 % threshold) → suppressed.
        assert_eq!(a.update(1, 950_000), None);
        // Now 12 % below the *last emitted* 1 Mbps → fires.
        assert_eq!(a.update(1, 880_000), Some(880_000));
        // Hysteresis is measured against the last emitted value, so a further
        // small wiggle around 880k is again suppressed.
        assert_eq!(a.update(1, 860_000), None);
    }

    #[test]
    fn floor_clamps_low_aggregate() {
        let mut a = PublisherRembAggregator::new(RembAggregatorConfig {
            min_bps: 200_000,
            alpha: 1.0,
            hysteresis: 0.10,
        });
        // A subscriber estimate below the floor is clamped up to the floor.
        assert_eq!(a.update(1, 50_000), Some(200_000));
    }

    #[test]
    fn ewma_smooths_a_transient_dip() {
        // Default alpha = 0.3: a one-shot dip moves the aggregate only partway.
        let mut a = PublisherRembAggregator::default();
        assert_eq!(a.update(1, 1_000_000), Some(1_000_000)); // first sample taken as-is
        // Dip to 400k: 0.3×400k + 0.7×1M = 820k — a 18 % drop, past hysteresis.
        assert_eq!(a.update(1, 400_000), Some(820_000));
        // Recover to 1M: 0.3×1M + 0.7×820k = 874k — a smoothed +6.6 % move, below
        // the 10 % threshold, so it is suppressed (no snap-back REMB flap).
        assert_eq!(a.update(1, 1_000_000), None);
        assert_eq!(a.target_bps(), Some(874_000));
        // A second clean sample keeps climbing: 0.3×1M + 0.7×874k = 911.8k,
        // now +11 % above the last emitted 820k → emits.
        assert_eq!(a.update(1, 1_000_000), Some(911_800));
    }

    #[test]
    fn tick_re_emits_steady_aggregate() {
        let mut a = raw_agg();
        assert_eq!(a.tick(), None, "no subscribers yet → nothing to emit");
        assert_eq!(a.update(1, 600_000), Some(600_000));
        // Steady feedback would not move the aggregate, but the periodic tick
        // re-emits the current target so the publisher keeps a live REMB.
        assert_eq!(a.tick(), Some(600_000));
        assert_eq!(a.tick(), Some(600_000));
    }

    #[test]
    fn removing_slowest_subscriber_lifts_aggregate() {
        let mut a = raw_agg();
        assert_eq!(a.update(1, 300_000), Some(300_000)); // slow subscriber
        assert_eq!(a.update(2, 2_000_000), None); // fast subscriber, MIN unchanged
        assert_eq!(a.subscriber_count(), 2);
        // Slow subscriber leaves → MIN jumps to the fast one → emit.
        assert_eq!(a.remove(1), Some(2_000_000));
        // Removing an unknown subscriber is a no-op.
        assert_eq!(a.remove(99), None);
        // Removing the last subscriber clears the aggregate.
        assert_eq!(a.remove(2), None);
        assert_eq!(a.target_bps(), None);
        assert_eq!(a.subscriber_count(), 0);
    }
}
