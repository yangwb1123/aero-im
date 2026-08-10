# Design — aero-live-core：IngestEvent 接线为直播生命周期事件生产者（RTMP/SRT）+ tag="kind" serde 冲突拆雷（2026-08-09 复核刷新 · 三 review 修订版）

- **Direction**: "Wire IngestEvent into RtmpIngest/SrtIngest as the live lifecycle audit producer and defuse the tag=\\\"kind\\\" serde collision before B5 adds variants"
- **Module (analysis root)**: `crates/aero-live-core`（叶子 crate）；交付面含 `crates/aero-live-rtmp` / `crates/aero-live-srt`（接线）、`crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（drill 扩展）
- **Requirements**: `docs/requirements/2026-08-08-aero-live-core-ingestevent-lifecycle-producer.req.md`（2026-08-09 复核 pass）
- **Supersedes**: `docs/design/2026-08-08-aero-live-core-ingestevent-lifecycle-producer.design.md`（本刷新保留其全部经核验结论，更正调用点认知 + 登记 08-09 行号漂移；08-08 版 §0.1 serde 机制更正仍有效并被本版继承）
- **Campaign**: `aero-im-b5`；contract anchor `scripts/b5-pin.sh` 37-slot pin
- **Status**: Design（evidence 全部逐条对源码核验，2026-08-09；行号为核对时锚点，可能漂移——**文件/符号**才是稳定锚点）

---

## 0. 证据核验（evidence = untrusted claims，逐条复核）+ 三 review 采纳登记

requirements 复核摘要（`docs/requirements/2026-08-08-...req.md`）的**全部引用均经本设计独立复核**——每个文件/符号/行号都对照工作树重查；2026-08-09 三份 adversarial review（durability-consistency / lifecycle-state-machine / test-acceptance）的 finding 逐条复核后纳入（§0.4 登记）。

| # | 证据 claim | 复核结论 | 核验要点（本设计实测） |
|---|---|---|---|
| E1 | `IngestEvent` @ `crates/aero-live-core/src/lib.rs:78-92`，`#[serde(tag="kind", rename_all="snake_case")]`，全仓零构造 | ✅ 精确 | `:78` serde attr、`:79 pub enum IngestEvent`、`:81-92` `Started{stream_id: Ulid(ulid_as_uuid), hls_path: PathBuf}` / `Ended{stream_id: Ulid, reason: String}`；`rg -n "IngestEvent" crates/` 唯一命中 = 定义本身 |
| E2 | observer 引文在 `IngestEvent` doc :74-75，非 :113 | ✅ 精确 | doc :73-75「intentionally lightweight — observers can hang notifications, bus publishing, or metrics off this…」；`:113` 在 `LiveError` Debug impl 内 |
| E3 | `LiveIngest` trait :133-134；`impl LiveIngest for` 恰 2 处（rtmp :262-267 / srt :310-313），均委托 `run_until_cancelled`；WHIP 无 impl | ✅ 精确 | trait `async fn run(&self, repo, cfg)`（:134-135）；rtmp :262 / srt :310；全仓无第三处 |
| E4 | 六生命周期锚点 | ✅ 精确 | RTMP `mark_live` :489（refusal `continue` :514-517）、`mark_ended` :364（publish loop 后无条件）+ :525（HLS init 失败回滚）；SRT `mark_live` :1098（`resolve_stream` :1085）、`mark_ended` :662（`finalize_session` :651，`if let Some(id)` 内）+ :1117（HLS 回滚 `return Err(Internal)`） |
| E5 | `StreamRepo::mark_live`（stream.rs:179-262）单 CTE + stable event_id；`mark_ended`（:264-266）零 outbox | ✅ 精确 | `UPDATE streams … RETURNING` → `INSERT INTO stream_go_live_outbox … FROM transitioned` → `MarkLiveOutcome::Started(GoLiveTransition{outbox_id, event_id})`（:255-259）；`mark_ended` 仅 `UPDATE status='ended'`、零 RETURNING/outbox |
| E6 | 0239 CHECK 值域 + claim `ORDER BY priority DESC` | ✅ 精确 | `status(0,1,2,3)` :30-31；`class('admin','message','room')` :33；`priority>0` :34-35；`delivery_mode('push')` :36-37；`pg.rs:116`（实测 :114-116）——**条件映射零 DDL**；trigger 仅 `message.moderated` 入队（其余 RETURN NEW） |
| E7 | kind-tag 规则 + rename 先例；serde 1.0.228 | ✅ | `notify_kind`（common/src/model/event.rs:77）/ `call_kind`（media.rs:189/:236/:268）；编译期硬错误（08-08 §0.1 继承） |
| E8 | truth-check 盲区（pub enum 永久隐身） | ✅ | truth-check.sh 只查 orphan 文件 + 零调用 `with_*` builder（:151-224）；`IngestEvent` 不报 |
| E9 | drill 结构：500/1/501/100/10；verdict 行 :276/:298/:332/:350 | ✅ 精确 | `BACKLOG_ROWS=500` :79、`MODERATION_ROWS=1` :80、`TOTAL_ROWS=501` :81、`BATCH_SIZE=100` :82、`MAX_ROUNDS=10` :54；verdicts :276/:298/:332/:350；D8′ 门（LOCK+COUNT+TRUNCATE :120-151，REFUSED exit 1 / 无 0239 exit 2）；词表 `MODERATION_OUTBOUND_ACTIONS` :77-78 |
| E10 | `run_priority` :669 spawn drill 透传 exit code；REFUSED 门 :714-721 | ✅ 漂移<10 行 | `audit_provision.rs` `run_priority` :669；D8′ 门 :715-719；spawn :744-763、exit 映射 :765-781 |
| E11 | harness grep `priority: landed` + `drill: moderation-action-vocabulary: PASS` → `b5_check "moderation-priority-drill"` | ✅ 漂移<10 行 | `scripts/test-integration.sh` 注释 :448、echo :542、grep :553/:558、`b5_check` :564；SKIP 分支 :567/:577 |
| E12 | boot/ingest.rs :60-88 sink 先例 | ✅ 精确 | `RtmpIngest::new().run_until_cancelled(repo, live_cfg, cancel)` :60-61；SRT 块 :66-88（`with_passphrase` :79-84）——boot 零改动 |
| E13 | `RtmpIngest` 单元 struct derive(Debug, Default, Clone) :171-174 | ✅ | `pub struct RtmpIngest;` :172-175；`new()` :177-180；spawn :243-252；`handle_connection` :270 |
| E14 | SRT `SessionBackend` trait :323-327（`resolve`/`finalize`）+ 构造唯一处 :362 | ✅ 精化 | trait :323-327；impl :335-343；构造 :362 |
| E15 | `backend.finalize` 调用点 | ⚠️ 精化 | **三个**调用点 = **:397（错误路径）/ :419（drain）/ :501（SHUTDOWN）**（08-08 只列 :395/:495 unify 两处，漏错误路径） |
| E16 | SrtSession 可测试构造 | ✅ | `SrtSession::new(hls)`（isolation_tests.rs 先例）、`finish` :1058 |
| E17 | b5-pin 37 槽（15 executed + 22 [PROPOSED]） | ✅ | array :84-120 恰 37、无重复；verdict 协议；`b5_check` helper :73 |
| E18 | serde_json 已依赖 aero-live-core | ✅ | `crates/aero-live-core/Cargo.toml:20` |
| E19 | 工作树状态 | ✅ | `audit_provision.rs`、`migrations/0239-0241`、`aero-audit-connector/` 为工作树在途文件 |

