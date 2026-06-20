//! Enterprise SSO via **SAML 2.0** — Service Provider (SP) endpoints.
//!
//! This complements the OIDC relying-party flow in [`crate::sso`] for the large
//! installed base of traditional enterprise IdPs (ADFS, Shibboleth, PingFederate,
//! Okta classic, …) that speak SAML rather than OIDC. The SP exposes three
//! endpoints, mirroring the OIDC module's *JIT-provisioning* style:
//!
//! * `GET  /saml/metadata` — SP metadata XML (entity id, ACS endpoint, binding).
//! * `GET  /saml/login`    — build an `AuthnRequest` and 302 to the IdP's SSO URL
//!   via the **HTTP-Redirect binding** (DEFLATE + base64 + url-encode).
//! * `POST /saml/acs`      — Assertion Consumer Service: receive the IdP's
//!   `SAMLResponse`, **verify its XML signature**, extract the `NameID` +
//!   attributes, JIT-provision a participant, and mint *our own* session tokens.
//!
//! # ⚠️ Security posture: signature verification is FAIL-CLOSED by default
//!
//! The security core of SAML is the **XML digital signature** on the IdP's
//! assertion (XML-DSig + canonicalization, per the W3C spec). Hand-rolling that
//! is a notorious source of vulnerabilities (signature-wrapping / XSW, canonical-
//! ization mismatches, comment-truncation, …) and **must never be faked**.
//!
//! The standard Rust library for this is `samael`, which wraps the C library
//! `xmlsec1`. In this build environment `xmlsec1` is **not installed**, so
//! `samael` does not compile (its `xmlsec` feature is mandatory for real crypto;
//! its `crypto_disabled` path is a *no-op stub* that would be a security hole).
//!
//! ## ⚠️ Opt-in experimental verifier: `bergshamra` (UN-AUDITED)
//!
//! [`verify_response_signature`] can be switched from fail-closed to a **real**
//! XML-DSig check backed by [`bergshamra`] — a pure-Rust XML-Security suite
//! (`#![forbid(unsafe_code)]`, exclusive-C14N + DSig-verify + unconditional
//! duplicate-ID rejection + strict positional XSW guard, zero system C deps). It
//! is **off by default** and only runs when the operator sets
//! `AERO_SAML_EXPERIMENTAL_VERIFY=1`.
//!
//! **SECURITY CAVEAT:** `bergshamra` is **pre-1.0 and has NOT had a third-party
//! security audit**. A canonicalization / digest / signature-wrapping bug in it
//! is a *silent authentication bypass*. **Review it yourself before enabling in
//! production; for a hardened deployment prefer the audited `xmlsec1` + `samael`
//! path.** The opt-in gate exists precisely so this trust is a conscious operator
//! choice, never an accident. See [`BERGSHAMRA_SECURITY_CAVEAT`].
//!
//! ## Pure-Rust XML-DSig survey (2026-06, why still fail-closed)
//!
//! The hardest, most security-critical piece is **exclusive C14N**
//! (`xml-exc-c14n`): a single canonicalization divergence from the IdP's signer
//! is a signature *bypass*, not a benign mismatch. The crypto primitives this
//! would need (`rsa`, `ring`, `sha2`, `x509-cert`/`x509-parser`, `der`/`spki`)
//! are already vetted and in the lockfile; the *gap* is a trustworthy C14N +
//! DSig-verify + XSW-defense stack. The full ecosystem was surveyed and
//! compile-tested:
//!
//! * `bergshamra` 0.5.1 (RustCrypto-based, `#![forbid(unsafe_code)]`, no libxml2)
//!   — the only credible pure-Rust XML-Security suite. **Compiles cleanly here,
//!   zero system C deps.** But: first published 2026-02-22 (~4 months old),
//!   pre-1.0, ~12k total downloads, **no third-party security audit**, and only
//!   itself as a reverse-dependency. Its own docs require the *caller* to enforce
//!   the SAML XSW invariant (a verified `<Reference>` must point at the consumed
//!   `<Assertion>`). Promising, not yet *vetted*.
//! * `xml-sec` 0.1.6 — README states it **"should not yet be used in production"**.
//! * `xml-canonicalization` 0.1.x — C14N primitive only (no DSig/verify), 0.1.
//! * `xmlsig-lc-rs` 0.1.0 / `saml` 0.0.1-alpha / `opensaml` 0.1.x — days/weeks
//!   old, alpha/0.1, double-digit downloads.
//!
//! Conclusion: **no vetted pure-Rust XML-DSig path exists yet.** A C14N or XSW
//! bug in any of these is a silent auth-bypass, so wiring one in today would be
//! *less* safe than failing closed. We deliberately do **not** hand-roll C14N
//! either — that is the canonical way to ship a bypassable verifier.
//!
//! Therefore this module ships the **non-cryptographic SP skeleton**: every other
//! part of the flow is real (metadata, AuthnRequest, base64 decode, assertion
//! field extraction, JIT provisioning), but [`verify_response_signature`] is
//! **fail-closed**: until a vetted XML-DSig verifier is wired in — `samael` with a
//! system `xmlsec1`, or a pure-Rust suite (e.g. `bergshamra`) once it has matured
//! / been audited — [`acs`] **rejects every assertion** with a clear error rather
//! than trusting an unverified one. The JIT path below the verification gate is
//! written but *unreachable* in this build — a documented **staging seam**: a
//! deployment that installs `xmlsec1` + flips on `samael` (or adopts a vetted
//! pure-Rust verifier) flips one function and the flow goes live. No code path in
//! this module ever provisions a session from an *unverified* assertion.
//!
//! SAML is **off by default**: [`SamlConfig::from_env`] returns `None` unless the
//! deployment configures the IdP entity id / SSO URL / certificate.

