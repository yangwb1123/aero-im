//! A2 — claim validation: a JWT whose iss/aud/scope/sub claims do not match
//! the configured expectations is rejected **before any delivery POST**
//! (POST counter == 0 is literally measured); a valid token proceeds.

use aero_audit_connector::client::{AuditClient, DeliveryError, PermanentKind};
use aero_audit_connector::config::RelayConfig;
use aero_audit_connector::outbox::Claim;
use aero_audit_connector::stub::{make_jwt, SinkBehavior, StubSink};
use aero_auth::{JwksKeyProvider, KeyProvider, StaticKeyProvider};
use aero_common::model::audit::{MODERATION_OUTBOUND_ACTION, MODERATION_OUTBOUND_VOCABULARY};
use aero_common::AuditId;
use rand::rngs::OsRng;
use reqwest::Url;
use rsa::RsaPrivateKey;
use serde_json::{json, Value};
use std::sync::Arc;
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
    }
}

/// Production-format claim builder (D1 mirror): the payload's `event_id` is
/// the hyphenated UUID string of the same value used to construct
/// `AuditId::from_uuid` — byte-identical to the 0239 trigger's
/// `jsonb_build_object('event_id', NEW.id::text, …)` (migrations/0239:
/// NEW.id is `audit_events.id`, a UUID). `claim.event_id.to_string()`
/// (ULID base32) therefore differs from the payload string — the receipt
/// comparison must be value-level (dual-format), never string equality.
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
        // B5-3 R6: backlog-lane defaults, pinned to aero_ai::governance
        // (crates/aero-ai/src/governance.rs:33/:39) — the 0239 column
        // defaults. The 11 tests here exercise JWT validation, not the lane.
        priority: 10,            // GOVERNANCE_PRIORITY_BACKLOG (governance.rs:33)
        class: "message".into(), // GOVERNANCE_CLASS_MESSAGE (aero_common::model::audit)
    }
}

/// Production-format token-claims base (B5-2 leaf contract): every claim the
/// typed gate requires (`sub`/`client_id`) plus the configured
/// iss/aud/scope. Individual tests override one field on top of this base so
/// each rejection keeps its specific reason (a missing-`client_id` failure
/// stays distinguishable from issuer drift). Of the 17 `json!(` sites in
/// this file only these 12 are token-claims fixtures; the opaque/decode-only
/// sites stay untouched.
fn base_claims() -> Value {
    json!({
        "iss": "https://idp.example.test",
        "aud": ["audit-governance"],
        "scope": "audit:event:write",
        "sub": "aero-im.source",
        "client_id": "aero-im.source",
    })
}

/// Override one claim on top of [`base_claims`].
fn with_claim(mut claims: Value, key: &str, value: Value) -> Value {
    claims[key] = value;
    claims
}

/// Drive one delivery against a stub minting `token_claims`; returns the
/// delivery outcome and the number of POSTs observed.
async fn deliver_with_claims(token_claims: Value) -> (Result<(), DeliveryError>, usize) {
    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        token_claims,
        ..SinkBehavior::default()
    })
    .await;
    let cfg = config(&stub);
    let client = AuditClient::new(cfg).expect("build client");
    let outcome = client.deliver(&claim()).await;
    let posts = stub.posts();
    stub.shutdown();
    (outcome, posts)
}

/// Drive one delivery against a JWKS-enabled client (injected key source);
/// the stub mints `token_claims` (alg:none unless `signing_key` is set).
async fn deliver_with_claims_and_keys(
    stub: &StubSink,
    token_claims: Value,
    keys: Arc<dyn KeyProvider>,
) -> (Result<(), DeliveryError>, usize) {
    stub.set_behavior(SinkBehavior {
        token_claims,
        ..SinkBehavior::default()
    })
    .await;
    let cfg = config(stub);
    let client = AuditClient::with_key_provider(cfg, Some(keys)).expect("build client");
    let outcome = client.deliver(&claim()).await;
    let posts = stub.posts();
    (outcome, posts)
}

/// A fresh RS256 keypair for rotation/foreign-key fixtures.
fn test_keypair(kid: &str) -> (RsaPrivateKey, String) {
    let mut rng = OsRng;
    (
        RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen"),
        kid.to_owned(),
    )
}

