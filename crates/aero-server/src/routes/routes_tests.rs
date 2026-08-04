// Tests for the route handlers, split out of routes.rs (3000-line routes exemption).

use super::*;
use axum::body::Body;
use axum::http::Request as HttpRequest;
use tower::ServiceExt as _; // `oneshot`

// Health/readiness tests live in `routes::health` now (REFACTOR_PLAN.md Step
// 7 split) — `health_live` and `readiness_decision` are private to that
// module and no longer reachable from here.

const COMPRESSION_TEST_ENTITY: &[u8] =
    b"range-capable attachment bytes stay identical through the global layer";

fn compression_test_etag() -> HeaderValue {
    use sha2::Digest as _;

    HeaderValue::from_str(&format!(
        "\"{}\"",
        hex::encode(sha2::Sha256::digest(COMPRESSION_TEST_ENTITY))
    ))
    .unwrap()
}

fn compression_test_response(
    status: StatusCode,
    body: &'static [u8],
    content_range: Option<&'static str>,
) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(header::ETAG, compression_test_etag());
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string()).unwrap(),
    );
    if let Some(content_range) = content_range {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_static(content_range),
        );
    }
    response
}

async fn compression_full_probe() -> Response {
    compression_test_response(StatusCode::OK, COMPRESSION_TEST_ENTITY, None)
}

async fn compression_range_probe() -> Response {
    compression_test_response(
        StatusCode::PARTIAL_CONTENT,
        &COMPRESSION_TEST_ENTITY[10..20],
        Some("bytes 10-19/70"),
    )
}

#[tokio::test]
async fn global_compression_preserves_range_capable_200_head_and_206() {
    let app: Router = Router::new()
        .route("/full", get(compression_full_probe))
        .route("/range", get(compression_range_probe))
        .layer(response_compression_layer());

    let get_response = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri("/full")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_response.status(), StatusCode::OK);
    assert!(!get_response
        .headers()
        .contains_key(header::CONTENT_ENCODING));
    assert_eq!(get_response.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(
        get_response.headers()[header::ETAG],
        compression_test_etag()
    );
    let get_body = axum::body::to_bytes(get_response.into_body(), 1 << 16)
        .await
        .unwrap();
    assert_eq!(&get_body[..], COMPRESSION_TEST_ENTITY);

    let head_response = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method(axum::http::Method::HEAD)
                .uri("/full")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head_response.status(), StatusCode::OK);
    assert!(!head_response
        .headers()
        .contains_key(header::CONTENT_ENCODING));
    assert_eq!(head_response.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(
        head_response.headers()[header::ETAG],
        compression_test_etag()
    );
    assert!(axum::body::to_bytes(head_response.into_body(), 1 << 16)
        .await
        .unwrap()
        .is_empty());

    let range_response = app
        .oneshot(
            HttpRequest::builder()
                .uri("/range")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(range_response.status(), StatusCode::PARTIAL_CONTENT);
    assert!(!range_response
        .headers()
        .contains_key(header::CONTENT_ENCODING));
    assert_eq!(
        range_response.headers()[header::CONTENT_RANGE],
        "bytes 10-19/70"
    );
    assert_eq!(
        range_response.headers()[header::ETAG],
        compression_test_etag()
    );
    let range_body = axum::body::to_bytes(range_response.into_body(), 1 << 16)
        .await
        .unwrap();
    assert_eq!(&range_body[..], &COMPRESSION_TEST_ENTITY[10..20]);
}

#[test]
fn history_limit_defaults_and_clamps() {
    // Absent ⇒ the documented default page size.
    assert_eq!(history_limit(None), DEFAULT_HISTORY_LIMIT);
    // Below the floor clamps up to 1; zero/negatives are never honored.
    assert_eq!(history_limit(Some(0)), 1);
    assert_eq!(history_limit(Some(-10)), 1);
    // In-window values pass through.
    assert_eq!(history_limit(Some(50)), 50);
    assert_eq!(history_limit(Some(MAX_HISTORY_LIMIT)), MAX_HISTORY_LIMIT);
    // Above the ceiling clamps down to the cap (mirrors the storage clamp).
    assert_eq!(
        history_limit(Some(MAX_HISTORY_LIMIT + 1)),
        MAX_HISTORY_LIMIT
    );
    assert_eq!(history_limit(Some(i64::MAX)), MAX_HISTORY_LIMIT);
}

#[test]
fn history_replica_policy_preserves_latest_and_reconnect_consistency() {
    let cursor = MessageId::new();
    assert_eq!(
        history_query_consistency(None, None, false),
        aero_storage::QueryConsistency::Strong
    );
    assert_eq!(
        history_query_consistency(None, Some(cursor), false),
        aero_storage::QueryConsistency::Strong
    );
    assert_eq!(
        history_query_consistency(Some(cursor), None, true),
        aero_storage::QueryConsistency::Eventual
    );
    assert_eq!(
        history_query_consistency(Some(cursor), None, false),
        aero_storage::QueryConsistency::Strong,
        "a future/current cursor is still the latest page"
    );
}

#[test]
fn parse_cursor_validates_and_labels() {
    // Absent ⇒ Ok(None).
    assert!(parse_cursor(None, "since").unwrap().is_none());
    // Valid id ⇒ Some(id), whitespace tolerated.
    let id = MessageId::new();
    assert_eq!(
        parse_cursor(Some(&id.to_string()), "since").unwrap(),
        Some(id)
    );
    assert_eq!(
        parse_cursor(Some(&format!(" {id} ")), "before").unwrap(),
        Some(id)
    );
    // Garbage ⇒ Invalid error carrying the field label so the client knows
    // which cursor was bad.
    let err = parse_cursor(Some("nope"), "since").unwrap_err();
    match err {
        AeroError::Invalid(msg) => assert!(msg.contains("since id"), "got: {msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn login_request_accepts_totp_or_recovery_code() {
    let totp: LoginReq = serde_json::from_value(serde_json::json!({
        "email": "person@example.com",
        "password": "secret",
        "totp": "123456"
    }))
    .unwrap();
    assert_eq!(totp.totp.as_deref(), Some("123456"));
    assert!(totp.recovery_code.is_none());

    let recovery: LoginReq = serde_json::from_value(serde_json::json!({
        "email": "person@example.com",
        "password": "secret",
        "recovery_code": "A1B2C3D4E5F60708"
    }))
    .unwrap();
    assert!(recovery.totp.is_none());
    assert_eq!(recovery.recovery_code.as_deref(), Some("A1B2C3D4E5F60708"));
}

/// Router-level test that the history route path + `HistoryQuery` extractor
/// accept the `?since=` (and `before`/`limit`) params offline. Mounts the
/// real path pattern and the real `HistoryQuery` type on a stand-in handler
/// (the production handler needs a full `AppState` → PG/Redis, absent in CI),
/// so this proves routing + query extraction without external deps.
#[tokio::test]
async fn history_route_accepts_since_query() {
    async fn probe(
        Path(room): Path<String>,
        Query(q): Query<HistoryQuery>,
    ) -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "room": room,
            "since": q.since,
            "before": q.before,
            "limit": q.limit,
        }))
    }
    let app: Router = Router::new().route("/api/rooms/:id/messages", get(probe));

    let id = MessageId::new();
    let resp = app
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/api/rooms/room-1/messages?since={id}&limit=50"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // 200 (not 404/400) proves the path matched and `since` deserialized.
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["since"], id.to_string());
    assert_eq!(v["limit"], 50);
    assert!(v["before"].is_null());
}

