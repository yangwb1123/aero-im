//! ClamAV (`clamd`) anti-virus scanning for uploaded blobs (defence-in-depth).
//!
//! [`crate::content_sniff`] only inspects a blob's leading *magic bytes*: it
//! rejects an executable / HTML / SVG payload disguised as an allowed type, and a
//! binary whose content does not match its declared family. But a real malicious
//! binary that simply carries a benign header — e.g. a polyglot or a malware
//! sample with a forged `image/png` signature — passes the sniffer untouched. A
//! real signature/heuristic scanner is the missing layer; this module is a
//! `clamd` **INSTREAM** client that streams the bytes to a ClamAV daemon and
//! interprets its verdict.
//!
//! ## The `clamd` INSTREAM wire protocol
//!
//! After connecting over TCP we send the null-terminated command `zINSTREAM\0`,
//! then a sequence of chunks — each a **4-byte big-endian length prefix** followed
//! by that many payload bytes — and finally a **zero-length chunk** (`\0\0\0\0`)
//! to mark the end of the stream. `clamd` replies with a single line:
//!
//! * `stream: OK\0`                     → clean
//! * `stream: <Signature> FOUND\0`      → infected (the name is the signature)
//! * `... ERROR\0`                      → a daemon-side error (e.g. size limit)
//!
//! The frame **encoding** and the **response parsing** are pure functions
//! ([`encode_instream`], [`parse_response`]) and are unit-tested with no I/O. The
//! end-to-end path that actually talks to a daemon is exercised only by the
//! `#[ignore]`d socket test — it needs a running `clamd`, which is a
//! **staging/integration seam**, not something the sandbox can spin up.

use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// `clamd` INSTREAM chunk size. Each chunk carries a 4-byte big-endian length
/// prefix; `clamd`'s `StreamMaxLength` defaults to 25 MiB but we cap our chunk
/// size well below the protocol's per-chunk maximum so a single frame is cheap to
/// buffer on both ends. (The *total* stream can be many chunks.)
const CHUNK_SIZE: usize = 64 * 1024;

/// How long to wait for the connect + full scan round-trip before giving up and
/// returning [`ScanVerdict::Error`] (which the upload path then resolves per the
/// fail-open / fail-closed policy).
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);

/// The outcome of scanning a blob's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanVerdict {
    /// `clamd` reported `stream: OK` — no signature matched.
    Clean,
    /// `clamd` reported `... FOUND` — the carried `String` is the signature name.
    Infected(String),
    /// The scan could not be completed (connect failed, timeout, malformed
    /// response, or a daemon-side `ERROR`). The carried `String` is a short
    /// diagnostic. The caller decides whether this blocks the upload
    /// (fail-closed) or is allowed through with a warning (fail-open).
    Error(String),
}

/// A `clamd` INSTREAM scanning client, addressed by the daemon's TCP endpoint.
#[derive(Debug, Clone)]
pub struct ClamdScanner {
    /// `host:port` of the `clamd` TCP socket (e.g. `127.0.0.1:3310`).
    addr: String,
}

impl ClamdScanner {
    /// Construct a scanner pointed at the given `clamd` TCP `addr`.
    #[must_use]
    pub fn new(addr: impl Into<String>) -> Self {
        Self { addr: addr.into() }
    }

    /// Build a scanner from the environment, or `None` to leave AV scanning OFF.
    ///
    /// * `AERO_CLAMAV_HOST` — the `clamd` TCP endpoint, e.g. `127.0.0.1:3310`.
    ///   Unset / empty ⇒ `None` (the feature is disabled; uploads are unaffected).
    #[must_use]
    pub fn from_env() -> Option<Self> {
        std::env::var("AERO_CLAMAV_HOST")
            .ok()
            .map(|h| h.trim().to_owned())
            .filter(|h| !h.is_empty())
            .map(Self::new)
    }

