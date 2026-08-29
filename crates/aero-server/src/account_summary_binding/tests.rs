use super::*;
use aero_auth::StaticKeyProvider;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::DecodingKey;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{SystemTime, UNIX_EPOCH};

const PRIVATE_KEY_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\n\
MC4CAQAwBQYDK2VwBCIEIGrD/e7uKYqSY4twDEsRfMMuLSrODf14dpTiTK6K1YI0\n\
-----END PRIVATE KEY-----\n";
const PUBLIC_KEY_X: &str = "2-Jj2UvNCvQiUPNYRgSi0cJSPiJI6Rs6D0UTeEpQVj8";

#[derive(Clone, Serialize)]
struct WireClaims {
    iss: String,
    aud: String,
    sub: String,
    client_id: String,
    account_id: String,
    canonical_uid: String,
    tenant_id: String,
    region: String,
    datasets: Vec<String>,
    method: String,
    path: String,
    request_hash: String,
    scope: String,
    jti: String,
    iat: u64,
    nbf: u64,
    exp: u64,
}

fn config() -> TargetAssertionConfig {
    TargetAssertionConfig {
        issuer: "https://aero-id.example.test/assertions".into(),
        audience: "aero-im-account-summary".into(),
        subject: "aero-id-sync".into(),
        scope: ACCOUNT_SUMMARY_SCOPE.into(),
        max_lifetime_secs: 60,
        clock_skew_secs: 30,
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_secs()
}

fn wire_claims(cfg: &TargetAssertionConfig, now: u64) -> WireClaims {
    let datasets = BTreeSet::from(["aero-im.profile".into(), "aero-im.workspaces".into()]);
    let hash = canonical_request_hash(
        ACCOUNT_TARGET_ASSERTION_METHOD,
        ACCOUNT_TARGET_ASSERTION_PATH,
        "opaque-account-1",
        "local",
        &datasets,
    )
    .expect("hash");
    WireClaims {
        iss: cfg.issuer.clone(),
        aud: cfg.audience.clone(),
        sub: cfg.subject.clone(),
        client_id: "aero-id-im-source".into(),
        account_id: "opaque-account-1".into(),
        canonical_uid: "canonical-user-1".into(),
        tenant_id: "tenant-1".into(),
        region: "local".into(),
        datasets: datasets.into_iter().collect(),
        method: ACCOUNT_TARGET_ASSERTION_METHOD.into(),
        path: ACCOUNT_TARGET_ASSERTION_PATH.into(),
        request_hash: URL_SAFE_NO_PAD.encode(hash),
        scope: cfg.scope.clone(),
        jti: "jti-1".into(),
        iat: now,
        nbf: now,
        exp: now + 30,
    }
}

fn sign(claims: &WireClaims, kid: Option<&str>) -> String {
    let mut header = Header::new(Algorithm::EdDSA);
    header.typ = Some(aero_auth::ACCOUNT_TARGET_ASSERTION_TYPE.into());
    header.kid = kid.map(ToOwned::to_owned);
    encode(
        &header,
        claims,
        &EncodingKey::from_ed_pem(PRIVATE_KEY_PEM).expect("private key"),
    )
    .expect("target assertion")
}

fn headers(token: &str) -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        ACCOUNT_SUMMARY_TARGET_ASSERTION_HEADER,
        axum::http::HeaderValue::from_str(token).expect("header value"),
    );
    for (name, value) in [
        ("x-aero-account-id", "opaque-account-1"),
        ("x-aero-canonical-uid", "canonical-user-1"),
        ("x-aero-tenant-id", "tenant-1"),
        ("x-aero-region", "local"),
    ] {
        headers.insert(name, axum::http::HeaderValue::from_static(value));
    }
    headers
}

fn verifier(replay: Arc<dyn ReplayStore>) -> AccountSummaryTargetVerifier {
    AccountSummaryTargetVerifier::with_dependencies(
        config(),
        Arc::new(StaticKeyProvider::with_keyed(vec![(
            "assertion-key".into(),
            DecodingKey::from_ed_components(PUBLIC_KEY_X).expect("public key"),
        )])),
        replay,
    )
    .expect("verifier")
}

struct RecordingReplay {
    calls: AtomicUsize,
    result: bool,
}

#[axum::async_trait]
impl ReplayStore for RecordingReplay {
    async fn claim(&self, _key: String, _ttl: Duration) -> Result<bool, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.result)
    }
}