### 0.1 继承的机制更正（08-08 §0.1，复核有效）

- **serde 撞名 = 编译期硬错误**（serde ≥1.0.28，PR #1170 / commit `a799ea17`）非运行时 panic。工作区钉死 1.0.228（Cargo.lock :4208-4210）——B5 未来加 `kind` 字段直接编译失败（fail-fast）。
- **运行时 Err 的复现面 = 线上重复键 JSON**：`{"kind":"started","kind":"started",…}` → `Err("duplicate field \`kind\` at line …")`（TaggedContentVisitor，实证）。
- 因此 R2.2 负例测试 = **真实 enum + 重复键 JSON**；un-renamed 镜像 enum 不可编译（测试本体编不进），其保护由 serde_derive 提供。
- 延后修正登记（供下批次碰 AGENTS.md 时执行）：§4.2 机制句折为「serde ≥1.0.28 编译期硬错误；线上重复键 JSON → 运行时 Err」——规范内容（必须 rename）不变。

### 0.2 本刷新新发现（相对 08-08 版的两处设计输入精化）

1. **SRT teardown 汇聚点**：`backend.finalize`（trait 方法，非自由函数）**三个**调用点——**:397（handle_datagram 错误路径）/ :419（drain）/ :501（SHUTDOWN）** → 唯一自由函数调用点 :341。**sink 放 `RepoSessionBackend` 字段 = 三条 teardown 路径全部自动发射 Ended**，:397/:419/:501 **零改动**。08-08 版把 :395/:495 当自由函数调用点（需改签名且漏了错误路径）——实为 trait 方法调用点（设计简化，见 §1.3）。
2. **RTMP Started 发射点唯一可达性**：refusal 分支（:499-517）以 `continue` 跳过，`if let Some(description) = refusal` 之后 = 仅 `Started` 可达——`dir = hls_path_for(&cfg.hls_dir, stream.id)` 与 `stream.id`/`cfg` 均在作用域，发射零额外计算。

### 0.3 行号漂移登记（08-09 复核）

