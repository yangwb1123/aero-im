# Requirements Spec — aero-live-core：IngestEvent 接线为直播生命周期事件生产者（RTMP/SRT）+ tag="kind" serde 冲突拆雷（B5 加 variant 前）

- **Module (analysis root)**: `crates/aero-live-core`（叶子 crate）；交付面含 `crates/aero-live-rtmp` / `crates/aero-live-srt`（接线）、`crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（drill 扩展）
- **Direction**: "Wire IngestEvent into RtmpIngest/SrtIngest as the live lifecycle audit producer and defuse the tag=\"kind\" serde collision before B5 adds variants"（value 8 / risk_reduction 8 / effort 4 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-live-core-src-ef6d180c.json`（direction #2，index 1；2026-08-09 重生成，direction 内容与先前 `crates-aero-live-core-d5bccc8d.json` 逐字一致）
- **Campaign**: `aero-im-b5`（B5 审计批次；contract anchor `scripts/b5-pin.sh` 37-slot pin）
- **Sibling specs（同批次，边界协调）**: `2026-08-08-aero-im-core-b5-1-in-tx-audit-governance-enqueue.req.md` / `2026-08-08-aero-im-core-b5-1-in-tx-audit-outbox-producer-seam.req.md`（governance outbox Rust 显式写路 = R3 条件映射的落点族）、`2026-08-08-aero-cli-b5-3-moderation-priority-drill.req.md`（priority drill 历史）、`2026-08-08-aero-eng-b5-1-in-tx-coverage-probe.req.md`
- **Status**: Requirements（下述证据 2026-08-09 全部经源码 grep 二次核对——行号为核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-09（复核）／2026-08-08（初核）

