# Design — aero-live-core：IngestEvent 接线为直播生命周期事件生产者（RTMP/SRT）+ tag="kind" serde 冲突拆雷

- **Direction**: "Wire IngestEvent into RtmpIngest/SrtIngest as the live lifecycle audit producer and defuse the tag=\"kind\" serde collision before B5 adds variants"
- **Module (analysis root)**: `crates/aero-live-core`（叶子 crate）；交付面含 `crates/aero-live-rtmp` / `crates/aero-live-srt`（接线）、`crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（drill 扩展）
- **Requirements**: `docs/requirements/2026-08-08-aero-live-core-ingestevent-lifecycle-producer.req.md`
- **Campaign**: `aero-im-b5`；contract anchor `scripts/b5-pin.sh` 37-slot pin
- **Status**: Design（证据核验完成 2026-08-08；**serde 机制更正已并入 §0/§3/§5——边界 = serde ≥1.0.28（2018-03 起，PR #1170 引入），非保守臆造的 ≥1.0.197；requirements 的 R2.2 负例测试形态据此修正，amendment 注记已落 requirements 文档并回链本 §0.1**；行号为核对时锚点，可能漂移——**文件/符号**才是稳定锚点）

---

## 0. 证据核验（evidence = untrusted claims，逐条对源码 + 实证）

**全部 17 项符号锚点命中**；三处登记更正（其中一处为**实质性机制更正**，改变 R2 测试形态）。逐条：

| 证据 | 结论 | 核验要点 |
|---|---|---|
| E1 `lib.rs:79` `IngestEvent` + `tag="kind"`、全仓死代码 | ✅ | `:78 #[serde(tag="kind", rename_all="snake_case")]`，`:79 pub enum IngestEvent{Started{stream_id:Ulid,hls_path:PathBuf},Ended{stream_id:Ulid,reason:String}}`；`rg -n IngestEvent crates/` 唯一命中 = 定义本身 |
| E2 `:113` observer 引文 | ✅ 登记漂移 | 引文实为 `:74-75`（IngestEvent doc）；`LiveIngest` trait `:133-134`；`:113` 在 `LiveError` Debug impl 内（证据已自行更正，属实） |
| E3 rtmp `:262` / srt `:310` 的 impl 不产事件 | ✅ | `impl LiveIngest for` 全仓仅此 2 处（whip/webrtc 无）；六锚点：RTMP `mark_live :486`（PublishStreamRequested 臂）、`mark_ended :364`（publish loop 后无条件）+ `:525`（HLS init 回滚）；SRT `mark_live :1098`（`resolve_stream`）、`mark_ended :662`（`finalize_session`）+ `:1117`（HLS init 回滚）——**全部实证核对行号** |
| E4 outbox 1:1 event_id / mark_ended 无行 | ✅ | `StreamRepo::mark_live`（stream.rs:179-262）单 data-modifying CTE（`UPDATE streams … RETURNING` → `INSERT INTO stream_go_live_outbox … FROM transitioned`）→ `GoLiveTransition{outbox_id,event_id}`；`mark_ended`（**stream.rs:264-266**，证据摘要把文件归属写得含糊，requirements 正文引用正确）仅 `UPDATE … status='ended'`，零 outbox 行 |
| E5 0239 status CHECK / class admin / priority 100 | ✅ | `:30-31` `status INTEGER DEFAULT 0 CHECK (status IN (0,1,2,3))`（行号精确）；`:32-37` `class IN ('admin','message','room')` + `priority > 0` + `delivery_mode IN ('push')` + `jsonb_typeof(payload)='object'` → 条件映射零 DDL |
| E6 kind-tag 规则 + rename 先例 | ✅ 机制更正（见 §0.1） | AGENTS §4.2 原文 + `notify_kind`（event.rs:77，tag :27）/ `call_kind`（media.rs:189/:236/:268）先例命中 |
| E7 truth-check 盲区 | ✅ | truth-check.sh 只查 orphan 文件 + `fn with_xxx(...self...` 零调用 builder（:151-224）；pub enum 编译进叶子 crate 即隐身 |
| E8 drill 结构 | ✅ | `BACKLOG_ROWS=500`（:79）+ 1 moderation（:216-231，后种）；batch 100（round-1 `COUNT(status=2)==100`）；exit-2 SKIP（:114/:167）；D8′ 门（:93/:119 + aero-eng wrapper `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1` 门，audit_provision.rs:715-740 直 spawn 透传 exit code）；`MAX_ROUNDS=10`（:56）；`drain-501`（:332）/`parity-501`（:350）；`drill: moderation-in-first-batch: PASS`（:276）/`drill: moderation-action-vocabulary: PASS`（:298）；词表硬编码对 `MODERATION_OUTBOUND_ACTIONS`（:77） |
| E9 boot 装配 = sink seam 先例 | ✅ | boot/ingest.rs：`RtmpIngest::new().run_until_cancelled(repo, live_cfg, cancel)`、`SrtIngest::new().with_passphrase(...)`；`with_identity`/`with_max_bandwidth`/`with_passphrase` builder 先例齐备（srt :198/:212/:230）；`run_until_cancelled` 非 trait `run` |
| E10 NATS 线词汇 ≠ 进程内词汇 | ✅ | `StreamEvent::Status`（common/src/live.rs:119-131，`StreamStatus{Idle,Live,Ended}`）→ `live.stream.*` golive relay；`IngestEvent` 零 wire 使用——不混用 |
| E11 37-slot pin | ✅ | b5-pin.sh：37 槽（15 executed + 22 [PROPOSED]），`assert_b5_contract_pin`（:84-120）恰 37、无重复、verdict 行协议；`t11-fail-closed` / `moderation-priority-drill` 槽位在列 |
| E12 serde_json 已依赖 | ✅ | aero-live-core Cargo.toml:20 `serde_json.workspace = true` → 零 Cargo.toml 变更 |
| E13 db_tests 先例 | ✅ 归属微调 | `StreamRepo::mark_live` db test 在 **stream.rs:488**（`mod db_tests` :348）；scheduled_stream.rs:432 `mod db_tests`（ScheduledStreamRepo）仅作模块形态先例——AC-b 落点取 stream.rs:348 区（§5） |
| E14 harness grep | ✅ | test-integration.sh:545-556：`grep -q "priority: landed"` + `grep -q "drill: moderation-action-vocabulary: PASS"` 后 `b5_check "moderation-priority-drill" "PASS"` |
| E15 六锚点互斥性 | ✅ 设计输入 | RTMP :525 回滚在 `Err(error) => {…; return Err(…)}` 内（先于 :364 执行即 return）；SRT :1117 同理（`return Err(LiveError::Internal(…))`）——回滚 Ended 与 teardown Ended 每条连接**结构性互斥** |
| E16 SRT 线程路径 | ✅ 设计输入 | `run_listener(ingest,…)`（:360）构造 `RepoSessionBackend{repo,cfg}`（:372-375）→ `handle_datagram(ingest,…)`（:486）→ `backend.resolve/finalize`；`resolve_stream`（:1085，pub 自由函数，生产调用点仅 :337 一处）与 `finalize_session`（:651，pub 自由函数，调用点 :339 + drain :395 + SHUTDOWN :495）——sink 经 backend 字段携带，trait `SessionBackend`（:327-331）**零改动**（内存测试 backend 不产事件） |
| E17 RTMP 线程路径 | ✅ 设计输入 | 三锚点全在 `handle_connection`（:270，私有自由函数）内；spawn 点 :243-252 捕获 `repo,cfg,connection_cancel`——sink 参数 +1；`RtmpIngest` 为 `#[derive(Debug, Default, Clone)]` 单元 struct（:171-174）——字段化须保三个 derive（新类型 `IngestSink` 自带三者实现即可） |

