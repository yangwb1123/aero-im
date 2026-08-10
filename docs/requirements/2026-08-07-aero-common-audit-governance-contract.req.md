# Requirements Spec — B5 共享审计治理域契约入 aero-common：action 分类（AuditClass/AuditPriority/AuditOutboxStatus 0..3/DeliveryMode）+ 规范 token 常量 + cc/claim 结构（iss/aud/scope/sub）+ AuditDeliveryId

- **Module (analysis root)**: `crates/aero-common` — 全系统共享叶子（`aero-storage` 的 workspace 依赖仅 `aero-common` 一项，E9 验证）；本 direction 交付物 = 新文件 `crates/aero-common/src/model/audit.rs` + `ids.rs` 新增 `AuditDeliveryId` + `model/mod.rs`/`lib.rs` re-export，**纯类型 + 单元测试，零 DB、零新第三方依赖**（aero-common `Cargo.toml` 已含 serde/serde_json/serde_with）
- **Direction**: "Shared audit-governance domain contract in aero-common (action taxonomy + class/priority/status-0..3 enums + delivery ID)"（value 9 / risk_reduction 8 / effort 4 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-common-4afe6237.json`（direction #1）
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml`）；in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（15 行门摘要；294 行全文与 v2 契约三文档在仓外，[PROPOSED]）；门禁 G6 = "37/37、T-11、moderation 优先级"（`docs/campaigns/implementation-gate.md:78`）
- **Sibling specs（同契约不同切片，本 direction 只供类型、不实现语义）**: `docs/requirements/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.req.md`（status 0/1/2/3 = pending/claimed/delivered/dead 已钉）、`docs/requirements/2026-08-06-aero-ai-moderation-governance-outbox.req.md` + `crates/aero-ai/src/governance.rs`（`message.moderated` → admin 类 + 优先级 100 + status 0 已落地）、`docs/requirements/2026-08-06-aero-audit-connector.req.md`（B5-2 connector，claim 校验 iss/aud/scope/sub）
- **Status**: Requirements（下述证据全部经源码 grep 核对）
- **Verification date**: 2026-08-07。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-common/src/ids.rs:121-122` — AuditId 存在、无 AuditDeliveryId | ✅ **Verified**。`define_id!`（ids.rs:12）定义 `AuditId` 在 **:122**（精确命中），`:126` 是 `NotificationId`。`grep -rn "AuditDeliveryId" crates/` → **0 命中**——delivery ID 类型全仓不存在（v1 用 TEXT `delivery_id = 'audit:'\|\|NEW.id`，E5）。`define_id!` 派生 `Clone/Copy/PartialEq/Eq/PartialOrd/Ord/Hash/Serialize/Deserialize` + `#[serde(transparent)]`（ULID 字符串线格式），含 `new/from_ulid/as_ulid/to_uuid/from_uuid/nil` |
| E2 | `crates/aero-common/src/error.rs` — 无 422、无 terminal 概念 | ✅ **Verified**。`status_code()` 在 :66：404/401/403/409/400/429/502/500——**无 422**、无 terminal/retryable 分类。本 direction **不改 error.rs**（terminal 分类是 sibling direction #3 / B5-2 connector 的活），只供 B5-2 依赖的纯类型 |
| E3 | `crates/aero-storage/src/audit.rs:113-171` — `append`/`append_in_tx` 自由 `&str` action | ✅ **Verified**。文件恰 **1049 行**（契约锚点精确命中）。`append` :113、`append_in_tx` :127、共享 `append_on` :138-171：`action: &str` 自由字符串，直接 bind 进 `audit_events.action`（0007 DDL：`action TEXT NOT NULL`）。文档 :23 定义 token 形态 `"member.add"`。**无任何共享 token 类型** |
| E4 | `migrations/0235_snaplink_commercial_control_plane.sql:161-187` — 无 status 枚举、lease 制 | ✅ **Verified**。`snaplink_delivery_outbox` DDL :161-184：`delivery_id/destination/workspace_id/tenant_id/client_id/source_system/idempotency_key/payload/occurred_at/available_at/attempts/claim_token/lease_expires_at/delivered_at/last_error/created_at` + claim-state CHECK（:179-181）+ `UNIQUE (destination, idempotency_key)`（:183）。**无 status 列、无 class/priority/delivery_mode**；due 索引 :187 `WHERE delivered_at IS NULL`。`grep -rn "status.*IN (0,1,2,3)" migrations/` 全仓 **0 命中**——status 0/1/2/3 DDL 不存在（sibling aero-bus B5-1 spec E11 同证） |
| E5 | `crates/aero-server/src/snaplink_commercial/http.rs:23` — `SCOPE_AUDIT="audit:event:write"` | ✅ **Verified（精确命中）**。`const SCOPE_AUDIT: &str = "audit:event:write";` 恰在 :23。`request_token` :233-254：`basic_auth(client_id, client_secret)` + form `grant_type=client_credentials` + `scope` + `resource`——cc grant 的 v1 现形。`validate_token` :353-364 仅形状校验——**iss/aud/scope/sub claim 校验是新的（B5-2 R6），本 direction 供结构** |
| E6 | `crates/aero-server/src/moderation_bot.rs:465-467` — 'message.moderated' | ✅ **Verified（行号近似，语义精确）**。:462-475 `moderate_delete` 调用区（:465-467 在 `metrics::inc_counter` 与 `content_digest` 之间）；实际 token 写入在 `crates/aero-storage/src/message/crud.rs::soft_delete_moderated` **:411**（`append_in_tx(..., None, "message.moderated", ...)`），经 `ImService::moderate_delete`（`crates/aero-im-core/src/service/messages.rs:611`）到达。moderation_bot.rs:198 注释自述 "mirrors the `message.deleted` audit digest in `routes.rs`" |
| E7 | 'message.deleted' 由 routes.rs 写入 | ⚠️ **勘误（token 属实，写入点修正）**。`crates/aero-server/src/routes/routes.rs` 内**无** `message.deleted` 字面量（grep 0 命中；该文件不含 audit 调用）。真实写入点：`crates/aero-storage/src/message/crud.rs::soft_delete_audited` **:378**、`message/authorization.rs` **:503**、`message/events.rs` **:622**（参数化 `audit_action`）。direction 的「三个 ad-hoc token」结论**成立**——'member.add'（生产写入 `crates/aero-server/src/workspaces/mod.rs:715`，audit.rs 文档/测试 :417/:540/:562）、'message.deleted'（crud.rs:378 等三处）、'message.moderated'（crud.rs:411、message_reports.rs:305、aero-ai governance.rs:49 `LOCAL_ACTION_MODERATED`）——但 const 断言测试的 grep 锚点必须用**修正后的真实写入点**（§4 R6） |
| E8 | `crates/aero-common/src/model/event.rs` — tagged-enum 先例 | ✅ **Verified**。`RoomEvent` :28 `#[serde(tag = "kind", rename_all = "snake_case")]`；:77 `#[serde(rename = "notify_kind")]` 规避 §4.2 撞名陷阱。`model/mod.rs` 子模块 + `pub use *` 扁平化先例（本 direction 的 `audit.rs` 复刻此结构） |
| E9 | aero-common 是唯一共享叶子（connector 与 storage 不能互相依赖） | ✅ **Verified**。`crates/aero-storage/Cargo.toml` workspace 依赖**仅 `aero-common`**；`aero-ai` 依赖 aero-common/storage/bus（在 storage 之上，governance.rs 无法供 storage/connector 用）；connector（B5-2 新 crate）须避 aero-server 依赖环。**aero-common 是 storage/connector/ai/server 的唯一共同叶子** |

