Now I have comprehensive understanding of the project. Here is my architectural analysis.

---

# Aero IM — Architecture Analysis & Strategic Roadmap

## 1. Architecture Assessment

### 1.1 Strengths

**Clean layered crate architecture.** The dependency graph is strictly bottom-up with no cycles:

```
common → {bus, storage, auth, signaling} → {im-core, im-call, ai, live-*} → server
```

Each crate maps to exactly one business capability. This makes incremental compilation fast, test boundaries clear, and prevents the "fat server crate" anti-pattern that plagues monorepo Rust projects.

**Event mesh with local fan-out.** The NATS JetStream + `Hub` bounded-mpsc architecture is the right level of abstraction for a multi-tenant IM platform. It provides:
- Cross-instance delivery without point-to-point wiring
- Process-local back-pressure (bounded channels with drop/disconnect)
- O(1) connection unregister via reverse index
- At-least-once semantics via durable consumers

**Repository pattern hides SQL.** 157 migrations and 80+ `*Repo` modules are cleanly separated from business logic. The `PiiDetector`, `SpamGuard`, `Moderator` traits in `aero-im-core` show good use of the strategy pattern.

**WebSocket frame protocol is well-designed.** The `ClientFrame`/`ServerFrame` tagged enum with `serde(tag = "type")` avoids the stringly-typed dispatch common in real-time systems. Back-pressure with `disconnect_on_full` and `RESYNC_FRAME` is production-grade thinking.

**AI service with graceful degradation.** `AiBackend` trait with fallback (`HashEmbedder`, heuristic summarization) means no LLM key ≠ broken system. The budget system (`KeyedCostBudget` + `CostBudget` with weighted allocation across kinds) is appropriate for shared-resource AI.

### 1.2 Key Architecture Limitations

**Hub is a single-process bottleneck disguised as a distributed system.** Despite NATS for cross-instance delivery, the `Hub` remains fundamentally a process-local DashMap-based registry. The architecture document (§2 of AGENTS.md) correctly states that "Hub fans-out within local process only", but this means:

- Every WebSocket connection must terminate on the process running the Hub
- You cannot have a pure "edge proxy" without either (a) rewriting the Hub as a distributed hash table, or (b) routing all WS messages through NATS (defeating the purpose of edge termination)
- Multi-region deployment requires either sticky routing or a Hub refactor

**DashMap as global shared state.** The Hub uses `DashMap`s for every hot-path data structure: `conns`, `rooms`, `stream_watchers`, `call_rosters`, `subs`. While DashMap is fine for moderate concurrency, it:
- Has no atomic multi-key operations (room join requires 3 separate DashMap writes)
- Has no built-in eviction or TTL (stale entries live forever until disconnect)
- Makes it impossible to snapshot state atomically (heartbeat needs a read-modify-write on `rooms`)

**Durable consumer per process creates NATS head-of-line blocking.** Each server instance runs a durable consumer `aero-server` on `im.room.*`. This means:
- A single slow message handler blocks the entire consumer stream
- The `while let Some(sub) = stream.next().await` loop in `run_bus_listener` has no concurrency
- A large room's member expansion (1000+ members) delays event delivery to other rooms

**157 migrations is a deployment risk.** Every binary embeds all migrations. While this guarantees version consistency, it means:
- Rollback requires writing a down-migration for every up-migration (none exist today)
- A failed migration at index 157 requires manual `_sqlx_migrations` intervention
- There is no mechanism for data migrations across major versions (zero-downtime deploy impossible)

**No vertical slice testing strategy.** The codebase has:
- `db_tests` (integration tests requiring Postgres, marked `#[ignore]`)
- Unit tests for individual modules
- No documented end-to-end testing framework for the WS protocol
- `scripts/` has `truth-check.sh`, `file-size-check.sh`, `web-check.sh` but no `smoke-test.sh` for the full system

**Room access control is a single function.** `assert_room_access(participant, room)` in `ImService` concatenates room→workspace resolution, membership check, deactivation gate, and 2FA enforcement. This is good for auditability but creates a single function that:
- Is called from 80+ route handlers
- Cannot be partially overridden for bots or webhooks (they use a separate path)
- Has no caching at the route level (every REST call goes through the full chain)

