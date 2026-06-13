//! HTTP route definitions. WS upgrade lives in `ws::handler`.

use std::str::FromStr;

use futures::StreamExt as _;
use sha2::Digest as _;

use aero_auth::{AuthUser, LoginRequest, RegisterRequest};
use aero_common::{
    BlobId, Error as AeroError, FileKind, MessageId, ParticipantId, Result as AeroResult, RoomId,
    RoomKind, StreamProtocol, StreamStatus, WorkspaceId, WorkspaceRole,
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
        .route("/health", get(health))
        // k8s-style probes: liveness is process-up only; readiness gates on deps.
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        // Auth
        .route("/api/auth/register", post(auth_register))
        .route("/api/auth/login", post(auth_login))
        .route("/api/me", get(me).patch(update_me))
        // Rooms
        .route("/api/rooms", post(create_room).get(list_rooms))
        .route("/api/rooms/:id/members", post(add_member))
        .route("/api/rooms/:id/messages", get(room_history))
        .route("/api/rooms/:id/changes", get(room_changes))
        .route("/api/rooms/:id/read", post(mark_read))
        .route("/api/rooms/:id/receipts", get(list_receipts))
        .route("/api/rooms/:id/search", post(room_search))
        // Messages
        .route("/api/messages/:id", axum::routing::patch(edit_message).delete(delete_message))
        .route("/api/messages/:id/reactions", post(toggle_reaction))
        .route("/api/messages/reactions", post(reactions_batch))
        // Blobs
        .route("/api/blobs", post(blob_upload))
        .route("/api/blobs/:id", get(blob_download))
        // AI
        .route("/api/ai/summarize", post(ai_summarize))
        .route("/api/ai/ask", post(ai_ask))
        .route("/api/ai/ask/stream", post(ai_ask_stream))
        .route("/api/ai/ask/context", post(ai_ask_context))
        // Live streams
        .route("/api/streams", post(stream_create).get(stream_list))
        .route("/api/streams/:id", get(stream_get))
        .route("/api/streams/:id/end", post(stream_end))
        // Live interactivity (P4 弹幕 + 礼物)
        .route("/api/live/gifts", get(live_gift_catalog))
        .route("/api/streams/:id/chat", get(stream_chat_list).post(stream_chat_post))
        .route("/api/streams/:id/gifts", get(stream_gift_list).post(stream_gift_send))
        .route("/api/streams/:id/leaderboard", get(stream_leaderboard))
        // WHIP / WHEP — body is SDP text, response is SDP text
        .route("/whip/:stream_key", post(whip_post))
        .route("/whip/resource/:stream_id", axum::routing::delete(whip_delete))
        .route("/whep/:stream_id", post(whep_post))
        // Agents (Bot/Agent participants)
        .route("/api/agents", post(create_agent))
        .route("/api/participants", get(search_participants))
        .route("/api/participants/:id", get(get_participant))
        .route("/api/rooms/:id/members/list", get(list_room_members))
        // MLS E2E (server is opaque relay; clients run openmls)
        .route("/api/mls/key-packages", post(mls_publish_kp))
        .route("/api/mls/key-packages/:participant", get(mls_consume_kp))
        .route("/api/mls/groups", post(mls_upsert_group))
        .route("/api/mls/groups/:gid", get(mls_get_group))
        // RTC config
        .route("/api/rtc/config", get(rtc_config))
        // WebSocket
        .route("/ws", get(ws::handler))
        // Workspace / Org management (ROADMAP 方向一 — multi-tenant foundation).
        // Defined alongside their RBAC guards in `crate::workspaces`.
        .merge(crate::workspaces::routes())
        .merge(crate::collab::routes())
        // Channel management (public/private, join/leave, archive, topic/desc).
        .merge(crate::channels::routes())
        // Webhooks (incoming inbound-message hooks + outgoing event delivery).
        .merge(crate::webhooks::routes())
        // SSO via OIDC: ID-token login + JIT provisioning (POST /api/auth/oidc).
        .merge(crate::sso::routes())
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

/// Probe each backing dependency (PG / Redis / NATS) with a short timeout.
/// Each result is `"ok"` / `"fail"` / `"timeout"`. Shared by `/health` and
/// `/health/ready` so the two never drift.
async fn probe_deps(s: &AppState) -> (&'static str, &'static str, &'static str) {
    use std::time::Duration;
    let pg_ok = tokio::time::timeout(Duration::from_secs(2), async {
        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(s.participants.pool())
            .await
    })
    .await;
    let pg = match pg_ok {
        Ok(Ok(_)) => "ok",
        Ok(Err(_)) => "fail",
        Err(_) => "timeout",
    };

    let redis_ok = tokio::time::timeout(Duration::from_secs(2), async {
        // PresenceStore holds a RedisClient; we re-resolve via the participant
        // pool's cousin — easier: just construct a tiny ad-hoc client using a
        // sentinel through the existing presence handle.
        s.presence.ping().await
    })
    .await;
    let redis = match redis_ok {
        Ok(Ok(_)) => "ok",
        Ok(Err(_)) => "fail",
        Err(_) => "timeout",
    };

    // NATS: bus reference is required at boot; if the connection has dropped,
    // downstream publish/subscribe will start logging warnings. Surface "ok"
    // here unless we can cheaply probe. We do a fire-and-forget publish on a
    // subject the IM_EVENTS stream covers — sub-millisecond when up, errors
    // out almost immediately when down.
    let nats_ok = tokio::time::timeout(Duration::from_secs(2), async {
        s.bus
            .publish("im.events.health", bytes::Bytes::from_static(b"ping"))
            .await
    })
    .await;
    let nats = match nats_ok {
        Ok(Ok(_)) => "ok",
        Ok(Err(_)) => "fail",
        Err(_) => "timeout",
    };

    (pg, redis, nats)
}

/// Probe the blob backend's reachability for readiness gating (ROADMAP 方向三).
/// The local FS store is always present, so only S3 can be remotely unreachable;
/// a short timeout means a hung endpoint reads as `"timeout"` (not ready) rather
/// than stalling the probe.
async fn probe_blob(s: &AppState) -> &'static str {
    if s.blob_backend != "s3" {
        return "ok";
    }
    match tokio::time::timeout(std::time::Duration::from_secs(2), s.blob_store.health_check()).await
    {
        Ok(Ok(())) => "ok",
        Ok(Err(_)) => "fail",
        Err(_) => "timeout",
    }
}

