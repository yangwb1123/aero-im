//! HTTP route definitions. WS upgrade lives in `ws::handler`.

use std::str::FromStr;

use futures::StreamExt as _;
use sha2::Digest as _;

use aero_auth::{AuthUser, LoginRequest, RegisterRequest};
use uuid::Uuid;

use aero_common::{
    BlobId, Error as AeroError, FileKind, MessageId, ParticipantId, Result as AeroResult, StreamProtocol, StreamStatus, WorkspaceId, WorkspaceRole,
};
use aero_live_whip::{accept_whep_offer, accept_whip_offer, SessionError, WhipError};
use aero_storage::{blob::NewBlob, stream::NewStream};
use axum::{
    extract::{Multipart, Path, Query, State},
    http::{header, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use serde::Deserialize;
use tower_http::compression::CompressionLayer;

use crate::error::ApiResult;
use crate::metrics;
use crate::routes::helpers::{merge_hits, parse_room_id, parse_room_kind};
use crate::state::AppState;
use crate::ws;

// ----- Request correlation ID middleware -----

/// Opaque correlation ID propagated through request extensions and echoed in
/// every response as `x-request-id`.  Handlers and middlewares that need to
/// surface it can extract it from `req.extensions()`.
#[derive(Clone)]
pub struct RequestId(pub String);

/// Middleware: read or generate a `x-request-id` header, attach a
/// [`RequestId`] extension, and echo the value in the response.
async fn inject_request_id(mut req: Request<axum::body::Body>, next: Next) -> Response {
    use tracing::Instrument as _;
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    req.extensions_mut().insert(RequestId(id.clone()));
    // Handle the request inside a span carrying `request_id`, so every structured
    // log line emitted while serving it is tagged with the same id echoed back in
    // the `x-request-id` response header — ops can pivot from the header to the
    // logs (ROADMAP5 方向二: log↔request correlation). The default `fmt` formatter
    // renders active span fields, so no log-format change is needed.
    let span = tracing::info_span!("http_request", request_id = %id);
    // Continue an upstream distributed trace when the caller sends a W3C
    // `traceparent` header, so a request's span (and everything it publishes onto
    // the bus) nests under the caller's trace (ROADMAP5 方向二).
    if let Some(tp) = req.headers().get("traceparent").and_then(|v| v.to_str().ok()) {
        aero_common::telemetry::set_span_parent_from_traceparent(&span, tp);
    }
    let mut res = next.run(req).instrument(span).await;
    if let Ok(v) = HeaderValue::from_str(&id) {
        res.headers_mut().insert("x-request-id", v);
    }
    res
}

pub fn build(state: AppState) -> Router {
    let mut router = Router::new()
        // Health probes — extracted to routes/health.rs
        .merge(crate::routes::health::routes())
        // Auth
        .route("/api/auth/register", post(auth_register))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/login-history", get(auth_login_history))
        .route("/api/me", get(me).patch(update_me))
        // Rooms
        .route("/api/rooms", post(create_room).get(list_rooms))
        .route("/api/rooms/:id/members", post(add_member))
        .route("/api/rooms/:id/messages", get(room_history))
        .route("/api/rooms/:id/changes", get(room_changes))
        .route("/api/rooms/:id/read", post(mark_read))
        .route("/api/rooms/:id/receipts", get(list_receipts))
        .route("/api/rooms/:id/delivery-cursor", get(get_delivery_cursor))
        .route("/api/rooms/:id/search", post(room_search))
        // Messages
        .route(
            "/api/messages/:id",
            axum::routing::get(get_message).patch(edit_message).delete(delete_message),
        )
        .route("/api/messages/:id/reactions", post(toggle_reaction))
        .route("/api/messages/reactions", post(reactions_batch))
        // Thread mute (mig 0088): mute/unmute the thread rooted at this message so
        // the caller stops receiving reply notifications for it. The storage +
        // dispatcher half (ThreadMuteRepo + notification suppression) was already
        // wired; these endpoints are the missing user-facing control surface. The
        // GET lists the muters of this thread (the only listing ThreadMuteRepo
        // exposes — `muted_by(root)`; there is no per-participant "threads I muted"
        // query, so no `/api/me/thread-mutes` route is offered).
        .route(
            "/api/threads/:root_message_id/mute",
            post(thread_mute).delete(thread_unmute),
        )
        .route("/api/threads/:root_message_id/mutes", get(thread_muters))
        // Blobs
        .route("/api/blobs", post(blob_upload))
        .route("/api/blobs/:id", get(blob_download))
        // AI
        .merge(crate::routes::ai::routes())
        // Live streams
        .merge(crate::routes::live::routes())
        // WHIP / WHEP — body is SDP text, response is SDP text
        .route("/whip/:stream_key", post(whip_post))
        .route("/whip/resource/:stream_key", axum::routing::delete(whip_delete))
        .route("/whep/:stream_id", post(whep_post))
        // Agents / Bots
        .merge(crate::routes::agents::routes())
        .route("/api/bots", post(bot_create).get(bot_list))
        .route("/api/bots/:id/token", post(bot_rotate_token))
        .route("/api/bots/:id/subscriptions", get(bot_list_subscriptions).post(bot_create_subscription))
        .route("/api/bots/:id/subscriptions/:sub_id", axum::routing::delete(bot_delete_subscription))
        .route("/api/bots/:id/deliveries", get(bot_list_deliveries))
        .route("/api/participants", get(search_participants))
        .route("/api/participants/:id", get(get_participant))
        .route("/api/rooms/:id/members/list", get(list_room_members))
        // MLS E2E — extracted to routes/mls.rs
        .merge(crate::routes::mls::routes())
        // RTC config
        .merge(crate::routes::rtc::routes())
        // WebSocket
        .route("/ws", get(ws::handler))
        // Workspace / Org management (ROADMAP 方向一 — multi-tenant foundation).
        // Defined alongside their RBAC guards in `crate::workspaces`.
        .merge(crate::workspaces::routes())
        .merge(crate::ai_usage::routes())
        .merge(crate::collab::routes())
        // Channel management (public/private, join/leave, archive, topic/desc).
        .merge(crate::channels::routes())
        // Webhooks (incoming inbound-message hooks + outgoing event delivery).
        .merge(crate::webhooks::routes())
        // SSO via OIDC: ID-token login + JIT provisioning (POST /api/auth/oidc).
        .merge(crate::sso::routes())
        // SSO via SAML 2.0 (enterprise IdPs): SP metadata + AuthnRequest redirect
        // + ACS. Signature verification is FAIL-CLOSED until a vetted XML-DSig
        // verifier is wired (see `crate::saml`). GET /saml/metadata, GET
        // /saml/login, POST /saml/acs.
        .merge(crate::saml::routes())
        // SCIM 2.0 provisioning (RFC 7643/7644). Bearer-token (not JWT) auth on
        // /scim/v2/*; AuthUser-gated token mint/revoke. See `crate::scim`.
        .merge(crate::scim::routes())
        // Scheduled messages + reminders ("Send later"). Background delivery is
        // driven by `crate::scheduled::run_scheduled_dispatcher`.
        .merge(crate::scheduled::routes())
        // Workspace invitations / shareable invite links (ROADMAP 方向一).
        .merge(crate::invitations::routes())
        // Cross-room (workspace-wide) message search, membership-scoped.
        .merge(crate::search::routes())
        // Saved searches: per-user, workspace-scoped named queries — list/run/
        // delete. Running reuses the same membership-scoped cross-room search.
        .merge(crate::saved_searches::routes())
        // Notification preferences: per-channel mute + per-user Do-Not-Disturb.
        .merge(crate::notif_prefs::routes())
        // Saved items / bookmarks (personal save-for-later, per-user cross-room).
        .merge(crate::bookmarks::routes())
        // Workspace custom emoji (`:shipit:`): name → uploaded image blob.
        .merge(crate::emoji::routes())
        // Durable user custom status + presence preference (profile-visible;
        // distinct from the ephemeral Redis online tracking).
        .merge(crate::user_status::routes())
        // Personal Access Tokens: mint/list/revoke long-lived API credentials.
        // The minted token works as a bearer credential on every AuthUser route
        // (the extractor accepts a PAT wherever it accepts a JWT).
        .merge(crate::pat::routes())
        // In-room polls: create / vote / live tally / creator-close.
        .merge(crate::polls::routes())
        // Live-stream chat moderation: owner bans/timeouts viewers from danmaku.
        .merge(crate::stream_mod::routes())
        // Live-stream chat modes: owner sets slow-mode / follower-only /
        // subscriber-only (Twitch-style) for the danmaku chat.
        .merge(crate::stream_chat_modes::routes())
        // Stream VOD / recording: flag a stream for recording, finalize a stream
        // into a VOD, and list/get/delete recordings (each with a playback URL).
        .merge(crate::vod::routes())
        // Message forwarding / share: copy a message's content into another room
        // (Slack "Forward"), prefixed with a `forwarded_message` provenance card.
        .merge(crate::forward::routes())
        // Built-in slash-commands (/me, /shrug, /giphy, /remind): parse `/cmd args`
        // typed in a room and apply the effect off the hot send path.
        .merge(crate::commands::routes())
        // Server-persisted per-room composer drafts (private to the author, one
        // per (participant, room), upsert-replaces). Follows the user across
        // devices/reloads like Slack drafts.
        .merge(crate::drafts::routes())
        // Scheduled streams (live-event announcements): a member announces an
        // upcoming live stream (title/time/optional room); members list upcoming,
        // the creator cancels. Announcement record only — going live still uses
        // the existing `/api/streams` ingest path.
        .merge(crate::scheduled_streams::routes())
        // Single-channel guest accounts (To-B external collaborators). Admin-only
        // guest enrollment / listing / removal; defined alongside its RBAC guard
        // in `crate::guests`.
        .merge(crate::guests::routes())
        // AI-native on-demand message translation (reuses the caption-translation
        // backend seam; echoes the source when no LLM key is configured).
        .merge(crate::translate::routes())
        // "Remind me about this message" (Slack-style): set a relative-time reminder
        // anchored to a specific message; reuses the durable scheduler — no new table.
        .merge(crate::message_reminders::routes())
        // Per-user channel sidebar sections (Slack/Teams "sections"): a user groups
        // their channels into named, ordered, PRIVATE folders scoped to a workspace.
        // Pure organizational metadata over existing rooms.
        .merge(crate::channel_sections::routes())
        // ---- Wave 10 (0033-0038) ----
        // User groups (@-usergroups): workspace-scoped named member sets that can be
        // @-mentioned as one. CRUD + membership; mention fan-out wired in ImService.
        .merge(crate::user_groups::routes())
        // Per-user starred/favorite channels (flat list, distinct from sections).
        .merge(crate::favorites::routes())
        // Custom user profile fields (title/pronouns/timezone/phone/status) in a
        // side table — does not touch the participants row or `update_me`.
        .merge(crate::profiles::routes())
        // Message edit history: read prior versions of an edited message (capture
        // on edit is wired in ImService::edit_message).
        .merge(crate::message_history::routes())
        // Message permalink / jump-to-message: a target message plus a centered
        // window of surrounding context (Slack "jump to message").
        .merge(crate::message_context::routes())
        // Keyword / highlight alerts: per-user subscriptions that notify on match
        // (dispatch wired in ImService::dispatch_notifications).
        .merge(crate::keyword_alerts::routes())
        // Workspace announcements / banners: admin posts, members read active ones.
        .merge(crate::announcements::routes())
        // ---- Wave 11 ----
        // Per-channel Files tab: list file/media attachments shared in a room.
        .merge(crate::files::routes())
        // Stream/creator follow: follow a participant; followers are notified on go-live.
        .merge(crate::stream_follows::routes())
        // Thread follow/subscribe: follow a root message to be notified of new replies.
        .merge(crate::thread_subs::routes())
        // Mark-all-read: clear unread for a room or across all the caller's rooms.
        .merge(crate::read_all::routes())
        // ---- Wave 12 ----
        // Direct-message (1:1) find-or-create.
        .merge(crate::dm::routes())
        // Recurring scheduled messages (hourly/daily/weekly).
        .merge(crate::recurring::routes())
        // AI "catch me up": summarize the caller's unread in a room.
        .merge(crate::catchup::routes())
        // Reaction detail: who reacted with each emoji.
        .merge(crate::reaction_detail::routes())
        // Workspace default channels (admin-set; new members auto-join).
        .merge(crate::default_channels::routes())
        // ---- Wave 13 ----
        // Group DM (multi-person direct) find-or-create.
        .merge(crate::group_dm::routes())
        // AI action-item extraction from a channel.
        .merge(crate::action_items::routes())
        // Channel join requests (request → owner/admin approve/deny).
        .merge(crate::join_requests::routes())
        // Per-conversation export (single room/DM message history).
        .merge(crate::conversation_export::routes())
        // ---- Wave 14 ----
        // Two-factor auth (TOTP) self-management; login enforcement is in auth_login.
        .merge(crate::twofa::routes())
        // Workspace user deactivation (admin); access enforcement is in assert_room_access.
        .merge(crate::deactivation::routes())
        // Message templates / canned responses.
        .merge(crate::templates::routes())
        // ---- Wave 15 ----
        // Session management: access-token refresh + logout/revocation.
        .merge(crate::session::routes())
        // Advanced search operators (from:/in:/before:/after:).
        .merge(crate::search_advanced::routes())
        // AI smart replies: suggested reply options for a room.
        .merge(crate::smart_replies::routes())
        // Channel role management: view roles, change role, transfer ownership.
        .merge(crate::channel_roles::routes())
        // ---- Wave 16 ----
        // Channel canvas: per-channel collaborative documents (create/list/get/edit/delete).
        .merge(crate::canvas::routes())
        // Channel bookmarks / header links: pinned per-channel resources.
        .merge(crate::channel_bookmarks::routes())
        // Stream categories & discovery: browse live streams by category/tag.
        .merge(crate::stream_discovery::routes())
        // Creator subscriptions / membership tiers (recurring support; complements gifts).
        .merge(crate::subscriptions::routes())
        // Workspace analytics: admin-only aggregate stats (overview/top-channels/timeline).
        .merge(crate::analytics::routes())
        // People directory: searchable workspace member list surfacing profile fields.
        .merge(crate::directory::routes())
        // ---- Wave 17 ----
        // Out-of-office / auto-responder: self-service status CRUD (the bot delivers
        // auto-replies out-of-band; see crate::ooo_bot, spawned in the bin).
        .merge(crate::ooo::routes())
        // Org chart / manager hierarchy: set/clear manager; read manager/reports/chain.
        .merge(crate::org_chart::routes())
        // Legal hold / retention exemption: admin holds a room/workspace; held rooms
        // are excluded from the retention sweep (eDiscovery preservation).
        .merge(crate::legal_holds::routes())
        // Tasks / to-do tracker: durable assignable stateful room tasks.
        .merge(crate::tasks::routes())
        // Workspace-wide file browser: file attachments across the caller's rooms.
        .merge(crate::workspace_files::routes())
        // Approvals workflow (Lark 审批-lite): requester → single approver decision.
        .merge(crate::approvals::routes())
        // ---- Wave 18 ----
        // Call history / call-log: per-conversation list of persisted call sessions.
        .merge(crate::call_history::routes())
        // Stream key rotation / reset: owner rotates a leaked stream key.
        .merge(crate::stream_key::routes())
        // AI writing assistant ("help me write"): rewrite / tone / concise.
        .merge(crate::ai_rewrite::routes())
        // Live-stream clips: viewer-marked [start,end] ranges over the HLS playlist.
        .merge(crate::clips::routes())
        // Stream / creator analytics: owner-only per-stream aggregate dashboard.
        .merge(crate::stream_analytics::routes())
        // Mark message / conversation as unread: roll the read cursor backwards.
        .merge(crate::mark_unread::routes())
        // ---- Wave 19 ----
        // Per-channel retention override: room.retention_days beats the workspace default.
        .merge(crate::channel_retention::routes())
        // Information barriers / ethical walls: barred user-group pairs can't DM/share.
        .merge(crate::info_barriers::routes())
        // Snooze notifications: one-off pause until a timestamp (distinct from DND).
        .merge(crate::snooze::routes())
        // Workspace-wide RAG ask: cross-channel AI Q&A bounded by membership.
        .merge(crate::workspace_ask::routes())
        // ---- Wave 21 ----
        // Active session inventory + remote / global sign-out: list active
        // login sessions/devices, revoke one, or "sign out everywhere else".
        .merge(crate::sessions::routes())
        // Activity feed: durable per-participant notices (e.g. a followed creator
        // going live), distinct from the message+room-scoped notification inbox.
        .merge(crate::activity::routes())
        // ---- Wave 23 ----
        // Call-transcript persistence + post-call AI recap: read a call's persisted
        // final caption lines and the AI summary produced when the call ended. The
        // write path is the WS caption relay + CallEnd hook (see crate::ws).
        .merge(crate::call_recap::routes())
        // ---- Wave 24 ----
        // Workspace-wide 2FA enforcement: admin toggles require_2fa; the gate is in
        // ImService::assert_room_access (a require-2FA member without activated TOTP
        // is locked out of room data until they enroll via /api/me/2fa).
        .merge(crate::workspace_security::routes())
        // GDPR personal data export: GET /api/me/export (data portability).
        .merge(crate::me_export::routes())
        // Mobile push token registration: POST/DELETE/GET /api/me/push-token.
        .merge(crate::push_tokens::routes())
        // AI dead-letter queue admin API (ROADMAP 方向五).
        .merge(crate::ai_dlq::routes())
        // Admin force-revoke a member's login sessions (offboarding).
        .merge(crate::admin_sessions::routes())
        // ---- Wave 16 Round 9 ----
        // Per-room online roster + count: who is currently connected via WebSocket
        // in a room (in-process hub view). Useful for sidebar decoration and
        // mobile background badge polling without a persistent WS connection.
        .merge(crate::online::routes())
        // ---- Live stream metadata edit ----
        // Owner-only PATCH /api/streams/:id to rename a stream while live.
        .merge(crate::stream_meta::routes())
        // ---- Workspace IP / network allowlist (authorized networks) ----
        // Admin-gated CRUD over a workspace's authorized CIDR ranges.
        .merge(crate::ip_allowlist::routes())
        // ---- AI-native cluster: thread summary, scheduled digests, find-expert ----
        // Thread-scoped AI summarization: POST /api/messages/:id/thread-summary.
        .merge(crate::thread_summarize::routes())
        // Scheduled/recurring AI digests: POST/GET /api/digests, DELETE /api/digests/:id.
        // The background dispatcher is spawned in bin/aero-server.rs.
        .merge(crate::digests::routes())
        // Find-expert: POST /api/workspaces/:id/find-expert (workspace-member gated).
        .merge(crate::find_expert::routes())
        // AI recommendations (workspace-member gated): suggested channels & people
        // to follow by affinity to the caller's own activity.
        //   GET /api/workspaces/:id/recommendations/channels
        //   GET /api/workspaces/:id/recommendations/people
        .merge(crate::recommendations::routes())
        // ---- Per-workspace rate-limit tiers (ROADMAP3 方向五 — 租户公平) ----
        // Owner-only PUT + member-readable GET /api/workspaces/:id/rate-tier.
        // Enforcement call sites live in the high-traffic handlers below
        // (room_history / room_search / blob_* / WS SendMessage).
        .merge(crate::ws_rate::routes())
        // ---- Collaboration parity batch (Slack/Lark/Teams) ----
        // Per-message read receipts ("Seen by …"): mark an individual message
        // seen + read the reader list. Broadcasts RoomEvent::MessageSeen.
        .merge(crate::message_receipts::routes())
        // Bookmark folders/collections: group personal saved items into named,
        // ordered collections; assign/clear a saved message's collection.
        .merge(crate::bookmark_collections::routes())
        // Multi-channel broadcast: post a copy of a message into several rooms at
        // once (forward fan-out), with per-target success/failure reporting.
        .merge(crate::broadcast::routes())
        // ---- Interactive-live / creator parity (migrations 0079-0083) ----
        // Hype train / combo gifts: escalating momentum the gift path feeds into.
        // GET the current session; the gift handlers call crate::hype_train::on_gift.
        .merge(crate::hype_train::routes())
        // Raids: a source stream's owner sends viewers to a target stream at end,
        // recorded + broadcast (StreamEvent::Raid) so watchers redirect.
        .merge(crate::raids::routes())
        // VOD chapters / markers: owner-added timestamped table-of-contents on a
        // recording; viewers list them, playback seeks the existing HLS playlist.
        .merge(crate::vod_chapters::routes())
        // Stream-moderator role assignment (distinct from chat bans): owner grants
        // a mod role; a moderator gains the same chat-ban authority as the owner.
        .merge(crate::stream_moderators::routes())
        // ---- Operability: outbound-webhook delivery log + DLQ + requeue (admin) ----
        // GET /api/webhooks/:id/deliveries[/dead], POST /api/webhook-deliveries/:id/requeue.
        .merge(crate::webhook_admin::routes())
        // ---- Operability: per-tenant usage report (admin) ----
        // GET /api/workspaces/:id/admin/usage — messages/AI tokens/blobs/members.
        .merge(crate::usage_report::routes())
        // ---- Interactive message blocks (Slack Block Kit-lite, migration 0086) ----
        // POST /api/messages/:id/interact records a click/option-pick on an
        // interactive Button/Select block (404 unless the message carries that
        // action_id), broadcasting RoomEvent::Interaction so the poster's bot/app
        // sees it live; GET /api/messages/:id/interactions lists them.
        .merge(crate::interactions::routes())
        // ---- ROADMAP6 Lane C ----
        // Clip collections / playlists: CRUD for per-user named, ordered groups
        // of stream clips (YouTube-playlist-style). Fully auth-gated.
        .merge(crate::clip_collections::router(state.clone()))
        // Clip share endpoint (POST /api/clips/:cid/share is already in
        // crate::clips::routes()). Public clip slug route: GET /clips/:slug
        // has no auth extractor so any user (even unauthenticated) can view
        // clip metadata via a shared link.
        .merge(crate::clips::public_routes())
        // ---- ROADMAP8 — user blocking ----
        // POST/DELETE /api/users/:user_id/block, GET /api/me/blocks.
        .merge(crate::user_blocks::router())
        // ---- ROADMAP9 — extended subscription tier levels (migration 0109) ----
        // POST/GET /api/creators/:id/subscription-tiers, DELETE /api/creators/:id/subscription-tiers/:tid
        .merge(crate::subscription_tiers::routes())
        // ---- ROADMAP10 — date-range search (no migration): handled via ParsedQuery extension in search_advanced.
        // ---- ROADMAP10 — gifted-sub leaderboard: already in subscriptions::routes().
        // ---- ROADMAP10 — clip tags (migration 0110): new routes added to clips::routes().
        // ---- ROADMAP10 — auto-mod rules (migration 0111) ----
        // POST/GET /api/workspaces/:id/auto-mod-rules, DELETE /api/workspaces/:id/auto-mod-rules/:rid
        .merge(crate::auto_mod::routes())
        // ---- ROADMAP10 — user reports (migration 0112) ----
        // POST /api/users/:id/report, GET/PATCH /api/workspaces/:id/user-reports[/:rid]
        .merge(crate::user_reports::routes())
        // ---- ROADMAP10 — channel-points expiry info (migration 0113) ----
        // GET /api/creators/:id/points/expiry
        .merge(crate::point_expiry::routes())
        // ---- ROADMAP11 — workspace custom emoji UUID-PK variant (migration 0115) ----
        // POST/GET /api/workspaces/:id/custom-emoji, DELETE /api/workspaces/:id/custom-emoji/:eid
        .merge(crate::workspace_custom_emoji::routes())
        // ---- ROADMAP11 — creator verified badge (migration 0116) ----
        // PATCH /api/admin/participants/:id/verify (admin only)
        // GET /api/participants/:id/verified (public read)
        .merge(crate::verified_badge::routes())
        .merge(crate::verified_badge::get_routes())
        // ---- ROADMAP11 — bulk unread summary (no migration) ----
        // GET /api/me/unread-summary
        .merge(crate::unread_summary::routes())
        // ---- internal cross-node call-bridge subscribe (方向五, secret-gated) ----
        .merge(crate::call_bridge_subscribe::routes())
        // ---- channel points (mig 0089) ----
        .merge(crate::channel_points::routes())
        // ---- ban appeals (mig 0091) ----
        .merge(crate::ban_appeals::routes())
        // ---- stream goals (mig 0090) ----
        .merge(crate::goals::routes())
        // ---- live predictions (mig 0092) ----
        .merge(crate::predictions::routes())
        // ---- message reports / moderation queue (mig 0093) ----
        .merge(crate::message_reports::routes())
        // ---- thread title (AI, no migration) ----
        .merge(crate::thread_title::routes())
        // ---- message sentiment (AI, no migration) ----
        .merge(crate::message_sentiment::routes())
        // ---- OpenAPI 3.0 spec (public, no auth) ----
        // GET /api/openapi.json returns the static OpenAPI document so API clients
        // and documentation generators can introspect the surface without credentials.
        .merge(crate::openapi::router());

    // Prometheus scrape endpoint (ROADMAP 方向四). Mounted unless disabled; the
    // handler self-gates on an optional bearer token. Left here (not behind the
    // auth extractor) so a scraper without a participant token can reach it,
    // mirroring `/health`.
    if state.metrics.enabled {
        router = router.route("/metrics", get(metrics::metrics_handler));
    }

    // ---- Request correlation ID + HTTP compression ----
    // inject_request_id: reads or generates x-request-id, attaches RequestId
    //   extension, echoes the value in the response header.
    // CompressionLayer: gzip-compress eligible responses (text/json/html).
    // Both layers wrap the whole router (including the metrics endpoint) so
    // every response is correlated and eligible for compression.
    router
        .layer(middleware::from_fn(inject_request_id))
        .layer(CompressionLayer::new())
        .with_state(state)
}

// ----- Auth -----

/// Best-effort: record an active login session keyed on the refresh token's hash
/// (so it lines up with the revoked-token check), pulling the `User-Agent` from
/// request headers when present. A failure here must NOT fail the login /
/// registration, so any error is logged and swallowed. Wave 21.
async fn record_session(
    s: &AppState,
    participant: ParticipantId,
    refresh_token: &str,
    headers: &header::HeaderMap,
) {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let hash = aero_storage::revoked_token::hash_token(refresh_token);
    if let Err(e) = aero_storage::SessionRepo::new(s.pg.clone())
        .record(participant, &hash, ua)
        .await
    {
        tracing::warn!(error = ?e, %participant, "auth session record failed");
    }
}

/// The client's source IP, read from the standard reverse-proxy forwarding
/// headers (`X-Forwarded-For`'s first hop, then `X-Real-IP`). Returns `None` when
/// neither is present (e.g. a direct connection in dev) — callers treat an absent
/// IP as "unobservable", never as a security signal.
///
/// TRUST ASSUMPTION: this value is only trustworthy when a trusted reverse proxy
/// **overwrites** `X-Forwarded-For` with the real client address (the standard
/// cloud-LB / nginx `proxy_set_header` setup). A client can forge the header, so
/// the IP fed to the new-login-IP signal is *defence-in-depth*, not an authz
/// input: a forged-known-IP can only suppress a new-IP alert (a false negative),
/// never grant access. Deploy behind a header-rewriting proxy for the signal to
/// be reliable.
fn client_ip(headers: &header::HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        // `X-Forwarded-For: client, proxy1, proxy2` — the client is the first hop.
        .and_then(|s| s.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            headers.get("x-real-ip").and_then(|v| v.to_str().ok()).map(str::trim)
        })
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Best-effort: record this successful login in the IP/device history and, when
/// it comes from an IP this account has never used before (and the account has
/// prior logins — so a first-ever login isn't flagged), emit a security warning +
/// audit event (ROADMAP5 方向五). The new-IP signal is the canonical account-
/// takeover tell; impossible-travel/geo-velocity builds on this same history but
/// needs a geo-IP database (a deployment seam). Never fails the login.
async fn record_login_event(
    s: &AppState,
    participant: ParticipantId,
    workspace: Option<aero_common::WorkspaceId>,
    headers: &header::HeaderMap,
) {
    let ip = client_ip(headers);
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let repo = aero_storage::LoginEventRepo::new(s.pg.clone());

    // Flag a login from a new IP — but only for an account that already has
    // history (every IP is "new" on the first-ever login).
    match (repo.is_known_ip(participant, ip.as_deref()).await, repo.has_any(participant).await) {
        (Ok(false), Ok(true)) => {
            tracing::warn!(%participant, ip = ip.as_deref().unwrap_or("?"), "login from a new IP");
            if let Some(ws) = workspace {
                let _ = aero_storage::AuditRepo::new(s.pg.clone())
                    .append(
                        ws,
                        Some(participant),
                        "auth.login.new_ip",
                        ip.as_deref(),
                        serde_json::json!({ "ip": ip, "user_agent": ua }),
                    )
                    .await;
            }
        }
        (Err(e), _) | (_, Err(e)) => {
            tracing::warn!(error = ?e, %participant, "new-IP login check failed");
        }
        _ => {}
    }

    if let Err(e) = repo.record(participant, ip.as_deref(), ua).await {
        tracing::warn!(error = ?e, %participant, "login event record failed");
    }
}

async fn auth_register(
    State(s): State<AppState>,
    headers: header::HeaderMap,
    Json(req): Json<RegisterRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let out = s.auth.register(req).await?;
    // Enroll the brand-new participant into the legacy/default workspace so they
    // immediately belong to a tenant — otherwise they could not create rooms
    // (`create_room_in_workspace` requires workspace membership). `add_member` is
    // an idempotent `ON CONFLICT DO NOTHING` upsert, so a retry is harmless. We
    // propagate failures (rather than swallowing) to preserve the invariant
    // "registered ⇒ workspace member"; the default workspace is guaranteed to
    // exist by migration 0006's backfill.
    s.workspaces
        .add_member(DEFAULT_WORKSPACE_ID, out.participant.id, WorkspaceRole::Member)
        .await
        .map_err(AeroError::from)?;
    // Onboarding: auto-join the new participant into the default workspace's
    // default channels (Wave 12). Best-effort — never fails registration.
    crate::default_channels::auto_join_defaults(&s, DEFAULT_WORKSPACE_ID, out.participant.id).await;
    // Wave 21: record the active session (best-effort; never fails registration).
    record_session(&s, out.participant.id, &out.refresh_token, &headers).await;
    Ok(Json(serde_json::json!({
        "access_token": out.access_token,
        "refresh_token": out.refresh_token,
        "participant": out.participant,
    })))
}

/// Login request — email + password, plus an optional `totp` code that is
/// *required* when the account has activated two-factor auth (Wave 14).
#[derive(Deserialize)]
struct LoginReq {
    email: String,
    password: String,
    #[serde(default)]
    totp: Option<String>,
}

async fn auth_login(
    State(s): State<AppState>,
    headers: header::HeaderMap,
    Json(req): Json<LoginReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let email = req.email.trim().to_lowercase();
    let out = match s
        .auth
        .login(LoginRequest { email: email.clone(), password: req.password })
        .await
    {
        Ok(u) => u,
        Err(e) => {
            // 方向五: record the failed attempt for anomaly detection. The
            // in-process LoginThrottle (per-node, restart-volatile, unqueryable)
            // already saw this failure; here we leave a *durable, queryable,
            // cross-node* trail (attempted account + source IP + user-agent) so a
            // slow credential-stuffing run that stays under the lockout threshold
            // is still detectable. Keyed on the *attempted* email (which may not
            // name a real account — enumeration is recorded too), not a
            // participant_id. Fail-OPEN: a recording error must never block the
            // login response, so we only warn.
            let ip = client_ip(&headers);
            let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
            if let Err(rec_err) = aero_storage::LoginFailureRepo::new(s.pg.clone())
                .record(&email, ip.as_deref(), ua)
                .await
            {
                tracing::warn!(error = ?rec_err, "failed to record login failure");
            }
            return Err(e.into());
        }
    };
    // Two-factor enforcement (Wave 14): once a participant has ACTIVATED TOTP, a
    // valid current code must accompany the (already-verified) password. Returns
    // 401 `2fa_required` when the code is missing or wrong, so a stolen password
    // alone can't complete the login.
    let totp = aero_storage::TotpRepo::new(s.pg.clone());
    if totp.is_activated(out.participant.id).await.map_err(AeroError::from)? {
        let secret = totp
            .get_secret(out.participant.id)
            .await
            .map_err(AeroError::from)?
            .ok_or_else(|| AeroError::Internal(anyhow::anyhow!("2FA activated without a secret")))?;
        let now = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0);
        let code = req.totp.as_deref().unwrap_or("");
        if !aero_auth::totp::verify(&secret, code, now) {
            // A wrong/missing second factor is a FAILED login: count it on the
            // lockout throttle (the password success was deferred, not recorded) so
            // the code can't be brute-forced, and leave the same durable trail a
            // bad password leaves. Both best-effort — never block the 401 response.
            s.auth.finalize_login(&email, false).await;
            let ip = client_ip(&headers);
            let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
            if let Err(rec_err) = aero_storage::LoginFailureRepo::new(s.pg.clone())
                .record(&email, ip.as_deref(), ua)
                .await
            {
                tracing::warn!(error = ?rec_err, "failed to record 2FA login failure");
            }
            return Err(AeroError::Unauthorized("2fa_required".into()).into());
        }
    }
    // All gates passed (password + any 2FA): NOW record the success, clearing the
    // per-account lockout counter (deferred from `auth.login` so a failed 2FA above
    // counted as a failure instead of resetting it).
    s.auth.finalize_login(&email, true).await;
    // Wave 21: record the active session (best-effort; never fails login). Done
    // only after 2FA passes, so a half-completed login leaves no session row.
    record_session(&s, out.participant.id, &out.refresh_token, &headers).await;
    // ROADMAP5 方向五: record the login in the IP/device history + flag a new-IP
    // login (best-effort; never fails login). The audit event is scoped to the
    // default workspace (login is workspace-agnostic — a participant can belong to
    // several; the all-zero default is where account-level security events land).
    record_login_event(&s, out.participant.id, Some(DEFAULT_WORKSPACE_ID), &headers).await;
    Ok(Json(serde_json::json!({
        "access_token": out.access_token,
        "refresh_token": out.refresh_token,
        "participant": out.participant,
    })))
}

/// `GET /api/auth/login-history` — the caller's recent successful logins (IP +
/// user-agent + time), newest first, for a "recent login activity" view
/// (ROADMAP5 方向五). Owner-scoped: a participant only ever sees their own.
async fn auth_login_history(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let events = aero_storage::LoginEventRepo::new(s.pg.clone())
        .recent(auth.participant_id, 50)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "logins": events })))
}

