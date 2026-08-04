//! Link unfurling — Open Graph URL previews (Slack-style).
//!
//! Backs `migrations/0024_unfurl_cache.sql`. When a message contains URL(s), a
//! background listener fetches each URL's Open Graph metadata and attaches a
//! `Block::Card { schema: "link_preview", .. }` to the message. This module is
//! the pure, dependency-light core: URL extraction, OG `<meta>` parsing, the
//! preview → card projection, the freshness/TTL decision, and a small
//! Postgres-backed cache so the same URL is not refetched constantly.
//!
//! ## Testable seams (DB-free, unit-tested)
//!
//! Everything except the live HTTP fetch is a pure function exercised offline:
//!
//! * [`extract_urls`] — scan message text for `http(s)://…` URLs (dedup, capped).
//! * [`parse_og`] — hand-rolled `<meta property="og:…">` extractor, tolerant of
//!   missing tags, falling back to `<title>` for the title.
//! * [`preview_to_card`] — project a [`LinkPreview`] into a `link_preview`
//!   [`Block::Card`].
//! * [`is_fresh`] — pure TTL decision for a cached row.
//!
//! The one impure edge — the HTTP GET — hides behind the [`Unfurler`] trait so
//! the fetch can be exercised offline via [`FakeUnfurler`]. [`ReqwestUnfurler`]
//! is the real transport (timeout, response-size cap, `text/html` only). HTML
//! parsing is intentionally hand-rolled (a small string/scan extractor) rather
//! than pulling in a heavyweight HTML-parser dependency.
//!
//! Purely additive: a NEW [`UnfurlRepo`]; no existing repo is touched.

use aero_common::Block;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

// --------------------------------------------------------------- Preview model

/// Schema tag carried by an unfurl `Block::Card` so clients know how to render it.
pub const LINK_PREVIEW_SCHEMA: &str = "link_preview";

/// One resolved link preview — the OG metadata we surface for a URL. Every field
/// but `url` is optional because real-world pages omit tags freely; a preview
/// with only a `url` is still valid (the client can render a bare link card).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LinkPreview {
    /// The URL this preview describes (always present).
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Preview image URL (`og:image`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Human site name (`og:site_name`), e.g. "GitHub".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
}

impl LinkPreview {
    /// A bare preview carrying only the URL (used when a fetch yields nothing
    /// useful but we still want a minimal card / cache entry).
    #[must_use]
    pub fn bare(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Self::default()
        }
    }

    /// Whether this preview carries any metadata beyond the URL itself. A preview
    /// with no `title`/`description`/`image`/`site_name` is not worth attaching
    /// as a card.
    #[must_use]
    pub fn has_metadata(&self) -> bool {
        self.title.is_some()
            || self.description.is_some()
            || self.image.is_some()
            || self.site_name.is_some()
    }
}

// ------------------------------------------------------------- URL extraction

/// Upper bound on how many URLs we unfurl per message — a paste-bomb of links
/// must not fan out into an unbounded number of fetches.
pub const MAX_URLS_PER_MESSAGE: usize = 5;

/// Characters that commonly trail a URL in prose but are not part of it. Trimmed
/// from the end of a scanned token so `(see https://x.com/a).` yields a clean URL.
const TRAILING_PUNCT: &[char] = &['.', ',', ')', ']', '}', '"', '\'', '!', '?', ';', ':', '>'];