/// Legacy combined health endpoint (kept for backward-compat). Always 200; the
/// body's `status` is `"ok"` only when every dependency probes healthy.
async fn health(State(s): State<AppState>) -> Json<serde_json::Value> {
    let (pg, redis, nats) = probe_deps(&s).await;
    let overall = if pg == "ok" && redis == "ok" && nats == "ok" {
        "ok"
    } else {
        "degraded"
    };

    Json(serde_json::json!({
        "status": overall,
        "deps": {
            "postgres": pg,
            "redis": redis,
            "nats": nats,
        },
        // Surface the active blob backend (s3/local) so operators can confirm
        // storage is wired as intended — fail-loud's companion (方向五).
        "blob_backend": s.blob_backend,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Liveness probe (k8s `livenessProbe`): the process is up and serving. Always
/// 200 — it must *not* depend on PG/Redis/NATS, or a transient backend blip
/// would get the pod killed and restarted (making the outage worse).
async fn health_live() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}

/// Pure readiness decision, split out so the policy is unit-testable without a
/// live PG/Redis/NATS. Draining (graceful shutdown in progress) takes
/// precedence: the pod must leave the LB rotation immediately, regardless of
/// dependency health. Otherwise ready only when every dependency probed healthy.
fn readiness_decision(shutting_down: bool, deps_ok: bool) -> (StatusCode, &'static str) {
    if shutting_down {
        (StatusCode::SERVICE_UNAVAILABLE, "draining")
    } else if deps_ok {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not_ready")
    }
}

/// Readiness probe (k8s `readinessProbe`): 200 only when every dependency is
/// reachable, else 503 so the pod is pulled from the load-balancer rotation
/// until it recovers (without being restarted). During graceful shutdown it
/// returns 503 `"draining"` immediately so the LB stops routing new traffic
/// before the pod stops accepting (ROADMAP 方向三).
async fn health_ready(State(s): State<AppState>) -> impl IntoResponse {
    // Draining short-circuits the dependency probe — once shutdown has begun the
    // answer is 503 regardless, and skipping the probe avoids needless backend
    // calls during teardown.
    if s.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
        let (status, state) = readiness_decision(true, false);
        return (
            status,
            Json(serde_json::json!({
                "status": state,
                "version": env!("CARGO_PKG_VERSION"),
            })),
        );
    }
    let (pg, redis, nats) = probe_deps(&s).await;
    let blob = probe_blob(&s).await;
    let deps_ok = pg == "ok" && redis == "ok" && nats == "ok" && blob == "ok";
    let (status, state) = readiness_decision(false, deps_ok);
    (
        status,
        Json(serde_json::json!({
            "status": state,
            "deps": {
                "postgres": pg,
                "redis": redis,
                "nats": nats,
                "blob": blob,
            },
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
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
    let out = s
        .auth
        .login(LoginRequest { email: req.email, password: req.password })
        .await?;
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
            return Err(AeroError::Unauthorized("2fa_required".into()).into());
        }
    }
    // Wave 21: record the active session (best-effort; never fails login). Done
    // only after 2FA passes, so a half-completed login leaves no session row.
    record_session(&s, out.participant.id, &out.refresh_token, &headers).await;
    Ok(Json(serde_json::json!({
        "access_token": out.access_token,
        "refresh_token": out.refresh_token,
        "participant": out.participant,
    })))
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
) -> ApiResult<Json<serde_json::Value>> {
    let kind = parse_room_kind(&req.kind)?;
    let workspace = resolve_workspace_id(req.workspace_id.as_deref())?;
    // Tenant choke point: verifies workspace membership + channel-create privilege
    // and persists `rooms.workspace_id` (fixes the NOT-NULL room-create regression
    // the old `create_room` hit after migration 0006).
    let room = s
        .im
        .create_room_in_workspace(auth.participant_id, workspace, kind, req.name)
        .await?;
    Ok(Json(serde_json::to_value(room).map_err(AeroError::from)?))
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
) -> ApiResult<Json<serde_json::Value>> {
    // Scoped to a workspace when `?workspace_id=` is given; otherwise unchanged
    // (every room the caller belongs to, across tenants).
    let rooms = match q.workspace_id.as_deref() {
        Some(raw) => {
            let ws = WorkspaceId::from_str(raw.trim())
                .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?;
            s.rooms
                .rooms_for_in_workspace(auth.participant_id, ws)
                .await
                .map_err(AeroError::from)?
        }
        None => s.im.list_my_rooms(auth.participant_id).await?,
    };
    Ok(Json(serde_json::to_value(rooms).map_err(AeroError::from)?))
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

// ----- Messages -----

#[derive(Deserialize)]
struct EditMessageReq {
    blocks: Vec<aero_common::Block>,
}

async fn edit_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<EditMessageReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = MessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let m = s.im.edit_message(auth.participant_id, id, req.blocks).await?;
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

async fn reactions_batch(
    State(s): State<AppState>,
    _auth: AuthUser,
    Json(req): Json<ReactionsBatchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ids: Vec<MessageId> = req
        .message_ids
        .iter()
        .map(|s| MessageId::from_str(s))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let summaries = s.im.reactions_for(&ids).await?;
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
    headers.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&format!("inline; filename=\"{}\"", meta.name))
            .unwrap_or_else(|_| header::HeaderValue::from_static("inline")),
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

// ----- AI -----

#[derive(Deserialize)]
struct AiSummarizeReq {
    room_id: String,
    #[serde(default)]
    last_n: Option<usize>,
}

async fn ai_summarize(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AiSummarizeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&req.room_id)?;
    // Tenant guard (workspace + room membership) — defense-in-depth on AI output
    // derived from a room's messages; supersedes the prior bare room-membership check.
    s.im.assert_room_access(auth.participant_id, room).await?;
    let last_n = req.last_n.unwrap_or(50);
    let ai = s.ai.as_ref().ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let summary = ai
        .summarize_room(room, last_n)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;
    Ok(Json(serde_json::json!({"summary": summary})))
}

#[derive(Deserialize)]
struct AiAskReq {
    room_id: String,
    question: String,
    #[serde(default)]
    k: Option<usize>,
}

async fn ai_ask(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AiAskReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&req.room_id)?;
    // Tenant guard (workspace + room membership) — defense-in-depth on RAG output.
    s.im.assert_room_access(auth.participant_id, room).await?;
    let k = req.k.unwrap_or(8);
    let ai = s.ai.as_ref().ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let answer = ai
        .answer_question(room, &req.question, k)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;
    Ok(Json(serde_json::json!({
        "answer": answer.answer,
        "citations": answer.citations,
    })))
}

/// Streaming variant of `ai_ask`.
///
/// Returns a Server-Sent Events stream with three event types:
/// - `event: citations` — JSON array of message-ID strings; emitted first,
///   before the first token, so the UI can render source chips immediately.
/// - `event: delta` — one UTF-8 text fragment per Anthropic SSE chunk.
/// - `event: done` — empty data; signals end of generation.
/// - `event: error` — non-fatal; stream continues but a warning is logged.
async fn ai_ask_stream(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AiAskReq>,
) -> impl IntoResponse {
    let room = match parse_room_id(&req.room_id) {
        Ok(r) => r,
        Err(e) => return crate::error::ApiError::from(e).into_response(),
    };
    if let Err(e) = s.im.assert_room_access(auth.participant_id, room).await {
        return crate::error::ApiError::from(e).into_response();
    }
    let k = req.k.unwrap_or(8);

    let Some(ai) = s.ai.as_ref() else {
        let event = Event::default().event("done").data("AI not configured");
        let stream = futures::stream::once(async move {
            Ok::<_, std::convert::Infallible>(event)
        });
        return Sse::new(stream).into_response();
    };
    let ai = ai.clone();

    let (citations, text_stream) =
        match ai.answer_question_stream(room, &req.question, k).await {
            Ok(v) => v,
            Err(e) => {
                return crate::error::ApiError::from(AeroError::Upstream(format!("ai: {e}")))
                    .into_response();
            }
        };

    let citations_json =
        serde_json::to_string(&citations.iter().map(ToString::to_string).collect::<Vec<_>>())
            .unwrap_or_else(|_| "[]".to_string());

    let event_stream = futures::stream::once(async move {
        Ok::<_, std::convert::Infallible>(
            Event::default().event("citations").data(citations_json),
        )
    })
    .chain(text_stream.map(|r| match r {
        Ok(text) => Ok(Event::default().event("delta").data(text)),
        Err(e) => {
            tracing::warn!(error = %e, "ai stream chunk error");
            Ok(Event::default().event("error").data(e))
        }
    }))
    .chain(futures::stream::once(async {
        Ok::<_, std::convert::Infallible>(Event::default().event("done").data(""))
    }));

    Sse::new(event_stream).keep_alive(KeepAlive::default()).into_response()
}

async fn ai_ask_context(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AiAskReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&req.room_id)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let k = req.k.unwrap_or(8);
    let ai = s.ai.as_ref().ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let answer = ai
        .ask_with_context(auth.participant_id, room, &req.question, k)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;
    Ok(Json(serde_json::json!({
        "answer": answer.answer,
        "citations": answer.citations,
    })))
}

// ----- Streams (P4) -----

#[derive(Deserialize)]
struct CreateStreamReq {
    title: String,
    #[serde(default)]
    room_id: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
}

async fn stream_create(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateStreamReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let proto = match req.protocol.as_deref().unwrap_or("rtmp") {
        "whip" => StreamProtocol::Whip,
        "srt" => StreamProtocol::Srt,
        _ => StreamProtocol::Rtmp,
    };
    let room_id = req
        .room_id
        .as_deref()
        .map(parse_room_id)
        .transpose()?;
    let stream = s
        .streams
        .create(NewStream {
            owner_id: auth.participant_id,
            room_id,
            title: req.title.clone(),
            protocol: proto,
            stream_key: None,
        })
        .await
        .map_err(AeroError::from)?;

    let ingest_url = match stream.protocol {
        StreamProtocol::Rtmp => format!("rtmp://{}/live/{}", strip_scheme(&s.public_base_url), stream.stream_key),
        StreamProtocol::Whip => format!("{}/whip/{}", s.public_base_url, stream.stream_key),
        StreamProtocol::Srt => format!("srt://{}?streamid={}", strip_scheme(&s.public_base_url), stream.stream_key),
    };
    let hls_url = format!("/hls/{}/index.m3u8", stream.id);

    // Best-effort: drop a stream card into the linked room so members can watch
    // inline. Failures don't block stream creation.
    if let Some(room) = room_id {
        let card = aero_common::Block::Card {
            schema: "stream".into(),
            payload: serde_json::json!({
                "stream_id": stream.id.to_string(),
                "owner_id": stream.owner_id.to_string(),
                "title": stream.title,
                "protocol": format!("{:?}", stream.protocol).to_lowercase(),
                "hls_url": hls_url,
                "ingest_url": ingest_url,
            }),
        };
        if let Err(e) = s
            .im
            .send_message(auth.participant_id, room, vec![card], None, None)
            .await
        {
            tracing::warn!(error = ?e, %room, "stream announce message failed");
        }
    }

    Ok(Json(serde_json::json!({
        "stream": stream,
        "ingest_url": ingest_url,
        "hls_url": hls_url,
    })))
}

async fn stream_list(
    State(s): State<AppState>,
    _auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let live = s.streams.list_live().await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(live).map_err(AeroError::from)?))
}

