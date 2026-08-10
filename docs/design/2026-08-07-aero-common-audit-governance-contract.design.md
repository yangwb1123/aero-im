# Design — aero-common 共享审计治理域契约（AuditDeliveryId + class/priority/status-0..3/delivery-mode 枚举 + token 常量 + cc/claim 结构）

- **Module**: `crates/aero-common`（唯一共享叶子，纯类型 + 单元测试，零 DB、零新依赖）
- **Source**: `docs/requirements/2026-08-07-aero-common-audit-governance-contract.req.md`（182 行，R1–R8 / A1–A5）
- **Campaign**: `aero-im-b5-outbox-relay`；门禁 **G6 (B5)** = "37/37、T-11、moderation 优先级"（`docs/campaigns/implementation-gate.md:78`）
- **Sibling designs**: `docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md`（bus 层 seam）、`docs/design/2026-08-07-aero-ai-b5-2-audit-connector-design.md`（connector）、`docs/design/2026-08-06-aero-ai-b5-1-governance-lane-design.md`（lane 值锚）

## 0. Evidence verification（untrusted claims 逐条回仓核对，2026-08-07）

| Claim | Verdict | Evidence |
|---|---|---|
| `ids.rs:121-122` — AuditId 存在、无 AuditDeliveryId | ✅ | `crates/aero-common/src/ids.rs`：`AuditId` 恰在 **:122**（`NotificationId` :126）；`AuditDeliveryId` 全仓 **0 命中**。`define_id!`（:12 起）派生 `Clone/Copy/PartialEq/Eq/PartialOrd/Ord/Hash/Serialize/Deserialize` + `#[serde(transparent)]` + `new/from_ulid/as_ulid/to_uuid/from_uuid/nil` + `Default/Debug/Display/FromStr/From<Ulid>` |
| `error.rs` 无 422 / 无 terminal | ✅ | `status_code()` 在 **:66**：404/401/403/409/400/429/502/500——无 422，无 terminal/retryable 分类。本设计**不改 error.rs** |
| `audit.rs:113-171` 自由 `&str` action | ✅ | `crates/aero-storage/src/audit.rs` 恰 **1049 行**；`append` **:113**、`append_in_tx` **:127**；`action: &str` 直接 bind `audit_events.action`（0007 DDL `action TEXT NOT NULL`） |
| `0235…sql:161-187` 无 status 枚举 | ✅ | `snaplink_delivery_outbox` :161-184：`delivery_id/destination/workspace_id/…/claim_token/lease_expires_at/…/last_error` + claim-state CHECK（:179-181）+ `UNIQUE (destination, idempotency_key)`（:183）。**无 status 列**；`grep 'status IN (0,1,2,3)' migrations/` = **0 命中** |
| `http.rs:23` `SCOPE_AUDIT` | ✅ | `crates/aero-server/src/snaplink_commercial/http.rs:23`：`const SCOPE_AUDIT: &str = "audit:event:write";` 精确命中；cc grant `request_token` :233-254（`basic_auth` + form `grant_type=client_credentials` + `scope` + `resource`）；`validate_token` :353-364 仅形状校验 |
| `moderation_bot.rs:465-467` | ✅ | :460-470 区域命中（`metrics::inc_counter` 与 `content_digest` 之间）；token 写入实为 `message/crud.rs::soft_delete_moderated` **:411**（经 `ImService::moderate_delete`，`im-core/service/messages.rs:611`） |
| routes.rs 写 `message.deleted` | ⚠️→✅ 勘误 | `routes/routes.rs` 内字面量 **0 命中**。真实写入点：`message/crud.rs:378`（`soft_delete_audited`）、`message/authorization.rs:503`、`message/events.rs:622`。`member.add` → `workspaces/mod.rs:715`；`message.moderated` → `crud.rs:411`、`message_reports.rs:305`。grep 锚点以修正为准 |
| tagged-enum 先例（E8） | ✅ | `model/event.rs:27` `#[serde(tag = "kind", rename_all = "snake_case")]`；`model/mod.rs` = `pub mod x;` + `pub use x::*;` 扁平化 |
| 层约束（E9） | ✅ | `crates/aero-storage/Cargo.toml` workspace 依赖**仅 `aero-common`**；aero-ai 在 storage 之上——governance.rs 无法供 storage/connector 用 |
| governance.rs 值对齐（E10） | ✅ | `crates/aero-ai/src/governance.rs`：`GOVERNANCE_PRIORITY_MODERATION: i16 = 100` :31、`GOVERNANCE_PRIORITY_BACKLOG: i16 = 10` :33（:32 注明 BACKLOG 10 = 0239 列默认）、`GOVERNANCE_CLASS_ADMIN="admin"` :37 / `MESSAGE="message"` :39 / `ROOM="room"` :41、`LOCAL_ACTION_MODERATED="message.moderated"` :49、`MODERATION_OUTBOUND_ACTION="admin.content.flag"` :56；`status: i16` :71、enqueue `status: 0` :92。文件头 :13 明言 claim 为 **priority DESC 高者先达**（与 ai_job ASC 相反） |
| sibling B5-1 钉 status 语义（E11） | ✅ | `docs/requirements/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.req.md` :97：0239 DDL `status SMALLINT NOT NULL + CHECK (status IN (0,1,2,3))`，**0=pending/1=claimed/2=delivered/3=dead**；claim 谓词 = `WHERE status IN (0,1)` |
| b5-pin.sh 37 槽（E12） | ✅ | `scripts/b5-pin.sh`：`B5_CONTRACT_TEST_LIST` 37 槽 = 15 执行 + 22 `[PROPOSED]`；`assert_b5_contract_pin` 守卫存在 |
| B5-2 claim 校验（E13） | ✅ | `docs/requirements/2026-08-06-aero-audit-connector.req.md` R6：iss=配置 issuer、aud 含配置 audience、scope 含 `audit:event:write`、sub=配置 identity；不匹配 fail-closed 不 POST |
| routes 目录结构（E14） | ✅ | `crates/aero-server/src/routes/` = `routes.rs`（装配）+ `handlers/*`；moderation_bot.rs:198 注释为历史措辞 |

