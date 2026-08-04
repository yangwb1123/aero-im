//! Expiring puller leases for the cross-node RTP egress.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aero_common::CallId;
use aero_live_webrtc::{BOUND_BRIDGE_COMPAT_VERSION, BOUND_BRIDGE_VERSION};
use parking_lot::Mutex;
use uuid::Uuid;

/// Pullers renew every 15 seconds: three full missed renewals are tolerated,
/// and the lease expires on the fourth renewal boundary.
pub(crate) const DEFAULT_SUBSCRIBER_LEASE: Duration = Duration::from_secs(60);
const REVOKED_GENERATION_TTL: Duration = Duration::from_secs(300);
const MAX_SUBSCRIBER_GENERATIONS_PER_CALL: usize = 512;
const MAX_SUBSCRIBER_CALLS: usize = 16_384;
const MAX_SUBSCRIBER_GENERATIONS: usize = 262_144;

#[derive(Clone, Copy)]
struct RegistryLimits {
    calls: usize,
    generations: usize,
    per_call: usize,
}

impl Default for RegistryLimits {
    fn default() -> Self {
        Self {
            calls: MAX_SUBSCRIBER_CALLS,
            generations: MAX_SUBSCRIBER_GENERATIONS,
            per_call: MAX_SUBSCRIBER_GENERATIONS_PER_CALL,
        }
    }
}

/// The optional UUID is a pull incarnation. Keeping overlapping generations as
/// separate leases prevents a delayed unsubscribe from an old UDP socket from
/// deleting a replacement that reused the same address.
type SubscriptionKey = (SocketAddr, Option<Uuid>);

#[derive(Clone, Copy)]
enum SubscriptionState {
    /// `None` is a rolling-upgrade legacy puller removed by call teardown.
    Live {
        expires_at: Option<Instant>,
        wire_version: Option<u8>,
    },
    /// Fences an in-flight refresh which arrives after unsubscribe.
    Revoked { expires_at: Instant },
}

type CallSubscribers = HashMap<SubscriptionKey, SubscriptionState>;

/// One live egress destination together with its negotiated wire generation.
///
/// `None` identifies a pre-upgrade puller that can only decode legacy v2
/// datagrams. Every upgraded puller has an exact UUID used by the v3/v4
/// generation-bound envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BridgeSubscriberTarget {
    pub(crate) addr: SocketAddr,
    pub(crate) generation: Option<Uuid>,
    pub(crate) wire_version: Option<u8>,
}

fn retain_state(state: &SubscriptionState, now: Instant) -> bool {
    match state {
        SubscriptionState::Live {
            expires_at: None, ..
        } => true,
        SubscriptionState::Live {
            expires_at: Some(deadline),
            ..
        }
        | SubscriptionState::Revoked {
            expires_at: deadline,
        } => *deadline > now,
    }
}

fn is_live(state: &SubscriptionState, now: Instant) -> bool {
    match state {
        SubscriptionState::Live {
            expires_at: None, ..
        } => true,
        SubscriptionState::Live {
            expires_at: Some(deadline),
            ..
        } => *deadline > now,
        SubscriptionState::Revoked { .. } => false,
    }
}

fn prune_call(calls: &mut HashMap<CallId, CallSubscribers>, call: CallId, now: Instant) {
    let empty = calls.get_mut(&call).is_some_and(|subscribers| {
        subscribers.retain(|_, state| retain_state(state, now));
        subscribers.is_empty()
    });
    if empty {
        calls.remove(&call);
    }
}

fn total_generations(calls: &HashMap<CallId, CallSubscribers>) -> usize {
    calls.values().map(HashMap::len).sum()
}

/// Registry of pulling-node addresses subscribed to each call's bridged RTP.
///
/// A subscription is a lease, not permanent process state. The puller refreshes
/// it through the authenticated subscribe endpoint; an aborted pull therefore
/// stops receiving after a bounded interval even when its best-effort
/// unsubscribe cannot reach the owner.
#[derive(Clone)]
pub struct BridgeSubscriberRegistry {
    inner: Arc<Mutex<HashMap<CallId, CallSubscribers>>>,
    lease: Duration,
    limits: RegistryLimits,
}

impl Default for BridgeSubscriberRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            lease: DEFAULT_SUBSCRIBER_LEASE,
            limits: RegistryLimits::default(),
        }
    }
}

impl BridgeSubscriberRegistry {
    /// Override the lease duration, primarily for deterministic tests.
    #[must_use]
    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    #[cfg(test)]
    fn with_limits(mut self, calls: usize, generations: usize, per_call: usize) -> Self {
        self.limits = RegistryLimits {
            calls,
            generations,
            per_call,
        };
        self
    }