### 1.1 补充证据（方向外事实，决定设计形态）

| # | Supplementary evidence | Verification result |
|---|---|---|
| E10 | **aero-ai governance.rs 已落地部分契约（值必须对齐）** | ✅ **Verified**。`crates/aero-ai/src/governance.rs`：`GOVERNANCE_CLASS_ADMIN="admin"` :37 / `GOVERNANCE_CLASS_MESSAGE="message"` :39 / `GOVERNANCE_CLASS_ROOM="room"` :41；`GOVERNANCE_PRIORITY_MODERATION: i16 = 100` :31 / `GOVERNANCE_PRIORITY_BACKLOG: i16 = 10` :33（:32 文档明言 **BACKLOG 10 同时是 0239 列默认**）；`LOCAL_ACTION_MODERATED="message.moderated"` :49；`MODERATION_OUTBOUND_ACTION="admin.content.flag"` :56（契约双候选锁定为一）；`GovernanceLane` 结构 :61、`status: i16` 字段 :71（:69 文档「Status 0 is the enqueue-time normative state (0239)」）、映射 `status: 0` :92；`governance_lane_for` 纯函数 + 6 个单元测试钉值。**全部是裸 `&'static str`/`i16` 原语、无枚举、无 DeliveryId、在 aero-ai（storage 之上）**——aero-common 枚举成为规范源后，两边的**值必须相等**（§6 协调） |
| E11 | **sibling aero-bus B5-1 spec 已钉 status 语义** | ✅ **Verified**。`docs/requirements/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.req.md` :97：0239 DDL `status SMALLINT NOT NULL + CHECK (status IN (0,1,2,3))`，**0=pending/1=claimed/2=delivered/3=dead**；claim 谓词 = `WHERE status IN (0,1)`。本 direction 的枚举值必须与此一致（R4 直接采用同一语义） |
| E12 | **37/37 契约测试清单仓内钉入点** | ✅ **Verified**。`scripts/b5-pin.sh`：`B5_CONTRACT_TEST_LIST` 恰 37 槽（15 执行 + 22 `[PROPOSED]` 仓外槽）；`assert_b5_contract_pin` 守卫（恰 37、无重复、无 malformed、非空转、verdict 行 `B5-CHECK <name>: PASS|SKIP`）。**22 个 contract-test-XX[PROPOSED] 槽 = 仓外契约夹具的落点**——本 direction 的 wire-shape 测试（A4）就是这些夹具入仓前的规范形状 |
| E13 | **B5-2 claim 校验语义（iss/aud/scope/sub）** | ✅ **Verified**。`docs/requirements/2026-08-06-aero-audit-connector.req.md` R6 :107-110：claim 校验 = `iss`=配置 issuer、`aud` 含配置 audience、`scope` 含 `audit:event:write`、`sub`=配置 identity；任何不匹配 ⇒ fail-closed（不 POST）；claim 校验是**每次投递的前置**。v1 无此物（E5）。sibling B5-2 spec（2026-08-07-aero-ai-b5-2-audit-connector.req.md E1）同证 |
| E14 | **direction 的「routes.rs 写入」为注释所指而非代码事实** | ✅ **Verified**。moderation_bot.rs:198 注释 "mirrors the `message.deleted` audit digest in `routes.rs`" 是历史措辞；`crates/aero-server/src/routes/` 目录现为 `routes.rs`（装配） + `handlers/*` 结构。A2 的 grep 锚点以 E7 修正为准 |

