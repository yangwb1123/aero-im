//! Two-factor authentication (TOTP, RFC 6238) self-management.
//!
//! Lets the authenticated participant manage their own authenticator-app 2FA:
//! enroll (mint a secret + `otpauth://` URI), verify (prove possession of the app
//! and activate), check status, and disable. The TOTP crypto lives in
//! [`aero_auth::totp`] and the secret/activation state in
//! [`TotpRepo`](aero_storage::TotpRepo); these handlers are the thin HTTP seam
//! between them.
//!
//! The login-time *enforcement* (require a valid code when 2FA is activated) is
//! NOT here — it is wired into the auth path by the orchestrator, which calls
//! [`TotpRepo::is_activated`](aero_storage::TotpRepo::is_activated) +
//! [`TotpRepo::get_secret`](aero_storage::TotpRepo::get_secret) +
//! [`aero_auth::totp::verify`]. Mounted via [`routes`] and `.merge`d into the
//! main router. Every route is `AuthUser`-gated and scoped to the caller, so a
//! participant can only ever manage their own 2FA.

use aero_auth::AuthUser;
use aero_common::Error as AeroError;
use aero_storage::{RecoveryCodeRepo, TotpRepo, TotpWriteError};
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All 2FA routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/2fa", get(status_2fa).delete(disable_2fa))
        .route("/api/me/2fa/enroll", post(enroll_2fa))
        .route("/api/me/2fa/verify", post(verify_2fa))
        // Recovery-code login is mounted beside /api/auth/login so both entry
        // points share the same password, lockout and session pipeline.
        .route("/api/me/2fa/recovery-codes", post(generate_recovery_codes))
}

/// Build a [`RecoveryCodeRepo`] from shared state.
fn recovery_repo(s: &AppState) -> RecoveryCodeRepo {
    RecoveryCodeRepo::new(s.pg.clone())
}

/// Issuer label embedded in the `otpauth://` URI (shown by authenticator apps).
const ISSUER: &str = "AeroIM";

/// Build a [`TotpRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> TotpRepo {
    TotpRepo::new(s.pg.clone())
}

fn map_totp_write_error(error: TotpWriteError) -> AeroError {
    match error {
        TotpWriteError::ChannelOwnerProtected(_) => AeroError::Conflict(
            "transfer channel ownership before disabling two-factor authentication".into(),
        ),
        TotpWriteError::Storage(error) => AeroError::from(error),
    }
}

/// Current unix time in whole seconds, for TOTP step derivation. Clamped at 0 on
/// the (impossible) pre-epoch clock so the cast never wraps.
fn now_unix() -> u64 {
    u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0)
}

/// Minimal RFC 3986 percent-encoding for the `otpauth` account label, so a
/// display name with spaces or reserved characters yields a well-formed URI.
/// Keeps unreserved characters (`A-Z a-z 0-9 - . _ ~`) verbatim.
fn encode_label(label: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(label.len());
    for b in label.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

#[derive(Deserialize)]
struct CodeReq {
    /// The current 6-digit code from the authenticator app.
    code: String,
}

/// `POST /api/me/2fa/enroll` — mint a fresh TOTP secret for the caller. Rejected
/// with `409` if 2FA is already activated (disable it first). Stores the secret
/// as a pending (unactivated) enrollment and returns it alongside an
/// `otpauth://` provisioning URI for the authenticator app to scan.
async fn enroll_2fa(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = repo(&s);
    if repo
        .is_activated(auth.participant_id)
        .await
        .map_err(AeroError::from)?
    {
        return Err(AeroError::Conflict("2fa already activated".into()).into());
    }
    // Resolve the display name for a human-readable authenticator-app label.
    let participant = s
        .participants
        .get(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;

    let secret = aero_auth::totp::generate_secret();
    // B5-1: enroll + governance pair (auth.totp.enroll /
    // admin.auth.totp.enroll — target = participant, detail
    // `{"stage":"enroll"}`) in ONE transaction; an upsert ALWAYS writes, so
    // the pair is unconditional. SAVEPOINT fail-open (R7).
    let mut tx = s.pg.begin().await.map_err(AeroError::from)?;
    TotpRepo::upsert_secret_in_tx(&mut tx, auth.participant_id, &secret)
        .await
        .map_err(map_totp_write_error)?;
    let _ = aero_storage::audit_governance::AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
        &mut tx,
        aero_common::WorkspaceId::nil(),
        Some(auth.participant_id),
        aero_storage::audit_governance::tokens::AUTH_TOTP_ENROLL,
        Some(&auth.participant_id.to_string()),
        serde_json::json!({ "stage": "enroll" }),
        aero_storage::audit_governance::tokens::OUTBOUND_AUTH_TOTP_ENROLL,
    )
    .await
    .map_err(AeroError::from)?; // Ok(None) = fail-open skip (DLQ row in-tx)
    tx.commit().await.map_err(AeroError::from)?;

    let label = encode_label(&participant.display_name);
    let otpauth_uri = format!(
        "otpauth://totp/{ISSUER}:{label}?secret={secret}&issuer={ISSUER}&digits=6&period=30"
    );
    Ok(Json(serde_json::json!({
        "secret": secret,
        "otpauth_uri": otpauth_uri,
    })))
}

/// `POST /api/me/2fa/verify` `{code}` — prove possession of the authenticator app
/// and activate the pending enrollment. `404` if the caller has not enrolled;
/// `400 "invalid code"` if the code does not verify against the stored secret.
/// On success the enrollment is activated and `{activated:true}` is returned.
async fn verify_2fa(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CodeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = repo(&s);
    let secret = repo
        .get_secret(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("2fa enrollment".into()))?;

    if !aero_auth::totp::verify(&secret, req.code.trim(), now_unix()) {
        return Err(AeroError::Invalid("invalid code".into()).into());
    }
    // Idempotent: `activate` is a no-op if it was already activated, so a repeated
    // verify still reports success. B5-1: activation + governance pair
    // (auth.totp.enroll / admin.auth.totp.enroll — detail
    // `{"stage":"activate"}`) in ONE transaction; the pair is emitted ONLY
    // when a row actually flipped (a no-op activation audits nothing).
    let mut tx = s.pg.begin().await.map_err(AeroError::from)?;
    let activated = TotpRepo::activate_in_tx(&mut tx, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if activated {
        let _ = aero_storage::audit_governance::AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
            &mut tx,
            aero_common::WorkspaceId::nil(),
            Some(auth.participant_id),
            aero_storage::audit_governance::tokens::AUTH_TOTP_ENROLL,
            Some(&auth.participant_id.to_string()),
            serde_json::json!({ "stage": "activate" }),
            aero_storage::audit_governance::tokens::OUTBOUND_AUTH_TOTP_ENROLL,
        )
        .await
        .map_err(AeroError::from)?; // Ok(None) = fail-open skip (DLQ row in-tx)
    }
    tx.commit().await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "activated": true })))
}

