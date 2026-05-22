//! Shared application state injected into Axum handlers.

use std::sync::Arc;

use aero_auth::AuthService;
use aero_bus::EventBus;
use aero_im_core::ImService;
use aero_storage::{MessageRepo, ParticipantRepo, PresenceStore, RoomRepo};
use axum::extract::FromRef;

use crate::hub::Hub;

#[derive(Clone)]
pub struct AppState {
    pub auth: AuthService,
    pub im: Arc<ImService>,
    pub participants: ParticipantRepo,
    pub rooms: RoomRepo,
    pub messages: MessageRepo,
    pub presence: PresenceStore,
    pub bus: Arc<dyn EventBus>,
    pub hub: Arc<Hub>,
}

// `AuthUser` extractor expects to pull `AuthService` from the router state.
impl FromRef<AppState> for AuthService {
    fn from_ref(state: &AppState) -> Self {
        state.auth.clone()
    }
}
