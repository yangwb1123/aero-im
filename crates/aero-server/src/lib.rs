//! HTTP/WS gateway. Composes auth + im-core + bus + storage into Axum routes.

pub mod agent_bot;
pub mod moderation_bot;
pub mod transcribe_bot;
pub mod ai_adapter;
pub mod collab;
pub mod config;
pub mod error;
pub mod hub;
pub mod live;
pub mod metrics;
pub mod rate_limit;
pub mod routes;
pub mod sso;
pub mod state;
pub mod webhooks;
pub mod workspaces;
pub mod ws;
pub mod channels;

pub use state::AppState;