### 0.1 ⚠️ 实质性机制更正（本设计新发现，改变 R2 测试形态）

证据/requirements 声称撞名在**反序列化运行时**报 `Err("duplicate field \`kind\`")` 并「实证复现」。对工作区钉死版本 serde **1.0.228** 实证（/tmp/serde-check，offline 直编）。**版本边界（已独立复核 serde 仓库 tag）**：该编译期检查由 PR #1170（commit `a799ea17`「Disallow variant field names to conflict with tag of internally-tagged enum」，2018-03-07）引入，随 **serde 1.0.28**（2018-03-08）发布——v1.0.27 的 `serde_derive_internals/src/check.rs` 无此检查，v1.0.28 起连续存在（抽验 1.0.35 / 1.0.50（移入 `serde_derive/src/internals/`）/ 1.0.60 / 1.0.197 / 1.0.228 / 1.0.229 均在）。本文档与 enum doc 一律以**真边界 ≥1.0.28** 表述；≥1.0.197 系臆造保守值（全仓无出处），弃用。

1. **编译期硬错误**：`#[serde(tag="kind")]` + variant 字段名 `kind`（未 rename）→ serde_derive **拒绝编译**：`error: variant field name \`kind\` conflicts with internal tag`（serde_derive-1.0.228/src/internals/check.rs:300-330 `check_internal_tag_field_name_conflict`）。**不是运行时 panic**——B5 未来加 `kind` 字段会直接编译失败（fail-fast，比「运行时 panic」更安全），但 requirements R2.2 的「未 rename 撞名 JSON 反序列化必须 Err」若以**镜像 enum** 实现则**无法编译**（测试本体编译不过）。
2. **运行时 Err 仍可复现**，但机制是**线上重复键**：对真实 enum 喂 `{"kind":"started","kind":"started","stream_id":7,"hls_path":"x"}` → `Err("duplicate field \`kind\` at line 1 column 24")`——负例改为「真实 enum + 重复键 JSON」即可执行（serde_json TaggedContentVisitor 对重复键报错，实证）。
3. **rename 修复 round-trip 实证**：`{"kind":"audit","stream_id":7,"audit_kind":"audit"}` → 恒等（镜像 enum + `#[serde(rename="audit_kind")]`）。

