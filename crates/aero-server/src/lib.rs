//! HTTP/WS gateway. Composes auth + im-core + bus + storage into Axum routes.

pub mod agent_bot;
pub mod transcribe_bot;
pub mod ai_adapter;
pub mod error;
pub mod hub;
pub mod routes;
pub mod state;
pub mod ws;

pub use state::AppState;
