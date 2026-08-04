//! Process-local WHIP/WHEP media lifecycle and UDP port allocation.
//!
//! The signaling registry in `aero-live-whip` stores HTTP resource metadata.
//! This layer binds that resource to the production objects the server must
//! keep alive: one relay and publisher cancellation token per stream, plus a
//! cancellation token for every WHEP viewer. Publisher generations fence late
//! task completion so an old task can never remove a replacement publisher.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use aero_live_whip::{MediaRelay, WhipError, WhipRegistry, WhipResource};
use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

/// A stable view of the current local publisher used while accepting a viewer.
#[derive(Clone)]
pub struct PublisherSnapshot {
    pub resource: WhipResource,
    pub relay: Arc<MediaRelay>,
}

/// Token and generation assigned to a newly registered publisher task.
pub struct PublisherRegistration {
    pub resource_id: Ulid,
    pub cancel: CancellationToken,
}

/// Relay, token and publisher generation assigned to one WHEP viewer task.
pub struct ViewerRegistration {
    pub publisher_resource_id: Ulid,
    pub relay: Arc<MediaRelay>,
    pub cancel: CancellationToken,
}

struct ViewerRuntime {
    cancel: CancellationToken,
    local_addr: SocketAddr,
}

struct PublisherRuntime {
    resource: WhipResource,
    relay: Arc<MediaRelay>,
    cancel: CancellationToken,
    local_addr: SocketAddr,
    viewers: HashMap<Ulid, ViewerRuntime>,
}

/// Publisher + viewer runtime registry for one server process.
pub struct WhipMediaRegistry {
    resources: Arc<WhipRegistry>,
    inner: Mutex<HashMap<Ulid, PublisherRuntime>>,
}

