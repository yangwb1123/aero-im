# DECISIONS — Architecture Decision Records

> Append-only. One ADR per significant decision. Status: PROPOSED · ACCEPTED ·
> PENDING (needs human) · SUPERSEDED.

## ADR-001 — GDPR right-to-erasure must null the message embedding

- **Date:** 2026-06-13
- **Status:** ACCEPTED (implemented, commit 984bffc)
- **Decision:** `ParticipantRepo::delete_participant` adds `embedding = NULL` to the
  message-anonymisation UPDATE, alongside the existing `blocks` / `searchable_text`
  clearing.
- **Reason:** A pgvector embedding is a semantic fingerprint of the original text;
  a nearest-neighbour search over a retained embedding reconstructs what was
  supposedly erased, so clearing only the text left erasure incomplete (GDPR Art. 17
  re-identification). All sibling content-clearing paths already null it
  (`message.rs:101/154/274`, `workspace.rs:826`); the erasure path was the outlier.
- **Impact:** Erased messages lose semantic searchability (intended). Added a
  PG-gated regression test (`participant::db_tests::erasure_nulls_message_embedding`).
- **Alternatives:** Leave embedding (rejected — re-identifiable); delete the row
  entirely (rejected — breaks thread/FK integrity, hence the placeholder approach).

## ADR-002 — Reconcile the two tier systems blocking migration 0109 (RESOLVED)

- **Date:** 2026-06-13
- **Status:** ACCEPTED — resolved by option D (drop the dead column), commit 40f8cab.
  Investigation showed the added column was never referenced by any code or later
  migration (the subscription-tiers feature CRUDs only the `subscription_tiers`
  table), so options A/B were moot — the column was simply removed. Full chain now
  replays clean on a fresh DB (125/125). If a creator_subscriptions→subscription_tiers
  link is ever needed, add it in a NEW migration as `subscription_tier_id`.
- **Context:** `0051_creator_subscriptions.sql` created `creator_tiers` +
  `creator_subscriptions.tier_id uuid NOT NULL`. Later, `0109_subscription_tiers.sql`
  introduced a *second* tier table `subscription_tiers` and tried
  `ALTER TABLE creator_subscriptions ADD COLUMN tier_id UUID REFERENCES
  subscription_tiers(id)` — colliding with the existing `tier_id`. A fresh DB fails
  at 0109. The dev DB (frozen at migration 32) and `#[ignore]`-only db-tests hid it.
- **Options:**
  - **A. Parallel + rename (smallest, reversible):** rename 0109's new column to
    `subscription_tier_id` (FK to `subscription_tiers`, nullable). Both tier systems
    coexist. Lowest risk; leaves two overlapping concepts.
  - **B. Replace-and-migrate:** treat `subscription_tiers` as the canonical system,
    backfill from `creator_tiers`, repoint `creator_subscriptions.tier_id` (add FK,
    make nullable), deprecate `creator_tiers`. Cleanest end state; data migration risk.
  - **C. Make 0109 idempotent only:** `ADD COLUMN IF NOT EXISTS` — rejected, leaves
    `tier_id` without the intended FK/nullability (semantically wrong).
- **Recommendation:** **A** now (unblock fresh deploys with minimal risk) +
  schedule **B** as a follow-up once product confirms the canonical tier model.
  Either way: add a CI job that replays `migrations/*.sql` against a scratch DB so
  the chain is execution-validated, not just compile-embedded.

## ADR-004 — Optional client idempotency key for gift sends

- **Date:** 2026-06-13
- **Status:** ACCEPTED (implemented, commit c1abddd, migration 0126)
- **Decision:** Gift sends accept an optional client-supplied idempotency key (REST
  `Idempotency-Key` header / WS `stream_gift` frame `nonce`). A partial unique index
  on `stream_gifts (sender_id, idempotency_key) WHERE idempotency_key IS NOT NULL`
  dedups; `insert_gift` uses `ON CONFLICT DO NOTHING RETURNING` and returns the
  original gift + `inserted=false` on a hit; `send_gift` then skips broadcast + goal
  feed, and signals callers to skip the hype-train feed.
- **Reason:** A retried gift RPC double-recorded the ledger row, double-advanced
  goal bars, and re-broadcast — a money-path/leaderboard correctness bug. Client-
  supplied keys are the standard idempotency mechanism for non-idempotent POSTs.