**补充核对**：迁移数 = **238**（`ls migrations/*.sql | wc -l`，下一序号 0239 属 B5-1）；`scripts/file-size-check.sh` 阈值 Rust 800 WARN / 1200 HARD（`:6-7`）；aero-common `lib.rs`/`model/mod.rs` 现有 re-export 与 `AuditClass/AuditClaim/AuditOutboxStatus/AuditPriority/ClientCredentials/DeliveryMode/AUDIT_*` **0 撞名**；aero-common `Cargo.toml` 已有 serde/serde_json/serde_with/serde_bytes/thiserror/ulid/uuid（零新依赖成立）；MSRV 1.80（const `assert!` 1.57 起稳定、`#[non_exhaustive]` 1.40 起稳定、`#[serde(into/try_from)]` 远古稳定）。

**All claims verified; zero fabrication found.**

## 1. Design overview

```
今天（三处 ad-hoc 字符串 + v1 TEXT 拼接 delivery_id，无共享类型）：
  'member.add'        workspaces/mod.rs:715 · 'message.deleted' crud.rs:378/authorization.rs:503/events.rs:622
  'message.moderated' crud.rs:411 · message_reports.rs:305 + aero-ai governance.rs:49（唯一常量化，但裸 &str 且在 aero-ai）
  audit_events ──0236 触发器──▶ snaplink_delivery_outbox（TEXT delivery_id = 'audit:'||NEW.id，claim_token/lease 制）
  ──relay──▶ cc grant（http.rs:233-254，scope='audit:event:write'）+ 202 receipt（形状校验 only）

本设计（aero-common 供类型，值 = B5-1/B5-2/B5-3 编译期共用单源）：
  ids.rs: AuditDeliveryId（ULID，serde-transparent）        ──▶ B5-1 0239 delivery_id/event_id · B5-2 Idempotency-Key
  model/audit.rs: AuditClass{message,room,admin}            ──▶ B5-1 0239 class 列 · B5-2 T-11 未知类→dead
                  AuditPriority{Backlog=10,Moderation=100}  ──▶ B5-3 claim ORDER BY priority DESC（100>10 先达）
                  AuditOutboxStatus{0..3} + is_terminal/is_claimable ──▶ B5-1 INSERT/claim 谓词 · B5-2 状态转移
                  DeliveryMode（[PROPOSED]，Default 往返）   ──▶ B5-1 0239 delivery_mode 列
                  AUDIT_ACTION_* 四常量 + const 断言          ──▶ 现网字面量编译期钉死（防漂移 tripwire）
                  ClientCredentials / AuditClaim              ──▶ B5-2 cc 拼装 + iss/aud/scope/sub 校验（R6）
```

