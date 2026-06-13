//! Behavioral spam / flood detection (ROADMAP5 方向五).
//!
//! The content [`Moderator`](crate::moderator::Moderator) classifies a message's
//! TEXT — it cannot catch a spammer who blasts clean-worded links to many rooms
//! in seconds. This guard tracks each sender's recent send *behaviour* over a
//! sliding window — message rate, same-content room fan-out, and duplicate
//! repeats — and throttles a sender whose pattern crosses the configured
//! thresholds. State is per-process and in-memory (a spammer hitting a single
//! node is caught; cross-node aggregation is a future refinement, like the
//! per-IP rate limiter).

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use aero_common::{ParticipantId, RoomId};
use dashmap::DashMap;

/// Why a send was throttled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpamReason {
    /// Too many messages from the sender within the window (flooding).
    Rate,
    /// The same content sent to too many distinct rooms within the window
    /// (cross-room blast — the canonical spammer signature).
    Fanout,
    /// The same content repeated too many times within the window.
    Duplicate,
}

/// Outcome of a [`SpamGuard::record`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpamDecision {
    Allow,
    Throttle(SpamReason),
}

/// Tunable thresholds. Each is a strict ceiling — `max_messages = 20` admits 20
/// messages in the window and throttles the 21st.
#[derive(Debug, Clone, Copy)]
pub struct SpamThresholds {
    /// Sliding-window length.
    pub window: Duration,
    /// Max messages per sender per window.
    pub max_messages: usize,
    /// Max distinct rooms a single piece of content may be sent to per window.
    pub max_rooms: usize,
    /// Max identical-content repeats per window.
    pub max_duplicates: usize,
}

impl Default for SpamThresholds {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(10),
            max_messages: 20,
            // Keep max_rooms < max_duplicates so a cross-room blast of one message
            // trips `Fanout` (the worse signal) before `Duplicate`.
            max_rooms: 5,
            max_duplicates: 10,
        }
    }
}

struct Activity {
    /// `(when, room, content_hash)`, time-ordered, pruned to the window per call.
    events: VecDeque<(Instant, RoomId, u64)>,
}

/// Per-process behavioral spam detector. Cheap to share behind an `Arc`.
pub struct SpamGuard {
    thresholds: SpamThresholds,
    senders: DashMap<ParticipantId, Activity>,
}

impl SpamGuard {
    #[must_use]
    pub fn new(thresholds: SpamThresholds) -> Self {
        Self { thresholds, senders: DashMap::new() }
    }

    /// Record a send from `sender` to `room` with text fingerprint `content_hash`
    /// and decide whether it should be throttled. `now` is injectable for tests.
    ///
    /// The send is always recorded (a throttled attempt still counts as spam
    /// behaviour); the caller drops the message when the decision is `Throttle`.
    pub fn record(
        &self,
        sender: ParticipantId,
        room: RoomId,
        content_hash: u64,
        now: Instant,
    ) -> SpamDecision {
        let mut entry = self
            .senders
            .entry(sender)
            .or_insert_with(|| Activity { events: VecDeque::new() });
        let act = entry.value_mut();

        // Drop events that have aged out of the window (events are time-ordered).
        while let Some((t, _, _)) = act.events.front() {
            if now.duration_since(*t) > self.thresholds.window {
                act.events.pop_front();
            } else {
                break;
            }
        }
        act.events.push_back((now, room, content_hash));

        if act.events.len() > self.thresholds.max_messages {
            return SpamDecision::Throttle(SpamReason::Rate);
        }
        let same_content = act.events.iter().filter(|(_, _, h)| *h == content_hash);
        let (dup_count, rooms): (usize, HashSet<RoomId>) =
            same_content.fold((0, HashSet::new()), |(n, mut rs), (_, r, _)| {
                rs.insert(*r);
                (n + 1, rs)
            });
        if dup_count > self.thresholds.max_duplicates {
            return SpamDecision::Throttle(SpamReason::Duplicate);
        }
        if rooms.len() > self.thresholds.max_rooms {
            return SpamDecision::Throttle(SpamReason::Fanout);
        }
        SpamDecision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> SpamGuard {
        SpamGuard::new(SpamThresholds {
            window: Duration::from_secs(10),
            max_messages: 5,
            max_rooms: 3,
            max_duplicates: 5,
        })
    }

    #[test]
    fn rate_flood_is_throttled() {
        let g = guard();
        let sender = ParticipantId::new();
        let room = RoomId::new();
        let t0 = Instant::now();
        // 5 distinct messages to one room are fine; the 6th floods.
        for i in 0..5 {
            assert_eq!(g.record(sender, room, i, t0), SpamDecision::Allow, "msg {i} ok");
        }
        assert_eq!(g.record(sender, room, 99, t0), SpamDecision::Throttle(SpamReason::Rate));
    }

    #[test]
    fn cross_room_blast_of_same_content_is_throttled() {
        let g = guard();
        let sender = ParticipantId::new();
        let t0 = Instant::now();
        let hash = 0xdead_beef;
        // Same content to 3 rooms is allowed (max_rooms=3); the 4th distinct room
        // is a same-content blast → Fanout (dup ceiling is higher, so it trips first).
        for _ in 0..3 {
            assert_eq!(g.record(sender, RoomId::new(), hash, t0), SpamDecision::Allow);
        }
        assert_eq!(
            g.record(sender, RoomId::new(), hash, t0),
            SpamDecision::Throttle(SpamReason::Fanout),
            "same content to a 4th room is a cross-room blast",
        );
    }

    #[test]
    fn repeated_duplicate_in_one_room_is_throttled() {
        // max_duplicates < max_messages so duplicates trip before the rate ceiling.
        let g = SpamGuard::new(SpamThresholds {
            window: Duration::from_secs(10),
            max_messages: 50,
            max_rooms: 50,
            max_duplicates: 3,
        });
        let sender = ParticipantId::new();
        let room = RoomId::new();
        let t0 = Instant::now();
        for _ in 0..3 {
            assert_eq!(g.record(sender, room, 7, t0), SpamDecision::Allow);
        }
        assert_eq!(g.record(sender, room, 7, t0), SpamDecision::Throttle(SpamReason::Duplicate));
    }

    #[test]
    fn distinct_content_per_room_is_not_fanout() {
        let g = guard();
        let sender = ParticipantId::new();
        let t0 = Instant::now();
        // Different content to 5 rooms: not a same-content blast, only rate-bound.
        for i in 0..5 {
            assert_eq!(g.record(sender, RoomId::new(), i, t0), SpamDecision::Allow, "msg {i}");
        }
    }

    #[test]
    fn window_expiry_resets_the_count() {
        let g = guard();
        let sender = ParticipantId::new();
        let room = RoomId::new();
        let t0 = Instant::now();
        for i in 0..5 {
            assert_eq!(g.record(sender, room, i, t0), SpamDecision::Allow);
        }
        // After the window passes, the old events prune and the sender is fresh.
        let later = t0 + Duration::from_secs(11);
        assert_eq!(g.record(sender, room, 100, later), SpamDecision::Allow, "window reset");
    }

    #[test]
    fn senders_are_independent() {
        let g = guard();
        let a = ParticipantId::new();
        let b = ParticipantId::new();
        let room = RoomId::new();
        let t0 = Instant::now();
        for i in 0..6 {
            let _ = g.record(a, room, i, t0); // a floods
        }
        // b is unaffected by a's flooding.
        assert_eq!(g.record(b, room, 0, t0), SpamDecision::Allow);
    }
}