**设计裁决**：R2.2 负例测试 = 真实 enum + 重复键 JSON（运行时 `Err(duplicate field kind)` pin）；un-renamed 镜像 enum 的编译期拒绝**不可测**（无法编进测试二进制），其保护由 serde_derive 本身提供（记录在 enum doc + 本设计 §3 F7）。AGENTS §4.2「否则 serde `duplicate field kind` panic」的机制描述对 **serde ≥1.0.28 起**已过时（升级为编译期硬错误 `variant field name \`kind\` conflicts with internal tag`）——规范内容（必须 rename）不变。**AGENTS.md 延后修正（deferral 条件已登记，供下批次执行，防腐烂）**：本批次零改动清单不含 AGENTS.md，但延后附带三条条件——① enum doc 机制更正**必须本批次落**（真边界 ≥1.0.28 + PR #1170，见 §1.1）——已落；② requirements R2.2 amendment 注记**必须本批次落**（真实 enum + dup-key，回链本 §0.1）——已落；③ 下批次凡触碰 AGENTS.md，把 §4.2 机制句折入一行描述性修正：「serde ≥1.0.28 编译期硬错误；线上重复键 JSON → 运行时 `duplicate field kind` Err」——仅描述性、零规范变更、零评审风险。此过时不会造成静默危害：任何按旧措辞行动者都会在 CI 得到编译错误（un-renamed 字段不编译 / 照 R2.2 旧形态写负例测试本体不编译），fail-loud。

### 0.2 核验中发现的额外设计输入（证据未覆盖、本设计采纳）

1. `resolve_stream`/`finalize_session` 生产调用点各仅 1 处（RepoSessionBackend::resolve :337 / ::finalize :339），签名 +1 参数的成本 = 2 调用点 + 新 db oracle；isolation_tests.rs 用内存 backend，**零触碰**。
2. `RtmpIngest` 的 `assert_send_sync::<RtmpIngest>()`（:797）与 srt tests.rs:132 同款——sink 为 `Arc<dyn Fn + Send + Sync>`，Send+Sync 保持，两断言继续绿。
3. RTMP `:521` 已算 `dir = hls_path_for(&cfg.hls_dir, stream.id)`（mark_live 之后）——Started 的 `hls_path` 直接取同表达式（纯函数，无副作用）。
4. root `Cargo.toml` 无 `panic = "abort"`（profile 未设）→ `catch_unwind` 有效；workspace lint `unsafe_code = "forbid"` 不影响（`catch_unwind`/`AssertUnwindSafe` 均 safe）。