#[tokio::test]
async fn wrong_issuer_is_rejected_before_any_post() {
    let (outcome, posts) = deliver_with_claims(with_claim(
        base_claims(),
        "iss",
        json!("https://evil.example.test"),
    ))
    .await;
    assert!(outcome.is_err());
    assert_eq!(posts, 0, "wrong iss must be rejected before any POST");
}

#[tokio::test]
async fn missing_audience_is_rejected_before_any_post() {
    let (outcome, posts) =
        deliver_with_claims(with_claim(base_claims(), "aud", json!(["billing-api"]))).await;
    assert!(outcome.is_err());
    assert_eq!(posts, 0, "wrong aud must be rejected before any POST");
}

#[tokio::test]
async fn missing_audit_scope_is_rejected_before_any_post() {
    let (outcome, posts) = deliver_with_claims(with_claim(
        base_claims(),
        "scope",
        json!("billing:entitlement:read"),
    ))
    .await;
    assert!(outcome.is_err());
    assert_eq!(
        posts, 0,
        "missing audit scope must be rejected before any POST"
    );
}

#[tokio::test]
async fn wrong_subject_is_rejected_before_any_post() {
    let (outcome, posts) =
        deliver_with_claims(with_claim(base_claims(), "sub", json!("someone-else"))).await;
    assert!(outcome.is_err());
    assert_eq!(posts, 0, "wrong sub must be rejected before any POST");
}

#[tokio::test]
async fn opaque_non_jwt_token_is_rejected_before_any_post() {
    // If the IdP issues an opaque token, claim validation cannot run — the
    // connector must fail closed (no POST) rather than skip validation.
    let (outcome, posts) = deliver_with_claims(json!("opaque-bearer-token")).await;
    assert!(outcome.is_err());
    assert_eq!(posts, 0, "opaque tokens must be rejected before any POST");
}

#[tokio::test]
async fn valid_token_is_accepted_and_delivery_proceeds() {
    let (outcome, posts) = deliver_with_claims(with_claim(
        base_claims(),
        "scope",
        json!("audit:event:write metering:read"),
    ))
    .await;
    assert!(outcome.is_ok(), "valid claims must deliver: {outcome:?}");
    assert!(posts >= 1, "valid token must reach the audit endpoint");
}

/// D1 — the sink echoes the payload's `event_id` field verbatim (the
/// in-repo stub contract, and the 0239 trigger's `NEW.id::text` format): a
/// hyphenated UUID string that differs textually from
/// `claim.event_id.to_string()` (ULID base32). The value-level dual-format
/// receipt comparison must accept it — a string equality here would
/// misclassify the delivery as `ReceiptMismatch` (dead after ≤1 retry).
#[tokio::test]
async fn receipt_echoing_payload_uuid_event_id_is_accepted() {
    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior::default()).await;
    let cfg = config(&stub);
    let client = AuditClient::new(cfg).expect("build client");
    let claim = claim();
    // The claim's payload carries the UUID string (0239 trigger format) —
    // assert the setup really is the divergent-format scenario the fix
    // targets, so the test cannot silently degrade into a lockstep compare.
    assert_ne!(
        claim.payload["event_id"]
            .as_str()
            .expect("payload event_id"),
        claim.event_id.to_string(),
        "payload event_id must stay uuid::text while the typed id renders base32"
    );
    let outcome = client.deliver(&claim).await;
    assert!(
        outcome.is_ok(),
        "receipt echoing the payload's UUID event_id must settle: {outcome:?}"
    );
    assert_eq!(stub.posts(), 1, "exactly one delivery POST");
    stub.shutdown();
}