async fn me(State(s): State<AppState>, auth: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    let p = s
        .participants
        .get(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;
    Ok(Json(serde_json::to_value(p).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct UpdateMeReq {
    #[serde(default)]
    display_name: Option<String>,
    /// Outer Option = field present; inner Option = nullable on the wire.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    avatar_url: Option<Option<String>>,
}

fn deserialize_optional_field<'de, D, T>(d: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

async fn update_me(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<UpdateMeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let name = req.display_name.as_deref().map(|n| n.trim()).filter(|n| !n.is_empty());
    if let Some(n) = name {
        if n.len() > 64 {
            return Err(AeroError::Invalid("display_name too long".into()).into());
        }
    }
    let url = req.avatar_url.map(|inner| inner.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()));
    let url_ref = url.as_ref().map(|inner| inner.as_deref());
    let updated = s
        .participants
        .update_profile(auth.participant_id, name, url_ref)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;
    // ROADMAP6 方向四: drop the just-updated profile from the per-process cache so
    // the next read re-fetches the new display_name/avatar instead of serving the
    // stale TTL window. (Other nodes still fall back to the 60s TTL — acceptable
    // for display names, as documented in `participant_cache`.)
    s.participant_cache.invalidate(&auth.participant_id);
    Ok(Json(serde_json::to_value(updated).map_err(AeroError::from)?))
}

// ----- Rooms -----

/// The legacy / default workspace (tenant) that all pre-tenancy data was
/// backfilled into by migration `0006_workspaces.sql` — the all-zero UUID, i.e.
/// the `WorkspaceId` whose underlying u128 is 0. Single-tenant clients that omit
/// a `workspace_id` (room creation) or `?workspace_id=` (room listing) operate
/// against this workspace, so existing callers keep working unchanged while the
/// required `rooms.workspace_id` (NOT NULL, no default) is always supplied.
const DEFAULT_WORKSPACE_ID: WorkspaceId = WorkspaceId(ulid::Ulid(0));

/// Resolve the workspace for a room operation: the explicitly-requested one when
/// present, else [`DEFAULT_WORKSPACE_ID`]. Pure, so the "provided vs absent"
/// default-selection rule is unit-tested offline (Postgres absent in CI).
///
/// # Errors
/// [`AeroError::Invalid`] when a present id fails to decode as a [`WorkspaceId`].
fn resolve_workspace_id(requested: Option<&str>) -> AeroResult<WorkspaceId> {
    match requested {
        Some(raw) => {
            WorkspaceId::from_str(raw.trim())
                .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
        }
        None => Ok(DEFAULT_WORKSPACE_ID),
    }
}

#[derive(Deserialize)]
struct CreateRoomReq {
    kind: String,
    name: Option<String>,
    /// Optional tenant the channel is created in. Absent ⇒ [`DEFAULT_WORKSPACE_ID`]
    /// (keeps single-tenant clients working). The handler routes through
    /// [`ImService::create_room_in_workspace`](aero_im_core::ImService::create_room_in_workspace)
    /// either way, so the room always carries its required `workspace_id`.
    #[serde(default)]
    workspace_id: Option<String>,
}

async fn create_room(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateRoomReq>,
) -> ApiResult<Response> {
    let kind = parse_room_kind(&req.kind)?;
    let workspace = resolve_workspace_id(req.workspace_id.as_deref())?;
    // Normalize the name like create_workspace: trim, treat blank as "no name"
    // (DM/group rooms are legitimately unnamed), and cap length so a malformed or
    // multi-megabyte name can't be persisted + rendered. `rooms.name` is nullable.
    let name = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    if let Some(n) = name {
        if n.chars().count() > 128 {
            return Err(AeroError::Invalid("room name too long (max 128 chars)".into()).into());
        }
    }
    // Tenant choke point: verifies workspace membership + channel-create privilege
    // and persists `rooms.workspace_id` (fixes the NOT-NULL room-create regression
    // the old `create_room` hit after migration 0006).
    let room = s
        .im
        .create_room_in_workspace(auth.participant_id, workspace, kind, name.map(str::to_owned))
        .await?;
    let body = Json(serde_json::to_value(room).map_err(AeroError::from)?).into_response();
    // Per-tenant HTTP metrics (response-extension pass-through): we already
    // resolved the owning workspace above, so stamp it for `http_metrics_layer`.
    // Cheap + bounded; ignored unless `AERO_PER_TENANT_METRICS` is on.
    Ok(metrics::attach_workspace_label(body, workspace))
}

#[derive(Deserialize)]
struct ListRoomsQuery {
    /// Optional tenant scope. Present ⇒ only the caller's rooms in that workspace
    /// (`rooms_for_in_workspace`); absent ⇒ all the caller's rooms (legacy behavior).
    #[serde(default)]
    workspace_id: Option<String>,
}

async fn list_rooms(
    State(s): State<AppState>,
    auth: AuthUser,
    Query(q): Query<ListRoomsQuery>,
) -> ApiResult<Response> {
    // Scoped to a workspace when `?workspace_id=` is given; otherwise unchanged
    // (every room the caller belongs to, across tenants).
    let (rooms, scope) = match q.workspace_id.as_deref() {
        Some(raw) => {
            let ws = WorkspaceId::from_str(raw.trim())
                .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?;
            let rooms = s
                .rooms
                .rooms_for_in_workspace(auth.participant_id, ws)
                .await
                .map_err(AeroError::from)?;
            // Only this branch knows a single owning tenant; the cross-tenant
            // listing below stays un-`workspace`-labeled (no marker stamped).
            (rooms, Some(ws))
        }
        None => (s.im.list_my_rooms(auth.participant_id).await?, None),
    };
    let body = Json(serde_json::to_value(rooms).map_err(AeroError::from)?).into_response();
    Ok(match scope {
        Some(ws) => metrics::attach_workspace_label(body, ws),
        None => body,
    })
}

#[derive(Deserialize)]
struct AddMemberReq {
    participant_id: String,
}

async fn add_member(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<AddMemberReq>,
) -> ApiResult<axum::http::StatusCode> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard: the actor must belong to BOTH the room's workspace and the
    // room itself before they may add anyone. `ImService::add_member` re-checks
    // the actor's room membership (a distinct, retained check).
    s.im.assert_room_access(auth.participant_id, room).await?;
    let member = ParticipantId::from_str(&req.participant_id)
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))?;
    s.im.add_member(auth.participant_id, room, member).await?;
    s.room_member_cache.invalidate(&room);
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct HistoryQuery {
    /// Backward keyset cursor (exclusive): page toward older messages.
    before: Option<String>,
    /// Forward keyset cursor (exclusive): catch up on messages created AFTER this
    /// id, ascending — the reconnect-backfill complement of `before`
    /// (ROADMAP 方向五). Mutually exclusive with `before`.
    since: Option<String>,
    limit: Option<i64>,
}

/// Default history page size when the client omits `limit`.
const DEFAULT_HISTORY_LIMIT: i64 = 100;
/// Hard ceiling on a history page, mirroring the clamp the storage keyset
/// queries (`list_recent` / `list_since`) apply. Applied here too so the cap is
/// validated at the edge and unit-testable without a database.
const MAX_HISTORY_LIMIT: i64 = 200;

/// Resolve the effective page size: default when absent, clamped into
/// `[1, MAX_HISTORY_LIMIT]`. Pure, so the cap/floor is unit-tested offline.
#[must_use]
fn history_limit(requested: Option<i64>) -> i64 {
    requested.unwrap_or(DEFAULT_HISTORY_LIMIT).clamp(1, MAX_HISTORY_LIMIT)
}

/// Parse an optional `MessageId` cursor query param, mapping a decode failure to
/// an `Invalid` API error tagged with `field` (e.g. `"before"` / `"since"`).
fn parse_cursor(raw: Option<&str>, field: &str) -> AeroResult<Option<MessageId>> {
    raw.map(|s| MessageId::from_str(s.trim()))
        .transpose()
        .map_err(|e| AeroError::Invalid(format!("{field} id: {e}")))
}

async fn room_history(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard: workspace + room membership. Subsumes the bare room-membership
    // check the forward (`since`) branch used to do, and complements the
    // membership check `ImService::history` does on the backward (`before`) path.
    s.im.assert_room_access(auth.participant_id, room).await?;
    // Tenant fairness (ROADMAP3 方向五): charge this read against the room's
    // workspace budget — AFTER the access check so non-members cannot drain a
    // victim workspace's budget by spamming its room ids.
    crate::ws_rate::check_ws_rate_room(&s, room).await?;
    let limit = history_limit(q.limit);
    // `before` pages backward, `since` pages forward — combining them is
    // ambiguous, so reject rather than silently pick one.
    if q.before.is_some() && q.since.is_some() {
        return Err(AeroError::Invalid("before and since are mutually exclusive".into()).into());
    }
    let since = parse_cursor(q.since.as_deref(), "since")?;

    if let Some(after) = since {
        // Forward catch-up (ROADMAP 方向五). Access already asserted above;
        // `ImService::history` only exposes the backward path, so read forward here.
        // KEYSET page: honour the clamped `limit` and return at most that many,
        // ascending by id. The client continues by passing the last returned id as
        // the next `since`, stopping when a page is shorter than `limit` (the web
        // client's `pullRoomSince` does exactly this). Previously `limit` was
        // discarded and the query hardcoded `LIMIT 500`, so a continuation past 500
        // silently truncated with no signal — messages 501..N were lost for any
        // client that trusted the (incorrect) "returns all" contract.
        let msgs = s.messages.list_since(room, after, limit).await.map_err(AeroError::from)?;
        return Ok(Json(serde_json::to_value(msgs).map_err(AeroError::from)?));
    }

    let before = parse_cursor(q.before.as_deref(), "before")?;
    let msgs = s.im.history(auth.participant_id, room, before, limit).await?;
    Ok(Json(serde_json::to_value(msgs).map_err(AeroError::from)?))
}

/// Query for the change-replay endpoint: an RFC3339 `since` instant + optional
/// `limit`.
#[derive(Deserialize)]
struct ChangesQuery {
    since: String,
    limit: Option<i64>,
}

/// `GET /api/rooms/:id/changes?since=<rfc3339>` — messages edited or deleted
/// since `since`, so a client reconnecting after offline edits/deletes can
/// converge on mutations to messages it already holds (ROADMAP 方向一). The
/// message backfill (`/messages?since=<id>`) only covers NEW messages; this is
/// its companion. Tombstones (deleted messages) are included; the client removes
/// those and replaces the rest.
async fn room_changes(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Query(q): Query<ChangesQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard FIRST (membership + deactivation), then charge the read.
    s.im.assert_room_access(auth.participant_id, room).await?;
    crate::ws_rate::check_ws_rate_room(&s, room).await?;
    // `/changes` keys on MUTATION TIME (edited_at/deleted_at), not message id, so
    // edits/deletes to already-held messages (whose id is <= the client's cursor)
    // are still returned. Parse the RFC3339 instant the contract documents. (The
    // split had rewritten this to an id cursor, which silently broke the feature.)
    let since = time::OffsetDateTime::parse(
        q.since.trim(),
        &time::format_description::well_known::Rfc3339,
    )
    .map_err(|e| AeroError::Invalid(format!("since must be an RFC3339 timestamp: {e}")))?;
    let limit = history_limit(q.limit);
    let msgs = s
        .messages
        .changes_since(room, since, limit)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(msgs).map_err(AeroError::from)?))
}

