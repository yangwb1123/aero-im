//! Workspace IP / network allowlist (authorized networks) — admin management API.
//!
//! An admin (Owner/Admin) declares the CIDR ranges allowed to reach a workspace's
//! data ("authorized networks"). An empty allowlist means the feature is disabled
//! (allow-all), so existing workspaces keep working until an admin adds a range.
//!
//! Routes (all workspace-admin-gated):
//! * `GET    /api/workspaces/:id/ip-allowlist`         — list authorized networks
//! * `POST   /api/workspaces/:id/ip-allowlist` `{cidr, note?}` — add one
//! * `DELETE /api/workspaces/:id/ip-allowlist` `{cidr}`        — remove one
//!
//! Enforcement (matching a live request's client IP) is intentionally NOT wired
//! into the hot path here — see the crate-level notes. This module owns only the
//! storage + admin-management surface; the pure matcher
//! ([`aero_storage::ip_allowlist::is_allowed`]) is what an enforcement middleware
//! would call.

use std::net::IpAddr;
use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, WorkspaceId, WorkspaceRole};
use aero_storage::ip_allowlist::{ip_in_cidr, is_allowed};
use aero_storage::IpAllowlistRepo;
use axum::{
    extract::{ConnectInfo, Path, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// IP-allowlist routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/workspaces/:id/ip-allowlist",
        get(list_allowlist)
            .post(add_allowlist)
            .delete(remove_allowlist),
    )
}

/// Enforcement middleware (the half the module doc said was unwired): for
/// authenticated API requests, reject (403) a participant whose client IP does
/// not satisfy every configured workspace policy they are currently subject to.
/// This tenant-policy intersection deliberately covers room/message/resource
/// routes that do not carry a workspace id in the URL, and prevents a global
/// `/ws` or cross-workspace endpoint from leaking data from a restricted tenant.
/// Direct `/api/workspaces/{id}/...` paths retain a path-scoped check as a second
/// boundary. An EMPTY allowlist permits everyone.
///
/// Fail-OPEN on a DB error (a storage hiccup must not lock everyone out of their
/// workspace), matching the rate-limit / spam-guard availability stance; it only
/// blocks when it can positively confirm the IP is outside a non-empty allowlist.
pub async fn enforce_layer(
    State(s): State<AppState>,
    connect_info: Option<ConnectInfo<std::net::SocketAddr>>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    let client_ip = crate::rate_limit::client_ip(&headers, connect_info.map(|ci| ci.0));
    request.extensions_mut().insert(ResolvedClientIp(client_ip));
    let path = request.uri().path().to_owned();
    // Anti-lockout: NEVER enforce on the allowlist-management route itself, or an
    // admin who adds an entry excluding their own IP could never reach the route to
    // fix it (every `/api/workspaces/:id/*` route, including this one, would 403).
    // Managing the allowlist still requires owner/admin auth via the handler.
    if path.ends_with("/ip-allowlist") {
        return next.run(request).await;
    }

    // SCIM uses its own per-workspace bearer credential and lives outside
    // `/api`, so it cannot be covered by `AuthUser` policy intersection below.
    // Resolve that token's tenant and enforce the same trusted client address
    // before any provisioning read or mutation reaches the SCIM handler.
    if path.starts_with("/scim/v2/") {
        if let Some(workspace) = scim_token_workspace(&s, &headers).await {
            match IpAllowlistRepo::new(s.pg.clone()).cidrs(workspace).await {
                Ok(cidrs) if !cidrs.is_empty() && !is_allowed(client_ip, &cidrs) => {
                    return scim_network_forbidden();
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        %workspace,
                        "SCIM ip-allowlist lookup failed; failing open"
                    );
                }
            }
        }
    }

    // Only anonymous identity/recovery/token-rotation endpoints are exempt.
    // Authenticated routes that happen to live under `/api/auth` (session list
    // and revocation, login history, password/email changes) still expose or
    // mutate security state and must obey the participant's workspace policy.
    if path.starts_with("/api/") && !auth_network_exempt(&path) {
        if let Some(user) = authenticated_user(&s, &headers).await {
            // Let the downstream extractor reuse this fully validated identity;
            // otherwise every request would hit the active-session store twice.
            request.extensions_mut().insert(user);
            if let Err(error) =
                assert_participant_network_access(&s, user.participant_id, client_ip).await
            {
                return ApiError(error).into_response();
            }
        }
    }

    if let Some(ws) = workspace_id_from_path(&path).and_then(|id| WorkspaceId::from_str(id).ok()) {
        match IpAllowlistRepo::new(s.pg.clone()).cidrs(ws).await {
            Ok(cidrs) if !cidrs.is_empty() => {
                if !is_allowed(client_ip, &cidrs) {
                    return ApiError(AeroError::Forbidden(
                        "client IP is not in this workspace's allowlist".into(),
                    ))
                    .into_response();
                }
            }
            Ok(_) => {} // empty allowlist ⇒ unrestricted
            Err(e) => {
                tracing::warn!(error = ?e, %ws, "ip-allowlist: cidrs lookup failed; failing open");
            }
        }
    }
    next.run(request).await
}