    /// The configured `clamd` endpoint.
    #[must_use]
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// The process-wide scanner, constructed once from [`ClamdScanner::from_env`].
    ///
    /// The upload hot path uses this so a `clamd` endpoint is resolved from the
    /// environment exactly once (not per request), while still keeping the wiring
    /// inside the handler (no `AppState` / boot changes). `None` ⇒ AV scanning is
    /// disabled and uploads behave exactly as before this module existed.
    #[must_use]
    pub fn global() -> Option<&'static Self> {
        static SCANNER: std::sync::OnceLock<Option<ClamdScanner>> = std::sync::OnceLock::new();
        SCANNER.get_or_init(Self::from_env).as_ref()
    }

    /// Scan `bytes` against the configured `clamd` daemon over INSTREAM.
    ///
    /// Connect failures, timeouts and malformed replies all resolve to
    /// [`ScanVerdict::Error`] (never panic / never propagate an `Err`): a flaky
    /// daemon must never crash the upload path, and the policy layer decides how
    /// an `Error` is treated. The whole exchange is bounded by [`SCAN_TIMEOUT`].
    pub async fn scan(&self, bytes: &[u8]) -> ScanVerdict {
        match tokio::time::timeout(SCAN_TIMEOUT, self.scan_inner(bytes)).await {
            Ok(v) => v,
            Err(_) => ScanVerdict::Error(format!("clamd scan timed out after {SCAN_TIMEOUT:?}")),
        }
    }

    /// The unguarded scan round-trip (wrapped in a timeout by [`scan`]).
    async fn scan_inner(&self, bytes: &[u8]) -> ScanVerdict {
        let mut stream = match TcpStream::connect(&self.addr).await {
            Ok(s) => s,
            Err(e) => return ScanVerdict::Error(format!("clamd connect {}: {e}", self.addr)),
        };
        let frame = encode_instream(bytes);
        if let Err(e) = stream.write_all(&frame).await {
            return ScanVerdict::Error(format!("clamd write: {e}"));
        }
        if let Err(e) = stream.flush().await {
            return ScanVerdict::Error(format!("clamd flush: {e}"));
        }
        // The reply is a single short line; read to EOF (clamd closes after it).
        let mut resp = Vec::with_capacity(64);
        if let Err(e) = stream.read_to_end(&mut resp).await {
            return ScanVerdict::Error(format!("clamd read: {e}"));
        }
        parse_response(&resp)
    }
}

/// Counter (server-local name): blob-upload AV scan outcomes, labeled by
/// `result` = `clean` | `infected` | `error`. Lets an operator watch the
/// infection-block rate and — critically — the `error` rate, which under the
/// default **fail-open** policy is the signal that `clamd` is flaky and malware
/// may be slipping through un-scanned.
pub const AV_SCAN_TOTAL: &str = "aero_av_scan_total";

/// Whether an unreachable / erroring `clamd` should **block** the upload
/// (fail-closed) rather than allow it through with a warning (fail-open).
///
/// * `AERO_CLAMAV_FAIL_CLOSED` = `1` / `true` / `yes` / `on` ⇒ fail-closed.
/// * unset / anything else ⇒ **fail-open** (the default): a scanner outage must
///   not block *all* uploads, so an `Error` verdict is allowed through (with a
///   `warn!` + an `error`-labeled metric for visibility).
///
/// Resolved at most once and cached for the process lifetime, so the upload hot
/// path never re-reads the environment.
#[must_use]
pub fn fail_closed() -> bool {
    static FAIL_CLOSED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FAIL_CLOSED.get_or_init(|| {
        std::env::var("AERO_CLAMAV_FAIL_CLOSED")
            .ok()
            .is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
    })
}

/// Records one AV-scan outcome on the [`AV_SCAN_TOTAL`] counter.
pub fn record_scan(result: &str) {
    aero_common::metrics::inc_counter_labeled(AV_SCAN_TOTAL, 1, &[("result", result)]);
}