// ----- Workspace scoping (multi-tenant rollout) -----

#[test]
fn default_workspace_id_is_the_all_zero_uuid() {
    // Migration 0006 backfilled `rooms.workspace_id` (and the default
    // workspace row) with the all-zero UUID. The const MUST map to exactly
    // that, or omitting `workspace_id` would target the wrong (or a
    // nonexistent) tenant.
    assert_eq!(DEFAULT_WORKSPACE_ID.to_uuid(), uuid::Uuid::nil());
    // And it round-trips through the same UUID constructor the storage layer
    // binds with.
    assert_eq!(
        DEFAULT_WORKSPACE_ID,
        WorkspaceId::from_uuid(uuid::Uuid::nil())
    );
}

#[test]
fn resolve_workspace_id_defaults_when_absent() {
    // Absent (single-tenant client) ⇒ the legacy default workspace.
    assert_eq!(resolve_workspace_id(None).unwrap(), DEFAULT_WORKSPACE_ID);
}

#[test]
fn resolve_workspace_id_uses_provided_value() {
    // Present + valid ⇒ exactly that workspace, whitespace tolerated.
    let ws = WorkspaceId::new();
    assert_eq!(resolve_workspace_id(Some(&ws.to_string())).unwrap(), ws);
    assert_eq!(
        resolve_workspace_id(Some(&format!("  {ws}  "))).unwrap(),
        ws
    );
}

