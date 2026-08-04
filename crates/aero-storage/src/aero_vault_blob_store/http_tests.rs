use std::collections::HashMap;
use std::io::ErrorKind;

use bytes::Bytes;
use futures::TryStreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};

use super::{AeroVaultAuth, AeroVaultBlobStore, AeroVaultConfig};
use crate::blob_store::BlobRange;
use crate::{BlobStore, BlobStoreError};
use aero_common::BlobId;
use reqwest::Url;

#[derive(Debug)]
struct RecordedRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl RecordedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

struct MockResponse {
    status: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static [u8],
}

impl MockResponse {
    fn new(status: &'static str, body: &'static [u8]) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
        }
    }

    fn header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }
}

async fn mock_server(responses: Vec<MockResponse>) -> (Url, JoinHandle<Vec<RecordedRequest>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(responses.len());
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);

            let mut head = format!(
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                response.status,
                response.body.len()
            );
            for (name, value) in response.headers {
                head.push_str(name);
                head.push_str(": ");
                head.push_str(value);
                head.push_str("\r\n");
            }
            head.push_str("\r\n");
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(response.body).await.unwrap();
            socket.shutdown().await.unwrap();
        }
        requests
    });

    (Url::parse(&format!("http://{address}/")).unwrap(), handle)
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> RecordedRequest {
    const MAX_REQUEST_BYTES: usize = 128 * 1024;

    let mut raw = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "client closed before request headers completed");
        raw.extend_from_slice(&chunk[..read]);
        assert!(raw.len() <= MAX_REQUEST_BYTES, "test request is too large");
        if let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };

    let head = std::str::from_utf8(&raw[..header_end]).unwrap();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap().split_whitespace();
    let method = request_line.next().unwrap().to_owned();
    let target = request_line.next().unwrap().to_owned();
    let headers: HashMap<String, String> = lines
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    let content_length = headers
        .get("content-length")
        .map_or(0, |value| value.parse::<usize>().unwrap());
    while raw.len() < header_end + content_length {
        let read = socket.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "client closed before request body completed");
        raw.extend_from_slice(&chunk[..read]);
        assert!(raw.len() <= MAX_REQUEST_BYTES, "test request is too large");
    }

    RecordedRequest {
        method,
        target,
        headers,
        body: raw[header_end..header_end + content_length].to_vec(),
    }
}

fn bearer_config(base_url: Url) -> AeroVaultConfig {
    AeroVaultConfig {
        base_url,
        tenant: "tenant-a".into(),
        prefix: "erp/im".into(),
        auth: AeroVaultAuth::Bearer("vault-test-token".into()),
    }
}

fn io_kind(error: BlobStoreError) -> ErrorKind {
    match error {
        BlobStoreError::Io(error) => error.kind(),
        other => panic!("expected BlobStoreError::Io, got {other:?}"),
    }
}

async fn requests(handle: JoinHandle<Vec<RecordedRequest>>) -> Vec<RecordedRequest> {
    timeout(Duration::from_secs(3), handle)
        .await
        .expect("mock server did not receive every request")
        .unwrap()
}