## 2. Verified current state

```
今天（三处 ad-hoc 字符串，无共享类型）：
  'member.add'          crates/aero-server/src/workspaces/mod.rs:715（另 :751 'member.role_change' / :777 'member.remove'）
  'message.deleted'     crates/aero-storage/src/message/crud.rs:378 · authorization.rs:503 · events.rs:622
  'message.moderated'   crates/aero-storage/src/message/crud.rs:411 · message_reports.rs:305
                        + crates/aero-ai/src/governance.rs:49 LOCAL_ACTION_MODERATED（唯一常量化，但裸 &str 且在 aero-ai）
  全部经 AuditRepo::append / append_in_tx (audit.rs:113/:127, action: &str) 自由字符串落 audit_events.action (0007)

v1 投递（无 status/class/priority/delivery_mode，E4/E5）：
  audit_events ──0236 AFTER INSERT 触发器──▶ snaplink_delivery_outbox (0235:161, claim_token/lease 制)
  ──内嵌 relay──▶ cc grant (http.rs:233-254) + scope="audit:event:write" (http.rs:23) + 202 receipt (形状校验 only)

v2 缺口（direction 问题陈述，全部验证）：
  (1) 无 AuditDeliveryId（E1）；(2) 无 AuditClass/AuditPriority/AuditOutboxStatus/DeliveryMode 枚举
      —— aero-ai governance.rs 只有原语常量（E10）；(3) 无 status 0/1/2/3 DDL（E4）；
  (4) 无 cc/claim 结构（iss/aud/scope/sub 是 B5-2 R6 的新要求，E13）；(5) 无共享 token 常量
      （B5-1 的 audit_governance.rs 与 B5-2 的 connector 今天没有任何可共同编译的类型——E9 层约束）
```