「cannot drift」机制 = **枚举即线格式即 DDL 语义**：B5-1 的 `audit_governance.rs` INSERT、B5-3 的 claim 排序、B5-2 的状态转移全部 `use aero_common::model::audit::*`——任何一处改值，其余各处编译期/测试期即红。三处 ad-hoc token 今天**不改调用点**（那是 B5-1 切片的事），本设计用模块级 const 断言把现网字面量钉进编译。

## 2. API changes（全部在 `crates/aero-common`，纯加法）

### 2.1 `ids.rs` — `AuditDeliveryId`（紧随 `AuditId` 之后）

```rust
define_id!(
    /// Identifies a single audit-governance delivery (v2 `audit_outbox.delivery_id`
    /// and connector `Idempotency-Key`). ULID, serde-transparent string wire format —
    /// replaces v1's TEXT concat `'audit:' || NEW.id` (0235:161).
    AuditDeliveryId
);
```

宏自带 `new/from_ulid/as_ulid/to_uuid/from_uuid/nil` + `Serialize/Deserialize`（透明字符串）。**DDL 侧存 UUID 的转换是 B5-1 repo 边界的显式调用（`to_uuid`/`from_uuid`），类型层不做隐式转换**。

### 2.2 新文件 `crates/aero-common/src/model/audit.rs`（完整 API 草图）