| 锚点 | 08-08 版 | 08-09 实测 |
|---|---|---|
| SRT `RepoSessionBackend` 构造 | :372-375 | **:362** |
| SRT trait / impl | :327-331 / :335 | :323-327 / :335-343 |
| `finalize_session` 自由函数 | :651 | :651 ✓ |
| `backend.finalize` 调用点 | 08-08 未列 | **:397 / :419 / :501** |
| audit_provision spawn | :752 | :744-788 |
| harness grep | :545-556 | :553/:558 |
| drill verdict 行 | :276/:298/:332/:350 | 全部精确 ✓ |
| boot/ingest.rs | :60/:80-85 | :60-61 / :66-88 |

### 0.4 三 review finding 采纳登记

| 来源 | finding | 采纳落点 |
|---|---|---|
| durability | **F6a** 后果反转（丢 Started 无害 / 丢 Ended 全损） | §1.4 语义注 + §4 F6a |
| durability | **F6b** `end_stream`（live.rs:515-527）owner-end 对 observer 零事件 | §3 F6b + §1.4 语义注 |
| durability | **F6c** Ended-before-committed 语义 + **observer-consumer-WEN R3 同批依赖 pin** | §1.1 语义注 + §1.5 producer pin + §6 依赖 pin |
| durability | **D1** REST-end + teardown 双 "ended" 无关联 key → 新 **F12**；**D2** WHIP/REST 不可见 → **F14**；**D3** observer Ended 可先于 relay Live → **F15**；**D4** relay env-disable（`AERO__SERVER__STREAM_LIVE_OUTBOX_POLL_MS=0`）→ **F16** | §3 F12/F14-F16 |
| lifecycle | **RTMP Started-without-Ended**（三路径：cycle-drain :327-339、post-Started 协商错 :544-549、dup-publish 覆写 :511-521）+ **AC 守卫** | §3 新 **F13** + §5 AC-a 守卫（无孤儿 Started 断言） |
| lifecycle | **F5 更正**：「行态由 retention/sweep 愈合」**无支撑机制**（仅 stream.rs:265 / stream_category.rs:409 触列；无 stale-live 恢复） | §3 F5 更正 + §1.4 顺序注 |
| lifecycle | `backend.finalize` 调用点漂移 | §0.2-1 / §0.3 登记 |
| test-acceptance | AC-b 不可执行 → **capability-probe 守卫**（0→early return / 1→全形状断言 / >1→fail，回滚注入腿同） | §5 AC-b 重写 + §4 迁移步骤 4/6 |
| test-acceptance | AC-d churn 面须为**全仓库字面量清单**（header doc :19/:26，含注释/println） | §1.5 全字面量 churn 清单 |
| test-acceptance | AC-a seed 措辞过重（`has_effective_room_access` 实际只在 room_id Some 时跑） | §5 AC-a 措辞修正 |
| test-acceptance | 新 verdict 以 exit-code 强制即可（drill 已 bail），harness grep 不必扩 | §1.5 harness 行 |

---

## 1. API 变更

### 1.1 aero-live-core（新公共 API，全部增量、零删除）

```rust
// lib.rs（IngestEvent 之后追加）

/// Reason spellings — single source of truth, pinned by unit tests (R1.2).
pub const REASON_PUBLISHER_DISCONNECTED: &str = "publisher disconnected";
pub const REASON_HLS_WRITER_INIT_FAILED: &str = "hls writer init failed";

impl IngestEvent {
    /// Single construction surface (R1.4): backends MUST build events through
    /// these helpers — grep-verifiable.
    pub fn started(stream_id: Ulid, hls_path: PathBuf) -> Self { Self::Started { stream_id, hls_path } }
    pub fn ended(stream_id: Ulid, reason: impl Into<String>) -> Self { Self::Ended { stream_id, reason: reason.into() } }
    /// `Some` iff the transition actually committed; `AlreadyLive`/`NotFound` → `None`.
    pub fn started_if_transitioned(outcome: &aero_storage::MarkLiveOutcome, stream_id: Ulid, hls_path: PathBuf) -> Option<Self> {
        match outcome {
            MarkLiveOutcome::Started(_) => Some(Self::started(stream_id, hls_path)),
            MarkLiveOutcome::AlreadyLive | MarkLiveOutcome::NotFound => None,
        }
    }
}

/// Best-effort observer sink (R1.5): default no-op; fail-open; catch_unwind .
#[derive(Clone)]
pub struct IngestSink(Arc<dyn Fn(IngestEvent) + Send + Sync>);
impl IngestSink { pub fn new<F>(f: F) -> Self; pub fn emit(&self, event) { /* catch_unwind + warn! */ } }
impl Debug for IngestSink { /* "IngestSink(noop)" | "IngestSink(closure)" */ }
impl Default for IngestSink { /* no-op */ }
```

- **enum doc 增补（R2.1）**：AGENTS §4.2 规则原文（variant 字段不得名 `kind`，必须 `#[serde(rename = …_kind)]`，先例 `call_kind`/`notify_kind`）+ serde ≥1.0.28 机制（编译期硬错误；线上重复键 JSON → 运行时 `Err("duplicate field \`kind\`")`）。精度注：检查作用于 internal-tag enum 的 struct-style variant 字段，skip 字段豁免。
- **语义注（F6c/F62b）**：`Ended` = "teardown 事实" — **不是** "committed 事实"（可先于 durable 行态）；`end_stream`（REST owner）对 observer **零事件**（F6b）——observer 消费方须自 reconcile owner-end。
- 派生集不变：三项 derive 均保（wire 零变化）；零新依赖；lib.rs 增量 ≤ ~150 行 < 800 行李（file-size-check.sh 唯一权威）。

