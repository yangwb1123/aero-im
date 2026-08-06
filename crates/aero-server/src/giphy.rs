//! Server-side GIPHY search used by the `/giphy` slash command.
//!
//! The API key never reaches the browser or persisted messages. Provider
//! responses are reduced to a small card and every remote URL is constrained to
//! HTTPS GIPHY origins before storage; the web client repeats the same check
//! before creating an image element.

use std::{sync::OnceLock, time::Duration};

use aero_common::{Block, Error, ParticipantId};
use aero_storage::{ws_rate::epoch_minute, WsRateStore};
use futures::StreamExt as _;
use serde::Deserialize;
use url::Url;

use crate::rate_limit::{ClientKey, RateLimiter};

const SEARCH_ENDPOINT: &str = "https://api.giphy.com/v1/gifs/search";
const MAX_QUERY_CHARS: usize = 50;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_CALLS_PER_MINUTE: u32 = 10;
const DEFAULT_BURST: u32 = 3;
static CALLER_LIMITER: OnceLock<RateLimiter> = OnceLock::new();

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    data: Vec<Gif>,
}

#[derive(Debug, Deserialize)]
struct Gif {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    images: Images,
}

#[derive(Debug, Deserialize)]
struct Images {
    fixed_height: Option<Rendition>,
    fixed_width: Option<Rendition>,
    downsized: Option<Rendition>,
    original: Option<Rendition>,
}

#[derive(Debug, Deserialize)]
struct Rendition {
    url: String,
    #[serde(default)]
    width: String,
    #[serde(default)]
    height: String,
}

/// Whether an operator supplied a usable integration key.
pub(crate) fn configured() -> bool {
    std::env::var("AERO_GIPHY_API_KEY")
        .map(|key| !key.trim().is_empty())
        .unwrap_or(false)
}

/// Consume one provider-call token for this participant.
///
/// The command route also spends the cluster-wide workspace send budget before
/// reaching this limiter. A local token bucket controls bursts while a Redis
/// fixed window prevents one authenticated caller multiplying provider quota
/// across gateway nodes.
pub(crate) async fn check_caller_rate(
    state: &crate::state::AppState,
    caller: ParticipantId,
) -> Result<(), Error> {
    let calls_per_minute = env_u32("AERO_GIPHY_RATE_PER_MIN", DEFAULT_CALLS_PER_MINUTE, 1, 600);
    let limiter = CALLER_LIMITER.get_or_init(|| {
        let burst = env_u32("AERO_GIPHY_RATE_BURST", DEFAULT_BURST, 1, 100);
        RateLimiter::with_rate(f64::from(calls_per_minute) / 60.0, f64::from(burst))
    });
    if !limiter.check(ClientKey::Participant(caller)) {
        return Err(Error::RateLimited);
    }

    let key = caller_window_key(
        caller,
        epoch_minute(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs()),
        ),
    );
    match WsRateStore::new(state.redis_client.clone())
        .incr_raw(key)
        .await
    {
        Ok(count) if count > u64::from(calls_per_minute) => Err(Error::RateLimited),
        Ok(_) => Ok(()),
        Err(error) => {
            tracing::warn!(?error, %caller, "GIPHY caller rate check failed open");
            Ok(())
        }
    }
}

fn caller_window_key(caller: ParticipantId, minute: u64) -> String {
    format!("aero:giphy:{caller}:{minute}")
}