> ⚠️ **direction 事实性更正（§1.1 逐条）**：① 「observers can hang notifications, bus publishing, or metrics off this」引文**不在** lib.rs:113（该行在 `LiveError` Debug impl 内）——实为 `IngestEvent` 的 doc :74-75；`LiveIngest` trait 在 :133-134，其 doc 无 observer 承诺。② 「live→ended 是 natural 1:1 row」的现有 in-tx 快照只覆盖 `idle|ended → live`（`mark_live` 单 CTE）；`mark_ended`（stream.rs:264）今天**不写任何 outbox 行**——live→ended 的 in-tx 行尚不存在，若映射需新 producer（R3 条件契约）。③ 0239 trigger 仅 token-keyed `message.moderated`——lifecycle 行不能骑 trigger，只能走 Rust 显式写路（与 sibling im-core spec 的 outbox enqueue seam 同族）；验收原文以 "(and, **if mapped**, ...)" 条件化——本 spec 将 outbox 映射定为**条件契约**（§4 R3），IngestEvent 接线（R1）与 drill 扩展（R4）为必达。④ trait doc 中的 "WHIP" 是愿望：全仓 `impl LiveIngest for` 仅 2 处（rtmp :262 / srt :310）——本 direction 只钉 RTMP/SRT，与验收一致。

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-live-core/src/lib.rs:79`（IngestEvent，`#[serde(tag="kind")]`，grep 全仓仅定义） | ✅ **Verified（行号精确）**。:78 `#[serde(tag = "kind", rename_all = "snake_case")]`，:79 `pub enum IngestEvent {`，:81-92 `Started{stream_id: Ulid (ulid_as_uuid), hls_path: PathBuf}` / `Ended{stream_id: Ulid, reason: String}`。`rg -n "IngestEvent" crates/`（含 server/whip/webrtc/srt/rtmp）**唯一命中 = aero-live-core 定义本身**——零构造点，确认死代码 |
| E2 | lib.rs:113 trait doc 承诺 observers | ⚠️ **引文漂移（符号锚点命中）**。引文在 :74-75（`IngestEvent` 的 doc：「These are intentionally lightweight — observers can hang notifications, bus publishing, or metrics off this…」）；`LiveIngest` trait :133-134（`async fn run(&self, repo: StreamRepo, cfg: Arc<LiveStreamConfig>)`），doc :131-132 只承诺「object-safe trait every ingest backend (RTMP, SRT, WHIP) implements」——**observer 承诺挂在 IngestEvent 上，恰是 dead vocabulary 的 doc** |
| E3 | rtmp :262 / srt :310 的 `LiveIngest` impl 不产 IngestEvent | ✅ **Verified（行号精确）**。`impl LiveIngest for RtmpIngest` :262 / `async fn run` :263；`impl LiveIngest for SrtIngest` :310 / :311。全仓 `impl LiveIngest for` 仅此 2 处。生命周期锚点（本 direction 的接线面）：RTMP `mark_live` :486（PublishStreamRequested 臂）、`mark_ended` :364（run_publish_loop 后无条件）+ :525（HLS init 失败回滚）；SRT `mark_live` :1098（`resolve_stream`）、`mark_ended` :662（`finalize_session`，established Streaming peer 唯一 teardown）+ :1117（HLS init 回滚） |
| E4 | `crates/aero-storage/src/stream_go_live_outbox.rs`（同生命周期转换已 in-tx 快照） | ✅ **Verified**。`StreamRepo::mark_live`（stream.rs:179-262）单 data-modifying CTE：`UPDATE streams SET status='live' … RETURNING` → `INSERT INTO stream_go_live_outbox … FROM transitioned`，返回 `GoLiveTransition{outbox_id, event_id}`——**stable event_id 每转换一个**（「1:1 event_id」先例）。`mark_ended`（:264-266）仅 `UPDATE streams SET status='ended', ended_at=NOW()`——**零 outbox 行**。模块 doc 明言「`mark_live` creates these immutable rows atomically with the idle\|ended → live state transition」 |
| E5 | `migrations/0239_audit_governance_outbox.sql:30-31`（status 0/1/2/3 CHECK） | ✅ **Verified（行号精确）**。:30 `status INTEGER NOT NULL DEFAULT 0`，:31 `CHECK (status IN (0, 1, 2, 3))`。配套：connector 状态机 `STATUS_ENQUEUED=0..STATUS_DEAD=3`（aero-audit-connector/src/pg.rs:34-37，值现单源自叶子 `aero_common::model::audit::OutboxStatus`——符号锚点不变，取值来源漂移已登记）、claim `ORDER BY candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id`（pg.rs:116——**B5-3 的 priority DESC 已落地**）、`class IN ('admin','message','room')` + `priority > 0` + `delivery_mode IN ('push')`（0239 :32-38）——**class 'admin' + priority 100 落 CHECK 值域内，条件映射零 DDL**。trigger `aero_enqueue_governance_audit()` 仅 `NEW.action = 'message.moderated'` 入队（其余 `RETURN NEW` fail-open pass-through） |
| E6 | AGENTS §4.2 kind-tag 规则 + common/src/model/ rename 先例 | ✅ **Verified**。AGENTS.md :151 逐字：「tagged-enum `kind` 标签撞名陷阱：总线 enum 用 `tag="kind"`，variant 内不得再有名为 `kind` 的字段（否则 serde `duplicate field kind` panic）」；先例 `#[serde(rename = "notify_kind")]`（common/src/model/event.rs:77，tag 在 :27）、`#[serde(rename = "call_kind")]`（media.rs:189/:236/:268）。**实证复现**（本 spec 验证时编译运行）：`{"kind":"envelope","stream_id":7,"kind":"audit"}` 反序列化 → `Err("duplicate field \`kind\` at line 1 column 39")`；字段 `#[serde(rename = "envelope_kind")]` 后 round-trip 成功，线上形 `{"kind":"envelope","stream_id":7,"envelope_kind":"audit"}`——rename 是唯一合规解法 |
| E7 | `scripts/truth-check.sh` 盲区（pub enum 不报） | ✅ **Verified**。truth-check 只查两类：① orphan = 未被引用文件（`IngestEvent` 所在文件被引用，不触发）；② unwired builder = 零调用 `with_*` 方法（:151-224，`IngestEvent` 非 builder）。**pub enum 编译进叶子 crate 即永久隐身**——「completed」claim 可落在零调用代码上而不红，正是本 direction 的动机 |
| E8 | `aero-audit-priority-drill`（待扩展） | ✅ **Verified**。`crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`：`BACKLOG_ROWS=500`（priority 10 / class 'message'，先种）+ `MODERATION_ROWS=1`（priority 100 / class 'admin' / action=叶子 `MODERATION_OUTBOUND_ACTION`，后种）；`TOTAL_ROWS=501`、`BATCH_SIZE=100`、`MAX_ROUNDS=10` → round-1 断言 moderation 行在 top-100 claimed 集（D3：membership 契约，非 firstness）+ `drill: moderation-in-first-batch: PASS`（:276）+ `drill: moderation-action-vocabulary: PASS`（:298）+ `drill: drain-501: PASS`（:332）+ `drill: parity-501: PASS`（:350，event_id 集合 parity）；exit 2 SKIP（0239 表/列缺）、D8′ 门（LOCK+COUNT+TRUNCATE，`AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1` 才放行）；词表 pin 硬编码 `MODERATION_OUTBOUND_ACTIONS` 对（**不**从叶子派生）。harness：`scripts/test-integration.sh` grep `priority: landed` + `drill: moderation-action-vocabulary: PASS` 后 `b5_check "moderation-priority-drill" "PASS"`（wrapper = aero-eng `audit-provision-check --priority` = `audit_provision.rs::run_priority` :669，直 spawn drill 透传 exit code :752；REFUSED 门 :715-717） |
| E9 | （补充）boot 装配 = sink seam 的构造先例 | ✅ **Verified**。`crates/aero-server/src/bin/boot/ingest.rs`：:60-61 `RtmpIngest::new().run_until_cancelled(repo, live_cfg, cancel)`；:80-88 `SrtIngest::new()` → 条件 `with_passphrase(pass.into_bytes())`（builder 方法先例）→ `run_until_cancelled`。boot 调 `run_until_cancelled`（非 trait `run`）——**sink 放 struct 字段 + builder 方法可零改动 boot** |
| E10 | （补充）NATS 线词汇 vs 进程内词汇 | ✅ **Verified**。`StreamEvent::Status{stream_id, status: StreamStatus}`（common/src/live.rs:118-131，tag="kind"；`StreamStatus{Idle,Live,Ended}` media.rs:16-22）→ `live.stream.*`，golive relay `bus_payload` 已发 `Status{status: StreamStatus::Live}`（server/src/stream_live_outbox.rs:213/:219）。**`StreamEvent`（NATS 线）≠ `IngestEvent`（进程内 observer 词汇）——不混用**；本 direction 只动后者 |