    /// Register or renew `addr` as a puller for `call`.
    pub fn subscribe(&self, call: CallId, addr: SocketAddr) {
        let _ = self.subscribe_generation(call, addr, Uuid::nil(), self.lease);
    }

    /// Register a v3/v4 puller generation for an explicitly negotiated lease.
    pub fn subscribe_generation(
        &self,
        call: CallId,
        addr: SocketAddr,
        generation: Uuid,
        lease: Duration,
    ) -> bool {
        self.subscribe_generation_version(call, addr, generation, lease, BOUND_BRIDGE_VERSION)
    }

    /// Register a generation-aware puller with its negotiated bound-frame
    /// version. Version 3 is retained only for mixed-binary rolling upgrades.
    pub fn subscribe_generation_version(
        &self,
        call: CallId,
        addr: SocketAddr,
        generation: Uuid,
        lease: Duration,
        wire_version: u8,
    ) -> bool {
        if !matches!(
            wire_version,
            BOUND_BRIDGE_COMPAT_VERSION | BOUND_BRIDGE_VERSION
        ) {
            return false;
        }
        self.subscribe_at_version(
            call,
            addr,
            Some(generation),
            lease,
            wire_version,
            Instant::now(),
        )
    }

    #[cfg(test)]
    fn subscribe_at(
        &self,
        call: CallId,
        addr: SocketAddr,
        generation: Option<Uuid>,
        lease: Duration,
        now: Instant,
    ) -> bool {
        self.subscribe_at_version(call, addr, generation, lease, BOUND_BRIDGE_VERSION, now)
    }

    fn subscribe_at_version(
        &self,
        call: CallId,
        addr: SocketAddr,
        generation: Option<Uuid>,
        lease: Duration,
        wire_version: u8,
        now: Instant,
    ) -> bool {
        let expires_at = now.checked_add(lease).unwrap_or(now);
        let revoked_until = now.checked_add(REVOKED_GENERATION_TTL).unwrap_or(now);
        let mut map = self.inner.lock();
        let key = (addr, generation);
        prune_call(&mut map, call, now);
        if let Some(subscribers) = map.get_mut(&call) {
            match subscribers.get(&key) {
                Some(SubscriptionState::Revoked { .. }) => return false,
                Some(SubscriptionState::Live {
                    wire_version: existing_version,
                    ..
                }) => {
                    if *existing_version != Some(wire_version) {
                        return false;
                    }
                    subscribers.insert(
                        key,
                        SubscriptionState::Live {
                            expires_at: Some(expires_at),
                            wire_version: Some(wire_version),
                        },
                    );
                    return true;
                }
                None => {}
            }
        }

        let (len, legacy_at_addr) = map.get(&call).map_or((0, 0), |subscribers| {
            (
                subscribers.len(),
                usize::from(subscribers.contains_key(&(addr, None))),
            )
        });
        let resulting_len = len.saturating_sub(legacy_at_addr).saturating_add(1);
        let resulting_total = total_generations(&map)
            .saturating_sub(legacy_at_addr)
            .saturating_add(1);
        if resulting_len > self.limits.per_call
            || resulting_total > self.limits.generations
            || (!map.contains_key(&call) && map.len() >= self.limits.calls)
        {
            return false;
        }

        let subscribers = map.entry(call).or_default();
        // This generation owns the UDP address. Retire every predecessor so
        // its delayed refresh cannot revive a dead socket after the replacement
        // unsubscribes.
        subscribers.remove(&(addr, None));
        for ((candidate, candidate_generation), state) in subscribers.iter_mut() {
            if *candidate == addr
                && candidate_generation.is_some()
                && *candidate_generation != generation
            {
                *state = SubscriptionState::Revoked {
                    expires_at: revoked_until,
                };
            }
        }
        subscribers.insert(
            key,
            SubscriptionState::Live {
                expires_at: Some(expires_at),
                wire_version: Some(wire_version),
            },
        );
        true
    }