- **Impact:** Backward-compatible — a `NULL` key (no header/nonce) keeps the legacy
  always-insert path, so existing clients and the web SPA are unaffected (they
  simply don't get dedup protection until they send a key).
- **Alternatives:** Server-derived dedup window (e.g. hash of sender+gift+qty over N
  seconds) — rejected: it would wrongly collapse two *intentional* identical gifts.
  Client keys make intent explicit.

## ADR-003 — Bus listeners resubscribe across NATS reconnects

- **Date:** 2026-06-13
- **Status:** ACCEPTED (implemented, commit e5f1fb1)
- **Decision:** `run_bus_listener` / `run_live_bus_listener` wrap their
  subscribe + consume loop in an outer `loop` that re-subscribes (1s backoff) when
  the subscription stream ends or a subscribe fails, instead of returning.
- **Reason:** The stream ends on a NATS reconnect/drop; the old code returned
  `Ok(())`, the boot-time task terminated, and nothing re-spawned it — the process
  silently stopped all room/stream fan-out with no error. A resubscribe loop is the
  standard durable-consumer pattern.
- **Impact:** The room listener's durable consumer (`aero-server`) resumes from its
  committed cursor → at-least-once preserved across reconnects. The live listener is
  ephemeral by design (broadcast; a few dropped danmaku across a reconnect are
  immaterial) but its loop now survives. Backoff is non-zero so a hard-down NATS
  can't spin a tight loop.
- **Alternatives:** Crash the process on stream-end and rely on an orchestrator to
  restart (rejected — drops every other in-process listener/session); per-task
  supervisor that re-spawns (rejected — heavier, same effect as the inline loop).
- **Test gap:** no regression test — `run_bus_listener` needs a full `AppState` and
  aero-server has no test-AppState harness (see TODO.md tech debt).

## ADR-005 — audit_governance_outbox 终态行保留清扫：显式推迟（跟踪项）

- **Date:** 2026-08-08
- **Status:** PENDING (needs human — tracked follow-up; trigger criteria in
  `docs/design/2026-08-08-aero-audit-connector-b5-4-fail-closed-operational.design.md` §8 D7)
- **Decision:** 不在 B5-4 fail-closed operational slice 内实施 `audit_governance_outbox`
  终态行（status 2 delivered / status 3 dead）的保留清扫；显式推迟并本 ADR 跟踪。
- **Reason:** (1) 该 slice 是零迁移、只读采样面（AC1 钉「no state mutation」，§3 红线
  「不改 0239 触发器 / 零新迁移」）——清扫是写路径迁移 + 新 sweeper + 表语义变更，
  属后续 slice；(2) status 2/3 是终态且全仓无 sweeper GC 这张表（`bin/boot/retention.rs`
  17+ 清扫零 outbox 表；连接器仅测试 teardown DELETE；0241 只 insert），累计行数随
  部署年龄无上界增长，任何 O(累计表) 查询都会随年龄退化（sql_perf 复核系统性 flag）；
  (3) 推迟可接受：B5-4 设计已把 30s 采样主路径改为 presence 探针 + partial-index
  计数（成本 ∝ due 集合，不随累计行数），全量精确计数摊销到 env 可配慢节奏
  （`AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS` 默认 600s），唯一 O(表) 余项是
  dead=0 时的 heap 早退探针（零迁移地板，每 30s 每实例一次 bounded heap pass）。
- **Impact（触发阈值，达到即拉前实施）：** outbox heap ≥ 1M 行（≈1 GB）/ 慢节奏全量
  > 1s / dead 探针 > 500ms——按部署年龄监控 `aero_audit_outbox_status` 与采样耗时。
- **实施草图（后续 slice）：** 新迁移 partial index `(status, created_at)`（0239 现有
  两个 partial index 只覆盖 status IN (0,1)，status 2/3 无索引，DELETE 谓词无索引支撑
  即又是 O(表)）+ retention 清扫链（`bin/boot/retention.rs` 既有 set-based sweep 模式）
  批量 DELETE：delivered 超窗删除（建议 30d，投递即终态、重放无价值——审计记录本体在
  `audit_events`，已有 365d 分区清扫）；dead 保留审查窗（建议 ≥90d——dead 行是
  fail-closed 触发器与取证证据，删除必须滞后 ops 响应时间）；可用哨兵串识别
  `audit sink rejected the service identity (HTTP 403)` 行供人工复盘。
- **Alternatives:** (a) 30s 主路径直接全量 GROUP BY（否决——首个 O(累计表) 30s 查询，
  且是唯一永不清扫的表，缓存污染 live path）；(b) 维护计数器镜像 `snaplink_usage_counters`
  先例（记录为更重方案，仅当探针仍太贵时）；(c) 加 `(status)` 索引（迁移，与清扫同属
  后续 slice，届时一并做）。