```rust
//! Shared audit-governance domain contract (B5). Pure types + constants; zero DB,
//! zero I/O. Wire values MUST equal `aero_ai::governance` (value-equality pinned
//! by tests on both sides — aero-common cannot depend on aero-ai, E9).

/// Audit class. Wire strings equal governance.rs `GOVERNANCE_CLASS_*` (:37/:39/:41).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]          // "message" / "room" / "admin"
pub enum AuditClass { Message, Room, Admin }

impl AuditClass {
    /// Exhaustive iteration set — used by tests and (later) mapper fns.
    pub const ALL: [AuditClass; 3] = [AuditClass::Message, AuditClass::Room, AuditClass::Admin];
    #[must_use] pub const fn as_str(self) -> &'static str { /* match, const-compatible */ }
}

impl FromStr for AuditClass {                // fail-closed: unknown => Err, never a default
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s { "message" => Ok(Self::Message), "room" => Ok(Self::Room),
                  "admin" => Ok(Self::Admin), _ => Err("unknown audit class") }
    }
}
// + impl Display (delegates as_str) — 便于审计日志打印，不引入新 error 类型。

/// Priority lane. Wire = JSON integer (SMALLINT-aligned). DESC semantics:
/// Moderation(100) claimed before Backlog(10) under B5-3 `ORDER BY priority DESC`.
/// Do NOT "align" with `ai_job::priority_for` (ASC lower-first) — governance.rs:7-16 warns.
#[repr(i16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(into = "i16", try_from = "i16")]     // integer wire; "100" string / 99 => Err
pub enum AuditPriority { Backlog = 10, Moderation = 100 }

impl AuditPriority {
    #[must_use] pub const fn as_i16(self) -> i16 { self as i16 }
    #[must_use] pub const fn from_i16(v: i16) -> Option<Self> { /* match 10/100, _ => None */ }
}
impl From<AuditPriority> for i16 { fn from(p: AuditPriority) -> i16 { p.as_i16() } }
impl TryFrom<i16> for AuditPriority { type Error = &'static str; /* None => Err */ }

/// Outbox status 0..3 — semantics verbatim from sibling B5-1 spec (E11):
/// 0=pending / 1=claimed / 2=delivered / 3=dead. 3 = dead-terminal, never reclaimable.
#[repr(i16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(into = "i16", try_from = "i16")]     // JSON integer 0..3; 4/-1/"claimed" => Err
pub enum AuditOutboxStatus { Pending = 0, Claimed = 1, Delivered = 2, Dead = 3 }

impl AuditOutboxStatus {
    #[must_use] pub const fn as_i16(self) -> i16 { self as i16 }
    #[must_use] pub const fn from_i16(v: i16) -> Option<Self> { /* 0..=3, _ => None */ }
    /// Terminal = exactly Dead. Dead rows are never reclaimed (B5-2 transfers to
    /// dead only on 403/422/409/receipt-error — T-11 fail-closed).
    #[must_use] pub const fn is_terminal(self) -> bool { matches!(self, Self::Dead) }
    /// Claim predicate `WHERE status IN (0,1)` — single source for repo query AND
    /// relay judgment (no predicate drift).
    #[must_use] pub const fn is_claimable(self) -> bool { matches!(self, Self::Pending | Self::Claimed) }
}
impl From<AuditOutboxStatus> for i16 { /* ... */ }
impl TryFrom<i16> for AuditOutboxStatus { type Error = &'static str; /* ... */ }

// 状态机转移表（doc 注释钉死；实现全在 B5-1/B5-2）：
//   0 → 1  claim + lease/claim_token（B5-1）
//   1 → 2  外部 ack/202 receipt（B5-2）
//   0/1 → 3  403/422/409/回执错 = dead-terminal（≤1 次重试，T-11 fail-closed）
//   1 → 0  lease 过期 re-park + backoff，attempts+1

/// Delivery mode. Variant set is [PROPOSED] — owned by the out-of-repo v2 contract
/// text (same one-touch pattern as governance.rs:54-56 MODERATION_OUTBOUND_ACTION).
/// Do NOT hardcode any variant in SQL/asserts until the contract lands (R-1).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]          // string wire format
pub enum DeliveryMode { /* contract variants; Default = enqueue-time mode */ }
impl Default for DeliveryMode { /* enqueue-time mode */ }

// ── action tokens：值 = 现网字面量，grep 锚点见各常量 doc（E7 修正后的真实写入点）──

/// `crates/aero-server/src/workspaces/mod.rs::add_member` (:715 literal)
pub const AUDIT_ACTION_MEMBER_ADD: &str = "member.add";
/// `crates/aero-storage/src/message/crud.rs::soft_delete_audited` (:378 literal;
/// also authorization.rs:503, events.rs:622)
pub const AUDIT_ACTION_MESSAGE_DELETED: &str = "message.deleted";
/// `crates/aero-storage/src/message/crud.rs::soft_delete_moderated` (:411 literal;
/// also message_reports.rs:305, aero-ai governance.rs:49 LOCAL_ACTION_MODERATED)
pub const AUDIT_ACTION_MESSAGE_MODERATED: &str = "message.moderated";
/// `crates/aero-server/src/snaplink_commercial/http.rs:23` SCOPE_AUDIT
pub const AUDIT_SCOPE: &str = "audit:event:write";

// 模块级 const 断言（非 test 模块——漂移即 build 红）：
const _: () = assert!(AUDIT_ACTION_MEMBER_ADD == "member.add");
const _: () = assert!(AUDIT_ACTION_MESSAGE_DELETED == "message.deleted");
const _: () = assert!(AUDIT_ACTION_MESSAGE_MODERATED == "message.moderated");
const _: () = assert!(AUDIT_SCOPE == "audit:event:write");
const _: () = assert!(AuditOutboxStatus::Pending.as_i16() == 0);
const _: () = assert!(AuditOutboxStatus::Claimed.as_i16() == 1);
const _: () = assert!(AuditOutboxStatus::Delivered.as_i16() == 2);
const _: () = assert!(AuditOutboxStatus::Dead.as_i16() == 3);
const _: () = assert!(AuditPriority::Backlog.as_i16() == 10);
const _: () = assert!(AuditPriority::Moderation.as_i16() == 100);
const _: () = assert!(AuditClass::Admin.as_str() == "admin");
const _: () = assert!(AuditClass::Message.as_str() == "message");
const _: () = assert!(AuditClass::Room.as_str() == "room");

/// cc-grant 配置载体（v1 request_token 形态 http.rs:233-254 的 serde 化；纯载体无 HTTP）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCredentials {
    pub token_endpoint: String,
    pub client_id: String,
    pub client_secret: String,
    pub scope: String,          // 规范值 = AUDIT_SCOPE
    pub resource: String,
}

/// Connector 投递前置校验目标（B5-2 R6, E13）。键名恰 iss/aud/scope/sub。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditClaim {
    pub iss: String,
    /// RFC 7519：单字符串或数组皆可入，序列化恒为数组。
    #[serde(default, deserialize_with = "de_aud_string_or_vec")]
    pub aud: Vec<String>,
    /// 空格分隔 scope 集。
    pub scope: String,
    pub sub: String,
}

impl AuditClaim {
    /// B5-2 R6「scope 含 audit:event:write」的纯函数化：空格分词包含即 true。
    #[must_use]
    pub fn has_scope(&self, required: &str) -> bool {
        self.scope.split_whitespace().any(|s| s == required)
    }
}

fn de_aud_string_or_vec<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where D: serde::Deserializer<'de> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany { One(String), Many(Vec<String>) }
    Ok(match OneOrMany::deserialize(d)? { OneOrMany::One(s) => vec![s], OneOrMany::Many(v) => v })
}
```

