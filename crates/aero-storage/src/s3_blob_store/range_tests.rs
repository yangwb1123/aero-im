use aero_common::BlobId;
use futures::TryStreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{BlobRange, BlobStore, S3BlobStore, S3Config};

#[tokio::test]
async fn ranged_stream_forwards_range_header_to_object_store() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        socket
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\n\
                  Content-Length: 3\r\n\
                  Content-Range: bytes 1-3/6\r\n\
                  Connection: close\r\n\r\nbcd",
            )
            .await
            .unwrap();
        String::from_utf8(request).unwrap()
    });

    let store = S3BlobStore::new(S3Config {
        bucket: "test-bucket".into(),
        region: "us-east-1".into(),
        endpoint: Some(format!("http://{address}")),
        access_key: "test-access".into(),
        secret_key: "test-secret".into(),
        kms_key_id: None,
    });
    let body = store
        .get_stream(BlobId::new(), BlobRange::new(1, 3))
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let body: Vec<u8> = body.into_iter().flat_map(|chunk| chunk.to_vec()).collect();
    assert_eq!(&body[..], b"bcd");

    let request = server.await.unwrap().to_ascii_lowercase();
    assert!(
        request.contains("\r\nrange: bytes=1-3\r\n"),
        "request must forward the exact resolved range:\n{request}"
    );
}

#[tokio::test]
async fn kms_put_sends_both_encryption_headers_and_signs_them() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        String::from_utf8(request).unwrap()
    });

    let kms_key_id = "arn:aws:kms:us-east-1:123456789012:key/1234abcd";
    let store = S3BlobStore::new(S3Config {
        bucket: "test-bucket".into(),
        region: "us-east-1".into(),
        endpoint: Some(format!("http://{address}")),
        access_key: "test-access".into(),
        secret_key: "test-secret".into(),
        kms_key_id: Some(kms_key_id.into()),
    });
    store
        .put(BlobId::new(), bytes::Bytes::from_static(b"encrypted"))
        .await
        .unwrap();

    let request = server.await.unwrap().to_ascii_lowercase();
    assert!(
        request.contains("\r\nx-amz-server-side-encryption: aws:kms\r\n"),
        "PUT must request SSE-KMS:\n{request}"
    );
    assert!(
        request.contains(&format!(
            "\r\nx-amz-server-side-encryption-aws-kms-key-id: {kms_key_id}\r\n"
        )),
        "PUT must carry the configured KMS key id:\n{request}"
    );
    assert!(
        request.contains(
            "signedheaders=host;x-amz-content-sha256;x-amz-date;\
             x-amz-server-side-encryption;\
             x-amz-server-side-encryption-aws-kms-key-id"
        ),
        "every emitted x-amz encryption header must be signed:\n{request}"
    );
}
