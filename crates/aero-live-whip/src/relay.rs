//! WHIP→WHEP media relay hub.
//!
//! [`MediaRelay`] receives depacketized H.264 access units from a live WHIP
//! publisher and fans them out to any number of concurrent WHEP subscribers.
//! Each subscriber holds a [`Subscription`], which implements
//! [`NalSource`](crate::whep::NalSource) so it can drive a
//! [`WhepSession::run`](crate::whep::WhepSession::run) directly.
//!
//! ## Fan-out mechanism
//!
//! Internally the hub uses a [`tokio::sync::broadcast`] channel. The sender
//! end is owned by the relay; subscribers each own one
//! [`broadcast::Receiver`]. Because broadcast channels are MPSC-capable but
//! the publisher side here is single (one WHIP session at a time), the API
//! exposes only `publish` (single caller) / `subscribe` (many callers).
//!
//! A subscriber that cannot keep up will miss frames (the broadcast channel
//! drops the oldest messages for lagging receivers). That is the correct
//! behaviour for live streaming — slow viewers should not stall the publisher.
//!
//! ## Access-unit representation
//!
//! The WHIP ingest path produces **Annex-B** access units (`Bytes`) with a
//! 90 kHz `u64` timestamp. [`Subscription::next_access_unit`] adapts this
//! to the [`NalSource`] contract: Annex-B is split into raw NAL slices
//! (no start codes) and the timestamp is truncated to `u32` (RTP timestamps
//! wrap at 32 bits).
//!
//! ## Drop = unsubscribe
//!
//! Dropping a [`Subscription`] drops its [`broadcast::Receiver`], which
//! removes it from the fan-out set automatically. No explicit teardown is
//! needed.

use bytes::Bytes;
use tokio::sync::broadcast;
use tracing::trace;

use crate::hls_sink::split_annex_b;
use crate::whep::NalSource;

/// One access unit published through the relay.
///
/// `Bytes` is cheaply cloneable (reference-counted) so broadcasting a
/// clone per subscriber is zero-copy.
#[derive(Debug, Clone)]
struct RelayAu {
    /// Annex-B encoded access unit (one or more NAL units, each prefixed with
    /// the 4-byte start code `00 00 00 01`).
    annex_b: Bytes,
    /// Presentation timestamp in 90 kHz ticks (same unit as
    /// [`MediaSink::on_video_au`](crate::hls_sink::MediaSink::on_video_au)).
    ts_90k: u64,
}

/// Default broadcast channel capacity.  Holds up to this many access units
/// before the oldest one is overwritten for lagging subscribers. At 30 fps
/// this is ~1 s of buffering.
const DEFAULT_CAPACITY: usize = 32;

/// Hub that relays H.264 access units from one WHIP publisher to N WHEP
/// subscribers.
///
/// # Usage
///
/// ```rust,ignore
/// let relay = MediaRelay::new();
/// // In the publisher task:
/// relay.publish(annex_b_bytes, pts_90k);
/// // In each subscriber task:
/// let sub = relay.subscribe();
/// whep_session.run(socket, sub).await?;
/// ```
pub struct MediaRelay {
    tx: broadcast::Sender<RelayAu>,
}

impl MediaRelay {
    /// Create a new relay with the default fan-out buffer size.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Create a relay with an explicit broadcast buffer capacity.
    ///
    /// A larger capacity tolerates burstier subscribers; a smaller capacity
    /// reduces per-subscriber memory overhead.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publish one H.264 access unit to all current subscribers.
    ///
    /// `annex_b` is the complete access unit in Annex-B form (the same bytes
    /// [`MediaSink::on_video_au`](crate::hls_sink::MediaSink::on_video_au) receives
    /// from the depacketizer). `ts_90k` is the 90 kHz presentation timestamp.
    ///
    /// Returns the number of active subscribers that received this AU. Returns
    /// `0` if there are no subscribers (the send silently succeeds but nobody
    /// is listening).
    pub fn publish(&self, annex_b: Bytes, ts_90k: u64) -> usize {
        let au = RelayAu { annex_b, ts_90k };
        match self.tx.send(au) {
            Ok(n) => {
                trace!(subscribers = n, ts_90k, "relay: published AU");
                n
            }
            // No active receivers — not an error; the relay simply has no
            // subscribers right now.
            Err(_) => 0,
        }
    }

    /// Open a new subscription.  The subscriber will receive all access units
    /// published *after* this call; earlier AUs are not replayed.
    ///
    /// Dropping the returned [`Subscription`] unsubscribes automatically.
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        Subscription {
            rx: self.tx.subscribe(),
        }
    }

    /// Number of subscribers currently active (approximate — updated lazily by
    /// the broadcast machinery).
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for MediaRelay {
    fn default() -> Self {
        Self::new()
    }
}

