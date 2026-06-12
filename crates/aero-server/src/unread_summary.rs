//! Bulk unread summary endpoint (Feature 5, no migration).
//!
//! Returns all rooms where the authenticated user has unread messages, with
//! the count of unread messages per room. Backed by the existing
//! `MessageRepo::unread_counts_by_room` query which reads `read_receipts` +
//! `messages` + `room_members` (all pre-existing tables).
//!
//! Route:
//!   GET /api/me/unread-summary  — authenticated; returns rooms with unread counts

use aero_auth::AuthUser;
use aero_common::Error as AeroError;
use axum::{
    extract::State,
    routing::get,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/me/unread-summary", get(unread_summary))
}

/// `GET /api/me/unread-summary` — rooms where the caller has unread messages,
/// each with the count. Only rooms with at least one unread message appear.
/// Rooms where the caller has no read receipt are treated as fully unread.
async fn unread_summary(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let counts = s
        .messages
        .unread_counts_by_room(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    let rooms: Vec<serde_json::Value> = counts
        .into_iter()
        .map(|(room_id, count)| {
            serde_json::json!({
                "room_id": room_id.to_string(),
                "unread_count": count,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "rooms": rooms })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_counts_serializes_to_empty_rooms_array() {
        // Purely structural check — no DB needed.
        let counts: Vec<(aero_common::RoomId, u32)> = vec![];
        let rooms: Vec<serde_json::Value> = counts
            .into_iter()
            .map(|(room_id, count)| {
                serde_json::json!({
                    "room_id": room_id.to_string(),
                    "unread_count": count,
                })
            })
            .collect();
        let body = serde_json::json!({ "rooms": rooms });
        assert_eq!(body["rooms"].as_array().unwrap().len(), 0);
    }
}
