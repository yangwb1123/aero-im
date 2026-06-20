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
        get(list_allowlist).post(add_allowlist).delete(remove_allowlist),
    )
}

/// Enforcement middleware (the half the module doc said was unwired): for
/// workspace-scoped paths `/api/workspaces/{id}/...`, reject (403) a client whose
/// IP is not in that workspace's allowlist. An EMPTY allowlist allows everyone
/// (`is_allowed` short-circuits), so a workspace with none configured pays only one
/// cheap `cidrs` SELECT — and these are low-frequency config/admin routes, not the
/// message hot path, so no cache is needed. Non-workspace paths pass through.
///
/// Fail-OPEN on a DB error (a storage hiccup must not lock everyone out of their
/// workspace), matching the rate-limit / spam-guard availability stance; it only
/// blocks when it can positively confirm the IP is outside a non-empty allowlist.
pub async fn enforce_layer(
    State(s): State<AppState>,
    connect_info: Option<ConnectInfo<std::net::SocketAddr>>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    // Anti-lockout: NEVER enforce on the allowlist-management route itself, or an
    // admin who adds an entry excluding their own IP could never reach the route to
    // fix it (every `/api/workspaces/:id/*` route, including this one, would 403).
    // Managing the allowlist still requires owner/admin auth via the handler.
    if path.ends_with("/ip-allowlist") {
        return next.run(request).await;
    }
    if let Some(ws) = workspace_id_from_path(path).and_then(|id| WorkspaceId::from_str(id).ok()) {
        match IpAllowlistRepo::new(s.pg.clone()).cidrs(ws).await {
            Ok(cidrs) if !cidrs.is_empty() => {
                let ip = crate::rate_limit::client_ip(&headers, connect_info.map(|ci| ci.0));
                if !is_allowed(ip, &cidrs) {
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
        .member_role(ws, caller)
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
    assert_admin(&s, ws, auth.participant_id).await?;
    let cidr = validate_cidr(&req.cidr)?;
    let note = req.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let entry = IpAllowlistRepo::new(s.pg.clone())
        .add(ws, &cidr, note)
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
    assert_admin(&s, ws, auth.participant_id).await?;
    let cidr = req.cidr.trim();
    let removed = IpAllowlistRepo::new(s.pg.clone())
        .remove(ws, cidr)
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
}
