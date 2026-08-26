use super::*;

fn nal(start4: bool, header: u8, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    if start4 {
        v.extend_from_slice(&[0, 0, 0, 1]);
    } else {
        v.extend_from_slice(&[0, 0, 1]);
    }
    v.push(header);
    v.extend_from_slice(body);
    v
}

#[test]
fn split_single_4byte_nal() {
    let buf = nal(true, 0x65, &[0xAA, 0xBB]);
    let nals = split_annex_b(&buf);
    assert_eq!(nals.len(), 1);
    assert_eq!(nals[0], &[0x65, 0xAA, 0xBB]);
}

#[test]
fn split_multiple_nals_mixed_start_codes() {
    // 4-byte SPS, 3-byte PPS, 4-byte IDR.
    let mut buf = nal(true, 0x67, &[0x42, 0x00]);
    buf.extend_from_slice(&nal(false, 0x68, &[0xCE]));
    buf.extend_from_slice(&nal(true, 0x65, &[0x01, 0x02, 0x03]));
    let nals = split_annex_b(&buf);
    assert_eq!(nals.len(), 3, "got {nals:?}");
    assert_eq!(nals[0][0] & 0x1F, 7); // SPS
    assert_eq!(nals[1][0] & 0x1F, 8); // PPS
    assert_eq!(nals[2][0] & 0x1F, 5); // IDR
    assert_eq!(nals[2], &[0x65, 0x01, 0x02, 0x03]);
}

#[test]
fn split_empty_or_no_start_code() {
    assert!(split_annex_b(&[]).is_empty());
    assert!(split_annex_b(&[0x65, 0xAA]).is_empty());
}

#[test]
fn classify_recognizes_sps_pps_idr() {
    assert_eq!(classify_nal(0x67), NalClass::Sps);
    assert_eq!(classify_nal(0x68), NalClass::Pps);
    assert_eq!(classify_nal(0x65), NalClass::IdrSlice);
    assert_eq!(classify_nal(0x61), NalClass::Other(1));
    assert_eq!(classify_nal(0x06), NalClass::Other(6)); // SEI
}

#[test]
fn keyframe_detection() {
    let mut kf = nal(true, 0x67, &[0x42]); // SPS
    kf.extend_from_slice(&nal(true, 0x68, &[0xCE])); // PPS
    kf.extend_from_slice(&nal(true, 0x65, &[0x01])); // IDR
    assert!(au_is_keyframe(&kf));

    let inter = nal(true, 0x61, &[0x01]); // non-IDR slice
    assert!(!au_is_keyframe(&inter));
}

#[test]
fn avcc_roundtrips_lengths_and_payload() {
    // Annex-B AU: SPS(2 body) + IDR(3 body).
    let mut au = nal(true, 0x67, &[0x42, 0x00]);
    au.extend_from_slice(&nal(true, 0x65, &[0x01, 0x02, 0x03]));

    let avcc = annex_b_to_avcc(&au);
    // First NAL: len = 1(header)+2 = 3 → 4-byte BE prefix.
    assert_eq!(&avcc[0..4], &3u32.to_be_bytes());
    assert_eq!(&avcc[4..7], &[0x67, 0x42, 0x00]);
    // Second NAL: len = 1+3 = 4.
    assert_eq!(&avcc[7..11], &4u32.to_be_bytes());
    assert_eq!(&avcc[11..15], &[0x65, 0x01, 0x02, 0x03]);
    assert_eq!(avcc.len(), 4 + 3 + 4 + 4);
}

#[test]
fn avcc_uses_4byte_start_code_const() {
    // Guard against ANNEX_B_START_CODE drift breaking the bridge contract.
    assert_eq!(
        crate::depacketize::ANNEX_B_START_CODE,
        [0x00, 0x00, 0x00, 0x01]
    );
}

