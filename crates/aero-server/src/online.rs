//! Per-room online-presence REST endpoints.
//!
//! Exposes the [`Hub`](crate::hub::Hub)'s in-process roster as HTTP, so clients
//! that cannot hold a WebSocket (dashboards, mobile background polls) can query
//! who is currently connected. Both endpoints are room-access gated — the caller
//! must be a member of the room.
//!
//! - `GET /api/rooms/:id/online` — full roster: ids + display names of all
//!   WebSocket clients currently connected to that room on THIS server node.
//! - `GET /api/rooms/:id/online/count` — lightweight count-only variant for
//!   badge polling, avoids fetching participant rows.
//!
//! **Cluster-wide (ROADMAP 方向一)**: both endpoints read the room's
//! [`PresenceStore`](aero_storage::PresenceStore) Redis sorted set, so the count
//! and roster reflect connections across every node — not just this process. The
//! in-process [`Hub`](crate::hub::Hub) roster is used only as a degradation
//! fallback when Redis is unreachable.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId};
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All online-presence routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rooms/:id/online", get(room_online))
        .route("/api/rooms/:id/online/count", get(room_online_count))
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// `GET /api/rooms/:id/online` — the participants currently connected via
/// WebSocket in this room (on this server node).
///
/// Returns an array of `{ id, display_name }` objects, one per unique connected
/// participant, aggregated cluster-wide from the room's Redis presence set (local
/// hub roster used only if Redis is unreachable).
async fn room_online(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&id_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;

    // Cluster-wide roster from Redis (ROADMAP 方向一); fall back to this node's
    // local hub view if Redis is unreachable or has no entry yet.
    let ids = match s.presence.members(room).await {
        Ok(members) if !members.is_empty() => members,
        _ => s.hub.room_members_online(room),
    };
    // Fetch display names through the in-memory participant cache (this is a hot,
    // pollable read path): `get_or_fetch` serves repeat lookups from the 60s TTL
    // cache instead of a per-participant DB round-trip, matching the cached call
    // site in `ws_impl/frame.rs`. A genuine miss still backfills from the repo.
    let mut members = Vec::with_capacity(ids.len());
    for pid in ids {
        if let Ok(Some(p)) = s.participant_cache.get_or_fetch(pid, &s.participants).await {
            members.push(serde_json::json!({
                "id": pid,
                "display_name": p.display_name,
            }));
        } else {
            // Participant row missing (race or deleted account) — include id only.
            members.push(serde_json::json!({ "id": pid }));
        }
    }
    Ok(Json(serde_json::json!({ "online": members })))
}

/// `GET /api/rooms/:id/online/count` — the count of participants currently
/// connected via WebSocket in this room, aggregated cluster-wide via Redis.
///
/// Lighter than the full `/online` endpoint: no participant row lookups. Use for
/// badge polling or sidebar decoration where only the number matters.
async fn room_online_count(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&id_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    // Cluster-wide count from Redis (ROADMAP 方向一); fall back to the node-local
    // hub count when Redis is unavailable.
    let count = match s.presence.count(room).await {
        Ok(n) => usize::try_from(n).unwrap_or(usize::MAX),
        Err(_) => s.hub.room_members_online(room).len(),
    };
    Ok(Json(serde_json::json!({ "count": count })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_room_rejects_garbage() {
        assert!(parse_room("not-a-uuid").is_err());
        assert!(parse_room("").is_err());
    }

    #[test]
    fn parse_room_trims_whitespace() {
        // A valid ULID (the format RoomId uses) with surrounding spaces parses fine.
        let id = aero_common::RoomId::new().to_string();
        let padded = format!("  {id}  ");
        assert!(parse_room(&padded).is_ok());
    }
}
