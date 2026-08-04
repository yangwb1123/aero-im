//! Webhook persistence + pure signing/delivery logic (integration plane).
//!
//! Backs `migrations/0013_webhooks.sql`. Two directions:
//!
//! * **Incoming** — an external system holds a bearer *token* and POSTs to
//!   `/hooks/in/:token`; the server posts the body into a room as a dedicated
//!   bot. Only the SHA-256 *hash* of the token is stored ([`hash_token`]); the
//!   plaintext is generated once at creation ([`generate_token`]) and returned to
//!   the caller, never persisted.
//! * **Outgoing** — the server POSTs each matching [`aero_common::RoomEvent`] to
//!   an external URL, HMAC-SHA256 signed with a per-hook secret so the receiver
//!   can verify authenticity ([`sign_payload`] / [`build_delivery`]).
//!
//! ## Testable seams (DB-free, unit-tested)
//!
//! The signing ([`sign_payload`]) and request-shaping ([`build_delivery`]) are
//! pure functions, and the HTTP POST hides behind the [`WebhookSender`] trait so
//! delivery can be exercised offline via [`FakeSender`] (Postgres + the network
//! are both absent in CI). [`ReqwestSender`] is the real transport.
//!
//! Purely additive: a NEW [`WebhookRepo`]; no existing repo is touched.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use super::crypto::sign_payload;

// --------------------------------------------------------- Delivery (the seam)

const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Header name carrying the HMAC signature of a delivery.
pub const SIGNATURE_HEADER: &str = "X-Aero-Signature";
/// Header name carrying the unix timestamp the signature was computed over.
pub const TIMESTAMP_HEADER: &str = "X-Aero-Timestamp";

/// A fully-shaped outgoing HTTP request: where to POST, the headers (signature +
/// timestamp + content-type), and the JSON body. Produced by [`build_delivery`]
/// and consumed by a [`WebhookSender`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub url: String,
    /// Ordered `(name, value)` header pairs.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Delivery {
    /// Lookup a header value by case-insensitive name (small list, linear scan).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Headers safe and necessary to retain for an exact retry. Authentication
    /// headers are regenerated with a fresh timestamp; authority, framing and
    /// other hop-by-hop headers are intentionally never persisted.
    #[must_use]
    pub fn retry_headers(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .filter(|(name, _)| is_retry_header(name))
            .cloned()
            .collect()
    }
}

fn is_retry_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-type") || name.eq_ignore_ascii_case("traceparent")
}

/// Build the signed delivery for `event_json` to `url` with `secret`, stamped at
/// `now` (unix seconds). Pure: same inputs ⇒ byte-identical request, so the
/// signature header is unit-testable without a clock or network. The signature
/// is computed over the *exact* JSON bytes that are sent, so a receiver
/// re-running [`sign_payload`] over the body it received reproduces it.
#[must_use]
pub fn build_delivery(
    url: &str,
    secret: &str,
    event_json: &serde_json::Value,
    now: i64,
) -> Delivery {
    // `to_vec` on a Value never fails; fall back to an empty object on the
    // theoretical error so the function stays total.
    let body = serde_json::to_vec(event_json).unwrap_or_else(|_| b"{}".to_vec());
    build_delivery_from_bytes(
        url,
        secret,
        &body,
        &[("Content-Type".to_owned(), "application/json".to_owned())],
        now,
    )
}

/// Build a freshly signed request around already-serialized immutable bytes.
///
/// Retry uses this entry point so the payload is byte-for-byte identical to the
/// first attempt while the timestamp and HMAC are refreshed. Only the small
/// allow-list accepted by [`Delivery::retry_headers`] is replayed.
#[must_use]
pub fn build_delivery_from_bytes(
    url: &str,
    secret: &str,
    body: &[u8],
    retry_headers: &[(String, String)],
    now: i64,
) -> Delivery {
    let signature = sign_payload(secret, now, &body);
    let mut headers: Vec<_> = retry_headers
        .iter()
        .filter(|(name, _)| is_retry_header(name))
        .cloned()
        .collect();
    headers.push((SIGNATURE_HEADER.to_owned(), signature));
    headers.push((TIMESTAMP_HEADER.to_owned(), now.to_string()));
    Delivery {
        url: url.to_owned(),
        headers,
        body: body.to_vec(),
    }
}

