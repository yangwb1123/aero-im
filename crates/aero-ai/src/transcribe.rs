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
}

impl WhisperTranscriber {
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("OPENAI_API_KEY").ok()?;
        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let model = std::env::var("WHISPER_MODEL").unwrap_or_else(|_| "whisper-1".into());
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .ok()?;
        Some(Self {
            api_key,
            base_url,
            model,
            http,
        })
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
        let part = reqwest::multipart::Part::bytes(bytes.to_vec())
            .file_name(file_name)
            .mime_str(mime)
            .map_err(|e| AiError::Internal(format!("multipart mime: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("model", self.model.clone())
            .part("file", part);
        let resp = self
            .http
            .post(format!("{}/audio/transcriptions", self.base_url))
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AiError::Internal(format!("whisper {status}: {body}")));
        }
        let v: serde_json::Value = resp.json().await?;
        let text = v
            .get("text")
            .and_then(|x| x.as_str())
            .map(str::to_owned)
            .unwrap_or_default();
        Ok(text)
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

    #[tokio::test]
    async fn stub_returns_placeholder_with_size() {
        let t = StubTranscriber;
        let s = t
            .transcribe(Bytes::from_static(b"abcd"), "audio/webm")
            .await
            .unwrap();
        assert!(s.contains("4 bytes"));
    }
}