### 1.1 对 direction 陈述的勘误/钉化（evidence-backed）

- **更正① 引文锚点**：observer 承诺在 `IngestEvent` doc :74-75，非 :113。语义不变（dead vocabulary 的 doc 承诺了 observer 挂点），仅锚点修正。
- **更正② 转换方向**：现有 in-tx 快照 = `idle|ended → live`（golive 通知用途，stable `event_id` 先例）；`live → ended` 无 in-tx 行。验收的 outbox 映射若落地，`mark_ended` 需同款单语句模式（R3）。
- **更正③ 映射条件化**：验收原文 "(and, if mapped, one outbox row per transition, 1:1 event_id)"——outbox 行是**条件项**（B5 落地时生效），本 direction 钉其形状（R3）而非强制落地；IngestEvent 接线与 drill 扩展是**必达项**。
- **行号漂移登记（2026-08-09 复核）**：lib.rs:113（引文）→ :74-75；其余符号锚点全部命中（:79/:78、:262/:263、:310/:311、:486/:364/:525、:1098/:662/:1117、0239:30-31、stream.rs:179-262/:264-266）。复核新增登记：pg.rs STATUS_* 值单源自叶子 `OutboxStatus`（常量名不变）；`bus_payload` :213（原 :217）；`run_priority` :669 / drill spawn :752（原 :715-740）；boot/ingest.rs :60-61/:80-88（原 :60/:80-85）；drill PASS 行名含 `moderation-in-first-batch`（:276）。**工作树状态**：`aero-audit-connector/`、`audit_provision.rs`、`migrations/0239-0241` 为未提交工作树文件（B5 批次在途）——锚点按工作树核对；`aero-live-core/rtmp/srt` 自 08-03/08-05 提交后未动，行号稳定。

## 2. Verified current state（缺口盘点）

```
IngestEvent（aero-live-core，唯一事件类型）  ── 定义 :78-92 + doc :73-75（observer 承诺）
                                              └─ 全仓零构造点（E1）→ dead vocabulary
                                                  truth-check 不报（E7）→ 可被误报「completed」

生命周期转换（两后端都已集中，接线面现成）：
  RTMP  handle_connection（:486 mark_live → :364 mark_ended；:525 HLS 回滚）
  SRT   resolve_stream（:1098 mark_live → finalize_session :662 mark_ended；:1117 回滚）
  └─ mark_live 返回 MarkLiveOutcome::{Started(GoLiveTransition),AlreadyLive,NotFound}（E4）
     mark_ended 无条件回写 status='ended'（无 outbox 行）

serde 撞名雷（E6）：tag="kind" + 未来 B5 variant 带 kind 字段 → 反序列化 panic
  「duplicate field `kind`」已实证；rename 先例 call_kind/notify_kind

治理 outbox（0239，E5）：status 0/1/2/3、class admin/message/room、priority DESC claim
  trigger 仅 message.moderated → lifecycle 行只能走 Rust 显式写路（条件映射 R3）
  priority drill 覆盖 moderation 单 lane，lifecycle lane 无任何执行证明（R4）

缺口（本 direction 关闭）：
  a) IngestEvent 零构造 → 生命周期无进程内事件词汇（B5 的 audit/notify/metrics observer 无挂点）
  b) tag="kind" 雷在 B5 加 variant 前未拆（无 round-trip pin）
  c) priority drill 无 lifecycle lane 行（「lifecycle 行按 admin/100 先于 backlog 被 claim」无执行证据）
```

