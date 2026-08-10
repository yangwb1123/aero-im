//! Isolation regression tests — single-UDP-loop isolation (B5 direction).
//!
//! Pins the isolation invariant (requirements spec
//! `docs/requirements/2026-08-07-aero-live-srt-b5-udp-loop-isolation.req.md`,
//! design `docs/design/2026-08-07-aero-live-srt-b5-udp-loop-isolation.design.md`):
//! the per-datagram path (`run_listener` select loop → `handle_datagram`
//! streaming arm → [`SrtSession::feed_packet`] → `pump`) performs **zero**
//! audit/outbox/relay I/O; backend (DB) awaits exist only at session start
//! (`resolve`) and end (`finalize`) — amortized per session, never per
//! datagram.
//!
//! Structural proof (by construction): `SrtSession` holds no repo/backend
//! handle and `pump` is synchronous; `feed_packet`'s only await is the
//! `LocalFs` `HlsWriter::push_segment`. This file adds behavioral pins:
//!
//! - **T1** `streaming_peer_never_awaits_backend` — streaming datagrams never
//!   touch the backend (call counters + 100ms latency bound vs a 500ms
//!   stalled backend) and HLS segments keep flowing.
//! - **T2** `relay_down_at_session_end_keeps_streaming` — a stalled + failing
//!   `finalize` (B5 connector terminal-failure simulation) stays contained at
//!   the session boundary; the loop survives and a second peer still streams.
//! - **T3** `relay_down_at_session_start_drops_only_that_peer` — a failing
//!   `resolve` drops only that peer's session attempt; the listener keeps
//!   serving other peers.
//! - **T4** `cargo_toml_stays_audit_and_relay_free` — static dependency guard
//!   (allowlist equality + `package =` rename scan), the in-crate half of the
//!   double pin with `scripts/dependency-check.sh:59`.
//! - **B1** `run_until_cancelled_boot_fails_open_without_audit_provisioning`
//!   (DB-gated, `#[ignore]`) — the full boot path accepts a handshake + TS
//!   feed with zero audit provisioning and marks live → ended.
//!
//! B5 enqueue contract (R1.2, contract-only — no worker is built here): any
//! future audit enqueue belongs ONLY at the session start/end boundaries via
//! non-blocking `try_send` into a bounded `mpsc` (`moderation_bot` 512 pattern)
//! consumed by a worker spawned outside `run_listener`; never in
//! feed/pump. T1's per-datagram `calls == 0` and T4 pin the boundary.

use super::*;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Helpers copied verbatim from tests.rs (canonical locations noted; keep in
// lockstep — a semantic drift would weaken the isolation assertions, F7).
// ---------------------------------------------------------------------------

/// Minimal 188-byte payload-only TS packet with the given PID/PUSI.
/// Canonical: `src/tests.rs` `ts_packet`.
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
/// Canonical: `src/tests.rs` `video_pes`.
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
/// Canonical: `src/tests.rs` `pat`.
fn pat() -> Vec<u8> {
    // pointer(0) table_id(0) B0 len.. tsid version sec last prog=1 pmt_pid
    let mut s = vec![0x00, 0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00];
    s.extend_from_slice(&1u16.to_be_bytes());
    s.extend_from_slice(&(0xE000u16 | 0x1000).to_be_bytes());
    s.extend_from_slice(&[0, 0, 0, 0]); // CRC (ignored)
    s
}

/// Canonical: `src/tests.rs` `pmt`.
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

/// Build a full SRT data packet (header + payload) with the given `seq_no`,
/// KK flag, and payload bytes. Canonical: `src/tests.rs` `make_data_packet`.
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

/// Serialize an SRT control datagram carrying `handshake`.
/// Canonical: `src/tests.rs` `handshake_datagram`.
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

/// `HSv5` INDUCTION with the given caller socket id.
/// Canonical: `src/tests.rs` `listener_test_induction`.
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

/// `HSv5` CONCLUSION in **clear mode** (no KeyMaterial): `HsReq` + `StreamId`
/// only. The no-passphrase listener's `negotiate_crypto` accepts a conclusion
/// iff `encryption_field == HS_ENC_CLEAR` and no KM extensions are present
/// (protocol.rs `negotiate_crypto`), so this drives the real established
/// branch. Canonical sibling: `src/tests.rs`
/// `listener_test_encrypted_conclusion` (this is its de-KM variant, R9).
fn listener_test_clear_conclusion(caller_socket_id: u32, cookie: u32, stream_id: &str) -> Vec<u8> {
    handshake_datagram(
        &Handshake {
            version: protocol::SRT_VERSION_HSV5,
            encryption_field: protocol::HS_ENC_CLEAR,
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
                protocol::HsExtension::StreamId(stream_id.to_string()),
            ],
            ..Default::default()
        },
        1,
    )
}