// ----- Read receipts -----

#[derive(Deserialize)]
struct MarkReadReq {
    last_message_id: String,
}

async fn mark_read(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<MarkReadReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard (workspace + room membership) before recording a receipt.
    s.im.assert_room_access(auth.participant_id, room).await?;
    let mid = MessageId::from_str(&req.last_message_id)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let r = s.im.mark_read(auth.participant_id, room, mid).await?;
    Ok(Json(serde_json::to_value(r).map_err(AeroError::from)?))
}

async fn list_receipts(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard (workspace + room membership) — supersedes the prior bare
    // room-membership check.
    s.im.assert_room_access(auth.participant_id, room).await?;
    let rs = s.im.receipts_for(room).await?;
    Ok(Json(serde_json::to_value(rs).map_err(AeroError::from)?))
}

/// `GET /api/rooms/:id/delivery-cursor` — the caller's persisted DELIVERY cursor
/// for this room (ROADMAP 方向三·A): the Last-Known-Good `(message_id, seq)` the
/// client has ACKed receiving. Lets a client fetch its LKG over REST (e.g. an
/// offline-first client priming before opening the socket). Returns `null` when
/// the caller has never ACKed in the room. Member-gated via `assert_room_access`.
async fn get_delivery_cursor(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let cur = s
        .delivery_cursors
        .get(auth.participant_id, room)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(cur).map_err(AeroError::from)?))
}