/// Scan a message's `Text` blocks for `http(s)://…` URLs, returning them in
/// first-appearance order with duplicates removed and the count capped at
/// [`MAX_URLS_PER_MESSAGE`].
///
/// Pure + total: only `Text` block content is considered (code/file/card blocks
/// are skipped — we do not unfurl links the author typed inside a code block).
/// Trailing prose punctuation is trimmed. This is the front of the pipeline and
/// is unit-tested without any I/O.
#[must_use]
pub fn extract_urls(blocks: &[Block]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for block in blocks {
        let Block::Text { content, .. } = block else {
            continue;
        };
        for token in content.split_whitespace() {
            if let Some(url) = normalize_url(token) {
                if !out.iter().any(|u| u == &url) {
                    out.push(url);
                    if out.len() >= MAX_URLS_PER_MESSAGE {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// Recognize and clean a single whitespace-delimited token as an `http(s)` URL,
/// or `None` if it is not one. Trailing prose punctuation is trimmed and the URL
/// must have a non-empty host after the scheme. Pure helper for [`extract_urls`].
fn normalize_url(token: &str) -> Option<String> {
    // A token may be wrapped in leading punctuation/brackets, e.g. `(https://…`.
    let token = token.trim_start_matches(['(', '[', '{', '<', '"', '\'']);
    let lowered = token.to_ascii_lowercase();
    if !(lowered.starts_with("http://") || lowered.starts_with("https://")) {
        return None;
    }
    let trimmed = token.trim_end_matches(TRAILING_PUNCT);
    // Reject scheme-only tokens like "https://" with no host.
    let after_scheme = trimmed.split_once("://").map_or("", |x| x.1);
    if after_scheme.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

// --------------------------------------------------------------- OG parsing

/// Extract Open Graph metadata from an HTML document into a [`LinkPreview`].
///
/// Hand-rolled (no HTML-parser dependency): scans for `<meta>` tags carrying an
/// OG `property`/`name` and reads their `content`, falling back to the document
/// `<title>` for the title when `og:title` is absent. Tolerant of missing tags,
/// single/double-quoted attributes, attribute order (`content` before
/// `property`), and a handful of HTML entities in values. `url` is the URL the
/// HTML was fetched from and is stored verbatim on the preview.
///
/// Pure + total, so it is unit-tested against sample HTML without any network.
#[must_use]
pub fn parse_og(html: &str, url: &str) -> LinkPreview {
    let mut preview = LinkPreview::bare(url);
    for tag in meta_tags(html) {
        let Some(key) = meta_key(&tag) else { continue };
        let Some(value) = attr_value(&tag, "content").map(|v| decode_entities(&v)) else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        match key.as_str() {
            "og:title" => set_if_empty(&mut preview.title, &value),
            "og:description" | "description" => set_if_empty(&mut preview.description, &value),
            "og:image" | "og:image:url" => set_if_empty(&mut preview.image, &value),
            "og:site_name" => set_if_empty(&mut preview.site_name, &value),
            _ => {}
        }
    }
    // Fallback to <title> if og:title was absent.
    if preview.title.is_none() {
        if let Some(t) = title_tag(html) {
            let t = decode_entities(&t);
            if !t.trim().is_empty() {
                preview.title = Some(t.trim().to_string());
            }
        }
    }
    preview
}

/// Set `slot` to `value` only when it is currently unset, so the FIRST occurrence
/// of a given OG property wins (matching how scrapers treat duplicate tags).
fn set_if_empty(slot: &mut Option<String>, value: &str) {
    if slot.is_none() {
        *slot = Some(value.trim().to_string());
    }
}

/// Yield the raw text of every `<meta …>` tag in the document (the substring
/// between `<meta` and the next `>`), lower-bounded so a malformed unterminated
/// tag at EOF is ignored rather than scanned forever.
fn meta_tags(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut tags = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find("<meta") {
        let start = from + rel;
        // Find the end of this tag.
        if let Some(end_rel) = html[start..].find('>') {
            let end = start + end_rel;
            tags.push(html[start..end].to_string());
            from = end + 1;
        } else {
            break;
        }
    }
    tags
}

/// The OG/standard key a `<meta>` tag declares, via its `property` (preferred) or
/// `name` attribute, lowercased. `None` when neither is present.
fn meta_key(tag: &str) -> Option<String> {
    attr_value(tag, "property")
        .or_else(|| attr_value(tag, "name"))
        .map(|v| v.trim().to_ascii_lowercase())
}

/// Read the value of attribute `name` from a tag's text, supporting single- or
/// double-quoted values. Case-insensitive on the attribute name. Returns the raw
/// (still entity-encoded) value. Pure string scan — no HTML parser.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let mut from = 0usize;
    loop {
        let rel = lower[from..].find(&needle)?;
        let at = from + rel;
        // Ensure the char before `name=` is a boundary (start or whitespace), so
        // `property=` is not matched inside e.g. `data-property=`.
        let boundary = at == 0
            || tag[..at]
                .chars()
                .next_back()
                .map_or(true, char::is_whitespace);
        if !boundary {
            from = at + needle.len();
            continue;
        }
        let rest = &tag[at + needle.len()..];
        let mut chars = rest.chars();
        let quote = chars.next()?;
        if quote != '"' && quote != '\'' {
            from = at + needle.len();
            continue;
        }
        let value_start = at + needle.len() + quote.len_utf8();
        let value_rest = &tag[value_start..];
        let end = value_rest.find(quote)?;
        return Some(value_rest[..end].to_string());
    }
}

/// Document `<title>…</title>` text, if present (first occurrence). Case-insensitive
/// on the tag name.
fn title_tag(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open_rel = lower.find("<title")?;
    let gt_rel = lower[open_rel..].find('>')?;
    let text_start = open_rel + gt_rel + 1;
    let close_rel = lower[text_start..].find("</title>")?;
    Some(html[text_start..text_start + close_rel].to_string())
}

/// Decode the small set of HTML entities that commonly appear in OG `content`
/// values. Deliberately minimal (named ampersand/quote/angle entities plus
/// `&#NN;`/`&#xNN;` numerics) — enough to clean real titles without a full HTML
/// entity table.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(semi_rel) = s[i..].find(';') {
                let entity = &s[i + 1..i + semi_rel];
                if let Some(decoded) = decode_one_entity(entity) {
                    out.push(decoded);
                    i += semi_rel + 1;
                    continue;
                }
            }
        }
        // Not an entity we recognize — copy this char through verbatim.
        let ch = s[i..].chars().next().unwrap_or('&');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Decode a single entity body (the text between `&` and `;`), or `None` if it is
/// not one we handle.
fn decode_one_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some('\u{a0}'),
        _ => {
            let num = entity.strip_prefix('#')?;
            let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                num.parse::<u32>().ok()?
            };
            char::from_u32(code)
        }
    }
}

// --------------------------------------------------------------- Card building

/// Project a [`LinkPreview`] into a `link_preview` [`Block::Card`] for inlining
/// into a message. The payload carries the URL plus whatever OG fields resolved
/// (`title`/`description`/`image`/`site_name`), each omitted when absent so the
/// card is compact. Pure, so the wire shape is unit-tested.
#[must_use]
pub fn preview_to_card(preview: &LinkPreview) -> Block {
    // `LinkPreview`'s Serialize already drops `None` fields, so serializing it is
    // exactly the compact `{url, title?, description?, image?, site_name?}` payload.
    let payload =
        serde_json::to_value(preview).unwrap_or_else(|_| serde_json::json!({ "url": preview.url }));
    Block::Card {
        schema: LINK_PREVIEW_SCHEMA.to_string(),
        payload,
    }
}

/// Whether a `Block::Card` already carries a link preview — used by the listener
/// to skip messages it has already unfurled (loop guard: editing a message
/// re-publishes it, which would otherwise re-trigger the unfurler).
#[must_use]
pub fn is_link_preview_card(block: &Block) -> bool {
    matches!(block, Block::Card { schema, .. } if schema == LINK_PREVIEW_SCHEMA)
}

// --------------------------------------------------------------- The HTTP seam

/// The injectable HTTP seam: fetch a URL and return its HTML body, or `None` when
/// the page can't be fetched/isn't HTML (best-effort — the caller logs and skips).
/// The real impl is [`ReqwestUnfurler`]; tests use [`FakeUnfurler`].
#[async_trait::async_trait]
pub trait Unfurler: Send + Sync {
    /// Fetch `url`, returning the decoded HTML body. `None` on any failure
    /// (transport error, non-2xx, non-HTML content type, oversized body, …) so an
    /// unreachable or hostile URL degrades to "no preview" rather than an error.
    async fn fetch(&self, url: &str) -> Option<String>;
}

/// Default per-request timeout for the real unfurler — a slow page must not stall
/// the listener.
const FETCH_TIMEOUT_SECS: u64 = 8;

/// Cap on how many bytes of a response body we read before giving up. OG tags
/// live in `<head>`, so a sane cap keeps a hostile/huge page from exhausting
/// memory while still capturing the metadata.
const MAX_BODY_BYTES: usize = 512 * 1024;

/// Real HTTP transport over `reqwest`: GET the URL with a timeout, accept only
/// `text/html`(-ish) responses, and read at most [`MAX_BODY_BYTES`] of the body.
#[derive(Clone)]
pub struct ReqwestUnfurler {
    client: reqwest::Client,
    max_bytes: usize,
}

impl ReqwestUnfurler {
    /// Build an unfurler with a sane timeout + body cap.
    #[must_use]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
            // Identify ourselves; some sites serve OG tags only to known agents.
            .user_agent("aero-im-unfurl/1.0 (+https://github.com/aero-im)")
            .build()
            .unwrap_or_default();
        Self {
            client,
            max_bytes: MAX_BODY_BYTES,
        }
    }
}