## 3. Scope

**In scope**：
- **R1** `IngestEvent` 接线：RTMP + SRT 在 E3 钉死的生命周期锚点各发恰一事件；sink observer seam（builder 设置、默认 no-op、boot 零改动）；aero-live-core 提供共享构造 helper（单一构造面）。
- **R2** `tag="kind"` 拆雷：doc 钉 AGENTS §4.2 规则 + serde round-trip pin（真实 enum 线上形 + 带 `kind` 字段的 variant rename 后 round-trip + 撞名负例）。
- **R3** 条件契约：lifecycle → `audit_governance_outbox` 映射的**形状**（若 B5 落地：同事务恰 1 行、1:1 event_id、status 0、class 'admin' + priority 100）；本 direction 不强制落地，但验收 oracle 按「若映射」门控。
- **R4** `aero-audit-priority-drill` 扩展：lifecycle 行（class 'admin' / priority 100）先于 backlog 被 claim；moderation 优先语义在 T-11/37 gate 下保持。

**Out of scope**：
- **零迁移**：0239/0240/0241 DDL、trigger（`aero_enqueue_governance_audit` 保持 `message.moderated` 唯一 SQL 生产者）——零改动。
- **connector 零改动**：pg.rs claim SQL（`priority DESC`）、relay 状态机、`aero-audit-t11-drill`、aero-eng wrapper（`audit_provision.rs`）——零改动；drill 只改 bin 自身 + harness 注释。
- **`LiveIngest` trait 签名零改动**（:133-134 冻结）；`run_until_cancelled`/`run` 签名零改动——sink 放 struct。
- **NATS/golive 路径零改动**：`stream_go_live_outbox` 语义、relay、`StreamEvent::Status`（E10 词汇区分）、`stream_live_outbox.rs`——零改动（审计行是加性、独立 event_id，不与 golive 行混淆）。
- **WHIP/webrtc**：无 `LiveIngest` impl（E3），不接线。
- 无新 b5-pin slot、无新 AERO_* env、无 web/ 改动、无 `aero-ai`/`aero-common` governance lane 改动（lifecycle token 是 B5 落地项，R3 仅钉形状）。

## 4. Requirements

### R1 — IngestEvent 接线：每生命周期恰一 Started + 一 Ended（RTMP + SRT）

**发射规则（行为需求）**：

| 规则 | 语义 |
|---|---|
| R1.1 Started | 当且仅当 `mark_live` 返回 `MarkLiveOutcome::Started(_)`（转换已提交）时发恰一 `IngestEvent::Started{stream_id, hls_path}`；`hls_path` = `hls_path_for(&cfg.hls_dir, stream.id)`（两后端均已计算）。`AlreadyLive` / `NotFound` / 非法 key 拒绝路径**零事件** |
| R1.2 Ended | 对本连接已发 Started 的 stream，在 `mark_ended` 调用点发恰一 `IngestEvent::Ended{stream_id, reason}`；覆盖全部三处：RTMP :364（publish loop 后无条件 teardown）、RTMP :525 与 SRT :1117（HLS init 失败回滚——转换已提交，Start→End 是诚实记录）、SRT :662（`finalize_session`）。`reason` 人类可读、不含 publisher 凭据（LiveError 同款纪律）；默认拼写 `"publisher disconnected"` / `"hls writer init failed"`（测试钉死，[PROPOSED] 可换拼写但须同步测试） |
| R1.3 顺序 | Started 在 `mark_live` 返回 `Started` **之后**发射（post-commit）；Ended 在 `mark_ended` 调用后发射（无论其 Ok/Err——行态可被后续 sweep 愈合，事件记录 teardown 事实本身） |
| R1.4 单一构造面 | 两后端一律经 aero-live-core 的构造 helper（如 `IngestEvent::started(stream_id, hls_path)` / `IngestEvent::ended(stream_id, reason)`）构造；**E3 六处锚点是唯一发射点**（grep-verifiable：`IngestEvent::started`/`::ended` 在 rtmp/srt 只出现于钉死行）——防「sink 存在但发射点漂移」的 vacuous wiring |
| R1.5 sink seam | `RtmpIngest`/`SrtIngest` 各增 observer sink（`Arc<dyn Fn(IngestEvent) + Send + Sync>` 或等价，设计自决），builder 方法设置（`with_passphrase` 先例 E9），**默认 no-op**——boot（ingest.rs:60/:80）与 trait 签名零改动；B5 的 audit/notify/metrics observer 挂在 seam 上（兑现 :74-75 doc 承诺） |
| R1.6 事件归属 | 事件在连接任务内同步发射（不跨 task 乱序）；`IngestEvent` 保持 `Debug + Clone + Serialize + Deserialize` 派生（R2 不改派生） |

