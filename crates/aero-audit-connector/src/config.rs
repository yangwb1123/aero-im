//! Fail-loud env-driven config for the audit connector.
//!
//! Activation is presence-gated on `AERO_AUDIT_TOKEN_ENDPOINT`; any other
//! `AERO_AUDIT_*` variable set while it is absent is a boot error, never a
//! silent disable (v1 `snaplink_commercial/config.rs` precedent). The two
//! lease bails are cloned verbatim from `snaplink_commercial/config.rs`:
//! `delivery_lease > 2 × request_timeout + 2s` and
//! `task_drain > 2 × request_timeout + shutdown_drain + 2s`.
//!
//! Plain single-underscore env family (AGENTS.md §4.3 exceptions), disjoint
//! from v1's `AERO_SNAPLINK_*` surface so both relays can coexist during the
//! v1→v2 cutover.

use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use reqwest::Url;

const MAX_ENDPOINT_BYTES: usize = 2_048;
const MAX_IDENTITY_BYTES: usize = 256;
const MAX_SECRET_BYTES: usize = 16 * 1024;

/// Connector configuration. All fields are public so tests and the drill bin
/// can construct a config directly; production loads it via
/// [`RelayConfig::from_env`].
#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub token_endpoint: Url,
    pub events_url: Url,
    pub resource: String,
    pub client_id: String,
    pub client_secret: String,
    pub expected_iss: String,
    pub expected_aud: String,
    pub expected_scope: String,
    pub expected_sub: String,
    /// Trusted source system the outbound payload must match (v1 binding
    /// clone).
    pub source_system: String,
    pub request_timeout: Duration,
    pub delivery_lease: Duration,
    pub poll_interval: Duration,
    pub shutdown_drain: Duration,
    pub batch_size: i64,
    pub concurrency: usize,
    /// Trusted-JWKS endpoint (`AERO_AUDIT_JWKS_URL`). `Some` ⇒ signature
    /// verification is enforced on every token before a delivery POST;
    /// `None` ⇒ signature verification is off (exp/nbf claim checks stay
    /// unconditional). Same URL policy as the service endpoints.
    pub jwks_uri: Option<Url>,
    /// Settle-face freshness window (`AERO_AUDIT_PROVISION_FRESHNESS_SECS`,
    /// default 300, bounds [60, 86400]): a durable heartbeat older than this
    /// rejects the acknowledgement fail-closed. Same `AERO_AUDIT_*` family,
    /// so setting it without `AERO_AUDIT_TOKEN_ENDPOINT` trips the existing
    /// stray-scan boot error (fail-loud, no new code).
    pub provision_freshness: Duration,
}