**Gaps the direction closes**（all verified）：B5-1（0239 DDL + `audit_governance.rs` repo）、B5-2（connector）、B5-3（priority DESC claim）三方各自要写 status/class/priority/delivery 语义，却没有任何共同类型可引用——`aero-storage` 与 connector 不能依赖 aero-server，也不能依赖 aero-ai（E9）；aero-common 是唯一叶子。方向在 aero-common 放**规范类型 + 常量 + 线格式**，让 B5-1 的 repo INSERT 与 B5-2 的状态转移**编译期共用同一枚举**（"cannot drift" 机制），并把今天散落的三个 token 收口为常量（A2 const 断言钉住现网字符串）。

## 3. Scope

**In scope（本 direction，module `crates/aero-common`，纯类型 + 单元测试）**:
- `ids.rs` 新增 `AuditDeliveryId`（`define_id!`，E1 宏现成）+ `lib.rs` re-export。
- 新文件 `crates/aero-common/src/model/audit.rs`（`model/mod.rs` 挂 `pub mod audit;` + `pub use audit::*;` 复刻 E8 先例）：
  - `AuditClass`（message/room/admin，snake_case 字符串线格式，`FromStr` fail-closed）；
  - `AuditPriority`（Backlog=10 / Moderation=100，i16 线格式，与 E10 值对齐，DESC 优先级语义文档化）；
  - `AuditOutboxStatus`（0=Pending/1=Claimed/2=Delivered/3=Dead，**与 E11 语义一致**，i16 线格式，`is_terminal`/`is_claimable`）；
  - `DeliveryMode`（serde snake_case，变体集 [PROPOSED] 待仓外 v2 契约，`#[non_exhaustive]` + `Default`，见 R5）；
  - action-token 常量 `AUDIT_ACTION_MEMBER_ADD` / `AUDIT_ACTION_MESSAGE_DELETED` / `AUDIT_ACTION_MESSAGE_MODERATED` + 模块级 `const _: () = assert!(...)` 钉死字符串 + `AUDIT_SCOPE = "audit:event:write"`；
  - cc/claim 结构：`ClientCredentials`（token_endpoint/client_id/client_secret/scope/resource，E5 v1 形态）与 `AuditClaim`（iss/aud/scope/sub，E13 语义）——精确键名线格式 + serde round-trip 测试（A4）。
- 依赖约束：**零新第三方依赖**（serde/serde_json 已在 aero-common Cargo.toml，E9）；**不改** `error.rs`、`event.rs`、`audit.rs`、`governance.rs`。

**Out of scope（sibling directions，勿在本模块实现）**:
- `0239_audit_governance_outbox.sql` DDL + `audit_governance.rs` repo + enqueue/reconcile 重定向 + 0→1→2/3 状态转移实现 → **B5-1 (aero-storage)**（sibling spec E11 已钉语义；本 direction 只供它 `use` 的枚举）。
- connector crate、lease/backoff/dead 转移、claim 校验运行时、403/422→dead → **B5-2 (aero-audit-connector)**。
- `claim_due ORDER BY priority DESC` + 反饥饿上限 → **B5-3 (aero-storage)**。
- `RoomEvent` → (class, action, target) 分类函数（analysis direction #2，未入选）；`RelayOutcome`/HTTP→outcome 映射（direction #3，未入选）。
- aero-ai `governance.rs` 重构为引用 aero-common 常量（本 direction 只保证**值相等**，见 §6；改 aero-ai 是 sibling 切片的事）。
- 改 `AuditRepo::append` 签名 / 迁移现有调用点（B5-1 的活）。

## 4. Requirements

