//! Audio transcription back-end. Plugs into the AI pipeline so Voice blocks
//! get a `transcript` field after persistence.
//!
//! Two impls:
//! - [`WhisperTranscriber`] — POSTs to `OpenAI`'s `/v1/audio/transcriptions`
//!   (model `whisper-1`) when `OPENAI_API_KEY` is set.
//! - [`StubTranscriber`] — returns a placeholder string when no key is
//!   configured, so dev paths still flow without remote calls.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::{AiError, Result};
use crate::retry::{is_retryable, retry_with_backoff, AttemptError};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "whisper-1";

#[async_trait]
pub trait Transcriber: Send + Sync {
    async fn transcribe(&self, bytes: Bytes, mime: &str) -> Result<String>;
    fn name(&self) -> &'static str;

    fn is_paid_provider(&self) -> bool {
        false
    }
}

pub struct WhisperTranscriber {
    api_key: String,
    base_url: String,
    model: String,
    http: reqwest::Client,
    retry_base: std::time::Duration,
}

impl WhisperTranscriber {
    /// Construct a client with explicit credentials. The endpoint defaults to
    /// `OpenAI`'s v1 API and can be replaced with [`Self::with_base_url`] for a
    /// proxy or a local test server.
    #[must_use]
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: model.into(),
            http,
            retry_base: std::time::Duration::from_secs(1),
        }
    }

    /// Override the API base URL (useful for proxies and deterministic tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_retry_base(mut self, retry_base: std::time::Duration) -> Self {
        self.retry_base = retry_base;
        self
    }

    #[must_use]
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("OPENAI_API_KEY").ok()?;
        let base_url = std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
        let model = std::env::var("WHISPER_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        Some(Self::new(api_key, model).with_base_url(base_url))
    }
}

#[async_trait]
impl Transcriber for WhisperTranscriber {
    async fn transcribe(&self, bytes: Bytes, mime: &str) -> Result<String> {
        let file_name = match mime {
            m if m.contains("webm") => "audio.webm",
            m if m.contains("ogg") => "audio.ogg",
            m if m.contains("wav") => "audio.wav",
            m if m.contains("mp3") => "audio.mp3",
            m if m.contains("mp4") || m.contains("m4a") => "audio.m4a",
            _ => "audio.bin",
        };
        let url = format!(
            "{}/audio/transcriptions",
            self.base_url.trim_end_matches('/')
        );
        retry_with_backoff(3, self.retry_base, || async {
            let part = reqwest::multipart::Part::bytes(bytes.to_vec())
                .file_name(file_name)
                .mime_str(mime)
                .map_err(|error| AttemptError {
                    retryable: false,
                    error: AiError::Internal(format!("multipart mime: {error}")),
                })?;
            let form = reqwest::multipart::Form::new()
                .text("model", self.model.clone())
                .part("file", part);
            let resp = self
                .http
                .post(&url)
                .bearer_auth(&self.api_key)
                .multipart(form)
                .send()
                .await
                .map_err(|error| AttemptError {
                    retryable: true,
                    error: AiError::Http(error.to_string()),
                })?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.map_err(|error| AttemptError {
                    retryable: true,
                    error: AiError::Http(error.to_string()),
                })?;
                return Err(AttemptError {
                    retryable: is_retryable(status),
                    error: AiError::Internal(format!("whisper {}: {body}", status.as_u16())),
                });
            }
            let v: serde_json::Value = resp.json().await.map_err(|error| AttemptError {
                retryable: false,
                error: AiError::Http(error.to_string()),
            })?;
            let text = v
                .get("text")
                .and_then(|x| x.as_str())
                .map(str::to_owned)
                .unwrap_or_default();
            Ok(text)
        })
        .await
    }

    fn name(&self) -> &'static str {
        "whisper-1"
    }

    fn is_paid_provider(&self) -> bool {
        true
    }
}

#[derive(Debug, Default, Clone)]
pub struct StubTranscriber;

#[async_trait]
impl Transcriber for StubTranscriber {
    async fn transcribe(&self, bytes: Bytes, _mime: &str) -> Result<String> {
        Ok(format!("[语音转写未配置 · {} bytes]", bytes.len()))
    }

    fn name(&self) -> &'static str {
        "stub"
    }
}

#[must_use]
pub fn default_transcriber() -> Arc<dyn Transcriber> {
    match WhisperTranscriber::from_env() {
        Some(t) => Arc::new(t),
        None => Arc::new(StubTranscriber),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn whisper_stub(
        statuses: Vec<u16>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4_096];
                loop {
                    let Ok(read) = socket.read(&mut buffer).await else {
                        break;
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let call = observed.fetch_add(1, Ordering::Relaxed);
                let status = statuses.get(call).copied().unwrap_or(500);
                let (status_text, body) = if status == 200 {
                    ("OK", "{\"text\":\"hello\"}".to_owned())
                } else {
                    ("Error", "{\"error\":\"try again\"}".to_owned())
                };
                let response = format!(
                    "HTTP/1.1 {status} {status_text}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{address}"), calls, task)
    }

    #[tokio::test]
    async fn stub_returns_placeholder_with_size() {
        let t = StubTranscriber;
        let s = t
            .transcribe(Bytes::from_static(b"abcd"), "audio/webm")
            .await
            .unwrap();
        assert!(s.contains("4 bytes"));
    }

    #[tokio::test]
    async fn whisper_retries_429_then_succeeds() {
        let (url, calls, server) = whisper_stub(vec![429, 200]).await;
        let transcriber = WhisperTranscriber::new("test-key", "whisper-test")
            .with_base_url(url)
            .with_retry_base(std::time::Duration::from_millis(1));
        let text = transcriber
            .transcribe(Bytes::from_static(b"voice"), "audio/webm")
            .await
            .unwrap();
        assert_eq!(text, "hello");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        server.abort();
    }

    #[tokio::test]
    async fn whisper_retries_three_5xx_attempts_and_preserves_error_prefix() {
        let (url, calls, server) = whisper_stub(vec![500, 500, 500]).await;
        let transcriber = WhisperTranscriber::new("test-key", "whisper-test")
            .with_base_url(url)
            .with_retry_base(std::time::Duration::from_millis(1));
        let error = transcriber
            .transcribe(Bytes::from_static(b"voice"), "audio/webm")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("whisper 500:"),
            "unexpected error: {error}"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        server.abort();
    }

    #[tokio::test]
    async fn whisper_fails_fast_on_non_retryable_4xx() {
        let (url, calls, server) = whisper_stub(vec![400, 200]).await;
        let transcriber = WhisperTranscriber::new("test-key", "whisper-test")
            .with_base_url(url)
            .with_retry_base(std::time::Duration::from_millis(1));
        let error = transcriber
            .transcribe(Bytes::from_static(b"voice"), "audio/webm")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("whisper 400:"),
            "unexpected error: {error}"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        server.abort();
    }
}