async fn stream_get(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = ulid::Ulid::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("stream id: {e}")))?;
    let stream = s
        .streams
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("stream".into()))?;
    Ok(Json(serde_json::to_value(stream).map_err(AeroError::from)?))
}

// ----- Live interactivity (P4 弹幕 + 礼物) -----

fn parse_stream_id(s: &str) -> AeroResult<ulid::Ulid> {
    ulid::Ulid::from_str(s).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

#[derive(Deserialize)]
struct LimitQuery {
    #[serde(default)]
    limit: Option<i64>,
}

/// Static gift catalog — the client renders the gift bar from this.
async fn live_gift_catalog(_auth: AuthUser) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "gifts": aero_common::gift_catalog() }))
}

async fn stream_chat_list(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<CursorLimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    // `since` is a best-effort forward catch-up cursor (the last chat-line id the
    // client rendered): a malformed value degrades to the recent tail rather than
    // erroring, matching the reconnect-cursor convention used for room messages.
    let since = q.since.as_deref().and_then(|c| ulid::Ulid::from_string(c.trim()).ok());
    let chat = s.live.recent_chat_since(id, since, q.limit.unwrap_or(50)).await?;
    Ok(Json(serde_json::json!({ "chat": chat })))
}

/// Query for the stream chat/gift list endpoints: a page `limit` plus an optional
/// `since` forward cursor (the last id the client rendered) for late-joiner
/// catch-up. Shared by `stream_chat_list` and `stream_gift_list`.
#[derive(serde::Deserialize)]
struct CursorLimitQuery {
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    since: Option<String>,
}