fn auth_network_exempt(path: &str) -> bool {
    matches!(
        path,
        "/api/auth/register"
            | "/api/auth/login"
            | "/api/auth/2fa/recover"
            | "/api/auth/forgot-password"
            | "/api/auth/reset-password"
            | "/api/auth/refresh"
            | "/api/auth/logout"
            | "/api/auth/oidc"
    )
}

/// Trusted client address resolved once at the edge and made available to
/// downstream handlers that record security telemetry.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedClientIp(pub IpAddr);

async fn authenticated_user(s: &AppState, headers: &HeaderMap) -> Option<AuthUser> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))?
        .trim();
    if token.is_empty() {
        return None;
    }

    // Opaque credential prefixes are disjoint from JWTs. Keeping the branches
    // explicit means a valid-but-revoked JWT can never fall through and be
    // reinterpreted as another credential kind.
    if token.starts_with("aero_pat_") {
        return s
            .auth
            .verify_pat(token)
            .await
            .map(|participant_id| AuthUser {
                participant_id,
                session_id: None,
                exp: None,
            });
    }
    if token.starts_with("bot_") {
        return s
            .auth
            .verify_bot_token(token)
            .await
            .map(|participant_id| AuthUser {
                participant_id,
                session_id: None,
                exp: None,
            });
    }
    let claims = s.auth.verify_access(token).await.ok()?;
    Some(AuthUser {
        participant_id: claims.participant_id().ok()?,
        session_id: claims.session_id().ok()?,
        exp: Some(claims.exp),
    })
}

async fn scim_token_workspace(s: &AppState, headers: &HeaderMap) -> Option<WorkspaceId> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))?
        .trim();
    if token.is_empty() {
        return None;
    }
    match aero_storage::ScimRepo::new(s.pg.clone())
        .workspace_for_token_hash(&aero_storage::scim::hash_token(token))
        .await
    {
        Ok(workspace) => workspace,
        Err(error) => {
            // Authentication/storage errors are rendered by the SCIM handler;
            // this middleware only makes a positive authorization decision.
            tracing::warn!(
                ?error,
                "SCIM token workspace lookup failed; deferring to handler"
            );
            None
        }
    }
}

fn scim_network_forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "schemas": ["urn:ietf:params:scim:api:messages:2.0:Error"],
            "detail": "client IP is not in this workspace's allowlist",
            "status": "403"
        })),
    )
        .into_response()
}

/// Enforce all configured authorized-network policies for a participant.
///
/// The server has global endpoints and one global WebSocket, so choosing only a
/// workspace encoded in the path is insufficient. The secure contract is the
/// intersection of the participant's restricted workspace memberships: if any
/// one tenant disallows the source IP, the global request/socket is rejected.
/// A database error remains fail-open in line with the existing availability
/// policy, while a confirmed mismatch is fail-closed.
pub async fn assert_participant_network_access(
    s: &AppState,
    participant: ParticipantId,
    client_ip: IpAddr,
) -> Result<(), AeroError> {
    let configured = match IpAllowlistRepo::new(s.pg.clone())
        .configured_for_participant(participant)
        .await
    {
        Ok(configured) => configured,
        Err(error) => {
            tracing::warn!(
                error = ?error,
                %participant,
                "ip-allowlist: participant policy lookup failed; failing open"
            );
            return Ok(());
        }
    };
    if let Some(workspace) = denied_workspace(client_ip, configured) {
        return Err(AeroError::Forbidden(format!(
            "client IP is not in workspace {workspace}'s allowlist"
        )));
    }
    Ok(())
}

