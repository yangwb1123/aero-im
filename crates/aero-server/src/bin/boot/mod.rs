//! Boot orchestration for the aero-server binary.
#![allow(unused_imports)]
//!
//! Splits the monolithic `main()` into focused modules:
//! - `persistence` — PG, Redis, NATS, blob, bus
//! - `repos` — all repository/store constructors
//! - `services` — Auth, IM, Live, AI service construction
//! - `ingest` — RTMP + SRT ingest spawning
//! - `orchestration` — call orchestrator + bridge supervisor
//! - `state_builder` — `AppState` composition
//! - `background` — bots, dispatchers, webhook loops, blob GC
//! - `metrics_tasks` — gauge samplers + cross-node heartbeats
//! - `retention` — message retention + data-lifecycle sweeps
//! - `serve` — Axum router + HTTP server + graceful shutdown
//! - `shutdown` — SIGTERM/Ctrl-C handler
//! - `helpers` — `connect_with_retry`, `build_push_gateways`, `srt_backing_rtmp_addr`

mod background;
mod helpers;
mod ingest;
mod metrics_tasks;
mod orchestration;
mod persistence;
mod repos;
mod retention;
pub(crate) mod serve;
mod services;
mod shutdown;
mod state_builder;

// Re-exports for main.rs
pub(crate) use background::spawn_all as spawn_background;
pub(crate) use helpers::{build_push_gateways, connect_with_retry, srt_backing_rtmp_addr};
pub(crate) use ingest::{from_server_cfg as ingest_config, spawn as spawn_ingest, IngestConfig};
pub(crate) use metrics_tasks::spawn_all as spawn_metrics_tasks;
pub(crate) use orchestration::{build as build_orchestration, Orchestration};
pub(crate) use persistence::{connect as connect_persistence, Persistence};
pub(crate) use repos::{new as build_repos, Repos};
pub(crate) use retention::spawn as spawn_retention;
pub(crate) use serve::serve;
pub(crate) use services::{build as build_services, Services, ServicesDeps};
pub(crate) use shutdown::shutdown_signal;
pub(crate) use state_builder::{build as build_state, StateDeps};