#[derive(Deserialize)]
struct ChatPostReq {
    body: String,
}

async fn stream_chat_post(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<ChatPostReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    // Reject a banned/timed-out poster before the line is accepted/broadcast.
    if aero_storage::StreamModRepo::new(s.participants.pool().clone())
        .is_banned(id, auth.participant_id, time::OffsetDateTime::now_utc())
        .await?
    {
        return Err(AeroError::Forbidden("banned from this stream's chat".into()).into());
    }
    // Enforce Twitch-style chat modes (slow mode / follower-only / subscriber-only)
    // before the line is accepted/broadcast.
    crate::stream_chat_modes::enforce_chat_modes(&s, id, auth.participant_id).await?;
    // Subscriber-badge flag (migration 0082): resolved at the edge from the
    // SubscriptionRepo + stream owner; degrades to false on any miss.
    let is_sub = s.live.subscriber_flag(id, auth.participant_id).await;
    let line = s.live.post_chat(auth.participant_id, id, req.body, is_sub).await?;
    Ok(Json(serde_json::to_value(line).map_err(AeroError::from)?))
}

async fn stream_gift_list(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<CursorLimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    // Best-effort forward catch-up cursor (last gift id rendered); malformed →
    // recent tail. Symmetric with `stream_chat_list`.
    let since = q.since.as_deref().and_then(|c| ulid::Ulid::from_string(c.trim()).ok());
    let gifts = s.live.recent_gifts_since(id, since, q.limit.unwrap_or(30)).await?;
    Ok(Json(serde_json::json!({ "gifts": gifts })))
}

