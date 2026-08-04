//! HTTP/WS gateway. Composes auth + im-core + bus + storage into Axum routes.

pub mod agent_bot;
/// User-bot event-subscription dispatcher (方向三): fans bus RoomEvents out to
/// matching `bot_event_subscriptions` webhooks, reusing the outbound webhook
/// signing/sender seam.
pub mod bot_dispatch;
// ROADMAP3 方向二: cross-node call-bridge spawn orchestration (supervisor).
pub mod call_bridge_subscribe;
pub mod call_bridge_supervisor;
// ROADMAP 方向五: per-peer SFU media session (the str0m loop driving on_rtp).
pub mod ai_adapter;
/// ClamAV (`clamd`) INSTREAM anti-virus scanning for uploaded blobs.
pub mod av_scan;
pub mod bookmarks;
pub mod channel_sections;
pub mod channels;
pub mod collab;
pub mod commands;
pub mod config;
mod consumer_event_receipt;
/// Magic-byte content sniffing for uploaded blobs (reject disguised executables/markup).
pub mod content_sniff;
mod deferred_blocks;
pub mod drafts;
pub mod emoji;
pub mod error;
pub mod forward;
mod giphy;
pub mod guests;
pub mod hub;
pub mod identity_migrations;
pub mod integrations;
pub mod invitations;
pub mod live;
/// Minimal transactional email sender (password reset, invitation).
pub mod mailer;
pub mod message_reminders;
mod message_send_policy;
pub mod metrics;
pub mod moderation_bot;
pub mod notif_prefs;
/// Per-process TTL cache for participant profiles (ROADMAP6 方向四 多级缓存).
pub mod participant_cache;
pub mod pat;
pub mod polls;
pub mod rate_limit;
/// Per-process TTL cache for room membership lists (ROADMAP6 方向四 多级缓存).
pub mod room_member_cache;
pub mod routes;
pub mod saml;
pub mod saved_searches;
pub mod scheduled;
pub mod scheduled_streams;
pub mod scim;
pub mod search;
pub mod sfu_media;
pub mod sso;
pub mod state;
pub mod stream_live_outbox;
pub mod stream_mod;
mod task_shutdown;
pub mod transcribe_bot;
pub mod translate;
pub mod unfurl_bot;
pub mod user_status;
pub mod vod;
pub mod webhooks;
pub mod whip_media;
pub mod workspaces;
pub mod ws;
// Wave 10 (0033-0038).
pub mod announcements;
pub mod favorites;
pub mod keyword_alerts;
pub mod message_context;
pub mod message_history;
pub mod profiles;
pub mod user_groups;
// Wave 11.
pub mod files;
pub mod read_all;
pub mod stream_follows;
pub mod thread_subs;
// Wave 12.
pub mod catchup;
pub mod default_channels;
pub mod dm;
pub mod reaction_detail;
pub mod recurring;
// Wave 13.
pub mod action_items;
pub mod conversation_export;
pub mod group_dm;
pub mod join_requests;
// Wave 14.
pub mod deactivation;
pub mod templates;
pub mod twofa;
// Wave 15.
pub mod channel_roles;
pub mod search_advanced;
pub mod session;
pub mod session_control;
pub mod smart_replies;

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
pub mod admin_sessions;
pub mod ai_dlq;
pub mod ai_usage;
pub mod me_export;
pub mod push_bot;
pub mod push_tokens;
pub mod workspace_security;

// ---- Wave 16 Round 9 ----
pub mod online;

// ---- Parity batch: live chat modes, stream metadata edit, IP allowlist ----
pub mod ip_allowlist;
pub mod stream_chat_modes;
pub mod stream_meta;

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
pub mod digests;
pub mod find_expert;
/// Per-message sentiment / toxicity scoring: POST /api/messages/:id/sentiment.
pub mod message_sentiment;
/// AI recommendations: suggested channels & people to follow (member-gated).
pub mod recommendations;
pub mod saved_search_monitor;
pub mod thread_summarize;
/// Thread auto-titling: POST /api/messages/:id/thread-title (degrade-safe).
pub mod thread_title;

// ---- Operability: webhook delivery DLQ admin + per-tenant usage reports ----
pub mod usage_report;
pub mod webhook_admin;

// ---- Interactive message blocks (Slack Block Kit-lite) ----
// Record clicks / option-picks on interactive Button/Select blocks (migration
// 0086); broadcasts RoomEvent::Interaction so the poster's bot/app sees them live.
pub mod interactions;

// ---- Live / creator economy (migrations 0089-0091) ----
// Channel points + custom-reward redemption, goal/bounty bars, ban/timeout appeals.
pub mod ban_appeals;
pub mod channel_points;
pub mod goals;

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
/// Channel-points expiry info endpoint.
pub mod point_expiry;
/// User-level report flow: any user can report another; admins resolve reports.
pub mod user_reports;

// ---- ROADMAP11 ----
/// Report-only PII backfill scan over historical messages (no migration).
pub mod pii_backfill;
/// Bulk unread summary: GET /api/me/unread-summary (no migration).
pub mod unread_summary;
/// Creator verified badge: admin grant/revoke (migration 0116).
pub mod verified_badge;
/// Workspace custom emoji (UUID-PK variant, migration 0115).
pub mod workspace_custom_emoji;

pub use state::AppState;
