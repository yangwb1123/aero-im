// ----- Auth -----

/// Best-effort: record an active login session keyed on the refresh token's hash
/// (so it lines up with the revoked-token check), pulling the `User-Agent` from
/// request headers when present. A failure here must NOT fail the login /
/// registration, so any error is logged and swallowed. Wave 21.
async fn record_session(
    s: &AppState,
    participant: ParticipantId,
    refresh_token: &str,
    headers: &header::HeaderMap,
) {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let hash = aero_storage::revoked_token::hash_token(refresh_token);
    if let Err(e) = aero_storage::SessionRepo::new(s.pg.clone())
        .record(participant, &hash, ua)
        .await
    {
        tracing::warn!(error = ?e, %participant, "auth session record failed");
    }
}

/// The client's source IP, read from the standard reverse-proxy forwarding
/// headers (`X-Forwarded-For`'s first hop, then `X-Real-IP`). Returns `None` when
/// neither is present (e.g. a direct connection in dev) — callers treat an absent
/// IP as "unobservable", never as a security signal.
///
/// TRUST ASSUMPTION: this value is only trustworthy when a trusted reverse proxy
/// **overwrites** `X-Forwarded-For` with the real client address (the standard
/// cloud-LB / nginx `proxy_set_header` setup). A client can forge the header, so
/// the IP fed to the new-login-IP signal is *defence-in-depth*, not an authz
/// input: a forged-known-IP can only suppress a new-IP alert (a false negative),
/// never grant access. Deploy behind a header-rewriting proxy for the signal to
/// be reliable.
fn client_ip(headers: &header::HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        // `X-Forwarded-For: client, proxy1, proxy2` — the client is the first hop.
        .and_then(|s| s.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            headers.get("x-real-ip").and_then(|v| v.to_str().ok()).map(str::trim)
        })
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
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
) {
    let ip = client_ip(headers);
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let repo = aero_storage::LoginEventRepo::new(s.pg.clone());

    // Flag a login from a new IP — but only for an account that already has
    // history (every IP is "new" on the first-ever login).
    match (repo.is_known_ip(participant, ip.as_deref()).await, repo.has_any(participant).await) {
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
    let out = s.auth.register(req).await?;
    // Enroll the brand-new participant into the legacy/default workspace so they
    // immediately belong to a tenant — otherwise they could not create rooms
    // (`create_room_in_workspace` requires workspace membership). `add_member` is
    // an idempotent `ON CONFLICT DO NOTHING` upsert, so a retry is harmless. We
    // propagate failures (rather than swallowing) to preserve the invariant
    // "registered ⇒ workspace member"; the default workspace is guaranteed to
    // exist by migration 0006's backfill.
    s.workspaces
        .add_member(DEFAULT_WORKSPACE_ID, out.participant.id, WorkspaceRole::Member)
        .await
        .map_err(AeroError::from)?;
    // Onboarding: auto-join the new participant into the default workspace's
    // default channels (Wave 12). Best-effort — never fails registration.
    crate::default_channels::auto_join_defaults(&s, DEFAULT_WORKSPACE_ID, out.participant.id).await;
    // Wave 21: record the active session (best-effort; never fails registration).
    record_session(&s, out.participant.id, &out.refresh_token, &headers).await;
    Ok(Json(serde_json::json!({
        "access_token": out.access_token,
        "refresh_token": out.refresh_token,
        "participant": out.participant,
    })))
}

/// Login request — email + password, plus an optional `totp` code that is
/// *required* when the account has activated two-factor auth (Wave 14).
#[derive(Deserialize)]
struct LoginReq {
    email: String,
    password: String,
    #[serde(default)]
    totp: Option<String>,
}

async fn auth_login(
    State(s): State<AppState>,
    headers: header::HeaderMap,
    Json(req): Json<LoginReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let email = req.email.trim().to_lowercase();
    let out = match s
        .auth
        .login(LoginRequest { email: email.clone(), password: req.password })
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
            let ip = client_ip(&headers);
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
    let totp = aero_storage::TotpRepo::new(s.pg.clone());
    if totp.is_activated(out.participant.id).await.map_err(AeroError::from)? {
        let secret = totp
            .get_secret(out.participant.id)
            .await
            .map_err(AeroError::from)?
            .ok_or_else(|| AeroError::Internal(anyhow::anyhow!("2FA activated without a secret")))?;
        let now = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0);
        let code = req.totp.as_deref().unwrap_or("");
        if !aero_auth::totp::verify(&secret, code, now) {
            // A wrong/missing second factor is a FAILED login: count it on the
            // lockout throttle (the password success was deferred, not recorded) so
            // the code can't be brute-forced, and leave the same durable trail a
            // bad password leaves. Both best-effort — never block the 401 response.
            s.auth.finalize_login(&email, false).await;
            let ip = client_ip(&headers);
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
    // Wave 21: record the active session (best-effort; never fails login). Done
    // only after 2FA passes, so a half-completed login leaves no session row.
    record_session(&s, out.participant.id, &out.refresh_token, &headers).await;
    // ROADMAP5 方向五: record the login in the IP/device history + flag a new-IP
    // login (best-effort; never fails login). The audit event is scoped to the
    // default workspace (login is workspace-agnostic — a participant can belong to
    // several; the all-zero default is where account-level security events land).
    record_login_event(&s, out.participant.id, Some(DEFAULT_WORKSPACE_ID), &headers).await;
    Ok(Json(serde_json::json!({
        "access_token": out.access_token,
        "refresh_token": out.refresh_token,
        "participant": out.participant,
    })))
}

/// `GET /api/auth/login-history` — the caller's recent successful logins (IP +
/// user-agent + time), newest first, for a "recent login activity" view
/// (ROADMAP5 方向五). Owner-scoped: a participant only ever sees their own.
async fn auth_login_history(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
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
    let url = req.avatar_url.map(|inner| inner.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()));
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