/// Encode `bytes` as a complete `clamd` INSTREAM request body: the
/// `zINSTREAM\0` command, then one length-prefixed chunk per [`CHUNK_SIZE`] slice
/// of `bytes`, then the terminating zero-length chunk (`\0\0\0\0`).
///
/// Pure / allocation-only — no I/O — so the framing is unit-testable offline.
#[must_use]
pub fn encode_instream(bytes: &[u8]) -> Vec<u8> {
    // Command + per-chunk (4-byte prefix + payload) + 4-byte terminator.
    let chunks = bytes.len().div_ceil(CHUNK_SIZE).max(1);
    let mut out = Vec::with_capacity(b"zINSTREAM\0".len() + bytes.len() + 4 * chunks + 4);
    out.extend_from_slice(b"zINSTREAM\0");
    for chunk in bytes.chunks(CHUNK_SIZE) {
        // 4-byte big-endian chunk length, then the chunk. `clamd` rejects a chunk
        // declaring a length of 0 anywhere but the terminator, so empty input
        // simply emits no data chunks and goes straight to the terminator.
        let len = u32::try_from(chunk.len()).expect("chunk len <= CHUNK_SIZE fits in u32");
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(chunk);
    }
    // Terminating zero-length chunk marks end-of-stream.
    out.extend_from_slice(&0u32.to_be_bytes());
    out
}