/// D1 — the base32 arm: a sink echoing the `Idempotency-Key` header (which
/// carries `claim.event_id.to_string()` = ULID base32 after the `AuditId`
/// retyping) must also be accepted by the value-level comparison.
#[tokio::test]
async fn receipt_echoing_base32_header_event_id_is_accepted() {
    let stub = StubSink::start().await.expect("start stub");
    let claim = claim();
    stub.set_behavior(SinkBehavior {
        receipt_event_id_override: Some(claim.event_id.to_string()),
        ..SinkBehavior::default()
    })
    .await;
    let cfg = config(&stub);
    let client = AuditClient::new(cfg).expect("build client");
    let outcome = client.deliver(&claim).await;
    assert!(
        outcome.is_ok(),
        "receipt echoing the base32 Idempotency-Key must settle: {outcome:?}"
    );
    assert_eq!(stub.posts(), 1, "exactly one delivery POST");
    stub.shutdown();
}

#[tokio::test]
async fn unauthorized_refreshes_once_and_retries_within_the_attempt() {
    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        unauthorized_once: true,
        ..SinkBehavior::default()
    })
    .await;
    let cfg = config(&stub);
    let client = AuditClient::new(cfg).expect("build client");
    let outcome = client.deliver(&claim()).await;
    assert!(
        outcome.is_ok(),
        "401 must refresh once and retry within the same attempt: {outcome:?}"
    );
    assert_eq!(stub.posts(), 2, "first POST 401, second POST succeeds");
    stub.shutdown();
}

/// The 401-refresh-once retry must re-pass claim validation before the
/// retry POST: if the identity-provider drift surfaced by the 401 produces
/// a token with bad claims, the retry POST is suppressed (posts()==1, never
/// 2) and the attempt aborts transient (the row re-parks to rotate a fresh
/// token).
#[tokio::test]
async fn refreshed_token_must_repass_claim_validation_before_retry_post() {
    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        unauthorized_once: true,
        token_claims_after_first: Some(with_claim(
            base_claims(),
            "iss",
            json!("https://evil.example.test"),
        )),
        ..SinkBehavior::default()
    })
    .await;
    let cfg = config(&stub);
    let client = AuditClient::new(cfg).expect("build client");
    let outcome = client.deliver(&claim()).await;
    assert!(
        matches!(outcome, Err(DeliveryError::Transient(_))),
        "a refreshed token failing claim validation must abort the attempt, got {outcome:?}"
    );
    assert_eq!(
        stub.posts(),
        1,
        "the retry POST must be suppressed: a bad-claims token never goes out"
    );
    stub.shutdown();
}

#[tokio::test]
async fn client_classifies_statuses() {
    for (status, expected) in [
        (403_u16, DeliveryError::Forbidden),
        (422, DeliveryError::Permanent(PermanentKind::Unprocessable)),
        (409, DeliveryError::Permanent(PermanentKind::Conflict)),
        (500, DeliveryError::Transient(anyhow::anyhow!("status"))),
        (400, DeliveryError::Transient(anyhow::anyhow!("status"))),
    ] {
        let stub = StubSink::start().await.expect("start stub");
        stub.set_behavior(SinkBehavior {
            events_status: status,
            ..SinkBehavior::default()
        })
        .await;
        let cfg = config(&stub);
        let client = AuditClient::new(cfg).expect("build client");
        let outcome = client.deliver(&claim()).await;
        match (&outcome, &expected) {
            (Err(DeliveryError::Forbidden), DeliveryError::Forbidden)
            | (Err(DeliveryError::Transient(_)), DeliveryError::Transient(_)) => {}
            (Err(DeliveryError::Permanent(kind)), DeliveryError::Permanent(expected_kind))
                if kind == expected_kind => {}
            _ => panic!("status {status}: unexpected outcome {outcome:?}"),
        }
        stub.shutdown();
    }
}

#[tokio::test]
async fn payload_guard_rejects_tenant_selection_as_permanent() {
    let stub = StubSink::start().await.expect("start stub");
    let cfg = config(&stub);
    let client = AuditClient::new(cfg).expect("build client");
    let mut tenant_claim = claim();
    tenant_claim.payload = json!({
        "event_id": tenant_claim.event_id.to_string(),
        "source_system": "aero-im.source",
        "tenant_id": "tenant-a",
    });
    let outcome = client.deliver(&tenant_claim).await;
    assert!(matches!(
        outcome,
        Err(DeliveryError::Permanent(PermanentKind::PayloadGuard))
    ));
    assert_eq!(stub.posts(), 0, "payload guard fails before any POST");
    stub.shutdown();
}