use aero_common::{Error as AeroError, ParticipantId, WorkspaceId, WorkspaceRole};
use aero_storage::SsoRepo;
use axum::{
    extract::State,
    http::header,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Json, Router,
};
use base64::Engine as _;
use serde::Deserialize;
use std::io::Write as _;

use crate::error::ApiResult;
use crate::state::AppState;

/// The legacy / default workspace every login enrolls into — the all-zero UUID,
/// matching [`crate::sso`] and `crate::routes::DEFAULT_WORKSPACE_ID` (kept in sync
/// with migration `0006_workspaces.sql`).
const DEFAULT_WORKSPACE_ID: WorkspaceId = WorkspaceId(ulid::Ulid(0));

/// Mount the SAML SP routes. Folded into the main router by
/// [`crate::routes::build`] via `.merge(crate::saml::routes())`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/saml/metadata", get(metadata))
        .route("/saml/login", get(login))
        .route("/saml/acs", post(acs))
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Static SAML SP ⇄ IdP configuration.
///
/// `sp_entity_id` and `acs_url` identify *us* (the Service Provider) in metadata
/// and in the `AuthnRequest`. `idp_entity_id` / `idp_sso_url` / `idp_cert_pem`
/// describe the trusted IdP — the certificate is the public key the assertion's
/// XML signature is verified against once verification is wired.
#[derive(Debug, Clone)]
pub struct SamlConfig {
    /// Our SP entity id (an absolute URI; conventionally the metadata URL).
    pub sp_entity_id: String,
    /// Our Assertion Consumer Service URL (where the IdP POSTs the `SAMLResponse`).
    pub acs_url: String,
    /// The trusted IdP's entity id — checked against the response `<Issuer>`.
    pub idp_entity_id: String,
    /// The IdP's SingleSignOn service URL (HTTP-Redirect binding target).
    pub idp_sso_url: String,
    /// The IdP's signing certificate, PEM-encoded (`-----BEGIN CERTIFICATE-----`).
    /// This is the trusted key material the XML-DSig verifier checks the response
    /// signature against (the *only* accepted key — inline `<KeyInfo>` keys are
    /// ignored). Used by the opt-in `bergshamra` path; with the verifier off
    /// (default) it is held for the staging seam.
    pub idp_cert_pem: String,
}

impl SamlConfig {
    /// Load from `AERO__SAML__SP_ENTITY_ID` / `AERO__SAML__ACS_URL` /
    /// `AERO__SAML__IDP_ENTITY_ID` / `AERO__SAML__IDP_SSO_URL` /
    /// `AERO__SAML__IDP_CERT_PEM`.
    ///
    /// Returns `None` unless **all five** are set and non-empty, so SAML stays
    /// disabled by default and a half-configured deployment fails closed rather
    /// than silently trusting a partial config (same discipline as
    /// [`aero_auth::OidcConfig::from_env`]).
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Some(Self {
            sp_entity_id: non_empty_env("AERO__SAML__SP_ENTITY_ID")?,
            acs_url: non_empty_env("AERO__SAML__ACS_URL")?,
            idp_entity_id: non_empty_env("AERO__SAML__IDP_ENTITY_ID")?,
            idp_sso_url: non_empty_env("AERO__SAML__IDP_SSO_URL")?,
            idp_cert_pem: non_empty_env("AERO__SAML__IDP_CERT_PEM")?,
        })
    }
}