---

## 1. API 变更

### 1.1 aero-live-core（新公共 API，全部增量）

```rust
// lib.rs（enum 之后追加）

/// Reason spellings — single source of truth, pinned by unit tests.
pub const REASON_PUBLISHER_DISCONNECTED: &str = "publisher disconnected";
pub const REASON_HLS_WRITER_INIT_FAILED: &str = "hls writer init failed";

impl IngestEvent {
    /// Single construction surface (R1.4): both backends MUST build events
    /// through these helpers only.
    pub fn started(stream_id: Ulid, hls_path: PathBuf) -> Self {
        Self::Started { stream_id, hls_path }
    }
    pub fn ended(stream_id: Ulid, reason: impl Into<String>) -> Self {
        Self::Ended { stream_id, reason: reason.into() }
    }
    /// Map a `mark_live` outcome to the Started event. `Some` iff the
    /// transition actually committed (`MarkLiveOutcome::Started`); the
    /// `AlreadyLive` / `NotFound` non-transitions yield `None` — zero events.
    pub fn started_if_transitioned(
        outcome: &aero_storage::MarkLiveOutcome,
        stream_id: Ulid,
        hls_path: PathBuf,
    ) -> Option<Self> {
        match outcome {
            MarkLiveOutcome::Started(_) => Some(Self::started(stream_id, hls_path)),
            MarkLiveOutcome::AlreadyLive | MarkLiveOutcome::NotFound => None,
        }
    }
}

/// Best-effort observer sink for lifecycle events.
///
/// Default = no-op; emission is fail-open: an observer panic is caught and
/// logged, never propagated into the ingest/connection path (same discipline
/// as `Hub::fan_out_raw` best-effort fan-out).
#[derive(Clone)]
pub struct IngestSink(Arc<dyn Fn(IngestEvent) + Send + Sync>);

impl IngestSink {
    pub fn new<F>(f: F) -> Self
    where F: Fn(IngestEvent) + Send + Sync + 'static { Self(Arc::new(f)) }
    pub fn emit(&self, event: IngestEvent) {
        let f = self.0.clone();
        if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || f(event))) {
            tracing::warn!(panic = ?panic, "ingest event observer panicked; swallowing");
        }
    }
}
impl Debug for IngestSink { /* "IngestSink(noop)" or "IngestSink(closure)" */ }
impl Default for IngestSink { /* no-op sink */ }
```

- enum doc（R2.1）增补：AGENTS §4.2 规则（规范内容：variant 字段不得名 `kind`，必须 `#[serde(rename = …_kind)]`，先例 `call_kind`/`notify_kind`）+ **serde ≥1.0.28 机制更正**（2018-03 起，PR #1170 / commit `a799ea17` 引入：撞名 = 编译期硬错误 `variant field name \`kind\` conflicts with internal tag`；线上重复键 JSON = 运行时 `Err("duplicate field \`kind\`")`）。精度注（照 check 源码）：检查仅作用于 internal-tag enum 的 struct-style variant 字段，且 `skip_serializing` + `skip_deserializing` **双跳过**的字段豁免——「variant 字段不得名 `kind`」对非跳过、非 rename 字段精确成立。
- 派生集不变：`Debug + Clone + Serialize + Deserialize`（wire 形零变化）。

### 1.2 aero-live-rtmp

```rust
#[derive(Debug, Default, Clone)]
pub struct RtmpIngest { sink: IngestSink }   // 单元 struct → 字段化；derive 全保留

impl RtmpIngest {
    pub fn new() -> Self { Self { sink: IngestSink::default() } }
    /// Observer sink for lifecycle events (default no-op). Boot stays zero-change.
    #[must_use]
    pub fn with_sink(mut self, sink: IngestSink) -> Self { self.sink = sink; self }
}
```

