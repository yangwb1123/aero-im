# Session Handoff — current implementation state

This handoff avoids treating migration or test totals as durable product facts.
Obtain the current migration ledger from `migrations/`. A dated verification
snapshot appears below only as delivery evidence and must not be copied into
undated capability claims.

## Completed implementation

- Authentication and registration state changes are transactional, including
  credential/session rotation and default workspace bootstrap.
- Message create/edit/delete is treated as an aggregate. The message state,
  sender idempotency key, ordered event-outbox version, and durable
  notification/AI side-effect jobs are committed together. The relay publishes
  only the earliest unpublished aggregate version, so edit/delete cannot
  overtake create.
- `POST /api/rooms/:id/messages` and WebSocket sending share access, validation,
  rate/slow-mode, TTL, attachment, idempotency, outbox, and side-effect logic.
  REST accepts `Idempotency-Key` or `client_message_id`; replay returns the
  original message with replay metadata.
- External-side-effect durable consumers use PostgreSQL
  `ConsumerEventReceiptRepo` leases keyed by `(consumer,event_id)`. Completion
  is durable, failures are released for retry, stale owners are fenced, and
  receipt retention preserves rows while a matching producer outbox item is
  pending.
- Invitation redemption is a transaction-owned aggregate with a durable
  `(invitation, participant)` idempotency key. Capacity, workspace membership,
  and the redemption record commit together; repeats never consume another slot
  or resurrect a membership removed by an administrator.
- Workspace member, SCIM, user-group, and single-channel guest mutations
  recheck current authority while holding canonical tenant/member locks. Guest
  workspace and room edges commit together, and existing members/admins/owners
  cannot be silently converted to guests.
- Room-scoped service identities are transaction-owned installations.
  `POST /api/agents` requires an explicit `room_id`; storage rechecks the
  caller's current room-management authority under the membership-governance
  lock, then commits the participant, ordinary workspace membership and room
  membership together. Direct rooms and marked group DMs reject the request
  before any write. After commit, `ImService` publishes `MemberAdded`, and the
  route invalidates the room-member cache. The stable storage anchor is
  `create_service_identity_authorized`.
- Workspace-scoped Bot-platform creation is also atomic:
  `create_authorized_with_token` commits the participant, ordinary workspace
  membership, Bot registry row and initial token hash together. Omitting
  `workspace_id` intentionally creates a personal/system Bot with no tenant
  membership. `migrations/*_bot_workspace_membership_backfill.sql` repairs
  legacy live scoped Bots as ordinary members, converts guest-shaped legacy
  edges to member, and preserves any existing higher role.
- Every active workspace must retain an effective owner at commit: the owner
  must be live, non-guest, non-deactivated and, when mandatory 2FA is enabled,
  have activated TOTP. Deferred final-state validation covers multi-row role,
  participant-kind/tombstone and TOTP changes; authorized deactivation follows
  the same canonical governance-lock order. Dormant nil-workspace bootstrap is
  the only explicit empty-workspace exception.
- Information barriers are not creation-only. Message create/edit holds the
  workspace policy fence and performs one symmetric aggregate recipient check,
  so a committed barrier or user-group membership change immediately blocks new
  communication in an existing DM, group DM, or shared room. Barrier/group
  writers and sends linearize on the workspace row; no unbounded participant
  list is materialized in application memory.
- Commit-time database fences now also cover deferred and recurring messages,
  opaque MLS relay state, keyword alerts, predictions, pins, live governance,
  stream goals and scheduled streams. They recheck actor identity, effective
  tenant/room/stream scope, lifecycle and quota at commit, with canonical lock
  ordering so raw SQL cannot bypass the critical resource boundary.
- AI action-item extraction can opt into durable creation through
  `POST /api/rooms/:id/action-items?persist=true`. A required
  `Idempotency-Key` identifies an atomic actor/room/idempotency-key-digest batch;
  concurrent retries return the original task ids, and an empty model result
  still receives a replayable receipt without storing the raw provider output.
  Participant erasure preserves the room's collaborative tasks: it detaches only
  their batch key/index metadata, then removes the erased actor's private
  receipt/digest.
- Advanced search issues a short-lived server-side impression containing the
  complete normalized request and ordered result snapshot. Click feedback
  accepts only the impression and selected result, derives query/rank from that
  proof, rechecks current access and permits only one deterministic click.
  Saved-search monitors use bounded keyset scans, a no-history-flood enablement
  baseline, a composite `(created_at,message_id)` cursor and stable notification
  ids so concurrent workers and crash retries converge.
- Live moderation, bans and raids serialize on the stream resource and recheck
  the current creator/moderator authority. Ban appeals are bound to a concrete
  ban revision; approval of an old appeal cannot lift a replacement ban.
