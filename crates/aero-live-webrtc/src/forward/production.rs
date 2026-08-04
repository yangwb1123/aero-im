//! Publisher-scoped routing table used by production SFU sessions.
//!
//! SDP MIDs are only unique inside one peer connection. This table therefore
//! keys every source as `(publisher, pub_mid)` and keeps a reverse
//! `(subscriber, out_mid)` lookup for routing PLI/FIR/REMB back upstream.

use std::collections::{HashMap, HashSet};

use aero_common::{ParticipantId, SfuSubscription};

use crate::remap::{ForwardTarget, RemappedRtp, RtpKey, RtpRemapper};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CallTrack {
    pub(super) publisher: ParticipantId,
    pub(super) mid: String,
}

impl CallTrack {
    pub(super) fn new(publisher: ParticipantId, mid: &str) -> Self {
        Self {
            publisher,
            mid: mid.to_owned(),
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct CallForwardTable {
    routes: HashMap<CallTrack, Vec<ForwardTarget>>,
    reverse: HashMap<(ParticipantId, String), CallTrack>,
    remappers: HashMap<(ParticipantId, String), RtpRemapper>,
}

impl CallForwardTable {
    /// Atomically replace every route owned by `subscriber`.
    ///
    /// Existing remap state is retained for an unchanged `out_mid`, avoiding an
    /// unnecessary sequence/timestamp reset when only another publisher joins.
    pub(super) fn replace_subscriber(
        &mut self,
        subscriber: ParticipantId,
        routes: &[SfuSubscription],
    ) {
        let new_out_mids: HashSet<&str> =
            routes.iter().map(|route| route.out_mid.as_str()).collect();
        self.routes.values_mut().for_each(|targets| {
            targets.retain(|target| target.subscriber != subscriber);
        });
        self.routes.retain(|_, targets| !targets.is_empty());
        self.reverse.retain(|(active, _), _| *active != subscriber);
        self.remappers.retain(|(active, out_mid), _| {
            *active != subscriber || new_out_mids.contains(out_mid.as_str())
        });

        for route in routes {
            let source = CallTrack::new(route.publisher, &route.pub_mid);
            let target = ForwardTarget {
                subscriber,
                out_mid: route.out_mid.clone(),
            };
            let targets = self.routes.entry(source.clone()).or_default();
            if !targets.contains(&target) {
                targets.push(target);
            }
            self.reverse
                .insert((subscriber, route.out_mid.clone()), source);
            self.remappers
                .entry((subscriber, route.out_mid.clone()))
                .or_default();
        }
    }

    pub(super) fn unlink_subscriber(&mut self, subscriber: ParticipantId) {
        self.routes.values_mut().for_each(|targets| {
            targets.retain(|target| target.subscriber != subscriber);
        });
        self.routes.retain(|_, targets| !targets.is_empty());
        self.reverse.retain(|(active, _), _| *active != subscriber);
        self.remappers
            .retain(|(active, _), _| *active != subscriber);
    }

    pub(super) fn unlink_publisher(&mut self, publisher: ParticipantId) {
        let removed_out_mids: HashSet<(ParticipantId, String)> = self
            .reverse
            .iter()
            .filter_map(|(target, source)| {
                (source.publisher == publisher).then_some(target.clone())
            })
            .collect();
        self.routes
            .retain(|source, _| source.publisher != publisher);
        self.reverse
            .retain(|_, source| source.publisher != publisher);
        self.remappers
            .retain(|target, _| !removed_out_mids.contains(target));
    }

    pub(super) fn targets(&self, publisher: ParticipantId, pub_mid: &str) -> &[ForwardTarget] {
        self.routes
            .get(&CallTrack::new(publisher, pub_mid))
            .map_or(&[], Vec::as_slice)
    }

    pub(super) fn remap_for(
        &mut self,
        subscriber: ParticipantId,
        out_mid: &str,
        key: RtpKey,
    ) -> Option<RemappedRtp> {
        self.remappers
            .get_mut(&(subscriber, out_mid.to_owned()))
            .map(|remapper| remapper.remap(key))
    }

    pub(super) fn source_for(&self, subscriber: ParticipantId, out_mid: &str) -> Option<CallTrack> {
        self.reverse.get(&(subscriber, out_mid.to_owned())).cloned()
    }

    pub(super) fn sources_for_subscriber(&self, subscriber: ParticipantId) -> Vec<CallTrack> {
        self.reverse
            .iter()
            .filter_map(|((active, _), source)| (*active == subscriber).then_some(source.clone()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    pub(super) fn publisher_tracks(&self, publisher: ParticipantId) -> Vec<CallTrack> {
        self.routes
            .keys()
            .filter(|source| source.publisher == publisher)
            .cloned()
            .collect()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_publisher_mids_are_distinct_and_reverse_resolvable() {
        let publisher_a = ParticipantId::new();
        let publisher_b = ParticipantId::new();
        let subscriber = ParticipantId::new();
        let mut table = CallForwardTable::default();
        table.replace_subscriber(
            subscriber,
            &[
                SfuSubscription {
                    publisher: publisher_a,
                    pub_mid: "0".into(),
                    out_mid: "recv-a".into(),
                },
                SfuSubscription {
                    publisher: publisher_b,
                    pub_mid: "0".into(),
                    out_mid: "recv-b".into(),
                },
            ],
        );

        assert_eq!(table.targets(publisher_a, "0")[0].out_mid, "recv-a");
        assert_eq!(table.targets(publisher_b, "0")[0].out_mid, "recv-b");
        assert_eq!(
            table.source_for(subscriber, "recv-b"),
            Some(CallTrack::new(publisher_b, "0"))
        );
    }
}