// ----- Messages -----

#[derive(Deserialize)]
struct EditMessageReq {
    blocks: Vec<aero_common::Block>,
    /// Optimistic-lock check (migration 0157): the `version` the client last
    /// saw this message at. A concurrent edit that already bumped the version
    /// past this fails with 409 instead of silently overwriting it. Omitted by
    /// clients that haven't adopted the check yet — falls back to unprotected
    /// last-write-wins, matching pre-versioning behavior.
    #[serde(default)]
    expected_version: Option<i32>,
}

/// `GET /api/messages/:id` — fetch a single (non-deleted) message, gated on the
/// caller's access to its room. Backs deep-links/permalinks and matches the
/// operation the OpenAPI spec advertises. 404 when the message is missing or
/// soft-deleted; 403 when the caller can't see its room.
async fn get_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = MessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let m = s
        .messages
        .get(id)
        .await?
        .filter(|m| m.deleted_at.is_none())
        .ok_or_else(|| AeroError::NotFound(format!("message {id}")))?;
    s.im.assert_room_access(auth.participant_id, m.room_id).await?;
    Ok(Json(serde_json::to_value(m).map_err(AeroError::from)?))
}

async fn edit_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<EditMessageReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = MessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let m = s
        .im
        .edit_message(auth.participant_id, id, req.blocks, req.expected_version)
        .await?;
    Ok(Json(serde_json::to_value(m).map_err(AeroError::from)?))
}