### R1 — `AuditDeliveryId`（ids.rs，强类型 delivery ID）
- `crates/aero-common/src/ids.rs` 用既有 `define_id!` 新增 `AuditDeliveryId`（紧随 `AuditId` 之后），并在 `crates/aero-common/src/lib.rs` 的 `pub use ids::{...}` 列表加入。ULID 存储为 UUID、线格式透明字符串（宏自带）——与 `audit_events.id` 的 `AuditId` 同构，取代 v1 的 TEXT 拼接 `'audit:'||NEW.id`（E4/E5）成为 v2 的 `delivery_id`/`event_id` 类型。
- **Acceptance A1**：单元测试 `audit_delivery_id_round_trips_via_uuid`（`to_uuid`/`from_uuid` 往返 + `new()` 两次不相等）；`AuditDeliveryId` 可 `Serialize`/`Deserialize`（serde 透明）。

### R2 — `AuditClass`（枚举，fail-closed 解析）
- `pub enum AuditClass { Message, Room, Admin }`，`#[serde(rename_all = "snake_case")]` → 线格式字符串 **"message" / "room" / "admin"**（与 E10 `GOVERNANCE_CLASS_*` 三值逐字相等，测试钉）。
- `impl FromStr`：**穷尽 match 三个已知 + `_ => Err`**——未知类在类型边界即拒绝（fail-closed），返回 `Err(AuditClassParseError)`（或 `Result<Self, &'static str>`，不引入新 error 类型）；配套 `as_str() -> &'static str`、`pub const ALL: [AuditClass; 3]`（穷尽迭代用）。
- 文档注明 T-11 映射契约：**未知类（与 403 同类）⇒ 投递侧死终态（status 3），绝不重试**——转移本身是 B5-2 的实现，本类型保证「未知进不了类型」。
- **Acceptance A3**：`from_str` 拒绝未知（"user" / "system" / "" / "MESSAGE" / "admin " 全部 Err，测试具名 `unknown_class_is_rejected_fail_closed`）；三个已知往返；`ALL` 穷尽覆盖恰好这三者。

### R3 — `AuditPriority`（i16 车道枚举，DESC 语义文档化）
- `#[repr(i16)] pub enum AuditPriority { Backlog = 10, Moderation = 100 }`，`pub const fn as_i16(self) -> i16` / `pub const fn from_i16(v: i16) -> Option<Self>`（未知车道 → `None`，fail-closed；新车道 = 改这一个枚举，单源）。
- 值必须等于 E10：`Backlog=10`（0239 列默认，governance.rs :32 文档明言）、`Moderation=100`——测试钉 `as_i16()` 与 `from_i16` 往返。
- serde 线格式 = JSON 整数（`SMALLINT` 对齐）；文档 + 测试钉 **DESC 语义**：`Moderation.as_i16() > Backlog.as_i16()` 且注释明确「claim `ORDER BY priority DESC` 高者先达」（**勿**按 `ai_job::priority_for` 的 ASC 方向"对齐"，governance.rs :7-16 已警告）。
- **Acceptance A3（延续）**：`from_i16` 拒绝 0/1/99/101/-1 → `None`；`as_i16`/`from_i16` 往返。