/// What one completed round trip tells the caller: the HTTP status code plus the
/// parsed `Retry-After` cooldown (only meaningful on a 429). Produced by a
/// [`WebhookSender`] and consumed by the delivery-log bookkeeping (numeric
/// `status`) and the circuit breaker ([`outcome_of`] reads `retry_after_secs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryResponse {
    /// The HTTP status code of the completed request.
    pub status: u16,
    /// The receiver's `Retry-After` cooldown in whole seconds, when it sent a
    /// usable integer-seconds value (typically alongside a 429). `None` when the
    /// header is absent, malformed, negative, or in the HTTP-date form (the latter
    /// is best-effort only).
    pub retry_after_secs: Option<i64>,
}

impl DeliveryResponse {
    /// A response carrying just a status (no `Retry-After`) — the common case and
    /// what test doubles / non-429 responses use.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self {
            status,
            retry_after_secs: None,
        }
    }
}

/// Parse an HTTP `Retry-After` header value into whole seconds. Only the
/// delta-seconds form is supported (`"120"` ⇒ `Some(120)`); the HTTP-date form and
/// any negative/garbage value yield `None` (best-effort — the breaker then falls
/// back to its default 429 cooldown). Pure, so it's unit-tested directly.
#[must_use]
pub fn parse_retry_after(value: &str) -> Option<i64> {
    // Delta-seconds is a non-negative integer; reject negatives and non-digits.
    match value.trim().parse::<i64>() {
        Ok(secs) if secs >= 0 => Some(secs),
        _ => None,
    }
}

/// The injectable HTTP seam: POST a built [`Delivery`], returning the response
/// status code (and any `Retry-After`). The real impl is [`ReqwestSender`]; tests
/// use [`FakeSender`].
#[async_trait::async_trait]
pub trait WebhookSender: Send + Sync {
    /// Deliver one request. Returns a [`DeliveryResponse`] on a completed round
    /// trip, or an error string when the request could not be made at all
    /// (DNS/connect/timeout) — the caller logs and moves on (best-effort).
    async fn deliver(&self, delivery: &Delivery) -> Result<DeliveryResponse, String>;
}

#[async_trait::async_trait]
trait DnsLookup: Send + Sync {
    async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String>;
}

struct SystemDns;

#[async_trait::async_trait]
impl DnsLookup for SystemDns {
    async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        tokio::net::lookup_host((host, port))
            .await
            .map(|addresses| addresses.collect())
            .map_err(|error| format!("webhook host does not resolve: {error}"))
    }
}

/// Whether an address must be rejected for webhook egress.
///
/// In addition to loopback/private/link-local ranges this rejects non-routable
/// shared, documentation, benchmarking, multicast and reserved space. IPv4
/// mapped IPv6 addresses are evaluated using their IPv4 value.
#[must_use]
pub fn webhook_ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || a == 0
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 240
        }
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return webhook_ip_is_blocked(IpAddr::V4(mapped));
            }
            let segments = ip.segments();
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] & 0xffc0) == 0xfe80
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        }
    }
}

fn parse_webhook_url(raw: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|_| "webhook URL is invalid".to_owned())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("webhook URL must use http or https".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("webhook URL must not contain credentials".to_owned());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "webhook URL has no host".to_owned())?;
    let normalized = host
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if normalized == "localhost"
        || normalized.ends_with(".localhost")
        || normalized.ends_with(".local")
        || normalized == "metadata.google.internal"
    {
        return Err("webhook URL host is not allowed".to_owned());
    }
    if let Ok(ip) = normalized.parse::<IpAddr>() {
        if webhook_ip_is_blocked(ip) {
            return Err("webhook URL resolves to a non-public address".to_owned());
        }
    }
    Ok(url)
}

fn validate_resolved_addresses(addresses: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, String> {
    if addresses.is_empty() {
        return Err("webhook URL host does not resolve".to_owned());
    }
    let mut validated = Vec::with_capacity(addresses.len());
    for address in addresses {
        if webhook_ip_is_blocked(address.ip()) {
            return Err("webhook URL resolves to a non-public address".to_owned());
        }
        if !validated.contains(&address) {
            validated.push(address);
        }
    }
    Ok(validated)
}

async fn resolve_webhook_target(
    url: &reqwest::Url,
    dns: &dyn DnsLookup,
) -> Result<(String, Vec<SocketAddr>), String> {
    let host = url
        .host_str()
        .ok_or_else(|| "webhook URL has no host".to_owned())?
        .trim_matches(['[', ']'])
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "webhook URL has no usable port".to_owned())?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        return validate_resolved_addresses(vec![SocketAddr::new(ip, port)])
            .map(|addresses| (host, addresses));
    }
    let addresses = dns.lookup(&host, port).await?;
    validate_resolved_addresses(addresses).map(|addresses| (host, addresses))
}