impl WhipMediaRegistry {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            resources: WhipRegistry::new(),
            inner: Mutex::new(HashMap::new()),
        })
    }

    /// Atomically reserve a stream for one publisher and attach its relay.
    pub fn register_publisher(
        &self,
        resource: WhipResource,
        relay: Arc<MediaRelay>,
        local_addr: SocketAddr,
        shutdown: &CancellationToken,
    ) -> Result<PublisherRegistration, WhipError> {
        let mut inner = self.inner.lock();
        if inner.contains_key(&resource.stream_id) {
            return Err(WhipError::Conflict);
        }
        self.resources.insert(resource.clone())?;
        let cancel = shutdown.child_token();
        let registration = PublisherRegistration {
            resource_id: resource.resource_id,
            cancel: cancel.clone(),
        };
        inner.insert(
            resource.stream_id,
            PublisherRuntime {
                resource,
                relay,
                cancel,
                local_addr,
                viewers: HashMap::new(),
            },
        );
        Ok(registration)
    }

    /// Current local publisher and relay, if this node owns the stream.
    #[must_use]
    pub fn publisher(&self, stream_id: Ulid) -> Option<PublisherSnapshot> {
        self.inner
            .lock()
            .get(&stream_id)
            .map(|runtime| PublisherSnapshot {
                resource: runtime.resource.clone(),
                relay: runtime.relay.clone(),
            })
    }

    /// Backward-compatible resource lookup used by sticky-routing checks.
    #[must_use]
    pub fn get(&self, stream_id: Ulid) -> Option<WhipResource> {
        self.publisher(stream_id).map(|snapshot| snapshot.resource)
    }

    /// Cancel one exact publisher generation without removing its reservation.
    ///
    /// The tracked publisher task owns the subsequent route/database rollback
    /// and calls [`Self::finish_publisher`] only after those external states are
    /// clean. Keeping the reservation until then prevents a replacement
    /// publisher from being removed or marked ended by stale cleanup.
    pub fn cancel_publisher(&self, stream_id: Ulid, resource_id: Ulid) -> bool {
        let inner = self.inner.lock();
        let Some(publisher) = inner.get(&stream_id) else {
            return false;
        };
        if publisher.resource.resource_id != resource_id {
            return false;
        }
        publisher.cancel.cancel();
        true
    }

    /// Register a viewer only if the publisher generation observed during SDP
    /// negotiation is still current.
    pub fn register_viewer(
        &self,
        stream_id: Ulid,
        publisher_resource_id: Ulid,
        viewer_id: Ulid,
        local_addr: SocketAddr,
    ) -> Option<ViewerRegistration> {
        let mut inner = self.inner.lock();
        let publisher = inner.get_mut(&stream_id)?;
        if publisher.resource.resource_id != publisher_resource_id {
            return None;
        }
        if publisher.viewers.contains_key(&viewer_id) {
            return None;
        }
        let cancel = publisher.cancel.child_token();
        publisher.viewers.insert(
            viewer_id,
            ViewerRuntime {
                cancel: cancel.clone(),
                local_addr,
            },
        );
        Some(ViewerRegistration {
            publisher_resource_id,
            relay: publisher.relay.clone(),
            cancel,
        })
    }

    /// Natural publisher-task completion, fenced by resource generation.
    ///
    /// Returns `true` only when this task removed the current publisher. A late
    /// completion from a deleted/replaced session is a no-op.
    pub fn finish_publisher(&self, stream_id: Ulid, resource_id: Ulid) -> bool {
        let runtime = {
            let mut inner = self.inner.lock();
            let is_current = match inner.get(&stream_id) {
                Some(runtime) => runtime.resource.resource_id == resource_id,
                None => false,
            };
            if !is_current {
                return false;
            }
            let runtime = inner
                .remove(&stream_id)
                .expect("publisher checked immediately before remove");
            self.resources.remove(stream_id);
            runtime
        };
        cancel_publisher(runtime);
        true
    }

    /// Explicitly cancel one WHEP resource, fenced by publisher generation.
    pub fn cancel_viewer(
        &self,
        stream_id: Ulid,
        publisher_resource_id: Ulid,
        viewer_id: Ulid,
    ) -> bool {
        let viewer = self.take_viewer(stream_id, publisher_resource_id, viewer_id);
        if let Some(viewer) = viewer {
            viewer.cancel.cancel();
            true
        } else {
            false
        }
    }

    /// Natural viewer-task completion. Removes bookkeeping without touching a
    /// replacement publisher or another viewer.
    pub fn finish_viewer(
        &self,
        stream_id: Ulid,
        publisher_resource_id: Ulid,
        viewer_id: Ulid,
    ) -> bool {
        self.take_viewer(stream_id, publisher_resource_id, viewer_id)
            .is_some()
    }

    fn take_viewer(
        &self,
        stream_id: Ulid,
        publisher_resource_id: Ulid,
        viewer_id: Ulid,
    ) -> Option<ViewerRuntime> {
        let mut inner = self.inner.lock();
        let publisher = inner.get_mut(&stream_id)?;
        if publisher.resource.resource_id != publisher_resource_id {
            return None;
        }
        publisher.viewers.remove(&viewer_id)
    }

    /// Number of active publishers (the existing WHIP gauge contract).
    #[must_use]
    pub fn active_sessions(&self) -> usize {
        self.resources.active_sessions()
    }

    /// Bound/advertised publisher address and current viewer addresses.
    ///
    /// Exposed for health diagnostics and lifecycle tests; callers receive a
    /// snapshot and cannot mutate registry state.
    #[must_use]
    pub fn local_addresses(&self, stream_id: Ulid) -> Option<(SocketAddr, Vec<SocketAddr>)> {
        self.inner.lock().get(&stream_id).map(|publisher| {
            (
                publisher.local_addr,
                publisher
                    .viewers
                    .values()
                    .map(|viewer| viewer.local_addr)
                    .collect(),
            )
        })
    }
}

fn cancel_publisher(runtime: PublisherRuntime) {
    runtime.cancel.cancel();
    for viewer in runtime.viewers.into_values() {
        viewer.cancel.cancel();
    }
}

