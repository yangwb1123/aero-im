//! SRT media-plane Prometheus metric names + thin emit helpers.
//!
//! These names are defined **locally** in this crate (not in `aero-common`)
//! because they describe the SRT ingest data plane specifically; the global
//! registry that ultimately holds them lives in `aero_common::metrics`, which
//! we emit into via [`aero_common::metrics::inc_counter`] /
//! [`aero_common::metrics::set_gauge`].
//!
//! The metrics exposed are:
//!
//! - `aero_srt_active_sessions` (gauge) — established SRT sessions currently
//!   feeding the segmenter, tracked process-wide via [`ACTIVE_SESSIONS`].
//! - `aero_srt_packets_received_total` (counter) — inbound SRT data datagrams.
//! - `aero_srt_packets_lost_total` (counter) — packets detected missing by the
//!   receiver-side reliability layer (the span of each emitted NAK loss range).
//! - `aero_srt_bytes_received_total` (counter) — total inbound datagram bytes.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Gauge: number of established SRT sessions currently streaming.
pub(crate) const ACTIVE_SESSIONS: &str = "aero_srt_active_sessions";
/// Counter: inbound SRT data datagrams accepted on an established session.
pub(crate) const PACKETS_RECEIVED_TOTAL: &str = "aero_srt_packets_received_total";
/// Counter: packets the receiver detected as lost (sum of NAK loss-range spans).
pub(crate) const PACKETS_LOST_TOTAL: &str = "aero_srt_packets_lost_total";
/// Counter: total inbound datagram bytes (full datagram, header included).
pub(crate) const BYTES_RECEIVED_TOTAL: &str = "aero_srt_bytes_received_total";

/// Process-wide count of established SRT sessions.
///
/// SRT multiplexes every caller over a single listener socket, and each
/// `SrtIngest::run` keeps its peers in a local map, so we cannot derive a global
/// session count from one map alone. This atomic is bumped on session
/// establishment and decremented on teardown; the `aero_srt_active_sessions`
/// gauge is re-published from it on every change via [`SessionCounter`].
static ACTIVE_SESSIONS_COUNT: AtomicUsize = AtomicUsize::new(0);

/// RAII-free counter guard for the live-session gauge.
///
/// Construct one ([`SessionCounter::added`]) the moment a session transitions to
/// streaming, and call [`SessionCounter::removed`] on teardown. Both operations
/// re-publish the `aero_srt_active_sessions` gauge from the shared atomic, so the
/// gauge always reflects the current process-wide session count regardless of
/// which listener accepted the peer.
///
/// We deliberately keep this as explicit add/remove calls (rather than a `Drop`
/// guard) because the streaming state lives inside an enum that is moved around
/// the peer map; an explicit call at the established / finalized boundaries
/// mirrors how the surrounding code already brackets session lifetime.
pub(crate) struct SessionCounter;

impl SessionCounter {
    /// Record a newly-established session and re-publish the gauge.
    pub(crate) fn added() {
        let n = ACTIVE_SESSIONS_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        publish_active_sessions(n);
    }

    /// Record a torn-down session and re-publish the gauge.
    ///
    /// Saturating: a spurious teardown without a matching add leaves the count
    /// at zero rather than underflowing.
    pub(crate) fn removed() {
        // Compare-and-swap loop so we never wrap below zero.
        let mut cur = ACTIVE_SESSIONS_COUNT.load(Ordering::Relaxed);
        loop {
            let next = cur.saturating_sub(1);
            match ACTIVE_SESSIONS_COUNT.compare_exchange_weak(
                cur,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    publish_active_sessions(next);
                    return;
                }
                Err(observed) => cur = observed,
            }
        }
    }

    /// Current process-wide live-session count (test/observability helper).
    #[cfg(test)]
    pub(crate) fn current() -> usize {
        ACTIVE_SESSIONS_COUNT.load(Ordering::Relaxed)
    }
}

/// Re-publish the active-sessions gauge from a raw count.
fn publish_active_sessions(count: usize) {
    // usize → f64 is lossless for any plausible session count.
    #[allow(clippy::cast_precision_loss)]
    aero_common::metrics::set_gauge(ACTIVE_SESSIONS, count as f64);
}

/// Record one accepted inbound SRT data datagram of `bytes` length.
pub(crate) fn record_datagram(bytes: usize) {
    aero_common::metrics::inc_counter(PACKETS_RECEIVED_TOTAL, 1);
    aero_common::metrics::inc_counter(BYTES_RECEIVED_TOTAL, u64::try_from(bytes).unwrap_or(u64::MAX));
}

/// Record `count` packets the receiver detected as lost (one NAK loss range).
pub(crate) fn record_lost(count: u64) {
    if count > 0 {
        aero_common::metrics::inc_counter(PACKETS_LOST_TOTAL, count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_names_are_stable() {
        // Guards against accidental renames that would break dashboards/alerts.
        assert_eq!(ACTIVE_SESSIONS, "aero_srt_active_sessions");
        assert_eq!(PACKETS_RECEIVED_TOTAL, "aero_srt_packets_received_total");
        assert_eq!(PACKETS_LOST_TOTAL, "aero_srt_packets_lost_total");
        assert_eq!(BYTES_RECEIVED_TOTAL, "aero_srt_bytes_received_total");
    }

    #[test]
    fn session_counter_add_remove_is_balanced() {
        // Relative assertions only: the underlying atomic is process-wide, so we
        // measure deltas against the current base rather than absolute values.
        let base = SessionCounter::current();

        SessionCounter::added();
        SessionCounter::added();
        assert_eq!(SessionCounter::current(), base + 2);

        SessionCounter::removed();
        assert_eq!(SessionCounter::current(), base + 1);

        SessionCounter::removed();
        assert_eq!(SessionCounter::current(), base, "adds and removes balance");
    }

    #[test]
    fn record_lost_zero_is_a_noop() {
        // A NAK loss range of length zero (which `on_data` never emits, but a
        // caller might compute) must not move the counter or panic.
        record_lost(0);
    }
}