### R4 — `AuditOutboxStatus`（status 0..3 规范枚举，dead-terminal 钉死）
- `#[repr(i16)] pub enum AuditOutboxStatus { Pending = 0, Claimed = 1, Delivered = 2, Dead = 3 }`——语义与 sibling aero-bus B5-1 spec（E11）逐字一致：**0=pending / 1=claimed / 2=delivered / 3=dead**；`from_i16` 拒绝 0..3 之外（`None`，fail-closed——DDL 的 `CHECK (status IN (0,1,2,3))` 在类型层同构）。
- `pub const fn is_terminal(self) -> bool`：**恰 `Dead` 为 true**（status 3 = dead-terminal，不可再转移）；`pub const fn is_claimable(self) -> bool`：`Pending | Claimed`（= sibling spec 的 claim 谓词 `WHERE status IN (0,1)`，E11——repo 查询与 relay 判断共用这一个函数，杜绝谓词漂移）。
- 文档钉状态机转移表：`0→1`（claim + lease/claim_token）、`1→2`（外部 ack/202 receipt 后）、`0/1→3`（403/422/409/回执错 = 死终态，≤1 次重试，T-11 fail-closed）、`1→0`（lease 过期 re-park + backoff，attempts+1）——实现全在 B5-1/B5-2，本枚举只做**规范表述**。
- serde 线格式 = JSON 整数 0..3；反序列化 4 / -1 / 字符串 → **Err**（fail-closed，测试钉）。
- **Acceptance A1**：`from_i16(0..=3)` ↔ `as_i16()` 双向 round-trip；`is_terminal` 仅 Dead；`is_claimable` 仅 Pending|Claimed；serde 0..3 round-trip；`from_str::<AuditOutboxStatus>("4")`/`"-1"`/`"\"claimed\""` 全 Err。
- **Acceptance A5**：模块级 `const _: () = assert!(AuditOutboxStatus::Dead.as_i16() == 3 && AuditOutboxStatus::Pending.as_i16() == 0);` 等四个值断言（编译期）——B5-1 `audit_governance.rs` INSERT 与 B5-2 connector 转移都 `use aero_common::model::audit::AuditOutboxStatus`，单源即不漂移；测试 `dead_is_terminal_and_never_reclaimable` 钉 `!is_claimable(Dead)` + `is_terminal(Dead)`。

### R5 — `DeliveryMode`（serde 枚举，变体集 [PROPOSED]）
- `#[non_exhaustive] pub enum DeliveryMode` + `#[serde(rename_all = "snake_case")]` + `Default`（= 0239 DDL 的 `delivery_mode` 列默认所对应的规范 enqueue 态）。**变体集是仓外 v2 契约文本的内容（[PROPOSED]，同 b5-pin.sh 22 槽）**：实现时以契约落地的变体名为准，本 spec 不臆造变体名——只钉类型边界、serde 线格式（snake_case 字符串）、`Default` 存在且序列化稳定。
- 文档注明：契约落地 = 一处变体名编辑（`MODERATION_OUTBOUND_ACTION` 同款 one-touch 模式，governance.rs :54-56）。
- **Acceptance A4（延续）**：`DeliveryMode::default()` serde round-trip；`#[non_exhaustive]` 保证后续加变体非破坏。

### R6 — action-token 常量 + 编译期 const 断言（A2，grep-anchored）
- `crates/aero-common/src/model/audit.rs` 定义：
  - `pub const AUDIT_ACTION_MEMBER_ADD: &str = "member.add";`
  - `pub const AUDIT_ACTION_MESSAGE_DELETED: &str = "message.deleted";`
  - `pub const AUDIT_ACTION_MESSAGE_MODERATED: &str = "message.moderated";`
  - `pub const AUDIT_SCOPE: &str = "audit:event:write";`（规范 scope 单源，钉 E5 http.rs:23）
- **模块级（非 test 模块，编译失败即 build 红）const 断言**，钉死与现网写入字符串逐字相等：
  ```rust
  const _: () = assert!(AUDIT_ACTION_MEMBER_ADD == "member.add");
  const _: () = assert!(AUDIT_ACTION_MESSAGE_DELETED == "message.deleted");
  const _: () = assert!(AUDIT_ACTION_MESSAGE_MODERATED == "message.moderated");
  const _: () = assert!(AUDIT_SCOPE == "audit:event:write");
  ```
- 每个常量 doc 注释带 **grep 锚点**（E7 修正后的真实写入点）：`AUDIT_ACTION_MEMBER_ADD` → `crates/aero-server/src/workspaces/mod.rs::add_member`（:715 字面量）、`AUDIT_ACTION_MESSAGE_DELETED` → `crates/aero-storage/src/message/crud.rs::soft_delete_audited`（:378 字面量；另 authorization.rs:503 / events.rs:622 同 token）、`AUDIT_ACTION_MESSAGE_MODERATED` → `crates/aero-storage/src/message/crud.rs::soft_delete_moderated`（:411 字面量；另 message_reports.rs:305、aero-ai governance.rs:49 `LOCAL_ACTION_MODERATED` 同值）、`AUDIT_SCOPE` → `crates/aero-server/src/snaplink_commercial/http.rs:23` `SCOPE_AUDIT`。
- 测试（同模块 `#[cfg(test)]`）再断言一次值与 `as_str` 语义一致（const 断言之外的运行时复述，方便断言信息输出）。
- **Acceptance A2**：上述四条 const 断言编译通过即验收；测试名 `token_constants_match_production_literals`。