/// Resolve a query to one durable, safely renderable GIPHY card.
pub(crate) async fn search_card(query: &str) -> Result<Block, Error> {
    let query = validate_query(query)?;
    let api_key = std::env::var("AERO_GIPHY_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| Error::Upstream("GIPHY integration is not configured".into()))?;
    let rating = configured_rating();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Error::Upstream("GIPHY client could not be initialized".into()))?;
    let response = client
        .get(SEARCH_ENDPOINT)
        .query(&[
            ("api_key", api_key.as_str()),
            ("q", query),
            ("limit", "1"),
            ("offset", "0"),
            ("rating", rating),
            ("bundle", "messaging_non_clips"),
        ])
        .send()
        .await
        // Do not expose a reqwest error containing the credential-bearing URL.
        .map_err(|_| Error::Upstream("GIPHY request failed".into()))?;
    if !response.status().is_success() {
        return Err(Error::Upstream(format!(
            "GIPHY returned HTTP {}",
            response.status().as_u16()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(Error::Upstream("GIPHY response was too large".into()));
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Error::Upstream("GIPHY response failed".into()))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(Error::Upstream("GIPHY response was too large".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    let response: SearchResponse = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Upstream("GIPHY returned an invalid response".into()))?;
    card_from_response(query, response)
}

fn env_u32(name: &str, default: u32, min: u32, max: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn validate_query(query: &str) -> Result<&str, Error> {
    let query = query.trim();
    if query.is_empty() {
        return Err(Error::Invalid("usage: /giphy <query>".into()));
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(Error::Invalid(format!(
            "GIPHY query must be at most {MAX_QUERY_CHARS} characters"
        )));
    }
    Ok(query)
}

fn configured_rating() -> &'static str {
    match std::env::var("AERO_GIPHY_RATING")
        .unwrap_or_else(|_| "pg".to_owned())
        .to_ascii_lowercase()
        .as_str()
    {
        "g" => "g",
        "pg-13" => "pg-13",
        // Fail toward the safer default for invalid or more permissive values.
        _ => "pg",
    }
}

fn card_from_response(query: &str, response: SearchResponse) -> Result<Block, Error> {
    let gif = response
        .data
        .into_iter()
        .next()
        .ok_or_else(|| Error::NotFound(format!("no GIPHY result for {query:?}")))?;
    let rendition = [
        gif.images.fixed_height,
        gif.images.fixed_width,
        gif.images.downsized,
        gif.images.original,
    ]
    .into_iter()
    .flatten()
    .find(|rendition| safe_media_url(&rendition.url))
    .ok_or_else(|| Error::Upstream("GIPHY returned no safe GIF rendition".into()))?;

    let title = if gif.title.trim().is_empty() {
        query.to_owned()
    } else {
        gif.title.trim().chars().take(200).collect()
    };
    let source_url = safe_page_url(&gif.url).then_some(gif.url);
    let width = parse_dimension(&rendition.width);
    let height = parse_dimension(&rendition.height);
    Ok(Block::Card {
        schema: "giphy".into(),
        payload: serde_json::json!({
            "id": gif.id,
            "title": title,
            "query": query,
            "image_url": rendition.url,
            "source_url": source_url,
            "width": width,
            "height": height,
            "provider": "giphy",
            "attribution": "Powered by GIPHY",
        }),
    })
}

fn parse_dimension(raw: &str) -> Option<u32> {
    raw.parse::<u32>()
        .ok()
        .filter(|value| (1..=10_000).contains(value))
}

fn safe_media_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let path = url.path().to_ascii_lowercase();
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && (host.eq_ignore_ascii_case("giphy.com")
            || host.to_ascii_lowercase().ends_with(".giphy.com"))
        && (std::path::Path::new(&path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("gif"))
            || std::path::Path::new(&path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("webp")))
}

fn safe_page_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && (host.eq_ignore_ascii_case("giphy.com")
            || host.to_ascii_lowercase().ends_with(".giphy.com"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(image_url: &str, source_url: &str) -> SearchResponse {
        SearchResponse {
            data: vec![Gif {
                id: "gif-1".into(),
                title: "Dancing cat".into(),
                url: source_url.into(),
                images: Images {
                    fixed_height: Some(Rendition {
                        url: image_url.into(),
                        width: "320".into(),
                        height: "200".into(),
                    }),
                    fixed_width: None,
                    downsized: None,
                    original: None,
                },
            }],
        }
    }

    #[test]
    fn valid_response_becomes_attributed_replayable_card() {
        let block = card_from_response(
            "dancing cat",
            response(
                "https://media2.giphy.com/media/abc/200.gif?cid=aero",
                "https://giphy.com/gifs/dancing-cat-abc",
            ),
        )
        .unwrap();
        let Block::Card { schema, payload } = block else {
            panic!("expected card");
        };
        assert_eq!(schema, "giphy");
        assert_eq!(
            payload["image_url"],
            "https://media2.giphy.com/media/abc/200.gif?cid=aero"
        );
        assert_eq!(
            payload["source_url"],
            "https://giphy.com/gifs/dancing-cat-abc"
        );
        assert_eq!(payload["attribution"], "Powered by GIPHY");
        assert_eq!(payload["width"], 320);
        assert_eq!(payload["height"], 200);
    }

    #[test]
    fn unsafe_media_origins_and_formats_are_rejected() {
        for url in [
            "http://media.giphy.com/media/a.gif",
            "https://giphy.com.evil.test/a.gif",
            "https://user:pass@media.giphy.com/a.gif",
            "https://media.giphy.com/a.svg",
            "javascript:alert(1)",
        ] {
            assert!(
                card_from_response("cat", response(url, "https://giphy.com/gifs/cat")).is_err(),
                "{url} must not become a persisted image"
            );
        }
    }

    #[test]
    fn query_and_empty_result_fail_before_a_placeholder_can_be_persisted() {
        assert!(validate_query("").is_err());
        assert!(validate_query(&"x".repeat(MAX_QUERY_CHARS + 1)).is_err());
        assert!(card_from_response("missing", SearchResponse { data: vec![] }).is_err());
    }

    #[test]
    fn caller_window_key_is_participant_and_window_scoped() {
        let caller = ParticipantId::new();
        assert_ne!(caller_window_key(caller, 10), caller_window_key(caller, 11));
        assert_ne!(
            caller_window_key(caller, 10),
            caller_window_key(ParticipantId::new(), 10)
        );
    }
}
