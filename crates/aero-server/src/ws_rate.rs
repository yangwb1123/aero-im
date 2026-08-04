//! Per-workspace (tenant) request ceiling — tiers, enforcement, and the admin
//! API (ROADMAP3 方向五 — 租户公平).
//!
//! The per-client limiter in [`crate::rate_limit`] caps each *participant* at
//! ~20 req/s, but a 100-person workspace can still collude to ~2000 req/s and
//! exhaust the shared PG pool. This module adds a second, cluster-wide ceiling
//! per **workspace**, with tiers:
//!
//! * `standard`  — `AERO_WS_RATE_STANDARD_PER_MIN` (default 1200/min),
//! * `premium`   — `AERO_WS_RATE_PREMIUM_PER_MIN` (default 6000/min),
//! * `unlimited` — no workspace ceiling (per-client limits still apply).
//!
//! The tier token is stored on `workspaces.rate_tier` (migration 0076,
//! [`aero_storage::WorkspaceRepo::set_rate_tier_authorized`]); the counter is a Redis
//! fixed window ([`aero_storage::WsRateStore`]) shared by every gateway node,
//! so the budget cannot be multiplied by spraying requests across nodes.
//!
//! ## Where it is enforced (deliberate, not blanket)
//!
//! The check is wired into the *high-traffic, DB-heavy* choke points only —
//! send message (WS frame), message history, room search, and blob
//! upload/download — rather than as middleware over every route. Reasons:
//!
//! * those routes dominate a tenant's load on the PG pool / blob store, so
//!   capping them caps the damage a colluding workspace can do;
//! * a blanket middleware would have to resolve the tenant for *every* URL
//!   shape (most admin/CRUD routes are cheap and already per-client limited),
//!   paying a resolution lookup precisely where it buys the least;
//! * keeping call sites explicit makes the charged surface auditable — grep
//!   for `check_ws_rate` to see exactly what counts against the budget.
//!
//! Room-scoped call sites charge **after** `assert_room_access`, so a
//! non-member cannot drain a victim workspace's budget by spamming its room
//! ids (the per-client limiter bounds the cost of the access check itself).
//!
//! ## Tenant resolution caches
//!
//! Room → workspace and workspace → tier are resolved through small TTL maps
//! ([`WsRateEnforcer`], ~60 s) so steady-state enforcement costs one Redis
//! `INCR` per request, not extra PG queries — the whole point is to *protect*
//! PG. Blob routes have no room context (blobs are owner-scoped), so they
//! charge the caller's most recently created workspace via a third cached
//! lookup; participants in no workspace skip the tenant check (their personal
//! traffic is already per-client limited).
//!
//! ## Fail-open
//!
//! Availability beats enforcement: any Redis/PG error during the check is
//! logged, counted in [`WS_RATE_FAIL_OPEN_TOTAL`], and the request proceeds.
//! Rejections (HTTP 429 / WS `error` frame) are counted in
//! [`WS_RATE_REJECTIONS_TOTAL`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use aero_common::{
    metrics as common_metrics, Error as AeroError, ParticipantId, Result as AeroResult, RoomId,
    WorkspaceId,
};
use aero_storage::WsRateStore;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::put,
    Json, Router,
};
use dashmap::DashMap;
use serde::Deserialize;
use std::str::FromStr;

use aero_auth::AuthUser;

use crate::error::ApiResult;
use crate::state::AppState;

// ---------- Metrics (server-local names, mirroring crate::metrics) ----------

/// Counter: requests rejected by the per-workspace ceiling (429).
pub const WS_RATE_REJECTIONS_TOTAL: &str = "aero_ws_rate_rejections_total";
/// Counter: checks skipped because Redis/PG errored mid-check (fail-open).
pub const WS_RATE_FAIL_OPEN_TOTAL: &str = "aero_ws_rate_fail_open_total";

// ---------- Tiers (pure, unit-tested) ----------

/// A workspace's rate-limit tier, parsed from the `workspaces.rate_tier` token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsRateTier {
    /// Default tier — every workspace starts here (migration 0076 default).
    Standard,
    /// Elevated ceiling for paying tenants.
    Premium,
    /// No per-workspace ceiling (per-client limits still apply).
    Unlimited,
}