- Attachments support LocalFs and a real S3-compatible reqwest + SigV4 backend.
  Reservation/finalization, room-visible attachment validation, message/GC
  locking, and `blob_gc_queue.force_delete` distinguish cancellable orphan
  cleanup from mandatory GDPR/stale-reservation deletion.
- Webhook initial delivery and retry share bounded global/per-endpoint
  concurrency and persisted request material. Go-live activity insertion is
  idempotent per follower and stream.
- Browser group calls use `call_sfu_v2`. Server-owned str0m media sessions,
  multi-publisher MID isolation, generation/revision topology, explicit
  subscription routing, Simulcast and RTCP feedback are wired into the
  production WS lifecycle.
- The call-leg generation migration adds a durable, monotonically increasing
  generation to each `(call, participant)` membership.
  Reconnect increments it in PostgreSQL; generation-aware CAS in PostgreSQL and
  Redis fences stale route/roster heartbeats and leaves, while WS call events,
  SFU publisher state and exact session cleanup carry the same generation.
  The later legacy-caller reconnect compatibility migration permits only an
  old v3 initiator's conflict against its pre-existing canonical `caller` row;
  a missing row or forged initiator-as-member insert remains fail-closed.
- Cross-node-capable call media is wired into the production path: local RTP is
  tapped into `CallEgress`, peer pullers announce their UDP target through the
  secret-gated subscribe endpoint, and PLI/FIR/REMB feedback returns through
  the secret-gated feedback endpoint.
- Internal call-bridge subscribers now receive a 60-second lease. Pullers send
  an authenticated refresh every 15 seconds and best-effort unsubscribe during
  shutdown. New subscribe requests carry `lease_secs` plus a unique
  `subscription_id`; owner-side tombstones fence delayed refresh/unsubscribe,
  and bound UDP frames must match both call and generation. Current v4 pullers
  advertise `wire_version`; owners emit the requested v3/v4 bound envelope and
  current pullers decode the previous v3 envelope. A hash/build-id/embedded-
  migration-verified old v3 binary and the current v4 binary passed real
  bidirectional Chrome audio/video across two local gateways, including v4
  owner downgrade to a v3 envelope. Unbound v2 remains only
  one-way compatible (new owner to old puller), because current pullers
  deliberately reject an old v2 owner's unbound frame. Deployments containing
  v2 therefore require draining/reconnecting active cross-node calls.
- The Web SPA has connected governance, enterprise-security and compliance
  administration surfaces. They cover audit/Bot operations; sessions, 2FA,
  storage region, IP allowlist, inbound SCIM, AutoMod and IdP status; and
  retention/export, legal holds, information barriers, member lifecycle,
  invitations and webhook operations. UI role gates are only affordances; every
  mutation is re-authorized by the server against current role and resource
  ownership.

## Current worktree additions

These paths are implemented and have targeted tests, but the delivery-cursor
batch remains under the current aggregate workspace/runtime gate:

- Message creation receives a room-scoped `delivery_ordinal`, and creation
  outbox publication observes that durable order. PostgreSQL stores one
  monotonic delivery cursor per participant and room; a cursor never infers a
  contiguous prefix from an independently generated ULID or NATS sequence.
- Reconnect replay pages until the complete ordinal tail is queued, then emits
  `delivery_ready`. Live messages are held behind that barrier. The Web client
  ACKs only after synchronous handlers apply a message and isolates persisted
  cursors by account plus socket generation.
- `QueryRouter` keeps security and read-after-write paths on primary. Only a
  backward history cursor proven by primary to be genuinely old may use the
  replica; message context defaults to strong and requires explicit
  `consistency=eventual`. Replica boot/query failures fall back to primary.
- Room presence and stream viewers use 256 Redis sorted-set shards with
  concurrent aggregate reads and per-shard expiry cleanup.
- The `messages_partitioned` shadow, resumable backfill and complete cutover
  runbook are ready. Production cutover is deliberately not automatic: it
  requires an approved maintenance window, write gate, DBA execution and
  rollback plan.
- Workspace storage region selection is persisted and each room-scoped blob
  snapshots its immutable workspace/region placement. Legacy `/api/blobs`
  uploads remain personally readable for migration/export compatibility but
  are not eligible for attachment to new messages.
- S3 uploads support SigV4-signed SSE-KMS headers. Audit storage is append-only
  through its repository, active legal holds block both row and partition
  retention, and CSV exports may carry an HMAC-SHA256 tamper-evident signature.