### R7 — cc/claim 结构（iss/aud/scope/sub，wire-shape 钉死）
- `ClientCredentials`（cc grant 配置载体，v1 `request_token` 形态 E5 的 serde 化）：
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  pub struct ClientCredentials {
      pub token_endpoint: String,
      pub client_id: String,
      pub client_secret: String,
      pub scope: String,      // 规范值 = AUDIT_SCOPE
      pub resource: String,
  }
  ```
  （纯载体，无 HTTP 逻辑——B5-2 用它拼 `basic_auth` + form 体。）
- `AuditClaim`（B5-2 R6 校验目标，E13）：
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  pub struct AuditClaim {
      pub iss: String,
      #[serde(default, deserialize_with = "de_aud_string_or_vec")]
      pub aud: Vec<String>,   // RFC 7519：单字符串或数组皆可入，序列化恒为数组
      pub scope: String,      // 空格分隔 scope 集
      pub sub: String,
  }
  ```
  - `pub fn has_scope(&self, required: &str) -> bool`：`scope` 按空格分词后包含 `required`（B5-2 R6「scope 含 `audit:event:write`」的纯函数化）。
- **Acceptance A4**：`serde_json::to_string` 产物键名**恰** `{"iss","aud","scope","sub"}` / `{"token_endpoint","client_id","client_secret","scope","resource"}`（snake_case、无多余键）；`"aud":"single"` 与 `"aud":["multi"]` 两种输入都反序列化成功且序列化恒为数组；`has_scope("audit:event:write")` 对 `"audit:event:write"`、`"a b audit:event:write c"` 为 true、对 `"audit:event:read"` 为 false；`ClientCredentials` 往返。测试名 `claim_wire_shape_is_exact` / `aud_accepts_single_or_array` / `scope_contains_is_tokenized`。
- **37-tests 联动（文档 + 测试注释）**：`scripts/b5-pin.sh` 的 22 个 `contract-test-XX[PROPOSED]` 槽是仓外契约夹具落点（E12）；这些夹具 JSON 一旦入仓，**必须以本结构反序列化为准**（`AuditClaim`/`ClientCredentials` 即规范形状；若契约夹具键名与此不同，改这里——一处、受 A4 测试保护）。

### R8 — 模块装配与依赖约束（no-drift 机制收口）
- `model/mod.rs`：`pub mod audit;` + `pub use audit::*;`（E8 先例）；`lib.rs`：`pub use ids::{..., AuditDeliveryId, ...}` 与 `pub use model::{..., AuditClass, AuditClaim, AuditOutboxStatus, AuditPriority, ClientCredentials, DeliveryMode, AUDIT_ACTION_MEMBER_ADD, AUDIT_ACTION_MESSAGE_DELETED, AUDIT_ACTION_MESSAGE_MODERATED, AUDIT_SCOPE, ...}`——**无撞名**（grep 核对现有 re-export 列表，E1/E8）。
- **零新依赖**：仅用 serde/serde_json（已在 Cargo.toml）；无 sqlx/tokio/time 参与（`AuditId`/`AuditDeliveryId` 的 ULID 来自宏既有依赖）。
- 契约文档（doc 注释）：`audit_governance.rs`（B5-1）INSERT 与 connector（B5-2）状态转移**必须**引用 `aero_common::model::audit::{AuditClass, AuditOutboxStatus, AuditPriority, AuditDeliveryId}`——枚举即线格式即 DDL 语义（§4.2「文件/符号是稳定 grep 锚点」）。
- **Acceptance A5（续）**：`cargo check --workspace` 干净 + `cargo test -p aero-common --lib` 全绿（本模块全部测试无 DB、无 `#[ignore]`）；`cargo clippy --workspace --all-targets` 无新警告。

## 5. Acceptance（direction 原五条，逐条可测化）