impl WsRateTier {
    /// Strict parse of a tier token (case-insensitive, trimmed). `None` for
    /// anything outside the known set — used by the admin `PUT` so a typo is a
    /// `400`, never silently persisted.
    #[must_use]
    pub fn parse_strict(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "standard" => Some(Self::Standard),
            "premium" => Some(Self::Premium),
            "unlimited" => Some(Self::Unlimited),
            _ => None,
        }
    }

    /// Lenient parse used on the *read* path: an unknown stored token degrades
    /// to [`Self::Standard`] (the tightest tier), never to "no limit".
    #[must_use]
    pub fn parse(token: &str) -> Self {
        Self::parse_strict(token).unwrap_or(Self::Standard)
    }

    /// The canonical storage token for this tier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Premium => "premium",
            Self::Unlimited => "unlimited",
        }
    }
}

/// Default `standard` ceiling: 1200 requests / workspace / minute (20 rps
/// sustained for the whole tenant — the same as ONE client's burst budget
/// today, scaled to a minute window).
pub const DEFAULT_STANDARD_PER_MIN: u64 = 1200;
/// Default `premium` ceiling: 6000 requests / workspace / minute.
pub const DEFAULT_PREMIUM_PER_MIN: u64 = 6000;

/// Per-tier request ceilings (requests per workspace per minute window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WsRateLimits {
    /// Ceiling for [`WsRateTier::Standard`] workspaces.
    pub standard_per_min: u64,
    /// Ceiling for [`WsRateTier::Premium`] workspaces.
    pub premium_per_min: u64,
}

impl Default for WsRateLimits {
    fn default() -> Self {
        Self {
            standard_per_min: DEFAULT_STANDARD_PER_MIN,
            premium_per_min: DEFAULT_PREMIUM_PER_MIN,
        }
    }
}

impl WsRateLimits {
    /// Resolve limits from raw env-var strings. Missing or unparsable values
    /// fall back to the defaults; a parsed `0` is floored to `1` so a misconfig
    /// can never hard-block a whole tier (use `unlimited` to *remove* a limit,
    /// not a zero to add an infinite one).
    #[must_use]
    pub fn resolve(standard: Option<&str>, premium: Option<&str>) -> Self {
        fn parse(raw: Option<&str>, default: u64) -> u64 {
            raw.and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(default)
                .max(1)
        }
        Self {
            standard_per_min: parse(standard, DEFAULT_STANDARD_PER_MIN),
            premium_per_min: parse(premium, DEFAULT_PREMIUM_PER_MIN),
        }
    }

    /// Read `AERO_WS_RATE_STANDARD_PER_MIN` / `AERO_WS_RATE_PREMIUM_PER_MIN`
    /// from the process environment (once, at startup).
    #[must_use]
    pub fn from_env() -> Self {
        Self::resolve(
            std::env::var("AERO_WS_RATE_STANDARD_PER_MIN")
                .ok()
                .as_deref(),
            std::env::var("AERO_WS_RATE_PREMIUM_PER_MIN")
                .ok()
                .as_deref(),
        )
    }

    /// The per-minute ceiling for a tier, or `None` when the tier is exempt
    /// ([`WsRateTier::Unlimited`]).
    #[must_use]
    pub const fn limit_for(self, tier: WsRateTier) -> Option<u64> {
        match tier {
            WsRateTier::Standard => Some(self.standard_per_min),
            WsRateTier::Premium => Some(self.premium_per_min),
            WsRateTier::Unlimited => None,
        }
    }
}

/// Is a post-increment window count over the ceiling? `count == limit` is the
/// last permitted request (the counter is incremented *before* the comparison,
/// so exactly `limit` requests pass per window).
#[must_use]
pub const fn over_budget(count: u64, limit: u64) -> bool {
    count > limit
}

// ---------- TTL caches (pure freshness rule, unit-tested) ----------