/// Read an env var, treating "missing" and "present but blank/whitespace" alike.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Load config or return the standard "not configured" client error.
fn require_config() -> Result<SamlConfig, AeroError> {
    SamlConfig::from_env().ok_or_else(|| AeroError::Invalid("saml not configured".into()))
}

// ---------------------------------------------------------------------------
// GET /saml/metadata — SP metadata XML
// ---------------------------------------------------------------------------

/// Render this SP's SAML 2.0 metadata document.
///
/// Advertises our entity id, the ACS endpoint with the **HTTP-POST binding**
/// (how the IdP returns the `SAMLResponse`), and that we want signed assertions
/// (`WantAssertionsSigned="true"` — consistent with the fail-closed posture).
#[must_use]
pub fn build_sp_metadata(cfg: &SamlConfig) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<EntityDescriptor xmlns="urn:oasis:names:tc:SAML:2.0:metadata" entityID="{entity}">
  <SPSSODescriptor AuthnRequestsSigned="false" WantAssertionsSigned="true" protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol">
    <NameIDFormat>urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress</NameIDFormat>
    <NameIDFormat>urn:oasis:names:tc:SAML:2.0:nameid-format:persistent</NameIDFormat>
    <AssertionConsumerService Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST" Location="{acs}" index="0" isDefault="true"/>
  </SPSSODescriptor>
</EntityDescriptor>"#,
        entity = xml_escape(&cfg.sp_entity_id),
        acs = xml_escape(&cfg.acs_url),
    )
}