- `handle_connection(socket, repo, cfg, cancel, sink: &IngestSink)`（私有自由函数，:270）+ spawn 点（:243-252）捕获 `self.sink.clone()`——**`LiveIngest::run` / `run_until_cancelled` 签名零改动**。
- 三锚点发射（§1.4）。

### 1.3 aero-live-srt

```rust
#[derive(Debug, Clone)]
pub struct SrtIngest { …, sink: IngestSink }   // new()/with_identity() 均置 IngestSink::default()

impl SrtIngest {
    pub fn with_sink(mut self, sink: IngestSink) -> Self { … }
}
```

- `RepoSessionBackend { repo, cfg, sink }`（:372 构造处 `sink: ingest.sink.clone()`）；trait `SessionBackend`（:327-331）**零改动**——内存测试 backend 不产事件。
- `resolve_stream(repo, cfg, stream_key, sink: &IngestSink)`（:1085）+ `finalize_session(session, repo, stream_id, sink: &IngestSink)`（:651）——pub 自由函数签名 +1 参数；生产调用点 :337/:339 + drain :395 + SHUTDOWN :495 同步。
- 三锚点发射（§1.4）。

### 1.4 发射规则（两后端共用，R1.1-R1.6 落地形态）

| 锚点 | 代码形态 |
|---|---|
| RTMP :486 / SRT :1098（mark_live 后） | `if let Some(ev) = IngestEvent::started_if_transitioned(&transition, stream.id, hls_path_for(&cfg.hls_dir, stream.id)) { sink.emit(ev); }`——放 refusal 分支之后（RTMP）/ match 之后（SRT），**唯一可达路径 = Started** |
| RTMP :525 / SRT :1117（HLS init 回滚） | `mark_ended` 调用后（无论 Ok/Err）：`sink.emit(IngestEvent::ended(stream.id, REASON_HLS_WRITER_INIT_FAILED));` |
| RTMP :364 / SRT :662（teardown） | `mark_ended` 调用后：`sink.emit(IngestEvent::ended(stream_id, REASON_PUBLISHER_DISCONNECTED));`（SRT 在 `if let Some(id)` 内，镜像既有结构） |

- **恰一性**：Started 恰一（DB 强制转换 + 唯一 fall-through）；Ended 每条**已 Started 连接**恰一（回滚 return 先于 teardown，E15 结构性互斥）。
- **顺序**：Started post-commit（mark_live 返回后）；Ended 在 mark_ended 调用后（Ok/Err 均发——事件记录 teardown 事实，行态由 sweep 愈合）。
- **同步发射**（连接任务内，不跨 task 乱序）；`IngestSink::emit` 内部 catch_unwind，observer 异常零影响连接路径。
- **单一构造面**：`rg "IngestEvent::started|IngestEvent::ended" crates/aero-live-rtmp crates/aero-live-srt` 只命中上述六行（grep 校验，防 vacuous wiring）。

## 2. 兼容性约束

| 约束 | 保持方式 |
|---|---|
| `LiveIngest` trait 签名冻结（:133-134） | 两 impl（:262/:310）签名不变；sink 走 struct 字段 + builder |
| `RtmpIngest::new()` / `SrtIngest::new()` 零参不变 | boot/ingest.rs、`spawn_rtmp_ingest*`、既有测试零改动编译；`with_identity`/`with_max_bandwidth`/`with_passphrase` 链式调用兼容（`with_sink` 可任意位置插入） |
| `RtmpIngest`/`SrtIngest` Send+Sync | `Arc<dyn Fn + Send + Sync>`；assert_send_sync 测试继续绿 |
| derive 保持 | `Debug`/`Default`/`Clone`（rtmp）、`Debug`/`Clone`（srt）——`IngestSink` 手工实现三者 |
| wire 形零变化 | `{"kind":"started","stream_id":…,"hls_path":…}` / `{"kind":"ended",…}`（R2 pin） |
| 内部 crate API 破坏（唯一） | `resolve_stream`/`finalize_session` 签名 +1 参数——aero-live-srt 内部 pub 自由函数，生产调用点共 3 处 + 新 oracle，无外部消费方 |
| 零迁移 / 零 env / 零 boot / 零新依赖 / 零 b5-pin 槽 | 0239-0241、trigger、connector、aero-eng、golive、`StreamEvent` 全部不动；`serde_json` 已依赖；37 槽不动 |
| aero-storage 零改动（本批次） | R3 映射不落地；AC-b 以 `#[ignore]` 契约测试驻留 |
| NATS/golive 词汇隔离 | `IngestEvent`（进程内 observer）与 `StreamEvent`（live.stream.* 线）不混用；审计行加性、独立 event_id |