/// How long cached tenant resolutions (room→workspace, workspace→tier,
/// participant→workspace) are trusted before being re-read from PG. Long
/// enough to amortize the lookup across many requests, short enough that a
/// tier change or room move propagates cluster-wide within a minute.
pub const CACHE_TTL: Duration = Duration::from_secs(60);

/// The freshness rule shared by all three caches: a cached value is usable iff
/// it was stamped less than `ttl` ago. Pure, so the TTL policy is testable
/// with synthetic instants.
fn fresh<V: Copy>(cached: Option<(V, Instant)>, now: Instant, ttl: Duration) -> Option<V> {
    cached.and_then(|(v, at)| (now.saturating_duration_since(at) < ttl).then_some(v))
}

/// Shared enforcement state: the Redis window counter, the resolved tier
/// limits, and the TTL resolution caches. One per process, cloned cheaply into
/// handlers via [`AppState`] (`Arc`s all the way down).
///
/// Cache growth is bounded by the number of *active* rooms / workspaces /
/// participants on this node (entries are overwritten in place on refresh) —
/// the same containment argument as the per-client token buckets in
/// [`crate::rate_limit`].
#[derive(Clone)]
pub struct WsRateEnforcer {
    store: WsRateStore,
    limits: WsRateLimits,
    /// room → (workspace, stamped-at). Rooms never change workspace today, but
    /// the TTL keeps the map self-correcting if that ever changes.
    room_ws: Arc<DashMap<RoomId, (WorkspaceId, Instant)>>,
    /// workspace → (tier, stamped-at). Also written through directly by the
    /// admin `PUT` so a tier change takes effect immediately on this node.
    tiers: Arc<DashMap<WorkspaceId, (WsRateTier, Instant)>>,
    /// participant → (their most recently created workspace, stamped-at).
    /// `None` = confirmed member of no workspace (negative result is cached
    /// too, so workspace-less users don't re-query PG every request).
    member_ws: Arc<DashMap<ParticipantId, (Option<WorkspaceId>, Instant)>>,
}

impl WsRateEnforcer {
    /// Build with explicit limits (tests / callers that resolved env earlier).
    #[must_use]
    pub fn new(store: WsRateStore, limits: WsRateLimits) -> Self {
        Self {
            store,
            limits,
            room_ws: Arc::new(DashMap::new()),
            tiers: Arc::new(DashMap::new()),
            member_ws: Arc::new(DashMap::new()),
        }
    }

    /// Build with limits read from `AERO_WS_RATE_*_PER_MIN`.
    #[must_use]
    pub fn from_env(store: WsRateStore) -> Self {
        Self::new(store, WsRateLimits::from_env())
    }

    /// The resolved per-tier ceilings (for the member-readable `GET`).
    #[must_use]
    pub const fn limits(&self) -> WsRateLimits {
        self.limits
    }

    /// Write-through a tier change so enforcement on this node flips
    /// immediately (other nodes converge within [`CACHE_TTL`]).
    pub fn note_tier(&self, workspace: WorkspaceId, tier: WsRateTier) {
        self.tiers.insert(workspace, (tier, Instant::now()));
    }
}

/// Log + count one fail-open (enforcement skipped, request allowed through).
fn fail_open(workspace: Option<WorkspaceId>, stage: &str, err: &dyn std::fmt::Debug) {
    common_metrics::inc_counter(WS_RATE_FAIL_OPEN_TOTAL, 1);
    tracing::warn!(error = ?err, ?workspace, stage, "ws-rate check failed open");
}

// ---------- Enforcement entry points ----------