/// A live subscription to a [`MediaRelay`].
///
/// Implements [`NalSource`] so it can be passed directly to
/// [`WhepSession::run`](crate::whep::WhepSession::run).
///
/// # Blocking behaviour
///
/// [`next_access_unit`](NalSource::next_access_unit) is a *synchronous* poll:
/// it returns `None` immediately if no AU is buffered rather than blocking.
/// This matches the contract [`WhepSession::run`] expects (it drives the poll
/// inside its own async loop and goes back to waiting for RTCP if no AU is
/// ready yet).
///
/// # Lagged subscribers
///
/// If the subscriber falls more than `capacity` AUs behind, the broadcast
/// channel will skip (overwrite) the oldest frames. The subscriber detects
/// this via [`broadcast::error::RecvError::Lagged`] and silently resumes from
/// the next available AU — live viewers see a brief glitch but stay connected.
pub struct Subscription {
    rx: broadcast::Receiver<RelayAu>,
}

impl NalSource for Subscription {
    /// Return the next buffered access unit as a `(nals, rtp_ts)` pair, or
    /// `None` if no AU is ready right now.
    ///
    /// `nals` contains the raw NAL unit bytes without Annex-B start codes,
    /// matching the format [`WhepPacketizer`](crate::packetize::WhepPacketizer)
    /// consumes. `rtp_ts` is the 90 kHz timestamp truncated to `u32` (RTP
    /// timestamp space).
    fn next_access_unit(&mut self) -> Option<(Vec<Vec<u8>>, u32)> {
        loop {
            match self.rx.try_recv() {
                Ok(au) => {
                    // Split Annex-B into individual NAL units (start-code stripped).
                    let nals: Vec<Vec<u8>> = split_annex_b(&au.annex_b)
                        .into_iter()
                        .map(<[u8]>::to_vec)
                        .collect();
                    if nals.is_empty() {
                        // Degenerate AU with no parseable NALs — skip it.
                        continue;
                    }
                    // RTP timestamps are 32-bit; wrap by truncation (standard
                    // RTP behaviour — the receiver handles wrap-around via seq).
                    #[allow(clippy::cast_possible_truncation)]
                    let rtp_ts = au.ts_90k as u32;
                    return Some((nals, rtp_ts));
                }
                // Nothing buffered right now, or the relay was dropped
                // (publisher gone). In both cases signal end-of-stream/no-data.
                Err(
                    broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed,
                ) => return None,
                // Fell behind: skip the lost AUs and try again immediately.
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    trace!(skipped = n, "relay: subscriber lagged, skipping AUs");
                    // loop to try_recv again
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    /// Build a minimal Annex-B access unit from `(nal_header, body)` pairs.
    fn annex_b(nals: &[(u8, &[u8])]) -> Bytes {
        let mut out = Vec::new();
        for (header, body) in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.push(*header);
            out.extend_from_slice(body);
        }
        Bytes::from(out)
    }

    // ── Basic publish/subscribe tests ────────────────────────────────────────

    #[test]
    fn publish_delivers_identical_bytes_and_ts_to_subscriber() {
        let relay = MediaRelay::new();
        let mut sub = relay.subscribe();

        let au = annex_b(&[(0x65, &[0xAA, 0xBB, 0xCC])]);
        let ts: u64 = 90_000;
        relay.publish(au.clone(), ts);

        let (nals, rtp_ts) = sub.next_access_unit().expect("AU must be delivered");
        assert_eq!(nals.len(), 1, "one NAL unit");
        // The NAL unit in the subscription is without the start code.
        assert_eq!(nals[0], &[0x65, 0xAA, 0xBB, 0xCC]);
        // Timestamp truncated to u32 (same value for this small number).
        #[allow(clippy::cast_possible_truncation)]
        let expected_ts = ts as u32;
        assert_eq!(rtp_ts, expected_ts);
    }

    #[test]
    fn subscriber_joining_after_publish_misses_earlier_au() {
        let relay = MediaRelay::new();

        // Publish BEFORE subscribing.
        let au = annex_b(&[(0x65, &[0x01])]);
        relay.publish(au, 0);

        // Subscribe AFTER the publish.
        let mut sub = relay.subscribe();

        // Must not receive the AU that was sent before subscription.
        assert!(
            sub.next_access_unit().is_none(),
            "late subscriber must not see pre-subscription AUs"
        );
    }

    #[test]
    fn dropping_subscriber_stops_delivery() {
        let relay = MediaRelay::new();
        let sub = relay.subscribe();
        assert_eq!(relay.subscriber_count(), 1);

        // Drop the subscription.
        drop(sub);
        // The relay should report no active subscribers.
        assert_eq!(relay.subscriber_count(), 0);

        // Publishing after drop is fine (no panic, just 0 receivers).
        let n = relay.publish(annex_b(&[(0x41, &[0x01])]), 0);
        assert_eq!(n, 0, "no subscribers → 0 deliveries");
    }

    #[test]
    fn n_subscribers_each_receive_a_copy() {
        let relay = MediaRelay::new();
        let mut subs: Vec<Subscription> = (0..4).map(|_| relay.subscribe()).collect();

        let au = annex_b(&[(0x67, &[0x42, 0x00, 0x1F]), (0x65, &[0xDE, 0xAD])]);
        let ts: u64 = 270_000;
        let delivered = relay.publish(au, ts);
        assert_eq!(delivered, 4, "all four subscribers should receive it");

        for (i, sub) in subs.iter_mut().enumerate() {
            let (nals, rtp_ts) = sub
                .next_access_unit()
                .unwrap_or_else(|| panic!("subscriber {i} must receive the AU"));
            assert_eq!(nals.len(), 2, "subscriber {i}: two NAL units");
            assert_eq!(nals[0], &[0x67, 0x42, 0x00, 0x1F], "subscriber {i}: SPS");
            assert_eq!(nals[1], &[0x65, 0xDE, 0xAD], "subscriber {i}: IDR");
            #[allow(clippy::cast_possible_truncation)]
            let expected_ts = ts as u32;
            assert_eq!(rtp_ts, expected_ts, "subscriber {i}: timestamp");
        }
    }

    #[test]
    fn subscriber_receives_multiple_aus_in_order() {
        let relay = MediaRelay::new();
        let mut sub = relay.subscribe();

        let aus: Vec<(Bytes, u64)> = vec![
            (annex_b(&[(0x67, &[0x42, 0x00, 0x1F]), (0x65, &[0x01])]), 0),
            (annex_b(&[(0x41, &[0x02])]), 3_000),
            (annex_b(&[(0x41, &[0x03])]), 6_000),
        ];

        for (au, ts) in &aus {
            relay.publish(au.clone(), *ts);
        }

        for (i, (_, expected_ts)) in aus.iter().enumerate() {
            let (nals, rtp_ts) = sub
                .next_access_unit()
                .unwrap_or_else(|| panic!("AU {i} must be delivered"));
            assert!(!nals.is_empty(), "AU {i}: must have NALs");
            #[allow(clippy::cast_possible_truncation)]
            let e = *expected_ts as u32;
            assert_eq!(rtp_ts, e, "AU {i}: timestamp mismatch");
        }

        // No more AUs.
        assert!(sub.next_access_unit().is_none(), "no further AUs buffered");
    }

    // ── NalSource / relay-backed subscription tests ──────────────────────────

    #[test]
    fn nal_source_yields_published_access_units_in_order() {
        // This directly exercises the NalSource impl on Subscription.
        let relay = MediaRelay::new();
        let mut source: Box<dyn NalSource> = Box::new(relay.subscribe());

        // Publish three AUs with known NAL content.
        let payloads: &[(u8, &[u8], u64)] = &[
            (0x65, &[0x10, 0x20], 90_000),
            (0x41, &[0x30, 0x40], 93_000),
            (0x41, &[0x50, 0x60], 96_000),
        ];
        for (header, body, ts) in payloads {
            relay.publish(annex_b(&[(*header, *body)]), *ts);
        }

        for (i, (header, body, ts)) in payloads.iter().enumerate() {
            let (nals, rtp_ts) = source
                .next_access_unit()
                .unwrap_or_else(|| panic!("NalSource AU {i} must be available"));
            assert_eq!(nals.len(), 1, "AU {i}: one NAL");
            let mut expected_nal = vec![*header];
            expected_nal.extend_from_slice(body);
            assert_eq!(nals[0], expected_nal, "AU {i}: NAL bytes");
            #[allow(clippy::cast_possible_truncation)]
            let e = *ts as u32;
            assert_eq!(rtp_ts, e, "AU {i}: timestamp");
        }

        // After all three, source is dry.
        assert!(
            source.next_access_unit().is_none(),
            "NalSource must return None when no more AUs are buffered"
        );
    }

    #[test]
    fn no_subscribers_publish_returns_zero() {
        let relay = MediaRelay::new();
        let n = relay.publish(annex_b(&[(0x41, &[0x01])]), 0);
        assert_eq!(n, 0);
    }

    #[test]
    fn publish_to_relay_with_no_capacity_for_lagged_subscriber() {
        // Use a tiny capacity (1) and publish 5 AUs before the subscriber reads.
        // The subscriber should silently skip lagged AUs and return the most
        // recent ones without panicking.
        let relay = MediaRelay::with_capacity(1);
        let mut sub = relay.subscribe();

        for i in 0u8..5 {
            relay.publish(annex_b(&[(0x41, &[i])]), u64::from(i) * 3_000);
        }

        // The subscriber should get at least the last AU (no panic on lag).
        let result = sub.next_access_unit();
        assert!(
            result.is_some(),
            "must receive at least one AU after lagging"
        );
    }
}