#[tokio::test]
async fn put_sends_vault_contract_and_returns_stable_key() {
    let (base_url, server) = mock_server(vec![MockResponse::new("201 Created", b"")]).await;
    let store = AeroVaultBlobStore::try_new(bearer_config(base_url)).unwrap();
    let id = BlobId::new();

    let key = store
        .put(id, Bytes::from_static(b"attachment bytes"))
        .await
        .unwrap();

    assert_eq!(key, format!("aero-vault://tenant-a/erp/im/{id}"));
    let requests = requests(server).await;
    let request = &requests[0];
    assert_eq!(request.method, "PUT");
    assert_eq!(request.target, format!("/v1/files/erp/im/{id}"));
    assert_eq!(
        request.header("authorization"),
        Some("Bearer vault-test-token")
    );
    assert_eq!(request.header("x-aero-tenant"), Some("tenant-a"));
    assert_eq!(
        request.header("content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(request.header("content-length"), Some("16"));
    assert_eq!(
        request.header("idempotency-key"),
        Some(format!("aero-im-blob:{id}").as_str())
    );
    assert_eq!(request.body, b"attachment bytes");
}

#[tokio::test]
async fn full_and_range_get_require_their_exact_success_status() {
    let responses = vec![
        MockResponse::new("200 OK", b"abcdef"),
        MockResponse::new("206 Partial Content", b"bcd").header("Content-Range", "bytes 1-3/6"),
        MockResponse::new("206 Partial Content", b"abcdef"),
        MockResponse::new("200 OK", b"bcd"),
    ];
    let (base_url, server) = mock_server(responses).await;
    let store = AeroVaultBlobStore::try_new(bearer_config(base_url)).unwrap();
    let id = BlobId::new();

    assert_eq!(store.get(id).await.unwrap(), Bytes::from_static(b"abcdef"));
    let chunks = store
        .get_stream(id, BlobRange::new(1, 3))
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(chunks.concat(), b"bcd");
    assert_eq!(io_kind(store.get(id).await.unwrap_err()), ErrorKind::Other);
    assert_eq!(
        io_kind(
            store
                .get_stream(id, BlobRange::new(1, 3))
                .await
                .err()
                .unwrap()
        ),
        ErrorKind::Other
    );

    let requests = requests(server).await;
    assert_eq!(requests[0].header("range"), None);
    assert_eq!(requests[1].header("range"), Some("bytes=1-3"));
    assert_eq!(requests[2].header("range"), None);
    assert_eq!(requests[3].header("range"), Some("bytes=1-3"));
}

#[tokio::test]
async fn delete_is_hard_idempotent_and_uses_a_stable_key() {
    let (base_url, server) = mock_server(vec![
        MockResponse::new("204 No Content", b""),
        MockResponse::new("404 Not Found", b""),
    ])
    .await;
    let store = AeroVaultBlobStore::try_new(bearer_config(base_url)).unwrap();
    let id = BlobId::new();

    store.delete(id).await.unwrap();
    store.delete(id).await.unwrap();

    let requests = requests(server).await;
    for request in requests {
        assert_eq!(request.method, "DELETE");
        assert_eq!(request.target, format!("/v1/files/erp/im/{id}?hard=1"));
        assert_eq!(
            request.header("idempotency-key"),
            Some(format!("aero-im-blob-delete:{id}").as_str())
        );
    }
}

#[tokio::test]
async fn health_uses_unauthenticated_ready_endpoint_and_maps_failure() {
    let (base_url, server) = mock_server(vec![
        MockResponse::new("200 OK", b"ready"),
        MockResponse::new("503 Service Unavailable", b"down"),
    ])
    .await;
    let store = AeroVaultBlobStore::try_new(bearer_config(base_url)).unwrap();

    store.health_check().await.unwrap();
    assert_eq!(
        io_kind(store.health_check().await.unwrap_err()),
        ErrorKind::Other
    );

    let requests = requests(server).await;
    for request in requests {
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/readyz");
        assert_eq!(request.header("authorization"), None);
        assert_eq!(request.header("x-aero-tenant"), None);
    }
}

#[tokio::test]
async fn maps_unauthorized_unsatisfied_range_and_retried_server_error() {
    let responses = vec![
        MockResponse::new("401 Unauthorized", b""),
        MockResponse::new("416 Range Not Satisfiable", b""),
        MockResponse::new("500 Internal Server Error", b""),
        MockResponse::new("502 Bad Gateway", b""),
        MockResponse::new("503 Service Unavailable", b""),
    ];
    let (base_url, server) = mock_server(responses).await;
    let store = AeroVaultBlobStore::try_new(bearer_config(base_url)).unwrap();
    let id = BlobId::new();

    assert_eq!(
        io_kind(store.get(id).await.unwrap_err()),
        ErrorKind::PermissionDenied
    );
    assert_eq!(
        io_kind(
            store
                .get_stream(id, BlobRange::new(10, 20))
                .await
                .err()
                .unwrap()
        ),
        ErrorKind::InvalidInput
    );
    assert_eq!(io_kind(store.get(id).await.unwrap_err()), ErrorKind::Other);

    let requests = requests(server).await;
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[1].header("range"), Some("bytes=10-20"));
}

#[tokio::test]
async fn client_credentials_refreshes_once_after_vault_rejects_cached_token() {
    let token_one = br#"{"access_token":"token-one","token_type":"Bearer","expires_in":3600}"#;
    let token_two = br#"{"access_token":"token-two","token_type":"bearer","expires_in":3600}"#;
    let responses = vec![
        MockResponse::new("200 OK", token_one).header("Content-Type", "application/json"),
        MockResponse::new("401 Unauthorized", b""),
        MockResponse::new("200 OK", token_two).header("Content-Type", "application/json"),
        MockResponse::new("200 OK", b"vault bytes"),
    ];
    let (base_url, server) = mock_server(responses).await;
    let token_endpoint = base_url.join("token").unwrap();
    let store = AeroVaultBlobStore::try_new(AeroVaultConfig {
        base_url: base_url.clone(),
        tenant: "tenant-a".into(),
        prefix: "im-attachments".into(),
        auth: AeroVaultAuth::ClientCredentials {
            token_endpoint,
            client_id: "aero-im".into(),
            client_secret: "secret".into(),
            scope: Some("read write".into()),
            resource: Some("aero-vault".into()),
        },
    })
    .unwrap();
    let id = BlobId::new();

    assert_eq!(
        store.get(id).await.unwrap(),
        Bytes::from_static(b"vault bytes")
    );

    let requests = requests(server).await;
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].target, "/token");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Basic YWVyby1pbTpzZWNyZXQ=")
    );
    assert_eq!(
        std::str::from_utf8(&requests[0].body).unwrap(),
        "grant_type=client_credentials&scope=read+write&resource=aero-vault"
    );
    assert_eq!(
        requests[1].header("authorization"),
        Some("Bearer token-one")
    );
    assert_eq!(requests[2].method, "POST");
    assert_eq!(
        requests[3].header("authorization"),
        Some("Bearer token-two")
    );
}