/// Charge one request against `workspace`'s per-minute budget.
///
/// Resolves the tier (cached, [`CACHE_TTL`]), increments the cluster-wide
/// Redis window counter, and rejects with [`AeroError::RateLimited`] (HTTP
/// 429) once the tier's ceiling is exceeded. `unlimited` workspaces skip the
/// counter entirely. Any Redis/PG error fails OPEN (see module docs).
///
/// # Errors
/// [`AeroError::RateLimited`] when the workspace is over its window budget.
pub async fn check_ws_rate(state: &AppState, workspace: WorkspaceId) -> AeroResult<()> {
    let e = &state.ws_rate;
    let now = Instant::now();

    let cached = e.tiers.get(&workspace).map(|r| *r.value());
    let tier = match fresh(cached, now, CACHE_TTL) {
        Some(t) => t,
        None => match state.workspaces.rate_tier(workspace).await {
            // Unknown workspace ⇒ treat as standard; downstream access checks
            // 404/403 it anyway, and charging a dead key is harmless.
            Ok(token) => {
                let t = token
                    .as_deref()
                    .map_or(WsRateTier::Standard, WsRateTier::parse);
                e.tiers.insert(workspace, (t, now));
                t
            }
            Err(err) => {
                fail_open(Some(workspace), "tier-lookup", &err);
                return Ok(());
            }
        },
    };

    let Some(limit) = e.limits.limit_for(tier) else {
        return Ok(()); // unlimited tier — no workspace ceiling.
    };

    match e.store.incr_current(workspace).await {
        Ok(count) if over_budget(count, limit) => {
            common_metrics::inc_counter(WS_RATE_REJECTIONS_TOTAL, 1);
            tracing::debug!(%workspace, count, limit, tier = tier.as_str(), "ws-rate rejected");
            Err(AeroError::RateLimited)
        }
        Ok(_) => Ok(()),
        Err(err) => {
            fail_open(Some(workspace), "redis-incr", &err);
            Ok(())
        }
    }
}

/// [`check_ws_rate`] for room-scoped routes: resolves the room's workspace
/// through the TTL cache (PG fallback on a miss), then charges that tenant.
///
/// Unknown rooms skip the check — the handler's own access/404 logic owns that
/// outcome, and nothing sensible could be charged. **Call this only after
/// `assert_room_access`** so non-members cannot drain a workspace's budget by
/// spamming its room ids.
///
/// # Errors
/// [`AeroError::RateLimited`] when the room's workspace is over budget.
pub async fn check_ws_rate_room(state: &AppState, room: RoomId) -> AeroResult<()> {
    let e = &state.ws_rate;
    let now = Instant::now();

    let cached = e.room_ws.get(&room).map(|r| *r.value());
    let ws = match fresh(cached, now, CACHE_TTL) {
        Some(ws) => ws,
        None => match state.rooms.room_workspace(room).await {
            Ok(Some(ws)) => {
                e.room_ws.insert(room, (ws, now));
                ws
            }
            Ok(None) => return Ok(()), // unknown room — nothing to charge.
            Err(err) => {
                fail_open(None, "room-workspace-lookup", &err);
                return Ok(());
            }
        },
    };
    check_ws_rate(state, ws).await
}

/// [`check_ws_rate`] for routes with **no room/workspace in the URL** (the
/// blob endpoints): charges the caller's most recently created workspace,
/// resolved through the TTL cache. Participants who belong to no workspace
/// skip the tenant check (their traffic is already per-client limited); for
/// multi-workspace users the newest workspace absorbs the charge — a
/// documented approximation, since an owner-scoped blob has no tenant of its
/// own.
///
/// # Errors
/// [`AeroError::RateLimited`] when the resolved workspace is over budget.
pub async fn check_ws_rate_participant(
    state: &AppState,
    participant: ParticipantId,
) -> AeroResult<()> {
    let e = &state.ws_rate;
    let now = Instant::now();

    let cached = e.member_ws.get(&participant).map(|r| *r.value());
    let ws = match fresh(cached, now, CACHE_TTL) {
        Some(ws) => ws,
        None => match state.workspaces.list_for_participant(participant).await {
            Ok(list) => {
                let ws = list.first().map(|w| w.id);
                e.member_ws.insert(participant, (ws, now));
                ws
            }
            Err(err) => {
                fail_open(None, "participant-workspace-lookup", &err);
                return Ok(());
            }
        },
    };
    match ws {
        Some(ws) => check_ws_rate(state, ws).await,
        None => Ok(()), // not in any workspace — per-client limits only.
    }
}

// ---------- Admin API ----------

/// Mount the rate-tier admin routes. Folded into the main router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/workspaces/:id/rate-tier",
        put(set_rate_tier).get(get_rate_tier),
    )
}

