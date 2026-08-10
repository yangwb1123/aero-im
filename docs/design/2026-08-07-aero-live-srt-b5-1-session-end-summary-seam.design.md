# Design — aero-live-srt: transactional session-end + per-session L1 summary seam (B5-1)

- **Module**: `crates/aero-live-srt` (+ `crates/aero-storage` slice + one migration)
- **Direction**: "Transactional session-end + per-session L1 summary seam at the SRT teardown path (the only B5-1 landing point in this crate)"
- **Input**: `docs/requirements/2026-08-07-aero-live-srt-b5-1-session-end-summary-seam.req.md` (23 KB, treated as **untrusted** — every load-bearing claim re-verified against the repo below)
- **Campaign anchors**: B5-1 outbox + in-tx (gate row 1, "‡ 类走 L1"); G6 "37/37"

## 0. Evidence verification（untrusted claims 逐条对仓库复核，2026-08-07）

All 12 cited claims confirmed; two supplementary constraints found that **shape the design** (C1, C2).

| Claim | Verdict |
|---|---|
| E1 `finalize_session` at lib.rs:651-666: `SessionCounter::removed()` → `session.finish()` warn-swallow → `repo.mark_ended(id)` warn-swallow; reached from error arm (lib.rs:341) + drain loop (~415) | ✅ exact lines; sole teardown path; no tx, no outbox, no identity |
| E2 `SrtSession::finish` at lib.rs:1058-1069 → `LiveResult<()>`: trailing flush if `has_segment_data()` + `hls.finish()`; `HlsWriter::finish` idempotent | ✅ exact; `finalized` flag at aero-live-hls/src/lib.rs:165-174 (second call `Ok(())`; `push_segment` after finalize fails) |
| E3 `flush_segment` at lib.rs:1044-1053: `take_segment` → `push_segment(bytes, SEGMENT_DURATION_SECS_F32)`, zero counters; `SEGMENT_DURATION_SECS=2` at **:136** (spec's "cited 134" corrected) | ✅ exact; also `SEGMENT_DURATION_SECS_F32=2.0` at :140 |
| E4 `resolve_stream` at lib.rs:1085-1124: `mark_live` → `HlsWriter::new`; HLS-init failure rolls back via best-effort `mark_ended` (:1117) | ✅ exact |
| E5 `mark_live` at storage stream.rs:179-260: lock-order resolve → `FOR UPDATE` → single CTE `WITH transitioned AS (… RETURNING id, room_id, owner_id, title)` + `INSERT INTO stream_go_live_outbox … RETURNING id, event_id` (:228/:238/:242) → commit | ✅ exact; `event_id`/`outbox_id` minted in Rust (`new_v4`), not by DB; transition guard is `status <> 'live'`; **unconditional commit** even when no row inserted (`AlreadyLive`) |
| E6 `GoLiveTransition` at stream_go_live_outbox.rs:26-28, `MarkLiveOutcome` at 33-40; `claim_due` :135 (SKIP LOCKED + claim-token fence + `attempts+1`), `claim_by_id` :173, `assign_seq_if_absent` :208, `mark_completed` :256 (requires nats + webhooks stages), `mark_failed` :286 (bounded backoff), `retry_delay` exponential cap `MAX_BACKOFF_SECONDS=300`, `MAX_CLAIM=500` | ✅ exact (file is 394 lines) |
| E7 `mark_ended` at stream.rs:264-271: plain `UPDATE streams SET status='ended', ended_at=NOW() WHERE id=$1` on pool — **the half-transactional gap**; `streams` DDL (0002:90-106) has **no duration/bytes columns** | ✅ exact |
| E8 37 test fns in tests.rs; db-gated precedent at :908 (`connect_lazy` at :921); isolation_tests.rs / rotation_tests.rs 5 each | ✅ exact counts; **tests.rs is 1137 lines / 1200 HARD — only 63 lines headroom** |
| E9 `CountingBackend` at isolation_tests.rs:206; `finalize_broken_writer` field :233; finalize swallow at :282-284 (dir deleted ⇒ `File::create` fails deterministically) | ✅ exact; the hermetic injection seam for the HLS-finish-error drill |
| E10 `MpegTsSegmenter` tracks only `packets_seen: u64` (:100/:134); no count/duration accumulator; `HlsWriter` exposes no sum | ✅ exact; `take_segment` drains even if the subsequent push fails (design-relevant, see §5 FM-5) |
| E11 migrations end at **0238** (`ls migrations/*.sql` = 238); no `0239_audit_governance_outbox.sql`; class list `message/room/admin` + L1 window [PROPOSED] | ✅ exact → stream.\* governance admission stays [PROPOSED]; this direction builds the producer-side seam only |
| E12 aero-live-srt deps = common/live-core/storage/live-hls + tokio/crypto; no audit/relay crates | ✅ exact; `StreamRepo` already injected at lib.rs:243-247 → `mark_ended_in_tx` adds **no new crate edge** |
| **C1 (new, load-bearing)** aero-storage's Cargo.toml does **not** depend on `aero-live-core` | ✅ `rg aero-live-core` in aero-storage/Cargo.toml = 0 → the summary type **cannot** live in a shared crate; `mark_ended_in_tx` takes **primitives**; `SessionSummary` stays crate-local in aero-live-srt |
| **C2 (new, load-bearing)** `SrtSession::finish` has exactly **6 call sites in the workspace**（rg 实测）：**2 处在 tests.rs 之外** —— finalize_session (lib.rs:658) + isolation_tests.rs:285（`let _ = session.finish().await`）；**4 处在 tests.rs** —— :319/:346/:348（`.unwrap()`）+ :392（`let _ =`） | ✅ 全部在新签名下**零改动**编译：`.unwrap()` 要求 `SessionSummary: Debug`（§2.1 派生保留）+ `LiveError: Debug`（aero-live-core 已成立）；`let _ =` 平凡兼容。返回类型变化 blast radius 低；`SessionBackend` trait 与 `CountingBackend` 编译不变；tests.rs 字节级不变式成立（37/37） |
| (bonus) `mark_ended` has **6+ callers** across crates (aero-live-rtmp, aero-live-srt, aero-server live.rs + whip.rs, aero-storage scheduled_stream.rs) | ✅ `mark_ended` must stay; new API is purely additive |
| (bonus) sqlx is a **dev-dependency** of aero-live-srt (Cargo.toml:45) | ✅ an `#[ignore]` db-gated test module in aero-live-srt is buildable (A2) |
| (bonus, 0239 依赖) 既有 db-gated 取消-drain 测试 `run_until_cancelled_boot_fails_open_without_audit_provisioning`（isolation_tests.rs:842-1033，`#[ignore]`）自带 `aero_storage::db::migrate(&pool)`，经**生产** `finalize_session` 路径断言 `status == Ended`（:1033） | ✅ `finalize_session` 重接后该测试要求 build 已嵌入 0239（自身 migrate 会建表）；旧 build 下 INSERT 失败被 warn 吞、status 停 live、既有测试转红 —— §6-2 点名为门 |
| (bonus) storage stream.rs db-tests seed pattern exists (participants INSERT at :363; mark_live outbox test at ~500-530) | ✅ template for the A2 storage-side test |

**Verification verdict**: the spec is accurate on every cited symbol; the only drift already noted in it (`SEGMENT_DURATION_SECS` at 136 vs cited 134). The two supplementary constraints C1/C2 are the ones that most change the shape of the implementation.

## 1. Design overview

```
publisher datagram ──► run_listener（单 UDP loop）──► SrtSession::feed_packet → feed_ts_bytes
                          └─ flush_segment → hls.push_segment(bytes, 2.0)   ← 每次 Ok 推进内存计数器
                                                                              （零 storage/locks/async）
disconnect / cancel / peer error
  └─ finalize_session（sole teardown path, lib.rs:651）
       ├─ metrics::SessionCounter::removed()                       （unchanged）
       ├─ session.finish() → LiveResult<SessionSummary>            ← R2: 终态快照，exactly-once
       │     Err → warn（containment 形状不变，isolation_tests mirror 保持绿）
       └─ repo.mark_ended_in_tx(id, cnt, bytes, millis)            ← R3: 单 CTE 语句
             └─ UPDATE streams SET status='ended' + INSERT stream_session_end_outbox
                （同语句全有或全无；稳定 event_id/outbox_id；失败 warn，row 由 claim pump 重试）
```

**交付物**（全部可独立验收）：

1. `crates/aero-live-srt/src/summary.rs`（新）—— `SessionSummary` 类型（crate-local，`pub` 导出）。
2. `crates/aero-live-srt/src/lib.rs` —— 4 个 struct 字段 + `flush_segment` 计数 + `finish()` 重写 + `summary()` 访问器 + `finalize_session` 重接 + 两行 `#[cfg(test)] mod`。**增量 ≤ ~40 行**（1142 → ≤1182，HARD 1200 红线，见 C2 约束）。
3. `crates/aero-live-srt/src/summary_tests.rs`（新，hermetic）—— A1 全部断言（tests.rs 1137/1200 只剩 63 行，**不碰** → 37/37 计数不变）。
4. `crates/aero-live-srt/src/db_tests.rs`（新，`#[ignore = "requires live Postgres"]`）—— A2 生产 `finalize_session` 全路径 drill（sqlx 已是 dev-dep）。
5. `migrations/0239_stream_session_end_outbox.sql`（新）—— 0179 镜像 + 3 个 summary 列。
6. `crates/aero-storage/src/stream_session_end_outbox.rs`（新，~400 行 clone）—— row/transition/outcome + claim 全套机器。
7. `crates/aero-storage/src/stream.rs` —— `StreamRepo::mark_ended_in_tx`（+~55 行 → 723→~778，**800 WARN 线内**）。
8. `crates/aero-storage/src/lib.rs` —— 模块注册 + re-export（无 token-helper 撞名：本模块无 `generate_token`/`hash_token`）。

**不变量（hot path）**：`feed_packet`/`pump` 继续零 storage await —— 计数器是 `SrtSession` 的普通字段（无锁无 async）；唯一新增 DB 点是每会话一次的 teardown（与既有 `mark_ended` 同频，摊薄每会话）。

## 2. API changes

### 2.1 `crates/aero-live-srt` — `src/summary.rs`（新模块）

```rust
/// Terminal per-session L1 summary（B5-1 “‡ 类走 L1” 的原子聚合单元）。
/// 只统计**已成功落盘**的 HLS segment：失败 push 贡献 0，部分 summary 永远是
/// "durably written" 的真话。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSummary {
    pub segment_count: u64,
    pub bytes_total: u64,
    /// 整数毫秒（= segment_count × SEGMENT_DURATION_SECS_F32 × 1000，精确无漂移：
    /// 2.0f32 × 1000.0 == 2000.0 恰可表示）。
    pub duration_millis: u64,
}

impl SessionSummary {
    pub const ZERO: Self = Self { segment_count: 0, bytes_total: 0, duration_millis: 0 };
}
```

无 serde（不进任何 wire/DB 类型；DB 列由 storage 侧 `i64` 承载，边界转换在 §2.3）。**为什么放独立模块而不是 lib.rs**：lib.rs 1142/1200 HARD，只剩 58 行预算（C2）。**`Debug` 派生是编译期约束（F2）**：tests.rs:319/:346/:348 的 `.unwrap()` 需要 `SessionSummary: Debug` —— 去掉派生即破坏 tests.rs 字节级不变式。

### 2.2 `crates/aero-live-srt/src/lib.rs` — `SrtSession`

新增字段（`with_crypto` 构造器初始化，lib.rs:737 区域）：

```rust
    /// 已成功 push 的 HLS segment 数 / 字节总数 / 毫秒时长（R1，仅 flush_segment Ok 后推进）。
    segment_count: u64,
    bytes_total: u64,
    duration_millis: u64,
    /// finish() 首次成功后的终态快照；重复调用返回同一快照（R2 exactly-once）。
    summary_snapshot: Option<SessionSummary>,
```

`flush_segment`（lib.rs:1044）—— 仅在 push Ok 后推进：

```rust
    async fn flush_segment(&mut self) -> LiveResult<()> {
        let bytes = self.segmenter.take_segment();
        if bytes.is_empty() {
            return Ok(());
        }
        self.hls
            .push_segment(bytes.into(), SEGMENT_DURATION_SECS_F32)
            .await
            .map_err(|e| LiveError::Internal(anyhow::anyhow!("hls push: {e}")))?;
        self.segment_count += 1;
        self.bytes_total += bytes.len() as u64;
        self.duration_millis += (SEGMENT_DURATION_SECS_F32 * 1000.0).round() as u64;  // F4: 非整数秒不截断
        self.has_open_segment = false;
        Ok(())
    }
```

`finish`（lib.rs:1058）—— 签名改为返回摘要，终态快照缓存：

```rust
    pub async fn finish(&mut self) -> LiveResult<SessionSummary> {
        if let Some(snapshot) = self.summary_snapshot {
            return Ok(snapshot);                     // exactly-once：第二/多次调用零副作用
        }
        if self.segmenter.has_segment_data() {
            self.flush_segment().await?;             // 失败 ⇒ Err，不缓存快照；take_segment 已排空
        }
        self.hls.finish().await
            .map_err(|e| LiveError::Internal(anyhow::anyhow!("hls finish: {e}")))?;
        let snapshot = self.summary();
        self.summary_snapshot = Some(snapshot);
        Ok(snapshot)
    }

    /// 当前计数器（无副作用）。finish Err 后仍可读部分摘要（R4 用）。
    #[must_use]
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            segment_count: self.segment_count,
            bytes_total: self.bytes_total,
            duration_millis: self.duration_millis,
        }
    }
```

`finalize_session`（lib.rs:651）—— 错误 containment 形状不变，durability backstop 换成 retryable row：

```rust
async fn finalize_session(
    mut session: SrtSession,
    repo: &StreamRepo,
    stream_id: Option<ulid::Ulid>,
) {
    metrics::SessionCounter::removed();
    if let Err(e) = session.finish().await {
        warn!(error = %e, "SRT: error finalizing HLS on disconnect");
        // 不 return early：部分摘要仍要落 outbox（R4）。
    }
    if let Some(id) = stream_id {
        let s = session.summary();
        if let Err(e) = repo
            .mark_ended_in_tx(id, s.segment_count, s.bytes_total, s.duration_millis)
            .await
        {
            warn!(error = %e, %id, "SRT: failed to persist session-end transition");
        }
    }
}
```

`resolve_stream` 的 HLS-init 回滚（lib.rs:1117）**保持 `mark_ended` 不变**（决策 D1，见 §3）：该路径从未创建 `SrtSession`，没有“会话”就没有会话结束事件；go-live 行与回滚是既有的 pre-session 语义，不在本 seam 的“每会话一行”契约内。

### 2.3 `crates/aero-storage` — `mark_ended_in_tx` + 新 repo

`src/stream_session_end_outbox.rs`（新，clone of stream_go_live_outbox.rs，394 行）：

```rust
/// 会话结束转换的稳定身份（sibling of `GoLiveTransition`）。
pub struct SessionEndTransition { pub outbox_id: Uuid, pub event_id: Uuid }

/// 镜像 `MarkLiveOutcome`：`Ended` = 状态翻转 + 行已落；`NotLive` = 非 live（idle/已 ended）
/// 时零提交 —— 幂等重放（第二次 mark_ended_in_tx 提交 nothing，与 `AlreadyLive` 先例对称）。
pub enum SessionEndOutcome { Ended(SessionEndTransition), NotLive }

pub struct StreamSessionEndOutboxRow { /* 0179 同构列 + segment_count/bytes_total/duration_millis: i64 */ }

pub struct StreamSessionEndOutboxRepo { pool: PgPool }
impl StreamSessionEndOutboxRepo {
    pub fn new(pool: PgPool) -> Self;
    pub async fn claim_due(&self, now: OffsetDateTime, lease: Duration, limit: i64)
        -> Result<Vec<StreamSessionEndOutboxRow>, sqlx::Error>;   // SKIP LOCKED + claim-token fence
    pub async fn claim_by_id(&self, id: Uuid, now: OffsetDateTime, lease: Duration)
        -> Result<Option<StreamSessionEndOutboxRow>, sqlx::Error>;
    pub async fn assign_seq_if_absent(&self, id: Uuid, claim_token: Uuid, seq: i64)
        -> Result<bool, sqlx::Error>;
    pub async fn mark_nats_published(&self, id: Uuid, claim_token: Uuid, at: OffsetDateTime)
        -> Result<bool, sqlx::Error>;       // mark_stage("nats_published_at")：两阶段之一（G6；clone 源 stream_go_live_outbox.rs:236）
    pub async fn mark_webhooks_materialized(&self, id: Uuid, claim_token: Uuid, at: OffsetDateTime)
        -> Result<bool, sqlx::Error>;       // mark_stage("webhooks_materialized_at")：两阶段之二（clone 源 :246）
    pub async fn mark_completed(&self, id: Uuid, claim_token: Uuid)
        -> Result<bool, sqlx::Error>;       // 需 nats+webhooks **两阶段都非空**才可完成（CHECK 硬约束；缺 stage-setter 则永远无法完成）
    pub async fn mark_failed(&self, id: Uuid, claim_token: Uuid, attempts: i32,
        now: OffsetDateTime, error: &str) -> Result<bool, sqlx::Error>;  // 指数退避 cap 300s
    pub async fn get(&self, id: Uuid) -> Result<Option<StreamSessionEndOutboxRow>, sqlx::Error>;
}
```

`src/stream.rs` — `StreamRepo::mark_ended_in_tx`（mirror `mark_live` CTE，**无 lock-order dance**，见 §5 FM-4）：

```rust
pub async fn mark_ended_in_tx(
    &self,
    id: Ulid,
    segment_count: u64,
    bytes_total: u64,
    duration_millis: u64,
) -> Result<SessionEndOutcome, sqlx::Error> {
    let stream_id = uuid::Uuid::from_u128(id.0);
    let mut tx = self.pool.begin().await?;
    let outbox_id = uuid::Uuid::new_v4();
    let event_id = uuid::Uuid::new_v4();
    let subject = format!("live.stream.{id}");
    let traceparent = aero_common::telemetry::current_traceparent();
    let inserted = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid)>(
        // 占位符 $1..$8 连续（0179 模板删掉 hls_path 后必须重排——留空洞或悬空 $9 会在首执行报
        // "bind message supplies 8 parameters, but prepared statement requires 9"，F1）：
        // $1=stream_id, $2=outbox_id, $3=event_id, $4=subject, $5=traceparent, $6..$8=summary。
        r"WITH transitioned AS (
              UPDATE streams
                 SET status = 'ended',
                     ended_at = now()
               WHERE id = $1
                 AND status = 'live'
           RETURNING id, room_id, owner_id, title
          )
          INSERT INTO stream_session_end_outbox
                (id, event_id, stream_id, room_id, owner_id, title, subject, traceparent,
                 segment_count, bytes_total, duration_millis)
          SELECT $2, $3, id, room_id, owner_id, title, $4, $5, $6, $7, $8
            FROM transitioned
       RETURNING id, event_id",
    )
    .bind(stream_id)
    .bind(outbox_id)
    .bind(event_id)
    .bind(subject)
    .bind(traceparent)
    .bind(i64::try_from(segment_count).unwrap_or(i64::MAX))
    .bind(i64::try_from(bytes_total).unwrap_or(i64::MAX))
    .bind(i64::try_from(duration_millis).unwrap_or(i64::MAX))
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((outbox_id, event_id)) = inserted {
        tx.commit().await?;
        return Ok(SessionEndOutcome::Ended(SessionEndTransition { outbox_id, event_id }));
    }
    tx.commit().await?;
    Ok(SessionEndOutcome::NotLive)
}
```

`src/lib.rs`（storage）：`pub mod stream_session_end_outbox;`（:67 区块）+ `pub use stream_session_end_outbox::{SessionEndOutcome, SessionEndTransition, StreamSessionEndOutboxRepo, StreamSessionEndOutboxRow};`（:247 区块）—— **不 re-export 任何 token helper**（本模块本就没有，无撞名风险）。`stream.rs` 顶部 import 新类型（与现有 `GoLiveTransition` import 同处）。

### 2.4 Migration `migrations/0239_stream_session_end_outbox.sql`

0179 的逐列镜像 + 3 summary 列（`CHECK >= 0`），幂等 `IF NOT EXISTS`，无 FK（删 stream 不得抹掉已提交的集成工作），due 索引 `(available_at, created_at, id) WHERE completed_at IS NULL` **+ stream 索引 `(stream_id, created_at DESC)`**（0179 逐列镜像的另一半：供 A2 按 `stream_id` 计数断言与 B5-3 按流 backfill/查询；无 FK、无 `UNIQUE(stream_id)` ⇒ 重开播产生新行，`(stream_id, created_at DESC)` 支撑每流多行），`completed_at` 两阶段 CHECK（`completed_at IS NULL OR (nats_published_at IS NOT NULL AND webhooks_materialized_at IS NOT NULL)`），claim-token fence，`COMMENT` 说明“stream.session L1 聚合单元，每会话一行”。

## 3. Compatibility constraints

1. **`mark_ended` 保留不动** —— 6+ 调用方跨 rtmp/whip/server/storage（§0 bonus）。新 API 纯增量。
2. **`SrtSession::finish` 返回类型变化 blast radius = 6**（§0 C2，rg 实测）：**2 处在 tests.rs 之外** —— finalize_session（lib.rs:658，本设计重写）+ isolation_tests.rs:285 `let _ = …`；**4 处在 tests.rs** —— :319/:346/:348（`.unwrap()`）+ :392（`let _ =`）。全部在新签名下**零改动**编译：`.unwrap()` 要求 `SessionSummary: Debug`（§2.1 派生，**必须保留**——去掉即 tests.rs 编译失败）+ `LiveError: Debug`（aero-live-core 已成立）；`let _ =` 平凡兼容。`SessionBackend` trait、`CountingBackend`、pump、feed 全部零改动 → hermetic 套件按原样继续驱动真实 listener 路径。
3. **无新 crate 边**：aero-live-srt→aero-storage 已存在；aero-storage 只加模块+迁移；`LiveError` 无需新 variant（`Internal` 已覆盖 hls push/finish）。
4. **C1 强制**：storage 不依赖 live-core ⇒ `mark_ended_in_tx` 收 primitives（u64×3），`SessionSummary` 留在 srt crate；转换 `i64::try_from`（实际不可达溢出，clamp 不报错——teardown 不得因 summary 转换失败）。
5. **duration 用整数毫秒**（证据 R1 明示允许的 “integer millis equivalent”）：`2.0f32 × 1000.0 == 2000.0` 恰可表示，`N×2000` 精确；与 `SEGMENT_DURATION_SECS_F32` 保持耦合（改时长自动跟随，无 const 漂移）。**F4 舍入注记**：`(SECS_F32 × 1000.0) as u64` 对非整数秒会截断（1.3f32 → 1299）；当前 const 2.0 不受影响，但“自动跟随”要任意时长成立，须 `.round()`（§2.2 片段已按此写）或改用整数毫秒 const。
6. **文件尺寸红线**（scripts/file-size-check.sh：800 WARN / 1200 HARD）：lib.rs 1142 → ≤1182；tests.rs **一行不碰**（1137 现状，37/37 计数不变）；stream.rs 723 → ~778（WARN 线内）；新 repo 独立文件（~400 行），**不 append 进 stream.rs**。
7. **hot-path 不变量**：计数器是普通字段；`feed_packet`/`pump` 无 async/storage（结构性成立 + 既有 isolation harness 钉死）。
8. **迁移纪律**（AGENTS.md §4.2）：加迁移后**先 `cargo build` 再 migrate**（编译期嵌入）；序号 = `ls migrations/*.sql | wc -l` + 1 = **0239**（不硬编码）。
9. **工作区 lints**：`SessionSummary` 派生不引 clippy 新警告；`summary()` 标 `#[must_use]`；无 `unsafe`、无新 `unreachable_pub`。
10. **决策 D1**：`resolve_stream` HLS-init 回滚继续用 `mark_ended`（无会话 ⇒ 无 session-end 行）—— 与 R5 “每会话一行” 契约一致，并在代码注释记录。
11. **无新 env、无新配置、无 web 改动**；SPA 不涉及（SRT 服务端路径）。

## 4. Test design（concrete assertion contracts）

### A1 — hermetic（`src/summary_tests.rs`，新，`cargo test --workspace --lib`）

复用既有直接驱动姿势（tests.rs:946 `session_with_max_bandwidth_seeds_the_pacer` 的 tempdir `HlsWriter` + `SrtSession::new`；keyframe 对齐 TS payload 构造沿用 isolation_tests.rs:341 区域 helpers）：

- **T1 确定性计数**：tempdir HlsWriter → `SrtSession::new` → `feed` 构造 K 个 keyframe 切分的完整 segment（记录每段 `bytes.len()`）→ `finish()` → `Ok(SessionSummary { segment_count: K, bytes_total: Σlen, duration_millis: K × 2000 })`。断言**精确相等**。**K 的定义 = 可观测 push 次数（F5）**：K 个 keyframe ⇒ (K−1) 次 cut-time push（每个 `CutBeforeKeyframe` 落一段，lib.rs:1030-1035）+ finish 时 1 次尾段 flush = 恰好 K 次 push —— `segment_count` 数的是 push 而非 keyframe/cut 数；且 K>6 时**不得数磁盘 `.ts` 文件**（`LIVE_WINDOW_SEGMENTS=6` 会驱逐旧段），按 cut accounting（每次 push 记录 `bytes.len()`）跟踪。
- **T2 exactly-once**：T1 后再次 `finish()` → `Ok` 且返回 `==` 首次快照；随后 `summary()` 亦相等。第二调用零副作用（segmenter 已排空 + HLS `finalized`，无二次 flush 机会——由“返回值相等 + 无 Err”断言，且实现上 `summary_snapshot` 短路）。
- **T3 N=0**：空 session（未 feed）→ `finish()` → `Ok(SessionSummary::ZERO)`。
- **T4 尾段失败部分摘要**：feed K 完整 segment + 一段只缓冲不切分的尾字节（`has_open_segment == true`）→ **删除 HLS 根目录**（`finalize_broken_writer` 同款确定性注入）→ 第 1 次 `finish()` → `Err`（尾段 push 的 `File::create` 失败，`hls.finish` **尚未进入**，`finalized == false`）；`session.summary()` == K-segment 摘要（失败尾段不计入；`take_segment` 已排空 ⇒ 无重推、无双计）。**第 2 次 `finish()` 契约（本设计取「恢复目录 → Ok」，二选一钉死）**：**先恢复 HLS 目录**再 `finish()` → `Ok`，摘要与第 1 次 `summary()` 逐字段相等（失败不缓存快照 ⇒ FM-10 重试收敛；`finalized` 置位先于 `write_manifest`，目录已恢复则 manifest 写入成功）。**若不恢复目录**，第 2 次 `finish()` 确定性返回 `Err`（跳过 flush → `hls.finish()` 置 `finalized=true` 后 `write_manifest` 再次 `File::create` 失败）——**不得断言 Ok**。
- **T5 计数只认成功 push**：把 `push_segment` 失败注入到**中途**（K 段后删目录再 feed 到下一次 cut）→ `summary()` 仍 == K。

### Teardown 形状 drill — hermetic（`src/isolation_tests.rs` 扩展，或经 `CountingBackend`）

既有 `finalize_broken_writer`（:273-284）已断言“finish 错误被吞、不 panic、finalize 计数 +1”——**原样保持绿**即钉死 R4 containment 形状；无需改 `CountingBackend`（C2：新签名下原样编译）。新增一条断言面（可选，低成本）：broken-writer drill 结束后 `session` 的 summary 可读（在 CountingBackend 内以 `session.summary()` 读一次并记录——但 backend 不持 DB，故仅验证“错误后 summary 可观测”这一形状，行落库交给 A2）。

### A2 — DB-gated

**srt 侧全路径 drill（`src/db_tests.rs`，新，`#[ignore = "requires live Postgres"]`）** —— 用 `DATABASE_URL` + throwaway 库（AGENTS.md §4.3）：

1. seed：`INSERT INTO participants …`（storage stream.rs:363 同款）+ `StreamRepo::create`（或直插 streams，room_id NULL 使 `mark_live` 跳过 room-access 臂）。
2. `repo.mark_live(id, …)` → `Started`。
3. 构造 `SrtSession`（cfg `hls_dir` 下 tempdir），**feed K+1 个 keyframe** ⇒ K 段在 cut 时已落盘 + 1 个 open 尾段（K = 已 flush 段数，与 T1 的 push 计数同口径；只 feed K 个 keyframe 会少一段——原稿 off-by-one）。
4. 删除 HLS 根目录 → 跑**生产** `finalize_session(session, &repo, Some(id))`（同 crate 私有可调）：尾段 flush 在 `push_segment` 失败（与 T4 同形状；若 drill 构造为无 open 尾段，错误形状则是 `write_manifest`——两者都属 FM-1 覆盖）→ warn → 部分摘要继续落行。
5. 断言：`streams.status == 'ended'`；`SELECT count(*) FROM stream_session_end_outbox WHERE stream_id=$1` **恰为 1**，`completed_at IS NULL`，`event_id`/`outbox_id` 非空，`segment_count == K`、`bytes_total == Σ`、`duration_millis == K×2000`；`StreamSessionEndOutboxRepo::claim_due(now, lease, 10)` 能 claim 到该行（retryable parking 生效）。
6. **FM-2 containment（G2）**：同库 seed 第二条流 `id2` → `mark_live` → `pool.close()`（closed pool 立即 Err，无 hang）→ `finalize_session(session2, &repo, Some(id2))` 干净返回、不 panic；重开 pool 断言 `id2.status` **仍为 'live'** 且其 outbox **零行**（原子性两面性：status 翻转与行同失）。

**storage 侧（`src/stream_session_end_outbox.rs` 或 stream.rs db_tests，`#[ignore]`）**：

- CTE 原子性：`mark_live` → `mark_ended_in_tx(id, 3, 12345, 6000)` → `Ended(transition)`；status='ended' **且** 恰一行、summary 列 == 传入值（同语句全有或全无）。**这是 F1 占位符重排的 tripwire**：占位符有空洞/悬空 $9 时首执行即报 `bind message supplies 8 parameters, but prepared statement requires 9`，先于任何生产接线。
- 幂等重放：再次 `mark_ended_in_tx` → `NotLive`，行数仍 1（不重复插入）。
- `NotLive` 先例：idle 行直接 `mark_ended_in_tx` → `NotLive` 零提交。
- **重开播 cardinality（G5）**：`live → ended → live → ended`（第二次 `mark_live` 走 `status <> 'live'` 守卫）→ outbox **2 行**、`event_id`/`outbox_id` 两两不同 —— 每**会话**一行（区别于每流一行），直接断言 FM-9 有界性。
- **溢出 clamp（G3）**：`mark_ended_in_tx(id, u64::MAX, u64::MAX, u64::MAX)` → `Ended`（非 Err），行内三个 summary 列 == `i64::MAX`（clamp 不报错，FM-6）。
- claim 机器：`claim_by_id`（fence：错误 claim_token 的 `mark_failed` **与 `mark_completed`** 均返回 false —— 后者补齐对称性）→ `mark_failed` 退避（复刻 `retry_is_bounded…` 单测断言 `retry_delay` cap 300s 的 clone）→ `mark_completed` 两阶段：仅 `mark_nats_published` 后 → false；再 `mark_webhooks_materialized` 后 → true（钉死 G6 的 stage-setter 存在性与两阶段 CHECK）。
- **`assign_seq_if_absent` 幂等（G4）**：assign → `Some(seq)`；同 token 再 assign 同 seq → 同值；同 token assign 不同 seq → **值不变**（COALESCE-once）；错误 claim_token → `None`（fence）。源仓库（stream_go_live_outbox.rs）同样无此测试 —— clone 补齐。
- 镜像 `stream.rs:500-530` 既有 go-live db-test 的断言形状。

### 迁移 replay

`make migrate-smoke`（throwaway 全链 replay）—— **`cargo build` 先行**（§3-8）。

### A3 — 记录型前置条件（不实现）

设计内交付**契约文本**（见 §8 交接块）：行形状（stream_id/room_id/owner_id/title/subject 快照 + 3 summary 列 + event_id/outbox_id）+ 每会话恰一行 + 默认 ingest 优先级车道形状。B5-3 落地的 500+1 drill（500 `stream.session` + 1 `admin.content.flag` → flag 先 claim）由该 slice 验收；本 direction 只保证可 backfill 的行形状与 cardinality。

### A4 — 37/37 不受影响

tests.rs 零改动（新测试全进 `summary_tests.rs`/`db_tests.rs`）；isolation_tests.rs/rotation_tests.rs 原样绿；`feed_packet`/`pump` 无 storage await（结构性 + isolation 断言不变）。

## 5. Failure modes & mitigations

- **FM-1 HLS-finish 错误（finalize_broken_writer 形状）**：`finish()` Err → warn 不 return early → 部分摘要经 `mark_ended_in_tx` 落 retryable row。审计身份不丢（A2 全路径断言）。这**就是本设计要修的洞**：今天该错误下连 status 翻转都只靠 mark_ended 的运气。
- **FM-2 teardown 时 DB 不可用**：`mark_ended_in_tx` Err → warn；**status 翻转与 outbox 行同失**（原子性两面性）。与现状持平（今天的 mark_ended 在 DB down 时同样丢失）且不更糟；不引入 in-crate 重试（阻塞 UDP loop 会拖死其他 peer —— 明确拒绝，证据 §7 同旨）。缓解：DB down 时 listener 整体已无法 resolve 新会话，属操作级事件。**containment 由 G2 钉死**（§4 A2 closed-pool drill：干净返回、不 panic、不 hang，status 与行同失）。
- **FM-3 重复结束（双协议/重放竞态）**：`WHERE status='live'` 守卫 ⇒ 第二次调用 `NotLive` 零提交；行数恒 ≤1。mark_live 允许 ended→live 重新开播，outbox 无 FK 不构成约束。
- **FM-4 死锁**：`mark_ended_in_tx` 只取**一个**行锁（UPDATE streams）+ 无其他表访问 ⇒ 单锁事务不可能成环；mark_live 的 workspace→room→stream 锁序 dance 不需要（它锁序存在是因为 room-linked 写路径先持 workspace 锁；本语句不读 rooms）。注释说明。
- **FM-5 失败 flush 的双重效应**：`take_segment` 先排空、push 后失败 ⇒ 该段字节已丢且不计入（“durably written” 真话）；`has_open_segment` 保持 true，但后续 `finish()` 的 `has_segment_data()` 为 false ⇒ 不重推、不双计。确定性（T4 钉死）。
- **FM-6 溢出**：u64 计数器 → i64 列经 `try_from` clamp（2s/段、100 年 10 Gbps 也远不及 i64::MAX；clamp 而非报错，teardown 不许因 summary 失败）。f32 时长以毫秒整数累积，无漂移。
- **FM-7 文件尺寸**：lib.rs ≤1182（HARD 1200）；tests.rs 不动；stream.rs ≤~778。任何超出即 file-size-check 红，先拆再写。
- **FM-8 迁移静默 no-op**：不 `cargo build` 就 migrate ⇒ 0239 不生效（编译期嵌入）。顺序钉死：build → migrate-smoke。
- **FM-9 无消费方的行堆积**：governance 消费方 [PROPOSED] 落地前，行以每会话一条速率累积且永远 claimable（`available_at` 到期即被 claim 再退避——无消费者时 claim 循环不存在，行静置）。有界（每会话一行）、due 索引廉价；B5-3 slice 上线后自然排空。操作注记写入 migration COMMENT。
- **FM-10 快照缓存与错误路径**：finish 成功才缓存快照；失败不缓存 ⇒ 二次 finish 重试 `hls.finish()`（幂等）→ **同一结果 = 相同的 summary 值**（非必然 Ok：目录仍删时 `write_manifest` 再失败 ⇒ 确定性 `Err`；目录恢复后 ⇒ `Ok`，T4 钉死两种形状）。无半缓存态。

## 6. Migration steps

1. **Crate-only hermetic slice（R1+R2+A1）**：`summary.rs` + `SrtSession` 字段/`flush_segment`/`finish`/`summary()` + `summary_tests.rs`。`cargo test --workspace --lib` 全绿（37/37 + 5 + 5 + 新 T1–T5）。
2. **Migration 0239 + storage slice（R3+A2-storage）**：写 `0239_stream_session_end_outbox.sql` → **`cargo build`**（嵌入迁移）→ throwaway 库 `aero-cli migrate` / `make migrate-smoke` → `stream_session_end_outbox.rs` + `mark_ended_in_tx` + storage lib.rs 注册 → db-tests（`-- --ignored` + `DATABASE_URL`）。**既有 db-gated 测试成为 0239 依赖门（F2）**：`run_until_cancelled_boot_fails_open_without_audit_provisioning`（isolation_tests.rs:842-1033，`#[ignore]`）自带 `aero_storage::db::migrate(&pool)`，重接后在取消 drain 上断言 `status == Ended`（:1033）——只有嵌入 0239 的新 build 能过；对旧 build 重接 ⇒ INSERT 失败被 warn 吞、status 停 'live'、该既有测试转红。
3. **`finalize_session` 重接（R4）** + `db_tests.rs` 全路径 A2 drill（`#[ignore]`）。此处才让生产路径写新表；重接后立即跑 `-- --ignored` 验证步骤 2 点名的既有 db-gated 测试仍绿（0239 依赖门）。
4. **A2-srt 全绿**：throwaway 库 replay 后跑 `cargo test --workspace --lib -- --ignored`（需 `DATABASE_URL` + 已迁移）。
5. **Full gate（A4）**：`cargo check --workspace` · `cargo test --workspace --lib`（+ `-- --ignored`）· `cargo clippy --workspace --all-targets`（零新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规；file-size 盯 lib.rs/stream.rs 增量）。
6. **交接 A3**：把 §8 契约块写进 `docs/requirements/…req.md` 的 A3 或 B5-3 slice 文档。

## 7. Testable acceptance mapping

| Acceptance | 测试 | 文件 / 门 |
|---|---|---|
| A1 确定性 exactly-once 摘要（N 段精确值、重复 finish 相等、N=0、尾段失败部分摘要、中途失败不计数） | T1–T5 | `src/summary_tests.rs`（hermetic）；`cargo test --workspace --lib` |
| R4 containment 形状（finish 错误不 return early、不 panic、summary 可观测） | 既有 `finalize_broken_writer` drill 保持绿 + summary 可读断言 | `src/isolation_tests.rs`（hermetic） |
| A2 CTE 原子性 + retryable claim + 幂等重放 + summary 列 + G3/G4/G5/F1-tripwire | storage db-tests（Ended/NotLive/claim_by_id/mark_failed **与 mark_completed** fence/两阶段经 stage-setter/重开播 cardinality/`u64::MAX` clamp/`assign_seq_if_absent` 幂等） | `src/stream_session_end_outbox.rs`（`#[ignore]` + `DATABASE_URL`） |
| A2 生产 teardown 全路径（finish 错误 → 行仍在、status ended、恰一行、可 claim）+ FM-2 containment | `src/db_tests.rs` drill（+ closed-pool G2 drill） | `#[ignore = "requires live Postgres"]`，throwaway 库 |
| 迁移 replay | `make migrate-smoke`（fresh-deploy 全链） | **先 `cargo build`** |
| A3 前置条件（500+1 drill 的行形状/cardinality） | 契约文本交接，不实现 | B5-3 slice 验收 |
| A4 37/37 + 5 + 5 不受影响；hot path 零 storage | tests.rs 零改动 + 全套件绿 + isolation 断言原样 | `cargo test --workspace --lib` |

## 8. Out of scope（boundary reminders）

- **Governance outbox 本体**（`0239_audit_governance_outbox.sql` 不存在；class 列表 message/room/admin；stream.\* 准入）—— [PROPOSED]。本 direction 只保证 producer-side 行形状 + 稳定 identity。
- **L1 聚合窗口 bookkeeping**（B5-1 req A4）—— 每会话一行即原子聚合单元；窗口/EWMA 不在此。
- **B5-3 moderation-priority drill** —— 服务端 slice；A3 仅记录前置条件。
- **WHIP / RTMP teardown 路径** —— 不同 crate，后续各自落同一 seam。
- **`feed_packet`/`pump` 上任何 storage/audit I/O** —— 硬禁（E12/C2）。
- **`resolve_stream` HLS-init 回滚** —— 保持 `mark_ended`（D1）。

**A3 交接契约（写给 B5-3 slice）**：`stream_session_end_outbox` 每会话恰一行；行 = `event_id`/`outbox_id`（稳定唯一）+ `stream_id`/`room_id`/`owner_id`/`title`/`subject`（= `live.stream.{id}`）+ `segment_count`/`bytes_total`/`duration_millis`；`completed_at IS NULL` 即待消费；默认 ingest 优先级车道。500+1 drill 的 backfill 直接 INSERT 本表即可。**两条必须写进 B5-3 前置条件（S2）**：
- **两阶段完成是硬约束**：`completed_at` CHECK 要求 `nats_published_at` **且** `webhooks_materialized_at` 均非空 —— **只发 NATS 不落 webhook 的消费方永远无法 `mark_completed`**（恒 false，行滞留）。B5-3 若只做 NATS 车道，须同时戳 webhook 阶段，或显式记录“完成 = 两阶段”的例外协议。
- **`stream.live` 无 end-row 不对称是预期（非泄漏）**：`resolve_stream` HLS-init 回滚（D1）提交了 go-live outbox 行并把 status 置 ended，但无会话 ⇒ **无** session-end 行。消费方不得把“收到 `stream.live` 事件却无匹配 end 行”当泄漏处理（pre-session 语义，不在本 seam 每会话一行契约内）。

## 9. Sequencing

与证据 §8 一致，五阶段：hermetic slice（R1/R2/A1）→ 迁移+storage（R3/A2-storage，build→migrate）→ finalize 重接（R4）+ A2-srt → 全门（A4）→ A3 交接。每阶段独立可验收；迁移序号 0239 在写文件时以 `ls migrations/*.sql | wc -l` 现场确认。