/// Failure to reserve an advertised UDP media endpoint.
#[derive(Debug, thiserror::Error)]
pub enum MediaSocketError {
    #[error("invalid numeric ICE candidate host: {0}")]
    InvalidCandidateHost(String),
    #[error("ICE candidate host must not be unspecified: {0}")]
    UnspecifiedCandidateHost(IpAddr),
    #[error("bind UDP media socket at {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

/// Bind one exclusive UDP socket and return the address to advertise in SDP.
///
/// The configured port is preferred for backward compatibility. If another
/// publisher/viewer already owns it, the OS allocates an ephemeral port. The
/// kernel therefore guarantees concurrent sessions never share a socket.
/// Non-loopback candidates bind a wildcard interface so a public/NAT
/// advertised IP does not need to exist as a local interface address.
pub async fn bind_media_socket(
    candidate_host: &str,
    preferred_port: u16,
) -> Result<(UdpSocket, SocketAddr), MediaSocketError> {
    let host = candidate_host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let candidate_ip: IpAddr = host
        .parse()
        .map_err(|_| MediaSocketError::InvalidCandidateHost(candidate_host.to_string()))?;
    if candidate_ip.is_unspecified() {
        return Err(MediaSocketError::UnspecifiedCandidateHost(candidate_ip));
    }
    let bind_ip = if candidate_ip.is_loopback() {
        candidate_ip
    } else if candidate_ip.is_ipv4() {
        IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
    };

    let preferred = SocketAddr::new(bind_ip, preferred_port);
    let socket = if preferred_port == 0 {
        UdpSocket::bind(preferred)
            .await
            .map_err(|source| MediaSocketError::Bind {
                addr: preferred,
                source,
            })?
    } else {
        match UdpSocket::bind(preferred).await {
            Ok(socket) => socket,
            Err(first_error) => {
                tracing::debug!(
                    error = %first_error,
                    %preferred,
                    "preferred media UDP port unavailable; allocating ephemeral port"
                );
                let fallback = SocketAddr::new(bind_ip, 0);
                UdpSocket::bind(fallback)
                    .await
                    .map_err(|source| MediaSocketError::Bind {
                        addr: fallback,
                        source,
                    })?
            }
        }
    };
    let candidate_addr = SocketAddr::new(
        candidate_ip,
        socket
            .local_addr()
            .map_err(|source| MediaSocketError::Bind {
                addr: preferred,
                source,
            })?
            .port(),
    );
    Ok((socket, candidate_addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(stream_id: Ulid) -> WhipResource {
        WhipResource::from_answer(stream_id, "v=0\r\n".to_string())
    }

    #[tokio::test]
    async fn preferred_port_collision_falls_back_to_unique_ephemeral_port() {
        let (first, first_candidate) = bind_media_socket("127.0.0.1", 0).await.unwrap();
        let (second, second_candidate) = bind_media_socket("127.0.0.1", first_candidate.port())
            .await
            .unwrap();

        assert_ne!(first_candidate.port(), second_candidate.port());
        assert_eq!(first.local_addr().unwrap().port(), first_candidate.port());
        assert_eq!(second.local_addr().unwrap().port(), second_candidate.port());
    }

    #[tokio::test]
    async fn publisher_cancellation_and_finish_clean_up_all_viewers() {
        let registry = WhipMediaRegistry::new();
        let shutdown = CancellationToken::new();
        let stream_id = Ulid::new();
        let resource = resource(stream_id);
        let resource_id = resource.resource_id;
        let publisher = registry
            .register_publisher(
                resource,
                Arc::new(MediaRelay::new()),
                "127.0.0.1:41000".parse().unwrap(),
                &shutdown,
            )
            .unwrap();
        let viewer_id = Ulid::new();
        let viewer = registry
            .register_viewer(
                stream_id,
                resource_id,
                viewer_id,
                "127.0.0.1:41001".parse().unwrap(),
            )
            .unwrap();

        assert_eq!(registry.active_sessions(), 1);
        assert_eq!(registry.local_addresses(stream_id).unwrap().1.len(), 1);
        assert!(!publisher.cancel.is_cancelled());
        assert!(!viewer.cancel.is_cancelled());

        assert!(registry.cancel_publisher(stream_id, resource_id));
        assert!(publisher.cancel.is_cancelled());
        assert!(viewer.cancel.is_cancelled());
        assert!(registry.finish_publisher(stream_id, resource_id));
        assert_eq!(registry.active_sessions(), 0);
        assert!(registry.publisher(stream_id).is_none());
    }

    #[test]
    fn stale_publisher_completion_cannot_remove_replacement() {
        let registry = WhipMediaRegistry::new();
        let shutdown = CancellationToken::new();
        let stream_id = Ulid::new();
        let first = resource(stream_id);
        let first_id = first.resource_id;
        registry
            .register_publisher(
                first,
                Arc::new(MediaRelay::new()),
                "127.0.0.1:42000".parse().unwrap(),
                &shutdown,
            )
            .unwrap();
        assert!(registry.finish_publisher(stream_id, first_id));

        let second = resource(stream_id);
        let second_id = second.resource_id;
        registry
            .register_publisher(
                second,
                Arc::new(MediaRelay::new()),
                "127.0.0.1:42001".parse().unwrap(),
                &shutdown,
            )
            .unwrap();

        assert!(!registry.finish_publisher(stream_id, first_id));
        assert_eq!(
            registry.publisher(stream_id).unwrap().resource.resource_id,
            second_id
        );
    }

    #[test]
    fn stale_publisher_cancellation_cannot_stop_replacement() {
        let registry = WhipMediaRegistry::new();
        let shutdown = CancellationToken::new();
        let stream_id = Ulid::new();
        let first = resource(stream_id);
        let first_id = first.resource_id;
        registry
            .register_publisher(
                first,
                Arc::new(MediaRelay::new()),
                "127.0.0.1:42300".parse().unwrap(),
                &shutdown,
            )
            .unwrap();
        assert!(registry.finish_publisher(stream_id, first_id));

        let second = resource(stream_id);
        let second_id = second.resource_id;
        let second_registration = registry
            .register_publisher(
                second,
                Arc::new(MediaRelay::new()),
                "127.0.0.1:42301".parse().unwrap(),
                &shutdown,
            )
            .unwrap();

        assert!(!registry.cancel_publisher(stream_id, first_id));
        assert!(!second_registration.cancel.is_cancelled());
        assert!(registry.cancel_publisher(stream_id, second_id));
        assert!(second_registration.cancel.is_cancelled());
    }

    #[test]
    fn viewer_completion_is_generation_fenced_and_removes_bookkeeping() {
        let registry = WhipMediaRegistry::new();
        let shutdown = CancellationToken::new();
        let stream_id = Ulid::new();
        let resource = resource(stream_id);
        let resource_id = resource.resource_id;
        registry
            .register_publisher(
                resource,
                Arc::new(MediaRelay::new()),
                "127.0.0.1:42500".parse().unwrap(),
                &shutdown,
            )
            .unwrap();
        let viewer_id = Ulid::new();
        registry
            .register_viewer(
                stream_id,
                resource_id,
                viewer_id,
                "127.0.0.1:42501".parse().unwrap(),
            )
            .unwrap();

        assert!(!registry.finish_viewer(stream_id, Ulid::new(), viewer_id));
        assert_eq!(registry.local_addresses(stream_id).unwrap().1.len(), 1);
        assert!(registry.finish_viewer(stream_id, resource_id, viewer_id));
        assert!(registry.local_addresses(stream_id).unwrap().1.is_empty());
    }

    #[test]
    fn process_shutdown_cascades_to_publisher_and_viewers() {
        let registry = WhipMediaRegistry::new();
        let shutdown = CancellationToken::new();
        let stream_id = Ulid::new();
        let resource = resource(stream_id);
        let resource_id = resource.resource_id;
        let publisher = registry
            .register_publisher(
                resource,
                Arc::new(MediaRelay::new()),
                "127.0.0.1:42600".parse().unwrap(),
                &shutdown,
            )
            .unwrap();
        let viewer = registry
            .register_viewer(
                stream_id,
                resource_id,
                Ulid::new(),
                "127.0.0.1:42601".parse().unwrap(),
            )
            .unwrap();

        shutdown.cancel();
        assert!(publisher.cancel.is_cancelled());
        assert!(viewer.cancel.is_cancelled());
    }

    #[tokio::test]
    async fn cancellation_releases_bound_publisher_socket() {
        let registry = WhipMediaRegistry::new();
        let shutdown = CancellationToken::new();
        let stream_id = Ulid::new();
        let resource = resource(stream_id);
        let resource_id = resource.resource_id;
        let (socket, candidate) = bind_media_socket("127.0.0.1", 0).await.unwrap();
        let publisher = registry
            .register_publisher(resource, Arc::new(MediaRelay::new()), candidate, &shutdown)
            .unwrap();
        let task_registry = registry.clone();
        let task_cancel = publisher.cancel.clone();
        let task = tokio::spawn(async move {
            task_cancel.cancelled().await;
            drop(socket);
            task_registry.finish_publisher(stream_id, resource_id);
        });

        assert!(registry.cancel_publisher(stream_id, resource_id));
        task.await.unwrap();
        let rebound = UdpSocket::bind(candidate).await.unwrap();
        assert_eq!(rebound.local_addr().unwrap(), candidate);
    }
}