### 1.2 aero-live-rtmp

```rust
#[derive(Debug, Default, Clone)]
pub struct RtmpIngest { sink: IngestSink }   // 单元 struct → 字段化；derive 行不动

impl RtmpIngest {
    pub fn new() -> Self { Self { sink: IngestSink::default() } }
    #[must_use]
    pub fn with_sink(mut self, sink: IngestSink) -> Self { self.sink = sink; self }
}
```

- `handle_connection(socket, repo, cfg, cancel, sink: &IngestSink)`（私有自由函数 :270；签名 +1 参数）；spawn 点 :243-252 捕获 `self.sink.clone()`。
- `LiveIngest::run` / `run_until_cancelled` / trait 签名 零改动。

### 1.3 aero-live-srt

```rust
#[derive(Debug, Clone)]
pub struct SrtIngest { /* 既有…, */ sink: IngestSink }
impl SrtIngest { pub fn with_sink(mut self, s: IngestSink) -> Self }
```

- `RepoSessionBackend { repo, cfg, sink }`（构造唯一处 :362 注入 `ingest.sink.clone()`）；trait `SessionBackend`（:323-327）**零改动**。
- `resolve_stream(…, sink: &IngestSink)`（:1085；调用点仅 :337）与 `finalize_session(…, stream_id, sink: &IngestSink)`（:341；调用点仅 :341）——pub 自由函数签名各 +1 参数。
- **:397 / :419 / :501 三 teardown 路径零改动**（§0.2-1：经 trait impl 单点汇聚自动获得 sink）。

### 1.4 发射规则（两后端共用，R1.1-R1.6 落地形态）

| 锚点 | 代码形态 |
|---|---|
| RTMP :489 / SRT :1098（mark_live 后） | `if let Some(ev) = IngestEvent::started_if_transitioned(&transition, stream.id, hls_path_for(&cfg.hls_dir, stream.id)) { sink.emit(ev); }` |
| RTMP :525 / SRT :1137（HLS init 回滚） | `mark_ended` 调用后（Ok/Err 均发）：`sink.emit(IngestEvent::ended(stream.id, REASON_HLS_WRITER_INIT_FAILED));` 再 `return Err(…)` |
| RTMP :364 / SRT :662（teardown） | `mark_ended` 调用后：`sink.emit(IngestEvent::ended(stream_id, REASON_PUBLISHER_DISCONNECTED));`（SRT 放 `if let Some(id)` 内镜像结构） |

- **恰一性（scope 修正，回复 lifecycle review）**：Started 恰一（DB 强制 + refusal `continue` 结构性保证）。Ended **对进入终路径（publish loop teardown / HLS 回滚 / SRT error-finalize）的已 Started 连接**恰一（回滚 `return Err` 先于 teardown；SRT 三 teardown 路径经 trait impl 汇聚）。**例外（F13）**：RTMP 三条既有缺口路径（握手 drain / post-Started 协商错误 / dup-publish refusal 覆写）可从 Started 直达连接结束而不触发 `mark_ended`——**如实登记为既有缺陷**（接线不新增 Started 证据也不修会话语义），Ended 恰一承诺不覆盖这些例外；AC-a 守卫「无孤儿 Started」钉之。
- **顺序**：Started post-commit（`mark_live` 返回 `Started` 后）；Ended 在 `mark_ended` 调用后（Ok/Err 均发——事件记录 teardown 事实本身，**可先于** durable 行态（F6c）；行态最终由 `mark_live` 下一位 / owner 手工收敛——**无 sweep 愈合机制**（F5 更正））。
- **同步发射**（连接任务内，无跨 task 队列）；`IngestSink::emit` catch_unwind。
- **单一构造面**：`rg "IngestEvent::started|IngestEvent::ended" crates/aero-live-rtmp crates/aero-live-srt` 只命中六锚点行（grep 校验，防 vacuous wiring；**F13 三缺口行不带发射**——负 grep 面，防未来误接）。

### 1.5 aero-audit-priority-drill 扩展（R4）