fn denied_workspace(
    client_ip: IpAddr,
    configured: Vec<(WorkspaceId, String)>,
) -> Option<WorkspaceId> {
    let mut policies: Vec<(WorkspaceId, Vec<String>)> = Vec::new();
    for (workspace, cidr) in configured {
        if let Some((_, cidrs)) = policies
            .iter_mut()
            .find(|(candidate, _)| *candidate == workspace)
        {
            cidrs.push(cidr);
        } else {
            policies.push((workspace, vec![cidr]));
        }
    }
    policies
        .into_iter()
        .find_map(|(workspace, cidrs)| (!is_allowed(client_ip, &cidrs)).then_some(workspace))
}

/// Extract the `{id}` segment from `/api/workspaces/{id}/...`. `None` for any other
/// path (not workspace-scoped — the allowlist does not apply).
fn workspace_id_from_path(path: &str) -> Option<&str> {
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    if segs.next()? == "api" && segs.next()? == "workspaces" {
        segs.next()
    } else {
        None
    }
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// Resolve the caller's workspace role and require admin (Owner/Admin), mirroring
/// the gate in `ai_dlq.rs` / `workspace_security.rs`.
async fn assert_admin(
    s: &AppState,
    ws: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    let role: WorkspaceRole = s
        .workspaces
        .effective_member_role(ws, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    if role.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("workspace admin required".into()))
    }
}

/// Validate a CIDR string the way the matcher does: it must be of the form
/// `addr/len` and parse to a usable v4/v6 prefix. We probe with the matcher's own
/// parser by checking that a same-family address matches it, so a malformed range
/// can never be persisted (and silently never enforce).
fn validate_cidr(cidr: &str) -> Result<String, AeroError> {
    let trimmed = cidr.trim();
    let net = trimmed
        .split_once('/')
        .and_then(|(net, _)| net.trim().parse::<IpAddr>().ok())
        .ok_or_else(|| AeroError::Invalid(format!("invalid cidr: {cidr}")))?;
    // The network address itself must fall inside its own prefix — true for any
    // well-formed CIDR, false for a bad prefix length / family mismatch.
    if ip_in_cidr(net, trimmed) {
        Ok(trimmed.to_owned())
    } else {
        Err(AeroError::Invalid(format!("invalid cidr: {cidr}")))
    }
}

/// `GET /api/workspaces/:id/ip-allowlist` — list a workspace's authorized networks.
async fn list_allowlist(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    let repo = IpAllowlistRepo::new(s.pg.clone());
    let entries = repo.list(ws).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        // Empty ⇒ the allowlist is disabled (allow-all).
        "enabled": !entries.is_empty(),
        "entries": entries,
    })))
}

#[derive(Deserialize)]
struct AddReq {
    cidr: String,
    #[serde(default)]
    note: Option<String>,
}