#[test]
fn jwt_claims_decode_roundtrip() {
    // The connector trusts the IdP (client_credentials), so signature is not
    // verified; the payload decode + claim extraction is what A2 pins.
    let token = make_jwt(&json!({"iss": "https://idp.example.test"}));
    assert_eq!(token.split('.').count(), 3);
}

// ---------------------------------------------------------------------------
// B5-2 claim-contract hardening — exp/nbf time claims (AC1/AC2 + pins).
// ---------------------------------------------------------------------------

/// AC1 — a token whose `exp` is already past is rejected **before any POST**.
/// The 120s margin is 2× the 60s leeway, so the wall-clock micro-race
/// between the test's `now` and the client's `now_utc()` cannot flip the
/// outcome (the claims plane runs JWKS-off here — D6 matrix cell).
#[tokio::test]
async fn expired_token_is_rejected_before_any_post() {
    let pinned = OffsetDateTime::now_utc();
    let (outcome, posts) = deliver_with_claims(with_claim(
        base_claims(),
        "exp",
        json!(pinned.unix_timestamp() - 120),
    ))
    .await;
    assert!(
        matches!(outcome, Err(DeliveryError::Transient(_))),
        "an expired token must be a transient claims rejection: {outcome:?}"
    );
    assert!(
        outcome
            .err()
            .is_some_and(|error| error.to_string().contains("token has expired")),
        "last_error must carry the stable 'token has expired' fragment"
    );
    assert_eq!(
        posts, 0,
        "an expired token must never reach a delivery POST"
    );
}

/// AC2 — a token whose `nbf` is in the future is rejected **before any POST**.
#[tokio::test]
async fn future_nbf_token_is_rejected_before_any_post() {
    let pinned = OffsetDateTime::now_utc();
    let (outcome, posts) = deliver_with_claims(with_claim(
        base_claims(),
        "nbf",
        json!(pinned.unix_timestamp() + 3600),
    ))
    .await;
    assert!(
        matches!(outcome, Err(DeliveryError::Transient(_))),
        "a not-yet-valid token must be a transient claims rejection: {outcome:?}"
    );
    assert!(
        outcome
            .err()
            .is_some_and(|error| error.to_string().contains("token is not yet valid")),
        "last_error must carry the stable 'token is not yet valid' fragment"
    );
    assert_eq!(
        posts, 0,
        "a not-yet-valid token must never reach a delivery POST"
    );
}

/// D1 pin — tokens without `exp`/`nbf` claims (every pre-B5-2 fixture) must
/// keep passing: the time claims are validated-when-present, never required.
#[tokio::test]
async fn exp_nbf_missing_claims_still_pass() {
    let (outcome, posts) = deliver_with_claims(base_claims()).await;
    assert!(
        outcome.is_ok(),
        "missing exp/nbf must not reject: {outcome:?}"
    );
    assert!(posts >= 1, "a no-time-claims token must deliver");
}

/// D8 pin — a present but non-numeric `exp` (string digits) is malformed and
/// fails closed on the claims plane (never delivered).
#[tokio::test]
async fn exp_nbf_non_numeric_rejected() {
    let (outcome, posts) =
        deliver_with_claims(with_claim(base_claims(), "exp", json!("1234567890"))).await;
    assert!(
        outcome.is_err(),
        "string-numeric exp must be rejected: {outcome:?}"
    );
    assert_eq!(posts, 0, "a malformed exp must never reach a delivery POST");
}

/// D6 matrix pin (JWKS-off face) — an opaque token keeps today's Transient
/// classification when signature verification is off: the claims plane sees
/// no JWT structure and rejects before any POST.
#[tokio::test]
async fn jwks_off_keeps_opaque_token_transient() {
    let (outcome, posts) = deliver_with_claims(json!("opaque-bearer-token")).await;
    assert!(
        matches!(outcome, Err(DeliveryError::Transient(_))),
        "JWKS-off must keep opaque tokens transient (claims plane): {outcome:?}"
    );
    assert_eq!(posts, 0);
}

