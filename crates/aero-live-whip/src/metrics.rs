//! WHIP media-plane Prometheus metric names + thin emit helpers.
//!
//! These names are defined **locally** in this crate (not in `aero-common`)
//! because they describe the WHIP (WebRTC) ingest data plane specifically; the
//! global registry that ultimately holds them lives in `aero_common::metrics`,
//! which we emit into via [`aero_common::metrics::inc_counter`] /
//! [`aero_common::metrics::set_gauge`].
//!
//! The metrics exposed are:
//!
//! - `aero_whip_rtp_packets_received_total` (counter) — inbound RTP packets
//!   surfaced by str0m on an established WHIP session, counted once per packet
//!   in the ingest run loop via [`record_rtp_packet`].
//! - `aero_whip_depacketize_failures_total` (counter) — RFC 6184 H.264
//!   depacketizer errors (reassembly desync from loss/reorder), counted once
//!   per error in the depacketize hot path via [`record_depacketize_failure`].
//! - `aero_whip_active_sessions` (gauge) — currently-registered live WHIP
//!   publishers, re-published from the [`WhipRegistry`](crate::WhipRegistry)
//!   session count via [`set_active_sessions`].

/// Counter: inbound RTP packets received on an established WHIP session.
pub(crate) const RTP_PACKETS_RECEIVED_TOTAL: &str = "aero_whip_rtp_packets_received_total";
/// Counter: H.264 (RFC 6184) depacketizer failures (resync-triggering errors).
pub(crate) const DEPACKETIZE_FAILURES_TOTAL: &str = "aero_whip_depacketize_failures_total";
/// Gauge: number of currently-registered live WHIP publishers.
pub(crate) const ACTIVE_SESSIONS: &str = "aero_whip_active_sessions";

/// Record one inbound RTP packet received in the WHIP ingest run loop.
pub(crate) fn record_rtp_packet() {
    aero_common::metrics::inc_counter(RTP_PACKETS_RECEIVED_TOTAL, 1);
}

/// Record one H.264 depacketize failure (reassembly desync / resync event).
pub(crate) fn record_depacketize_failure() {
    aero_common::metrics::inc_counter(DEPACKETIZE_FAILURES_TOTAL, 1);
}

/// Re-publish the active-sessions gauge from a raw session count.
pub(crate) fn set_active_sessions(count: usize) {
    // usize → f64 is lossless for any plausible session count.
    #[allow(clippy::cast_precision_loss)]
    aero_common::metrics::set_gauge(ACTIVE_SESSIONS, count as f64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_names_are_stable() {
        // Guards against accidental renames that would break dashboards/alerts.
        assert_eq!(
            RTP_PACKETS_RECEIVED_TOTAL,
            "aero_whip_rtp_packets_received_total"
        );
        assert_eq!(
            DEPACKETIZE_FAILURES_TOTAL,
            "aero_whip_depacketize_failures_total"
        );
        assert_eq!(ACTIVE_SESSIONS, "aero_whip_active_sessions");
    }

    #[test]
    fn emit_helpers_do_not_panic() {
        // The helpers funnel into the process-wide `aero_common` registry; we
        // can't read absolute counter values back here without coupling to that
        // crate's internals, so we just exercise each path to prove the names
        // are accepted and the casts hold.
        record_rtp_packet();
        record_depacketize_failure();
        set_active_sessions(0);
        set_active_sessions(7);
    }
}