## 3. 失败模式

| # | 模式 | 处置 |
|---|---|---|
| F1 | observer panic / Err | `IngestSink::emit` catch_unwind + warn 吞掉；连接路径零影响（R1.5 实现注记；root 无 panic=abort，catch_unwind 有效） |
| F2 | `AlreadyLive`/`NotFound` 误发 Started | `started_if_transitioned` 唯一映射点（None 分支），结构上不可能；AC-a oracle 断言 0 事件 |
| F3 | vacuous wiring（sink 在但发射点漂移） | 六锚点 grep 校验 + 单一构造面 + SRT 端到端 oracle 真驱动数事件 |
| F4 | 双 Ended（回滚 + teardown 都发） | 回滚路径 `return Err` 先于 teardown（E15 结构性互斥）；oracle 覆盖 teardown 路径，回滚路径由单测钉 reason 拼写 |
| F5 | `mark_ended` DB 失败 | 事件照发（记录 teardown 事实；行态由 retention/sweep 愈合）——设计如此，测试钉「Err 不影响发射」 |
| F6 | crash 窗口（mark_live 提交后、emit 前进程死亡） | 事件 at-most-once；流状态行是事实源；B5 落地 R3 时 in-tx 行关闭该窗口——文档化，非本批次缺陷 |
| F7 | serde 撞名（B5 加 `kind` 字段） | **serde ≥1.0.28 编译期硬错误**（2018-03 起 PR #1170 引入；fail-fast，比运行时 panic 更安全）；线上重复键 JSON → `Err("duplicate field \`kind\`")` → 反序列化失败（nack）；rename 约定 + 双测试 pin（§5 AC-c；测试 ③ doc comment 写明钉的是 serde_json **通用**重复键拒绝，非 rename 属性） |
| F8 | drill 501 字面量漏改 | 任一漏改 → parity/drain 断言红（fail-loud，非 vacuous）；502 = 2 条 100 + 500 条 10，round 数 6 ≤ MAX_ROUNDS 10 |
| F9 | 事件跨 task 乱序 | 同步发射（连接任务内），无跨 task 队列——R1.6 |
| F10 | rtmp derive 破坏（单元 struct 字段化） | `IngestSink` 新类型自带 Debug/Default/Clone 实现，`#[derive]` 行不动 |

## 4. 迁移步骤（无 DB 迁移；每步可独立 revert 的提交）

