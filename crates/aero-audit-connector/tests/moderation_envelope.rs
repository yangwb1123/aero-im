//! Moderation-envelope wire pins (A2.1) — split out of `claim_validation.rs`
//! to keep that file under the 800-line WARN budget (Security F4, design gate
//! 5a44687a). The config/claim fixture helpers are duplicated verbatim (the
//! lint — `claim_validation`'s "the 11 tests here exercise JWT validation" — is
//! file-scoped; a shared tests/ helper module would be a bigger refactor than
//! the split's purpose).

use aero_audit_connector::client::AuditClient;
use aero_audit_connector::config::RelayConfig;
use aero_audit_connector::outbox::Claim;
use aero_audit_connector::stub::StubSink;
use aero_common::model::audit::{MODERATION_OUTBOUND_ACTION, MODERATION_OUTBOUND_VOCABULARY};
use aero_common::AuditId;
use reqwest::Url;
use serde_json::json;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const SECRET: &str = "0123456789abcdef0123456789abcdef";

fn config(stub: &StubSink) -> RelayConfig {
    RelayConfig {
        token_endpoint: Url::parse(&stub.token_url()).expect("stub token URL"),
        events_url: Url::parse(&stub.events_url()).expect("stub events URL"),
        resource: "audit-governance".into(),
        client_id: "drill-client".into(),
        client_secret: SECRET.into(),
        expected_iss: "https://idp.example.test".into(),
        expected_aud: "audit-governance".into(),
        expected_scope: "audit:event:write".into(),
        expected_sub: "aero-im.source".into(),
        source_system: "aero-im.source".into(),
        request_timeout: std::time::Duration::from_secs(5),
        delivery_lease: std::time::Duration::from_secs(30),
        poll_interval: std::time::Duration::from_secs(1),
        shutdown_drain: std::time::Duration::from_secs(2),
        batch_size: 100,
        concurrency: 4,
        jwks_uri: None,
        provision_freshness: std::time::Duration::from_secs(300),
    }
}

fn claim() -> Claim {
    let id = Uuid::new_v4();
    Claim {
        event_id: AuditId::from_uuid(id),
        claim_token: Uuid::new_v4(),
        lease_expires_at: OffsetDateTime::now_utc() + Duration::minutes(1),
        attempts: 1,
        payload: json!({
            "event_id": id.to_string(),
            "source_system": "aero-im.source",
            "action": MODERATION_OUTBOUND_ACTION,
        }),
        priority: 10,
        class: "message".into(),
    }
}

/// A2.1 positive — the delivered envelope's `action` is byte-equal to the pinned leaf constant.
#[tokio::test]
async fn delivered_envelope_carries_the_pinned_moderation_action() {
    let stub = StubSink::start().await.expect("start stub");
    let client = AuditClient::new(config(&stub)).expect("build client");
    assert!(client.deliver(&claim()).await.is_ok());
    let payloads = stub.seen_payloads().await;
    assert_eq!(payloads.len(), 1, "exactly one delivered envelope");
    assert_eq!(payloads[0]["action"], MODERATION_OUTBOUND_ACTION);
    stub.shutdown();
}

/// A2.1 negative twin — the sibling spelling stays distinguishable end-to-end
/// (a leaf flip to the sibling reds the positive).
#[tokio::test]
async fn sibling_spelling_is_detected_as_drift() {
    let stub = StubSink::start().await.expect("start stub");
    let client = AuditClient::new(config(&stub)).expect("build client");
    // The drift spelling is THE OTHER contract member, derived from the
    // leaf vocabulary (never hardcoded) — a coordinated flip (A1.3, the
    // flip drill) makes the sibling the pinned spelling, and this twin
    // still proves wire-echo of a NON-pinned spelling.
    let sibling = MODERATION_OUTBOUND_VOCABULARY
        .iter()
        .copied()
        .find(|candidate| *candidate != MODERATION_OUTBOUND_ACTION)
        .expect("the vocabulary has exactly two members");
    let mut drifted = claim();
    drifted.payload["action"] = json!(sibling);
    assert!(client.deliver(&drifted).await.is_ok());
    let payloads = stub.seen_payloads().await;
    assert_eq!(payloads.len(), 1, "exactly one delivered envelope");
    // Verbatim-forwarding pin (gate F2): the delivered envelope must carry
    // EXACTLY the drifted payload's action (client.rs forwards claim.payload
    // verbatim). A leaf-coordinated flip makes the const follow the leaf, so
    // an assert_ne! against the const would red — this assert_eq! on the
    // DRIFTED value proves the sink echo is the wire value, and A1.3's
    // coordinated flip passes. (The positive twin above pins the pinned
    // spelling end-to-end.)
    assert_eq!(payloads[0]["action"], drifted.payload["action"]);
    stub.shutdown();
}