/// Resolve and validate a webhook endpoint at registration time.
///
/// [`ReqwestSender`] repeats the same validation immediately before every
/// connection and pins the returned addresses, so this early check is only fast
/// feedback and is not treated as the security boundary.
pub async fn validate_webhook_url(raw: &str) -> Result<(), String> {
    let url = parse_webhook_url(raw)?;
    tokio::time::timeout(DELIVERY_TIMEOUT, resolve_webhook_target(&url, &SystemDns))
        .await
        .map_err(|_| "webhook URL resolution timed out".to_owned())??;
    Ok(())
}

async fn send_pinned(
    delivery: &Delivery,
    url: reqwest::Url,
    host: &str,
    addresses: &[SocketAddr],
    timeout: Duration,
) -> Result<DeliveryResponse, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        // The request retains its original URL, Host header and TLS SNI while
        // connect is constrained to the addresses validated just above.
        .resolve_to_addrs(host, addresses)
        .build()
        .map_err(|error| format!("webhook client build failed: {error}"))?;
    let mut request = client.post(url).body(delivery.body.clone());
    for (name, value) in &delivery.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let retry_after_secs = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after);
    Ok(DeliveryResponse {
        status,
        retry_after_secs,
    })
}

/// Real HTTP transport over `reqwest`. Each request performs a fresh DNS lookup,
/// rejects every non-public answer, then pins the validated addresses for the
/// connection. Redirects and environment proxies are disabled.
#[derive(Clone)]
pub struct ReqwestSender {
    timeout: Duration,
    dns: Arc<dyn DnsLookup>,
}

impl ReqwestSender {
    /// Build a sender with a sane per-request timeout so a slow endpoint can't
    /// stall the dispatcher.
    #[must_use]
    pub fn new() -> Self {
        Self {
            timeout: DELIVERY_TIMEOUT,
            dns: Arc::new(SystemDns),
        }
    }
}