#[tokio::test]
async fn valid_assertion_binds_client_target_and_hash() {
    let replay = Arc::new(RecordingReplay {
        calls: AtomicUsize::new(0),
        result: true,
    });
    let verifier = verifier(replay.clone());
    let now = now();
    let token = sign(&wire_claims(&config(), now), Some("assertion-key"));
    let datasets = BTreeSet::from(["aero-im.profile".into(), "aero-im.workspaces".into()]);
    let target = verifier
        .verify_request(
            "aero-id-im-source",
            "opaque-account-1",
            Some("local"),
            &datasets,
            &headers(&token),
        )
        .await
        .expect("valid binding");
    assert_eq!(target.canonical_uid, "canonical-user-1");
    verifier.consume_replay(&target).await.expect("first use");
    assert_eq!(replay.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn target_and_request_mutations_are_rejected_before_replay() {
    let replay = Arc::new(RecordingReplay {
        calls: AtomicUsize::new(0),
        result: true,
    });
    let verifier = verifier(replay.clone());
    let now = now();
    let datasets = BTreeSet::from(["aero-im.profile".into(), "aero-im.workspaces".into()]);
    let cases = [
        (
            "account",
            "different-account",
            Some("local"),
            datasets.clone(),
        ),
        (
            "region",
            "opaque-account-1",
            Some("remote"),
            datasets.clone(),
        ),
        (
            "dataset",
            "opaque-account-1",
            Some("local"),
            BTreeSet::from(["aero-im.profile".into()]),
        ),
        (
            "client",
            "opaque-account-1",
            Some("local"),
            datasets.clone(),
        ),
    ];
    for (name, account, region, requested) in cases {
        let client = if name == "client" {
            "different-client"
        } else {
            "aero-id-im-source"
        };
        let result = verifier
            .verify_request(
                client,
                account,
                region,
                &requested,
                &headers(&sign(&wire_claims(&config(), now), Some("assertion-key"))),
            )
            .await;
        assert!(result.is_err(), "accepted {name} mutation");
    }
    assert_eq!(
        replay.calls.load(Ordering::SeqCst),
        0,
        "invalid targets must not touch replay state"
    );
}

#[tokio::test]
async fn missing_duplicate_and_malformed_assertion_headers_fail_closed() {
    let replay = Arc::new(RecordingReplay {
        calls: AtomicUsize::new(0),
        result: true,
    });
    let verifier = verifier(replay);
    let now = now();
    let datasets = BTreeSet::from(["aero-im.profile".into(), "aero-im.workspaces".into()]);
    let empty = axum::http::HeaderMap::new();
    assert!(verifier
        .verify_request(
            "aero-id-im-source",
            "opaque-account-1",
            Some("local"),
            &datasets,
            &empty
        )
        .await
        .is_err());

    let token = sign(&wire_claims(&config(), now), Some("assertion-key"));
    let mut duplicate = headers(&token);
    duplicate.append(
        ACCOUNT_SUMMARY_TARGET_ASSERTION_HEADER,
        axum::http::HeaderValue::from_static("another"),
    );
    assert!(verifier
        .verify_request(
            "aero-id-im-source",
            "opaque-account-1",
            Some("local"),
            &datasets,
            &duplicate
        )
        .await
        .is_err());
}

#[tokio::test]
async fn replay_store_failure_is_fail_closed() {
    struct FailedReplay;
    #[axum::async_trait]
    impl ReplayStore for FailedReplay {
        async fn claim(&self, _key: String, _ttl: Duration) -> Result<bool, String> {
            Err("redis unavailable".into())
        }
    }
    let verifier = verifier(Arc::new(FailedReplay));
    let now = now();
    let datasets = BTreeSet::from(["aero-im.profile".into(), "aero-im.workspaces".into()]);
    let target = verifier
        .verify_request(
            "aero-id-im-source",
            "opaque-account-1",
            Some("local"),
            &datasets,
            &headers(&sign(&wire_claims(&config(), now), Some("assertion-key"))),
        )
        .await
        .expect("cryptographic binding");
    assert!(matches!(
        verifier.consume_replay(&target).await,
        Err(AeroError::Upstream(_))
    ));
}

#[test]
fn canonical_request_vector_is_stable_across_languages() {
    let datasets = BTreeSet::from(["aero-im.workspaces".into(), "aero-im.profile".into()]);
    let bytes = canonical_request_bytes(
        ACCOUNT_TARGET_ASSERTION_METHOD,
        ACCOUNT_TARGET_ASSERTION_PATH,
        "acct-opaque-01",
        "us-east-1",
        &datasets,
    )
    .expect("canonical bytes");
    let hex = hex::encode(&bytes);
    assert_eq!(
        hex,
        "000000176165726f2d6163636f756e742d73756d6d6172792d763100000003474554000000192f696e7465726e616c2f6163636f756e742d73756d6d6172790000000e616363742d6f70617175652d30310000000975732d656173742d310000000f6165726f2d696d2e70726f66696c65000000126165726f2d696d2e776f726b737061636573"
    );
    let hash = Sha256::digest(bytes);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(hash),
        "6U4cCAVicfmUggJmtb9rNdAJsKhhkfymA3teXMoDq3g"
    );
}