- Canvas uses a gap-free immutable operation log, `(canvas,author,client_op_id)`
  retry idempotency and a `snapshot_op_seq` baseline. Snapshot compaction and
  concurrent op append share the canvas-row lock; the Web reducer replays only
  the ordered tail after that baseline.

## Operator configuration added or clarified

See `.env.example` for the canonical sample values:

- Message relays and retention:
  `AERO__SERVER__EVENT_OUTBOX_*`,
  `AERO__SERVER__MESSAGE_SIDE_EFFECT_*`,
  `AERO__SERVER__CONSUMER_RECEIPT_RETENTION_DAYS`,
  `AERO__SERVER__PASSWORD_RESET_RETENTION_DAYS`.
- Durable bot delivery retention:
  `AERO_BOT_DELIVERY_RETENTION_DAYS`,
  `AERO_BOT_DELIVERY_DLQ_RETENTION_DAYS`,
  `AERO_BOT_DELIVERY_SWEEP_SECS`.
- Optional notification aggregation:
  `AERO_NOTIFICATION_BUNDLES`,
  `AERO_NOTIFICATION_BUNDLE_FLUSH_SECS`,
  `AERO__SERVER__NOTIFICATION_BUNDLE_RETENTION_DAYS`.
- Webhook limits:
  `AERO_WEBHOOK_GLOBAL_CONCURRENCY`,
  `AERO_WEBHOOK_ENDPOINT_CONCURRENCY`.
- Trusted proxy parsing:
  `AERO_TRUSTED_PROXY_CIDRS` (empty by default; forwarded headers are otherwise ignored).
- SFU/bridge networking:
  `AERO_SFU_BIND_ADDR`,
  `AERO_SFU_ADVERTISE_HOST`,
  `AERO_BRIDGE_ADVERTISE_HOST`,
  `AERO_INTERNAL_BRIDGE_SECRET`.
- S3 selection is explicit: set `AERO_BLOB_BACKEND=s3` plus complete
  `AERO_S3_*` configuration. Production boot fails loudly instead of silently
  falling back to node-local storage when S3 was requested but is incomplete.
  `AERO_S3_KMS_KEY_ID` enables SSE-KMS for the default S3 backend; configured
  `[storage_regions.<code>]` entries may carry their own `kms_key_id`.
- `AERO_AUDIT_SIGNING_KEY` enables the `x-audit-signature` HMAC on audit CSV
  exports.

## Verification sequence

After any migration change, build before migrating because migrations are
compiled into `aero-cli`.

```bash
RUSTUP_TOOLCHAIN=1.80.0 cargo build --workspace --locked

# Use a newly created throwaway PostgreSQL database, never the shared dev DB.
AERO__DATABASE__URL=postgres://... target/debug/aero-cli migrate

RUSTUP_TOOLCHAIN=1.80.0 cargo check --workspace --all-targets --locked
RUSTUP_TOOLCHAIN=1.80.0 cargo test --workspace --lib --locked
RUSTUP_TOOLCHAIN=1.80.0 cargo clippy --workspace --all-targets --locked
RUSTUP_TOOLCHAIN=1.80.0 cargo test -p aero-server --test authz_lint --locked
scripts/truth-check.sh
scripts/file-size-check.sh
scripts/web-check.sh
(cd web && npm test && npm run lint)
```

Run ignored PG tests with `DATABASE_URL` pointing at the fully migrated
throwaway database. The migration CLI intentionally uses
`AERO__DATABASE__URL`; set both variables when a script performs migration and
tests in one process. Then perform the REST/WS runtime smoke against that same
isolated database. Drop it only after the process has stopped.

### Verification snapshot — 2026-07-29 (before call-leg migration 0229)

- The disposable-database integration run passed the rolling-upgrade and
  targeted migration-regression lanes, then the fresh latest schema. Ignored
  PostgreSQL suites passed for `aero-im-core` (25), `aero-server` (14), and
  `aero-storage` (534).
- Exact Rust 1.80 workspace build/check/lib-test/clippy, authz lint, project
  rustfmt check, truth/file-size/web checks, Web unit tests and Web lint passed.
- One disposable runtime backed by PostgreSQL, Redis, NATS and LocalFs passed
  the core/P2, delivery-cursor, Canvas, enterprise, thread, live/caption,
  Bot-SDK and security/authz smoke categories. Representative scripts include
  `smoke.sh`, `smoke_p2.py`, `smoke_delivery_cursor.py`,
  `smoke_canvas_ops.py`, `smoke_enterprise.py`, `smoke_bot_sdk.py`,
  `smoke_live.py`, and `smoke_captions.py`. The server was stopped before the
  disposable database was dropped.