fn parse_workspace_id(s: &str) -> AeroResult<WorkspaceId> {
    WorkspaceId::from_str(s).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// Resolve the caller's role, rejecting non-members (mirrors
/// `crate::workspaces::caller_role`, which is private to that module).
async fn caller_role(
    s: &AppState,
    ws: WorkspaceId,
    caller: ParticipantId,
) -> AeroResult<aero_common::WorkspaceRole> {
    s.workspaces
        .effective_member_role(ws, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))
}

#[derive(Deserialize)]
struct SetTierReq {
    tier: String,
}

/// `PUT /api/workspaces/:id/rate-tier` — **owner only**: set the workspace's
/// rate tier (`standard` | `premium` | `unlimited`). Unknown tokens are a
/// `400` (never persisted). Emits a `workspace.rate_tier` audit event and
/// write-throughs this node's tier cache so the change is immediate here
/// (other nodes converge within [`CACHE_TTL`]).
async fn set_rate_tier(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<SetTierReq>,
) -> ApiResult<StatusCode> {
    let ws = parse_workspace_id(&id_str)?;
    let tier = WsRateTier::parse_strict(&req.tier).ok_or_else(|| {
        AeroError::Invalid(format!(
            "unknown rate tier {:?} (expected standard | premium | unlimited)",
            req.tier
        ))
    })?;
    s.workspaces
        .set_rate_tier_authorized(ws, tier.as_str(), auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    s.ws_rate.note_tier(ws, tier);
    // Best-effort audit (observability, not a transactional invariant).
    if let Err(e) = s
        .audit
        .append(
            ws,
            Some(auth.participant_id),
            "workspace.rate_tier",
            None,
            serde_json::json!({ "tier": tier.as_str() }),
        )
        .await
    {
        tracing::warn!(error = ?e, %ws, "audit append failed for workspace.rate_tier");
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/workspaces/:id/rate-tier` — **any member**: the workspace's tier
/// plus the effective per-minute ceiling (`null` for `unlimited`), so a tenant
/// can see the budget it is being held to.
async fn get_rate_tier(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;
    // Membership (any role) is sufficient to read the tier.
    let _role = caller_role(&s, ws, auth.participant_id).await?;
    let token = s
        .workspaces
        .rate_tier(ws)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("workspace".into()))?;
    let tier = WsRateTier::parse(&token);
    Ok(Json(serde_json::json!({
        "tier": tier.as_str(),
        "per_minute_limit": s.ws_rate.limits().limit_for(tier),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- tier parsing -----

    #[test]
    fn parse_strict_accepts_known_tiers_case_insensitively() {
        assert_eq!(
            WsRateTier::parse_strict("standard"),
            Some(WsRateTier::Standard)
        );
        assert_eq!(
            WsRateTier::parse_strict("premium"),
            Some(WsRateTier::Premium)
        );
        assert_eq!(
            WsRateTier::parse_strict("unlimited"),
            Some(WsRateTier::Unlimited)
        );
        assert_eq!(
            WsRateTier::parse_strict(" Premium "),
            Some(WsRateTier::Premium)
        );
        assert_eq!(
            WsRateTier::parse_strict("UNLIMITED"),
            Some(WsRateTier::Unlimited)
        );
    }

    #[test]
    fn parse_strict_rejects_unknown_tokens() {
        for bad in ["", "gold", "premium+", "standard premium", "none", "0"] {
            assert_eq!(
                WsRateTier::parse_strict(bad),
                None,
                "{bad:?} must not parse"
            );
        }
    }

    #[test]
    fn lenient_parse_degrades_unknown_to_standard_never_unlimited() {
        assert_eq!(WsRateTier::parse("premium"), WsRateTier::Premium);
        assert_eq!(WsRateTier::parse("unlimited"), WsRateTier::Unlimited);
        // A corrupt/legacy token tightens to standard rather than opening up.
        assert_eq!(WsRateTier::parse("???"), WsRateTier::Standard);
        assert_eq!(WsRateTier::parse(""), WsRateTier::Standard);
    }

    #[test]
    fn as_str_round_trips_through_strict_parse() {
        for tier in [
            WsRateTier::Standard,
            WsRateTier::Premium,
            WsRateTier::Unlimited,
        ] {
            assert_eq!(WsRateTier::parse_strict(tier.as_str()), Some(tier));
        }
    }

    // ----- limit table -----

    #[test]
    fn limits_default_to_spec_values() {
        let l = WsRateLimits::default();
        assert_eq!(l.standard_per_min, 1200);
        assert_eq!(l.premium_per_min, 6000);
    }

    #[test]
    fn resolve_parses_env_strings_and_falls_back_on_garbage() {
        // Explicit values win.
        let l = WsRateLimits::resolve(Some("300"), Some("9000"));
        assert_eq!(
            l,
            WsRateLimits {
                standard_per_min: 300,
                premium_per_min: 9000
            }
        );
        // Missing / unparsable → defaults.
        let l = WsRateLimits::resolve(None, Some("not-a-number"));
        assert_eq!(l.standard_per_min, DEFAULT_STANDARD_PER_MIN);
        assert_eq!(l.premium_per_min, DEFAULT_PREMIUM_PER_MIN);
        // Whitespace tolerated.
        assert_eq!(
            WsRateLimits::resolve(Some(" 42 "), None).standard_per_min,
            42
        );
        // A configured 0 is floored to 1 (can't hard-block a tier by typo).
        assert_eq!(
            WsRateLimits::resolve(Some("0"), Some("0")).standard_per_min,
            1
        );
        assert_eq!(
            WsRateLimits::resolve(Some("0"), Some("0")).premium_per_min,
            1
        );
    }

    #[test]
    fn limit_for_maps_each_tier() {
        let l = WsRateLimits {
            standard_per_min: 100,
            premium_per_min: 500,
        };
        assert_eq!(l.limit_for(WsRateTier::Standard), Some(100));
        assert_eq!(l.limit_for(WsRateTier::Premium), Some(500));
        assert_eq!(l.limit_for(WsRateTier::Unlimited), None);
    }

    #[test]
    fn over_budget_permits_exactly_limit_requests() {
        // Counter is post-increment: the limit-th request reads `count == limit`.
        assert!(!over_budget(1, 3));
        assert!(!over_budget(3, 3)); // last permitted request
        assert!(over_budget(4, 3)); // first rejected request
        assert!(over_budget(u64::MAX, 3));
    }

    // ----- cache freshness rule -----

    #[test]
    fn fresh_honours_ttl_boundary() {
        let ttl = Duration::from_secs(60);
        let now = Instant::now();
        // Missing entry → miss.
        assert_eq!(fresh::<u8>(None, now, ttl), None);
        // Just-written entry → hit.
        assert_eq!(fresh(Some((7u8, now)), now, ttl), Some(7));
        // Inside the window → hit.
        let newish = now + Duration::from_secs(59);
        assert_eq!(fresh(Some((7u8, now)), newish, ttl), Some(7));
        // At/after the boundary → miss (strict `<`).
        assert_eq!(fresh(Some((7u8, now)), now + ttl, ttl), None);
        assert_eq!(
            fresh(Some((7u8, now)), now + ttl + Duration::from_secs(5), ttl),
            None
        );
    }

    #[test]
    fn fresh_tolerates_clock_going_backwards() {
        // `now` earlier than the stamp (monotonic clocks shouldn't do this, but
        // saturating_duration_since makes it a hit, not a panic/underflow).
        let ttl = Duration::from_secs(60);
        let stamp = Instant::now() + Duration::from_secs(10);
        assert_eq!(fresh(Some((1u8, stamp)), Instant::now(), ttl), Some(1));
    }

    // ----- metric names (greppable contract with dashboards) -----

    #[test]
    fn metric_names_are_namespaced_and_distinct() {
        assert_eq!(WS_RATE_REJECTIONS_TOTAL, "aero_ws_rate_rejections_total");
        assert_eq!(WS_RATE_FAIL_OPEN_TOTAL, "aero_ws_rate_fail_open_total");
        assert_ne!(WS_RATE_REJECTIONS_TOTAL, WS_RATE_FAIL_OPEN_TOTAL);
    }
}