### 1.3 Technical Debt Signals

**`aero-im-core/src/service/orig.rs` is a known debt item.** The AGENTS.md mentions it was split from a monolithic `service.rs` but "not yet extracted" sub-modules remain in `orig`. This is the largest technical debt item—a catch-all module that violates the single-responsibility pattern the rest of the codebase follows.

**Zero down-migrations.** After 157 schema changes, rolling back a deployment requires manual SQL. For a production system targeting enterprise adoption, this is a deployment risk.

**Model types serve double duty.** The comment in `crates/aero-common/src/model/message.rs` says "These types are wire-format AND storage-format." This means:
- A schema change requires coordinated deployment of API clients and server (breaking change for `web/` SPA if a field is renamed)
- Internal fields (like `deleted_at`) are serialized over the wire even when `skip_serializing_if = "Option::is_none"` is set
- The `metadata` field is `serde_json::Value`—no schema enforcement

**Rate limiter has a split personality.** The system has both:
- WS rate limiting (`check_ws_rate_room` with per-client bucket)
- HTTP rate limiting (workspace-level with Redis)
- AI budget with per-ws + global tiers
- Spam guard with configurable thresholds
These are four separate mechanisms with different config formats and no unified policy interface.

**WebSocket frame dispatch uses `match` on a single enum.** `handle_text` in `frame.rs` is a growing `match ClientFrame` that handles ~20 variants. This pattern doesn't scale well—new frame types require modifying a single file, and unrelated features' frame handling is coupled.

---

## 2. High-Value Architectural Expansion Directions

### Direction 1: Distributed Hub — Decouple Connection Termination from Business Logic

**Why it's needed.** Currently every WebSocket connection must terminate on the Hub process. This prevents:
- Multi-region deployment (users connect to nearest PoP, not origin)
- Horizontal scaling of Hub independently of WS connections
- Zero-downtime deploys (draining connections requires Coordinated shutdown)

**Core challenge.** The Hub is currently a DashMap of `(ParticipantId → Vec<WsSender>)`. Making it distributed means either:
- Option A: Replace DashMap with Redis-backed connection registry (NATS subscriber per region subscribes to connection events)
- Option B: Introduce a lightweight "WS proxy" layer that forwards to a Hub pool via consistent hashing
- Option C: Adopt the Discord model—each guild (workspace) is assigned to a shard, and the shard's hub owns all connections for that workspace

**Recommended approach: Option B (WS proxy + Hub pool).**
- New crate `aero-edge`: lightweight, stateless WS proxy that terminates the WS connection, authenticates via JWT, and forwards frames to a Hub via an internal NATS subject (`ws.proxy.{region}.{hub_id}`)
- Hub pool is sized independently (e.g. 4 Hubs per region)
- Hub identifies itself with a `hub_id` and subscribes to `ws.proxy.us-east.>`
- Edge proxy selects hub via `hash(participant_id) % n_hubs` for consistent routing
- Connection state (rooms joined, stream watchers) remains in-process on the Hub, not in Redis

**Architecture changes:**
- New crate: `aero-edge` (depends on `aero-common`, `aero-auth`)
- `Hub` gains a `region` and `hub_id` field
- NATS gets new subject namespace: `ws.proxy.{region}.{hub_id}.>`
- Bus listener for `ws.proxy.*` in the Hub (alongside existing `im.room.*` and `live.stream.*`)
- `Hub::register` becomes region-aware: it only registers connections that the edge proxy assigned to it
- Connection disconnect → Hub publishes `hub.connection.dropped.{region}.{hub_id}` so edge proxies can re-route

**Impact on existing system:**
- Existing `ws::handler` becomes a thin wrapper that authenticates and then delegates to `aero-edge` for the WS handshake
- `Hub::register` and `Hub::unregister` signatures unchanged—they already accept `(ParticipantId, WsSender)`
- Bus listeners (`run_bus_listener`, `run_live_bus_listener`) remain identical—they still fan out through the local Hub
- Edge proxy has zero business logic: it only routes frames and heartbeats