/// A bare 16-byte SRT SHUTDOWN control header (real SRT SHUTDOWN has no CIF
/// body; `is_shutdown` only requires a parseable control header).
/// Canonical pattern: `src/tests.rs`
/// `is_shutdown_detects_only_shutdown_control`.
fn shutdown_control_datagram(dest_socket_id: u32) -> Vec<u8> {
    SrtHeader {
        kind: PacketKind::Control {
            control_type: ControlType::Shutdown,
            subtype: 0,
            type_specific: 0,
        },
        timestamp: 0,
        dest_socket_id,
    }
    .to_bytes()
    .to_vec()
}

// ---------------------------------------------------------------------------
// Instrumented backend (B5 relay-down in-crate simulation).
// ---------------------------------------------------------------------------

/// `SessionBackend` with call counters + stall/error injection, standing in
/// for the B5 connector/provisioning side (B5-2 422/403/backoff semantics map
/// in-crate to a stalled/erroring backend at the only two points the B5
/// enqueue contract permits: session start (`resolve`) and end (`finalize`)).
struct CountingBackend {
    stream_id: ulid::Ulid,
    /// Pre-built `SrtSession` (HLS writer rooted in a tempdir); taken by
    /// `resolve`. A single slot serves exactly one resolve (R5).
    session: Mutex<Option<SrtSession>>,
    resolve_calls: AtomicUsize,
    finalize_calls: AtomicUsize,
    /// Whole-seam counter (resolve + finalize + any future method): the
    /// per-datagram `calls == 0` assertion is stronger than the two separate
    /// counters — a future default-bodied seam method (e.g.
    /// `async fn audit(&self) {}`) is invisible to them but visible here.
    calls: AtomicUsize,
    /// When non-zero, `resolve` stalls this long first (T1's 500ms; slow
    /// DB/provisioning).
    resolve_delay: Duration,
    /// true → `resolve` returns Err (T3; provisioning/DB refusal).
    resolve_error: bool,
    /// When non-zero, `finalize` stalls this long first (T2's 500ms;
    /// connector terminal failure).
    finalize_delay: Duration,
    /// true → `finalize` runs `session.finish()` against a writer whose root
    /// directory the test has already deleted (T2 step 3, R7): the internal
    /// error is swallowed, mirroring production `finalize_session`'s
    /// warn-swallow (it also stands in for the `mark_ended` failure branch —
    /// same containment shape). The writer has no sticky error state and
    /// opens/drops file handles per call, so deleting the dir makes the next
    /// `File::create` fail deterministically.
    finalize_broken_writer: bool,
}

impl Default for CountingBackend {
    fn default() -> Self {
        Self {
            stream_id: ulid::Ulid::new(),
            session: Mutex::new(None),
            resolve_calls: AtomicUsize::new(0),
            finalize_calls: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            resolve_delay: Duration::ZERO,
            resolve_error: false,
            finalize_delay: Duration::ZERO,
            finalize_broken_writer: false,
        }
    }
}