    /// Register a pre-lease puller during a rolling upgrade.
    ///
    /// Old binaries announce only `{call_id, addr}` and never refresh. Expiring
    /// those entries would cut active calls as soon as the owner upgrades. This
    /// compatibility entry is still bounded by explicit unsubscribe and call
    /// teardown; all upgraded pullers advertise `lease_secs`.
    pub fn subscribe_legacy(&self, call: CallId, addr: SocketAddr) -> bool {
        let now = Instant::now();
        let mut map = self.inner.lock();
        prune_call(&mut map, call, now);
        if map.get(&call).is_some_and(|subscribers| {
            subscribers
                .keys()
                .any(|(candidate, generation)| *candidate == addr && generation.is_some())
        }) {
            // A delayed pre-upgrade request must not downgrade an address that
            // already has a generation-aware live lease or tombstone.
            return true;
        }
        let key = (addr, None);
        let (len, key_exists) = map.get(&call).map_or((0, false), |subscribers| {
            (subscribers.len(), subscribers.contains_key(&key))
        });
        if !key_exists
            && (len >= self.limits.per_call
                || total_generations(&map) >= self.limits.generations
                || (!map.contains_key(&call) && map.len() >= self.limits.calls))
        {
            return false;
        }
        map.entry(call).or_default().insert(
            key,
            SubscriptionState::Live {
                expires_at: None,
                wire_version: None,
            },
        );
        true
    }

    /// Remove every generation for `addr`; used only by local lifecycle code.
    pub fn unsubscribe(&self, call: CallId, addr: SocketAddr) {
        self.unsubscribe_matching(call, addr, None, true);
    }

    /// Remove one exact remote pull incarnation.
    ///
    /// A delayed unsubscribe from a superseded socket cannot remove a newer
    /// generation at the same address.
    pub fn unsubscribe_generation(&self, call: CallId, addr: SocketAddr, generation: Option<Uuid>) {
        self.unsubscribe_generation_at(call, addr, generation, Instant::now(), false);
    }

    /// Revoke an exact generation for a call whose egress lifecycle was
    /// atomically verified by the supervisor. This may create a tombstone
    /// before the corresponding initial subscribe arrives.
    pub(crate) fn revoke_generation(
        &self,
        call: CallId,
        addr: SocketAddr,
        generation: Option<Uuid>,
    ) {
        self.unsubscribe_generation_at(call, addr, generation, Instant::now(), true);
    }

    fn unsubscribe_generation_at(
        &self,
        call: CallId,
        addr: SocketAddr,
        generation: Option<Uuid>,
        now: Instant,
        allow_create: bool,
    ) {
        if generation.is_none() {
            self.unsubscribe_matching(call, addr, generation, false);
            return;
        }
        let mut map = self.inner.lock();
        prune_call(&mut map, call, now);
        if !allow_create && !map.contains_key(&call) {
            return;
        }
        let key = (addr, generation);
        let (len, key_exists) = map.get(&call).map_or((0, false), |subscribers| {
            (subscribers.len(), subscribers.contains_key(&key))
        });
        if !key_exists
            && (len >= self.limits.per_call
                || total_generations(&map) >= self.limits.generations
                || (!map.contains_key(&call) && map.len() >= self.limits.calls))
        {
            return;
        }
        let expires_at = now.checked_add(REVOKED_GENERATION_TTL).unwrap_or(now);
        map.entry(call)
            .or_default()
            .insert(key, SubscriptionState::Revoked { expires_at });
    }

    fn unsubscribe_matching(
        &self,
        call: CallId,
        addr: SocketAddr,
        generation: Option<Uuid>,
        all_generations: bool,
    ) {
        let mut map = self.inner.lock();
        let empty = map.get_mut(&call).is_some_and(|subscribers| {
            if all_generations {
                subscribers.retain(|(candidate, _), _| *candidate != addr);
            } else {
                subscribers.remove(&(addr, generation));
            }
            subscribers.is_empty()
        });
        if empty {
            map.remove(&call);
        }
    }

    /// Remove every puller for a call during call/egress teardown.
    pub fn clear_call(&self, call: CallId) {
        self.inner.lock().remove(&call);
    }

    /// Prune expired leases and generation tombstones across every call.
    ///
    /// The heartbeat timer invokes this even when no RTP is flowing, so outer
    /// map entries cannot accumulate solely because fanout is idle.
    pub fn prune_expired(&self) -> usize {
        let now = Instant::now();
        let mut map = self.inner.lock();
        let before: usize = map.values().map(HashMap::len).sum();
        map.retain(|_, subscribers| {
            subscribers.retain(|_, state| retain_state(state, now));
            !subscribers.is_empty()
        });
        let after: usize = map.values().map(HashMap::len).sum();
        before.saturating_sub(after)
    }

    /// Return only live pullers, pruning expired leases before every RTP fanout.
    #[must_use]
    pub fn subscribers(&self, call: CallId) -> Vec<SocketAddr> {
        self.subscribers_at(call, Instant::now())
    }

    fn subscribers_at(&self, call: CallId, now: Instant) -> Vec<SocketAddr> {
        self.targets_at(call, now)
            .into_iter()
            .map(|target| target.addr)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    }