/// `GET /api/me/2fa` — the caller's 2FA state: whether a secret is enrolled and
/// whether that enrollment has been activated.
async fn status_2fa(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = repo(&s);
    let enrolled = repo
        .get_secret(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .is_some();
    let activated = repo
        .is_activated(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "enrolled": enrolled,
        "activated": activated,
    })))
}

/// `DELETE /api/me/2fa` `{code}` — remove the caller's 2FA enrollment. When 2FA
/// is *activated*, a valid current code is required first (`400 "invalid code"`
/// otherwise), so a hijacked session cannot silently strip 2FA. A merely-pending
/// (never-activated) enrollment can be cleared without a code. Always reports
/// `{disabled:true}` once the enrollment is gone.
async fn disable_2fa(
    State(s): State<AppState>,
    auth: AuthUser,
    body: Option<Json<CodeReq>>,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = repo(&s);
    if repo
        .is_activated(auth.participant_id)
        .await
        .map_err(AeroError::from)?
    {
        let secret = repo
            .get_secret(auth.participant_id)
            .await
            .map_err(AeroError::from)?
            .ok_or_else(|| AeroError::NotFound("2fa enrollment".into()))?;
        let code = body.map(|Json(b)| b.code).unwrap_or_default();
        if !aero_auth::totp::verify(&secret, code.trim(), now_unix()) {
            return Err(AeroError::Invalid("invalid code".into()).into());
        }
    }
    // Revoke before removing TOTP. If the latter fails, losing the old backup
    // batch is safer than leaving codes that could reactivate on re-enrollment.
    recovery_repo(&s)
        .delete_all(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    repo.disable(auth.participant_id)
        .await
        .map_err(map_totp_write_error)?;
    Ok(Json(serde_json::json!({ "disabled": true })))
}

// ─────────────────────────────────────────────────── Recovery codes ──────────

/// `POST /api/me/2fa/recovery-codes` — generate a fresh batch of 8 one-time
/// recovery codes, replacing any existing unused ones.
///
/// Requires the caller's 2FA to be **activated** and a current TOTP code. This
/// prevents a stolen, still-valid access session from silently replacing every
/// recovery code. Returns the 8 plaintext codes once; the previous batch is
/// revoked atomically by [`RecoveryCodeRepo::generate`].
async fn generate_recovery_codes(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CodeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    // Only makes sense when 2FA is fully activated.
    let totp = repo(&s);
    if !totp
        .is_activated(auth.participant_id)
        .await
        .map_err(AeroError::from)?
    {
        return Err(AeroError::Conflict(
            "2fa must be activated before generating recovery codes".into(),
        )
        .into());
    }
    let secret = totp
        .get_secret(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("2fa enrollment".into()))?;
    if !aero_auth::totp::verify(&secret, req.code.trim(), now_unix()) {
        return Err(AeroError::Invalid("invalid code".into()).into());
    }

    let codes = recovery_repo(&s)
        .generate(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "codes": codes,
        "count": codes.len(),
    })))
}
