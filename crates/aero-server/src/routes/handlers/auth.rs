// ----- Auth -----

/// Record an active login session keyed on the refresh token's hash (so it lines
/// up with the revoked-token check), pulling the `User-Agent` from request
/// headers when present.
async fn record_session(
    s: &AppState,
    participant: ParticipantId,
    session_id: aero_common::SessionId,
    refresh_token: &str,
    headers: &header::HeaderMap,
) -> Result<(), sqlx::Error> {
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let hash = aero_storage::revoked_token::hash_token(refresh_token);
    aero_storage::SessionRepo::new(s.pg.clone())
        .record_with_id(participant, session_id, &hash, ua)
        .await?;
    Ok(())
}

/// Source address resolved by the edge middleware's trusted-proxy policy.
///
/// Never read forwarding headers directly here: doing so would let a direct
/// client suppress new-IP alerts with a forged `X-Forwarded-For` value.
fn client_ip(resolved: Option<crate::ip_allowlist::ResolvedClientIp>) -> Option<String> {
    resolved.map(|ip| ip.0.to_string())
}

/// Best-effort: record this successful login in the IP/device history and, when
/// it comes from an IP this account has never used before (and the account has
/// prior logins — so a first-ever login isn't flagged), emit a security warning +
/// audit event (ROADMAP5 方向五). The new-IP signal is the canonical account-
/// takeover tell; impossible-travel/geo-velocity builds on this same history but
/// needs a geo-IP database (a deployment seam). Never fails the login.
async fn record_login_event(
    s: &AppState,
    participant: ParticipantId,
    workspace: Option<aero_common::WorkspaceId>,
    headers: &header::HeaderMap,
    resolved_ip: Option<crate::ip_allowlist::ResolvedClientIp>,
) {
    let ip = client_ip(resolved_ip);
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let repo = aero_storage::LoginEventRepo::new(s.pg.clone());

    // Flag a login from a new IP — but only for an account that already has
    // history (every IP is "new" on the first-ever login).
    match (
        repo.is_known_ip(participant, ip.as_deref()).await,
        repo.has_any(participant).await,
    ) {
        (Ok(false), Ok(true)) => {
            tracing::warn!(%participant, ip = ip.as_deref().unwrap_or("?"), "login from a new IP");
            if let Some(ws) = workspace {
                let _ = aero_storage::AuditRepo::new(s.pg.clone())
                    .append(
                        ws,
                        Some(participant),
                        "auth.login.new_ip",
                        ip.as_deref(),
                        serde_json::json!({ "ip": ip, "user_agent": ua }),
                    )
                    .await;
            }
        }
        (Err(e), _) | (_, Err(e)) => {
            tracing::warn!(error = ?e, %participant, "new-IP login check failed");
        }
        _ => {}
    }

    if let Err(e) = repo.record(participant, ip.as_deref(), ua).await {
        tracing::warn!(error = ?e, %participant, "login event record failed");
    }
}

async fn auth_register(
    State(s): State<AppState>,
    headers: header::HeaderMap,
    Json(req): Json<RegisterRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user_agent = headers.get(header::USER_AGENT).and_then(|value| value.to_str().ok());
    // Account, credentials, default-tenant/channel memberships, and the initial
    // refresh-session inventory commit atomically. No token can escape for a
    // half-created account and a failed signup can be retried with the same email.
    let out = s.auth.register_enrolled(req, DEFAULT_WORKSPACE_ID, user_agent).await?;
    Ok(Json(serde_json::json!({
        "access_token": out.access_token,
        "refresh_token": out.refresh_token,
        "participant": out.participant,
    })))
}

/// Login request — email + password, plus either a current TOTP or a one-time
/// recovery code when the account has activated two-factor auth.
#[derive(Deserialize)]
struct LoginReq {
    email: String,
    password: String,
    #[serde(default)]
    totp: Option<String>,
    #[serde(default)]
    recovery_code: Option<String>,
}