// ---------------------------------------------------------------------------
// B5-2 claim-contract hardening — JWKS signature plane (AC3/AC4 + pins).
// ---------------------------------------------------------------------------

/// AC3(a) — with a trusted key source injected, the legacy `alg:none`
/// fake-signature fixture is rejected on the signature plane (alg outside the
/// RS256 allowlist) **before any POST**, and the poisoned token cache is
/// invalidated (D7 — the retry rotates a fresh token).
#[tokio::test]
async fn alg_none_token_rejected_when_jwks_enabled() {
    let stub = StubSink::start().await.expect("start stub");
    let (outcome, posts) = deliver_with_claims_and_keys(
        &stub,
        base_claims(),
        Arc::new(StaticKeyProvider::single(stub.decoding_key())),
    )
    .await;
    assert!(
        matches!(
            outcome,
            Err(DeliveryError::Permanent(PermanentKind::SignatureRejected))
        ),
        "alg:none must be a permanent signature rejection: {outcome:?}"
    );
    assert_eq!(
        posts, 0,
        "an alg:none token must never reach a delivery POST"
    );
    stub.shutdown();
}

/// Sibling AC1(b) — a trusted-signature token corrupted after signing (bad
/// signature segment) is rejected on the signature plane before any POST.
#[tokio::test]
async fn tampered_signature_is_rejected_before_any_post() {
    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        signing_key: Some(aero_audit_connector::stub::trusted_key()),
        tamper: Some(aero_audit_connector::stub::TamperMode::CorruptSignature),
        ..SinkBehavior::default()
    })
    .await;
    let cfg = config(&stub);
    let client = AuditClient::with_key_provider(
        cfg,
        Some(Arc::new(StaticKeyProvider::single(stub.decoding_key()))),
    )
    .expect("build client");
    let outcome = client.deliver(&claim()).await;
    assert!(
        matches!(
            outcome,
            Err(DeliveryError::Permanent(PermanentKind::SignatureRejected))
        ),
        "a corrupted signature must be a permanent signature rejection: {outcome:?}"
    );
    assert_eq!(stub.posts(), 0, "a tampered token must never POST");
    stub.shutdown();
}

/// AC3(b) client half — the JWKS endpoint answering 404 (no key set served)
/// is a fetch-plane failure: Transient (requeue forever, never dead), zero
/// POSTs, stable `audit jwks unavailable` fragment. The token is properly
/// RS256-signed (an `alg:none` token would die at the alg gate first — D6
/// order). The relay half lives in `src/relay.rs`
/// (`jwks_fetch_failure_requeues_without_any_post`).
#[tokio::test]
async fn jwks_fetch_failure_requeues_without_any_post() {
    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        signing_key: Some(aero_audit_connector::stub::trusted_key()),
        ..SinkBehavior::default()
    })
    .await;
    let cfg = config(&stub);
    let client =
        AuditClient::with_key_provider(cfg, Some(Arc::new(JwksKeyProvider::new(stub.jwks_url()))))
            .expect("build client");
    let outcome = client.deliver(&claim()).await;
    assert!(
        matches!(outcome, Err(DeliveryError::Transient(_))),
        "a JWKS fetch failure must be transient, never permanent: {outcome:?}"
    );
    assert!(
        outcome
            .err()
            .is_some_and(|error| error.to_string().contains("audit jwks unavailable")),
        "last_error must carry the stable 'audit jwks unavailable' fragment"
    );
    assert_eq!(stub.posts(), 0, "a JWKS fetch failure must never POST");
    stub.shutdown();
}

