//! Error type for the signaling crate.
//!
//! These errors are produced by validation helpers and the in-memory
//! [`crate::roster::CallRoster`]. WebSocket / HTTP layers above translate them
//! into protocol-specific responses.

use thiserror::Error;

/// All fallible operations in `aero-signaling` return this error.
#[derive(Debug, Error)]
pub enum SignalingError {
    /// The SDP blob failed structural validation (missing `v=0`, too large, etc).
    #[error("invalid SDP: {0}")]
    InvalidSdp(&'static str),

    /// An ICE candidate JSON value did not match the expected shape.
    #[error("invalid ICE candidate: {0}")]
    InvalidCandidate(&'static str),

    /// Lookup of a call/participant returned nothing.
    #[error("not found: {0}")]
    NotFound(String),

    /// A participant tried to join a call they are already part of.
    #[error("participant already joined")]
    AlreadyJoined,

    /// A participant referenced in a relayed event is not part of the roster.
    #[error("participant is not a member of this call")]
    NotInCall,

    /// Generic catch-all for protocol violations not covered above.
    #[error("signaling protocol error: {0}")]
    Protocol(String),
}