**Risk:** Increased NATS traffic for each WS frame. Each user message now goes: `User → Edge → NATS → Hub (process) → NATS → other Hubs → Edge → Recipient`. Mitigation: batch small frames, use request-reply pattern for common operations (typing, presence).

### Direction 2: Unified Policy Engine — Replace Ad-hoc Rate Limiting with a Declarative Policy Framework

**Why it's needed.** The system currently has four independent rate-limiting/abuse-prevention mechanisms:
- WS rate limiter (token bucket per client)
- HTTP rate limiter (workspace-level, Redis-backed)
- AI budget (per-ws + global, weights by kind)
- Spam guard (configurable thresholds)

Each has its own config format, its own storage backend, and its own error reporting. Adding a new policy (e.g., "limit custom emoji uploads to 5 per hour per workspace") requires wiring a new mechanism from scratch.

**Core challenge.** A unified policy engine must support:
- Multiple rate dimensions: per-participant, per-room, per-workspace, global
- Multiple time windows: sliding window, fixed window, leaky bucket
- Multiple actions: reject, throttle, log-only, notify-admin
- Dynamic configuration: some policies should be changeable at runtime without restart

**Recommended approach: Policy Registry + Evaluator.**
- Define a `Policy` trait: `fn evaluate(&self, ctx: &PolicyCtx) -> PolicyVerdict`
- `PolicyCtx` includes: `participant_id`, `room_id`, `workspace_id`, `action` (enum: `SendMessage`, `UploadBlob`, `CreateEmoji`, `RunSearch`, etc.), `timestamp`
- `PolicyVerdict`: `Allow | Deny { reason, retry_after } | Throttle { delay_ms } | LogOnly`
- Registry holds a `Vec<Box<dyn Policy>>` evaluated in order (fail-fast on deny)
- Policies can be: `RateLimitPolicy` (configurable dimension + window), `SpamPolicy` (keyword/ML-based), `BudgetPolicy` (AI token budget), `SlowModePolicy` (per-room cooldown)
- Config stored in Postgres `policy_configs` table, cached with TTL in Redis

**Architecture changes:**
- New crate: `aero-policy` (depends on `aero-common`, `aero-storage`)
- New migration: `policy_configs` table (id, name, kind (jsonb), enabled, priority, updated_at)
- `PolicyRegistry` struct with `async fn check(action, ctx) -> PolicyVerdict`
- `RateLimitPolicy` backed by Redis (sorted set for sliding window)
- Route/WS handlers call `registry.check()` instead of ad-hoc helpers
- Admin API: `GET/POST /api/admin/policies` to list/create/update policies

**Impact on existing system:**
- Replace direct calls to `rate_limit::check()` in WS handlers with `policy_registry.check(...)`
- `SpamGuard` implements `Policy` trait
- `AiBudget` implements `Policy` trait
- Background sweeps (`rate_limiter/spam/slow-mode idle sweep`) consolidated into a single sweep
- Existing `AERO_RATE_LIMIT_PER_SEC` env vars become initial defaults for the corresponding policy row

**Risk:** Performance overhead of evaluating N policies per request. Mitigation: short-circuit on first deny, cache policy list in process memory with watch channel for updates, batch Redis operations.

### Direction 3: Event Sourcing for Room State — From Soft-Delete to True Event Log

**Why it's needed.** The current architecture uses soft-delete (`deleted_at` columns) for message deletion. This works for basic compliance but breaks down for:
- **Collaborative editing:** tracking who edited what and when (currently only `message_history` captures pre-edit snapshots)
- **Conflict resolution:** two concurrent edits can race
- **Audit trails:** compliance officers need a complete, immutable log of room activity
- **Disaster recovery:** restoring a room to a point-in-time requires reconstructing from partial snapshots
- **Replay for new features:** a "read this room from the beginning" or "AI catchup on what changed since yesterday" requires scanning the entire message table