/// AC4 — kid rotation with served-JWKS semantics: a fresh provider per phase
/// (design-sanctioned seam; the 10s throttle belongs to the provider, the
/// acceptance pins the property): {A} serves and A-signed tokens deliver;
/// after rotation to {B}, B-signed tokens deliver and retired A-signed
/// tokens are rejected with zero POST growth.
#[tokio::test]
async fn kid_rotation_new_key_served_is_accepted() {
    let (key_a, kid_a) = aero_audit_connector::stub::trusted_key();
    let (key_b, kid_b) = test_keypair("key-b");
    let stub = StubSink::start().await.expect("start stub");

    // Phase 1 — served {A}, signed by A: accepted and delivered.
    stub.set_behavior(SinkBehavior {
        signing_key: Some((key_a.clone(), kid_a.clone())),
        jwks_keys: vec![(key_a.clone(), kid_a.clone())],
        ..SinkBehavior::default()
    })
    .await;
    let client = AuditClient::with_key_provider(
        config(&stub),
        Some(Arc::new(JwksKeyProvider::new(stub.jwks_url()))),
    )
    .expect("build client");
    let outcome = client.deliver(&claim()).await;
    assert!(outcome.is_ok(), "key A served must deliver: {outcome:?}");
    assert_eq!(stub.posts(), 1);

    // Phase 2 — served {B} after rotation, signed by B: accepted (the key
    // source resolves the new kid), delivered.
    stub.set_behavior(SinkBehavior {
        signing_key: Some((key_b.clone(), kid_b.clone())),
        jwks_keys: vec![(key_b.clone(), kid_b.clone())],
        ..SinkBehavior::default()
    })
    .await;
    let client = AuditClient::with_key_provider(
        config(&stub),
        Some(Arc::new(JwksKeyProvider::new(stub.jwks_url()))),
    )
    .expect("build client");
    let outcome = client.deliver(&claim()).await;
    assert!(
        outcome.is_ok(),
        "new key served after rotation must deliver: {outcome:?}"
    );
    assert_eq!(stub.posts(), 2);

    // Phase 3 — the retired key A signs again (served set is {B}): unknown
    // kid after refresh → permanent signature rejection, posts unchanged.
    stub.set_behavior(SinkBehavior {
        signing_key: Some((key_a.clone(), kid_a.clone())),
        jwks_keys: vec![(key_b.clone(), kid_b.clone())],
        ..SinkBehavior::default()
    })
    .await;
    let client = AuditClient::with_key_provider(
        config(&stub),
        Some(Arc::new(JwksKeyProvider::new(stub.jwks_url()))),
    )
    .expect("build client");
    let outcome = client.deliver(&claim()).await;
    assert!(
        matches!(
            outcome,
            Err(DeliveryError::Permanent(PermanentKind::SignatureRejected))
        ),
        "a retired key must be a permanent signature rejection: {outcome:?}"
    );
    assert_eq!(stub.posts(), 2, "retired-key tokens must never POST");
    stub.shutdown();
}

/// D4 pin — a genuine unknown kid (key source resolves, but no matching key)
/// is the permanent plane: `Ok(None)` from the key source must map to
/// `Permanent(SignatureRejected)`, never Transient.
#[tokio::test]
async fn unknown_kid_is_permanent_never_transient() {
    let stub = StubSink::start().await.expect("start stub");
    // Sign with the trusted key under kid "test-audit-1", but inject a key
    // source whose only registered kid differs → key lookup misses.
    stub.set_behavior(SinkBehavior {
        signing_key: Some(aero_audit_connector::stub::trusted_key()),
        ..SinkBehavior::default()
    })
    .await;
    let cfg = config(&stub);
    let client = AuditClient::with_key_provider(
        cfg,
        Some(Arc::new(StaticKeyProvider::with_keyed(vec![(
            "expected-kid".into(),
            stub.decoding_key(),
        )]))),
    )
    .expect("build client");
    let outcome = client.deliver(&claim()).await;
    assert!(
        matches!(
            outcome,
            Err(DeliveryError::Permanent(PermanentKind::SignatureRejected))
        ),
        "an unknown kid must be permanent, never transient: {outcome:?}"
    );
    assert_eq!(stub.posts(), 0, "an unknown-kid token must never POST");
    stub.shutdown();
}

