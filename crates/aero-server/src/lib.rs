//! HTTP/WS gateway. Composes auth + im-core + bus + storage into Axum routes.

pub mod agent_bot;
pub mod moderation_bot;
pub mod transcribe_bot;
pub mod ai_adapter;
pub mod bookmarks;
pub mod collab;
pub mod config;
pub mod emoji;
pub mod error;
pub mod hub;
pub mod invitations;
pub mod live;
pub mod metrics;
pub mod notif_prefs;
pub mod pat;
pub mod rate_limit;
pub mod routes;
pub mod scim;
pub mod sso;
pub mod state;
pub mod user_status;
pub mod webhooks;
pub mod workspaces;
pub mod ws;
pub mod channels;
pub mod scheduled;
pub mod search;

pub use state::AppState;