- Environment-backed media acceptance passed with two real Chrome clients on
  one SFU gateway and then across two independent local gateways (`:18082` and
  `:18083`) using different NATS durables while sharing PostgreSQL, Redis and
  NATS. The latter exercised bidirectional RTP through `call-bridge`. Local
  two-instance acceptance has passed and remains under a repeated regression
  gate for late-subscriber bidirectional media; it is not cross-host, NAT or
  TURN acceptance.
- Real ffmpeg RTMP, WHIP and SRT ingest passed. Local integration acceptance
  also passed for MinIO S3, Mailpit SMTP, mock OIDC with RS256/JWKS, Jaeger OTLP
  traces, ClamAV and metrics through an OTel collector.

This snapshot closes only the named local or single-machine acceptance cases.
It predates the call-leg generation and legacy-reconnect compatibility
migrations; the current-build delta below supersedes those media caveats.
Local and mock integration results are not production-provider acceptance.

### Verification delta — current generation-fenced media build

- The current workspace was built before applying
  `0229_call_leg_generation.sql` to a disposable database. PostgreSQL and Redis
  generation-CAS integration tests passed against those local dependencies.
- Two real Chrome clients repeatedly passed late-subscriber bidirectional
  audio/video RTP through `call-bridge` across the two independent local
  gateways. Both gateways logged generation-bound bridge frames, owner egress
  and pull-side receipt; browser packet and byte counters continued advancing.
- A same-participant reconnect moved the logical leg from the old gateway to
  the replacement gateway with a strictly newer durable leg generation. The
  old WebSocket was then closed without a leave frame; replacement audio/video
  continued advancing during and after the old node's delayed disconnect
  cleanup.
- A verified old v3 dependency binary and the current v4 binary passed real
  bidirectional Chrome audio/video. Runtime logs prove v3-owner to v4-puller
  generation-bound RTP and v4-owner downgrade to the v3 envelope. The legacy
  caller reconnect fix preserved the canonical caller role while the same raw
  insert without a pre-existing caller row remained rejected.
- The initial SFU offer now reserves seven receive slots per media kind,
  covering the server's default eight-participant group without a Firefox
  post-DTLS bundled-m-line expansion. Real Firefox passed bidirectional SFU
  audio/video with live tracks and increasing inbound/outbound counters using
  one offer/answer. Chrome also passed forced relay-only ICE through a
  credentialed local coturn instance, including the two-gateway and reconnect
  cases. This is local/LAN TURN evidence, not public NAT or cross-host
  acceptance.
- Real Chrome WHEP completed ICE/DTLS/SRTP and decoded continuously increasing
  H.264 frames; session DELETE returned successfully. Real OBS Studio published
  RTMP through its own output stack into continuous H.264/AAC HLS.
- Encrypted libsrt/ffmpeg SRT ingest completed repeated post-handshake even/odd
  SEK rotations. The server-side rotation metric increased while H.264/AAC HLS
  continued to advance, so the result is not inferred only from sender logs.

This delta closes the named single-host browser, local TURN, ingest and
mixed-v3/v4 cases only. It does not broaden them to cross-host/public-NAT,
physical-device, Safari or production-provider acceptance.

## Remaining staging acceptance

Most remaining work is environment-backed acceptance beyond the named local
tests rather than an unimplemented server seam. SAML is the explicit exception
listed below:

- Browser/device coverage beyond the passed Chrome/Firefox virtual-media cases:
  physical microphone/camera devices, Safari, long-lived weak-network
  reconnect and real Firefox expansion beyond the default group capacity.
- Cross-host deployment: routable advertised addresses, sticky/call route
  resolution, UDP reachability, NAT/firewall rules, public TURN, and
  bidirectional RTP/RTCP.
  The passed two-gateway test used one host and shared PostgreSQL, Redis and
  NATS, and the coturn relay used a local LAN address; neither may be reported
  as cross-host/public-NAT completion.
- Real external S3/KMS, FCM/APNs, SMTP/OIDC/OTLP and other configured provider
  credential round trips. The passed local
  MinIO/Mailpit/mock-OIDC/Jaeger/ClamAV/OTel cases must not be generalized to
  production providers.
- SAML: metadata, AuthnRequest and the JIT path exist, but the default ACS is
  intentionally fail-closed until a vetted XML-DSig verifier is wired and
  security-reviewed. The opt-in `AERO_SAML_EXPERIMENTAL_VERIFY=1` path uses an
  unaudited pre-1.0 verifier and is not production acceptance.
- Production `messages` partition cutover: approved maintenance window, backup
  restore rehearsal, parity checks and execution of the checked-in runbook.

Do not report these staging items as complete until the corresponding real
environment has been exercised. MLS client cryptography, federation and native
mobile SDKs remain explicit product non-goals.