| 项 | 变更 |
|---|---|
| 常量 | `const LIFECYCLE_OUTBOUND_ACTION: &str = "stream.live.ended";`（**硬编码 + comment-pin，不从叶子派生**——`MODERATION_OUTBOUND_ACTIONS` 同款纪律；lifecycle 叶子 token 是 B5 落地项，落地前晒死拼写并注明 [PROPOSED]）；`LIFECYCLE_ROWS: i64 = 1`；`TOTAL_ROWS = BACKLOG_ROWS + MODERATION_ROWS + LIFECYCLE_ROWS` → **502** |
| 种子 | moderation 行之后追加 1 行 lifecycle：`class='admin'`、`priority=100`（复用 `MODERATION_PRIORITY`）、`status=0`、`attempts=0`、payload `{"event_id":…,"action": LIFECYCLE_OUTBOUND_ACTION}`；`seeded.push` |
| round-1 断言 | lifecycle `delivered_at` 非空 + 新 verdict `drill: lifecycle-in-first-batch: PASS`（D3 **membership** 契约，非 firstness） |
| 总数连锁 | **`drain-502: PASS` / `parity-502: PASS`**（332/350 改名）；moderation 两 PASS（:276/:298）**逐字保留**（harness grep 依赖，E11） |
| **全字面量 churn 清单（AC-d 补，test-acceptance 复核）** | **①** header doc :23/:31（"cannot hold all 501 rows" / "COUNT(status=2) == 501" → 502）；**②** in-lane comments :242（"< 501"）/ :300（"remaining 401"）/ :354（"parity-501"）；**③** seeding println :233-236（"then 1 moderation row" → "+1 lifecycle"）；**④** harness 注释 :448 与 echo :542；**⑤** verdict 行 :332/:350。①-④ 注释/println（不影响 fail-loud），⑤断言（漏改即红）。`aero-eng/tests/audit_provision.rs:459-483` 的 501 是 parser 单元样例，**非** churn，不改 |
| 门 | D8′（LOCK+COUNT+TRUNCATE）与列 probe 零改动；`MAX_ROUNDS=10` 对 502 充足（round1 claimed 100 + 最多 5 drain = 6 ≤ 10）；t11-fail-closed 不受影响（lifecycle 行 status 0 缺席 relay 保持 pending） |
| 事件生产者 pin（durability） | (a) `stream.live.ended` 的最终 producer = **B5 R3 in-tx audit**，**永不可能是 observer**（observer 无 `event_id`、无 outbox 写路径——webhook 语义是 deliver 物化，不是源）；(b) R3 落地后 drill 词表以叶子常量收敛（comment 钉死拼写）；(c) **不入 webhook `events` 集**——kind `stream.live` 相邻，未来误接会与 go-live 的 `(webhook_id, event_id)` dedup 冲突 |
| harness | `test-integration.sh` :448/:542 注释更新（"500 backlog + 1 moderation + 1 lifecycle = 502"）；grep :553/:558 与 `b5_check` :564 **不变**；wrapper（aero-eng）零改动。**新 verdict 由 exit-code 强制**（drill 任一 verdict 失败即 bail → harness 捕获 RC ≠ 0），不再扩展 harness grep——greppable 属性只保留给 `moderation-action-vocabulary`（注释言明这是刻意决定） |

---

## 2. 兼容性约束

| 约束 | 保持方式 |
|---|---|
| `LiveIngest` trait 签名冻结（:134-135） | 两 impl 零改动；sink 走 struct 字段 + builder |
| `RtmpIngest::new()` / `SrtIngest::new()` 零参 | boot/ingest.rs（:60-61/:66-88）、spawn_* 既有测试零改动；`with_sink` 可任意位置插入既有链式 builder |
| Send + Sync | `Arc<dyn Fn(IngestEvent) + Send + Sync>`；`assert_send_sync` 同款断言保持绿 |
| derive 保持 | rtmp `Debug/Default/Clone`、srt `Debug/Clone` 行不动 |
| wire 形零变化 | `{"kind":"started","stream_id":…,"hls_path":…}` / `{"kind":"ended",…}`（R2.2 pin） |
| 内部 crate API 破坏（唯一且最小） | `resolve_stream` / `finalize_session` +1 `&IngestSink`（各仅 1 生产调用点）；`handle_connection` / `forward_session_results` +1 参数（rtmp 内部） |
| `SessionBackend` trait 冻结 | :323-327 零改动；内存测试 backend（isolation_tests.rs）零触碰 |
| 零迁移 / 零新 env / 零新依赖 / 零新 b5-pin 槽 | 0239/0240/0241、trigger、connector（pg.rs/relay/client/outbox.rs)、aero-eng、golive、`StreamEvent` 不动；`serde_json` 已在（E18）；37 槽不动（E17）。**本批不加 stale-live 恢复机制**（F5 是矫正声明，非新功能；不给 `streams.status` 加额外写者） |
| aero-storage 零代码改动 | R3 映射**不落地**（条件契约）；AC-b 以 capability-probe `#[ignore]` 测试驻留（§5 b） |
| 词汇隔离 | `IngestEvent`（进程内 observer）与 `StreamEvent`（live.stream.* Nats 线）不混用；`stream.live.ended` 不入 webhook events（§1.5 pin） |

---

## 3. 失败模式（F1-F16，含 08-09 新增 F12-F16）

