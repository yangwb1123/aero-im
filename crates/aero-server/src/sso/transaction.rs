use aero_common::Error as AeroError;
use axum::http::{header, HeaderMap};

pub(super) const MAX_CALLBACK_QUERY_BYTES: usize = 16 * 1024;
pub(super) const RANDOM_VALUE_LEN: usize = 43;

pub(super) fn cleanup_state_from_callback(raw: Option<&str>) -> Option<String> {
    let raw = raw.filter(|value| value.len() <= MAX_CALLBACK_QUERY_BYTES)?;
    let mut state = None;
    for (name, value) in form_urlencoded::parse(raw.as_bytes()) {
        if name == "state" && state.replace(value.into_owned()).is_some() {
            return None;
        }
    }
    let state = state?;
    validate_random_value(&state, "state").ok()?;
    Some(state)
}

pub(super) fn set_unique_query_value(
    slot: &mut Option<String>,
    value: String,
    name: &str,
) -> Result<(), AeroError> {
    if slot.replace(value).is_some() {
        return Err(AeroError::Invalid(format!(
            "duplicate oidc callback {name}"
        )));
    }
    Ok(())
}

pub(super) fn validate_random_value(value: &str, name: &str) -> Result<(), AeroError> {
    if value.len() != RANDOM_VALUE_LEN
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(AeroError::Unauthorized(format!("invalid oidc flow {name}")));
    }
    Ok(())
}

pub(super) fn optional_cookie_value(
    headers: &HeaderMap,
    name: &str,
) -> Result<Option<String>, AeroError> {
    let mut found = None;
    for header_value in headers.get_all(header::COOKIE) {
        let Ok(raw) = header_value.to_str() else {
            return Err(AeroError::Unauthorized("invalid oidc flow cookie".into()));
        };
        for pair in raw.split(';') {
            let Some((cookie_name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if cookie_name == name && found.replace(value.to_owned()).is_some() {
                return Err(AeroError::Unauthorized("duplicate oidc flow cookie".into()));
            }
        }
    }
    Ok(found)
}

pub(super) fn cookie_value(headers: &HeaderMap, name: &str) -> Result<String, AeroError> {
    let value = optional_cookie_value(headers, name)?
        .ok_or_else(|| AeroError::Unauthorized("oidc flow cookie missing".into()))?;
    validate_random_value(&value, "cookie")?;
    Ok(value)
}

pub(super) fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}
