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
    assert!(
        s.delay_trend_us > 0,
        "monotonically growing deltas → rising"
    );
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