| # | 模式 | 处置 |
|---|---|---|
| F1 | observer panic / Err | `IngestSink::emit` catch_unwind + warn 吞掉；连接路径零影响（`Hub::fan_out_raw` best-effort 同款） |
| F2 | `AlreadyLive` / `NotFound` 误发 Started | `started_if_transitioned` 唯一映射点（None 分支）+ RTMP refusal `continue` 结构性排除；AC-a oracle 断言 0 事件 |
| F3 | vacuous wiring（发射点漂移） | 单一构造 + 六锚点 grep + SRT oracle 数事件 |
| F4 | 双 Ended（回滚 + teardown） | 回滚 `return Err` 先于 teardown（E15 互斥）；SRT 三路径经同一 trait impl 汇聚（peer 状态机只触发其一） |
| F5 | `mark_ended` DB 失败 | 事件照发（记录 teardown 事实）。**F5 更正（lifecycle review）**：「行态由 retention/sweep 愈合」**无支撑机制**——全树只有 `stream.rs:265` 与 `stream_category.rs:409` 戳 `streams.status`；retention sweep 只盖消息/ban/points，**不盖 stream 行**（无 stale-live 恢复）。改述：行态由下一位 `mark_live` / owner 手工收敛，观察者事件**非**「状态算收敛」信号。**本批不新增恢复机制**（矫正声明，非新功能） |
| F6 | 事件 at-most-once + crash 窗口 | 流状态行是事实源；B5 落 R3 时 in-tx 行关闭窗口。**F6a** 后果反转：丢观察者 Started 无害（durable 通道已投）；丢观察者 Ended 全损（disconnect-driven end 的唯一通道，RTMP :364 / SRT :662 均无 outbox 可回放）。**F6b** `end_stream`（live.rs:515-527，REST owner）是既有第三个 with-most-once "ended" 生产者（Nats `Status{Ended}`、无 `event_id`），对 observer **零事件**——observer 消费方必须自行 reconcile owner-ended（publish loop 只读 socket/cancel 不轮询 DB，teardown 可能永不发生）。**F6c**（F5 所致）observer `Ended` 可先于 durable 行态——"teardown happened" 而非 "ended committed"。**依赖 pin：B5 接线 observer consumer 时 R3 必须同批落地** |
| F7 | serde 撞名（B5 加 kind 字段） | 编译期硬错误（fail-fast）；线上重复键 JSON → 反序列化 Err；rename 约定 + 双测试 pin（§5 AC-c；测试③ doc comment：通用 serde_json 拒绝，非 rename 检测） |
| F8 | drill 501 字面量漏改 | 任一漏改 → parity/drain 红（fail-loud，非 vacuous）；全量清单 §1.5 |
| F9 | 事件跨 task 乱序 | 同步发射（连接任务内），无跨 task 队列 |
| F10 | rtmp derive 破坏 | `IngestSink` 新类型自带 Debug/Default/Clone；derive 行不动 |
| F11 | `stream_id=None` 时调 finalize | 既有结构保证不 `mark_ended` 不发射；oracle 覆盖 |
| **F12** | **teardown 双 "ended" 事实（D1）** | REST `end_stream`（owner 侧 Nats `Status{Ended}`）+ 后到 teardown `Ended`（含 F5 场景）双发；两者均无 correlation key——**两个通道独立即事实**：observer row = "teardown happened"（无 harness-aware owner-end 关联）；接受为语义（观察者 reconcile 属于 B5 consumer 责任，审计去重以 R3 `event_id` 为锚，不在本批） |
| **F13** | **RTMP Started-without-Ended（三缺口，lifecycle review）** | ① cycle 溢出 drain（:327-314：pipelining 客户端 connect+createStream+publish 同段 → `process_results/forward_session_results` `Ok(false)` → `return Ok(())` 于 :343，loop 永不入，:364 不跑）；② post-Started 协商错误（:544-550：`accept`/`write_all` `?` 传播，连接任务死 → 无 mark_ended；仅 `HlsWriter::new` 失败有 :525 回滚）；③同批重复 publish（:514-527 + :531：第二 publish 被 `AlreadyLive` 拒 → refusal 覆写 `outcome=Disconnect` → step-3 循环尾 `return Ok(())`，无 publish loop 无 mark_ended，还多了个多余 BadName 拒绝）。**SRT 无等价**（Started→Streaming 间无 await；HLS 失败同事务回滚）。**处置**：接线不修会话语义（B5 外单步），但**如实登记 + 守卫**：AC-a 断言「无孤儿 Started」（每个 Started 在有事件序列中必被 Ended 或显式 fail 收尾）+ RTMP 负 grep（三缺口行不带发射） |
| **F14** | **覆盖范围不对称（D2）** | observer 只盖 RTMP/SRT；WHIP 无 `impl LiveIngest`（仍走 `mark_live`），REST end 无 observer 事件——**观察者 ≠ 全量账本**；durable 覆盖全 ingest 面（声明 comment pin） |
| **F15** | **跨通道乱序（D3, HLS-init phantom）** | observer `Ended` 可先于 relay `Live`（HLS 初始化回滚先提交 outbox，relay flush 滞后；该 phantom 进程内唯一 observable 信号 = observer）。reconciler（B5）必须容忍 Ended-before-Live——pin 声明 |
| **F16** | **通道独立使能（D4）** | relay 可由 env 关闭（`AERO__SERVER__STREAM_LIVE_OUTBOX_POLL_MS=0`，boot/background.rs 0-disable）而 observer（带 sink 后）不可——消费方不得假设双通道同时活跃，任一缺席不得 fail-closed |