**Core challenge.** Converting an existing CRUD system to event sourcing is invasive. The `RoomEvent` enum already looks like an event store, but:
- Messages are stored as rows in `messages` table, not as events in an event log
- `Edited` and `Deleted` events modify the message row in place
- There is no global ordering of events across rooms (per-room seq only)
- Event replay for new nodes would require scanning all room events from NATS (which has 7-day retention)

**Recommended approach: Dual-write Event Log (write to both message table and event store, then migrate).**
- Phase 1: Add an `event_log` table (`room_id, seq, event_type, payload(jsonb), created_at, trace_id`)
- Phase 2: Every `RoomEvent` publication also writes to `event_log` (dual-write, best-effort)
- Phase 3: Backfill historical messages as events (sequential batch job)
- Phase 4: Deprecate `deleted_at` soft-delete; deletion becomes an event that the query layer interprets
- Phase 5: For new rooms, make the event log the source of truth; `messages` table becomes a materialized view

**Architecture changes:**
- New migration: `event_log` table (composite PK: `room_id, seq`)
- `EventLogRepo` in `aero-storage`: `append`, `read_since(room_id, seq, limit)`, `read_range(room_id, start_seq, end_seq)`
- `ImService::publish_room_event` dual-writes to NATS + event log (async, non-blocking)
- New route: `GET /api/rooms/:id/events?since={seq}&limit={n}` for catch-up
- `AiWorker::catchup` reads from event log instead of scanning messages table
- Room state cache: `RoomState { members, topic, pinned_messages }` snapshot materialized from events

**Impact on existing system:**
- `messages` table remains for backward-compatible queries (room history, search)
- All existing REST endpoints unchanged (they read from `messages` table)
- WS clients that reconnect use `since` parameter (seq) to catch up via event log
- `deleted_at` still works but is now a query-layer concern, not storage-layer
- NATS durable consumer remains the primary delivery mechanism (event log is for replay, not delivery)

**Risk:** Write amplification (every message write goes to 2 stores). Mitigation: async dual-write with error tolerance; if event log write fails, log and continue (NATS message still delivers). The event log can be backfilled offline.

### Direction 4: Multi-Level Cache Hierarchy — From Single-Level Cache to TTL-Coherent Multi-Tier

**Why it's needed.** The current caching architecture is ad-hoc:
- `participant_cache::get_or_fetch`: process-level TTL cache for participant profiles
- `room_member_cache::get_or_fetch`: process-level TTL cache for room members
- Redis presence: sorted-set with TTL expiry for online status
- Everything else: direct Postgres query

This works for moderate scale but breaks at 10K+ concurrent users because:
- Each REST handler for participant profiles hits the cache line (hot cache, fine), but cache invalidation is manual (must call `.invalidate()` from every write path)
- Room member list is cached per-process with no cross-process invalidation (stale until TTL)
- No cache for frequently-queried aggregates (room message count, unread count, reaction tallies)
- Query patterns like "list rooms with unread counts" require N+1 queries

**Core challenge.** Caching in a real-time collaborative app is harder than CRUD because:
- Stale data is immediately visible ("I see you're offline but you just typed")
- Many reads are for personalized aggregates (unread count per room per user)
- Cache invalidation must be near-instant for presence/typing but can be 5-30s for other data

**Recommended approach: Three-tier cache with explicit coherence.**
- **L1 (Process memory):** `dashmap + tokio::sync::watch` for room metadata, participant profiles, member lists. TTL: 30s, updated via watch channel on relevant `RoomEvent`.
- **L2 (Redis):** Presence, unread counters, room aggregate stats (message count, reaction count). TTL: 5-60s depending on staleness tolerance. Redis pub/sub for invalidation.
- **L3 (Postgres):** Source of truth for everything.

**Architecture changes:**
- New crate: `aero-cache` (depends on `aero-common`, `aero-storage`)
- `CacheBackend` trait: `get(key)`, `set(key, value, ttl)`, `invalidate(key)`, `invalidate_prefix(prefix)`
- Implementations: `ProcessCache` (DashMap + watch), `RedisCache` (fred), `NullCache` (for testing)
- `CacheRegistry`: holds named cache instances, each with a backend + TTL configuration
- Invalidation bus: a dedicated NATS subject `cache.invalidate.{region}` for cross-process invalidation
- Each `*Repo` method that writes also publishes invalidation events