/// `GET /saml/metadata` — return the SP metadata XML for IdP registration.
async fn metadata() -> Result<Response, crate::error::ApiError> {
    let cfg = require_config()?;
    let body = build_sp_metadata(&cfg);
    Ok((
        [(header::CONTENT_TYPE, "application/samlmetadata+xml")],
        body,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// GET /saml/login — AuthnRequest via HTTP-Redirect binding
// ---------------------------------------------------------------------------

/// Build a minimal SAML 2.0 `<AuthnRequest>` XML document.
///
/// `id` must be an XML-id (start with a letter; we prefix `_`). `issue_instant`
/// is RFC-3339 UTC. The request is unsigned (`AuthnRequestsSigned="false"` in our
/// metadata); the IdP authenticates the *user*, and we trust the *response*
/// signature, not the request.
#[must_use]
pub fn build_authn_request(cfg: &SamlConfig, id: &str, issue_instant: &str) -> String {
    format!(
        r#"<samlp:AuthnRequest xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" ID="{id}" Version="2.0" IssueInstant="{instant}" Destination="{dest}" ProtocolBinding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST" AssertionConsumerServiceURL="{acs}"><saml:Issuer>{issuer}</saml:Issuer><samlp:NameIDPolicy Format="urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress" AllowCreate="true"/></samlp:AuthnRequest>"#,
        id = xml_escape(id),
        instant = xml_escape(issue_instant),
        dest = xml_escape(&cfg.idp_sso_url),
        acs = xml_escape(&cfg.acs_url),
        issuer = xml_escape(&cfg.sp_entity_id),
    )
}

/// Encode an `AuthnRequest` XML for the **HTTP-Redirect binding**: raw DEFLATE
/// (RFC 1951, no zlib header) → base64 → percent-encode as the `SAMLRequest`
/// query parameter. Returns the full redirect URL to the IdP.
///
/// # Errors
/// Returns an error only if DEFLATE compression fails (effectively never for the
/// small, well-formed XML we generate).
pub fn redirect_url_for_authn_request(cfg: &SamlConfig, xml: &str) -> Result<String, AeroError> {
    // Raw DEFLATE (no zlib/gzip wrapper) per the SAML HTTP-Redirect binding.
    let mut enc =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(xml.as_bytes())
        .and_then(|()| enc.flush())
        .map_err(|e| AeroError::Internal(anyhow::anyhow!("saml deflate: {e}")))?;
    let deflated = enc
        .finish()
        .map_err(|e| AeroError::Internal(anyhow::anyhow!("saml deflate: {e}")))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(deflated);

    // Append SAMLRequest to the IdP SSO URL, preserving any existing query.
    let sep = if cfg.idp_sso_url.contains('?') { '&' } else { '?' };
    let qs = form_urlencoded::Serializer::new(String::new())
        .append_pair("SAMLRequest", &b64)
        .finish();
    Ok(format!("{}{sep}{qs}", cfg.idp_sso_url))
}

/// `GET /saml/login` — begin SP-initiated SSO: 302 to the IdP's SSO URL carrying
/// a freshly minted `AuthnRequest` (HTTP-Redirect binding).
async fn login() -> Result<Response, crate::error::ApiError> {
    let cfg = require_config()?;
    // XML-id: must start with a non-digit; ULID is base32 (may start with a digit)
    // so we prefix `_`. Time-ordered + random → a fine request id / replay guard.
    let id = format!("_{}", ulid::Ulid::new());
    let issue_instant = now_rfc3339();
    let xml = build_authn_request(&cfg, &id, &issue_instant);
    let url = redirect_url_for_authn_request(&cfg, &xml)?;
    Ok(Redirect::to(&url).into_response())
}

// ---------------------------------------------------------------------------
// POST /saml/acs — Assertion Consumer Service
// ---------------------------------------------------------------------------

/// Form body the IdP POSTs to the ACS (HTTP-POST binding).
#[derive(Deserialize)]
struct AcsForm {
    /// Base64-encoded `<samlp:Response>` XML.
    #[serde(rename = "SAMLResponse")]
    saml_response: String,
    /// Opaque relay state the SP set on the AuthnRequest (echoed back). Unused
    /// here beyond acceptance; carried for binding compliance.
    #[serde(rename = "RelayState", default)]
    _relay_state: Option<String>,
}

/// Identity fields extracted from a `<samlp:Response>` assertion.
///
/// Produced by [`extract_assertion`]. NOTE: extraction is **not** verification —
/// these fields are only trustworthy *after* the XML signature has been verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamlAssertion {
    /// The response `<Issuer>` — must equal the configured IdP entity id.
    pub issuer: String,
    /// The subject `<NameID>` — the stable per-IdP user identifier.
    pub name_id: String,
    /// Released attributes (`<Attribute Name=…><AttributeValue>…`), first value
    /// per name. Common keys: `email`, `displayName`, `name`.
    pub attributes: Vec<(String, String)>,
}

impl SamlAssertion {
    /// Look up the first attribute value matching any of `names` (case-sensitive,
    /// in order), trimming whitespace.
    #[must_use]
    pub fn attr_any(&self, names: &[&str]) -> Option<String> {
        for n in names {
            if let Some((_, v)) = self.attributes.iter().find(|(k, _)| k == n) {
                let t = v.trim();
                if !t.is_empty() {
                    return Some(t.to_owned());
                }
            }
        }
        None
    }

    /// Email, drawn from common attribute names, falling back to an email-shaped
    /// `NameID`.
    #[must_use]
    pub fn email(&self) -> Option<String> {
        self.attr_any(&[
            "email",
            "mail",
            "urn:oid:0.9.2342.19200300.100.1.3",
            "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress",
        ])
        .or_else(|| {
            let n = self.name_id.trim();
            (n.contains('@')).then(|| n.to_owned())
        })
    }

    /// Best display name: explicit display-name attribute, else email local-part,
    /// else the `NameID`. Always non-empty.
    #[must_use]
    pub fn best_display_name(&self) -> String {
        if let Some(n) = self.attr_any(&[
            "displayName",
            "name",
            "cn",
            "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/name",
        ]) {
            return n;
        }
        if let Some(local) = self
            .email()
            .as_deref()
            .and_then(|e| e.split('@').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return local.to_owned();
        }
        let n = self.name_id.trim();
        if n.is_empty() {
            "saml-user".to_owned()
        } else {
            n.to_owned()
        }
    }
}

/// Decode a base64 `SAMLResponse` body to its XML string.
///
/// Accepts standard base64 (the HTTP-POST binding uses standard, *not* url-safe);
/// whitespace/newlines IdPs sometimes insert are stripped first.
///
/// # Errors
/// [`AeroError::Invalid`] on non-base64 input or non-UTF-8 decoded bytes.
pub fn decode_saml_response(b64: &str) -> Result<String, AeroError> {
    let compact: String = b64.split_whitespace().collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(compact.as_bytes())
        .map_err(|e| AeroError::Invalid(format!("saml: base64 decode: {e}")))?;
    String::from_utf8(bytes).map_err(|e| AeroError::Invalid(format!("saml: utf8: {e}")))
}

/// Extract identity fields from a decoded `<samlp:Response>` XML string.
///
/// This is a **lightweight, namespace-agnostic** text scan — *not* a validating
/// XML parser and *not* a signature check. It exists so the post-verification
/// JIT path has data to consume the moment verification is wired; its output is
/// untrusted until [`verify_response_signature`] passes.
///
/// # Errors
/// [`AeroError::Invalid`] if no `<NameID>` can be located (a structurally
/// unusable response).
pub fn extract_assertion(xml: &str) -> Result<SamlAssertion, AeroError> {
    let issuer = first_element_text(xml, "Issuer").unwrap_or_default();
    let name_id = first_element_text(xml, "NameID")
        .ok_or_else(|| AeroError::Invalid("saml: response has no NameID".into()))?;
    let attributes = extract_attributes(xml);
    Ok(SamlAssertion {
        issuer: issuer.trim().to_owned(),
        name_id: name_id.trim().to_owned(),
        attributes,
    })
}

/// Opt-in env switch that enables the **experimental** `bergshamra`-backed XML-DSig
/// verifier. When unset / not truthy, [`verify_response_signature`] stays
/// **fail-closed** (rejects every assertion), preserving the default posture.
///
/// Accepted truthy values (case-insensitive): `1`, `true`, `yes`, `on`.
const EXPERIMENTAL_VERIFY_ENV: &str = "AERO_SAML_EXPERIMENTAL_VERIFY";

/// ⚠️⚠️⚠️ **SECURITY CAVEAT — READ BEFORE ENABLING** ⚠️⚠️⚠️
///
/// The real-verification path below is built on **`bergshamra`** (0.5.x), a
/// **pre-1.0, NOT third-party-audited** pure-Rust XML-DSig implementation. A
/// canonicalization, digest, or signature-wrapping bug in it is a *silent
/// authentication bypass* (forged assertions accepted). It is shipped here behind
/// an explicit opt-in so an operator must *consciously* choose to trust it.
///
/// **Before enabling [`EXPERIMENTAL_VERIFY_ENV`] in production: perform your own
/// security review of `bergshamra`.** For a hardened deployment, prefer the
/// audited C path — `xmlsec1` + `samael` — over this experimental verifier.
const BERGSHAMRA_SECURITY_CAVEAT: &str = "\
EXPERIMENTAL: SAML signatures verified by bergshamra, a pre-1.0, un-audited \
pure-Rust XML-DSig library. Security-review it yourself before production; \
prefer audited xmlsec1+samael. Enabled via AERO_SAML_EXPERIMENTAL_VERIFY.";

/// Is the opt-in experimental verifier turned on for this process?
fn experimental_verify_enabled() -> bool {
    std::env::var(EXPERIMENTAL_VERIFY_ENV)
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

/// XML-signature verification gate for the IdP's `<samlp:Response>`.
///
/// # Default posture: **FAIL-CLOSED**
///
/// Unless the operator explicitly sets [`EXPERIMENTAL_VERIFY_ENV`]
/// (`AERO_SAML_EXPERIMENTAL_VERIFY=1`), this **always rejects** — exactly as
/// before — so no assertion is ever trusted by default.
///
/// # Opt-in: experimental `bergshamra` verifier
///
/// ⚠️ **SECURITY CAVEAT** ⚠️ When the env switch is on, signatures are checked by
/// [`bergshamra`] — a **pre-1.0, NOT third-party-audited** pure-Rust XML-DSig
/// library (`#![forbid(unsafe_code)]`, no system C deps). A C14N / digest / XSW
/// bug in it is a *silent auth bypass*. **Security-review it yourself before
/// enabling in production; prefer audited `xmlsec1` + `samael`.** See
/// [`BERGSHAMRA_SECURITY_CAVEAT`].
///
/// When enabled, the full XML-DSig verification sequence runs:
/// 1. Load `cfg.idp_cert_pem` (the trusted IdP signing cert) into a
///    [`KeysManager`](bergshamra::keys::KeysManager).
/// 2. [`DsigContext::new`](bergshamra::dsig::DsigContext) → hardened defaults:
///    `trusted_keys_only = true` (ignore attacker-supplied inline `<KeyInfo>`
///    keys), `strict_verification = true` (positional XSW guard),
///    `hmac_min_out_len = 160`.
/// 3. [`bergshamra::dsig::verify::verify`]: locate `<Signature>`/`<SignedInfo>`,
///    apply the declared (exclusive) C14N, verify each `<Reference>` digest, then
///    verify `<SignatureValue>` (RSA-SHA256) against the trusted cert. Duplicate
///    ID rejection is unconditional in bergshamra (XSW hardening).
/// 4. **XSW invariant enforced here** ([`enforce_signed_assertion`]): the response
///    must contain *exactly one* `<Assertion>`, and a *verified* `<Reference>`
///    must resolve to that very element — so the bytes we consume are the bytes
///    that were signed.
///
/// # Errors
/// [`AeroError::Unauthorized`] when the switch is off (fail-closed), when the
/// signature is missing/invalid, or when the XSW invariant is violated.
pub fn verify_response_signature(cfg: &SamlConfig, xml: &str) -> Result<(), AeroError> {
    // SECURITY: do NOT replace the fail-closed default with `Ok(())`. Off by
    // default; the bergshamra path below only runs when an operator opts in.
    if !experimental_verify_enabled() {
        return Err(AeroError::Unauthorized(
            "saml: signature validation not yet wired (fail-closed: no vetted XML-DSig verifier in this build; set AERO_SAML_EXPERIMENTAL_VERIFY=1 to enable the experimental, UN-AUDITED bergshamra verifier, or install xmlsec1 + enable samael to go live)".into(),
        ));
    }

    // ⚠️ SECURITY CAVEAT ⚠️ — everything past here trusts `bergshamra`, a pre-1.0,
    // un-audited pure-Rust XML-DSig library. Security-review it before production;
    // prefer audited xmlsec1+samael. (BERGSHAMRA_SECURITY_CAVEAT)
    let _ = BERGSHAMRA_SECURITY_CAVEAT; // referenced so the caveat const can't drift unused
    bergshamra_verify(cfg, xml)
}

/// The experimental real-verification body. Split out so the gate above reads as
/// a pure policy decision and this reads as the crypto sequence.
///
/// ⚠️ Trusts the un-audited `bergshamra` crate — see [`BERGSHAMRA_SECURITY_CAVEAT`].
fn bergshamra_verify(cfg: &SamlConfig, xml: &str) -> Result<(), AeroError> {
    use bergshamra::dsig::{verify::verify, DsigContext, VerifyResult};
    use bergshamra::keys::{loader::load_x509_cert_pem, KeysManager};

    // 1. Load the *trusted* IdP signing certificate. This is the only key the
    //    verifier will accept (trusted_keys_only); a cert embedded by an attacker
    //    in the response's <KeyInfo> is ignored.
    let idp_key = load_x509_cert_pem(cfg.idp_cert_pem.as_bytes()).map_err(|e| {
        AeroError::Unauthorized(format!("saml: cannot load configured IdP certificate: {e}"))
    })?;
    let mut km = KeysManager::new();
    km.add_key(idp_key);

    // 2. Hardened context: trusted_keys_only + strict_verification + HMAC floor.
    let ctx = DsigContext::new(km);

    // 3. Full XML-DSig verify sequence (C14N → Reference digests → SignatureValue).
    let result = verify(&ctx, xml).map_err(|e| {
        // A structural/parse/crypto error is a *rejection*, never a pass.
        AeroError::Unauthorized(format!("saml: signature verification error: {e}"))
    })?;

    let references = match result {
        VerifyResult::Valid { references, .. } => references,
        VerifyResult::Invalid { reason } => {
            return Err(AeroError::Unauthorized(format!(
                "saml: signature invalid: {reason}"
            )));
        }
    };

    // 4. XSW invariant: a *verified* reference must cover the single <Assertion>
    //    we will consume. Without this, a passing signature over some *other*
    //    element (a wrapped/decoy assertion, the Response shell, …) would be
    //    accepted while we read an unsigned assertion.
    enforce_signed_assertion(xml, &references)
}

/// Signature-Wrapping (XSW) defence, enforced by the *consumer*.
///
/// bergshamra already rejects duplicate IDs unconditionally and (in strict mode)
/// constrains reference *positions*; this adds the SAML-specific binding the
/// library docs explicitly delegate to the caller: the verified signature must
/// cover **the exact `<Assertion>` we extract identity from**.
///
/// Rules (all must hold):
/// * the response contains **exactly one** `<Assertion>` element (reject 0, and
///   reject multiple — the classic XSW shape of one signed decoy + one unsigned
///   real assertion), and
/// * at least one **digest-verified** `<Reference>` resolved to that very node.
///
/// ⚠️ Uses the un-audited `bergshamra`/`uppsala` parser — see [`BERGSHAMRA_SECURITY_CAVEAT`].
fn enforce_signed_assertion(
    xml: &str,
    references: &[bergshamra::dsig::VerifiedReference],
) -> Result<(), AeroError> {
    let doc = bergshamra::xml::uppsala::parse(xml)
        .map_err(|e| AeroError::Unauthorized(format!("saml: cannot reparse response: {e}")))?;

    // Exactly one <Assertion> (local name, namespace-agnostic). 0 → nothing to
    // trust; >1 → ambiguous, the wrapping attack surface — reject both.
    let assertions = doc.get_elements_by_tag_name("Assertion");
    let assertion_node = match assertions.as_slice() {
        [one] => *one,
        [] => {
            return Err(AeroError::Unauthorized(
                "saml: response has no <Assertion> to bind the signature to".into(),
            ))
        }
        _ => {
            return Err(AeroError::Unauthorized(format!(
                "saml: response has {} <Assertion> elements; signature-wrapping refused (expected exactly one)",
                assertions.len()
            )))
        }
    };

    // A *digest-verified* reference must resolve to that single Assertion node.
    let covered = references.iter().any(|r| {
        r.digest_verified && r.resolved_node.is_some_and(|n| n == assertion_node)
    });
    if !covered {
        return Err(AeroError::Unauthorized(
            "saml: the verified signature does not cover the consumed <Assertion> (signature-wrapping refused)".into(),
        ));
    }
    Ok(())
}

/// `POST /saml/acs` — consume the IdP's `SAMLResponse`.
///
/// Flow: decode base64 → **verify XML signature** → extract `NameID`/attributes
/// → check issuer → JIT-provision → mint our session tokens.
/// [`verify_response_signature`] is **fail-closed by default** (returns `401` for
/// every assertion) unless the operator opts into the experimental, un-audited
/// `bergshamra` verifier via `AERO_SAML_EXPERIMENTAL_VERIFY=1`. The provisioning
/// code below the gate runs only on a *verified* assertion.
async fn acs(
    State(s): State<AppState>,
    headers: axum::http::HeaderMap,
    Form(form): Form<AcsForm>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = require_config()?;

    // 1. Decode the base64 SAMLResponse to XML.
    let xml = decode_saml_response(&form.saml_response)?;

    // 2. SECURITY GATE — verify the assertion's XML signature. FAIL-CLOSED by
    //    default (rejects every assertion); only the opt-in, un-audited bergshamra
    //    verifier (AERO_SAML_EXPERIMENTAL_VERIFY=1) lets a *signature-verified*
    //    assertion through. Nothing below runs on an unverified assertion.
    verify_response_signature(&cfg, &xml)?;

    // ---- Everything below runs ONLY on a verified assertion. With the verifier
    // ---- off (default) it is an unreachable staging seam; it is also exercised
    // ---- directly by unit tests.

    // 3. Extract identity and bind it to the configured IdP.
    let assertion = extract_assertion(&xml)?;
    if assertion.issuer != cfg.idp_entity_id {
        return Err(AeroError::Unauthorized(format!(
            "saml: response issuer {:?} does not match configured IdP",
            assertion.issuer
        ))
        .into());
    }

    // 4. Resolve the external identity → internal participant (JIT on first sight).
    //    Keyed on (IdP entity id, NameID), mirroring the OIDC (issuer, sub) key.
    let sso = SsoRepo::new(s.participants.pool().clone());
    let participant_id = match sso
        .find_participant(&cfg.idp_entity_id, &assertion.name_id)
        .await
        .map_err(AeroError::from)?
    {
        Some(pid) => pid,
        None => jit_provision(&s, &sso, &cfg, &assertion).await?,
    };

    // 5. Mint OUR tokens + return the standard envelope (same shape as OIDC SSO).
    let tokens = s.auth.issue_for_participant(participant_id)?;
    let participant = s
        .participants
        .get(participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;
    // Best-effort session record keyed on the refresh-token hash (mirrors OIDC).
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let hash = aero_storage::revoked_token::hash_token(&tokens.refresh_token);
    if let Err(e) = aero_storage::SessionRepo::new(s.participants.pool().clone())
        .record(participant_id, &hash, ua)
        .await
    {
        tracing::warn!(error = ?e, %participant_id, "saml acs session record failed");
    }

    Ok(Json(serde_json::json!({
        "access_token": tokens.access_token,
        "refresh_token": tokens.refresh_token,
        "participant": participant,
    })))
}

/// JIT-provision a participant for a first-seen SAML identity. Mirrors
/// [`crate::sso::jit_provision`]: create a credential-less human participant,
/// enroll it into the default workspace, link the external identity.
async fn jit_provision(
    s: &AppState,
    sso: &SsoRepo,
    cfg: &SamlConfig,
    assertion: &SamlAssertion,
) -> Result<ParticipantId, AeroError> {
    let display_name = assertion.best_display_name();
    let participant = s
        .participants
        .create_bot(
            &display_name,
            aero_common::ParticipantKind::Human,
            None,
            None,
        )
        .await
        .map_err(AeroError::from)?;
    s.workspaces
        .add_member(DEFAULT_WORKSPACE_ID, participant.id, WorkspaceRole::Member)
        .await
        .map_err(AeroError::from)?;
    sso.link(
        &cfg.idp_entity_id,
        &assertion.name_id,
        participant.id,
        assertion.email().as_deref(),
    )
    .await
    .map_err(AeroError::from)?;
    Ok(participant.id)
}

// ---------------------------------------------------------------------------
// Small XML helpers (intentionally dependency-free; only ever feed UNTRUSTED
// extraction, never signature verification).
// ---------------------------------------------------------------------------

/// Escape the five XML predefined entities for safe interpolation into our
/// generated documents (metadata / AuthnRequest).
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Decode the five XML predefined entities and the common numeric forms in
/// extracted text. Best-effort for the untrusted extraction path.
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#x2F;", "/")
        .replace("&#47;", "/")
        // amp last so we don't double-decode (e.g. "&amp;lt;").
        .replace("&amp;", "&")
}

/// Local name of an element start-tag token (strips any `ns:` prefix), e.g.
/// `"saml:NameID"` → `"NameID"`. The token is the text after `<` up to the first
/// whitespace, `/`, or `>`.
fn local_name(token: &str) -> &str {
    let name = token
        .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .next()
        .unwrap_or("");
    name.rsplit(':').next().unwrap_or(name)
}

/// Return the text content of the first element whose *local* name is `local`
/// (namespace-prefix-insensitive). Returns the inner text with XML entities
/// decoded, or `None` if no such (non-self-closing) element exists.
fn first_element_text(xml: &str, local: &str) -> Option<String> {
    // `pos` is an ABSOLUTE byte offset into `xml`, advanced past each `<…>` token.
    let mut pos = 0usize;
    while let Some(rel) = xml[pos..].find('<') {
        let lt = pos + rel; // absolute index of this `<`
        let after = &xml[lt + 1..];
        // Skip closing tags, comments/CDATA/doctype, declarations/PIs (`?xml`).
        if after.starts_with('/') || after.starts_with('!') || after.starts_with('?') {
            pos = lt + 1;
            continue;
        }
        let Some(gt_rel) = after.find('>') else { break };
        let tag = &after[..gt_rel]; // start-tag inner, e.g. `saml:NameID Format="..."`
        let self_closing = tag.ends_with('/');
        let content_start = lt + 1 + gt_rel + 1; // absolute index just past `>`
        if local_name(tag) == local && !self_closing {
            // Inner text runs from just past this start-tag's `>` to the next `<`
            // — sufficient for the leaf text elements we read (NameID / Issuer /
            // AttributeValue).
            let inner = &xml[content_start..];
            let end = inner.find('<').unwrap_or(inner.len());
            return Some(xml_unescape(inner[..end].trim()));
        }
        pos = content_start;
    }
    None
}

/// Extract `(name, first-value)` pairs from `<Attribute Name="…">` elements.
/// Namespace-prefix-insensitive; reads the first `<AttributeValue>` per attribute.
fn extract_attributes(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = xml[search_from..].find('<') {
        let lt = search_from + rel;
        let after = &xml[lt + 1..];
        let Some(gt) = after.find('>') else { break };
        let tag = &after[..gt];
        search_from = lt + 1 + gt + 1;
        if local_name(tag) != "Attribute" || tag.ends_with('/') {
            continue;
        }
        // Pull the Name="..." attribute out of the start-tag.
        let Some(name) = tag_attr_value(tag, "Name") else { continue };
        // Find the first <AttributeValue> after this start-tag.
        let region = &xml[search_from..];
        if let Some(val) = first_element_text(region, "AttributeValue") {
            out.push((name, val));
        }
    }
    out
}

/// Extract the value of XML attribute `attr` from a start-tag inner string, e.g.
/// `tag_attr_value(r#"Attribute Name="email""#, "Name")` → `Some("email")`.
/// Handles either quote style; whitespace-tolerant around `=`.
fn tag_attr_value(tag: &str, attr: &str) -> Option<String> {
    let mut hay = tag;
    while let Some(pos) = hay.find(attr) {
        let before_ok = pos == 0
            || hay[..pos]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace());
        let after = hay[pos + attr.len()..].trim_start();
        if before_ok && after.starts_with('=') {
            let after_eq = after[1..].trim_start();
            let quote = after_eq.chars().next()?;
            if quote == '"' || quote == '\'' {
                let v = &after_eq[1..];
                if let Some(end) = v.find(quote) {
                    return Some(xml_unescape(&v[..end]));
                }
            }
        }
        hay = &hay[pos + attr.len()..];
    }
    None
}

/// Current UTC instant formatted as SAML expects (RFC 3339, second precision,
/// `Z` suffix), e.g. `2026-06-19T12:34:56Z`.
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests;