1. **core 词汇 + 拆雷**（commit 1）：构造 helper + `started_if_transitioned` + `IngestSink` + reason 常量 + enum doc 增补 + R2.2 三测试 + 构造/mapping 单测 → `cargo test -p aero-live-core` 绿。
2. **RTMP 接线**（commit 2）：字段化 + `with_sink` + `handle_connection` 参数 + 三锚点发射 + 单测（reason 拼写、Started 映射）→ `cargo check -p aero-live-rtmp`。
3. **SRT 接线**（commit 3）：字段化 + `with_sink` + `RepoSessionBackend.sink` + `resolve_stream`/`finalize_session` 签名 + 三锚点发射 → `cargo check -p aero-live-srt` + isolation tests 绿（内存 backend 零触碰）。
4. **SRT 端到端 db oracle**（commit 4）：`#[ignore]` + `DATABASE_URL`（throwaway 库），见 §5 AC-a。
5. **drill 扩展 502**（commit 5）：`LIFECYCLE_OUTBOUND_ACTION` 常量 + lifecycle 种子行 + round-1 membership 断言 + `drill: lifecycle-in-first-batch: PASS` + drain/parity 502；`drill: moderation-in-first-batch: PASS` / `drill: moderation-action-vocabulary: PASS` 逐字保留；throwaway 库直跑 drill 全绿（含 D8′ 门负例）。
6. **harness 注释 + 全链门禁**（commit 6）：test-integration.sh 的 "500 backlog + 1 moderation" 注释/echo → 502；跑 `cargo test --workspace --lib`（含 `-- --ignored`）+ `bash scripts/test-integration.sh` + clippy + `scripts/{truth-check,file-size-check,web-check}.sh` + b5-pin 37 槽。

## 5. 可测验收映射（AC-a…e，测试名 = 落库锚点）

| AC（requirements 原句分句） | 可测断言 | 测试位置 / 名称 |
|---|---|---|
| **(a)** exactly one Started + one Ended | SRT 端到端 db oracle：种 stream 行 → `resolve_stream(…, &sink)` 捕获 → 恰 1 `Started{stream_id, hls_path}`；同 key 再 `resolve_stream` → `AlreadyLive` Err 且 **0 新事件**；`finalize_session(…, Some(id), &sink)` → 恰 1 `Ended{reason: "publisher disconnected"}`。RTMP 半：`IngestEvent::started_if_transitioned` 单测（Started→Some / AlreadyLive/NotFound→None）+ 构造 helper 恒等 + `hls_path` = `hls_path_for` + reason 常量拼写 | aero-live-srt `#[cfg(test)]` 新 `db_tests` 模块（`mod db_tests;` 惯例，`#[ignore = "requires live Postgres"]`）+ aero-live-core/rtmp 单测 |
| **(b)** if mapped, one outbox row per transition, 1:1 event_id | 条件契约 db test：`mark_live` → 恰 1 `audit_governance_outbox` 行（status 0 / class 'admin' / priority 100 / payload object / event_id = 转换 stable id）；`mark_ended` → 恰 1 行；注入失败回滚 → 0 行。**映射未落地前 `#[ignore]` 驻留**（doc 注明「B5 落地 R3 时启用」，验收 "if mapped" 如实编码） | aero-storage `mod db_tests`（**stream.rs:348 区**，紧邻 `mark_live_is_atomic_idempotent_and_snapshot_survives_stream_delete` :488）`mark_live_ended_governance_outbox_shape` |
| **(c)** serde kind-rename convention pin | 三测试：① 真实 enum 线上形 round-trip 恒等（钉 `{"kind":"started",…}`/`{"kind":"ended",…}`）；② 镜像 enum `#[serde(rename="…_kind")]` 后 round-trip 无 panic（rename 约定的**直接** pin——约定本体由 serde_derive ≥1.0.28 编译期强制，运行时只 pin 镜像示范防未来加字段踩雷）；③ **真实 enum + 重复键 JSON**（`{"kind":"started","kind":"started",…}`）→ `Err(duplicate field kind)`（§0.1 机制更正：un-renamed 镜像 enum 不可编译，负例改线上重复键形态）。**测试 ③ doc comment 必须写明**：钉的是 **serde_json 通用重复键 wire 拒绝**（任何重复键都 `Err`——dup tag 键、dup variant 字段键、plain struct dup 键一律如此；与 rename 约定无关——约定废弃该测试仍绿，它**不**检测未来 un-renamed `kind` 字段）；rename 约定的真正检测 = serde_derive 编译期拒绝（fail-loud in CI），运行时由测试 ② 镜像 round-trip pin | aero-live-core `#[cfg(test)]`（serde_json 已依赖）`ingest_event_wire_shape_round_trips` / `kind_field_rename_round_trips` / `duplicate_kind_key_json_is_rejected` |
| **(d)** drill lifecycle row claimed before backlog | 种子 500 backlog + 1 moderation + 1 lifecycle（两条 100 后种）；round-1 `claimed==100` 且**两**条 priority-100 行 `delivered_at` 非空；`drill: lifecycle-in-first-batch: PASS` 新行；drain-502 / parity-502；`drill: moderation-action-vocabulary: PASS` 逐字保留 | `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs` |
| **(e)** moderation priority preserved under T-11/37 gate | `scripts/b5-pin.sh` 37 槽零增删（`assert_b5_contract_pin` 绿）；`t11-fail-closed` 槽 PASS（t11 drill 零改动，lifecycle 行 T-11 形状：status 0 / attempts 0 / relay 缺席保持 pending）；`moderation-priority-drill` 槽 PASS（wrapper 透传 drill exit code，harness grep 行不变） | `scripts/test-integration.sh`（仅注释更新）+ `scripts/b5-pin.sh`（零改动） |