---

## 4. 迁移步骤（零 DB 迁移；每步可独立 revert 的提交）

> **零迁移纪律**：无新迁移文件（0239 CHECK 值域已容 class 'admin' + priority 100）；build→migrate 规则不触发；迁移计数不动。

1. **core 词汇 + 拆雷**（commit 1）：构造 helper + `started_if_transitioned` + `IngestSink` + reason 常量 + enum doc 增补（R2.1）+ R2.2 三测试 → `cargo test -p aero-live-core` 绿。
2. **RTMP 接线**（commit 2）：字段化 + `with_sink` + `handle_connection` +1 参数 + 三锚点发射 + 单测（reason 拼写、Started 映射）+ **F13 无接线 grep** → `cargo check -p aero-live-rtmp`。
3. **SRT 接线**（commit 3）：字段化 + `with_sink` + `RepoSessionBackend.sink`（:362）+ 两自由函数签名 +1 + 三锚点发射 → `cargo check -p aero-live-srt` + isolation tests 绿。
4. **SRT 端到端 db oracle + AC-b 契约测试**（commit 4）：AC-a oracle（`#[ignore]`, `DATABASE_URL`，含「无孤儿 Started」守卫）+ AC-b capability-probe（aero-storage `mod db_tests`：先 0 行探测（R3 未映射 → early return 绿））。见 §5 a/b。
5. **drill 扩展 502**（commit 5）：常量 + lifecycle 种子 + round-1 断言 + `drill: lifecycle-in-first-batch: PASS` + drain/parity 502；moderation 两 PASS 逐字保留；churn 清单（§1.5）全同步；**in-pin 注释**（R3 是 producer、webhook 词汇隔离）。
6. **harness 注释 + 全链门禁**（commit 6）：test-integration.sh :448/:542 注释更新；跑 `cargo test --workspace --lib`（含 `-- --ignored` + throwaway 库）+ `bash scripts/test-integration.sh` + clippy + cuts + 37 槽。（AC-a/b 与 drill 在 throwaway 库：CREATE→migrate→DROP。）

---

## 5. 可测验收映射（AC-a…e）

| AC（requirements 原句分句） | 可测断言 | 测试位置 / 名称 |
|---|---|---|
| **(a)** a live ingest lifecycle emits 1 Started + 1 Ended | **SRT db oracle**：throwaway 库 seed stream 行（**仅需 owner FK 行**——`streams.owner_id REFERENCES participants`；`room_id` 可空，`has_effective_room_access` 只在 `room_id: Some` 时调用（stream.rs :45-48/:56 门），seed 用 `NewStream{room_id: None, …}` 即可，**不需要**满足 room access）→ `resolve_stream(repo, cfg, key, &capture_sink)` → 恰 1 `Started(stream_id, hls_path)`；同 key 再 resolve → `Err(AlreadyLive)` 且 0 事件；`finalize_session(…)` → 恰 1 `Ended(REASON_PUBLISHER_DISCONNECTED)`；**守卫**：断言 started_count == ended_count == 1（F13 无孤儿） | aero-live-srt 新 `mod db_tests`（`#[ignore = "requires live Postgres"]`） |
| **(b)** and, if mapped, one outbox row per transition | **capability-probe 条件体（test-acceptance 修订）**：`#[ignore]` 驻留；转换后查 `audit_governance_outbox WHERE event_id=$1`：**0 行**（映射未落地）→ early-return **PASS**；**1 行** → 断言全形状（status 0 / class 'admin' / priority 100 / `jsonb_typeof(payload)='object'` / event_id 匹配 + stable）；**>1 行** → fail。回滚注入腿同守卫（0/1/70）。pre-R3（今天）与 post-R3 都绿 | aero-storage `mod db_tests` 邻 `mark_live_ended_outbox_shape` icon，`mark_live_ended_governance_outbox_shape_capability_probe` |
| **(c)** serde round-trip pins rename | ① 真实 enum wire 形 round-trip 恒等（钉 JSON 字符串）；② 镜像调用 `#[serde(rename)]` round-trip 无冲突；③ 真实 enum + 重复键 → `Err(\`duplicate field kind\`)`（doc：通用 serde_json 拒绝，非 rename 检测） | aero-live-core `#[cfg(test)]` 三测试 |
| **(d)** drill lifecycle row | 种子 500/1/1、round-1 claimed==100 两条 priority-100 行 delivered_at 非空、`drill: lifecycle-in-first-batch: PASS`、drain/parity-502、moderation 两 PASS 逐字保留；churn 清单全覆盖（§1.5） | aero-audit-priority-drill |
| **(e)** drill/T-11/37 门禁 | 37 槽零增删（b5-pin `assert_b5_contract_pin` 绿）；`t11-fail-closed` PASS；`moderation-priority-drill` PASS（harness grep ⓘ :553/:558 稳） | test-integration.sh（only comments） |

