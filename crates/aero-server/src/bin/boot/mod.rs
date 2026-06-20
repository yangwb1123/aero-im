//! Boot orchestration for the aero-server binary.
//!
//! Splits the monolithic `main()` into focused modules:
//! - `persistence` — PG, Redis, NATS, blob, bus
//! - `repos` — all repository/store constructors
//! - `services` — Auth, IM, Live, AI service construction
//! - `ingest` — RTMP + SRT ingest spawning
//! - `orchestration` — call orchestrator + bridge supervisor
//! - `state_builder` — AppState composition
//! - `background` — bots, dispatchers, webhook loops, blob GC
//! - `metrics_tasks` — gauge samplers + cross-node heartbeats
//! - `retention` — message retention + data-lifecycle sweeps
//! - `serve` — Axum router + HTTP server + graceful shutdown
//! - `shutdown` — SIGTERM/Ctrl-C handler
//! - `helpers` — connect_with_retry, build_push_gateways, srt_backing_rtmp_addr

mod persistence;
mod repos;
mod services;
mod ingest;
mod orchestration;
mod state_builder;
mod background;
mod metrics_tasks;
mod retention;
mod shutdown;
pub(crate) mod serve;
mod helpers;

// Re-exports for main.rs
pub(crate) use serve::serve;
pub(crate) use shutdown::shutdown_signal;
pub(crate) use helpers::{connect_with_retry, build_push_gateways, srt_backing_rtmp_addr};
pub(crate) use persistence::{Persistence, connect as connect_persistence};
pub(crate) use repos::{Repos, new as build_repos};
pub(crate) use services::{Services, build as build_services, ServicesDeps};
pub(crate) use ingest::{IngestConfig, from_server_cfg as ingest_config, spawn as spawn_ingest};
pub(crate) use orchestration::{Orchestration, build as build_orchestration};
pub(crate) use state_builder::{StateDeps, build as build_state};
pub(crate) use background::spawn_all as spawn_background;
pub(crate) use metrics_tasks::spawn_all as spawn_metrics_tasks;
pub(crate) use retention::spawn as spawn_retention;