设计要点（实现时勿偏离）：
- `AuditPriority`/`AuditOutboxStatus` 用 `#[serde(into = "i16", try_from = "i16")]` 拿 **JSON 整数**线格式（SMALLINT 对齐），`TryFrom` 的 `Error = &'static str`（serde try_from 路径要求 `Display`，`&'static str` 满足）。JSON 字符串 `"100"`/`"claimed"`、越界整数一律反序列化 **Err**——**禁止**给 `AuditClass` 加 `#[serde(other)]` 或其他宽容臂（fail-closed 边界即类型边界）。
- `de_aud_string_or_vec` 保持私有（clippy `unreachable_pub` 约束）。
- 所有 `pub const fn` / `pub const` 数组加 `#[must_use]`（存量风格一致，pedantic 无新警告）。
- `DeliveryMode` 落地时以仓外契约变体名为准：**一处变体编辑**，测试只钉 `Default` 往返（见 R-1 风险）。

### 2.3 装配（两处 re-export，0 撞名已核对）

`model/mod.rs`：
```rust
pub mod audit;
// ... 既有 pub mod 列表内，字母序插入
pub use audit::*;   // 既有 pub use 列表内
```

`lib.rs`：
```rust
pub use ids::{
    /* 字母序 */ ..., AuditDeliveryId, AuditId, ...,
};
pub use model::{
    /* 字母序 */ ..., AuditClaim, AuditClass, AuditOutboxStatus, AuditPriority,
    ClientCredentials, DeliveryMode, ...,
    AUDIT_ACTION_MEMBER_ADD, AUDIT_ACTION_MESSAGE_DELETED, AUDIT_ACTION_MESSAGE_MODERATED,
    AUDIT_SCOPE, ...,
};
```

### 2.4 明确不改（兼容边界）

- `error.rs`（无 422/terminal——terminal 语义属 B5-2）、`model/event.rs`（tagged-enum 不动）、`aero-storage/src/audit.rs`（`append`/`append_in_tx` 签名不动——改签名是 B5-1 的活）、`aero-ai/src/governance.rs`（重构为引用 aero-common 常量是 sibling 切片的事）、`snaplink_commercial/http.rs`、三处 token 字面量写入点、任何迁移文件（**迁移计数保持 238**，0239 归 B5-1）。

## 3. Compatibility constraints