**Impact on existing system:**
- `participant_cache` and `room_member_cache` modules become thin wrappers over `CacheRegistry`
- `Hub::fan_out_raw` for `RoomEvent::Message` also publishes `cache.invalidate.{room_id}` with updated aggregate
- Existing Repo methods unchanged (they still write to Postgres first)
- Route handlers that read aggregates (unread count, room list with preview) use cache-first
- The `presence` Redis backend stays as an L2 cache with its own TTL

**Risk:** Cache invalidation storms (a single `@everyone` message triggers invalidations for all room members). Mitigation: batch invalidation keys per room (one invalidation message per room, not per member); use set-based operations in Redis.

### Direction 5: Declarative WebSocket Router — From Monolithic Dispatch to Pluggable Handler Registry

**Why it's needed.** The `handle_text` function in `ws/ws_impl/frame.rs` is a growing `match ClientFrame` that handles ~20 frame types. As more features are added (call signaling, live interactivity, polls, reactions), this file will grow uncontrollably. Currently:
- Frame dispatch is a single file
- Adding a new frame type requires modifying this file
- No middleware chain for cross-cutting concerns (rate limiting, audit logging, tracing)
- No way for extensions (bots, plugins) to register custom frame handlers

**Core challenge.** Refactoring the WS dispatch is tricky because:
- `handle_text` holds a mutable reference to `state` and `tx` (the WS sender)
- Some frames need the full `AppState`, others only need a subset
- Error handling is mixed with business logic
- The `send_blocks_frame` shared dispatch is woven into the match arms

**Recommended approach: Actor-like Handler Registry with middleware pipeline.**
- Define a `WsHandler` trait: `async fn handle(&self, ctx: WsCtx, frame: ClientFrame) -> Result<()>`
- `WsCtx` provides: `participant_id`, `state: &AppState`, `tx: &WsSender`, `hub: &Hub`
- A `WsRouter` struct that maps `ClientFrame` variant tags to handlers
- Middleware chain runs before the handler: rate limit, audit log, tracing span
- Handlers are registered at boot time: `ws_router.register(ClientFrame::SendMessage, messages_handler)`
- Cross-cutting concerns (rate limiting, audit) are middleware, not per-handler logic

**Architecture changes:**
- `frame.rs` gains `WsRouter` and `WsHandler` trait
- Existing `handle_text` becomes a dispatcher that iterates the middleware chain then calls the registered handler
- Each feature area (messages, reactions, typing, calls, live chat) provides its own `impl WsHandler`
- Middleware: `RateLimitMw`, `AuditLogMw`, `TracingSpanMw`, `MeteringMw`
- The bot dispatch (`bot_dispatch.rs`) also implements `WsHandler` for delegated bot responses

**Impact on existing system:**
- `handle_text` shrinks to ~20 lines (lookup handler, run middleware, execute)
- Each handler is in its own file, collocated with its feature
- `send_blocks_frame` stays as a shared utility, called by the message handler
- New frame types don't touch `handle_text` or `frame.rs` (just register a new handler)
- Bot event subscription dispatch (which currently reads from bus) can optionally register WS handlers too

**Risk:** Performance overhead of dynamic dispatch through trait objects. Mitigation: use enum dispatch internally (handlers are still matched in `WsRouter`, but the match is generated at registration time via a `match!` macro or a precomputed hashmap).

---

## 3. Interface Design Recommendations

### 3.1 Principles

**Explicit over implicit.** The codebase currently conflates wire-format and storage-format in model types. Introduce separate DTO types for:
- `WireMessage` (what goes over WebSocket/REST—stripped of internal fields like `deleted_at`, `searchable_text`)
- `StorageMessage` (what goes into Postgres—full row)
- `BusMessage` (what goes on NATS—includes `seq`, `traceparent`, optional `explicit_recipients`)

This adds verbosity but prevents accidental field leaks and decouples API versioning from schema versioning.