---

## 6. 协作 & AGENTS 硬规则走查

- **kind-tag 规则（§4.2）**：不换 tag 名（wire 稳定）；rename 约定 + pin；机制写 enum doc（上文）。
- **trait 冻结**：`LiveIngest::run`/`run_until_cancelled`、`SessionBackend`、rtmp/srt `new()` 零改动；sink 走 struct 字段 + builder。
- **房间数据访问**：不涉及（storage 侧 seed FK-only；无 `assert_room_access`）。
- **零迁移纪律**：无新迁移；build→migrate 不触发。
- **字面量纪律**：reason 常量单源；drill 词法硬编码 + 注释（不从叶子派生）；churn 全量清单 §1.5。
- **不引新警告**：clippy `--workspace --all-targets` 零新增；lib.rs 增量小；db_tests 独立文件 < 800 行李。
- **依赖 pin（durability F6b/F6c）**：B5 接线 observer consumer，**必须同一批次落地 R3（in-pg `mark_ended` audit 行）**——分开 = 有消费者时重开 lost-batency 窗口。
- **事件生产者 pin**：`stream.live.ended` 的源 = R3 in-tx 行，**observer 仅是观察者**（无 `event_id`、无 outbox 写路径）；不入 webhook `events` 集（与 `(webhook_id,event_id)` dedup 避免冲突）。

## 7. Metrics

- **vacuous wiring**（最高）：单一构造 + 六锚点 grep + SRT oracle 数事件。
- **serde 机制认知差**：写 un-renamed 镜像负例会编译卡住；用 ③（重复键 JSON）微复现。
- **drill 501→502 连锁**：churn 清单全量；parity/drain 断言 fail-loud。
- **RTMP 无 DB 驱动路径**（用户定义超范围）：不在 oracle；用 grep 面 + 单测守卫；RTMP e2e 在 staging seam（真实 ffmpeg/OBS，AGENTS §4.5）。
- **「if mapped」误读为必达**：AC-b 用 0-行 early-return 显式「未映射 = 无断言」。
- **F13 缺口误读为 keep-forever 状态**：已登记 + AC 守卫；观察者语义不影响既有会话。

## 8. Sequencing（6 commits，每步可独立 revert）

1. core fuse（构造面 + sink + 文档 + R2.2 测试）
2. RTMP wiring（字段 + builder + 参数 + 三锚点 + 负向 grep）
3. SRT wiring（≤字段 + trait impl 注入 :362 + 两自由函数签名 + 三锚点）
4. DB oracle + AC-b probe（throwaway 库全链）
5. drill 502（常量/种子/verdict/512 连锁/清单）
6. harness comment + 门禁（integration.sh / 四脚本 / 37 槽）

每步 `cargo check` 干净；db 依赖步在 throwaway 库验证后 DROP（AGENTS §4.3 活验证纪律）。

---

## 9. 终检（post-amendment re-verify，2026-08-09）

- **零-EDL / 零迁移**：本批零迁移、零 DDL、零 env、零 boot、零依赖（E6：0239 CHECK 已容 class 'admin'); no new table; drill 502 不加迁移 → **hold**。
- **37-slot pin**：零 slot 增删；`t11-fail-closed`、`moderation-priority-drill` 证据线不变；lifecycle 行只在 drill 的 throwaway 库 → **hold**。
- **drill 502 链**：TOTAL_ROWS=502 由常量组合（500/1/1）；rounds ≤10；parity 集合含 lifecycle_id；moderation 两 PASS 逐字保留实现→ **hold**。
- **恰一性面**：Started 恰一；Ended 恰一（终路径）+ F13 例外如实登记（不延伸）+ AC 哨兵 = 与 lifecycle-review 结论一致 → **hold**；F6 observer 语义（at-most-once、owner 不可 reconcile 观察者、Ended-before-committed）如实登记 → **hold**。
- **AC 映射一致**：AC-a→commit 4 oracle；AC-b→commit 4 probe（0/1/70+），pre/post-R3 双绿；AC-c→commit 1；AC-d→commit 5 churn；AC-e→commit 6 门禁 → 与五 AC 全映射无遗漏 → **hold**。
- **一致性**：八 finding 全部采纳（§0.4 表 ↔ §1/§3/§5 一一对应）；6-commit 与 §0.4 不违；全文无 stale-live 恢复机制添置（F5 纯矫正）；drill 502 / zero-EDL / 37-slot 均未触 → **hold**。

<!-- 复核关闭：本文档已包含 2026-08-09 三份 adversarial review 的全部 finding；终态 = 六锚点 + 述评 frame（F12-F16 作为观察者管理噪声真实）+ AC-a…e 映射 + 零迁移/零 slot/502 链。 -->