| # | 约束 | 依据 |
|---|---|---|
| C1 | **零新第三方依赖**：只用 serde/serde_json（已在 `aero-common/Cargo.toml`）；ULID 走宏既有 ulid/uuid | R8，E9 |
| C2 | **aero-common 保持叶子**：不得依赖 storage/ai/server 任何符号（值相等由两侧测试钉，不靠依赖） | E9，§4.2 |
| C3 | **值相等（逐字）**：`Admin=="admin"`、`Message=="message"`、`Room=="room"`、`Backlog==10`、`Moderation==100`、`AUDIT_ACTION_MESSAGE_MODERATED=="message.moderated"`、`Pending==0` 与 governance.rs / B5-1 sibling spec 完全一致；`Moderation.as_i16() > Backlog.as_i16()`（DESC 语义） | E10/E11，R2/R3/R4 |
| C4 | **整数线格式**：AuditPriority/AuditOutboxStatus 序列化为 JSON 整数（非字符串、非变体名）——serde 默认对 unit variant 会输出变体名，必须经 `into/try_from` 显式拿整数 | R3/R4 |
| C5 | **fail-closed 解析**：未知 class / 未知车道 / 越界 status / 未知 serde variant ⇒ Err/None，**绝不回落默认值**；与 DDL `CHECK (status IN (0,1,2,3))` 同构 | R2/R3/R4 |
| C6 | **现网调用点不动**：三个 token 字面量（crud.rs:378/:411、authorization.rs:503、events.rs:622、message_reports.rs:305、workspaces/mod.rs:715）本设计零改动——const 断言保证它们与常量逐字相等，B5-1 之后才统一改引常量 | R6，E7 |
| C7 | **无迁移、无 `aero-cli migrate`**：纯类型方向不触碰 `sqlx::migrate!` 嵌入的 migrations/（迁移计数 238 不变）；B5-1 的 0239 才需要「build → migrate」顺序 | §4.2 |
| C8 | **MSRV 1.80 兼容**：const `assert!`（1.57）、`#[non_exhaustive]`（1.40）、`#[serde(into/try_from)]`、`const fn` match（1.46）全部可用 | rust-toolchain |
| C9 | **无撞名**：lib.rs/model re-export 新名与现存 0 冲突（已 grep 核对）；`unreachable_pub`/pedantic 零新警告（私有 helper、`#[must_use]`、`pub const ALL` 带 doc） | R8，§4.2 |
| C10 | **文件尺寸**：`model/audit.rs`（含测试）≪ 800 WARN 阈值 | `scripts/file-size-check.sh:6` |

## 4. Failure modes & mitigations

| # | 失败模式 | 触发 | 缓解（设计内建） |
|---|---|---|---|
| FM-1 | 现网 token 字面量被改（如 crud.rs:378 改成 `"message.deleted.v2"`） | 后续切片/重构手滑 | 模块级 const 断言 → **build 红**，tripwire 强制走契约评审；测试 `token_constants_match_production_literals` 运行时复述 |
| FM-2 | 值与 aero-ai governance.rs 漂移（aero-common 无法依赖 aero-ai，E9） | 任一侧改动 | 两侧各自测试钉值（governance.rs 已有 6 测；本模块 `class_values_match_governance_literals` 等）+ G6 drill 兜底 |
| FM-3 | `DeliveryMode` 变体名与仓外契约不一致 | 契约落地时 | R-1：契约落地前**禁止**在 SQL/断言/connector 硬编码变体名；只钉 `Default` 往返；落地 = 一处变体编辑 + 测试跟随（one-touch） |
| FM-4 | 契约夹具键名不同（如 `aud` 恒为字符串） | b5-pin.sh 22 槽夹具入仓 | A4 精确形状测试现行；改 R7 一处 + 测试跟随，**禁止**在 connector 侧引入第二套形状 |
| FM-5 | 运行时收到越界 status/未知 class（腐坏数据/版本错配） | 外部回执、旧行 | 类型边界 Err/None（fail-closed）；B5-2 对解析失败按 T-11 走 dead-terminal，绝不重试——「未知进不了类型 ⇒ 投递侧只能走 terminal」 |
| FM-6 | JSON 字符串冒充整数（`"100"`/`"claimed"`） | 反序列化 | `into/try_from` 路径先解 i16，字符串即 Err——测试 `invalid_status_rejected_fail_closed` 钉死 |
| FM-7 | 有人给 `AuditClass` 加 `#[serde(other)]` 宽容臂 | 后续维护 | doc + 测试 `unknown_class_is_rejected_fail_closed` 双保险；code review 锚点写进 §2.2 设计要点 |
| FM-8 | re-export 撞名 / 顺序错 | 装配 | `cargo check` 编译错即捕获；C9 已预核 0 撞名 |
| FM-9 | ULID↔UUID 隐式互转（delivery_id 线格式混淆） | B5-1 repo 边界 | `serde(transparent)` 只保证字符串线格式；DB UUID 转换显式 `to_uuid/from_uuid`，无隐式 |

