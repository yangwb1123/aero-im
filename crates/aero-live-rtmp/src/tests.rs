use super::*;

#[test]
fn ingest_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RtmpIngest>();
}

#[test]
fn segment_duration_is_two_seconds() {
    // The README and design doc both reference a ~2s segment cadence; keep
    // them in sync with a test so a future refactor can't silently break it.
    assert_eq!(SEGMENT_DURATION_SECS, 2);
}

#[test]
fn validate_stream_key_accepts_generated_shape() {
    // A 32-char hex token is exactly what `random_key()` emits in
    // aero-storage; the canonical happy path must pass.
    assert!(validate_stream_key("0123456789abcdef0123456789abcdef").is_ok());
    // Custom keys with common URL-safe punctuation are also fine.
    assert!(validate_stream_key("live-room_42.key").is_ok());
    // Exactly at the length limit is allowed.
    let max = "a".repeat(MAX_STREAM_KEY_LEN);
    assert!(validate_stream_key(&max).is_ok());
}

#[test]
fn validate_stream_key_rejects_empty_and_whitespace() {
    assert_eq!(validate_stream_key(""), Err(StreamKeyRejection::Empty));
    assert_eq!(
        validate_stream_key("   \t "),
        Err(StreamKeyRejection::Empty)
    );
}

#[test]
fn validate_stream_key_rejects_too_long_before_scanning() {
    // One byte over the limit. Length is checked first so an oversized blob
    // never gets a full character scan.
    let oversized = "x".repeat(MAX_STREAM_KEY_LEN + 1);
    assert_eq!(
        validate_stream_key(&oversized),
        Err(StreamKeyRejection::TooLong)
    );
    // A huge blob that also contains control chars is still classified by
    // length (cheapest-first ordering), proving the early return.
    let huge = format!("{}\n", "y".repeat(MAX_STREAM_KEY_LEN * 4));
    assert_eq!(validate_stream_key(&huge), Err(StreamKeyRejection::TooLong));
}

#[test]
fn validate_stream_key_rejects_control_characters() {
    assert_eq!(
        validate_stream_key("good\nkey"),
        Err(StreamKeyRejection::ControlChar)
    );
    assert_eq!(
        validate_stream_key("nul\0byte"),
        Err(StreamKeyRejection::ControlChar)
    );
    // A bare carriage return would let a publisher forge log lines.
    assert_eq!(
        validate_stream_key("key\r INFO forged"),
        Err(StreamKeyRejection::ControlChar)
    );
}

#[test]
fn validate_stream_key_rejects_path_traversal() {
    assert_eq!(
        validate_stream_key("../../etc/passwd"),
        Err(StreamKeyRejection::PathTraversal)
    );
    assert_eq!(
        validate_stream_key("a/b"),
        Err(StreamKeyRejection::PathTraversal)
    );
    assert_eq!(
        validate_stream_key("windows\\style"),
        Err(StreamKeyRejection::PathTraversal)
    );
    // `..` without a slash is still suspicious and rejected.
    assert_eq!(
        validate_stream_key("ab..cd"),
        Err(StreamKeyRejection::PathTraversal)
    );
}

#[tokio::test]
async fn idle_listener_stops_promptly_when_cancelled() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = probe.local_addr().unwrap();
    drop(probe);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://u:p@localhost/aero")
        .unwrap();
    let cfg = Arc::new(LiveStreamConfig {
        hls_dir: std::env::temp_dir().join("aero-rtmp-cancel-test"),
        rtmp_listen: listen,
    });
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        RtmpIngest::new()
            .run_until_cancelled(StreamRepo::new(pool), cfg, task_cancel)
            .await
    });

    tokio::time::sleep(Duration::from_millis(20)).await;
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("cancelled listener should stop promptly")
        .expect("listener task should not panic");
    assert!(result.is_ok(), "listener should stop cleanly: {result:?}");
}