#[derive(Deserialize)]
struct GiftSendReq {
    gift_id: String,
    #[serde(default)]
    qty: Option<u32>,
}

async fn stream_gift_send(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    headers: header::HeaderMap,
    Json(req): Json<GiftSendReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    let qty = req.qty.unwrap_or(1);
    // Optional idempotency: a retried POST carrying the same `Idempotency-Key`
    // records/broadcasts the gift exactly once (money-path double-charge guard).
    let idem = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (line, inserted) = s
        .live
        .send_gift(auth.participant_id, id, &req.gift_id, qty, idem)
        .await?;
    // Feed the gift into the hype train (escalating combo-gift momentum). Runs
    // only on a fresh send; best-effort (never fails the gift). Skipped on a dedup
    // hit so a resend can't double-count the train.
    if inserted {
        crate::hype_train::on_gift(&s, id, auth.participant_id, qty).await;
    }
    Ok(Json(serde_json::to_value(line).map_err(AeroError::from)?))
}

async fn stream_leaderboard(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    let rows = s.live.leaderboard(id, q.limit.unwrap_or(10)).await?;
    Ok(Json(serde_json::json!({ "leaderboard": rows })))
}

async fn stream_end(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;
    s.live.end_stream(auth.participant_id, id).await?;
    // Auto-finalize a VOD when the stream was flagged for recording. Best-effort:
    // the stream has already ended, so a recording hiccup must not fail the
    // request. Re-read the (now-Ended) stream so the VOD captures the final state.
    // The explicit `POST /api/streams/:id/vod` route remains the canonical path.
    let mut recorded_vod: Option<serde_json::Value> = None;
    if let Ok(Some(stream)) = s.streams.get(id).await {
        let flagged = aero_storage::VodRepo::new(s.participants.pool().clone())
            .is_recording(id)
            .await
            .unwrap_or(Some(false))
            .unwrap_or(false);
        if flagged {
            match crate::vod::finalize_recording(&s, &stream).await {
                Ok(vod) => recorded_vod = serde_json::to_value(&vod).ok(),
                Err(e) => tracing::warn!(error = ?e, stream = %id, "auto-VOD finalize failed"),
            }
        }
    }
    Ok(Json(serde_json::json!({ "ok": true, "vod": recorded_vod })))
}

fn strip_scheme(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_owned()
}

// ----- Agents -----

#[derive(Deserialize)]
struct CreateAgentReq {
    display_name: String,
    #[serde(default)]
    kind: Option<String>, // "bot" | "agent"
    #[serde(default)]
    avatar_url: Option<String>,
}