#[test]
fn logging_sink_counts() {
    let mut s = LoggingSink::default();
    s.on_video_au(Bytes::from_static(&[0, 0, 0, 1, 0x65, 0xAA]), 9000)
        .unwrap();
    s.on_audio(Bytes::from_static(&[1, 2, 3]), 9000).unwrap();
    assert_eq!(s.video_aus, 1);
    assert_eq!(s.video_bytes, 6);
    assert_eq!(s.audio_packets, 1);
}

// --------------------- avcC synthesis ---------------------

#[test]
fn avc_decoder_config_layout_is_well_formed() {
    // SPS NAL: header 0x67, profile_idc=0x42, constraint=0x00, level=0x1F.
    let sps = [0x67u8, 0x42, 0x00, 0x1F, 0xAB, 0xCD];
    let pps = [0x68u8, 0xCE, 0x3C];
    let cfg = build_avc_decoder_config(&sps, &pps).expect("config");

    assert_eq!(cfg[0], 1, "configurationVersion");
    assert_eq!(cfg[1], 0x42, "AVCProfileIndication lifted from SPS");
    assert_eq!(cfg[2], 0x00, "profile_compatibility lifted from SPS");
    assert_eq!(cfg[3], 0x1F, "AVCLevelIndication lifted from SPS");
    assert_eq!(cfg[4] & 0x03, 0x03, "lengthSizeMinusOne = 3 (4-byte NALs)");
    assert_eq!(cfg[5] & 0x1F, 1, "numOfSequenceParameterSets = 1");
    let sps_len = u16::from_be_bytes([cfg[6], cfg[7]]) as usize;
    assert_eq!(sps_len, sps.len());
    assert_eq!(&cfg[8..8 + sps_len], &sps);
    let p = 8 + sps_len;
    assert_eq!(cfg[p], 1, "numOfPictureParameterSets = 1");
    let pps_len = u16::from_be_bytes([cfg[p + 1], cfg[p + 2]]) as usize;
    assert_eq!(pps_len, pps.len());
    assert_eq!(&cfg[p + 3..p + 3 + pps_len], &pps);
}

#[test]
fn avc_decoder_config_rejects_short_sps() {
    // Fewer than 4 bytes → no profile/level triplet → None.
    assert!(build_avc_decoder_config(&[0x67, 0x42], &[0x68, 0xCE]).is_none());
}

#[test]
fn synthesized_avcc_is_accepted_by_real_muxer() {
    // The whole point of synthesizing the record is that the *real*
    // FlvToTsConverter parses it back. Wrap it as an FLV AVC sequence header
    // and confirm the muxer becomes ready (no fork, no re-mux).
    let sps = [0x67u8, 0x42, 0x00, 0x1F, 0xAB];
    let pps = [0x68u8, 0xCE];
    let avcc = build_avc_decoder_config(&sps, &pps).unwrap();
    let mut tag = vec![0x17u8, 0x00, 0x00, 0x00, 0x00];
    tag.extend_from_slice(&avcc);

    let mut conv = aero_live_hls::FlvToTsConverter::new();
    assert!(!conv.is_ready());
    conv.push_video_tag(&tag, 0)
        .expect("muxer accepts our avcC");
    assert!(
        conv.is_ready(),
        "muxer should be ready after the seq header"
    );
}

// ------------------ HlsSink (channel level) ------------------

/// Build a minimal Annex-B access unit from `(nal_header, body)` pairs.
fn annex_b_au(nals: &[(u8, &[u8])]) -> Bytes {
    let mut out = BytesMut::new();
    for (header, body) in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.put_u8(*header);
        out.extend_from_slice(body);
    }
    out.freeze()
}