### R2 — tag="kind" 拆雷 + serde round-trip pin

- **R2.1** `IngestEvent` 保留 `tag = "kind"`（总线 wire 约定，不换 tag 名）；enum doc 增补 AGENTS §4.2 规则原文 + rename 约定（`call_kind`/`notify_kind` 先例）：**B5 新增 envelope variant 若带 `kind` 字段，必须 `#[serde(rename = ...)]`**。
- **R2.2** aero-live-core 新增 serde 测试（`serde_json` 已是依赖，零 Cargo.toml 变更）：
  - 真实 enum round-trip：`Started`/`Ended` 序列化 → 反序列化恒等，**钉线上形** `{"kind":"started",…}` / `{"kind":"ended",…}`（tag 拼写 = `rename_all = "snake_case"` 的 variant 名，防未来 tag 漂移）；
  - 约定 pin（镜像 enum，测试本地）：`#[serde(tag="kind")]` + 带 `kind` 字段的 variant，字段 `#[serde(rename = "…_kind")]` 后 round-trip 无 panic（钉 rename 约定本身——生产 enum 当前无 `kind` 字段，测试用镜像 enum 钉惯例，防未来加字段时踩雷）；
  - 撞名负例：未 rename 的撞名 JSON（`{"kind":…,"kind":…}`）反序列化必须 `Err`（`duplicate field kind`）——把 AGENTS 规则变成可执行断言。

> **amendment 注记（2026-08-08，设计核验后——回链 design §0.1 / §5 AC-c；2026-08-09 复核确认 workspace serde = 1.0.228，≥1.0.28）**：R2.2 撞名负例以**真实 enum + 重复键 JSON**（`{"kind":"started","kind":"started",…}`）实现。原 spec 的「un-renamed 镜像 enum」负例在 **serde ≥1.0.28**（2018-03 起，PR #1170 引入编译期检查 `variant field name \`kind\` conflicts with internal tag`）下**不可编译**，无法编进测试二进制；编译期拒绝正是 rename 约定的真正强制点（比任何运行时测试更强，fail-loud in CI）。因此：负例测试主体改为真实 enum + 重复键 JSON（钉 serde_json **通用**重复键 wire 拒绝——任何重复键都 Err，不限于 tag/rename）；测试 ③ 的 doc comment 必须写明该角色（不检测未来 un-renamed `kind` 字段）；rename 约定由编译期 + 镜像 round-trip（测试 ②）pin。

### R3 — 条件契约：lifecycle → audit_governance_outbox 映射（若 B5 落地）

验收原文以 "(and, if mapped, one outbox row per transition, 1:1 event_id)" 条件化——本 direction 钉**形状**，落地属 B5（sibling im-core spec 的 outbox enqueue seam 族）。若 B5 加 lifecycle 映射：

