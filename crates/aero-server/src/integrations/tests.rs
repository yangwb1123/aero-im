use std::collections::BTreeSet;

use super::account_summary::{
    account_notification_limit_usize, authorize_account_summary_target, notification_dataset,
    parse_account_summary_query, requested_account_datasets,
    validate_account_summary_legacy_headers, validate_bound_account_summary_target,
    AccountSummaryRequest, AuthorizedAccountSummaryTarget, ACCOUNT_ID_HEADER,
    ACCOUNT_SUMMARY_BINDING_UNAVAILABLE, CANONICAL_UID_HEADER, MAX_ACCOUNT_SUMMARY_QUERY_BYTES,
    REGION_HEADER, REQUIRED_ACCOUNT_SUMMARY_SCOPE, TENANT_ID_HEADER,
};
use super::*;
use aero_common::Notification;

#[test]
fn bearer_and_idempotency_headers_are_strict() {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("bearer token"),
    );
    assert_eq!(bearer_token(&headers).unwrap(), "token");
    headers.append(
        header::AUTHORIZATION,
        HeaderValue::from_static("Basic attacker"),
    );
    assert!(bearer_token(&headers).is_err());
    headers.remove(header::AUTHORIZATION);
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("bearer token"),
    );
    headers.insert("idempotency-key", HeaderValue::from_static("not-a-uuid"));
    assert!(required_idempotency_key(&headers).is_err());
    let key = Uuid::new_v4();
    headers.insert(
        "idempotency-key",
        HeaderValue::from_str(&key.to_string()).unwrap(),
    );
    assert_eq!(required_idempotency_key(&headers).unwrap(), key);
    headers.append(
        "idempotency-key",
        HeaderValue::from_str(&Uuid::new_v4().to_string()).unwrap(),
    );
    assert!(required_idempotency_key(&headers).is_err());
}

#[test]
fn request_hash_binds_target_and_blocks() {
    let room = IntegrationTarget::Room(RoomId::new());
    let base = notification_request_hash(&room, &[Block::text("hello")]).unwrap();
    assert_eq!(
        base,
        notification_request_hash(&room, &[Block::text("hello")]).unwrap()
    );
    assert_ne!(
        base,
        notification_request_hash(&room, &[Block::text("changed")]).unwrap()
    );
    assert_ne!(
        base,
        notification_request_hash(
            &IntegrationTarget::SnaplinkUser("user-1".into()),
            &[Block::text("hello")],
        )
        .unwrap()
    );
}