**Repository as boundary, not leak.** The `*Repo` pattern in `aero-storage` is clean. Enforce that no crate outside `aero-storage` imports sqlx directly. This is already largely true—formalize it with a lint.

**Event-first, query-second.** New features should publish events as the primary action, with queries as secondary materialization. The `ImService::publish_room_event` pattern is correct—extend it to all mutating operations.

### 3.2 New Abstractions Needed

**`WsCtx` context object.** Currently each WS handler extracts `State`, `tx`, `pid` from the closure. A `WsCtx` struct would:
- Provide typed access to frequently-used repositories (`state.pg`, `state.presence`)
- Carry the `CancellationToken` for the connection
- Expose helper methods: `send_frame(ServerFrame)`, `rate_limit_check(action)`, `close(reason)`

**`PolicyEngine` trait.** As described in Direction 2, this would consolidate the four independent rate-limiting mechanisms. Important: the policy engine should evaluate policies, not enforce them—enforcement is the caller's responsibility after receiving a `PolicyVerdict`.

**`CacheRegistry` with typed handles.** Instead of `get_or_fetch::<Participant>()` with string keys, use typed cache handles:
```rust
let cache: CacheHandle<ParticipantId, ParticipantProfile> = registry.handle("participant");
cache.get(pid, || repo.get_profile(pid)).await
```

**`EventStore` trait.** As described in Direction 3, this abstracts the event log behind `append()`, `read_since()`, `read_range()`. Implementations: `NatsEventStore` (backed by JetStream KV), `PgEventStore` (backed by `event_log` table), `NullEventStore` (for testing).

### 3.3 Backward Compatibility

For all abstraction changes, follow this migration pattern:

1. **Add new trait/interface alongside existing code** (both paths work)
2. **Instrument both paths for observability** (compare error rates, latency)
3. **Feature-flag the new path** (env var `AERO_USE_NEW_POLICY_ENGINE=true`)
4. **Shadow-run in production** (run both, compare results without affecting user traffic)
5. **Flip the flag to default-on** after confidence threshold (e.g., 7 days no divergence)
6. **Remove the old path** after one release cycle

This is already partially practiced (the `Hub::with_ws_config` pattern for test injection). Formalize it for all architectural changes.

---

## 4. Technology Selection Guidance

### 4.1 New Dependencies Evaluation Criteria

For any proposed new dependency, evaluate against:

| Criterion | Weight | Notes |
|-----------|--------|-------|
| Rust ecosystem maturity | High | Prefer crates with 10K+ downloads, active in last 6 months |
| Unsafe code | Forbidden | Workspace lint `unsafe_code = "forbid"`—this rules out many C-to-Rust bindings |
| Tokio compatibility | Required | The entire system is tokio-based; blocking IO in async context is unacceptable |
| License compatibility | Required | MIT/Apache 2.0 preferred (current workspace license) |
| ASRF (API surface ratio) | Medium | Favor crates that expose <5 traits/structs we actually need over kitchen-sink abstractions |

### 4.2 Specific Technology Assessments

**`str0m` (already used in `aero-live-whip`/`aero-live-webrtc`).** Correct choice for WebRTC. Pure Rust, no C dependencies, well-maintained. Keep in those two crates only—do not pull into root or any IM crate.

**`image` crate (needed for Direction 1 thumbnail generation).** If attachment thumbnail generation becomes a real requirement, `image` (pure Rust image processing) is the right choice. It's large (~150 transitive deps) but thumbnail generation is a one-time per-upload cost. Consider running it in a dedicated Tokio blocking pool to avoid starving async tasks.

**`reqwest` (already in use).** Correct for outbound HTTP as well as `S3BlobStore`. The async HTTP client choice is sound.

**`fred` (already in use).** Correct for Redis. Async-native, supports cluster mode, TLS. Keep.

**`sqlx` (already in use).** Correct for Postgres. Already used correctly with compile-time query checking (`sqlx::query!`).