| 契约 | 内容 |
|---|---|
| R3.1 同事务 | 每转换恰 1 outbox 行，与转换同一 PG 事务（镜像 `mark_live` 单 CTE 模式 E4；`mark_ended` 需同款单语句：`UPDATE streams SET status='ended' … RETURNING` → `INSERT INTO audit_governance_outbox … FROM transitioned`） |
| R3.2 1:1 event_id | outbox `event_id` = 该转换自己的 stable id（审计行 event_id），**与** `stream_go_live_outbox.event_id` 区分（两个 outbox、两个 event_id，互不混淆——R1.6 词汇区分） |
| R3.3 行形状 | `status = 0`（DDL 默认，CHECK 0/1/2/3 内）、`class = 'admin'`、`priority = 100`（= `GOVERNANCE_PRIORITY_MODERATION` lane，验收原句）、`payload` JSONB object（`jsonb_typeof='object'` CHECK 内）、`delivery_mode` 默认 'push'——**零 DDL**（0239 CHECK 值域已含，E5） |
| R3.4 生产者 | 走 Rust 显式写路（trigger 仅 `message.moderated`，E5）；`ON CONFLICT (event_id) DO NOTHING` 幂等；**无条件入队**（不 consult `snaplink_commercial_runtime`，与 sibling im-core spec 同款门语义） |
| R3.5 失败语义 | 写失败 = 整事务回滚（0 行逃逸）；IngestEvent 发射与 outbox 行解耦（R1 事件先于映射存在，映射只是 observer 之一） |

**若 B5 不落地映射**：R1 的接线与 R4 的 drill 扩展独立成立（drill 自种 lifecycle 行，不依赖 producer）。

### R4 — aero-audit-priority-drill 扩展：lifecycle 行（class 'admin'，priority 100）先于 backlog

- **R4.1 种子**：在 moderation 行之后（更晚 `available_at`）追加 1 lifecycle 行：`class='admin'`、`priority=100`（= `MODERATION_PRIORITY` 常量值）、payload `action` = 新 drill 本地常量 `LIFECYCLE_OUTBOUND_ACTION = "stream.live.ended"`——**硬编码 + comment-pin，不从叶子派生**（drill 既有纪律：`MODERATION_OUTBOUND_ACTIONS` 对 :33-35 先例；lifecycle 的叶子 token 是 B5 落地项，落地前 drill 用钉死拼写并注明 [PROPOSED]）。
- **R4.2 断言**：round-1 `claimed == 100` 且 **moderation 行与 lifecycle 行都在 top-100 claimed 集**（各读 `delivered_at` 非空；D3 membership 契约，非 firstness）；新增 `drill: lifecycle-in-first-batch: PASS`；drain 与 parity 总数 501 → **502**（`drain-502` / `parity-502` 行名同步）；`drill: moderation-in-first-batch: PASS` 与 `drill: moderation-action-vocabulary: PASS` **逐字保留**（harness grep 依赖，E8）。
- **R4.3 门与 T-11**：D8′ 门（LOCK+COUNT+TRUNCATE）零改动（fixture 行数无关）；lifecycle 行与 moderation 行同 T-11 形状（status 0 / attempts 0 / relay 缺席保持 pending——t11-fail-closed slot 语义不受影响）；`MAX_ROUNDS=10` 对 502 行充足（6 round）。
- **R4.4 harness 注释同步**：drill doc header 与 `scripts/test-integration.sh` 的 "500 backlog + 1 moderation" 注释/echo 更新为 502（grep 断言 `priority: landed` + `drill: moderation-action-vocabulary: PASS` **不变**）；wrapper（aero-eng）零改动。

### R5 — harness/pin 保持 + 零改动清单

| 槽位（scripts/b5-pin.sh:40-41 等） | 保持方式 |
|---|---|
| `t11-fail-closed` | t11 drill 零改动；lifecycle 行 T-11 形状（R4.3） |
| `moderation-priority-drill` | wrapper 透传 drill exit code（E8），PASS 行不变；drill 扩展后仍 exit 0 |
| 37-slot pin（:87-95 恰 37 槽） | **零新 slot、零删除**；`B5-CHECK` verdict 行协议不变 |

- 零改动清单：`migrations/0239/0240/0241`、trigger、`aero-audit-connector/src/{pg,relay,client,outbox}.rs`、`aero-audit-t11-drill.rs`、`aero-eng/audit_provision.rs`、`aero-server`（boot/ingest.rs、stream_live_outbox.rs、golive_bot.rs）、`aero-common`（`StreamEvent`/`StreamStatus`/governance 常量）、`web/`。
- aero-live-core：零新依赖（`serde_json` 已依赖）；lib.rs 176 行 + 构造 helper/测试 ≤ ~120 行，远低于 800 WARN（file-size-check.sh 唯一权威）。

## 5. Testable acceptance mapping（direction acceptance 原句保留 + 可测试化）

> **Acceptance 原文**：Unit/db tests: a live ingest lifecycle emits exactly one IngestEvent::Started + one Ended (and, if mapped, one outbox row per transition, 1:1 event_id). Serde round-trip test pins the kind-rename convention (a variant carrying a `kind` field round-trips without duplicate-field panic). Extend aero-audit-priority-drill with a lifecycle row (class admin, priority 100) claimed before backlog — moderation priority preserved under the T-11/37 gate.

