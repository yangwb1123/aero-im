use super::{credentials::RedactedAccessToken, invalid_ws_query_response, WsParams};
use axum::{
    body::{to_bytes, Body},
    extract::{rejection::QueryRejection, Query},
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use tower::ServiceExt as _;

#[test]
fn websocket_query_debug_never_formats_the_bearer_token() {
    let secret = "aero-secret-access-token";
    let params = WsParams {
        token: Some(RedactedAccessToken::for_test(secret)),
        since: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned()),
        summarize: Some(true),
        cursors: Some("1".to_owned()),
    };

    let rendered = format!("{params:?}");
    assert!(!rendered.contains(secret));
    assert!(rendered.contains("[REDACTED]"));
}

async fn safe_query(query: Result<Query<WsParams>, QueryRejection>) -> Response {
    match query {
        Ok(Query(params)) => format!("{params:?}").into_response(),
        Err(_) => invalid_ws_query_response(),
    }
}

#[tokio::test]
async fn malformed_query_error_never_echoes_the_token() {
    let secret = "query-secret-must-not-escape";
    let app = Router::new().route("/", get(safe_query));
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/?token={secret}&summarize=not-a-bool"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(body, "invalid websocket query");
    assert!(!body.contains(secret));
}

#[test]
fn websocket_handler_span_skips_every_argument() {
    let source = include_str!("mod.rs");
    assert!(
        source.contains("#[instrument(name = \"ws_handshake\", skip_all)]\npub async fn handler")
    );
}