**What NOT to add:**
- **`elasticsearch`**: Use Postgres `pgvector` + `pg_trgm` for search (already done). No need for a separate search cluster at this scale.
- **`kafka`**: NATS JetStream covers the messaging use case well. Kafka's operational overhead is not justified.
- **`tonic` for gRPC**: The system is HTTP+WS+WebRTC. NATS is the internal RPC mechanism. gRPC would add another transport layer with little benefit.
- **`tower` for middleware**: Already using `tower-http` for HTTP middleware. Keep WS middleware in-process (the `WsRouter` pattern described in Direction 5).

### 4.3 Build vs. Buy Decisions

| Concern | Decision | Rationale |
|---------|----------|-----------|
| Thumbnail generation | Build | One-time per upload, simple resize. `image` crate is sufficient. |
| Policy engine | Build | Too domain-specific for an off-the-shelf solution. The cost is implementing the `Policy` trait, not buying a product. |
| Edge WS termination | Build | Existing Hub architecture is close—add a thin proxy layer. Commercial solutions (Pusher, Ably) cost $1K+/month at scale. |
| AI content moderation | Buy (existing Anthropic) | Already done. Don't build custom classifiers. |
| Multi-region NATS | Configure | NATS Gateway mode exists—it's configuration, not code. |

---

## 5. Implementation Roadmap

### Phase 0: Foundation (Weeks 1-4) — High Confidence, Immediate Value

**Priority: P0**

| Item | Effort | Depends On | Success Criteria |
|------|--------|------------|-----------------|
| Create `event_log` table + dual-write for all RoomEvents | 1 week | None | Every message/delete/edit also writes to `event_log`; existing dashboard unchanged |
| `WsRouter` with middleware chain | 2 weeks | None | `handle_text` dispatches through router; existing frame types work identically |
| Policy engine MVP (consolidate WS + HTTP rate limiting into one `Policy::check` call) | 1 week | None | Route handlers use `registry.check()`; old `rate_limit::check` calls deprecated |
| Down-migration for all 157 existing migrations | 2 weeks | None | `aero-cli migrate down` works for any N; CI validates roundtrip |

**Risk:** Event log dual-write adds latency to message paths. Mitigation: async write with fire-and-forget; if event log write fails, log and proceed (NATS message already delivered). Catch up in background job.

### Phase 1: Scalability (Weeks 5-10) — Multi-Region Foundation

**Priority: P1**

| Item | Effort | Depends On | Success Criteria |
|------|--------|------------|-----------------|
| NATS Super Cluster configuration (gateway mode) | 1 week | Phase 0 | Two regions can publish/subscribe across boundaries |
| Edge WS proxy (`aero-edge` crate) | 3 weeks | Phase 0 (WsRouter) | WS connection terminates in edge proxy, frames routed to Hub via NATS |
| Hub pool sizing + consistent hashing | 1 week | Edge proxy | 2 Hub instances can share connections for one workspace |
| Cache registry with L1 (process) + L2 (Redis) | 2 weeks | Phase 0 | Read path for participant profile goes L1 → L2 → DB; invalidations propagate cross-process |
| Region-aware presence | 1 week | NATS Super Cluster | User in `us-east` publishes presence; `eu-west` sees it within 5s |

**Risk:** Edge proxy NATS routing adds ~5-10ms per WS frame. Mitigation: batch small frames (typing, presence); use direct Redis reads for hot data (presence, room metadata) instead of routing through NATS.

### Phase 2: Capability (Weeks 11-16) — Event Sourcing + Advanced Caching

**Priority: P1 (Event Sourcing) / P2 (Caching)**

| Item | Effort | Depends On | Success Criteria |
|------|--------|------------|-----------------|
| Backfill historical messages into event log | 2 weeks | Phase 0 (event_log) | All rooms have full event history in event_log |
| Event log as source of truth for new rooms | 2 weeks | Backfill | New rooms' messages read from event log; `messages` table is materialized view |
| Aggregate caching (unread count, reaction tally, message count) | 2 weeks | Phase 1 (cache registry) | "Rooms with unread count" loads from cache; miss hits DB |
| Event log catch-up for WS reconnect | 1 week | Event log source of truth | Client reconnects with `since={seq}` and receives missed events |
| Policy engine expansion (AI budget, spam guard as policies) | 2 weeks | Phase 0 (policy engine MVP) | `AiBudget` and `SpamGuard` implement `Policy` trait; admin API for policy config |