async fn create_agent(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateAgentReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let kind = match req.kind.as_deref().unwrap_or("bot") {
        "agent" => aero_common::ParticipantKind::Agent,
        _ => aero_common::ParticipantKind::Bot,
    };
    let bot = s
        .participants
        .create_bot(&req.display_name, kind, Some(auth.participant_id), req.avatar_url.as_deref())
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(bot).map_err(AeroError::from)?))
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

// ----- MLS (P8) — opaque-bytes relay -----

#[derive(Deserialize)]
struct PublishKpReq {
    ciphersuite: String,
    /// Base64-encoded KeyPackage bytes.
    payload_b64: String,
}

fn b64_decode(s: &str) -> Result<Vec<u8>, AeroError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| AeroError::Invalid(format!("base64: {e}")))
}
fn b64_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

async fn mls_publish_kp(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<PublishKpReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let payload = b64_decode(&req.payload_b64)?;
    if payload.len() > 16 * 1024 {
        return Err(AeroError::Invalid("KeyPackage too large".into()).into());
    }
    let kp = s
        .key_packages
        .publish(auth.participant_id, &req.ciphersuite, payload)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "id": kp.id,
        "ciphersuite": kp.ciphersuite,
        "created_at": kp.created_at,
    })))
}

async fn mls_consume_kp(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(pid_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = ParticipantId::from_str(&pid_str)
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))?;
    let kp = s
        .key_packages
        .consume_one(target)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("no fresh KeyPackages".into()))?;
    Ok(Json(serde_json::json!({
        "id": kp.id,
        "participant_id": kp.participant_id,
        "ciphersuite": kp.ciphersuite,
        "payload_b64": b64_encode(&kp.payload),
        "created_at": kp.created_at,
    })))
}

#[derive(Deserialize)]
struct UpsertGroupReq {
    group_id_b64: String,
    ciphersuite: String,
    epoch: u64,
    state_b64: String,
    #[serde(default)]
    room_id: Option<String>,
}