impl RelayConfig {
    /// Load from `AERO_AUDIT_*` env. Returns `Ok(None)` when the feature is
    /// off (no `AERO_AUDIT_TOKEN_ENDPOINT` and no other `AERO_AUDIT_*` var).
    /// Any malformed or incomplete surface is a hard error.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let Some(token_raw) = optional_env("AERO_AUDIT_TOKEN_ENDPOINT") else {
            let stray = std::env::vars_os()
                .filter(|(name, _)| name.to_string_lossy().starts_with("AERO_AUDIT_"))
                .count();
            if stray > 0 {
                bail!(
                    "AERO_AUDIT_TOKEN_ENDPOINT is unset while {stray} AERO_AUDIT_* variable(s) are present; \
                     configure the audit connector completely or remove them"
                );
            }
            return Ok(None);
        };
        let allow_insecure = bool_or_default("AERO_AUDIT_ALLOW_INSECURE_LOOPBACK", false)?;
        let token_endpoint = service_url("AERO_AUDIT_TOKEN_ENDPOINT", &token_raw, allow_insecure)?;
        let events_raw = required_env("AERO_AUDIT_EVENTS_URL")?;
        let events_url = service_url("AERO_AUDIT_EVENTS_URL", &events_raw, allow_insecure)?;

        // AM-3 (merged B5-2): the two sibling specs unify on the single env
        // name `AERO_AUDIT_JWKS_URL` ("URL wins") — there is no runtime
        // precedence. Both set = the deployment is not unified = ambiguity →
        // fail-loud at boot; URI-only = a deployment still using the sibling
        // name, which must migrate rather than silently disable signature
        // verification (the sibling's fail-loud terminal state).

        let request_timeout = duration_secs("AERO_AUDIT_REQUEST_TIMEOUT_SECS", 10, 1, 120)?;
        let delivery_lease = duration_secs("AERO_AUDIT_DELIVERY_LEASE_SECS", 30, 5, 300)?;
        check_lease_invariant(request_timeout, delivery_lease)?;
        let shutdown_drain = duration_secs("AERO_AUDIT_SHUTDOWN_DRAIN_SECS", 5, 1, 60)?;
        let task_drain = duration_secs("AERO_AUDIT_TASK_DRAIN_SECS", 30, 1, 600)?;
        check_drain_invariant(request_timeout, shutdown_drain, task_drain)?;
        let poll_interval = duration_secs("AERO_AUDIT_POLL_INTERVAL_SECS", 5, 1, 300)?;
        let batch_size = integer_env("AERO_AUDIT_BATCH_SIZE", 100, 1, 500)?;
        let concurrency =
            usize::try_from(integer_env("AERO_AUDIT_CONCURRENCY", 4, 1, 32)?).expect("bounded");
        let provision_freshness =
            duration_secs("AERO_AUDIT_PROVISION_FRESHNESS_SECS", 300, 60, 86_400)?;

        let jwks_uri = if let Some(raw) = optional_env("AERO_AUDIT_JWKS_URL") {
            if optional_env("AERO_AUDIT_JWKS_URI").is_some() {
                bail!(
                    "AERO_AUDIT_JWKS_URL and AERO_AUDIT_JWKS_URI are both set; \
                         use only AERO_AUDIT_JWKS_URL (the unified B5-2 env name)"
                );
            }
            Some(service_url("AERO_AUDIT_JWKS_URL", &raw, allow_insecure)?)
        } else if let Some(legacy) = optional_env("AERO_AUDIT_JWKS_URI") {
            bail!(
                "AERO_AUDIT_JWKS_URI is deprecated; set AERO_AUDIT_JWKS_URL instead \
                     (got {legacy})"
            );
        } else {
            None
        };

        Ok(Some(Self {
            token_endpoint,
            events_url,
            resource: identity_env("AERO_AUDIT_RESOURCE", 512)?,
            client_id: identity_env("AERO_AUDIT_CLIENT_ID", 512)?,
            client_secret: secret_env("AERO_AUDIT_CLIENT_SECRET")?,
            expected_iss: identity_env("AERO_AUDIT_EXPECTED_ISS", MAX_IDENTITY_BYTES)?,
            expected_aud: identity_env("AERO_AUDIT_EXPECTED_AUD", MAX_IDENTITY_BYTES)?,
            expected_scope: optional_env("AERO_AUDIT_EXPECTED_SCOPE")
                .unwrap_or_else(|| "audit:event:write".to_owned()),
            expected_sub: identity_env("AERO_AUDIT_EXPECTED_SUB", MAX_IDENTITY_BYTES)?,
            source_system: identity_env("AERO_AUDIT_SOURCE_SYSTEM", MAX_IDENTITY_BYTES)?,
            request_timeout,
            delivery_lease,
            poll_interval,
            shutdown_drain,
            batch_size,
            concurrency,
            jwks_uri,
            provision_freshness,
        }))
    }
}

/// `delivery_lease > 2 × request_timeout + 2s` (v1 `config.rs:246-251` clone).
/// Two request timeouts must fit inside one lease (401-refresh-once retry),
/// plus slack for scheduling. This bounds the in-attempt retry budget: with
/// the lease minted on the repo clock (`clock_timestamp() + lease` — single
/// clock domain), the worst-case in-attempt HTTP budget (initial POST +
/// one 401-refresh) measured against that same clock can never outlive the
/// lease, so a second instance cannot reclaim mid-attempt. The bail is
/// clock-domain-independent (pure duration arithmetic), so it holds
/// unchanged after the claim-side clock fix.
pub fn check_lease_invariant(
    request_timeout: Duration,
    delivery_lease: Duration,
) -> anyhow::Result<()> {
    if delivery_lease <= request_timeout.saturating_mul(2) + Duration::from_secs(2) {
        bail!("AERO_AUDIT_DELIVERY_LEASE_SECS must exceed two request timeouts by more than two seconds");
    }
    Ok(())
}

/// `task_drain > 2 × request_timeout + shutdown_drain + 2s` (v1
/// `config.rs:253-255` clone).
pub fn check_drain_invariant(
    request_timeout: Duration,
    shutdown_drain: Duration,
    task_drain: Duration,
) -> anyhow::Result<()> {
    if task_drain <= request_timeout.saturating_mul(2) + shutdown_drain + Duration::from_secs(2) {
        bail!("AERO_AUDIT_TASK_DRAIN_SECS must exceed two request timeouts and the shutdown drain budget by more than two seconds");
    }
    Ok(())
}