#[test]
fn installation_user_dm_policy_is_opt_in() {
    let request: CreateInstallationReq = serde_json::from_value(serde_json::json!({
        "bot_id": ParticipantId::new(),
        "client_id": "erp-client",
        "name": "ERP",
        "room_ids": [],
    }))
    .unwrap();
    assert!(!request.allow_user_dm);
    assert!(request.user_identity_issuer.is_none());

    let enabled: CreateInstallationReq = serde_json::from_value(serde_json::json!({
        "bot_id": ParticipantId::new(),
        "client_id": "erp-client",
        "name": "ERP",
        "user_identity_issuer": "https://human-sso.example",
        "allow_user_dm": true,
    }))
    .unwrap();
    assert!(enabled.allow_user_dm);
    assert_eq!(
        enabled.user_identity_issuer.as_deref(),
        Some("https://human-sso.example")
    );

    let update: UpdateInstallationReq = serde_json::from_value(serde_json::json!({
        "rotate_to_current_issuer": true,
        "user_identity_issuer": "https://rotated-human-sso.example"
    }))
    .unwrap();
    assert!(update.rotate_to_current_issuer);
    assert_eq!(
        update.user_identity_issuer.as_deref(),
        Some("https://rotated-human-sso.example")
    );
    assert!(
        serde_json::from_value::<UpdateInstallationReq>(serde_json::json!({
            "issuer": "https://attacker-controlled.example"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<CreateInstallationReq>(serde_json::json!({
            "bot_id": ParticipantId::new(),
            "client_id": "erp-client",
            "name": "ERP",
            "issuer": "https://attacker-controlled.example"
        }))
        .is_err()
    );
}

#[test]
fn human_identity_issuer_comes_only_from_trusted_admin_or_oidc_config() {
    assert_eq!(
        select_user_identity_issuer(None, Some("https://configured-human-sso.example".into()))
            .unwrap(),
        "https://configured-human-sso.example"
    );
    assert_eq!(
        select_user_identity_issuer(
            Some("https://admin-selected-human-sso.example".into()),
            Some("https://configured-human-sso.example".into())
        )
        .unwrap(),
        "https://admin-selected-human-sso.example"
    );
    assert!(select_user_identity_issuer(None, None).is_err());
    assert!(select_user_identity_issuer(Some(" bad".into()), None).is_err());
    assert!(select_user_identity_issuer(Some("https://bad\nissuer".into()), None).is_err());
}

#[test]
fn pending_claim_reprobes_are_bounded_and_back_off() {
    let mut backoff = ClaimBackoff::new();
    let mut delays = Vec::new();
    while let Some(delay) = backoff.next_delay() {
        delays.push(delay);
    }
    assert_eq!(delays.len(), usize::from(CLAIM_REPROBE_LIMIT));
    assert_eq!(delays[0], CLAIM_REPROBE_INITIAL);
    assert_eq!(delays.last().copied(), Some(CLAIM_REPROBE_MAX));
    assert!(delays.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(backoff.next_delay().is_none());

    let response = request_pending_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(response.headers().get("retry-after").unwrap(), "1");
}

#[test]
fn integration_rate_limit_response_has_retry_after_without_changing_other_errors() {
    let response = IntegrationApiError::from(AeroError::RateLimited).into_response();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.headers().get(header::RETRY_AFTER).unwrap(),
        INTEGRATION_RATE_RETRY_AFTER_SECS
    );

    let response =
        IntegrationApiError::from(AeroError::Invalid("bad request".into())).into_response();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response.headers().get(header::RETRY_AFTER).is_none());
}

#[test]
fn integration_upload_concurrency_is_bounded() {
    assert_eq!(
        integration_upload_concurrency_from_value(None),
        DEFAULT_INTEGRATION_UPLOAD_CONCURRENCY
    );
    assert_eq!(integration_upload_concurrency_from_value(Some("0")), 1);
    assert_eq!(integration_upload_concurrency_from_value(Some("7")), 7);
    assert_eq!(
        integration_upload_concurrency_from_value(Some("999999")),
        MAX_INTEGRATION_UPLOAD_CONCURRENCY
    );
    assert_eq!(
        integration_upload_concurrency_from_value(Some("invalid")),
        DEFAULT_INTEGRATION_UPLOAD_CONCURRENCY
    );
}

#[test]
fn account_summary_query_is_bounded_strict_and_opaque() {
    let account_id = "tenant-A/account-opaque";
    let query = parse_account_summary_query(Some(&format!(
        "account_id={account_id}&region=local&dataset=aero-im.profile&dataset=aero-im.workspaces"
    )))
    .unwrap();
    assert_eq!(query.account_id, account_id);
    assert_eq!(query.region.as_deref(), Some("local"));
    assert_eq!(query.datasets.len(), 2);
    assert!(query.datasets.contains("aero-im.profile"));
    assert!(parse_account_summary_query(Some(&format!("account_id={account_id}"))).is_err());
    assert!(parse_account_summary_query(Some(&format!(
        "account_id={account_id}&region=local&dataset=aero-im.workspaces&dataset=aero-im.profile"
    )))
    .is_err());
    assert!(parse_account_summary_query(Some("account_id=")).is_err());
    assert!(parse_account_summary_query(Some("account_id= account")).is_err());
    assert!(parse_account_summary_query(Some(&format!(
        "account_id={account_id}&account_id={account_id}"
    )))
    .is_err());
    assert!(parse_account_summary_query(Some(&format!(
        "account_id={account_id}&region=local&region=local"
    )))
    .is_err());
    assert!(parse_account_summary_query(Some(&format!(
        "account_id={account_id}&dataset=aero-im.profile&dataset=aero-im.profile"
    )))
    .is_err());
    assert!(parse_account_summary_query(Some(&format!(
        "account_id={account_id}&dataset=aero-im.secret"
    )))
    .is_err());
    assert!(
        parse_account_summary_query(Some(&format!("account_id={account_id}&unknown=value")))
            .is_err()
    );
    assert!(parse_account_summary_query(Some("account_id=%ZZ")).is_err());
    assert!(parse_account_summary_query(Some("account_id=a&region=local&")).is_err());
    assert!(parse_account_summary_query(Some("account_id=a&&region=local")).is_err());
    assert!(parse_account_summary_query(Some("account_id=a&reconcile=true&region=local")).is_err());
    assert!(
        parse_account_summary_query(Some(&"x".repeat(MAX_ACCOUNT_SUMMARY_QUERY_BYTES + 1)))
            .is_err()
    );
}

#[test]
fn account_summary_legacy_headers_are_consistency_only() {
    let target = AuthorizedAccountSummaryTarget {
        account_id: "tenant-A/account-opaque".into(),
        canonical_uid: "canonical-user-1".into(),
        tenant_id: "tenant-A".into(),
        region: "local".into(),
        datasets: BTreeSet::from(["aero-im.profile".into()]),
        jti: "test-jti".into(),
        exp: 1,
    };
    let mut headers = HeaderMap::new();
    assert!(validate_account_summary_legacy_headers(&headers, &target).is_err());

    headers.insert(
        ACCOUNT_ID_HEADER,
        HeaderValue::from_static("tenant-B/account-opaque"),
    );
    assert!(validate_account_summary_legacy_headers(&headers, &target).is_err());

    headers.insert(
        ACCOUNT_ID_HEADER,
        HeaderValue::from_static("tenant-A/account-opaque"),
    );
    headers.insert(
        CANONICAL_UID_HEADER,
        HeaderValue::from_static("canonical-user-1"),
    );
    headers.insert(TENANT_ID_HEADER, HeaderValue::from_static("tenant-A"));
    headers.insert(REGION_HEADER, HeaderValue::from_static("local"));
    assert!(validate_account_summary_legacy_headers(&headers, &target).is_ok());

    headers.append(
        ACCOUNT_ID_HEADER,
        HeaderValue::from_static("tenant-A/account-opaque"),
    );
    assert!(validate_account_summary_legacy_headers(&headers, &target).is_err());

    headers.remove(ACCOUNT_ID_HEADER);
    headers.insert(
        CANONICAL_UID_HEADER,
        HeaderValue::from_static("canonical-user-2"),
    );
    assert!(validate_account_summary_legacy_headers(&headers, &target).is_err());

    headers.remove(CANONICAL_UID_HEADER);
    headers.insert(
        CANONICAL_UID_HEADER,
        HeaderValue::from_static(" canonical-user-1"),
    );
    assert!(validate_account_summary_legacy_headers(&headers, &target).is_err());
}

#[tokio::test]
async fn account_summary_binding_gate_rejects_equal_headers_without_authorization() {
    let principal = MachinePrincipal {
        issuer: "https://issuer.example".into(),
        client_id: "aero-id".into(),
    };
    let request = AccountSummaryRequest {
        account_id: "tenant-A/account-opaque".into(),
        region: Some("local".into()),
        datasets: BTreeSet::from(["aero-im.profile".into()]),
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCOUNT_ID_HEADER,
        HeaderValue::from_static("tenant-A/account-opaque"),
    );
    headers.insert(
        CANONICAL_UID_HEADER,
        HeaderValue::from_static("canonical-user-B"),
    );

    let error = authorize_account_summary_target(None, &principal, &request, "local", &headers)
        .await
        .unwrap_err();
    assert!(
        matches!(error, AeroError::Upstream(ref message) if message == ACCOUNT_SUMMARY_BINDING_UNAVAILABLE)
    );
    assert!(!error.to_string().contains("tenant-A/account-opaque"));
    assert!(!error.to_string().contains("canonical-user-B"));
}

#[test]
fn account_summary_bound_target_requires_exact_target_fields() {
    let request = AccountSummaryRequest {
        account_id: "tenant-A/account-opaque".into(),
        region: Some("local".into()),
        datasets: BTreeSet::from(["aero-im.profile".into()]),
    };
    let target = AuthorizedAccountSummaryTarget {
        account_id: request.account_id.clone(),
        canonical_uid: "canonical-user-A".into(),
        tenant_id: "tenant-A".into(),
        region: "local".into(),
        datasets: request.datasets.clone(),
        jti: "test-jti".into(),
        exp: 1,
    };
    assert!(validate_bound_account_summary_target(&request, &target, "local").is_ok());

    for mutate in [
        |target: &mut AuthorizedAccountSummaryTarget| {
            target.account_id = "tenant-B/account-opaque".into();
        },
        |target: &mut AuthorizedAccountSummaryTarget| target.region = "remote".into(),
        |target: &mut AuthorizedAccountSummaryTarget| {
            target.datasets = BTreeSet::from(["aero-im.workspaces".into()]);
        },
        |target: &mut AuthorizedAccountSummaryTarget| target.canonical_uid.clear(),
        |target: &mut AuthorizedAccountSummaryTarget| target.tenant_id.clear(),
    ] {
        let mut mutated = target.clone();
        mutate(&mut mutated);
        assert!(validate_bound_account_summary_target(&request, &mutated, "local").is_err());
    }
}

#[test]
fn account_summary_audience_override_leaves_generic_integrations_unchanged() {
    let keys = [
        "AERO__INTEGRATIONS__ISSUER",
        "AERO__INTEGRATIONS__AUDIENCE",
        "AERO__INTEGRATIONS__JWKS_URI",
    ];
    let previous = keys
        .iter()
        .map(|key| (*key, std::env::var_os(key)))
        .collect::<Vec<_>>();
    for (key, value) in [
        ("AERO__INTEGRATIONS__ISSUER", "https://sso.example"),
        ("AERO__INTEGRATIONS__AUDIENCE", "aero-im-integration"),
        ("AERO__INTEGRATIONS__JWKS_URI", "https://sso.example/jwks"),
    ] {
        std::env::set_var(key, value);
    }
    let generic = IntegrationAuthConfig::from_env("generic.scope").unwrap();
    let account_summary = IntegrationAuthConfig::from_env_with_audience(
        REQUIRED_ACCOUNT_SUMMARY_SCOPE,
        Some(super::ACCOUNT_SUMMARY_ACCESS_AUDIENCE),
    )
    .unwrap();
    for (key, value) in previous {
        if let Some(value) = value {
            std::env::set_var(key, value);
        } else {
            std::env::remove_var(key);
        }
    }

    assert_eq!(generic.token.audience, "aero-im-integration");
    assert_eq!(
        account_summary.token.audience,
        super::ACCOUNT_SUMMARY_ACCESS_AUDIENCE
    );
    assert_eq!(generic.jwks_uri, account_summary.jwks_uri);
    assert_eq!(generic.token.issuer, account_summary.token.issuer);
}

#[tokio::test]
async fn account_summary_auth_gate_rejects_missing_or_malformed_machine_auth() {
    for headers in [HeaderMap::new(), {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic attacker"),
        );
        headers
    }] {
        assert!(matches!(
            authenticate_machine(&headers, REQUIRED_ACCOUNT_SUMMARY_SCOPE).await,
            Err(AeroError::Unauthorized(_))
        ));
    }
}

#[test]
fn account_summary_machine_auth_rejects_missing_invalid_and_duplicate_bearers() {
    assert!(matches!(
        bearer_token(&HeaderMap::new()),
        Err(AeroError::Unauthorized(_))
    ));

    for value in ["Basic token", "Bearer", "Bearer token "] {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static(value));
        assert!(matches!(
            bearer_token(&headers),
            Err(AeroError::Unauthorized(_))
        ));
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer token"),
    );
    headers.append(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer another-token"),
    );
    assert!(matches!(
        bearer_token(&headers),
        Err(AeroError::Unauthorized(_))
    ));
}

