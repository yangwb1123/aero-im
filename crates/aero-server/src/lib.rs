//! HTTP/WS gateway. Composes auth + im-core + bus + storage into Axum routes.

pub mod agent_bot;
// ROADMAP3 方向二: cross-node call-bridge spawn orchestration (supervisor).
pub mod call_bridge_supervisor;
pub mod moderation_bot;
pub mod transcribe_bot;
pub mod unfurl_bot;
pub mod ai_adapter;
pub mod bookmarks;
pub mod channel_sections;
pub mod collab;
pub mod config;
pub mod drafts;
pub mod emoji;
pub mod error;
pub mod guests;
pub mod hub;
pub mod invitations;
pub mod live;
pub mod metrics;
pub mod notif_prefs;
pub mod pat;
pub mod polls;
pub mod rate_limit;
pub mod routes;
pub mod saved_searches;
pub mod scim;
pub mod sso;
pub mod state;
pub mod stream_mod;
pub mod user_status;
pub mod vod;
pub mod webhooks;
pub mod workspaces;
pub mod ws;
pub mod channels;
pub mod commands;
pub mod forward;
pub mod message_reminders;
pub mod scheduled;
pub mod scheduled_streams;
pub mod search;
pub mod translate;
// Wave 10 (0033-0038).
pub mod announcements;
pub mod favorites;
pub mod keyword_alerts;
pub mod message_history;
pub mod message_context;
pub mod profiles;
pub mod user_groups;
// Wave 11.
pub mod files;
pub mod read_all;
pub mod stream_follows;
pub mod thread_subs;
// Wave 12.
pub mod dm;
pub mod recurring;
pub mod catchup;
pub mod reaction_detail;
pub mod default_channels;
// Wave 13.
pub mod group_dm;
pub mod action_items;
pub mod join_requests;
pub mod conversation_export;
// Wave 14.
pub mod twofa;
pub mod deactivation;
pub mod templates;
// Wave 15.
pub mod session;
pub mod search_advanced;
pub mod smart_replies;
pub mod channel_roles;

// ---- Wave 16 ----
pub mod analytics;
pub mod canvas;
pub mod channel_bookmarks;
pub mod directory;
pub mod stream_discovery;
pub mod subscriptions;

// ---- Wave 17 ----
pub mod approvals;
pub mod legal_holds;
pub mod ooo;
pub mod ooo_bot;
pub mod org_chart;
pub mod tasks;
pub mod workspace_files;

// ---- Wave 18 ----
pub mod ai_rewrite;
pub mod call_history;
/// Clip collections / playlists.
pub mod clip_collections;
pub mod clips;
pub mod mark_unread;
pub mod stream_analytics;
pub mod stream_key;

// ---- Wave 19 ----
pub mod channel_retention;
pub mod info_barriers;
pub mod snooze;
pub mod workspace_ask;

// ---- Wave 21 ----
pub mod activity;
pub mod golive_bot;
pub mod sessions;

// ---- Wave 23 ----
pub mod call_recap;

// ---- Wave 24 ----
pub mod workspace_security;
pub mod me_export;
pub mod push_tokens;
pub mod push_bot;
pub mod ai_dlq;
pub mod admin_sessions;

// ---- Wave 16 Round 9 ----
pub mod online;

// ---- Parity batch: live chat modes, stream metadata edit, IP allowlist ----
pub mod stream_chat_modes;
pub mod stream_meta;
pub mod ip_allowlist;

// ---- ROADMAP3 方向五: per-workspace rate-limit tiers (租户公平) ----
pub mod ws_rate;

// ---- Collaboration parity batch (Slack/Lark/Teams) ----
// Per-message read receipts ("Seen by"), bookmark folders/collections, and
// multi-channel broadcast; scheduled-message editing extends crate::scheduled.
pub mod bookmark_collections;
pub mod broadcast;
pub mod message_receipts;

// ---- Interactive-live / creator parity (migrations 0079-0083) ----
// Hype train / combo gifts, raids, VOD chapters, stream-moderator role assignment.
pub mod hype_train;
pub mod raids;
pub mod stream_moderators;
pub mod vod_chapters;

// ---- AI-native cluster: thread summary, scheduled digests, find-expert ----
pub mod thread_summarize;
/// Thread auto-titling: POST /api/messages/:id/thread-title (degrade-safe).
pub mod thread_title;
/// Per-message sentiment / toxicity scoring: POST /api/messages/:id/sentiment.
pub mod message_sentiment;
pub mod digests;
pub mod find_expert;
/// AI recommendations: suggested channels & people to follow (member-gated).
pub mod recommendations;

// ---- Operability: webhook delivery DLQ admin + per-tenant usage reports ----
pub mod webhook_admin;
pub mod usage_report;

// ---- Interactive message blocks (Slack Block Kit-lite) ----
// Record clicks / option-picks on interactive Button/Select blocks (migration
// 0086); broadcasts RoomEvent::Interaction so the poster's bot/app sees them live.
pub mod interactions;

// ---- Live / creator economy (migrations 0089-0091) ----
// Channel points + custom-reward redemption, goal/bounty bars, ban/timeout appeals.
pub mod channel_points;
pub mod goals;
pub mod ban_appeals;

// ---- Community predictions / channel betting (migration 0092) ----
// Creator opens a prediction (2+ outcomes); viewers STAKE channel points on one;
// the creator LOCKS then RESOLVES (winners paid proportionally) or CANCELS (refund all).
pub mod predictions;

// User-initiated message reports -> workspace moderation review queue (migration
// 0093): a room member reports a message, an admin keeps/removes it (removal reuses
// the existing transactional moderate-delete path).
pub mod message_reports;

/// OpenAPI 3.0 spec endpoint.
pub mod openapi;

// ---- User-level blocking / ignoring (migration 0106) ----
/// Block/unblock another participant + list caller's blocks.
pub mod user_blocks;

// ---- ROADMAP9 — extended subscription tier levels (migration 0109) ----
/// Extended per-creator subscription tiers (position + benefits JSONB).
pub mod subscription_tiers;

// ---- ROADMAP10 ----
/// Auto-moderation rules: workspace admins define text-matching block/warn rules.
pub mod auto_mod;
/// User-level report flow: any user can report another; admins resolve reports.
pub mod user_reports;
/// Channel-points expiry info endpoint.
pub mod point_expiry;

pub use state::AppState;