async fn delete_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let id = MessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let started = std::time::Instant::now();

    // Pre-fetch the message before the delete so the audit trail can record
    // the room AND a content digest (the blocks are cleared by the soft-delete,
    // so this is the only chance to capture "what was deleted").
    let Some(pre) = s.messages.get(id).await.map_err(AeroError::from)? else {
        return Err(AeroError::NotFound(format!("message {id}")).into());
    };
    let room_id = pre.room_id;
    // Char-boundary-safe 120-char summary so the audit row stays compact.
    let digest: String = pre.searchable_text().chars().take(120).collect();

    match s.rooms.room_workspace(room_id).await.map_err(AeroError::from)? {
        // Tenant-owned room: transactional delete + audit (ROADMAP 第三版 方向五
        // 审计事务化) — if the audit row can't be written the delete rolls back
        // and the client gets a 5xx, never a silently-unaudited delete.
        Some(ws) => {
            // Mirrors `ImService::delete_message` authorization exactly:
            // re-deleting is an idempotent no-op (checked FIRST, so it never
            // 403s), then only the sender may delete.
            if pre.deleted_at.is_some() {
                return Ok(StatusCode::NO_CONTENT);
            }
            if pre.sender_id != auth.participant_id {
                return Err(AeroError::Forbidden("only sender may delete".into()).into());
            }
            let detail = serde_json::json!({ "room_id": room_id, "digest": digest });
            let deleted = s
                .messages
                .soft_delete_audited(id, ws, Some(auth.participant_id), detail)
                .await
                .map_err(AeroError::from)?;
            // `false` = lost a race with a concurrent delete — already gone, so
            // no event/metric replay (the winner emitted them).
            if deleted {
                s.im.broadcast_room_event(
                    room_id,
                    aero_common::RoomEvent::Deleted {
                        room_id,
                        message_id: id,
                        by: auth.participant_id,
                    },
                )
                .await;
                aero_common::metrics::inc_counter(
                    aero_common::metrics::names::MESSAGES_DELETED_TOTAL,
                    1,
                );
                // Metric parity with `ImService::delete_message`, which times
                // the legacy (non-audited) path under the same label.
                aero_common::metrics::observe_histogram_labeled(
                    aero_common::metrics::names::MESSAGE_PROCESSING_DURATION_SECONDS,
                    started.elapsed().as_secs_f64(),
                    &[("op", "delete")],
                );
            }
        }
        // Legacy room with no owning workspace: there is no audit trail to write
        // into, so keep the original (service) delete path unchanged.
        None => s.im.delete_message(auth.participant_id, id).await?,
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ToggleReactionReq {
    emoji: String,
}

async fn toggle_reaction(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<ToggleReactionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = MessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let op = s.im.toggle_reaction(auth.participant_id, id, &req.emoji).await?;
    Ok(Json(serde_json::json!({
        "message_id": id,
        "emoji": req.emoji,
        "op": op,
    })))
}

#[derive(Deserialize)]
struct ReactionsBatchReq {
    message_ids: Vec<String>,
}

/// Max message ids one reactions-batch request may resolve. The gateway body cap
/// bounds the request loosely; this is the explicit per-request ceiling so a
/// single call can't fan into an arbitrarily large `ANY($1)` scan.
const MAX_REACTIONS_BATCH: usize = 256;

async fn reactions_batch(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<ReactionsBatchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    if req.message_ids.len() > MAX_REACTIONS_BATCH {
        return Err(AeroError::Invalid(format!(
            "too many message_ids: {} (max {MAX_REACTIONS_BATCH})",
            req.message_ids.len()
        ))
        .into());
    }
    let ids: Vec<MessageId> = req
        .message_ids
        .iter()
        .map(|s| MessageId::from_str(s))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    // Membership-scoped: only reactions on messages in rooms the caller belongs to
    // (the storage JOIN room_members is the boundary). A message id the caller
    // can't access is silently absent — no cross-room reaction-count / reactor-id
    // leak (IDOR).
    let summaries = s.im.reactions_for_accessible(auth.participant_id, &ids).await?;
    let summaries_json: serde_json::Map<String, serde_json::Value> = summaries
        .into_iter()
        .map(|(mid, list)| {
            (
                mid.to_string(),
                serde_json::to_value(list).unwrap_or(serde_json::Value::Null),
            )
        })
        .collect();
    Ok(Json(serde_json::Value::Object(summaries_json)))
}

// ----- Thread mute (migration 0088) -----
//
// The storage layer (`aero_storage::ThreadMuteRepo`) and the notification
// suppression in the dispatcher were already wired; what was missing was any way
// for a user to CREATE or REMOVE a mute. These three handlers are that control
// surface. A thread is identified by its root message id. `ThreadMuteRepo` is
// constructed inline from the shared pool (`s.pg`), mirroring how other handlers
// here build repos on demand — no `AppState` change. Resolving the root message's
// room (via `s.messages.get`, the same lookup `delete_message` uses) lets us
// `assert_room_access` first, so a caller can only mute a thread in a room they
// belong to. An unknown root message id is a 404.

/// Resolve the room owning `root` (a 404 when the message does not exist), then
/// assert the caller may access it. Shared by the mute/unmute/list handlers so the
/// access policy is identical across all three.
async fn assert_thread_room_access(
    s: &AppState,
    participant: ParticipantId,
    root: MessageId,
) -> AeroResult<()> {
    let msg = s
        .messages
        .get(root)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("message {root}")))?;
    s.im.assert_room_access(participant, msg.room_id).await
}

