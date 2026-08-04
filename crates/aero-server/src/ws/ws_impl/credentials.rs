//! WebSocket handshake credential selection.
//!
//! Non-browser clients can keep bearer material out of the request target by
//! using `Authorization`. Browser `WebSocket` constructors cannot set that
//! header, so the existing `token` query parameter remains a compatibility
//! fallback. Aero IM does not currently issue an authentication cookie: the
//! `aero_oidc_*` cookies are short-lived OIDC transaction state and must never
//! be accepted as session credentials.

use axum::http::{header, HeaderMap};
use serde::Deserialize;
use std::fmt;

pub(super) const MAX_WS_ACCESS_TOKEN_BYTES: usize = 48 * 1024;

/// Bearer material whose formatting is always redacted.
///
/// In particular, `WsParams.token` uses this type directly so a query token
/// never exists inside a normally-debuggable `String` field.
#[derive(Deserialize)]
#[serde(transparent)]
pub(super) struct RedactedAccessToken(String);

impl RedactedAccessToken {
    pub(super) fn expose(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(super) fn for_test(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Debug for RedactedAccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Selected handshake credential. Its `Debug` output is safe as defence in
/// depth if a future tracing span accidentally records it.
pub(super) struct SelectedAccessToken {
    token: RedactedAccessToken,
}

impl SelectedAccessToken {
    pub(super) fn expose(&self) -> &str {
        self.token.expose()
    }
}

impl fmt::Debug for SelectedAccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SelectedAccessToken")
            .field("token", &self.token)
            .finish()
    }
}

/// Static-only errors: no variant carries attacker-controlled credential data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WsCredentialError {
    Missing,
    InvalidAuthorization,
    Empty,
    TooLong,
}

impl fmt::Display for WsCredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "missing websocket credential",
            Self::InvalidAuthorization => "invalid websocket authorization header",
            Self::Empty => "empty websocket credential",
            Self::TooLong => "websocket credential exceeds the size limit",
        })
    }
}

/// Select `Authorization: Bearer` first, then the legacy query credential.
///
/// A present but malformed Authorization header owns the attempt and fails
/// closed instead of silently downgrading to the query value. Cookies are not
/// inspected because Aero IM has no authentication-cookie contract.
pub(super) fn select_access_token(
    headers: &HeaderMap,
    query_token: Option<RedactedAccessToken>,
) -> Result<SelectedAccessToken, WsCredentialError> {
    let mut authorization_values = headers.get_all(header::AUTHORIZATION).iter();
    let selected = if let Some(value) = authorization_values.next() {
        if authorization_values.next().is_some() {
            return Err(WsCredentialError::InvalidAuthorization);
        }
        let value = value
            .to_str()
            .map_err(|_| WsCredentialError::InvalidAuthorization)?;
        let (scheme, token) = value
            .split_once(' ')
            .ok_or(WsCredentialError::InvalidAuthorization)?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(WsCredentialError::InvalidAuthorization);
        }
        SelectedAccessToken {
            token: RedactedAccessToken(token.trim().to_owned()),
        }
    } else {
        SelectedAccessToken {
            token: query_token.ok_or(WsCredentialError::Missing)?,
        }
    };

    if selected.expose().is_empty() {
        return Err(WsCredentialError::Empty);
    }
    if selected.expose().len() > MAX_WS_ACCESS_TOKEN_BYTES {
        return Err(WsCredentialError::TooLong);
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn query(value: impl Into<String>) -> Option<RedactedAccessToken> {
        Some(RedactedAccessToken::for_test(value))
    }

    #[test]
    fn authorization_precedes_query_and_every_debug_view_is_redacted() {
        let header_secret = "header-secret";
        let query_secret = "query-secret";
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer header-secret"),
        );

        let selected = select_access_token(&headers, query(query_secret)).unwrap();
        assert_eq!(selected.expose(), header_secret);

        let rendered = format!("{selected:?}");
        assert!(!rendered.contains(header_secret));
        assert!(!rendered.contains(query_secret));
        assert!(rendered.contains("[REDACTED]"));
    }

    #[test]
    fn malformed_authorization_does_not_downgrade_to_query() {
        for value in ["Basic header-secret", "Bearer", "Bearer    "] {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            let error = select_access_token(&headers, query("query-secret")).unwrap_err();
            assert!(matches!(
                error,
                WsCredentialError::InvalidAuthorization | WsCredentialError::Empty
            ));
            assert!(!format!("{error:?} {error}").contains("query-secret"));
        }
    }

    #[test]
    fn query_is_the_legacy_fallback_and_oidc_flow_cookies_are_not_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("aero_oidc_state=not-a-session; other=value"),
        );

        let selected = select_access_token(&headers, query("query-secret")).unwrap();
        assert_eq!(selected.expose(), "query-secret");

        let error = select_access_token(&headers, None).unwrap_err();
        assert_eq!(error, WsCredentialError::Missing);
    }

    #[test]
    fn both_sources_enforce_the_same_48_kib_limit() {
        let exact = "a".repeat(MAX_WS_ACCESS_TOKEN_BYTES);
        let oversized = "a".repeat(MAX_WS_ACCESS_TOKEN_BYTES + 1);

        let selected = select_access_token(&HeaderMap::new(), query(&exact)).unwrap();
        assert_eq!(selected.expose().len(), MAX_WS_ACCESS_TOKEN_BYTES);
        assert_eq!(
            select_access_token(&HeaderMap::new(), query(&oversized)).unwrap_err(),
            WsCredentialError::TooLong
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {exact}")).unwrap(),
        );
        assert_eq!(
            select_access_token(&headers, None).unwrap().expose().len(),
            MAX_WS_ACCESS_TOKEN_BYTES
        );
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {oversized}")).unwrap(),
        );
        assert_eq!(
            select_access_token(&headers, query("valid-query")).unwrap_err(),
            WsCredentialError::TooLong
        );
    }
}