/// Parse a raw `clamd` INSTREAM reply into a [`ScanVerdict`].
///
/// Recognises (case-sensitively on the `clamd` keywords, after trimming the
/// trailing `\0` / whitespace):
/// * `... OK`               → [`ScanVerdict::Clean`]
/// * `... <Sig> FOUND`      → [`ScanVerdict::Infected`] (signature extracted)
/// * `... ERROR` / anything else → [`ScanVerdict::Error`]
///
/// Pure — unit-tested offline.
#[must_use]
pub fn parse_response(raw: &[u8]) -> ScanVerdict {
    // `clamd` replies in ASCII/UTF-8 and terminates the line with a NUL.
    let text = String::from_utf8_lossy(raw);
    let line = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    if line.is_empty() {
        return ScanVerdict::Error("clamd: empty response".to_owned());
    }
    if line.ends_with("OK") {
        return ScanVerdict::Clean;
    }
    if let Some(sig) = line.strip_suffix("FOUND") {
        // Reply shape: `stream: <Signature> FOUND`. Strip the `stream:` prefix and
        // the trailing keyword to recover the signature name.
        let sig = sig
            .trim()
            .strip_prefix("stream:")
            .unwrap_or(sig)
            .trim()
            .to_owned();
        let name = if sig.is_empty() { "unknown-signature".to_owned() } else { sig };
        return ScanVerdict::Infected(name);
    }
    // `... ERROR` or any unrecognised reply — surface the line for diagnostics.
    ScanVerdict::Error(format!("clamd: {line}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_off_when_unset_or_empty() {
        // Snapshot + restore so we don't leak state into sibling tests.
        let prev = std::env::var("AERO_CLAMAV_HOST").ok();
        std::env::remove_var("AERO_CLAMAV_HOST");
        assert!(ClamdScanner::from_env().is_none(), "unset ⇒ feature off");
        std::env::set_var("AERO_CLAMAV_HOST", "   ");
        assert!(ClamdScanner::from_env().is_none(), "blank ⇒ feature off");
        std::env::set_var("AERO_CLAMAV_HOST", " 127.0.0.1:3310 ");
        let s = ClamdScanner::from_env().expect("set ⇒ feature on");
        assert_eq!(s.addr(), "127.0.0.1:3310", "trimmed");
        match prev {
            Some(v) => std::env::set_var("AERO_CLAMAV_HOST", v),
            None => std::env::remove_var("AERO_CLAMAV_HOST"),
        }
    }

    #[test]
    fn encode_instream_frames_command_chunk_and_terminator() {
        let payload = b"hello";
        let frame = encode_instream(payload);
        // Command prefix.
        assert!(frame.starts_with(b"zINSTREAM\0"));
        let rest = &frame[b"zINSTREAM\0".len()..];
        // 4-byte BE length == 5, then the payload, then the 4-byte zero terminator.
        assert_eq!(&rest[0..4], &5u32.to_be_bytes(), "chunk length prefix (BE)");
        assert_eq!(&rest[4..9], payload, "chunk payload");
        assert_eq!(&rest[9..13], &[0, 0, 0, 0], "zero-length terminator");
        assert_eq!(rest.len(), 13, "exactly one chunk + terminator");
    }

    #[test]
    fn encode_instream_empty_input_is_command_plus_terminator_only() {
        let frame = encode_instream(b"");
        // No data chunks; straight to the zero terminator.
        let mut expected = b"zINSTREAM\0".to_vec();
        expected.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(frame, expected);
    }

    #[test]
    fn encode_instream_splits_into_chunks_of_chunk_size() {
        // Two-and-a-bit chunks worth of data ⇒ 3 length-prefixed frames.
        let payload = vec![0xABu8; CHUNK_SIZE * 2 + 7];
        let frame = encode_instream(&payload);
        let mut cur = &frame[b"zINSTREAM\0".len()..];
        let mut seen = 0usize;
        let mut total = 0usize;
        loop {
            let len = u32::from_be_bytes(cur[0..4].try_into().unwrap()) as usize;
            if len == 0 {
                break; // terminator
            }
            assert!(len <= CHUNK_SIZE, "no chunk exceeds CHUNK_SIZE");
            cur = &cur[4 + len..];
            seen += 1;
            total += len;
        }
        assert_eq!(seen, 3, "ceil(2*CHUNK+7 / CHUNK) == 3 chunks");
        assert_eq!(total, payload.len(), "all bytes accounted for");
    }

    #[test]
    fn av_scan_counter_uses_server_local_name() {
        assert_eq!(AV_SCAN_TOTAL, "aero_av_scan_total");
        // Emitting bumps the namespaced counter and it shows up in the exposition.
        record_scan("clean");
        assert!(aero_common::metrics::render_prometheus().contains(AV_SCAN_TOTAL));
    }

    #[test]
    fn parse_response_clean() {
        assert_eq!(parse_response(b"stream: OK\0"), ScanVerdict::Clean);
        assert_eq!(parse_response(b"stream: OK\n"), ScanVerdict::Clean);
    }

    #[test]
    fn parse_response_found_extracts_signature() {
        assert_eq!(
            parse_response(b"stream: Eicar-Test-Signature FOUND\0"),
            ScanVerdict::Infected("Eicar-Test-Signature".to_owned()),
        );
        // Multi-word signature names survive (only the trailing keyword is removed).
        assert_eq!(
            parse_response(b"stream: Win.Test.EICAR_HDB-1 FOUND"),
            ScanVerdict::Infected("Win.Test.EICAR_HDB-1".to_owned()),
        );
    }

    #[test]
    fn parse_response_error_and_empty() {
        match parse_response(b"INSTREAM size limit exceeded. ERROR\0") {
            ScanVerdict::Error(m) => assert!(m.contains("ERROR"), "got {m}"),
            other => panic!("expected Error, got {other:?}"),
        }
        match parse_response(b"\0") {
            ScanVerdict::Error(m) => assert!(m.contains("empty"), "got {m}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// End-to-end against a real `clamd`. **Staging/integration seam** — requires a
    /// running daemon (set `AERO_CLAMAV_HOST`, e.g. `127.0.0.1:3310`), so it is
    /// `#[ignore]`d in CI/sandbox. The payload is the standard EICAR test string,
    /// which every AV engine reports as a (harmless) test signature.
    #[tokio::test]
    #[ignore = "needs a running clamd daemon (staging seam); set AERO_CLAMAV_HOST"]
    async fn eicar_is_reported_infected_against_real_clamd() {
        let scanner = ClamdScanner::from_env().expect("set AERO_CLAMAV_HOST to run this");
        // The canonical EICAR test file (split to avoid tripping *this* repo's AV).
        let eicar = format!(
            "X5O!P%@AP[4\\PZX54(P^)7CC)7}}${}-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*",
            "EICAR"
        );
        match scanner.scan(eicar.as_bytes()).await {
            ScanVerdict::Infected(name) => assert!(name.contains("EICAR") || name.contains("Eicar")),
            other => panic!("expected EICAR to be Infected, got {other:?}"),
        }
        // A benign payload should come back Clean.
        assert_eq!(scanner.scan(b"just some words").await, ScanVerdict::Clean);
    }

    /// End-to-end socket round-trip against an **in-process mock `clamd`** that
    /// speaks the real INSTREAM wire protocol. Unlike the `#[ignore]`d real-daemon
    /// test above, this needs no external daemon, so it runs in CI/sandbox and
    /// covers what the pure `encode_instream`/`parse_response` unit tests cannot:
    /// `scan_inner`'s actual connect → write-frame → read-to-EOF exchange, plus
    /// that a multi-chunk (>`CHUNK_SIZE`) payload is framed so a real daemon can
    /// reassemble it. The mock DECODES the client's length-prefixed chunks and
    /// only reports `FOUND` when the reassembled bytes carry the marker — so a
    /// framing bug surfaces as a wrong verdict, never a false pass.
    #[tokio::test]
    async fn scan_round_trip_against_mock_clamd_speaking_instream() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        const MARKER: &[u8] = b"EICAR-MARKER";

        // A mock clamd: accept one connection, parse the INSTREAM framing exactly
        // as the real daemon would, reassemble the payload, and reply per verdict.
        async fn serve_one(listener: TcpListener) {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            // Command: the null-terminated `zINSTREAM\0` (10 bytes).
            let mut cmd = [0u8; 10];
            if sock.read_exact(&mut cmd).await.is_err() || &cmd != b"zINSTREAM\0" {
                return; // malformed command ⇒ no reply ⇒ client observes an Error
            }
            // Reassemble the streamed payload from 4-byte-BE length-prefixed chunks
            // until the zero-length terminator — i.e. validate the client's framing.
            let mut payload = Vec::new();
            loop {
                let mut len = [0u8; 4];
                if sock.read_exact(&mut len).await.is_err() {
                    return;
                }
                let n = u32::from_be_bytes(len) as usize;
                if n == 0 {
                    break; // terminator
                }
                let mut chunk = vec![0u8; n];
                if sock.read_exact(&mut chunk).await.is_err() {
                    return;
                }
                payload.extend_from_slice(&chunk);
            }
            // Reply exactly as clamd does, keyed on the REASSEMBLED bytes.
            let reply: &[u8] = if payload.windows(MARKER.len()).any(|w| w == MARKER) {
                b"stream: Eicar-Test-Signature FOUND\0"
            } else {
                b"stream: OK\0"
            };
            let _ = sock.write_all(reply).await;
            let _ = sock.shutdown().await; // close so the client's read_to_end returns
        }

        async fn scan_via_mock(bytes: &[u8]) -> ScanVerdict {
            // Bind before spawning so the connection is accepted from the backlog
            // even if `serve_one` hasn't reached `accept()` yet (no startup race).
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock clamd");
            let addr = listener.local_addr().unwrap().to_string();
            tokio::spawn(serve_one(listener));
            ClamdScanner::new(addr).scan(bytes).await
        }

        // (1) Infected single-chunk payload → Infected(signature parsed from FOUND).
        match scan_via_mock(b"prefix EICAR-MARKER suffix").await {
            ScanVerdict::Infected(sig) => assert_eq!(sig, "Eicar-Test-Signature"),
            other => panic!("expected Infected, got {other:?}"),
        }

        // (2) Benign payload → Clean.
        assert_eq!(scan_via_mock(b"perfectly benign bytes").await, ScanVerdict::Clean);

        // (3) Payload larger than CHUNK_SIZE exercises multi-chunk framing: the
        // marker only reassembles if every length-prefixed chunk is emitted and
        // decoded correctly.
        let mut big = vec![b'.'; CHUNK_SIZE * 2 + 1234];
        big.extend_from_slice(MARKER);
        match scan_via_mock(&big).await {
            ScanVerdict::Infected(sig) => assert_eq!(sig, "Eicar-Test-Signature"),
            other => panic!("expected Infected for multi-chunk payload, got {other:?}"),
        }
    }
}