| AC（原句分句） | 可测断言（测试形式） | 位置 |
|---|---|---|
| **(a)** a live ingest lifecycle emits exactly one `IngestEvent::Started` + one `Ended` | **SRT 端到端 db oracle**（`#[ignore]` + `DATABASE_URL` 门控，throwaway 库）：`StreamRepo` 种 stream 行 → `resolve_stream`（:1085）带 capture sink → 断言恰 1 `Started`（`MarkLiveOutcome::Started` 路径）；再 `resolve_stream` 同 key → `AlreadyLive` Err 且 **0 新事件**；`finalize_session` → 恰 1 `Ended`。**RTMP**：发射决策走同一核心映射 helper（R1.4 单一构造面），单元测试钉 mapping（Started iff `Started` outcome；`AlreadyLive`/`NotFound`/非法 key → None）；发射点由 grep 校验（R1.4 六锚点）。**单元测试**（aero-live-core）：构造 helper 恒等 + `hls_path` 取自 `hls_path_for` + `reason` 拼写钉死 | aero-live-srt（新 db_tests，`--ignored`）+ aero-live-core/rtmp 单测 |
| **(b)** and, if mapped, one outbox row per transition, 1:1 event_id | **条件门控 db test**（aero-storage，`#[ignore]`，随 R3 落地启用）：`mark_live` → 恰 1 `audit_governance_outbox` 行（status 0 / class 'admin' / priority 100 / payload object / `event_id` = 该转换 stable id）；`mark_ended` → 恰 1 行；转换回滚（注入失败）→ 0 行（无 orphan）。映射未落地时该测试以 `#[ignore]` 驻留不跑——验收的 "if mapped" 语义如实编码 | aero-storage（stream db_tests 区，scheduled_stream.rs:436+ 先例） |
| **(c)** Serde round-trip test pins the kind-rename convention (a variant carrying a `kind` field round-trips without duplicate-field panic) | R2.2 三测：真实 enum 线上形 round-trip；镜像 enum `kind` 字段 rename 后 round-trip 无 panic；**真实 enum + 重复键 JSON** → `Err("duplicate field \`kind\`")`（负例把 AGENTS §4.2 变成可执行断言；amendment：un-renamed 镜像 enum 在 serde ≥1.0.28 下不可编译，测试 ③ doc comment 写明钉的是 serde_json 通用重复键拒绝——见 R2.2 amendment 注记 + design §0.1） | aero-live-core `#[cfg(test)]`（serde_json 已依赖） |
| **(d)** Extend aero-audit-priority-drill with a lifecycle row (class admin, priority 100) claimed before backlog | R4：种子 500 backlog + 1 moderation + 1 lifecycle（全部先种 backlog、后种两条 100，顺序证据只来自 priority）；round-1 top-100 含**两**条 priority-100 行（各自 `delivered_at` 非空）+ `drill: lifecycle-in-first-batch: PASS`；drain 502 / parity 502；`drill: moderation-action-vocabulary: PASS` 逐字保留 | aero-audit-connector/bin/aero-audit-priority-drill.rs |
| **(e)** moderation priority preserved under the T-11/37 gate | `scripts/b5-pin.sh` 37 slot pin 保持（零增删，`assert_b5_contract_pin` 绿）；`t11-fail-closed` 槽位 PASS（t11 drill 零改动 + lifecycle 行 T-11 形状）；`moderation-priority-drill` 槽位 PASS（wrapper 透传，harness grep 行不变）；验证 = `bash scripts/test-integration.sh` 全链（throwaway DB） | scripts/test-integration.sh（既有段，仅注释更新）+ scripts/b5-pin.sh（零改动） |

## 6. Coordination & hard rules（AGENTS §4）