#[test]
fn resolve_workspace_id_rejects_garbage() {
    // Present + undecodable ⇒ Invalid (400), NOT a silent fall-through to the
    // default (which would mask a client bug and cross tenant boundaries).
    let err = resolve_workspace_id(Some("not-a-ulid")).unwrap_err();
    match err {
        AeroError::Invalid(msg) => assert!(msg.contains("workspace id"), "got: {msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn generic_room_creation_rejects_direct_kind() {
    assert!(matches!(
        validate_generic_room_kind(RoomKind::Direct),
        Err(AeroError::Invalid(_))
    ));
    assert_eq!(
        validate_generic_room_kind(RoomKind::Group).unwrap(),
        RoomKind::Group
    );
    assert_eq!(
        validate_generic_room_kind(RoomKind::Channel).unwrap(),
        RoomKind::Channel
    );
}

/// Router-level proof that `CreateRoomReq` parses the OPTIONAL `workspace_id`
/// body field and that the handler's default-selection composes correctly:
/// omitting it resolves to [`DEFAULT_WORKSPACE_ID`], supplying it resolves to
/// that id. Uses a stand-in handler with the real request type + the real
/// `resolve_workspace_id` (the production handler needs a full `AppState`).
#[tokio::test]
async fn create_room_body_parses_optional_workspace_id() {
    async fn probe(Json(req): Json<CreateRoomReq>) -> Json<serde_json::Value> {
        let ws =
            resolve_workspace_id(req.workspace_id.as_deref()).expect("valid workspace id in test");
        Json(serde_json::json!({ "kind": req.kind, "workspace": ws.to_string() }))
    }
    let app: Router = Router::new().route("/api/rooms", post(probe));

    // (a) Body WITHOUT workspace_id ⇒ resolves to the default workspace.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/api/rooms")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"kind":"group","name":"hi"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["kind"], "group");
    assert_eq!(v["workspace"], DEFAULT_WORKSPACE_ID.to_string());

    // (b) Body WITH workspace_id ⇒ resolves to exactly that workspace.
    let ws = WorkspaceId::new();
    let resp = app
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/api/rooms")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"kind":"channel","workspace_id":"{ws}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["kind"], "channel");
    assert_eq!(v["workspace"], ws.to_string());
}

/// Router-level proof that `list_rooms`'s `ListRoomsQuery` extractor accepts an
/// optional `?workspace_id=` and exposes it (present vs absent) so the handler
/// can branch scoped-vs-all. Stand-in handler (the real one needs `AppState`).
#[tokio::test]
async fn list_rooms_query_parses_optional_workspace_id() {
    async fn probe(Query(q): Query<ListRoomsQuery>) -> Json<serde_json::Value> {
        Json(serde_json::json!({ "workspace_id": q.workspace_id }))
    }
    let app: Router = Router::new().route("/api/rooms", get(probe));

    // Absent ⇒ None (handler keeps legacy "all my rooms" behavior).
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri("/api/rooms")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v["workspace_id"].is_null());

    // Present ⇒ surfaced verbatim (handler scopes via rooms_for_in_workspace).
    let ws = WorkspaceId::new();
    let resp = app
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/api/rooms?workspace_id={ws}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["workspace_id"], ws.to_string());
}

#[test]
fn whip_ingest_guard_accepts_idle_or_ended_whip_streams() {
    assert!(validate_whip_ingest(StreamProtocol::Whip, StreamStatus::Idle).is_ok());
    assert!(validate_whip_ingest(StreamProtocol::Whip, StreamStatus::Ended).is_ok());
}

#[test]
fn whip_ingest_guard_rejects_live_or_different_protocol_streams() {
    let live = validate_whip_ingest(StreamProtocol::Whip, StreamStatus::Live).unwrap_err();
    assert_eq!(live.status_code(), StatusCode::CONFLICT.as_u16());

    for protocol in [StreamProtocol::Rtmp, StreamProtocol::Srt] {
        let wrong_protocol = validate_whip_ingest(protocol, StreamStatus::Idle).unwrap_err();
        assert_eq!(wrong_protocol.status_code(), StatusCode::CONFLICT.as_u16());
    }
}

#[test]
fn remote_whep_target_redirects_only_to_the_registered_owner() {
    let stream_id = ulid::Ulid::new();
    assert_eq!(
        remote_whep_target(
            "https://node-b.example",
            Some("https://node-a.example/"),
            stream_id
        ),
        Some(format!("https://node-a.example/whep/{stream_id}"))
    );
    assert_eq!(
        remote_whep_target(
            "https://node-b.example",
            Some("https://node-b.example/"),
            stream_id
        ),
        None
    );
    assert_eq!(
        remote_whep_target("https://node-b.example", None, stream_id),
        None
    );
}