/// `POST /api/threads/:root_message_id/mute` — mute the thread rooted at this
/// message for the caller, so they stop receiving reply notifications for it. The
/// caller must be able to access the root message's room. Idempotent (re-muting is
/// a no-op). Always reports `muted: true`.
async fn thread_mute(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(root_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = MessageId::from_str(root_str.trim())
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    assert_thread_room_access(&s, auth.participant_id, root).await?;
    aero_storage::ThreadMuteRepo::new(s.pg.clone())
        .mute(auth.participant_id, root)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "root_message_id": root, "muted": true })))
}

/// `DELETE /api/threads/:root_message_id/mute` — unmute the thread for the caller.
/// Owner-scoped at the SQL layer (only ever removes the caller's own mute), but we
/// still assert room access first for a consistent 404/403 with the mute path.
/// Unmuting a thread that was never muted is a no-op. Always reports `muted: false`.
async fn thread_unmute(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(root_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = MessageId::from_str(root_str.trim())
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    assert_thread_room_access(&s, auth.participant_id, root).await?;
    aero_storage::ThreadMuteRepo::new(s.pg.clone())
        .unmute(auth.participant_id, root)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "root_message_id": root, "muted": false })))
}

/// `GET /api/threads/:root_message_id/mutes` — the participants who have MUTED this
/// thread (`ThreadMuteRepo::muted_by`). The caller must be able to access the root
/// message's room. NOTE: `ThreadMuteRepo` exposes no per-participant "threads I
/// muted" query, so there is no `/api/me/thread-mutes`; this lists the muters of a
/// single thread instead.
async fn thread_muters(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(root_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = MessageId::from_str(root_str.trim())
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    assert_thread_room_access(&s, auth.participant_id, root).await?;
    let muters = aero_storage::ThreadMuteRepo::new(s.pg.clone())
        .muted_by(root)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "root_message_id": root, "muters": muters })))
}

// ----- Search -----

#[derive(Deserialize)]
struct SearchReq {
    query: String,
    #[serde(default)]
    limit: Option<i64>,
    /// "fts" (default) | "vector" | "auto"
    #[serde(default)]
    mode: Option<String>,
}

async fn room_search(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<SearchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard (workspace + room membership) — supersedes the prior bare
    // room-membership check before searching the room's messages.
    s.im.assert_room_access(auth.participant_id, room).await?;
    // Tenant fairness (ROADMAP3 方向五): search is one of the most expensive
    // per-request PG paths, so it is a charged choke point.
    crate::ws_rate::check_ws_rate_room(&s, room).await?;
    if req.query.trim().is_empty() {
        return Err(AeroError::Invalid("empty query".into()).into());
    }
    let limit = req.limit.unwrap_or(20);
    let mode = req.mode.as_deref().unwrap_or("auto");

    let hits = match (mode, &s.ai) {
        ("vector", Some(ai)) => {
            let embedding = ai
                .embed_text(&req.query)
                .await
                .map_err(|e| AeroError::Upstream(format!("ai embed: {e}")))?;
            s.messages
                .search_vector(room, embedding, limit)
                .await
                .map_err(AeroError::from)?
        }
        ("hybrid", Some(ai)) => {
            // FTS + vector fused with simple max-score merge.
            let mut fts = s
                .messages
                .search_fts(room, &req.query, limit)
                .await
                .map_err(AeroError::from)?;
            if let Ok(embedding) = ai.embed_text(&req.query).await {
                let vec_hits = s
                    .messages
                    .search_vector(room, embedding, limit)
                    .await
                    .map_err(AeroError::from)?;
                fts = merge_hits(fts, vec_hits, limit);
            }
            fts
        }
        _ => s
            .messages
            .search_fts(room, &req.query, limit)
            .await
            .map_err(AeroError::from)?,
    };

    Ok(Json(serde_json::json!({
        "query": req.query,
        "mode": mode,
        "results": hits.into_iter().map(|h| {
            serde_json::json!({
                "score": h.score,
                "message": h.message,
            })
        }).collect::<Vec<_>>(),
    })))
}

// ----- Blobs (multipart upload, byte download) -----

const MAX_BLOB_BYTES: usize = 32 * 1024 * 1024; // 32 MiB

/// MIME-type prefix allowlist for uploads. Covers image, video, and audio.
const ALLOWED_MIME_PREFIXES: &[&str] = &["image/", "video/", "audio/"];
/// Exact MIME types allowed beyond the prefix allowlist above.
const ALLOWED_MIME_EXACT: &[&str] = &[
    "application/pdf",
    "text/plain",
    "text/csv",
    "application/zip",
    "application/x-zip-compressed",
    "application/octet-stream", // browser default for binary files without an extension
];

fn is_allowed_mime(mime: &str) -> bool {
    ALLOWED_MIME_PREFIXES.iter().any(|p| mime.starts_with(p))
        || ALLOWED_MIME_EXACT.contains(&mime)
}

async fn blob_upload(
    State(s): State<AppState>,
    auth: AuthUser,
    mut mp: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    // Tenant fairness (ROADMAP3 方向五): blobs are owner-scoped (no room in the
    // URL), so the charge resolves through the uploader's workspace membership.
    // Checked before the multipart body is read, shedding the bytes early.
    crate::ws_rate::check_ws_rate_participant(&s, auth.participant_id).await?;
    while let Some(field) = mp.next_field().await.map_err(|e| AeroError::Invalid(e.to_string()))? {
        if field.name() != Some("file") {
            continue;
        }
        let name = field.file_name().unwrap_or("untitled").to_owned();
        let mime = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_owned();
        let bytes = field.bytes().await.map_err(|e| AeroError::Invalid(e.to_string()))?;
        if bytes.len() > MAX_BLOB_BYTES {
            return Err(AeroError::Invalid(format!("blob too large: {} bytes", bytes.len())).into());
        }
        if !is_allowed_mime(&mime) {
            return Err(AeroError::Invalid(format!("unsupported file type: {mime}")).into());
        }
        // Defence-in-depth: the MIME above is client-claimed, so sniff the actual
        // bytes — reject executables / HTML / SVG payloads disguised as an allowed
        // type (stored malware / stored-XSS), and binary types whose content does
        // not match their declared family.
        if !crate::content_sniff::is_consistent(&mime, &bytes) {
            return Err(AeroError::Invalid(format!(
                "file content does not match its declared type ({mime}), or is a disallowed executable/markup payload"
            ))
            .into());
        }
        // Real anti-virus scan (defence-in-depth beyond the magic-byte sniffer):
        // a malicious binary carrying a benign header passes `is_consistent`, so —
        // when a `clamd` endpoint is configured (`AERO_CLAMAV_HOST`) — stream the
        // bytes to ClamAV via INSTREAM.
        //
        //   * `Infected` → reject the upload (400) with the signature name.
        //   * `Error` (clamd unreachable / timeout / daemon error) → policy:
        //     fail-OPEN by default (warn + metric, allow through, so a flaky
        //     daemon doesn't block every upload); `AERO_CLAMAV_FAIL_CLOSED` flips
        //     it to fail-CLOSED (reject with 502).
        //   * `Clean` → proceed.
        if let Some(scanner) = crate::av_scan::ClamdScanner::global() {
            match scanner.scan(&bytes).await {
                crate::av_scan::ScanVerdict::Clean => {
                    crate::av_scan::record_scan("clean");
                }
                crate::av_scan::ScanVerdict::Infected(sig) => {
                    crate::av_scan::record_scan("infected");
                    tracing::warn!(
                        participant = %auth.participant_id,
                        signature = %sig,
                        "blob upload rejected: malware detected by clamd",
                    );
                    return Err(AeroError::Invalid(format!(
                        "file rejected by virus scan ({sig})"
                    ))
                    .into());
                }
                crate::av_scan::ScanVerdict::Error(diag) => {
                    crate::av_scan::record_scan("error");
                    if crate::av_scan::fail_closed() {
                        tracing::warn!(error = %diag, "blob upload rejected: clamd unavailable (fail-closed)");
                        return Err(AeroError::Upstream(format!(
                            "virus scanner unavailable: {diag}"
                        ))
                        .into());
                    }
                    tracing::warn!(error = %diag, "clamd scan failed; allowing upload (fail-open)");
                }
            }
        }
        let kind = guess_file_kind(&mime);
        let size = bytes.len() as u64;
        let sha256_hex = hex::encode(sha2::Sha256::digest(&bytes));

        // Content dedup (owner-scoped): if this participant already uploaded
        // identical bytes, return the existing blob without re-writing storage.
        if let Some(existing) = s
            .blobs
            .find_by_owner_sha256(auth.participant_id, &sha256_hex)
            .await
            .map_err(AeroError::from)?
        {
            return Ok(Json(serde_json::json!({
                "id": existing.id,
                "name": existing.name,
                "mime": existing.mime,
                "size": existing.size,
                "kind": existing.kind,
            })));
        }

        let blob = s
            .blobs
            .create(NewBlob {
                owner_id: auth.participant_id,
                kind,
                name: name.clone(),
                mime: mime.clone(),
                size,
                sha256: Some(sha256_hex),
                storage_key: format!("pending:{}", uuid::Uuid::new_v4()),
            })
            .await
            .map_err(AeroError::from)?;
        let key = s
            .blob_store
            .put(blob.id, bytes)
            .await
            .map_err(|e| AeroError::Internal(anyhow::anyhow!("blob put: {e}")))?;
        let _ = key; // The store knows the key from the blob id.
        return Ok(Json(serde_json::json!({
            "id": blob.id,
            "name": blob.name,
            "mime": blob.mime,
            "size": blob.size,
            "kind": blob.kind,
        })));
    }
    Err(AeroError::Invalid("multipart missing 'file' field".into()).into())
}