    /// Return each live destination with the generation its wire frame must use.
    pub(crate) fn targets(&self, call: CallId) -> Vec<BridgeSubscriberTarget> {
        self.targets_at(call, Instant::now())
    }

    fn targets_at(&self, call: CallId, now: Instant) -> Vec<BridgeSubscriberTarget> {
        let mut map = self.inner.lock();
        let (targets, empty) = match map.get_mut(&call) {
            Some(subscribers) => {
                subscribers.retain(|_, state| retain_state(state, now));
                (
                    subscribers
                        .iter()
                        .filter(|(_, state)| is_live(state, now))
                        .filter_map(|((addr, generation), state)| {
                            let wire_version = match state {
                                SubscriptionState::Live { wire_version, .. } => *wire_version,
                                SubscriptionState::Revoked { .. } => return None,
                            };
                            Some(BridgeSubscriberTarget {
                                addr: *addr,
                                generation: *generation,
                                wire_version,
                            })
                        })
                        .collect(),
                    subscribers.is_empty(),
                )
            }
            None => return Vec::new(),
        };
        if empty {
            map.remove(&call);
        }
        targets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_renews_unsubscribes_and_expires_per_address() {
        let lease = Duration::from_secs(60);
        let reg = BridgeSubscriberRegistry::default().with_lease(lease);
        let call = CallId::new();
        let other = CallId::new();
        let a: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:5001".parse().unwrap();
        let legacy: SocketAddr = "127.0.0.1:5002".parse().unwrap();
        let start = Instant::now();
        let generation_a = Uuid::new_v4();
        let generation_b = Uuid::new_v4();

        assert!(reg.subscribe_at(call, a, Some(generation_a), lease, start));
        assert!(reg.subscribe_legacy(call, legacy));
        assert!(reg.subscribe_at(other, b, Some(generation_b), lease, start));
        let renewed_at = start + Duration::from_secs(40);
        assert!(reg.subscribe_at(call, a, Some(generation_a), lease, renewed_at));
        assert!(reg.subscribe_at(call, b, Some(generation_b), lease, renewed_at));
        let observed_at = start + Duration::from_secs(80);

        let mut live = reg.subscribers_at(call, observed_at);
        live.sort();
        assert_eq!(
            live,
            vec![a, b, legacy],
            "renewed and rolling-upgrade legacy leases survive"
        );
        assert!(
            reg.subscribers_at(other, observed_at).is_empty(),
            "old lease expires"
        );
        reg.unsubscribe(call, a);
        let mut remaining = reg.subscribers(call);
        remaining.sort();
        assert_eq!(remaining, vec![b, legacy]);
        reg.unsubscribe(call, b);
        reg.unsubscribe(call, legacy);
        assert!(reg.subscribers(call).is_empty());
    }

    #[test]
    fn stale_unsubscribe_cannot_remove_replacement_generation() {
        let reg = BridgeSubscriberRegistry::default();
        let call = CallId::new();
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let old = Uuid::new_v4();
        let replacement = Uuid::new_v4();
        assert!(reg.subscribe_generation(call, addr, old, Duration::from_secs(60)));
        assert!(reg.subscribe_generation(call, addr, replacement, Duration::from_secs(60)));

        reg.unsubscribe_generation(call, addr, Some(old));
        assert_eq!(reg.subscribers(call), vec![addr]);
        reg.unsubscribe_generation(call, addr, Some(old));
        assert_eq!(reg.subscribers(call), vec![addr]);
        reg.unsubscribe_generation(call, addr, Some(replacement));
        assert!(reg.subscribers(call).is_empty());
    }

    #[test]
    fn unsubscribe_tombstone_rejects_late_refresh_but_not_replacement() {
        let reg = BridgeSubscriberRegistry::default();
        let call = CallId::new();
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let old = Uuid::new_v4();
        let replacement = Uuid::new_v4();
        let start = Instant::now();
        assert!(reg.subscribe_at(call, addr, Some(old), Duration::from_secs(60), start));
        reg.unsubscribe_generation_at(call, addr, Some(old), start + Duration::from_secs(1), false);
        assert!(
            !reg.subscribe_at(
                call,
                addr,
                Some(old),
                Duration::from_secs(60),
                start + Duration::from_secs(2),
            ),
            "late refresh of a revoked generation is fenced"
        );
        assert!(reg.subscribe_at(
            call,
            addr,
            Some(replacement),
            Duration::from_secs(60),
            start + Duration::from_secs(2),
        ));
        assert_eq!(
            reg.subscribers_at(call, start + Duration::from_secs(3)),
            vec![addr]
        );
    }

    #[test]
    fn clear_call_does_not_touch_other_calls() {
        let reg = BridgeSubscriberRegistry::default();
        let call = CallId::new();
        let other = CallId::new();
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        reg.subscribe(call, addr);
        reg.subscribe(other, addr);

        reg.clear_call(call);
        assert!(reg.subscribers(call).is_empty());
        assert_eq!(reg.subscribers(other), vec![addr]);
    }

    #[test]
    fn generation_upgrade_retires_legacy_and_delayed_legacy_cannot_downgrade() {
        let reg = BridgeSubscriberRegistry::default();
        let call = CallId::new();
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let generation = Uuid::new_v4();

        assert!(reg.subscribe_legacy(call, addr));
        assert!(reg.subscribe_generation(call, addr, generation, Duration::from_secs(60)));
        reg.unsubscribe_generation(call, addr, Some(generation));
        assert!(reg.subscribers(call).is_empty());
        assert!(
            reg.subscribe_legacy(call, addr),
            "delayed legacy request is accepted as a no-op"
        );
        assert!(
            reg.subscribers(call).is_empty(),
            "legacy cannot revive the retired address"
        );
    }

    #[test]
    fn replacement_generation_does_not_fall_back_to_the_old_socket() {
        let reg = BridgeSubscriberRegistry::default();
        let call = CallId::new();
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let old = Uuid::new_v4();
        let replacement = Uuid::new_v4();

        assert!(reg.subscribe_generation(call, addr, old, Duration::from_secs(60)));
        assert!(reg.subscribe_generation(call, addr, replacement, Duration::from_secs(60)));
        reg.unsubscribe_generation(call, addr, Some(replacement));
        assert!(reg.subscribers(call).is_empty());
        assert!(
            !reg.subscribe_generation(call, addr, old, Duration::from_secs(60)),
            "a late refresh from the superseded generation is fenced"
        );
    }

    #[test]
    fn egress_targets_preserve_each_live_wire_generation() {
        let registry = BridgeSubscriberRegistry::default();
        let call = CallId::new();
        let legacy_addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let current_addr: SocketAddr = "127.0.0.1:5001".parse().unwrap();
        let old = Uuid::new_v4();
        let current = Uuid::new_v4();

        assert!(registry.subscribe_legacy(call, legacy_addr));
        assert!(registry.subscribe_generation(call, current_addr, old, Duration::from_secs(60)));
        assert!(registry.subscribe_generation(
            call,
            current_addr,
            current,
            Duration::from_secs(60)
        ));

        let mut targets = registry.targets(call);
        targets.sort_by_key(|target| target.addr);
        assert_eq!(
            targets,
            vec![
                BridgeSubscriberTarget {
                    addr: legacy_addr,
                    generation: None,
                    wire_version: None,
                },
                BridgeSubscriberTarget {
                    addr: current_addr,
                    generation: Some(current),
                    wire_version: Some(BOUND_BRIDGE_VERSION),
                },
            ]
        );
    }

    #[test]
    fn one_generation_cannot_change_wire_version_during_refresh() {
        let registry = BridgeSubscriberRegistry::default();
        let call = CallId::new();
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let generation = Uuid::new_v4();
        assert!(registry.subscribe_generation_version(
            call,
            addr,
            generation,
            Duration::from_secs(60),
            BOUND_BRIDGE_COMPAT_VERSION
        ));
        assert!(!registry.subscribe_generation_version(
            call,
            addr,
            generation,
            Duration::from_secs(60),
            BOUND_BRIDGE_VERSION
        ));
        assert_eq!(
            registry.targets(call),
            vec![BridgeSubscriberTarget {
                addr,
                generation: Some(generation),
                wire_version: Some(BOUND_BRIDGE_COMPAT_VERSION),
            }]
        );
    }

    #[test]
    fn unknown_unsubscribe_does_not_allocate_and_active_tombstones_are_bounded() {
        let reg = BridgeSubscriberRegistry::default().with_limits(1, 2, 2);
        let call = CallId::new();
        let other = CallId::new();
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();

        reg.unsubscribe_generation(call, addr, Some(Uuid::new_v4()));
        assert!(reg.inner.lock().is_empty());
        reg.revoke_generation(call, addr, Some(Uuid::new_v4()));
        reg.revoke_generation(call, addr, Some(Uuid::new_v4()));
        reg.revoke_generation(call, addr, Some(Uuid::new_v4()));
        reg.revoke_generation(other, addr, Some(Uuid::new_v4()));
        let map = reg.inner.lock();
        assert_eq!(map.len(), 1, "outer call map respects its hard cap");
        assert_eq!(
            total_generations(&map),
            2,
            "global and per-call generation caps are enforced"
        );
    }
}