async fn mls_upsert_group(
    State(s): State<AppState>,
    _auth: AuthUser,
    Json(req): Json<UpsertGroupReq>,
) -> ApiResult<axum::http::StatusCode> {
    let group_id = aero_common::mls::MlsGroupId::new(b64_decode(&req.group_id_b64)?);
    let state_bytes = b64_decode(&req.state_b64)?;
    let room_id = req.room_id.as_deref().map(parse_room_id).transpose()?;
    let g = aero_common::mls::MlsGroupState {
        group_id,
        room_id,
        ciphersuite: req.ciphersuite,
        epoch: req.epoch,
        state: state_bytes,
        updated_at: time::OffsetDateTime::now_utc(),
    };
    s.mls_groups.upsert(&g).await.map_err(AeroError::from)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn mls_get_group(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(gid_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let gid_bytes = b64_decode(&gid_str)?;
    let gid = aero_common::mls::MlsGroupId::new(gid_bytes);
    let g = s
        .mls_groups
        .get(&gid)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("group".into()))?;
    Ok(Json(serde_json::json!({
        "group_id_b64": b64_encode(g.group_id.as_bytes()),
        "room_id": g.room_id,
        "ciphersuite": g.ciphersuite,
        "epoch": g.epoch,
        "state_b64": b64_encode(&g.state),
        "updated_at": g.updated_at,
    })))
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
                            Ok(delivery_id) => {
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
        HeaderValue::from_str(&format!("/whip/resource/{}", stream.id))
            .unwrap_or_else(|_| HeaderValue::from_static("/whip/resource")),
    );
    Ok(resp)
}

async fn whip_delete(
    State(s): State<AppState>,
    Path(stream_id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let stream_id = ulid::Ulid::from_str(&stream_id_str)
        .map_err(|e| AeroError::Invalid(format!("stream id: {e}")))?;
    s.whip.remove(stream_id);
    if let Err(e) = s.stream_routes.unpublish(stream_id).await {
        tracing::warn!(error=?e, stream=%stream_id, "stream route unpublish failed");
    }
    if let Err(e) = s.streams.mark_ended(stream_id).await {
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

// ----- RTC config -----

async fn rtc_config(_auth: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(rtc_config_payload()))
}

fn rtc_config_payload() -> serde_json::Value {
    let stun = std::env::var("AERO_STUN_URLS")
        .unwrap_or_else(|_| "stun:stun.l.google.com:19302".into());
    let urls: Vec<String> = stun.split(',').map(|s| s.trim().to_owned()).collect();
    let mut ice_servers = vec![serde_json::json!({"urls": urls})];
    if let (Ok(url), Ok(user), Ok(pass)) = (
        std::env::var("AERO_TURN_URL"),
        std::env::var("AERO_TURN_USERNAME"),
        std::env::var("AERO_TURN_PASSWORD"),
    ) {
        ice_servers.push(serde_json::json!({
            "urls": [url],
            "username": user,
            "credential": pass,
        }));
    }
    serde_json::json!({
        "ice_servers": ice_servers,
        "ice_transport_policy": "all",
    })
}

// ----- helpers -----

fn parse_room_kind(s: &str) -> AeroResult<RoomKind> {
    match s {
        "direct" => Ok(RoomKind::Direct),
        "group" => Ok(RoomKind::Group),
        "channel" => Ok(RoomKind::Channel),
        _ => Err(AeroError::Invalid(format!("unknown room kind: {s}"))),
    }
}

/// Merge two SearchHit lists (FTS + vector). Dedupes by message id, takes the
/// max score per id, returns top `limit` ordered by score desc.
fn merge_hits(
    a: Vec<aero_storage::SearchHit>,
    b: Vec<aero_storage::SearchHit>,
    limit: i64,
) -> Vec<aero_storage::SearchHit> {
    use std::collections::HashMap;
    let mut best: HashMap<MessageId, aero_storage::SearchHit> = HashMap::new();
    for h in a.into_iter().chain(b.into_iter()) {
        let id = h.message.id;
        match best.get(&id) {
            Some(existing) if existing.score >= h.score => {}
            _ => {
                best.insert(id, h);
            }
        }
    }
    let mut out: Vec<_> = best.into_values().collect();
    out.sort_by(|x, y| y.score.partial_cmp(&x.score).unwrap_or(std::cmp::Ordering::Equal));
    let limit = limit.clamp(1, 100) as usize;
    out.truncate(limit);
    out
}

fn parse_room_id(s: &str) -> AeroResult<RoomId> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt as _; // `oneshot`

    /// The liveness handler takes no state, so it mounts on a state-free router
    /// and is fully testable offline (it must never touch PG/Redis/NATS).
    #[tokio::test]
    async fn health_live_returns_200_without_dependencies() {
        let app: Router = Router::new().route("/health/live", get(health_live));
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/health/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], "ok");
    }

    #[test]
    fn readiness_decision_draining_takes_precedence() {
        // Draining → 503 regardless of dependency health.
        assert_eq!(
            readiness_decision(true, true),
            (StatusCode::SERVICE_UNAVAILABLE, "draining")
        );
        assert_eq!(
            readiness_decision(true, false),
            (StatusCode::SERVICE_UNAVAILABLE, "draining")
        );
        // Not draining: ready iff every dependency is healthy.
        assert_eq!(readiness_decision(false, true), (StatusCode::OK, "ready"));
        assert_eq!(
            readiness_decision(false, false),
            (StatusCode::SERVICE_UNAVAILABLE, "not_ready")
        );
    }

    #[test]
    fn history_limit_defaults_and_clamps() {
        // Absent ⇒ the documented default page size.
        assert_eq!(history_limit(None), DEFAULT_HISTORY_LIMIT);
        // Below the floor clamps up to 1; zero/negatives are never honored.
        assert_eq!(history_limit(Some(0)), 1);
        assert_eq!(history_limit(Some(-10)), 1);
        // In-window values pass through.
        assert_eq!(history_limit(Some(50)), 50);
        assert_eq!(history_limit(Some(MAX_HISTORY_LIMIT)), MAX_HISTORY_LIMIT);
        // Above the ceiling clamps down to the cap (mirrors the storage clamp).
        assert_eq!(history_limit(Some(MAX_HISTORY_LIMIT + 1)), MAX_HISTORY_LIMIT);
        assert_eq!(history_limit(Some(i64::MAX)), MAX_HISTORY_LIMIT);
    }

    #[test]
    fn parse_cursor_validates_and_labels() {
        // Absent ⇒ Ok(None).
        assert!(parse_cursor(None, "since").unwrap().is_none());
        // Valid id ⇒ Some(id), whitespace tolerated.
        let id = MessageId::new();
        assert_eq!(parse_cursor(Some(&id.to_string()), "since").unwrap(), Some(id));
        assert_eq!(parse_cursor(Some(&format!(" {id} ")), "before").unwrap(), Some(id));
        // Garbage ⇒ Invalid error carrying the field label so the client knows
        // which cursor was bad.
        let err = parse_cursor(Some("nope"), "since").unwrap_err();
        match err {
            AeroError::Invalid(msg) => assert!(msg.contains("since id"), "got: {msg}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    /// Router-level test that the history route path + `HistoryQuery` extractor
    /// accept the `?since=` (and `before`/`limit`) params offline. Mounts the
    /// real path pattern and the real `HistoryQuery` type on a stand-in handler
    /// (the production handler needs a full `AppState` → PG/Redis, absent in CI),
    /// so this proves routing + query extraction without external deps.
    #[tokio::test]
    async fn history_route_accepts_since_query() {
        async fn probe(
            Path(room): Path<String>,
            Query(q): Query<HistoryQuery>,
        ) -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "room": room,
                "since": q.since,
                "before": q.before,
                "limit": q.limit,
            }))
        }
        let app: Router =
            Router::new().route("/api/rooms/:id/messages", get(probe));

        let id = MessageId::new();
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri(format!("/api/rooms/room-1/messages?since={id}&limit=50"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // 200 (not 404/400) proves the path matched and `since` deserialized.
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["since"], id.to_string());
        assert_eq!(v["limit"], 50);
        assert!(v["before"].is_null());
    }

    // ----- Workspace scoping (multi-tenant rollout) -----

    #[test]
    fn default_workspace_id_is_the_all_zero_uuid() {
        // Migration 0006 backfilled `rooms.workspace_id` (and the default
        // workspace row) with the all-zero UUID. The const MUST map to exactly
        // that, or omitting `workspace_id` would target the wrong (or a
        // nonexistent) tenant.
        assert_eq!(DEFAULT_WORKSPACE_ID.to_uuid(), uuid::Uuid::nil());
        // And it round-trips through the same UUID constructor the storage layer
        // binds with.
        assert_eq!(DEFAULT_WORKSPACE_ID, WorkspaceId::from_uuid(uuid::Uuid::nil()));
    }

    #[test]
    fn resolve_workspace_id_defaults_when_absent() {
        // Absent (single-tenant client) ⇒ the legacy default workspace.
        assert_eq!(resolve_workspace_id(None).unwrap(), DEFAULT_WORKSPACE_ID);
    }

    #[test]
    fn resolve_workspace_id_uses_provided_value() {
        // Present + valid ⇒ exactly that workspace, whitespace tolerated.
        let ws = WorkspaceId::new();
        assert_eq!(resolve_workspace_id(Some(&ws.to_string())).unwrap(), ws);
        assert_eq!(resolve_workspace_id(Some(&format!("  {ws}  "))).unwrap(), ws);
    }

    #[test]
    fn resolve_workspace_id_rejects_garbage() {
        // Present + undecodable ⇒ Invalid (400), NOT a silent fall-through to the
        // default (which would mask a client bug and cross tenant boundaries).
        let err = resolve_workspace_id(Some("not-a-ulid")).unwrap_err();
        match err {
            AeroError::Invalid(msg) => assert!(msg.contains("workspace id"), "got: {msg}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    /// Router-level proof that `CreateRoomReq` parses the OPTIONAL `workspace_id`
    /// body field and that the handler's default-selection composes correctly:
    /// omitting it resolves to [`DEFAULT_WORKSPACE_ID`], supplying it resolves to
    /// that id. Uses a stand-in handler with the real request type + the real
    /// `resolve_workspace_id` (the production handler needs a full `AppState`).
    #[tokio::test]
    async fn create_room_body_parses_optional_workspace_id() {
        async fn probe(Json(req): Json<CreateRoomReq>) -> Json<serde_json::Value> {
            let ws = resolve_workspace_id(req.workspace_id.as_deref())
                .expect("valid workspace id in test");
            Json(serde_json::json!({ "kind": req.kind, "workspace": ws.to_string() }))
        }
        let app: Router = Router::new().route("/api/rooms", post(probe));

        // (a) Body WITHOUT workspace_id ⇒ resolves to the default workspace.
        let resp = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/rooms")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"kind":"group","name":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["kind"], "group");
        assert_eq!(v["workspace"], DEFAULT_WORKSPACE_ID.to_string());

        // (b) Body WITH workspace_id ⇒ resolves to exactly that workspace.
        let ws = WorkspaceId::new();
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/rooms")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"kind":"channel","workspace_id":"{ws}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["kind"], "channel");
        assert_eq!(v["workspace"], ws.to_string());
    }

    /// Router-level proof that `list_rooms`'s `ListRoomsQuery` extractor accepts an
    /// optional `?workspace_id=` and exposes it (present vs absent) so the handler
    /// can branch scoped-vs-all. Stand-in handler (the real one needs `AppState`).
    #[tokio::test]
    async fn list_rooms_query_parses_optional_workspace_id() {
        async fn probe(Query(q): Query<ListRoomsQuery>) -> Json<serde_json::Value> {
            Json(serde_json::json!({ "workspace_id": q.workspace_id }))
        }
        let app: Router = Router::new().route("/api/rooms", get(probe));

        // Absent ⇒ None (handler keeps legacy "all my rooms" behavior).
        let resp = app
            .clone()
            .oneshot(HttpRequest::builder().uri("/api/rooms").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["workspace_id"].is_null());

        // Present ⇒ surfaced verbatim (handler scopes via rooms_for_in_workspace).
        let ws = WorkspaceId::new();
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri(format!("/api/rooms?workspace_id={ws}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["workspace_id"], ws.to_string());
    }
}