fn service_url(name: &str, raw: &str, insecure: bool) -> anyhow::Result<Url> {
    let url = Url::parse(raw).with_context(|| format!("parse {name}"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || url.as_str().len() > MAX_ENDPOINT_BYTES
    {
        bail!("{name} must not contain credentials, query, fragment, or exceed 2048 bytes");
    }
    match url.scheme() {
        "https" => {}
        "http" if insecure && is_loopback(&url) => {}
        _ => bail!("{name} must use HTTPS (HTTP is opt-in and loopback-only)"),
    }
    Ok(url)
}

fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn identity_env(name: &str, max: usize) -> anyhow::Result<String> {
    let value = required_env(name)?;
    if value.is_empty()
        || value != value.trim()
        || value.len() > max
        || value.chars().any(char::is_control)
    {
        bail!("{name} is invalid");
    }
    Ok(value)
}

fn secret_env(name: &str) -> anyhow::Result<String> {
    let value = required_env(name)?;
    if value.len() < 32
        || value != value.trim()
        || value.len() > MAX_SECRET_BYTES
        || value.chars().any(char::is_control)
    {
        bail!("{name} is invalid (at least 32 non-control bytes)");
    }
    Ok(value)
}

fn required_env(name: &str) -> anyhow::Result<String> {
    optional_env(name).ok_or_else(|| anyhow!("{name} is required"))
}

fn optional_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn bool_or_default(name: &str, default: bool) -> anyhow::Result<bool> {
    match std::env::var(name) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" => Ok(true),
            "0" | "false" => Ok(false),
            _ => bail!("{name} must be true/false or 1/0"),
        },
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn integer_env(name: &str, default: i64, min: i64, max: i64) -> anyhow::Result<i64> {
    let value = optional_env(name)
        .map(|raw| raw.parse::<i64>().with_context(|| format!("parse {name}")))
        .transpose()?
        .unwrap_or(default);
    if !(min..=max).contains(&value) {
        bail!("{name} must be between {min} and {max}");
    }
    Ok(value)
}

fn duration_secs(name: &str, default: i64, min: i64, max: i64) -> anyhow::Result<Duration> {
    Ok(Duration::from_secs(
        u64::try_from(integer_env(name, default, min, max)?).expect("positive duration"),
    ))
}

#[cfg(test)]
mod tests {
    use super::{check_drain_invariant, check_lease_invariant, RelayConfig};
    use reqwest::Url;
    use std::time::Duration;

    #[test]
    fn lease_must_exceed_two_request_timeouts_plus_slack() {
        let timeout = Duration::from_secs(10);
        assert!(check_lease_invariant(timeout, Duration::from_secs(30)).is_ok());
        assert!(check_lease_invariant(timeout, Duration::from_secs(22)).is_err());
        assert!(check_lease_invariant(timeout, Duration::from_secs(21)).is_err());
        assert!(check_lease_invariant(Duration::from_secs(5), Duration::from_secs(12)).is_err());
        assert!(check_lease_invariant(Duration::from_secs(5), Duration::from_secs(13)).is_ok());
    }

    #[test]
    fn task_drain_must_exceed_two_timeouts_and_the_shutdown_budget() {
        let timeout = Duration::from_secs(10);
        let shutdown = Duration::from_secs(5);
        assert!(check_drain_invariant(timeout, shutdown, Duration::from_secs(30)).is_ok());
        assert!(check_drain_invariant(timeout, shutdown, Duration::from_secs(27)).is_err());
    }

    // -----------------------------------------------------------------------
    // `AERO_AUDIT_JWKS_URL` parsing (merged B5-2 config surface).
    // -----------------------------------------------------------------------

    const REQUIRED_AUDIT_ENV: &[(&str, &str)] = &[
        (
            "AERO_AUDIT_TOKEN_ENDPOINT",
            "https://idp.example.test/token",
        ),
        ("AERO_AUDIT_EVENTS_URL", "https://audit.example.test/events"),
        ("AERO_AUDIT_RESOURCE", "audit-governance"),
        ("AERO_AUDIT_CLIENT_ID", "drill-client"),
        (
            "AERO_AUDIT_CLIENT_SECRET",
            "0123456789abcdef0123456789abcdef",
        ),
        ("AERO_AUDIT_EXPECTED_ISS", "https://idp.example.test"),
        ("AERO_AUDIT_EXPECTED_AUD", "audit-governance"),
        ("AERO_AUDIT_EXPECTED_SUB", "aero-im.source"),
        ("AERO_AUDIT_SOURCE_SYSTEM", "aero-im.source"),
    ];

    /// Serializes the env-mutating config tests (cargo runs them concurrently
    /// in one process).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const JWKS_ENV_KEYS: &[&str] = &[
        "AERO_AUDIT_JWKS_URL",
        "AERO_AUDIT_JWKS_URI",
        "AERO_AUDIT_TOKEN_ENDPOINT",
        "AERO_AUDIT_EVENTS_URL",
        "AERO_AUDIT_RESOURCE",
        "AERO_AUDIT_CLIENT_ID",
        "AERO_AUDIT_CLIENT_SECRET",
        "AERO_AUDIT_EXPECTED_ISS",
        "AERO_AUDIT_EXPECTED_AUD",
        "AERO_AUDIT_EXPECTED_SCOPE",
        "AERO_AUDIT_EXPECTED_SUB",
        "AERO_AUDIT_SOURCE_SYSTEM",
        "AERO_AUDIT_ALLOW_INSECURE_LOOPBACK",
        "AERO_AUDIT_PROVISION_FRESHNESS_SECS",
    ];

    /// Run `f` with a controlled `AERO_AUDIT_*` env (required vars + `extra`),
    /// snapshotting and restoring the caller's environment afterwards.
    fn with_audit_env<T>(extra: &[(&str, &str)], f: impl FnOnce() -> T) -> T {
        let saved: Vec<(String, Option<String>)> = JWKS_ENV_KEYS
            .iter()
            .map(|key| ((*key).to_owned(), std::env::var(key).ok()))
            .collect();
        for (key, _) in &saved {
            std::env::remove_var(key);
        }
        for (key, value) in REQUIRED_AUDIT_ENV.iter().chain(extra) {
            std::env::set_var(key, value);
        }
        let result = f();
        for (key, value) in saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        result
    }

    #[test]
    fn jwks_url_absent_parses_none() {
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let config = with_audit_env(&[], RelayConfig::from_env)
            .expect("complete audit env must parse")
            .expect("connector enabled");
        assert!(
            config.jwks_uri.is_none(),
            "unset AERO_AUDIT_JWKS_URL must leave signature verification off"
        );
    }

    #[test]
    fn jwks_url_legal_parses() {
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let config = with_audit_env(
            &[("AERO_AUDIT_JWKS_URL", "https://idp.example.test/jwks")],
            RelayConfig::from_env,
        )
        .expect("complete audit env must parse")
        .expect("connector enabled");
        assert_eq!(
            config.jwks_uri.as_ref().map(Url::as_str),
            Some("https://idp.example.test/jwks"),
            "a legal JWKS URL must parse into the config"
        );
    }

    #[test]
    fn jwks_url_illegal_bails() {
        let _guard = ENV_LOCK.lock().expect("env test lock");
        // Non-loopback cleartext HTTP fails the service_url policy (boot
        // fail-loud: the relay never starts, never claims, never delivers).
        let error = with_audit_env(
            &[("AERO_AUDIT_JWKS_URL", "http://evil.example.test/jwks")],
            RelayConfig::from_env,
        )
        .expect_err("non-loopback HTTP JWKS URL must fail loudly");
        assert!(error.to_string().contains("AERO_AUDIT_JWKS_URL"));
    }

    #[test]
    fn both_jwks_env_vars_bail() {
        let _guard = ENV_LOCK.lock().expect("env test lock");
        // AM-3 (merged): the two sibling specs recognize exactly one env
        // name — both set is ambiguity, never a runtime precedence.
        let error = with_audit_env(
            &[
                ("AERO_AUDIT_JWKS_URL", "https://idp.example.test/jwks"),
                ("AERO_AUDIT_JWKS_URI", "https://idp.example.test/jwks"),
            ],
            RelayConfig::from_env,
        )
        .expect_err("both JWKS env names must fail loudly");
        assert!(error.to_string().contains("AERO_AUDIT_JWKS_URL"));
        assert!(error.to_string().contains("AERO_AUDIT_JWKS_URI"));
    }

    /// R4.3 — `AERO_AUDIT_PROVISION_FRESHNESS_SECS`: default 300, legal
    /// values parse, out-of-bounds (below 60 / above 86400 / garbage) bail
    /// fail-loud like the rest of the `AERO_AUDIT_*` family.
    #[test]
    fn provision_freshness_defaults_and_bounds() {
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let default = with_audit_env(&[], RelayConfig::from_env)
            .expect("complete audit env must parse")
            .expect("connector enabled");
        assert_eq!(
            default.provision_freshness,
            std::time::Duration::from_secs(300),
            "unset AERO_AUDIT_PROVISION_FRESHNESS_SECS defaults to 300s"
        );

        let legal = with_audit_env(
            &[("AERO_AUDIT_PROVISION_FRESHNESS_SECS", "600")],
            RelayConfig::from_env,
        )
        .expect("legal freshness must parse")
        .expect("connector enabled");
        assert_eq!(
            legal.provision_freshness,
            std::time::Duration::from_secs(600)
        );

        for bad in ["59", "86401", "not-a-number"] {
            let error = with_audit_env(
                &[("AERO_AUDIT_PROVISION_FRESHNESS_SECS", bad)],
                RelayConfig::from_env,
            )
            .expect_err("out-of-bounds freshness must bail fail-loud");
            assert!(
                error
                    .to_string()
                    .contains("AERO_AUDIT_PROVISION_FRESHNESS_SECS"),
                "error must name the offending env var (got: {error})"
            );
        }
    }
}