impl Default for ReqwestUnfurler {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a `Content-Type` header value denotes an HTML document we should parse
/// for OG tags. Pure so the content-type gate is unit-testable without a network.
#[must_use]
pub fn is_html_content_type(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    let main = ct.split(';').next().unwrap_or("").trim();
    main == "text/html" || main == "application/xhtml+xml"
}

#[async_trait::async_trait]
impl Unfurler for ReqwestUnfurler {
    async fn fetch(&self, url: &str) -> Option<String> {
        let resp = self.client.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        // Only parse HTML — skip images, PDFs, JSON, etc.
        let is_html = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map_or(true, is_html_content_type);
        if !is_html {
            return None;
        }
        let body = resp.bytes().await.ok()?;
        let capped = &body[..body.len().min(self.max_bytes)];
        Some(String::from_utf8_lossy(capped).into_owned())
    }
}

/// Test double: returns a canned HTML body for any URL (or `None` to simulate a
/// failed fetch). Lets the fetch → `parse_og` → card path be exercised offline.
#[derive(Clone, Default)]
pub struct FakeUnfurler {
    html: Option<String>,
}

impl FakeUnfurler {
    /// An unfurler that returns `html` for every URL.
    #[must_use]
    pub fn with_html(html: impl Into<String>) -> Self {
        Self {
            html: Some(html.into()),
        }
    }