**Risk:** Event sourcing migration is the highest-risk item in this roadmap. Mitigation: keep dual-write active for 2 release cycles; monitor divergence between `messages` table and event log; rollback plan is to disable event log reads and fall back to `messages` table.

### Phase 3: Optimization (Weeks 17-20) — Proactive Performance

**Priority: P2**

| Item | Effort | Depends On | Success Criteria |
|------|--------|------------|-----------------|
| Hub fan-out concurrency (parallel processing per room) | 1 week | Phase 0 (WsRouter) | Large room's event doesn't block other rooms' delivery |
| Predictive preloading (frequency-based room pre-connect) | 2 weeks | Phase 2 (aggregate cache) | Top 5 rooms per user have pre-established WS connection; switch latency < 50ms |
| Adaptive notification scheduling (rule-based, not ML) | 2 weeks | Phase 1 (cache registry) | User ignores 80%+ of `Reaction` notifications → auto-downgrade after 7 days |
| Attachment thumbnail pipeline (`image` crate + thumbnails dir in blob_store) | 1 week | Phase 0 | `POST /api/blobs` generates 256px thumbnail; `GET /api/blobs/:id?size=thumb` returns it |

### Phase 4: Intelligence (Week 21+) — AI-Driven Features

**Priority: P3**

| Item | Effort | Depends On | Success Criteria |
|------|--------|------------|-----------------|
| NL2API (parameterized operation classification, not NL2SQL) | 3 weeks | Phase 2 (event log) | `"Who's most active this week?"` → `{ operation: "top_posters", params: { time_range: "last_7d", limit: 5 } }` |
| AI notification summarization (per-channel digest) | 3 weeks | Phase 3 (notification scheduling) | 3 messages in same channel within 5 min → 1 AI-summarized notification |
| Predictive search results (frequency-based priors) | 2 weeks | Phase 2 (cache registry) | Most-frequently-searched rooms appear first in search autocomplete |

### Risk Register

| Risk | Probability | Impact | Mitigation |
|------|------------|--------|------------|
| Event sourcing migration causes data inconsistency | Medium | High | Dual-write for 2 release cycles; automated reconciliation job; rollback procedure documented |
| NATS Gateway mode introduces cross-region latency | Low | Medium | Benchmark with realistic workload before enabling; start with 2 regions in same cloud provider |
| Edge WS proxy becomes a bottleneck | Low | Medium | Edge proxy is stateless and horizontally scalable; load test at 10x projected traffic before GA |
| Policy engine adds unacceptable latency to hot path | Medium | Medium | Cache policy evaluation results for 1s window; short-circuit on first Deny |
| Migration from 157 to 0 down-migrations hits compatibility issues | Medium | High | Test rollback in staging with production-like data before running in production |

### Overall Priority Summary

```
Phase 0 (P0, Weeks 1-4):    Event log + WsRouter + Policy engine + Down-migrations
Phase 1 (P1, Weeks 5-10):   NATS multi-region + Edge proxy + Cache hierarchy
Phase 2 (P1/P2, Weeks 11-16): Event sourcing + Aggregate caching
Phase 3 (P2, Weeks 17-20):  Performance + Preloading + Thumbnails
Phase 4 (P3, Week 21+):     NL2API + AI summarization + Predictive search
```

The first two phases deliver the highest ROI: they fix the architectural limitations (single-region Hub, no policy framework, no rollback capability) that would block enterprise adoption. Phases 2-4 build on this foundation to add the intelligent features the product roadmap calls for.

---

**Summary assessment:** The Aero IM architecture is well-structured for a single-region deployment handling moderate load. The most critical architectural debt items are the monolithic WS dispatch (fixable in 2 weeks with a router pattern), the lack of down-migrations (fixable in 2 weeks), and the process-coupled Hub (fixable in 3-4 weeks with an edge proxy layer). The five directions above address these while preserving the existing investment in NATS-based event delivery, the repository pattern, and the clean crate hierarchy.