| # | Direction acceptance（原文） | 可测化落点（本 spec） |
|---|---|---|
| A1 | status enum serializes/parses 0..3 round-trip | R4 + 测试 `status_0_3_round_trip`（from_i16/as_i16/serde_json 三向）+ `invalid_status_rejected_fail_closed`（4/-1/字符串 → Err/None）；R1 `audit_delivery_id_round_trips_via_uuid` |
| A2 | compile-time const assertion：token 常量 = audit.rs append 调用 / 'message.deleted' / 'message.moderated' 逐字字符串（grep-anchored） | R6 模块级四条 `const _: () = assert!(...)`（**build 失败即红**）+ doc 注释带 E7 修正后的 grep 锚点 + 测试 `token_constants_match_production_literals` |
| A3 | exhaustive `AuditClass::from_str` rejects unknown → T-11 fail-closed（unknown/403 → terminal） | R2 `_ => Err` 穷尽臂 + 测试 `unknown_class_is_rejected_fail_closed`（未知类永不进类型 ⇒ 投递侧只能走 terminal，转移实现属 B5-2）+ R3 `from_i16` 拒绝未知车道 |
| A4 | wire-shape JSON of claim contract matches 37-tests contract once test-integration.sh pins fixtures | R7 精确键名 round-trip 测试（`claim_wire_shape_is_exact` / `aud_accepts_single_or_array` / `scope_contains_is_tokenized`）+ R5 `DeliveryMode::default()` 往返；37-tests 联动 = b5-pin.sh 22 `[PROPOSED]` 槽（E12）——夹具入仓即以本结构为准，测试注释指路 |
| A5 | audit_governance.rs inserts reference same enums as connector's status transitions（status 3 = dead-terminal），repo 与 relay 不漂移 | R4 单源枚举 + 值 const 断言 + `is_terminal`/`is_claimable`（claim 谓词 `WHERE status IN (0,1)` 同函数化）+ R8 装配/依赖约束；测试 `dead_is_terminal_and_never_reclaimable`、`status_values_match_ddl_contract` |

## 6. 跨切片协调与风险

- **aero-ai governance.rs 值对齐（E10）**：aero-common 的 `AuditClass::Admin.as_str()=="admin"`、`AuditPriority::Moderation.as_i16()==100`、`Backlog==10`、`AUDIT_ACTION_MESSAGE_MODERATED=="message.moderated"`、`AuditOutboxStatus::Pending==0` 全部与 governance.rs 常量**逐值相等**（R2/R3/R4 测试钉）。aero-common 不能依赖 aero-ai（E9），故相等性由两侧各自测试钉值 + G6 drill 兜底；**aero-ai 改引 aero-common 常量是 sibling 切片的事，本 direction 不做**（不越模块）。
- **B5-1（aero-storage）**：`audit_governance.rs` 消费 R4 枚举做 0239 INSERT；DDL `CHECK (status IN (0,1,2,3))` 与 `from_i16` 拒绝域同构（E11）。
- **B5-2（connector）**：消费 R1/R2/R7——`AuditDeliveryId` 作 `event_id`/`Idempotency-Key`、`AuditClaim` 作 iss/aud/scope/sub 校验目标、`AuditOutboxStatus::Dead` 作死终态、未知 `AuditClass` 的 T-11 映射（R2 文档契约）。
- **B5-3（aero-storage）**：`priority DESC` claim 消费 R3（值 100 > 10 保证 moderation 先达，A3 drill）。
- **风险 R-1**：`DeliveryMode` 变体名在契约落地前是 [PROPOSED] 占位——不得用它在任何 SQL/断言里硬编码（R5 已限 `Default` 往返）。
- **风险 R-2**：若仓外契约夹具的 claim 键名与本 spec 不同（如 `aud` 恒为字符串），改 R7 一处 + A4 测试跟随（one-touch），勿在 connector 侧再引入第二套形状。

## 7. 交付验证（提交前必过，AGENTS.md §4.3）

`cargo check --workspace`（干净）· `cargo test -p aero-common --lib`（全绿，本模块测试全为无 DB 单元测试）· `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规；`model/audit.rs` 含测试远小于 800 WARN 阈值）。