#[async_trait]
impl SessionBackend for CountingBackend {
    async fn resolve(&self, stream_key: &str) -> LiveResult<(ulid::Ulid, SrtSession)> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.resolve_calls.fetch_add(1, Ordering::SeqCst);
        if !self.resolve_delay.is_zero() {
            tokio::time::sleep(self.resolve_delay).await;
        }
        if self.resolve_error {
            return Err(LiveError::UnknownStreamKey(stream_key.to_string()));
        }
        let session = self
            .session
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| LiveError::Protocol("test session already consumed".into()))?;
        Ok((self.stream_id, session))
    }

    async fn finalize(&self, mut session: SrtSession, _stream_id: Option<ulid::Ulid>) {
        // Mirror production `finalize_session`'s unconditional gauge decrement
        // (the established arm bumped it) so hermetic tests keep the
        // process-wide counter balanced for the concurrent metrics test.
        metrics::SessionCounter::removed();
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.finalize_calls.fetch_add(1, Ordering::SeqCst);
        if !self.finalize_delay.is_zero() {
            tokio::time::sleep(self.finalize_delay).await;
        }
        if self.finalize_broken_writer {
            // HLS writer root deleted by the test → `finish()` fails internally
            // → swallowed (production `finalize_session` warn-swallow mirror).
            let _ = session.finish().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Direct-drive harness (tests.rs:459 pattern: private `handle_datagram` +
// real loopback UDP, zero DB).
// ---------------------------------------------------------------------------

/// Drive the full `HSv5` handshake (INDUCTION → clear CONCLUSION) through
/// `handle_datagram`, returning the peer's source address. The conclusion
/// datagram legitimately pays `backend.resolve` (the allowed per-session slow
/// point, F6) — it is intentionally NOT wrapped in the per-datagram bound.
async fn establish_peer(
    ingest: &SrtIngest,
    server: &tokio::net::UdpSocket,
    caller: &tokio::net::UdpSocket,
    backend: &impl SessionBackend,
    peers: &mut HashMap<SocketAddr, PeerState>,
    caller_socket_id: u32,
    stream_key: &str,
) -> SocketAddr {
    let peer = caller.local_addr().unwrap();
    handle_datagram(
        ingest,
        server,
        backend,
        peers,
        peer,
        &listener_test_induction(caller_socket_id),
    )
    .await
    .expect("induction must be accepted");
    let mut reply = [0u8; 2048];
    let (reply_len, _) = tokio::time::timeout(Duration::from_secs(1), caller.recv_from(&mut reply))
        .await
        .expect("induction response timed out")
        .expect("recv failed");
    let induction = Handshake::parse(&reply[protocol::SRT_HEADER_LEN..reply_len]).unwrap();

    handle_datagram(
        ingest,
        server,
        backend,
        peers,
        peer,
        &listener_test_clear_conclusion(caller_socket_id, induction.syn_cookie, stream_key),
    )
    .await
    .expect("clear conclusion must be accepted");
    let (reply_len, _) = tokio::time::timeout(Duration::from_secs(1), caller.recv_from(&mut reply))
        .await
        .expect("conclusion response timed out")
        .expect("recv failed");
    let agreement = Handshake::parse(&reply[protocol::SRT_HEADER_LEN..reply_len]).unwrap();
    assert_eq!(
        agreement.handshake_type,
        protocol::HandshakeType::Conclusion,
        "listener must answer the conclusion with an HSRSP"
    );
    peer
}

/// One MPEG-TS payload for data-packet index `i`: PAT, PMT, then video PES
/// with an IDR keyframe (NAL type 5) at 1-indexed packets 8/16/24/32
/// (0-indexed 7/15/23/31) to trigger keyframe-aligned segment cuts.
/// Workload invariant (0.3 ruling 3): 4 IDR cuts → at most 5 segments <
/// `LIVE_WINDOW_SEGMENTS` = 6, so no window eviction and the `.ts` file count
/// is monotonic.
fn ts_payload_for(i: usize) -> Vec<u8> {
    match i {
        0 => ts_packet(0x0000, true, &pat()),
        1 => ts_packet(0x1000, true, &pmt()),
        i if i % 8 == 7 => ts_packet(0x0100, true, &video_pes(&[5])), // IDR keyframe
        _ => ts_packet(0x0100, true, &video_pes(&[1])),
    }
}

/// Feed one TS data packet through `handle_datagram`, asserting the 100ms
/// per-datagram bound and that the whole backend seam is untouched
/// (`calls` unchanged — catches future default-bodied seam methods and sync
/// `try_send` enqueues that latency bounds cannot see, F10).
async fn feed_data_packet(
    ingest: &SrtIngest,
    server: &tokio::net::UdpSocket,
    backend: &CountingBackend,
    peers: &mut HashMap<SocketAddr, PeerState>,
    peer: SocketAddr,
    i: usize,
) {
    let pkt = make_data_packet(u32::try_from(i).unwrap(), KkFlag::Clear, &ts_payload_for(i));
    let calls_before = backend.calls.load(Ordering::SeqCst);
    let outcome = tokio::time::timeout(
        Duration::from_millis(100),
        handle_datagram(ingest, server, backend, peers, peer, &pkt),
    )
    .await
    .expect("per-datagram processing must not stall on the backend");
    outcome.expect("data packet must be accepted");
    assert_eq!(
        backend.calls.load(Ordering::SeqCst),
        calls_before,
        "data packet {i} must not invoke the backend"
    );
}

/// Count `.ts` segment files under `dir` (monotonic while the workload stays
/// under the HLS window — see `ts_payload_for` invariant).
fn ts_file_count(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .expect("hls dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "ts"))
        .count()
}

/// Balance the process-global active-session gauge by `n` (mirrors the
/// `removed()` calls production makes on teardown). The hermetic tests
/// establish sessions via `handle_datagram`'s established arm (which bumps
/// the gauge) and may never finalize them; the concurrent
/// `metrics::session_counter_add_remove_is_balanced` test asserts relative
/// deltas, so unbalanced leftovers make the suite flaky.
fn drain_active_session_gauge(times: usize) {
    for _ in 0..times {
        metrics::SessionCounter::removed();
    }
}

// ---------------------------------------------------------------------------
// T1 — AC1 main pin: streaming arm never awaits the backend.
// ---------------------------------------------------------------------------

/// An established session keeps feeding packets within the 100ms per-datagram
/// bound while the backend is stalled 500ms at the session start (B5 relay-
/// down worst case); the backend is called exactly once per session and HLS
/// segments keep flowing. A second peer then completes a full session on the
/// same loop state — the listener survives.
#[tokio::test]
async fn streaming_peer_never_awaits_backend() {
    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let backend = CountingBackend {
        session: Mutex::new(Some(SrtSession::new(hls))),
        // B5 relay-down worst case at session start (the only allowed slow
        // point, F6): 5× the 100ms per-datagram bound.
        resolve_delay: Duration::from_millis(500),
        ..CountingBackend::default()
    };
    let ingest = SrtIngest::with_identity(0x5254_0001, 0xCAFE);
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caller = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut peers = HashMap::new();
    let peer = establish_peer(
        &ingest,
        &server,
        &caller,
        &backend,
        &mut peers,
        0x1111_2222,
        "isolation/t1",
    )
    .await;

    // Snapshot assertions 1–3 BEFORE the second-peer phase (R6): the 500ms
    // start stall was paid exactly once — the per-session amortization proof.
    assert_eq!(
        backend.resolve_calls.load(Ordering::SeqCst),
        1,
        "resolve must run exactly once, at session start"
    );
    assert_eq!(
        backend.finalize_calls.load(Ordering::SeqCst),
        0,
        "no teardown during streaming"
    );
    assert_eq!(
        backend.calls.load(Ordering::SeqCst),
        1,
        "whole seam must see exactly the one start call"
    );

    // Feed 16 packets; each must stay inside the 100ms bound with zero
    // backend involvement. After 16 packets (IDR at 1-indexed 8 and 16) one
    // segment is closed → count_before >= 1.
    for i in 0..16 {
        feed_data_packet(&ingest, &server, &backend, &mut peers, peer, i).await;
    }
    let before = ts_file_count(dir.path());
    assert!(before >= 1, "segments must be flowing before the snapshot");

    // Feed 16 more; segments keep flowing, backend still untouched.
    for i in 16..32 {
        feed_data_packet(&ingest, &server, &backend, &mut peers, peer, i).await;
    }
    let after = ts_file_count(dir.path());
    assert!(
        after > before,
        "segments must keep flowing while the backend is stalled: {before} -> {after}"
    );
    assert_eq!(
        backend.resolve_calls.load(Ordering::SeqCst),
        1,
        "no second resolve during streaming"
    );
    assert_eq!(backend.finalize_calls.load(Ordering::SeqCst), 0);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);

    // Phase 5: a second peer (fresh backend — the single session slot serves
    // one resolve, R5) completes a full handshake + feed on the same loop
    // state: the listener is alive after the stalled start.
    let dir2 = tempfile::tempdir().unwrap();
    let hls2 = HlsWriter::new(dir2.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let backend2 = CountingBackend {
        session: Mutex::new(Some(SrtSession::new(hls2))),
        ..CountingBackend::default()
    };
    let caller2 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let second_peer = establish_peer(
        &ingest,
        &server,
        &caller2,
        &backend2,
        &mut peers,
        0x3333_4444,
        "isolation/t1b",
    )
    .await;
    feed_data_packet(&ingest, &server, &backend2, &mut peers, second_peer, 0).await;
    assert_eq!(
        backend2.resolve_calls.load(Ordering::SeqCst),
        1,
        "second peer resolves exactly once"
    );
    assert_eq!(
        backend.resolve_calls.load(Ordering::SeqCst),
        1,
        "first backend untouched by the second peer phase"
    );
    // Balance the gauge (2 established sessions, none finalized).
    drain_active_session_gauge(2);
}

// ---------------------------------------------------------------------------
// T2 — AC1 end-state simulation: stalled + failing finalize stays contained.
// ---------------------------------------------------------------------------

/// A relay-down at session end (B5-2 terminal failure: stalled finalize +
/// internally-failing HLS finish swallowed, mirroring `finalize_session`'s
/// warn-swallow) must never propagate into the media plane: streaming stays
/// within the per-datagram bound, the SHUTDOWN teardown completes within a
/// bounded 2s window, the peer is removed, and a second peer still streams.
#[tokio::test]
async fn relay_down_at_session_end_keeps_streaming() {
    let dir = tempfile::tempdir().unwrap();
    let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let backend = CountingBackend {
        session: Mutex::new(Some(SrtSession::new(hls))),
        // Connector terminal failure: finalize stalls 500ms (B5-2 backoff
        // bound lives in the connector; this is the in-crate worst case).
        finalize_delay: Duration::from_millis(500),
        // The broken-writer injection is armed here but only *triggered* by
        // the explicit `remove_dir_all` in step 2 (R7): without the deletion
        // the finish returns Ok and `let _` swallows an Ok — the Err branch
        // would never execute.
        finalize_broken_writer: true,
        ..CountingBackend::default()
    };
    let ingest = SrtIngest::with_identity(0x5254_0001, 0xCAFE);
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caller = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut peers = HashMap::new();
    let peer = establish_peer(
        &ingest,
        &server,
        &caller,
        &backend,
        &mut peers,
        0x1111_2222,
        "isolation/t2",
    )
    .await;

    // 1. Streaming keeps flowing (16 packets, all within the 100ms bound)
    //    while the (stalled-at-end) backend sits untouched.
    for i in 0..16 {
        feed_data_packet(&ingest, &server, &backend, &mut peers, peer, i).await;
    }
    assert!(
        ts_file_count(dir.path()) >= 1,
        "segments flowing before teardown"
    );
    assert_eq!(backend.finalize_calls.load(Ordering::SeqCst), 0);

    // 2. Inject the broken writer (R7): the HLS writer has no sticky error
    //    state and opens/drops file handles per call, so deleting its root
    //    makes the next `File::create` fail deterministically.
    std::fs::remove_dir_all(dir.path()).expect("remove hls dir");

    // 3. SHUTDOWN → teardown must complete within a bounded window. The
    //    bound is 2s, NOT 500ms (R2): the 500ms stall plus the broken-writer
    //    finish can exceed 500ms on real clocks; 2s is a 4× margin. The
    //    elapsed >= 400ms assertion proves the stall was actually paid.
    let shutdown = shutdown_control_datagram(0x1111_2222);
    let started = Instant::now();
    tokio::time::timeout(
        Duration::from_secs(2),
        handle_datagram(&ingest, &server, &backend, &mut peers, peer, &shutdown),
    )
    .await
    .expect("finalize stall must stay bounded")
    .expect("SHUTDOWN must be handled");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(400),
        "the finalize stall must actually be paid: {elapsed:?}"
    );

    // 4. Assertions: teardown ran exactly once at the boundary, the peer is
    //    removed (the SHUTDOWN arm self-removes inside handle_datagram), and
    //    no error escaped to the caller.
    assert_eq!(
        backend.finalize_calls.load(Ordering::SeqCst),
        1,
        "finalize must run exactly once, at the SHUTDOWN boundary"
    );
    assert!(
        !peers.contains_key(&peer),
        "SHUTDOWN must remove the peer from the loop state"
    );

    // 5. A second peer (fresh backend) still completes a full session: the
    //    failed teardown did not bite the media plane.
    let dir2 = tempfile::tempdir().unwrap();
    let hls2 = HlsWriter::new(dir2.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let backend2 = CountingBackend {
        session: Mutex::new(Some(SrtSession::new(hls2))),
        ..CountingBackend::default()
    };
    let caller2 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let second_peer = establish_peer(
        &ingest,
        &server,
        &caller2,
        &backend2,
        &mut peers,
        0x3333_4444,
        "isolation/t2b",
    )
    .await;
    feed_data_packet(&ingest, &server, &backend2, &mut peers, second_peer, 0).await;
    assert_eq!(backend2.resolve_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        backend.finalize_calls.load(Ordering::SeqCst),
        1,
        "first backend still saw exactly one finalize"
    );
    // Balance the gauge: peer1 was removed by `finalize`, peer2 (established,
    // never finalized) is removed here.
    drain_active_session_gauge(1);
}

// ---------------------------------------------------------------------------
// T3 — AC1 start-state simulation: failing resolve drops only that peer.
// ---------------------------------------------------------------------------

/// A relay-down at session start (provisioning/DB refusal) fails only that
/// peer's session attempt: `handle_datagram` surfaces the error, the peer
/// entry stays `Handshaking` (direct-drive semantics, R8 — removal is the
/// `run_listener` error arm's job, covered by B1 at loop level), and a second
/// peer completes a full session.
#[tokio::test]
async fn relay_down_at_session_start_drops_only_that_peer() {
    let backend1 = CountingBackend {
        resolve_error: true,
        ..CountingBackend::default()
    };
    let ingest = SrtIngest::with_identity(0x5254_0001, 0xCAFE);
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caller1 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let first_peer = caller1.local_addr().unwrap();
    let mut peers = HashMap::new();

    // INDUCTION → cookie.
    handle_datagram(
        &ingest,
        &server,
        &backend1,
        &mut peers,
        first_peer,
        &listener_test_induction(0x1111_2222),
    )
    .await
    .expect("induction must be accepted");
    let mut reply = [0u8; 2048];
    let (reply_len, _) =
        tokio::time::timeout(Duration::from_secs(1), caller1.recv_from(&mut reply))
            .await
            .expect("induction response timed out")
            .expect("recv failed");
    let induction = Handshake::parse(&reply[protocol::SRT_HEADER_LEN..reply_len]).unwrap();

    // CONCLUSION → resolve refuses → the error propagates out of
    // `handle_datagram` (production routes it to the run_listener drop arm).
    let err = handle_datagram(
        &ingest,
        &server,
        &backend1,
        &mut peers,
        first_peer,
        &listener_test_clear_conclusion(0x1111_2222, induction.syn_cookie, "isolation/t3"),
    )
    .await
    .expect_err("failing resolve must surface as an error");
    assert!(
        matches!(err, LiveError::UnknownStreamKey(_)),
        "resolve refusal must surface as UnknownStreamKey: {err}"
    );
    assert_eq!(backend1.resolve_calls.load(Ordering::SeqCst), 1);

    // Direct-drive semantics (R8): the entry stays `Handshaking` — `*entry =
    // Streaming` happens only after resolve succeeds; the removal is the
    // run_listener error arm's responsibility (loop level, covered by B1).
    assert!(
        matches!(peers.get(&first_peer), Some(PeerState::Handshaking(_))),
        "failed resolve leaves the peer in Handshaking (loop arm removes it)"
    );

    // A second peer with a healthy backend completes a full session: one
    // peer's start failure does not take the listener down.
    let dir2 = tempfile::tempdir().unwrap();
    let hls2 = HlsWriter::new(dir2.path().to_path_buf(), SEGMENT_DURATION_SECS)
        .await
        .unwrap();
    let backend2 = CountingBackend {
        session: Mutex::new(Some(SrtSession::new(hls2))),
        ..CountingBackend::default()
    };
    let caller2 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let second_peer = establish_peer(
        &ingest,
        &server,
        &caller2,
        &backend2,
        &mut peers,
        0x3333_4444,
        "isolation/t3b",
    )
    .await;
    feed_data_packet(&ingest, &server, &backend2, &mut peers, second_peer, 0).await;
    assert_eq!(
        backend2.resolve_calls.load(Ordering::SeqCst),
        1,
        "second peer resolves exactly once"
    );
    assert_eq!(
        backend1.resolve_calls.load(Ordering::SeqCst),
        1,
        "resolve total across both backends = 2 (one per peer)"
    );
    // Balance the gauge: the first peer's resolve failed before the
    // established arm's `added()`, so only the second peer's session counts.
    drain_active_session_gauge(1);
}

// ---------------------------------------------------------------------------
// T4 — static dependency guard (in-crate half of the double pin).
// ---------------------------------------------------------------------------

const CARGO_TOML: &str = include_str!("../Cargo.toml");

/// Dependency-key normalization (same semantics as
/// `scripts/dependency-check.sh:29`): `aero-common.workspace = true` → take
/// the `=`-prefixed segment, then the first `.`-prefixed segment →
/// `aero-common`. Whole-file scan (covers `[target...dependencies]` cfg
/// sections and `[build-dependencies]` — no `take_while` section bypass);
/// comment lines filtered first; `[dev-dependencies]` carries no aero-* key
/// (tokio/tempfile/sqlx) so it cannot false-positive.
fn dep_keys() -> Vec<String> {
    let mut keys = CARGO_TOML
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| {
            let key = line.split('=').next()?.trim();
            let key = key.split('.').next()?.trim();
            key.starts_with("aero-").then(|| key.to_string())
        })
        .collect::<Vec<_>>();
    keys.sort();
    keys
}

/// The crate's aero-* dependency set must stay exactly the media-plane
/// allowlist (aero-common / aero-live-core / aero-live-hls / aero-storage):
/// any future audit/relay/outbox crate is red here AND in
/// `scripts/dependency-check.sh:59` (double pin, R1.3 / AC3).
#[test]
fn cargo_toml_stays_audit_and_relay_free() {
    // Sorted-set equality: an extra aero-* dep is red, a missing one is red,
    // a legal reorder is not a false positive.
    assert_eq!(
        dep_keys(),
        [
            "aero-common",
            "aero-live-core",
            "aero-live-hls",
            "aero-storage"
        ],
        "aero-* dependency allowlist drifted"
    );

    // Rename bypass guard (R4): `my-connector.workspace = true` + a root
    // `[workspace.dependencies] my-connector = { package = "aero-audit-connector" }`
    // yields a key that is not aero-* — invisible to both the key equality
    // above and dependency-check.sh's key-only sed. Scan `package = "..."`
    // values inside the `[dependencies]` section for real aero-* names.
    let section = CARGO_TOML
        .lines()
        .skip_while(|line| !line.starts_with("[dependencies]"))
        .take_while(|line| !line.starts_with('[') || line.starts_with("[dependencies]"));
    let renamed: Vec<&str> = section
        .filter_map(|line| {
            let rest = line.split("package = ").nth(1)?;
            let pkg = rest.trim().trim_start_matches('"');
            let pkg = pkg.split(['"', ',', '}']).next().unwrap_or(pkg).trim();
            pkg.starts_with("aero-").then_some(pkg)
        })
        .collect();
    assert!(
        renamed.iter().all(|pkg| [
            "aero-common",
            "aero-live-core",
            "aero-live-hls",
            "aero-storage"
        ]
        .contains(pkg)),
        "renamed aero dep bypass: {renamed:?}"
    );
}

// ---------------------------------------------------------------------------
// B1 — AC2: full boot path fails open with zero audit provisioning (DB-gated).
// ---------------------------------------------------------------------------

/// The complete boot path — `SrtIngest::run_until_cancelled` — accepts a
/// full `HSv5` handshake + TS feed with **zero audit provisioning** configured:
/// `mark_live` flips the row live, HLS artifacts land on disk, cancel drains
/// through `mark_ended`. The crate has no audit symbols at all (crate-wide
/// `grep -ri audit` = 0) and `run_until_cancelled` has no provisioning
/// parameter — that construction, plus T4, is the fail-open proof; this test
/// proves the boot path itself.
///
/// Discipline (AGENTS.md §4.3): `#[ignore]` + `DATABASE_URL` gate with
/// skip-not-red, throwaway DB created/dropped around the run, `pool.close()`
/// before `DROP DATABASE` on both the success and failure paths (sqlx idle
/// connections block the drop), and `handle.abort()` + await after any join
/// timeout (a lingering task holds pooled connections).
#[tokio::test]
#[ignore = "requires live Postgres (throwaway DB; skipped when DATABASE_URL is unset)"]
async fn run_until_cancelled_boot_fails_open_without_audit_provisioning() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipped: DATABASE_URL unset");
        return;
    };
    let parse_opts = || -> sqlx::postgres::PgConnectOptions {
        url.parse()
            .expect("DATABASE_URL must be a valid postgres URL")
    };

    // Throwaway DB lifecycle: admin connection (server-level CREATE/DROP
    // works from any database), stale-run cleanup, create.
    let throwaway = format!("aero_live_srt_b1_{}", std::process::id());
    let admin_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(parse_opts())
        .await
        .expect("admin connection");
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {throwaway} WITH (FORCE)"))
        .execute(&admin_pool)
        .await;
    sqlx::query(&format!("CREATE DATABASE {throwaway}"))
        .execute(&admin_pool)
        .await
        .expect("create throwaway database");

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(parse_opts().database(&throwaway))
        .await
        .expect("connect throwaway database");
    aero_storage::db::migrate(&pool)
        .await
        .expect("migrate throwaway database");

    // Seed an owner participant + a stream row keyed `b1/e2e-key`.
    let owner_id = aero_common::ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(owner_id.to_uuid())
        .bind("aero-live-srt-b1")
        .execute(&pool)
        .await
        .expect("insert owner participant");
    let repo = aero_storage::StreamRepo::new(pool.clone());
    let stream = repo
        .create(aero_storage::NewStream {
            owner_id,
            room_id: None,
            title: "srt isolation b1".into(),
            protocol: aero_common::StreamProtocol::Srt,
            stream_key: Some("b1/e2e-key".into()),
        })
        .await
        .expect("create stream row");

    // Probe the SRT port P, then give RTMP P-1 (R3, tests.rs:909-916
    // template): `listen_addr` = rtmp + 1, so probing P and using it as
    // `rtmp_listen` would leave the actually-bound P+1 unprobed.
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
    let hls_dir = tempfile::tempdir().unwrap();
    let cfg = Arc::new(LiveStreamConfig {
        hls_dir: hls_dir.path().to_path_buf(),
        rtmp_listen: rtmp_addr,
    });
    let cancel = CancellationToken::new();
    let mut handle = tokio::spawn({
        let repo = repo.clone();
        let cancel = cancel.clone();
        async move {
            SrtIngest::new()
                .run_until_cancelled(repo, cfg, cancel)
                .await
        }
    });

    // Full HSv5 handshake against the real listener. The bind is async, so
    // retry the INDUCTION until answered (also tolerates UDP loss).
    let caller = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caller_socket_id = 0x2222_3333;
    let mut reply = [0u8; 2048];
    let mut reply_len = None;
    for _ in 0..100 {
        caller
            .send_to(&listener_test_induction(caller_socket_id), srt_addr)
            .await
            .unwrap();
        let Ok(Ok((len, _))) =
            tokio::time::timeout(Duration::from_millis(100), caller.recv_from(&mut reply)).await
        else {
            continue;
        };
        reply_len = Some(len);
        break;
    }
    let reply_len = reply_len.expect("listener never answered the INDUCTION");
    let induction = Handshake::parse(&reply[protocol::SRT_HEADER_LEN..reply_len]).unwrap();
    caller
        .send_to(
            &listener_test_clear_conclusion(caller_socket_id, induction.syn_cookie, "b1/e2e-key"),
            srt_addr,
        )
        .await
        .unwrap();
    let (reply_len, _) = tokio::time::timeout(Duration::from_secs(1), caller.recv_from(&mut reply))
        .await
        .expect("conclusion response timed out")
        .unwrap();
    let agreement = Handshake::parse(&reply[protocol::SRT_HEADER_LEN..reply_len]).unwrap();
    assert_eq!(
        agreement.handshake_type,
        protocol::HandshakeType::Conclusion,
        "listener must answer the conclusion with an HSRSP"
    );

    // TS feed (16 packets, IDR at 1-indexed 8 and 16 → one segment cut).
    for i in 0..16 {
        caller
            .send_to(
                &make_data_packet(u32::try_from(i).unwrap(), KkFlag::Clear, &ts_payload_for(i)),
                srt_addr,
            )
            .await
            .unwrap();
    }

    // mark_live must have flipped the row; HLS artifacts must exist. The
    // listener sends the HSRSP *before* resolve, so poll briefly for the
    // live state instead of racing the in-loop `mark_live` commit.
    let live = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let row = repo
                .get_by_key("b1/e2e-key")
                .await
                .expect("get_by_key after mark_live");
            if row
                .as_ref()
                .is_some_and(|r| r.status == aero_common::StreamStatus::Live)
            {
                break row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("stream must flip to live after the handshake");
    assert_eq!(
        live.as_ref().map(|row| &row.status),
        Some(&aero_common::StreamStatus::Live),
        "mark_live must have flipped the stream to live"
    );
    let stream_dir = hls_dir.path().join(stream.id.to_string());
    // The data packets queue in the UDP buffer until resolve completes, so
    // poll for the keyframe-cut artifacts as well.
    let artifacts = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let manifest = stream_dir.join("index.m3u8").exists();
            let segments = ts_file_count(&stream_dir);
            if manifest && segments >= 1 {
                break (manifest, segments);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("HLS artifacts must appear after the TS feed");
    assert!(artifacts.0, "HLS manifest must be written under hls_dir");
    assert!(artifacts.1 >= 1, "at least one HLS segment must be written");

    // Cancel → graceful drain (finalize_session: finish + mark_ended).
    cancel.cancel();
    let joined = tokio::time::timeout(Duration::from_secs(5), &mut handle)
        .await
        .expect("cancelled listener should stop promptly");
    let Ok(live_result) = joined else {
        // R3: abort + await before touching the DB — a lingering task
        // holds StreamRepo pooled connections and would block the drop.
        handle.abort();
        let _ = handle.await;
        panic!("listener did not stop within 5s of cancel");
    };
    assert!(
        live_result.is_ok(),
        "listener should stop cleanly: {live_result:?}"
    );
    let ended = repo
        .get_by_key("b1/e2e-key")
        .await
        .expect("get_by_key after cancel");
    assert_eq!(
        ended.as_ref().map(|row| &row.status),
        Some(&aero_common::StreamStatus::Ended),
        "mark_ended must run on the cancel drain"
    );

    // R3 teardown: close the pool (both paths) before DROP DATABASE — sqlx
    // keeps idle connections open and would block the drop.
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {throwaway} WITH (FORCE)"))
        .execute(&admin_pool)
        .await
        .expect("drop throwaway database");
    admin_pool.close().await;
}
