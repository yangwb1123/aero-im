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
    let reference_time_64ms =
        (i32::from(pkt[16]) << 24 | i32::from(pkt[17]) << 16 | i32::from(pkt[18]) << 8) >> 8;
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
mod tests;