## 5. Migration steps（实现顺序；本方向无 DB 迁移）

> 与 AGENTS.md §4.2 的「加迁移必先 cargo build」无关——**本方向不产生迁移文件**（计数 238 不变，`aero-cli migrate` 不涉及）。以下为代码集成顺序：

1. **`ids.rs`**：`AuditId`（:122）之后插入 `AuditDeliveryId`（`define_id!`）；`cargo check -p aero-common` 先行验证宏无遗漏。
2. **新建 `crates/aero-common/src/model/audit.rs`**：§2.2 全量（枚举 + 常量 + const 断言 + 私有 helper + `#[cfg(test)]` 测试模块）。
3. **装配**：`model/mod.rs` 加 `pub mod audit;` + `pub use audit::*;`；`lib.rs` 的 ids 列表与 model 列表各加新符号（字母序，§2.3）。
4. **验证门**（AGENTS.md §4.3 全套）：
   - `cargo check --workspace`（干净）
   - `cargo test -p aero-common --lib`（全绿；本模块测试无 DB、无 `#[ignore]`）
   - `cargo clippy --workspace --all-targets`（零新警告）
   - `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）
5. **sibling 交接**（本方向不做，但文档指路）：B5-1 消费 `AuditOutboxStatus`/`AuditClass`/`AuditPriority`/`AuditDeliveryId` 写 0239 DDL + repo；B5-2 消费 `AuditClaim`/`ClientCredentials`/`AuditDeliveryId`/`Dead`；B5-3 消费 `AuditPriority` 值序。交接后原 token 字面量统一改引 `AUDIT_ACTION_*`（届时 const 断言继续护航）。

## 6. Testable acceptance mapping（A1–A5 → 具名测试）

| Acceptance（direction 原文） | 测试（`crates/aero-common/src/model/audit.rs` 内 `#[cfg(test)]`，另有 ids.rs 测试） | 对应 R |
|---|---|---|
| A1: status enum serializes/parses 0..3 round-trip | `status_0_3_round_trip`（from_i16 ↔ as_i16 ↔ serde_json 三向，0..=3 全遍历）；`invalid_status_rejected_fail_closed`（serde_json "4"/"-1"/"\"claimed\"" 全 Err；from_i16 越界 None）；`audit_delivery_id_round_trips_via_uuid`（ids.rs：to_uuid/from_uuid 往返 + `new()` 两次不相等 + serde 透明序列化） | R1/R4 |
| A2: compile-time const assertion：token 常量 = 现网字面量（grep-anchored） | 模块级 4 条 `const _: () = assert!(...)`（**build 失败即红**）+ doc 锚点（§2.2）+ `token_constants_match_production_literals`（运行时复述，断言信息可输出） | R6 |
| A3: exhaustive `AuditClass::from_str` rejects unknown → T-11 fail-closed | `unknown_class_is_rejected_fail_closed`（"user"/"system"/""/"MESSAGE"/"admin " 全 Err——大小写与空白敏感）；`class_all_is_exhaustive`（`ALL` 恰 3 元素且 as_str 覆盖 message/room/admin）；`class_serde_rejects_unknown_variant`（`"\"system\""` 反序列化 Err）；`class_values_match_governance_literals`（== governance.rs 三值）；`priority_from_i16_rejects_unknown_lanes`（0/1/99/101/-1 → None）；`priority_lane_values_and_direction`（10/100 往返 + `Moderation > Backlog` 断言 + serde 整数线格式） | R2/R3 |
| A4: wire-shape JSON of claim contract matches 37-tests fixtures | `claim_wire_shape_is_exact`（`serde_json::to_string` 键名恰 `{"iss","aud","scope","sub"}` / `{"token_endpoint","client_id","client_secret","scope","resource"}`，snake_case 无多余键）；`aud_accepts_single_or_array`（`"aud":"single"` 与 `"aud":["multi"]` 皆入，序列化恒为数组）；`scope_contains_is_tokenized`（"audit:event:write" / "a b audit:event:write c" → true；"audit:event:read" → false；空 scope → false）；`client_credentials_round_trips`；`delivery_mode_default_round_trips`（Default 序列化稳定） | R5/R7 |
| A5: repo 与 relay 引用同一枚举（status 3 = dead-terminal，不漂移） | `dead_is_terminal_and_never_reclaimable`（`is_terminal(Dead)` && `!is_claimable(Dead)`；Delivered 非 terminal 非 claimable）；`is_claimable_equals_claim_predicate`（Pending\|Claimed ⇔ B5-1 谓词 `WHERE status IN (0,1)`）；`status_values_match_ddl_contract`（const 断言 0..3 + from_i16 拒绝域 = CHECK 同构） | R4/R8 |

