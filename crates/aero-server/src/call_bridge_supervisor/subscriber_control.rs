//! Atomic control-plane admission against the owning egress lifecycle.

use std::net::SocketAddr;
use std::time::Duration;

use aero_common::CallId;
use uuid::Uuid;

use super::CallBridgeSupervisor;

impl CallBridgeSupervisor {
    pub(crate) fn subscribe_egress_generation(
        &self,
        call: CallId,
        addr: SocketAddr,
        generation: Uuid,
        lease: Duration,
        wire_version: u8,
    ) -> Option<bool> {
        let egress = self.egress.lock();
        if !egress.contains_key(&call) {
            return None;
        }
        Some(self.subscribers.subscribe_generation_version(
            call,
            addr,
            generation,
            lease,
            wire_version,
        ))
    }

    pub(crate) fn subscribe_egress_legacy(&self, call: CallId, addr: SocketAddr) -> Option<bool> {
        let egress = self.egress.lock();
        if !egress.contains_key(&call) {
            return None;
        }
        Some(self.subscribers.subscribe_legacy(call, addr))
    }

    pub(crate) fn unsubscribe_egress_generation(
        &self,
        call: CallId,
        addr: SocketAddr,
        generation: Option<Uuid>,
    ) -> bool {
        let egress = self.egress.lock();
        if !egress.contains_key(&call) {
            return false;
        }
        self.subscribers.revoke_generation(call, addr, generation);
        true
    }
}