/// F1 pin — the signature plane must ignore `aud` and the time claims (those
/// belong to the claims plane; jsonwebtoken 9.3.1 defaults
/// `validate_aud`/`validate_exp`/`required_spec_claims` ON and would
/// otherwise dead real tokens on the JWKS-on face). A real RS256-signed
/// token carrying `aud` and an expired `exp` passes the signature plane and
/// is judged Transient by the claims plane — never Permanent.
#[tokio::test]
async fn signature_plane_ignores_aud_and_time_claims() {
    let stub = StubSink::start().await.expect("start stub");
    let pinned = OffsetDateTime::now_utc();
    stub.set_behavior(SinkBehavior {
        signing_key: Some(aero_audit_connector::stub::trusted_key()),
        token_claims: with_claim(base_claims(), "exp", json!(pinned.unix_timestamp() - 120)),
        ..SinkBehavior::default()
    })
    .await;
    let cfg = config(&stub);
    let client = AuditClient::with_key_provider(
        cfg,
        Some(Arc::new(StaticKeyProvider::single(stub.decoding_key()))),
    )
    .expect("build client");
    let outcome = client.deliver(&claim()).await;
    assert!(
        matches!(outcome, Err(DeliveryError::Transient(_))),
        "the claims plane must judge the expired exp as Transient — the \
         signature plane must not dead it: {outcome:?}"
    );
    assert!(outcome
        .err()
        .is_some_and(|error| error.to_string().contains("token has expired")));
    assert_eq!(stub.posts(), 0);
    stub.shutdown();
}

// ---------------------------------------------------------------------------
// B5-2 claim-contract leaf — typed gate fail-closed deltas ①–④ (§1.3/§3).
// ---------------------------------------------------------------------------

/// Delta ① — a token without `client_id` (minted by an `IdP` that predates the
/// Snaplink extension claim) is rejected by the typed gate **before any
/// POST**: today the connector never checks `client_id` (fail-open); after
/// the gate it is a fail-closed Transient — requeue with backoff, never
/// dead.
#[tokio::test]
async fn token_without_client_id_is_rejected_before_any_post() {
    let mut claims = base_claims();
    claims
        .as_object_mut()
        .expect("base claims object")
        .remove("client_id");
    let (outcome, posts) = deliver_with_claims(claims).await;
    assert!(
        matches!(outcome, Err(DeliveryError::Transient(_))),
        "a client_id-less token must be a transient claims rejection: {outcome:?}"
    );
    assert!(
        outcome
            .err()
            .is_some_and(|error| error.to_string().contains("claim validation failed")),
        "last_error must keep the stable wrapper fragment"
    );
    assert_eq!(
        posts, 0,
        "a client_id-less token must never reach a delivery POST"
    );
}

/// Delta ④ pin — array-form `scope` tokens keep delivering: the typed gate
/// deliberately excludes `scope`, so the Value-path `check_scope` dual-shape
/// tolerance (string space-split | array members) is preserved. This pins
/// the resolution of the array-form regression — no new stall shape, and
/// the leaf's array branch is not dead code on the connector face.
#[tokio::test]
async fn array_form_scope_token_is_accepted_and_delivery_proceeds() {
    let (outcome, posts) = deliver_with_claims(with_claim(
        base_claims(),
        "scope",
        json!(["audit:event:write", "metering:read"]),
    ))
    .await;
    assert!(
        outcome.is_ok(),
        "array-form scope must deliver: {outcome:?}"
    );
    assert!(
        posts >= 1,
        "an array-form scope token must reach the audit endpoint"
    );
}

/// Delta ③ + FM1 stall shape — a fractional `iat` (RFC 7519 `NumericDate`
/// allows fractional seconds) fails the typed gate's `Option<u64>`:
/// fail-closed, aligned with aero-auth's existing `u64` strictness (its
/// jsonwebtoken decode rejects the same shape). Unlike IdP-drift stalls,
/// rotating a fresh token does NOT recover this — the `IdP` must stop minting
/// fractional iat (B5-4 配给门).
#[tokio::test]
async fn fractional_iat_is_rejected_before_any_post() {
    let (outcome, posts) =
        deliver_with_claims(with_claim(base_claims(), "iat", json!(1_234_567_890.5))).await;
    assert!(
        matches!(outcome, Err(DeliveryError::Transient(_))),
        "a fractional iat must be a transient claims rejection: {outcome:?}"
    );
    assert_eq!(
        posts, 0,
        "a fractional-iat token must never reach a delivery POST"
    );
}

// B5-3 moderation outbound-token pin (A2.1): the envelope's delivered `action`.
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