async fn blob_download(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<axum::response::Response> {
    let id = BlobId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("blob id: {e}")))?;
    // Tenant fairness (ROADMAP3 方向五): charge the download (a PG meta read +
    // a full blob-store read) against the caller's workspace budget up front.
    crate::ws_rate::check_ws_rate_participant(&s, auth.participant_id).await?;
    let meta = s
        .blobs
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("blob".into()))?;
    // IDOR guard: a logged-in user may only download a blob they uploaded or one
    // referenced by a message in a room they belong to. Without this any holder
    // of a blob id could read any attachment.
    if !s
        .blobs
        .is_accessible_by(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?
    {
        return Err(AeroError::Forbidden("blob".into()).into());
    }
    let bytes: Bytes = s
        .blob_store
        .get(id)
        .await
        .map_err(|e| AeroError::Internal(anyhow::anyhow!("blob get: {e}")))?;
    let mut resp = bytes.into_response();
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_str(&meta.mime).unwrap_or_else(|_| {
            header::HeaderValue::from_static("application/octet-stream")
        }),
    );
    // Anti-XSS: force attachment disposition for non-visual content types.
    // Images, video, audio, and PDF may render inline (expected UX); everything
    // else (HTML, SVG, Office docs, executables) is forced to download so a
    // malicious blob cannot execute scripts in the browser (ROADMAP 方向三).
    // The nosniff header prevents MIME-type confusion attacks regardless.
    let inline_safe = meta.mime.starts_with("image/")
        || meta.mime.starts_with("video/")
        || meta.mime.starts_with("audio/")
        || meta.mime == "application/pdf";
    let disposition = if inline_safe {
        format!("inline; filename=\"{}\"", meta.name)
    } else {
        format!("attachment; filename=\"{}\"", meta.name)
    };
    headers.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&disposition)
            .unwrap_or_else(|_| {
                if inline_safe {
                    header::HeaderValue::from_static("inline")
                } else {
                    header::HeaderValue::from_static("attachment")
                }
            }),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    Ok(resp)
}

fn guess_file_kind(mime: &str) -> FileKind {
    if mime.starts_with("image/") {
        FileKind::Image
    } else if mime.starts_with("video/") {
        FileKind::Video
    } else if mime.starts_with("audio/") {
        FileKind::Audio
    } else if mime == "application/pdf" || mime.starts_with("text/") || mime.contains("document") {
        FileKind::Document
    } else {
        FileKind::Other
    }
}

// ---------- Bot SDK (方向三) ----------

#[derive(serde::Deserialize)]
struct CreateBotReq {
    name: String,
    #[serde(default)]
    icon_url: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
}

/// Register a new bot (participant + bot row + initial token).
async fn bot_create(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateBotReq>,
) -> ApiResult<Response> {
    let workspace = req.workspace_id.as_ref().and_then(|w| aero_common::WorkspaceId::from_str(w).ok());
    let name = validate_bot_name(&req.name)?;
    let repo = aero_storage::BotRepo::new(s.pg.clone());
    let bot_id = repo
        .create(auth.participant_id, &name, req.icon_url.as_deref(), workspace)
        .await
        .map_err(AeroError::from)?;
    let token = repo
        .rotate_token(bot_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Internal(anyhow::anyhow!("bot created but token failed")))?;
    let body = Json(serde_json::json!({
        "bot_id": bot_id,
        "token": token,
        "name": name,
    }))
    .into_response();
    // Per-tenant HTTP metrics: stamp the resolved tenant when the request named
    // one (response-extension pass-through; bounded + flag-gated in the layer).
    Ok(match workspace {
        Some(ws) => metrics::attach_workspace_label(body, ws),
        None => body,
    })
}

/// Look up a bot's `owner_id` by its participant id, scoped to the `bots`
/// table only.  Returns `None` when no such bot exists.
///
/// Authorization helper kept in the routes layer: `BotRepo` exposes no
/// fetch-by-id, so we read just the `owner_id` column here to assert ownership
/// before any mutating bot operation. We never expose this row directly.
async fn bot_owner(
    pg: &sqlx::PgPool,
    bot_id: ParticipantId,
) -> Result<Option<ParticipantId>, AeroError> {
    let row = sqlx::query_as::<_, (Uuid,)>("SELECT owner_id FROM bots WHERE id = $1")
        .bind(bot_id.to_uuid())
        .fetch_optional(pg)
        .await
        .map_err(AeroError::from)?;
    Ok(row.map(|(o,)| ParticipantId::from_uuid(o)))
}

/// Assert that `caller` owns the bot `bot_id`.
///
/// Returns [`AeroError::NotFound`] when the bot does not exist and
/// [`AeroError::Forbidden`] when the caller is not its owner. On success the
/// caller is cleared to perform a mutating operation on the bot.
async fn ensure_bot_owner(
    pg: &sqlx::PgPool,
    bot_id: ParticipantId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    let owner = bot_owner(pg, bot_id)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("bot {bot_id}")))?;
    if owner != caller {
        return Err(AeroError::Forbidden("not the bot owner".into()));
    }
    Ok(())
}

/// List the calling participant's own bots (across all workspaces).
///
/// Scoped strictly to `owner_id = auth.participant_id` — callers never see
/// bots owned by other participants.
async fn bot_list(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = sqlx::query_as::<_, BotListRow>(
        r"SELECT id, owner_id, name, icon_url, workspace_id, token_hash IS NOT NULL AS has_token, created_at
           FROM bots
          WHERE owner_id = $1
          ORDER BY created_at DESC",
    )
    .bind(auth.participant_id.to_uuid())
    .fetch_all(&s.pg)
    .await
    .map_err(AeroError::from)?;
    let bots: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": ParticipantId::from_uuid(r.id),
                "owner_id": ParticipantId::from_uuid(r.owner_id),
                "name": r.name,
                "icon_url": r.icon_url,
                "workspace_id": r.workspace_id.map(WorkspaceId::from_uuid),
                "has_token": r.has_token,
                "created_at": r.created_at,
            })
        })
        .collect();
    Ok(Json(serde_json::Value::Array(bots)))
}

/// Row shape for [`bot_list`] (mirrors `storage::bot::BotRow`, kept local so
/// this stays a routes-only change).
#[derive(sqlx::FromRow)]
struct BotListRow {
    id: Uuid,
    owner_id: Uuid,
    name: String,
    icon_url: Option<String>,
    workspace_id: Option<Uuid>,
    has_token: bool,
    created_at: time::OffsetDateTime,
}

/// Rotate a bot's token (revokes the old one, returns the new plaintext).
///
/// Only the bot's owner may rotate its token.
async fn bot_rotate_token(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot_id = ParticipantId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    ensure_bot_owner(&s.pg, bot_id, auth.participant_id).await?;
    let repo = aero_storage::BotRepo::new(s.pg.clone());
    let token = repo
        .rotate_token(bot_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("bot {bot_id}")))?;
    Ok(Json(serde_json::json!({ "token": token })))
}

// ---------- Bot event subscriptions ----------

#[derive(serde::Deserialize)]
struct CreateSubReq {
    event_type: String,
    #[serde(default)]
    filters: Option<serde_json::Value>,
    #[serde(default)]
    webhook_url: Option<String>,
}

/// Subscribe a bot to an event type.
///
/// Only the bot's owner may add subscriptions.
async fn bot_create_subscription(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<CreateSubReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot_id = ParticipantId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    ensure_bot_owner(&s.pg, bot_id, auth.participant_id).await?;
    // SSRF guard: `webhook_url` is fetched by the SERVER (bot_dispatch.rs) on every
    // matching event, so — exactly like the room-level outgoing webhook gate — a
    // destination resolving to loopback/private/link-local (incl. the cloud
    // metadata IP) must be rejected at creation. `None` (WS-delivered bot) skips
    // the check entirely; there's no URL to validate.
    if let Some(url) = req.webhook_url.as_deref() {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(AeroError::Invalid("webhook_url must be http(s)".into()).into());
        }
        crate::webhooks::assert_webhook_url_safe(url).await?;
    }
    let repo = aero_storage::BotRepo::new(s.pg.clone());
    let sub_id = repo
        .subscribe(bot_id, &req.event_type, req.filters.as_ref(), req.webhook_url.as_deref())
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "id": sub_id, "event_type": req.event_type })))
}

/// List a bot's event subscriptions.
///
/// Only the bot's owner may view its subscriptions (they can embed
/// `webhook_url` secrets).
async fn bot_list_subscriptions(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot_id = ParticipantId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    ensure_bot_owner(&s.pg, bot_id, auth.participant_id).await?;
    let repo = aero_storage::BotRepo::new(s.pg.clone());
    let subs = repo.list_subscriptions(bot_id).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(subs).map_err(AeroError::from)?))
}