    /// An unfurler that always fails to fetch (returns `None`).
    #[must_use]
    pub fn failing() -> Self {
        Self { html: None }
    }
}

#[async_trait::async_trait]
impl Unfurler for FakeUnfurler {
    async fn fetch(&self, _url: &str) -> Option<String> {
        self.html.clone()
    }
}

// --------------------------------------------------------------- Cache + TTL

/// How long a cached preview is considered fresh before a re-fetch is allowed.
pub const CACHE_TTL: time::Duration = time::Duration::hours(24);

/// SHA-256 hex of a URL — the cache primary key, so the table is keyed on a
/// fixed-width hash rather than an unbounded URL string. Deterministic + pure.
#[must_use]
pub fn url_hash(url: &str) -> String {
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    hex::encode(h.finalize())
}

/// Whether a cache row fetched at `fetched_at` is still fresh as of `now` (i.e.
/// younger than [`CACHE_TTL`]). A row from the future (clock skew) is treated as
/// fresh. Pure decision function so the TTL policy is unit-tested without a DB.
#[must_use]
pub fn is_fresh(fetched_at: time::OffsetDateTime, now: time::OffsetDateTime) -> bool {
    let age = now - fetched_at;
    age < CACHE_TTL
}

/// Postgres-backed unfurl cache (`migrations/0024_unfurl_cache.sql`). Stores one
/// row per URL (keyed by [`url_hash`]) carrying the resolved [`LinkPreview`] as
/// JSONB and the fetch time, so a hot URL is parsed once per [`CACHE_TTL`].
#[derive(Clone)]
pub struct UnfurlRepo {
    pool: PgPool,
}

impl UnfurlRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Look up a still-fresh cached preview for `url`. Returns `None` when there
    /// is no row OR the row is older than [`CACHE_TTL`] (a stale row is ignored so
    /// the caller re-fetches). Freshness is evaluated against the current UTC time.
    pub async fn get(&self, url: &str) -> Result<Option<LinkPreview>, sqlx::Error> {
        let hash = url_hash(url);
        let row = sqlx::query_as::<_, (serde_json::Value, time::OffsetDateTime)>(
            r"SELECT preview, fetched_at
               FROM unfurl_cache
               WHERE url_hash = $1",
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?;
        let Some((preview, fetched_at)) = row else {
            return Ok(None);
        };
        if !is_fresh(fetched_at, time::OffsetDateTime::now_utc()) {
            return Ok(None);
        }
        // A malformed stored blob degrades to a cache miss rather than an error.
        Ok(serde_json::from_value(preview).ok())
    }

    /// Upsert a freshly-resolved preview for `url`, restamping `fetched_at` so the
    /// TTL window restarts. Keyed on [`url_hash`]; the URL is stored alongside for
    /// debuggability.
    pub async fn put(&self, url: &str, preview: &LinkPreview) -> Result<(), sqlx::Error> {
        let hash = url_hash(url);
        let json = serde_json::to_value(preview).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        sqlx::query(
            r"INSERT INTO unfurl_cache (url_hash, url, preview, fetched_at)
               VALUES ($1, $2, $3, now())
               ON CONFLICT (url_hash)
               DO UPDATE SET url = EXCLUDED.url,
                             preview = EXCLUDED.preview,
                             fetched_at = now()",
        )
        .bind(&hash)
        .bind(url)
        .bind(sqlx::types::Json(json))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

// ------------------------------------------------------------------ Unit tests

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::Block;

    fn text(s: &str) -> Block {
        Block::text(s)
    }

    // ---- extract_urls ----

    #[test]
    fn extract_urls_finds_http_and_https() {
        let blocks = [text("see http://a.com and https://b.com/page")];
        assert_eq!(
            extract_urls(&blocks),
            vec!["http://a.com".to_string(), "https://b.com/page".to_string()]
        );
    }

    #[test]
    fn extract_urls_dedups_preserving_first_order() {
        let blocks = [text(
            "https://x.com then https://y.com then https://x.com again",
        )];
        assert_eq!(
            extract_urls(&blocks),
            vec!["https://x.com".to_string(), "https://y.com".to_string()]
        );
    }

    #[test]
    fn extract_urls_trims_trailing_punctuation_and_brackets() {
        let blocks = [text("(see https://example.com/path).")];
        assert_eq!(
            extract_urls(&blocks),
            vec!["https://example.com/path".to_string()]
        );
        let wrapped = [text("<https://example.com>")];
        assert_eq!(
            extract_urls(&wrapped),
            vec!["https://example.com".to_string()]
        );
    }

    #[test]
    fn extract_urls_ignores_non_text_blocks_and_non_urls() {
        let blocks = [
            Block::Code {
                lang: "rs".into(),
                content: "let u = \"https://incode.com\";".into(),
            },
            text("no links here, just ftp://nope.com and bareword.com"),
        ];
        assert!(extract_urls(&blocks).is_empty());
    }

    #[test]
    fn extract_urls_rejects_scheme_only_token() {
        let blocks = [text("broken https:// and http://")];
        assert!(extract_urls(&blocks).is_empty());
    }

    #[test]
    fn extract_urls_caps_at_max() {
        let many = (0..20)
            .map(|i| format!("https://h{i}.com"))
            .collect::<Vec<_>>()
            .join(" ");
        let blocks = [text(&many)];
        assert_eq!(extract_urls(&blocks).len(), MAX_URLS_PER_MESSAGE);
    }

    #[test]
    fn extract_urls_is_case_insensitive_on_scheme() {
        let blocks = [text("HTTPS://Example.COM/Path")];
        assert_eq!(
            extract_urls(&blocks),
            vec!["HTTPS://Example.COM/Path".to_string()]
        );
    }

    // ---- parse_og ----

    const SAMPLE: &str = r#"
        <html><head>
          <title>Fallback &amp; Title</title>
          <meta property="og:title" content="OG Title">
          <meta property="og:description" content="A great &quot;page&quot;">
          <meta property="og:image" content="https://cdn.example.com/img.png">
          <meta property="og:site_name" content="Example">
        </head><body>hi</body></html>
    "#;

    #[test]
    fn parse_og_reads_all_fields_and_decodes_entities() {
        let p = parse_og(SAMPLE, "https://example.com/x");
        assert_eq!(p.url, "https://example.com/x");
        assert_eq!(p.title.as_deref(), Some("OG Title"));
        assert_eq!(p.description.as_deref(), Some("A great \"page\""));
        assert_eq!(p.image.as_deref(), Some("https://cdn.example.com/img.png"));
        assert_eq!(p.site_name.as_deref(), Some("Example"));
    }

    #[test]
    fn parse_og_falls_back_to_title_tag() {
        let html = r#"<html><head><title>Just A Title</title>
            <meta property="og:description" content="desc only"></head></html>"#;
        let p = parse_og(html, "https://example.com");
        assert_eq!(p.title.as_deref(), Some("Just A Title"));
        assert_eq!(p.description.as_deref(), Some("desc only"));
        assert!(p.image.is_none());
        assert!(p.site_name.is_none());
    }

    #[test]
    fn parse_og_tolerates_missing_tags() {
        let p = parse_og(
            "<html><head></head><body>nothing</body></html>",
            "https://x.com",
        );
        assert_eq!(p.url, "https://x.com");
        assert!(p.title.is_none());
        assert!(!p.has_metadata());
    }

    #[test]
    fn parse_og_handles_single_quotes_and_attr_order() {
        // content before property, single-quoted values.
        let html = "<meta content='Reversed' property='og:title'>";
        let p = parse_og(html, "https://x.com");
        assert_eq!(p.title.as_deref(), Some("Reversed"));
    }

    #[test]
    fn parse_og_uses_name_meta_for_description() {
        let html = r#"<meta name="description" content="from name attr">"#;
        let p = parse_og(html, "https://x.com");
        assert_eq!(p.description.as_deref(), Some("from name attr"));
    }

    #[test]
    fn parse_og_first_occurrence_wins() {
        let html = concat!(
            r#"<meta property="og:title" content="First">"#,
            r#"<meta property="og:title" content="Second">"#,
        );
        let p = parse_og(html, "https://x.com");
        assert_eq!(p.title.as_deref(), Some("First"));
    }

    #[test]
    fn parse_og_decodes_numeric_entities() {
        let html = r#"<meta property="og:title" content="A&#38;B &#x2764; C">"#;
        let p = parse_og(html, "https://x.com");
        assert_eq!(p.title.as_deref(), Some("A&B \u{2764} C"));
    }

    #[test]
    fn parse_og_ignores_empty_content() {
        let html = r#"<meta property="og:title" content="">
            <title>Real Title</title>"#;
        let p = parse_og(html, "https://x.com");
        // Empty og:title is skipped, so the <title> fallback fills in.
        assert_eq!(p.title.as_deref(), Some("Real Title"));
    }

    // ---- preview_to_card ----

    #[test]
    fn preview_to_card_builds_link_preview_card() {
        let preview = LinkPreview {
            url: "https://example.com".into(),
            title: Some("T".into()),
            description: Some("D".into()),
            image: Some("https://img".into()),
            site_name: Some("Example".into()),
        };
        let card = preview_to_card(&preview);
        match card {
            Block::Card { schema, payload } => {
                assert_eq!(schema, LINK_PREVIEW_SCHEMA);
                assert_eq!(payload["url"], "https://example.com");
                assert_eq!(payload["title"], "T");
                assert_eq!(payload["description"], "D");
                assert_eq!(payload["image"], "https://img");
                assert_eq!(payload["site_name"], "Example");
            }
            _ => panic!("expected a Card block"),
        }
    }

    #[test]
    fn preview_to_card_omits_absent_fields() {
        let preview = LinkPreview::bare("https://example.com");
        let card = preview_to_card(&preview);
        match card {
            Block::Card { schema, payload } => {
                assert_eq!(schema, LINK_PREVIEW_SCHEMA);
                assert_eq!(payload["url"], "https://example.com");
                let obj = payload.as_object().expect("payload is an object");
                // Only `url` survives — every None field is dropped.
                assert_eq!(obj.len(), 1, "absent fields omitted from payload");
            }
            _ => panic!("expected a Card block"),
        }
    }

    #[test]
    fn is_link_preview_card_detects_only_link_previews() {
        let lp = preview_to_card(&LinkPreview::bare("https://x.com"));
        assert!(is_link_preview_card(&lp));
        let other = Block::Card {
            schema: "citation".into(),
            payload: serde_json::json!({}),
        };
        assert!(!is_link_preview_card(&other));
        assert!(!is_link_preview_card(&Block::text("hi")));
    }

    #[test]
    fn card_roundtrips_through_json() {
        // The card must serialize as a normal Block so it can be stored + sent.
        let card = preview_to_card(&LinkPreview {
            url: "https://x.com".into(),
            title: Some("Hi".into()),
            ..LinkPreview::default()
        });
        let j = serde_json::to_string(&card).unwrap();
        assert!(j.contains("\"type\":\"card\""));
        assert!(j.contains("\"schema\":\"link_preview\""));
        let back: Block = serde_json::from_str(&j).unwrap();
        assert!(is_link_preview_card(&back));
    }

    // ---- has_metadata ----

    #[test]
    fn has_metadata_distinguishes_bare_from_rich() {
        assert!(!LinkPreview::bare("https://x.com").has_metadata());
        let mut p = LinkPreview::bare("https://x.com");
        p.title = Some("t".into());
        assert!(p.has_metadata());
    }

    // ---- content-type gate ----

    #[test]
    fn is_html_content_type_accepts_html_rejects_others() {
        assert!(is_html_content_type("text/html"));
        assert!(is_html_content_type("text/html; charset=utf-8"));
        assert!(is_html_content_type("TEXT/HTML"));
        assert!(is_html_content_type("application/xhtml+xml"));
        assert!(!is_html_content_type("application/json"));
        assert!(!is_html_content_type("image/png"));
        assert!(!is_html_content_type("text/plain"));
    }

    // ---- url_hash ----

    #[test]
    fn url_hash_is_deterministic_and_distinct() {
        assert_eq!(url_hash("https://a.com"), url_hash("https://a.com"));
        assert_ne!(url_hash("https://a.com"), url_hash("https://b.com"));
        // SHA-256 hex is 64 chars.
        assert_eq!(url_hash("https://a.com").len(), 64);
    }

    // ---- is_fresh / TTL ----

    #[test]
    fn is_fresh_within_ttl_and_stale_after() {
        let now = time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(100);
        // Just under 24h old => fresh.
        assert!(is_fresh(now - time::Duration::hours(23), now));
        // Exactly at/over the TTL => stale.
        assert!(!is_fresh(now - CACHE_TTL, now));
        assert!(!is_fresh(now - time::Duration::hours(25), now));
        // A timestamp in the future (clock skew) is treated as fresh.
        assert!(is_fresh(now + time::Duration::hours(1), now));
    }

    // ---- the fake seam ----

    #[test]
    fn fake_unfurler_returns_canned_html_or_none() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let ok = FakeUnfurler::with_html("<title>X</title>");
            assert_eq!(
                ok.fetch("https://x.com").await.as_deref(),
                Some("<title>X</title>")
            );
            let fail = FakeUnfurler::failing();
            assert!(fail.fetch("https://x.com").await.is_none());
        });
    }

    #[test]
    fn end_to_end_fake_fetch_parse_card() {
        // Exercise the full offline pipeline: extract -> fetch (fake) -> parse -> card.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let blocks = [text("check https://example.com/post out")];
            let urls = extract_urls(&blocks);
            assert_eq!(urls, vec!["https://example.com/post".to_string()]);

            let fetcher = FakeUnfurler::with_html(SAMPLE);
            let html = fetcher
                .fetch(&urls[0])
                .await
                .expect("fake always returns html");
            let preview = parse_og(&html, &urls[0]);
            assert!(preview.has_metadata());
            let card = preview_to_card(&preview);
            assert!(is_link_preview_card(&card));
        });
    }
}