async fn auth_login(
    State(s): State<AppState>,
    headers: header::HeaderMap,
    resolved_ip: Option<axum::Extension<crate::ip_allowlist::ResolvedClientIp>>,
    Json(req): Json<LoginReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let resolved_ip = resolved_ip.map(|axum::Extension(ip)| ip);
    let LoginReq {
        email,
        password,
        totp: submitted_totp,
        recovery_code,
    } = req;
    let email = email.trim().to_lowercase();
    let out = match s
        .auth
        .login(LoginRequest {
            email: email.clone(),
            password,
        })
        .await
    {
        Ok(u) => u,
        Err(e) => {
            // 方向五: record the failed attempt for anomaly detection. The
            // in-process LoginThrottle (per-node, restart-volatile, unqueryable)
            // already saw this failure; here we leave a *durable, queryable,
            // cross-node* trail (attempted account + source IP + user-agent) so a
            // slow credential-stuffing run that stays under the lockout threshold
            // is still detectable. Keyed on the *attempted* email (which may not
            // name a real account — enumeration is recorded too), not a
            // participant_id. Fail-OPEN: a recording error must never block the
            // login response, so we only warn.
            let ip = client_ip(resolved_ip);
            let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
            if let Err(rec_err) = aero_storage::LoginFailureRepo::new(s.pg.clone())
                .record(&email, ip.as_deref(), ua)
                .await
            {
                tracing::warn!(error = ?rec_err, "failed to record login failure");
            }
            return Err(e.into());
        }
    };
    // Two-factor enforcement (Wave 14): once a participant has ACTIVATED TOTP, a
    // valid current code must accompany the (already-verified) password. Returns
    // 401 `2fa_required` when the code is missing or wrong, so a stolen password
    // alone can't complete the login.
    let totp_repo = aero_storage::TotpRepo::new(s.pg.clone());
    let mut used_recovery_code = false;
    if totp_repo
        .is_activated(out.participant.id)
        .await
        .map_err(AeroError::from)?
    {
        let secret = totp_repo
            .get_secret(out.participant.id)
            .await
            .map_err(AeroError::from)?
            .ok_or_else(|| AeroError::Internal(anyhow::anyhow!("2FA activated without a secret")))?;
        let now = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0);
        let totp_code = submitted_totp.as_deref().map(str::trim).unwrap_or("");
        let totp_valid = aero_auth::totp::verify(&secret, totp_code, now);
        if !totp_valid {
            if let Some(code) = recovery_code.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
                used_recovery_code = aero_storage::RecoveryCodeRepo::new(s.pg.clone())
                    .verify(out.participant.id, code)
                    .await
                    .map_err(AeroError::from)?;
            }
        }
        if !totp_valid && !used_recovery_code {
            // A wrong/missing second factor is a FAILED login: count it on the
            // lockout throttle (the password success was deferred, not recorded) so
            // the code can't be brute-forced, and leave the same durable trail a
            // bad password leaves. Both best-effort — never block the 401 response.
            s.auth.finalize_login(&email, false).await;
            let ip = client_ip(resolved_ip);
            let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
            if let Err(rec_err) = aero_storage::LoginFailureRepo::new(s.pg.clone())
                .record(&email, ip.as_deref(), ua)
                .await
            {
                tracing::warn!(error = ?rec_err, "failed to record 2FA login failure");
            }
            return Err(AeroError::Unauthorized("2fa_required".into()).into());
        }
    }
    // All gates passed (password + any 2FA): NOW record the success, clearing the
    // per-account lockout counter (deferred from `auth.login` so a failed 2FA above
    // counted as a failure instead of resetting it).
    s.auth.finalize_login(&email, true).await;
    // A password login is safely retryable, so fail closed if its refresh
    // session cannot be recorded. Otherwise the token would escape global
    // revocation inventory and later fail refresh unexpectedly.
    record_session(&s, out.participant.id, out.session_id, &out.refresh_token, &headers)
        .await
        .map_err(AeroError::from)?;
    // ROADMAP5 方向五: record the login in the IP/device history + flag a new-IP
    // login (best-effort; never fails login). The audit event is scoped to the
    // default workspace (login is workspace-agnostic — a participant can belong to
    // several; the all-zero default is where account-level security events land).
    record_login_event(
        &s,
        out.participant.id,
        Some(DEFAULT_WORKSPACE_ID),
        &headers,
        resolved_ip,
    )
    .await;
    if used_recovery_code {
        if let Err(e) = aero_storage::AuditRepo::new(s.pg.clone())
            .append(
                DEFAULT_WORKSPACE_ID,
                Some(out.participant.id),
                "auth.login.recovery_code",
                None,
                serde_json::json!({}),
            )
            .await
        {
            tracing::warn!(error = ?e, participant = %out.participant.id, "recovery-code login audit failed");
        }
    }
    Ok(Json(serde_json::json!({
        "access_token": out.access_token,
        "refresh_token": out.refresh_token,
        "participant": out.participant,
    })))
}

/// `GET /api/auth/login-history` — the caller's recent successful logins (IP +
/// user-agent + time), newest first, for a "recent login activity" view
/// (ROADMAP5 方向五). Owner-scoped: a participant only ever sees their own.
async fn auth_login_history(State(s): State<AppState>, auth: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    let events = aero_storage::LoginEventRepo::new(s.pg.clone())
        .recent(auth.participant_id, 50)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "logins": events })))
}

async fn me(State(s): State<AppState>, auth: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    let p = s
        .participants
        .get(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;
    Ok(Json(serde_json::to_value(p).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct UpdateMeReq {
    #[serde(default)]
    display_name: Option<String>,
    /// Outer Option = field present; inner Option = nullable on the wire.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    avatar_url: Option<Option<String>>,
}

fn deserialize_optional_field<'de, D, T>(d: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

async fn update_me(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<UpdateMeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let name = req.display_name.as_deref().map(|n| n.trim()).filter(|n| !n.is_empty());
    if let Some(n) = name {
        if n.len() > 64 {
            return Err(AeroError::Invalid("display_name too long".into()).into());
        }
    }
    let url = req
        .avatar_url
        .map(|inner| inner.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()));
    let url_ref = url.as_ref().map(|inner| inner.as_deref());
    let updated = s
        .participants
        .update_profile(auth.participant_id, name, url_ref)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;
    // ROADMAP6 方向四: drop the just-updated profile from the per-process cache so
    // the next read re-fetches the new display_name/avatar instead of serving the
    // stale TTL window. (Other nodes still fall back to the 60s TTL — acceptable
    // for display names, as documented in `participant_cache`.)
    s.participant_cache.invalidate(&auth.participant_id);
    Ok(Json(serde_json::to_value(updated).map_err(AeroError::from)?))
}