/// Delete a bot's event subscription.
///
/// Only the bot's owner may delete its subscriptions.
async fn bot_delete_subscription(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((bot_str, sub_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot_id = ParticipantId::from_str(&bot_str)
        .map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    let sub_id = Uuid::from_str(&sub_str)
        .map_err(|e| AeroError::Invalid(format!("subscription id: {e}")))?;
    ensure_bot_owner(&s.pg, bot_id, auth.participant_id).await?;
    let repo = aero_storage::BotRepo::new(s.pg.clone());
    let deleted = repo.delete_subscription(sub_id, bot_id).await.map_err(AeroError::from)?;
    if !deleted {
        return Err(AeroError::NotFound("subscription".into()).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// Query for [`bot_list_deliveries`]: an optional `limit` (clamped in storage).
#[derive(serde::Deserialize)]
struct DeliveriesQuery {
    #[serde(default)]
    limit: Option<i64>,
}

/// List a bot's recent webhook delivery records (migration 0147), newest first.
///
/// Surfaces the previously-invisible bot-subscription delivery outcomes the
/// dispatcher now records (per-attempt `delivered`/`failed` + HTTP status +
/// error), so a bot owner can see whether their subscriptions are actually
/// reaching their `webhook_url`.
///
/// Only the bot's owner may view its deliveries (records correlate to the bot's
/// subscriptions, which can embed `webhook_url` secrets).
async fn bot_list_deliveries(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<DeliveriesQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot_id = ParticipantId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    ensure_bot_owner(&s.pg, bot_id, auth.participant_id).await?;
    let repo = aero_storage::BotRepo::new(s.pg.clone());
    let deliveries = repo
        .list_deliveries_for_bot(bot_id, q.limit.unwrap_or(100))
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(deliveries).map_err(AeroError::from)?))
}

async fn get_participant(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = ParticipantId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))?;
    let p = s
        .participants
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;
    Ok(Json(serde_json::to_value(p).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct ParticipantSearchQuery {
    q: String,
    #[serde(default)]
    limit: Option<i64>,
}

async fn search_participants(
    State(s): State<AppState>,
    _auth: AuthUser,
    Query(p): Query<ParticipantSearchQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let limit = p.limit.unwrap_or(20);
    let list = s.participants.search(&p.q, limit).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(list).map_err(AeroError::from)?))
}

async fn list_room_members(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard (workspace + room membership) — supersedes the prior bare
    // room-membership check before listing the room's members.
    s.im.assert_room_access(auth.participant_id, room).await?;
    let ids = s.rooms.members(room).await.map_err(AeroError::from)?;
    // Resolve to full Participant objects.
    let mut out = Vec::with_capacity(ids.len());
    for pid in ids {
        if let Ok(Some(p)) = s.participants.get(pid).await {
            out.push(p);
        }
    }
    Ok(Json(serde_json::to_value(out).map_err(AeroError::from)?))
}

// ----- WHIP / WHEP -----

async fn whip_post(
    State(s): State<AppState>,
    Path(stream_key): Path<String>,
    sdp_offer: String,
) -> ApiResult<axum::response::Response> {
    use axum::http::{header, HeaderValue, StatusCode};
    let stream = s
        .streams
        .get_by_key(&stream_key)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("stream".into()))?;
    let resource = accept_whip_offer(&stream, &sdp_offer, &s.ingest_host, s.ingest_port)
        .map_err(|e| match e {
            WhipError::InvalidSdp(m) => AeroError::Invalid(m),
            WhipError::Conflict => AeroError::Conflict("publisher present".into()),
            WhipError::NotFound => AeroError::NotFound("stream".into()),
            WhipError::Internal(m) => AeroError::Internal(anyhow::anyhow!(m)),
        })?;
    s.whip
        .insert(resource.clone())
        .map_err(|_| AeroError::Conflict("publisher present".into()))?;
    // Sticky routing (ROADMAP 方向二): advertise that THIS node now ingests the
    // stream, so WHEP pulls landing on other nodes can be redirected here.
    if let Err(e) = s.stream_routes.publish(stream.id, &s.public_base_url).await {
        tracing::warn!(error=?e, stream=%stream.id, "stream route publish failed");
    }
    let hls_path = format!("/hls/{}/index.m3u8", stream.id);
    let was_live = matches!(stream.status, StreamStatus::Live);
    if let Err(e) = s.streams.mark_live(stream.id, &hls_path).await {
        tracing::warn!(error=?e, "mark live failed");
    } else if !was_live {
        // Announce the go-live on the live bus so the out-of-band golive_bot can fan
        // out "went live" notices to the creator's followers (durable activity feed),
        // without touching this ingest hot path. Only on the idle/ended->live edge,
        // so a republish of an already-live stream does not re-notify. Best-effort.
        // Funnels through LiveService so the event carries the publish-time `"seq"`
        // stamp like every other StreamEvent (ROADMAP 第三版 方向一).
        s.live.publish_go_live(stream.id).await;
        // Fire outgoing webhooks for the stream.live event on the stream's room
        // (if the stream is room-bound). Best-effort: any error is logged and
        // swallowed so the WHIP ingest path is never delayed.
        if let Some(room_id) = stream.room_id {
            let webhook_repo = aero_storage::WebhookRepo::new(s.pg.clone());
            let delivery_repo = aero_storage::WebhookDeliveryRepo::new(s.pg.clone());
            match webhook_repo.list_outgoing_for_room_event(room_id, "stream.live").await {
                Ok(targets) => {
                    let payload = serde_json::json!({
                        "kind": "stream.live",
                        "stream_id": stream.id.to_string(),
                        "title": stream.title,
                        "owner_id": stream.owner_id,
                    });
                    let now = time::OffsetDateTime::now_utc().unix_timestamp();
                    let sender = aero_storage::ReqwestSender::new();
                    for target in targets {
                        let delivery = aero_storage::build_delivery(
                            &target.url,
                            &target.secret,
                            &payload,
                            now,
                        );
                        let event_id = Some(stream.id.to_string());
                        match delivery_repo.record_attempt(target.id, event_id.as_deref()).await {
                            // `None` = this stream.live event was already delivered to
                            // this endpoint (idempotent claim); skip the duplicate POST.
                            Ok(None) => {}
                            Ok(Some(delivery_id)) => {
                                use aero_storage::WebhookSender;
                                match sender.deliver(&delivery).await {
                                    // Delivery-log bookkeeping keys on the numeric
                                    // status (this one-off fire has no breaker).
                                    Ok(resp) if (200..300).contains(&resp.status) => {
                                        let _ = delivery_repo
                                            .mark_delivered(delivery_id, i32::from(resp.status))
                                            .await;
                                    }
                                    Ok(resp) => {
                                        let _ = delivery_repo
                                            .mark_failed_with_backoff(
                                                delivery_id,
                                                1,
                                                Some(i32::from(resp.status)),
                                                "non-2xx response",
                                            )
                                            .await;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = ?e,
                                            hook = %target.id,
                                            "stream.live webhook delivery failed"
                                        );
                                        let _ = delivery_repo
                                            .mark_failed_with_backoff(
                                                delivery_id,
                                                1,
                                                None,
                                                "transport error",
                                            )
                                            .await;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = ?e,
                                    hook = %target.id,
                                    "stream.live webhook record_attempt failed"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = ?e, stream = %stream.id, "stream.live webhook lookup failed");
                }
            }
        }
    }
    let mut resp = (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/sdp")],
        resource.answer_sdp.clone(),
    )
        .into_response();
    resp.headers_mut().insert(
        header::LOCATION,
        // The WHIP resource is addressed by the publisher's stream KEY (the secret
        // ingest credential), NOT the public stream id — so only the publisher can
        // tear the session down (see whip_delete).
        HeaderValue::from_str(&format!("/whip/resource/{}", stream.stream_key))
            .unwrap_or_else(|_| HeaderValue::from_static("/whip/resource")),
    );
    Ok(resp)
}

async fn whip_delete(
    State(s): State<AppState>,
    Path(stream_key): Path<String>,
) -> ApiResult<StatusCode> {
    // Identify the resource by the publisher's stream KEY, not the public stream
    // id: the id appears in HLS/WHEP playback URLs, so keying the teardown on it
    // let ANYONE end any live stream (DoS). Possession of the key — the same secret
    // that authorized publishing — authorizes ending it.
    let stream = s
        .streams
        .get_by_key(&stream_key)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("stream".into()))?;
    s.whip.remove(stream.id);
    if let Err(e) = s.stream_routes.unpublish(stream.id).await {
        tracing::warn!(error=?e, stream=%stream.id, "stream route unpublish failed");
    }
    if let Err(e) = s.streams.mark_ended(stream.id).await {
        tracing::warn!(error=?e, "mark ended failed");
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn whep_post(
    State(s): State<AppState>,
    Path(stream_id_str): Path<String>,
    sdp_offer: String,
) -> ApiResult<axum::response::Response> {
    use axum::http::{header, StatusCode};
    let stream_id = ulid::Ulid::from_str(&stream_id_str)
        .map_err(|e| AeroError::Invalid(format!("stream id: {e}")))?;
    // A live publisher must exist for there to be anything to play back. If it
    // isn't on THIS node, the stream may be ingested elsewhere: sticky routing
    // (ROADMAP 方向二) redirects the viewer to the owning node, so a stream
    // ingested on node A is playable from node B without inter-node media relay.
    if s.whip.get(stream_id).is_none() {
        let located = s
            .stream_routes
            .locate(stream_id)
            .await
            .map_err(|e| AeroError::Internal(anyhow::anyhow!("stream route lookup: {e}")))?;
        if let Some(home) = aero_storage::redirect_base(&s.public_base_url, located.as_deref()) {
            use axum::http::{header, HeaderValue};
            let target = format!("{}/whep/{}", home.trim_end_matches('/'), stream_id);
            let mut resp = StatusCode::TEMPORARY_REDIRECT.into_response();
            resp.headers_mut().insert(
                header::LOCATION,
                HeaderValue::from_str(&target)
                    .map_err(|e| AeroError::Internal(anyhow::anyhow!("redirect target: {e}")))?,
            );
            return Ok(resp);
        }
        return Err(AeroError::NotFound("no live publisher".into()).into());
    }
    // Negotiate a real WHEP *sendonly* SDP answer for the viewer's recvonly offer
    // (str0m via `WhepSession`). NOTE: the WHIP->WHEP media relay — forwarding the
    // publisher's RTP into this egress session — and browser playback are not yet
    // wired; that path requires a live publisher + browser (absent from CI).
    let answer = accept_whep_offer(&sdp_offer, &s.ingest_host, s.ingest_port).map_err(|e| match e {
        SessionError::Offer(m) => AeroError::Invalid(format!("whep offer: {m}")),
        SessionError::Addr(h, p) => {
            AeroError::Internal(anyhow::anyhow!("whep egress addr {h}:{p}"))
        }
        other => AeroError::Internal(anyhow::anyhow!(other.to_string())),
    })?;
    let resp = (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/sdp")],
        answer,
    )
        .into_response();
    Ok(resp)
}

// ---------- Tests ----------


// Module name matches the file (`routes_tests`) so the orphan-check sees the
// `mod` declaration; `#[path]` keeps the file a sibling rather than nesting a
// `routes/routes/` directory.
#[cfg(test)]
#[path = "routes_tests.rs"]
mod routes_tests;