impl Default for ReqwestSender {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl WebhookSender for ReqwestSender {
    async fn deliver(&self, delivery: &Delivery) -> Result<DeliveryResponse, String> {
        let url = parse_webhook_url(&delivery.url)?;
        tokio::time::timeout(self.timeout, async {
            let (host, addresses) = resolve_webhook_target(&url, self.dns.as_ref()).await?;
            send_pinned(delivery, url, &host, &addresses, self.timeout).await
        })
        .await
        .map_err(|_| "webhook delivery timed out".to_owned())?
    }
}

/// Test double: records every [`Delivery`] and returns a canned status (and an
/// optional canned `Retry-After`). Lets the delivery path (and `build_delivery`'s
/// signature) be asserted without a server.
#[derive(Clone)]
pub struct FakeSender {
    status: u16,
    /// Canned `Retry-After` seconds returned on every delivery (default `None`).
    retry_after_secs: Option<i64>,
    calls: std::sync::Arc<std::sync::Mutex<Vec<Delivery>>>,
}

impl FakeSender {
    /// A sender that always reports `status` (no `Retry-After`) and captures calls.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self {
            status,
            retry_after_secs: None,
            calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Builder: also report a canned `Retry-After` of `secs` seconds (e.g. paired
    /// with `new(429)` to exercise the rate-limit path). Consumes and returns self.
    #[must_use]
    pub fn with_retry_after(mut self, secs: i64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    /// Snapshot of every delivery seen so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<Delivery> {
        self.calls
            .lock()
            .expect("fake-sender mutex not poisoned")
            .clone()
    }
}

#[async_trait::async_trait]
impl WebhookSender for FakeSender {
    async fn deliver(&self, delivery: &Delivery) -> Result<DeliveryResponse, String> {
        self.calls
            .lock()
            .expect("fake-sender mutex not poisoned")
            .push(delivery.clone());
        Ok(DeliveryResponse {
            status: self.status,
            retry_after_secs: self.retry_after_secs,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    #[test]
    fn exact_body_is_reused_while_authentication_is_refreshed() {
        let body = br#"{"kind":"message","text":"immutable"}"#;
        let initial = build_delivery_from_bytes(
            "https://hooks.example/events",
            "secret",
            body,
            &[
                ("Content-Type".to_owned(), "application/json".to_owned()),
                ("traceparent".to_owned(), "00-trace-parent-01".to_owned()),
                ("Host".to_owned(), "attacker.example".to_owned()),
            ],
            10,
        );
        let retry = build_delivery_from_bytes(
            &initial.url,
            "secret",
            &initial.body,
            &initial.retry_headers(),
            11,
        );

        assert_eq!(retry.body, body);
        assert_eq!(retry.header("Content-Type"), Some("application/json"));
        assert_eq!(retry.header("traceparent"), Some("00-trace-parent-01"));
        assert!(
            retry.header("Host").is_none(),
            "authority headers are not replayed"
        );
        assert_ne!(
            retry.header(SIGNATURE_HEADER),
            initial.header(SIGNATURE_HEADER),
            "new timestamp must produce a fresh signature"
        );
        assert_eq!(retry.header(TIMESTAMP_HEADER), Some("11"));
    }

    #[test]
    fn public_address_policy_rejects_internal_and_reserved_ranges() {
        let blocked = [
            "127.0.0.1",
            "10.1.2.3",
            "100.64.0.1",
            "169.254.169.254",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "240.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
        ];
        for raw in blocked {
            assert!(
                webhook_ip_is_blocked(raw.parse().unwrap()),
                "{raw} must be rejected"
            );
        }
        for raw in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(
                !webhook_ip_is_blocked(raw.parse().unwrap()),
                "{raw} must be accepted"
            );
        }
    }

    #[test]
    fn url_parser_rejects_credentials_and_non_http_schemes() {
        assert!(parse_webhook_url("http://user:pass@example.com/hook").is_err());
        assert!(parse_webhook_url("file:///etc/passwd").is_err());
        assert!(parse_webhook_url("http://localhost/hook").is_err());
        assert!(parse_webhook_url("https://metadata.google.internal/hook").is_err());
        assert!(parse_webhook_url("https://8.8.8.8/hook").is_ok());
    }

    struct PrivateDns {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl DnsLookup for PrivateDns {
        async fn lookup(&self, _host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            Ok(vec![SocketAddr::new(
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                port,
            )])
        }
    }

    #[tokio::test]
    async fn sender_resolves_every_delivery_and_rejects_rebound_private_dns() {
        let dns = Arc::new(PrivateDns {
            calls: AtomicUsize::new(0),
        });
        let sender = ReqwestSender {
            timeout: Duration::from_secs(1),
            dns: dns.clone(),
        };
        let delivery = build_delivery(
            "https://rebind.example/hook",
            "secret",
            &serde_json::json!({"kind": "message"}),
            1,
        );

        for _ in 0..2 {
            let error = sender.deliver(&delivery).await.unwrap_err();
            assert!(error.contains("non-public"));
        }
        assert_eq!(
            dns.calls.load(Ordering::Acquire),
            2,
            "DNS must be checked afresh for every attempted delivery"
        );
    }

    #[tokio::test]
    async fn pinned_transport_preserves_host_and_never_follows_redirects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let read = socket.read(&mut request).await.unwrap();
            request.truncate(read);
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://hooks.example:{}/follow\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                address.port()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            drop(socket);
            let followed = tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_ok();
            (String::from_utf8_lossy(&request).into_owned(), followed)
        });

        let url = reqwest::Url::parse(&format!("http://hooks.example:{}/initial", address.port()))
            .unwrap();
        let delivery = Delivery {
            url: url.to_string(),
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: b"{}".to_vec(),
        };
        let response = send_pinned(
            &delivery,
            url,
            "hooks.example",
            &[address],
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(response.status, 302);

        let (request, followed) = server.await.unwrap();
        assert!(
            request
                .to_ascii_lowercase()
                .contains(&format!("host: hooks.example:{}", address.port())),
            "original URL authority must be retained while the IP is pinned"
        );
        assert!(!followed, "redirect policy must not issue a second request");
    }
}