- **kind-tag 规则（§4.2）是本 direction 的 R2 本体**：不换 tag 名（wire 稳定），rename 约定 + round-trip pin；新 variant 复刻 `call_kind`/`notify_kind` 先例。
- **trait 签名冻结**：`LiveIngest::run` / `run_until_cancelled` 签名零改动；sink 经 struct 字段 + builder 方法（`with_passphrase` 先例 E9）——boot/ingest.rs 零改动（默认 no-op）。
- **零迁移纪律**：无新迁移（0239 CHECK 值域已容 class 'admin'/priority 100，E5）；build→migrate 规则不触发；迁移计数不动。
- **字面量纪律**：`reason` 拼写由单测钉死；drill 的 `LIFECYCLE_OUTBOUND_ACTION` 硬编码 + comment-pin（不派生，叶子 token 落地后由 B5 收敛——drill 既有 `MODERATION_OUTBOUND_ACTIONS` 对同款纪律）。
- **文件尺寸**：aero-live-core lib.rs（176 行）增量 ≤ ~120 行 < 800 WARN；srt 新 db_tests 落 `tests.rs` 旁新文件（`db_tests.rs`，`mod db_tests;` 惯例）或独立文件，不超限。
- **共享文件集成**：`IngestEvent` 当前全仓唯一属主 = aero-live-core（E1）——无并行 agent 冲突面；若 sibling B5 spec 同时落地 outbox seam，手接 `AuditGovernanceOutboxRepo`（R3 落点）时以本 spec R3.1-R3.5 形状为准。
- **提交前必过**：`cargo check --workspace` · `cargo test --workspace --lib`（全绿底线；新 db oracle 加 `-- --ignored` + `DATABASE_URL`）· `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· throwaway 库跑 priority drill（502 全 drain）。
- **活验证**：全新一次性库建库 → `aero-cli migrate` → 跑 srt db oracle（`--ignored`）+ priority drill → DROP；drill 的 D8′ 门要求 `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1`（throwaway 专属）。

## 7. Risks

- **Vacuous wiring（最高风险，direction 点名）**：sink 存在但发射点漂移/漏点 → 又回到「completed claim on unwired code」。缓解：R1.4 单一构造面（helper 在 core，两后端只能调 helper）+ 六锚点 grep 校验 + SRT 端到端 db oracle（AC-a）真驱动 `resolve_stream`/`finalize_session` 数事件。
- **drill 总数变更的连锁**：501 → 502 涉及 bin 内全部字面量（round-1 断言不受影响——batch 100 < 502；drain 轮数 6 ≤ MAX_ROUNDS 10）；`test-integration.sh` 的 D8′ 负例 fixture（1 行 status=2）与 drill 内容无关，零影响；漏改任一 501 → parity 断言红（fail-loud，非 vacuous）。
- **RTMP 无 DB 驱动路径**：RTMP 连接测试需真实 TCP+RTMP 握手——本 direction 以核心映射单测 + 锚点 grep 覆盖；完整 RTMP e2e 属 staging seam（AGENTS §4.5，真实 ffmpeg/OBS），不在验收内。
- **「if mapped」的条件语义被误读为必达**：R3 明确条件化（验收原句），AC-b 以 `#[ignore]` 驻留；若 B5 落地映射，形状已钉（零 DDL、1:1 event_id、同事务）——不产生「半落地」行。
- **reason 拼写 churn**：[PROPOSED] 拼写由单测钉死，换拼写须同步测试——Fail-open 语义（事件发射失败不阻断发布路径？）——**R1.5 明确 sink 为 no-op 默认 + 同步发射**：observer panic/Err 不得影响连接路径（sink 内部捕获，与 `Hub::fan_out_raw` 的 best-effort 纪律一致），此纪律写入 R1.5 实现注记。
- **B5 落地并行冲突**：sibling im-core spec 若先落 `AuditGovernanceOutboxRepo`，R3 落点手接（§6 共享文件规则）；IngestEvent 无并行属主。

## 8. Sequencing

1. **core 词汇 + 拆雷（R2）**：构造 helper（R1.4）+ enum doc 增补 + 三个 serde 测试 → `cargo test -p aero-live-core` 绿。
2. **sink seam（R1.5）**：两后端 struct 字段 + builder 方法 + 默认 no-op；boot/ingest.rs 确认零改动编译。
3. **六锚点接线（R1.1-R1.4）**：RTMP :486/:364/:525、SRT :1098/:662/:1117 → 各发恰一事件；grep 校验单一构造面。
4. **SRT 端到端 db oracle（AC-a）**：`#[ignore]` + `DATABASE_URL`，throwaway 库跑绿（1 Started / 0 on AlreadyLive / 1 Ended）。
5. **drill 扩展（R4）**：bin 字面量 502 + lifecycle 断言 + PASS 行；throwaway 库直跑 drill 全绿（含 D8′ 门负例）。
6. **全链门禁**：`cargo test --workspace --lib`（含 `-- --ignored`）· `bash scripts/test-integration.sh`（t11-fail-closed / moderation-priority-drill PASS）· clippy + 三脚本 0 违规 · b5-pin 37 槽保持。
