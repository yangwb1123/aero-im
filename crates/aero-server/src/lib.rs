//! HTTP/WS gateway. Composes auth + im-core + bus + storage into Axum routes.

pub mod error;
pub mod hub;
pub mod routes;
pub mod state;
pub mod ws;

pub use state::AppState;