/// `POST /api/workspaces/:id/ip-allowlist` `{cidr, note?}` — add an authorized
/// network. Idempotent on `(workspace, cidr)`.
async fn add_allowlist(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<AddReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    let cidr = validate_cidr(&req.cidr)?;
    let note = req.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let entry = IpAllowlistRepo::new(s.pg.clone())
        .add_authorized(ws, &cidr, note, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(entry).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct RemoveReq {
    cidr: String,
}

/// `DELETE /api/workspaces/:id/ip-allowlist` `{cidr}` — remove an authorized
/// network. 404 when the range was not on the allowlist.
async fn remove_allowlist(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<RemoveReq>,
) -> ApiResult<StatusCode> {
    let ws = parse_workspace(&ws_str)?;
    let cidr = req.cidr.trim();
    let removed = IpAllowlistRepo::new(s.pg.clone())
        .remove_authorized(ws, cidr, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AeroError::NotFound("cidr not on allowlist".into()).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_required_matrix() {
        let cases: &[(WorkspaceRole, bool)] = &[
            (WorkspaceRole::Owner, true),
            (WorkspaceRole::Admin, true),
            (WorkspaceRole::Member, false),
            (WorkspaceRole::Guest, false),
        ];
        for (role, expect_ok) in cases {
            assert_eq!(role.can_administer(), *expect_ok, "role {role:?}");
        }
    }

    #[test]
    fn validate_cidr_accepts_well_formed() {
        assert_eq!(validate_cidr("10.0.0.0/8").unwrap(), "10.0.0.0/8");
        assert_eq!(validate_cidr(" 1.2.3.4/32 ").unwrap(), "1.2.3.4/32");
        assert_eq!(validate_cidr("2001:db8::/32").unwrap(), "2001:db8::/32");
    }

    #[test]
    fn validate_cidr_rejects_malformed() {
        assert!(validate_cidr("10.0.0.0").is_err()); // no slash
        assert!(validate_cidr("10.0.0.0/33").is_err()); // bad v4 prefix len
        assert!(validate_cidr("::/129").is_err()); // bad v6 prefix len
        assert!(validate_cidr("not-an-ip/8").is_err());
        assert!(validate_cidr("").is_err());
    }

    /// The enforcement middleware keys on this parse — a wrong segment would either
    /// enforce the allowlist on the wrong/no workspace (security hole) or block
    /// non-workspace traffic (outage). Pin the exact contract.
    #[test]
    fn workspace_id_from_path_extracts_only_workspace_scoped_paths() {
        use super::workspace_id_from_path as f;
        assert_eq!(f("/api/workspaces/01ABC/members"), Some("01ABC"));
        assert_eq!(f("/api/workspaces/01ABC"), Some("01ABC"));
        assert_eq!(f("/api/workspaces/01ABC/admin/usage"), Some("01ABC"));
        assert_eq!(f("/api/workspaces/01ABC/ip-allowlist"), Some("01ABC"));
        // Not workspace-scoped → no enforcement.
        assert_eq!(f("/api/rooms/01ABC/messages"), None);
        assert_eq!(f("/api/workspaces"), None);
        assert_eq!(f("/api/auth/login"), None);
        assert_eq!(f("/health/ready"), None);
        assert_eq!(f("/"), None);
        assert_eq!(f(""), None);
    }

    #[test]
    fn participant_policy_is_intersection_of_restricted_workspaces() {
        let first = WorkspaceId::new();
        let second = WorkspaceId::new();
        let configured = vec![
            (first, "10.0.0.0/8".to_owned()),
            (first, "192.168.0.0/16".to_owned()),
            (second, "10.20.0.0/16".to_owned()),
        ];
        assert_eq!(
            denied_workspace("10.20.4.5".parse().unwrap(), configured.clone()),
            None,
            "one matching CIDR in every restricted workspace permits the request"
        );
        assert_eq!(
            denied_workspace("10.30.4.5".parse().unwrap(), configured),
            Some(second),
            "a mismatch in any restricted workspace rejects the global request"
        );
        assert_eq!(
            denied_workspace("203.0.113.7".parse().unwrap(), Vec::new()),
            None,
            "no configured membership is unrestricted"
        );
    }

    #[test]
    fn only_anonymous_auth_entrypoints_bypass_network_policy() {
        for path in [
            "/api/auth/register",
            "/api/auth/login",
            "/api/auth/2fa/recover",
            "/api/auth/forgot-password",
            "/api/auth/reset-password",
            "/api/auth/refresh",
            "/api/auth/logout",
            "/api/auth/oidc",
        ] {
            assert!(auth_network_exempt(path), "{path} must remain recoverable");
        }
        for path in [
            "/api/auth/sessions",
            "/api/auth/sessions/revoke-others",
            "/api/auth/sessions/01ABC",
            "/api/auth/login-history",
            "/api/auth/change-password",
            "/api/auth/change-email",
        ] {
            assert!(
                !auth_network_exempt(path),
                "{path} must enforce authorized networks"
            );
        }
        assert!(
            !auth_network_exempt("/api/auth/login/extra"),
            "matching is exact, not prefix-based"
        );
    }
}