## 6. Coordination & hard rules（AGENTS §4）

- **kind-tag 规则（§4.2）**：不换 tag 名；rename 约定 + 双 pin（wire 重复键 Err + rename round-trip）；机制描述按 **serde ≥1.0.28（PR #1170，2018-03 起）** 更正（编译期硬错误）写入 enum doc。
- **trait 签名冻结**：`LiveIngest::run` / `run_until_cancelled` 零改动；sink 走 struct 字段 + `with_sink`（`with_passphrase` 先例）。
- **零迁移纪律**：无新迁移（0239 CHECK 值域已容）；build→migrate 规则不触发。
- **字面量纪律**：reason 常量单源（core 定义，单测钉死）；drill `LIFECYCLE_OUTBOUND_ACTION` 硬编码 + comment-pin（不派生，B5 落地后收敛——`MODERATION_OUTBOUND_ACTIONS` 同款纪律）。
- **文件尺寸**：aero-live-core lib.rs 增量 ≤ ~130 行 < 800 WARN（file-size-check.sh 唯一权威）；srt db_tests 独立文件不超限。
- **共享文件集成**：`IngestEvent` 全仓唯一属主 = aero-live-core（E1）——无并行冲突面；若 sibling B5 spec 同时落 outbox seam，R3 落点手接时以 requirements R3.1-R3.5 形状为准。
- **提交前必过**：`cargo check --workspace` · `cargo test --workspace --lib`（含 `-- --ignored` + `DATABASE_URL`）· clippy 无新警告 · 三脚本 0 违规 · throwaway 库跑 priority drill（502 全 drain）。

## 7. Risks

- **vacuous wiring**（最高风险）：sink 在但发射点漂移 → 单一构造面 + 六锚点 grep + SRT oracle 真驱动（数事件）。
- **serde 机制认知差**：若实现者照 requirements R2.2 原样写 un-renamed 镜像 enum 负例 → 编译失败卡住。本设计 §0.1 已更正测试形态（重复键 JSON），实现按此执行；测试 ③ 的 doc comment 照 §5 AC-c 写明「通用 serde_json 重复键拒绝」角色，避免后人误读为 rename 约定的运行时检测。
- **drill 总数连锁**：501→502 字面量全量同步（bin + harness 注释）；漏改 → parity 红（fail-loud）。
- **RTMP 无 DB 驱动路径**：映射单测 + 锚点 grep 覆盖；完整 RTMP e2e 属 staging seam（真实 ffmpeg/OBS），不在验收内。
- **「if mapped」误读为必达**：R3 明确条件化；AC-b `#[ignore]` 驻留；B5 落地时形状已钉（零 DDL、1:1 event_id、同事务）。
- **B5 并行冲突**：IngestEvent 无并行属主；outbox seam 落点手接（§6）。

## 8. Sequencing

1. core 词汇 + 拆雷（commit 1）→ 2. RTMP 接线（commit 2）→ 3. SRT 接线（commit 3）→ 4. SRT db oracle（commit 4）→ 5. drill 502（commit 5）→ 6. 全链门禁（commit 6）。每步后 `cargo check` 干净；db oracle 与 drill 在 throwaway 库验证后 DROP。