附加（设计级，非 A1–A5 但保护 §3/§4 约束）：`class_from_str_round_trips`、`priority_json_rejects_string`（`"\"100\""` Err）、`audit_delivery_id_distinct_from_audit_id`（`TypeId` 不同，复刻 ids.rs `distinct_id_types_do_not_mix` 先例）。

## 7. Cross-slice handoff（交接契约，本方向只供类型）

| 切片 | 消费符号 | 语义义务 |
|---|---|---|
| B5-1（aero-storage，0239 DDL + `audit_governance.rs`） | `AuditOutboxStatus`（INSERT status=0、claim 谓词用 `is_claimable`）、`AuditClass`（class 列）、`AuditPriority`（priority 列 + 默认 Backlog=10）、`AuditDeliveryId`（delivery_id/event_id）、`DeliveryMode`（列默认与 `Default` 对齐，R-1 先行） | DDL `CHECK (status IN (0,1,2,3))` 与 `from_i16` 拒绝域同构；`delivery_id UUID` 由 `AuditDeliveryId::to_uuid` 显式写入 |
| B5-2（aero-audit-connector） | `AuditClaim`（iss/aud/scope/sub 校验目标 + `has_scope`）、`ClientCredentials`（cc grant 拼装）、`AuditDeliveryId`（Idempotency-Key/event_id）、`AuditOutboxStatus::Dead`（死终态）、未知 `AuditClass` 的 T-11 映射 | claim 校验每次投递前置，不匹配 fail-closed 不 POST（E13 R6）；dead 后绝不重试 |
| B5-3（aero-storage，claim 排序） | `AuditPriority`（`ORDER BY priority DESC`，100>10 保证 moderation 先达） | 反饥饿上限仍按 B5-3 spec，本类型只保证值序 |

## 8. Out-of-scope（边界提醒，勿在本模块实现）

- 0239 DDL / `audit_governance.rs` repo / enqueue-reconcile 重定向 / 状态转移实现 → **B5-1**；connector crate、lease/backoff/dead 转移、claim 校验运行时、403/422→dead → **B5-2**；`claim_due ORDER BY priority DESC` → **B5-3**；`RoomEvent → (class, action, target)` 分类函数与 `RelayOutcome` 映射（analysis direction #2/#3，未入选）。
- 改 `AuditRepo::append` 签名 / 迁移三处 token 调用点 / aero-ai governance.rs 重构为引用 aero-common（sibling 切片）。
- error.rs 加 422/terminal 分类（direction #3 的活）。
