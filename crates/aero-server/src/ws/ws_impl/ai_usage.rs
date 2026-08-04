//! Workspace-scoped accounting adapters for optional call AI features.

use aero_common::RoomId;

use crate::state::{AiBackend, AppState};

async fn context(state: &AppState, room: RoomId) -> aero_ai::usage::UsageContext {
    let workspace = match state.rooms.room_workspace(room).await {
        Ok(workspace) => workspace.map(|value| value.to_uuid()),
        Err(error) => {
            tracing::warn!(
                ?error,
                %room,
                "AI usage workspace lookup failed; recording unscoped"
            );
            None
        }
    };
    aero_ai::usage::UsageContext::new(workspace)
}

pub(super) async fn summarize(
    state: &AppState,
    ai: &dyn AiBackend,
    room: RoomId,
    text: &str,
) -> Result<String, String> {
    ai.summarize_text_with_usage_context(text, context(state, room).await)
        .await
}

pub(super) async fn translate(
    state: &AppState,
    ai: &dyn AiBackend,
    room: RoomId,
    text: &str,
    target: &str,
) -> Result<String, String> {
    ai.translate_with_usage_context(text, target, context(state, room).await)
        .await
}