#[test]
fn account_summary_defaults_to_all_public_im_datasets() {
    let values = BTreeSet::new();
    assert_eq!(
        requested_account_datasets(&values),
        BTreeSet::from([
            "aero-im.activity_summary",
            "aero-im.notifications",
            "aero-im.profile",
            "aero-im.workspaces",
        ])
    );
}

#[test]
fn account_notification_projection_is_bounded_and_content_free() {
    use aero_common::{MessageId, NotificationId, NotificationKind};

    let participant_id = ParticipantId::new();
    let notification = Notification {
        id: NotificationId::new(),
        participant_id,
        room_id: RoomId::new(),
        message_id: MessageId::new(),
        kind: NotificationKind::Mention,
        actor_id: Some(ParticipantId::new()),
        created_at: time::OffsetDateTime::now_utc(),
        read_at: None,
        aggregate_count: None,
        importance_score: 1.0,
    };
    let projection = notification_dataset(vec![notification; 51], 51, "active");
    let items = projection["items"].as_array().unwrap();

    assert_eq!(items.len(), account_notification_limit_usize());
    assert_eq!(projection["unread_count"], 51);
    assert_eq!(projection["truncated"], true);
    assert_eq!(projection["source_account_status"], "active");
    assert!(items[0].get("participant_id").is_none());
    assert!(items[0].get("content").is_none());
    assert!(items[0].get("blocks").is_none());
}

#[test]
fn missing_account_notification_projection_keeps_the_stable_shape() {
    let projection = notification_dataset(Vec::new(), 0, "not_found");

    assert_eq!(projection["items"].as_array().unwrap().len(), 0);
    assert_eq!(projection["unread_count"], 0);
    assert_eq!(projection["truncated"], false);
    assert_eq!(projection["source_account_status"], "not_found");
}

#[tokio::test]
async fn integration_upload_gate_limits_concurrency_and_releases_permits() {
    let gate = IntegrationUploadGate::new(1);
    let first = gate.acquire().await.unwrap();
    assert_eq!(gate.available_permits(), 0);

    let waiting_gate = gate.clone();
    let mut waiting = tokio::spawn(async move { waiting_gate.acquire().await.unwrap() });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut waiting)
            .await
            .is_err()
    );

    drop(first);
    let second = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("waiting upload must acquire a released permit")
        .unwrap();
    assert_eq!(gate.available_permits(), 0);
    drop(second);
    assert_eq!(gate.available_permits(), 1);

    let final_permit = gate.acquire().await.unwrap();
    drop(final_permit);
    assert_eq!(gate.available_permits(), 1);
}
