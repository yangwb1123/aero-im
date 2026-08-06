#![allow(unused_imports)]
//! HTTP route definitions. WS upgrade lives in `ws::handler`.
use crate::error::ApiResult;
use crate::metrics;
use crate::routes::helpers::{merge_hits, parse_room_id, parse_room_kind};
use crate::state::AppState;
use crate::ws;
#[allow(unused_imports)]
use aero_auth::{AuthUser, LoginRequest, RegisterRequest};
#[allow(unused_imports)]
use aero_common::{
    BlobId, Error as AeroError, FileKind, MessageId, ParticipantId, Result as AeroResult, RoomId,
    RoomKind, StreamProtocol, StreamStatus, WorkspaceId, WorkspaceRole,
};
#[allow(unused_imports)]
use aero_live_whip::{MediaRelay, SessionError, WhepSession, WhipResource, WhipSession};
#[allow(unused_imports)]
use aero_storage::blob::NewBlob;
#[allow(unused_imports)]
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
#[allow(unused_imports)]
use bytes::Bytes;
#[allow(unused_imports)]
use serde::Deserialize;
#[allow(unused_imports)]
use sha2::Digest;
#[allow(unused_imports)]
use std::str::FromStr;
#[allow(unused_imports)]
use tower_http::compression::{
    predicate::{DefaultPredicate, Predicate},
    CompressionLayer,
};
#[allow(unused_imports)]
use uuid::Uuid;
/// Opaque correlation ID propagated through request extensions and echoed in
/// every response as `x-request-id`.  Handlers and middlewares that need to
/// surface it can extract it from `req.extensions()`.
#[derive(Clone)]
pub struct RequestId(pub String);
/// Middleware: read or generate a `x-request-id` header, attach a
/// [`RequestId`] extension, and echo the value in the response.
async fn inject_request_id(mut req: Request<axum::body::Body>, next: Next) -> Response {
    #[allow(unused_imports)]
    use tracing::Instrument as _;
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok()).map_or_else(|| uuid::Uuid::new_v4().to_string(), String::from);
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
    if let Some(tp) = req
        .headers()
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
    {
        aero_common::telemetry::set_span_parent_from_traceparent(&span, tp);
    }
    let mut res = next.run(req).instrument(span).await;
    if let Ok(v) = HeaderValue::from_str(&id) {
        res.headers_mut().insert("x-request-id", v);
    }
    res
}

fn response_compression_layer() -> CompressionLayer<impl Predicate> {
    CompressionLayer::new().compress_when(DefaultPredicate::new().and(
        |_: StatusCode,
         _: axum::http::Version,
         headers: &axum::http::HeaderMap,
         _: &axum::http::Extensions| {
            // A byte-addressable representation must remain the exact bytes
            // covered by its strong ETag and Range offsets. tower-http otherwise
            // gzip-compresses eligible 200 responses and removes Accept-Ranges.
            !headers.contains_key(header::ACCEPT_RANGES)
        },
    ))
}

pub fn build(state: AppState) -> Router {
    let mut router = Router::new()
        // Health probes — extracted to routes/health.rs
        .merge(crate::routes::health::routes())
        // Auth
        .route("/api/auth/register", post(auth_register))
        .route("/api/auth/login", post(auth_login))
        // Backward-compatible recovery entry point. It accepts the same
        // password-first LoginReq as /login, with `recovery_code` populated.
        .route("/api/auth/2fa/recover", post(auth_login))
        .route("/api/auth/login-history", get(auth_login_history))
        .route("/api/me", get(me).patch(update_me))
        // Rooms
        .route("/api/rooms", post(create_room).get(list_rooms))
        .route("/api/rooms/:id/members", post(add_member))
        .route(
            "/api/rooms/:id/messages",
            get(room_history).post(create_message),
        )
        .route("/api/rooms/:id/changes", get(room_changes))
        .route(
            "/api/rooms/:id/delivery-cursor",
            get(crate::routes::reads::get_delivery_cursor),
        )
        .route(
            "/api/rooms/:id/search",
            post(crate::routes::search::room_search),
        )
        // Messages
        .route(
            "/api/messages/:id",
            axum::routing::get(get_message)
                .patch(edit_message)
                .delete(delete_message),
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
        .route("/api/rooms/:id/blobs", post(room_blob_upload))
        .route("/api/blobs/:id", get(blob_download))
        // AI
        .merge(crate::routes::ai::routes())
        // Live streams
        .merge(crate::routes::live::routes())
        // WHIP / WHEP — body is SDP text, response is SDP text
        .route("/whip/:stream_key", post(whip_post))
        .route(
            "/whip/resource/:stream_key",
            axum::routing::delete(whip_delete),
        )
        .route("/whep/:stream_id", post(whep_post))
        .route(
            "/whep/resource/:stream_id/:viewer_id",
            axum::routing::delete(whep_delete),
        )
        // Agents / Bots
        .merge(crate::routes::agents::routes())
        .route("/api/bots", post(bot_create).get(bot_list))
        .route("/api/bots/:id/token", post(bot_rotate_token))
        .route(
            "/api/bots/:id/subscriptions",
            get(bot_list_subscriptions).post(bot_create_subscription),
        )
        .route(
            "/api/bots/:id/subscriptions/:sub_id",
            axum::routing::delete(bot_delete_subscription),
        )
        .route(
            "/api/bots/:id/subscriptions/:sub_id/secret",
            post(bot_rotate_subscription_secret),
        )
        .route("/api/bots/:id/deliveries", get(bot_list_deliveries))
        .route(
            "/api/bots/:id/deliveries/:delivery_id/requeue",
            post(bot_requeue_delivery),
        )
        .route("/api/participants", get(search_participants))
        .route("/api/participants/:id", get(get_participant))
        .route("/api/rooms/:id/members/list", get(list_room_members))
        // MLS E2E — extracted to routes/mls.rs
        .merge(crate::routes::mls::routes())
        // RTC config
        .merge(crate::routes::rtc::routes())
        // WebSocket
        .merge(crate::routes::reads::routes())
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
        .merge(crate::identity_migrations::routes())
        .merge(crate::integrations::routes())
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
        // ---- internal call-bridge subscribe + RTCP feedback (secret-gated) ----
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
        .layer(response_compression_layer())
        .with_state(state)
}

include!("handlers/auth.rs");
include!("handlers/rooms.rs");
include!("handlers/messages.rs");
include!("handlers/threads.rs");
include!("handlers/blobs.rs");
include!("handlers/bots.rs");
include!("handlers/bot_subs.rs");
include!("handlers/whip.rs");

#[cfg(test)]
mod tests {
    include!("routes_tests.rs");
}