#[test]
fn hls_sink_emits_segment_on_second_keyframe() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut sink = HlsSink::new(tx);

    // Keyframe AU #1: SPS + PPS + IDR. Opens the first (still-open) segment;
    // nothing cut yet.
    let kf1 = annex_b_au(&[
        (0x67, &[0x42, 0x00, 0x1F]),
        (0x68, &[0xCE]),
        (0x65, &[0x10, 0x20, 0x30]),
    ]);
    sink.on_video_au(kf1, 0).unwrap();
    assert!(
        rx.try_recv().is_err(),
        "first keyframe must not cut a segment"
    );

    // A P-frame extends the open segment.
    let p = annex_b_au(&[(0x41, &[0x11, 0x22])]);
    sink.on_video_au(p, 3_000).unwrap();

    // Keyframe AU #2 closes the first segment (it has TS data) before muxing.
    let kf2 = annex_b_au(&[
        (0x67, &[0x42, 0x00, 0x1F]),
        (0x68, &[0xCE]),
        (0x65, &[0x40, 0x50]),
    ]);
    sink.on_video_au(kf2, 6_000).unwrap();

    let seg = rx.try_recv().expect("one segment cut at the 2nd keyframe");
    assert_eq!(sink.segments_emitted(), 1);
    assert_eq!(sink.video_aus(), 3);
    // Real MPEG-TS: starts with PAT then PMT (each a 188-byte sync packet).
    assert_eq!(seg.bytes[0], 0x47, "TS sync byte (PAT)");
    assert_eq!(seg.bytes[188], 0x47, "TS sync byte (PMT)");
    assert_eq!(seg.bytes.len() % 188, 0, "whole TS packets");
    // Duration spans PTS 0 → 6000 ticks = 1/15 s, strictly positive.
    let expected = 6_000.0f32 / 90_000.0;
    assert!(
        (seg.duration_secs - expected).abs() < 1e-4,
        "got {}",
        seg.duration_secs
    );
}

#[test]
fn hls_sink_drops_inter_frames_before_first_keyframe() {
    // Mirrors FlvToTsConverter's own rule: no segment data until a keyframe.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut sink = HlsSink::new(tx);
    sink.on_video_au(annex_b_au(&[(0x41, &[0x01])]), 0).unwrap();
    sink.on_video_au(annex_b_au(&[(0x41, &[0x02])]), 3_000)
        .unwrap();
    assert!(
        rx.try_recv().is_err(),
        "no segment without a leading keyframe"
    );
    assert_eq!(sink.segments_emitted(), 0);
}

#[tokio::test]
async fn hls_sink_writer_pair_persists_files() {
    let dir = crate::testutil::TempDir::new().unwrap();
    let stream_dir = dir.path().join("stream-xyz");
    let (mut sink, writer) = hls_sink(stream_dir.clone(), 2).await.unwrap();
    let writer_task = tokio::spawn(writer.run());

    // Two GOPs so the first segment is cut at the second keyframe; dropping
    // the sink flushes the trailing one and finalizes the manifest.
    let kf = |pts: u64| {
        let au = annex_b_au(&[
            (0x67, &[0x42, 0x00, 0x1F]),
            (0x68, &[0xCE]),
            (0x65, &[0xAA, 0xBB, 0xCC]),
        ]);
        (au, pts)
    };
    let (a, ap) = kf(0);
    sink.on_video_au(a, ap).unwrap();
    sink.on_video_au(annex_b_au(&[(0x41, &[0x01])]), 3_000)
        .unwrap();
    let (b, bp) = kf(6_000);
    sink.on_video_au(b, bp).unwrap();
    drop(sink); // closes the channel → writer finalizes

    let segments = writer_task.await.unwrap().unwrap();
    assert!(segments >= 1, "at least the first GOP segment persisted");
    assert!(stream_dir.join("0.ts").exists(), "0.ts on disk");
    let manifest = std::fs::read_to_string(stream_dir.join("index.m3u8")).unwrap();
    assert!(manifest.contains("#EXTM3U"));
    assert!(manifest.contains("#EXTINF"));
    assert!(manifest.contains("0.ts"));
    assert!(
        manifest.contains("#EXT-X-ENDLIST"),
        "finalized on sink drop"
    );
}