/// PG-gated integration test (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored unfurl_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn unfurl_cache_put_then_get_roundtrip() {
        let repo = UnfurlRepo::new(pool());
        let url = format!("https://example.com/{}", url_hash("seed")); // unique-ish
        let preview = LinkPreview {
            url: url.clone(),
            title: Some("Cached Title".into()),
            description: Some("Cached Desc".into()),
            image: Some("https://cdn/img.png".into()),
            site_name: Some("Example".into()),
        };

        repo.put(&url, &preview).await.unwrap();
        let got = repo.get(&url).await.unwrap().expect("fresh row returned");
        assert_eq!(got, preview, "put -> get roundtrips the full preview");

        // Upsert with new metadata overwrites in place (same url_hash).
        let updated = LinkPreview {
            title: Some("New Title".into()),
            ..preview.clone()
        };
        repo.put(&url, &updated).await.unwrap();
        let got2 = repo.get(&url).await.unwrap().expect("still fresh");
        assert_eq!(got2.title.as_deref(), Some("New Title"));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn unfurl_cache_miss_for_unknown_url() {
        let repo = UnfurlRepo::new(pool());
        let url = format!("https://never-cached.example/{}", url_hash("missing"));
        assert!(repo.get(&url).await.unwrap().is_none());
    }
}
