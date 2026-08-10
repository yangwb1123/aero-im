# Requirements Spec — 把 B5 契约词表单源化为 aero-common 叶子类型（`model::audit`：status 0/1/2/3、class、outbound action token、`Claim` 以 `AuditId` 定型）

- **Module (analysis root)**: `crates/aero-common`（AGENTS.md crate 地图叶子层）；消费面含 `crates/aero-audit-connector`（新增依赖）、`crates/aero-ai/src/governance.rs`（re-export 链）、`crates/aero-eng/src/audit_provision.rs`（Q3 桶解析）、`crates/aero-storage/src/audit_governance.rs`（测试字面量）
- **Direction**: "Single-source the B5 contract vocabulary as typed aero-common leaf types (model::audit): status 0/1/2/3, class, outbound action tokens, and a Claim typed on AuditId"（value 10 / risk_reduction 9 / effort 4 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-common-4afe6237.json`（direction #0）
- **Campaign**: `aero-im-b5-outbox-relay`；in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（v2 契约正文在仓外，[PROPOSED]）
- **Sibling specs（同批次，边界协调）**: `2026-08-08-aero-audit-connector-b5-1-0239-landing.req.md`（0239 DDL 落地 = 本词表的 DB 孪生）、`2026-08-08-aero-audit-connector-b5-3-claim-lane-carriage.req.md`（GovernanceLane 映射所有权）、`2026-08-08-aero-ai-b5-1-migration-0239-governance-outbox.req.md`
- **Status**: Requirements（下述证据全部经源码 grep + **实跑**核对：`cargo test -p aero-common` / `-p aero-audit-connector` / `cargo check --workspace` 基线全绿；行号为核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-08

> ⚠️ **与 analysis 的关键差异（§1.1 逐条更正）**：① direction 断言「0239 DDL [PROPOSED]、migrations 尾号 0238」——tracked 基线确为 0238，但工作树 **untracked** 已有 `0239_audit_governance_outbox.sql` + `0240` + `0241`（status/class 的 DB 侧 CHECK 已落地，本 direction 不改 DDL）；② direction 验收引用的套件计数 `state_machine` 9 / `claim_validation` 14 与仓内不符——实跑为 **7 / 11**（§1.1 勘误）；③ `admin.content.flag` 现况不止 governance.rs 一处，drill bin 与 aero-storage 测试另有字面量——rg=0 验收需枚举清理点（§4 R6/R7）。

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-common/src/ids.rs:122`（AuditId） | ✅ **Verified**。`define_id!`（ids.rs:12）宏展开出 Ulid 新类型：`AuditId` 在 :122（doc "Identifies a single audit-trail event (workspace administration log)"）。宏自带 `nil()`（:52-58，all-zero 系统 actor 哨兵）、`to_uuid()`/`from_uuid()`（:35-46）、`Display`（:66-70，**client.rs 的 `claim.event_id.to_string()` 换型后零改动**）、`From<AuditId> for uuid::Uuid`（:92-95） |
| E2 | `crates/aero-audit-connector/Cargo.toml`（deps:12 — 无 aero-common） | ✅ **Verified**。`[dependencies]` = tokio/tokio-util/async-trait/futures/reqwest/sqlx/serde/serde_json/thiserror/anyhow/tracing/time/uuid/base64——**无 aero-common**。需新增 `aero-common.workspace = true`（§4 R3） |
| E3 | `crates/aero-audit-connector/src/outbox.rs:27-37`（`Claim { event_id: Uuid, payload: Value }`） | ✅ **Verified（struct 在 :34，字段齐 :34-49）**。`Claim { event_id: Uuid, claim_token: Uuid, lease_expires_at: OffsetDateTime, attempts: i64, payload: Value, priority: i16, class: String }`；doc :27-33 自述 "`event_id` is the stable audit event id (`audit_events.id` — B5-1 1:1 parity)"——**1:1 规则目前只是 doc 惯例**。`OutboxRepo` trait（:54-）的 `settle`/`requeue`/`mark_dead` 全部以 `event_id: Uuid` 作 fence 参数 |
| E4 | `crates/aero-audit-connector/src/pg.rs:27-31`（本地 STATUS_ENQUEUED..STATUS_DEAD） | ✅ **Verified（常量在 :28-31，:27 为 doc）**。`STATUS_ENQUEUED: i32 = 0 / CLAIMED = 1 / DELIVERED = 2 / DEAD = 3`，doc 自述 "B5-1 0239 DDL normative values"。**全仓唯一引用点 = 定义自身 + 0239 DDL 注释 pin**（`rg STATUS_ENQUEUED` crate 内零消费）——换派生别名零破坏 |
| E5 | `crates/aero-ai/src/governance.rs:31,33,56`（优先级 + outbound token） | ✅ **Verified（行号 :31/:33/:56）**。`GOVERNANCE_PRIORITY_MODERATION: i16 = 100` :31、`GOVERNANCE_PRIORITY_BACKLOG: i16 = 10` :33（doc 自述 "also the 0239 column default"）、`MODERATION_OUTBOUND_ACTION: &str = "admin.content.flag"` :56；另有 `GOVERNANCE_CLASS_ADMIN/MESSAGE/ROOM` :37-41（"admin"/"message"/"room"）、`LOCAL_ACTION_MODERATED = "message.moderated"` :49、`GovernanceLane` :64-91（class/priority/outbound_action/status 映射，:197 测试钉死 `MODERATION_OUTBOUND_ACTION == "admin.content.flag"`） |
| E6 | `crates/aero-eng/src/audit_provision.rs:48`（Q3_SQL 状态桶字面量） | ✅ **Verified**。`Q3_SQL` :48（"0239 four status buckets (0=pending, 1=claimed, 2=delivered, 3=dead)"）；`parse_buckets` :271-284 把 psql `status\|count` 行解析进 `[i64; 4]`，**未知 status 直接按索引越界忽略**（:283-284）——状态词表以 usize 索引散落，未走叶子枚举 |
| E7 | `migrations/`（0238 latest — 0239 DDL [PROPOSED]） | ⚠️→✅ **半过时（核心校正）**。tracked 尾号确为 `0238_message_recall.sql`（`git ls-files migrations/` 末位），但工作树 **untracked** 已有 `0239_audit_governance_outbox.sql`、`0240_audit_governance_due_prio_idx.sql`、`0241_governance_reconcile.sql`（`git status --short migrations/` 三个 `??`）。0239 含 `status INTEGER NOT NULL DEFAULT 0 CHECK (status IN (0,1,2,3))` + `class TEXT NOT NULL DEFAULT 'message' CHECK (class IN ('admin','message','room'))` + cross-slice pin 注释（"connector status machine (pg.rs:27-30)"、"governance.rs:31/:33/:34/:49/:60"）。**本 direction 不改 DDL**——SQL 无法 import Rust，CHECK 字面量 = DB 侧单源；本 direction 交付 Rust 侧单源，两者经 R6/R7 的交叉断言互钉 |
| E8 | （补充）`docs/proposals/audit-contract-batch-aero-im.md` | ✅ **Verified**。:8 B5-1（0239 表 status 0/1/2/3 normative、class message/room/admin、priority、delivery_mode）；:8 "P2 parity = `event_id` 1:1 断言"——即 `Claim.event_id` 与 `audit_events.id` 的 1:1，本 direction 把它从 doc 惯例升级为编译期不变量 |
| E9 | （补充）`scripts/b5-pin.sh` | ✅ **Verified**。37 槽 = 15 执行 + 22 [PROPOSED]；`audit_governance::` 槽 :37（cargo-test 过滤前缀，非空转守卫见 :80-130 与 `scripts/test-b5-pin-guard.sh`）、`t11-fail-closed` 槽 :40、`moderation-priority-drill` :41、`audit-provision-check` :46 |
| E10 | （补充）aero-common 叶子可行性 | ✅ **Verified**。`src/model/` 已存在（mod.rs 目录 + `pub use *` 扁平化惯例）；serde/serde_json/serde_with 已在 `[dependencies]`；dev-deps 有 pretty_assertions + serde_json（serde 测试先例 `model/tests.rs`）；lib.rs :40 显式 re-export 清单（`pub use model::{…}`）需补 audit 符号 |

### 1.1 对 direction 陈述的勘误/钉化（evidence-backed）

- **套件计数 9/14 → 实跑 7/11**：`cargo test -p aero-audit-connector` 实跑 = lib 8 passed + 3 ignored（pg.rs 需 PG）、`--test state_machine` **7 passed**（stale_token_cannot_ack_after_reclaim / backoff_is_bounded_and_exponential / permanent_error_dead_after_exactly_two_attempts / forbidden_dead_on_first_attempt / happy_path_settles_and_removes_from_claimable / skew_gt_lease_cannot_livelock_claim_fence_settle / priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane）、`--test claim_validation` **11 passed**（wrong_issuer / missing_audience / missing_audit_scope / wrong_subject / opaque_non_jwt / valid_token / unauthorized_refreshes / refreshed_token_must_repass / client_classifies_statuses / payload_guard_rejects / jwt_claims_decode_roundtrip）。direction 的 9/14 与仓内任何历史（sibling spec E10 记 6/11）都不吻合——**验收以「套件全绿」为不变式，计数取实跑值**（§5 AC3）。
- **「0239 DDL [PROPOSED]」→ 已落地（untracked）**：见 E7。本 direction 不依赖 DDL 是否提交；b5-pin 验收的前提 = 工作树全套件绿（§8 提交顺序）。
- **「triplicated」实为五处（非三处）**：status 字面量 = pg.rs:28-31（常量）+ 0239 CHECK（SQL）+ Q3_SQL 注释（aero-eng:48）+ aero-storage 测试断言；class 字面量 = governance.rs:37-41 + 0239 CHECK/DEFAULT + aero-storage 断言 + drill SQL bind + 两 connector 测试注释；`admin.content.flag` = governance.rs:56（常量）+ :197（测试断言）+ :53/:159（doc）+ drill bin :68（`MODERATION_ACTION` 局部常量）+ aero-storage/audit_governance.rs :17/:243/:319/:820/:858/:883（doc + 断言）。**rg=0 验收的清理点清单 = §4 R6/R7 枚举，全部 verified**。
- **`Claim.event_id: AuditId` 的换型波及面（verified）**：`Claim` 构造点仅 3 处——`fake.rs:231`（fake 行 map 键 `BTreeMap<Uuid, FakeRow>` :79）、`pg.rs:59`（`From<ClaimRow>`）、`tests/claim_validation.rs:39`（helper `claim()`）；消费点 = `relay.rs:183`（`let event_id = claim.event_id` → 直通 settle/requeue/mark_dead）与 `client.rs:143/:385`（`to_string()`，AuditId 有 Display → 零改动）。trait 五方法中 settle/requeue/mark_dead 的参数 `event_id: Uuid` 一并换 `AuditId`（fence 也要编译期 1:1），pg.rs 在 SQL bind 边界 `.to_uuid()`。drill bin 的种子 INSERT 是直写 SQL 测试夹具（`Uuid::new_v4()` 合成行），**不属 Claim 类型面，不动**。

## 2. Verified current state（实跑证据）

```
基线（本 spec 亲自实跑，2026-08-08）：
  cargo check --workspace                → 干净（无输出）
  cargo test -p aero-common               → 3 passed; 1 ignored
  cargo test -p aero-audit-connector      → lib 8 passed + 3 ignored（pg.rs 需 PG）
                                            state_machine 7 passed（DB-free，fake+stub）
                                            claim_validation 11 passed（DB-free）
  cargo test -p aero-ai governance::       → 6 passed（governance 单测基线）
  cargo test -p aero-eng --test audit_provision → 25 passed（含 parse_buckets 四臂）
  scripts/b5-pin.sh                       → 37 槽（15 执行 + 22 [PROPOSED]），audit_governance:: /
                                            t11-fail-closed / moderation-priority-drill /
                                            audit-provision-check 槽位在列（未实跑——需 test-integration 全链）

现状缺口（本 direction 关闭，全部 verified）：
  a) B5 词表无叶子类型：status 0/1/2/3 仅 pg.rs:28-31 未消费常量 + SQL CHECK + 注释；
     class 仅 governance.rs:37-41 &str 常量；outbound token 仅 governance.rs:56 &str 常量
     ——aero-common（叶子层、AGENTS.md crate 地图钦定共享契约位）只有 AuditId 一个 B5 符号
  b) connector 无 aero-common 依赖（E2）；Claim.event_id: Uuid——「event_id 1:1 与
     audit_events.id」是 doc 惯例（outbox.rs:27-33），非编译期不变量
  c) 'admin.content.flag' 在 4 个 crate 6 个文件出现（§1.1）——漂移会静默把 B5-3
     moderation 行投错车道（触发词改为别的字符串 = 无任何编译/测试报错）
  d) aero-eng parse_buckets 用 usize 索引散落状态词表（audit_provision.rs:271-284），
     与叶子枚举无关联
```

**Gaps this direction closes**（all verified）：① 叶子 `model::audit` 类型化词表（R1）；② connector 依赖 + `Claim.event_id: AuditId` 编译期 1:1（R3/R4）；③ governance.rs re-export 链单源（R5）；④ rg=0 清理（R6/R7）；⑤ aero-eng 桶解析走叶子枚举（R8）。

## 3. Scope

**In scope**：
- `crates/aero-common/src/model/audit.rs`：`OutboxStatus`（0/1/2/3，serde 整数往返）、`AuditClass`（message/room/admin，serde lowercase）、action-token 常量（`MODERATION_OUTBOUND_ACTION` + `LOCAL_ACTION_MODERATED`）、class 派生别名常量；`model/mod.rs` + `lib.rs` re-export 接线
- `crates/aero-audit-connector/Cargo.toml` 增 `aero-common.workspace = true`；`Claim.event_id: AuditId` + trait `settle/requeue/mark_dead` 参数换 `AuditId`；pg.rs SQL 边界 `.to_uuid()` + `STATUS_*` 改派生别名；fake.rs 行键换 `AuditId`
- `crates/aero-ai/src/governance.rs`：词表常量改 `pub use aero_common::model::audit::{…}` re-export（**保留 `aero_ai::governance::*` 与 `aero_ai::*` 双路径**，0239 DDL 注释 pin 与 sibling 消费方零破坏）；:197 字面量断言迁至叶子测试；:53/:159 doc 改写不拼写 token
- `crates/aero-eng/src/audit_provision.rs`：`parse_buckets` 状态解析改走 `OutboxStatus::from_i32`（行为不变，词表单源）
- `crates/aero-storage/src/audit_governance.rs`：:319/:883 断言改对比叶子常量；doc 注释改引叶子符号（aero-storage 依赖 aero-common 合法、依赖 aero-ai 非法——叶子正是唯一合法通道）
- `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`：:68 `MODERATION_ACTION` 局部常量改 import 叶子

**Out of scope（红线）**：
- **GovernanceLane / governance_lane_for / is_admin_class 迁移到叶子**——映射所有权属 sibling direction（analysis direction #2），本 direction 只单源化其引用词表，不移动映射本身
- **`GOVERNANCE_PRIORITY_*`（100/10）**——优先级是车道值，不在本 direction 词表（status/class/action token）内，留在 governance.rs
- **`AppConfig`/`RelayConfig` audit 段、`Error::RelayTerminal` 403/422/409 分类**——analysis direction #1（leaf relay/scope contract）范围
- **0239/0240/0241 DDL 内容改动**——SQL 侧 CHECK 已是 DB 单源；本 direction 只加 Rust 侧孪生 + 交叉断言
- **L1 聚合、`enqueue_in_tx`、claim 排序改动**——[PROPOSED]/B5-3 所有权
- **drill bin 种子 INSERT 的 status/class SQL 字面量**——测试夹具直写 SQL，非类型面；rg=0 验收仅限 `admin.content.flag`（SQL 无此 token）
- **`FakeStatus`（fake.rs 测试替身自有枚举）**——替身内部表示，不动
- 新依赖：**零新增**（serde/serde_json 已在 aero-common；serde_repr 不引入，手写 ~25 行 serde impl）

## 4. Requirements

### R1 — 叶子模块 `crates/aero-common/src/model/audit.rs`（词表单源）

- `OutboxStatus`：`#[repr(i32)]` 枚举 `Enqueued = 0 / Claimed = 1 / Delivered = 2 / Dead = 3`，配 `as_i32()` 与 `from_i32(i32) -> Option<Self>`（越界返回 None）；**手写 `Serialize`/`Deserialize`**（序列化为 i32，反序列化经 `from_i32`，未知值报 serde 错）——与 0239 `CHECK (status IN (0,1,2,3))` 及 psql Q3 整数输出同构。零新依赖。
- `AuditClass`：`Message / Room / Admin`，`#[derive(Serialize, Deserialize)] #[serde(rename_all = "lowercase")]` → "message"/"room"/"admin"（与 0239 `CHECK (class IN ('admin','message','room'))` 及 `GOVERNANCE_CLASS_*` 字符串逐一对应）；配 `as_str()`。
- 常量（词表唯一定义点）：
  - `pub const MODERATION_OUTBOUND_ACTION: &str = "admin.content.flag";`（**全仓唯一合法字面量**，§5 AC4）
  - `pub const LOCAL_ACTION_MODERATED: &str = "message.moderated";`
  - `pub const GOVERNANCE_CLASS_ADMIN: &str = AuditClass::Admin.as_str();` 及 `MESSAGE`/`ROOM` 同型（派生别名，保 `aero_ai::governance::GOVERNANCE_CLASS_*` 链 + 0239 DDL 注释 pin 文本稳定）
- 接线：`model/mod.rs` 增 `pub mod audit;` + `pub use audit::*;`；`lib.rs:40` 显式 re-export 清单补 audit 符号（house 惯例）。

### R2 — 叶子 serde/词表测试（`#[cfg(test)]` in-module + `model/tests.rs` 惯例）

- `OutboxStatus`：serde_json 往返 `0/1/2/3` ↔ 四变体；`from_i32(4)`/`from_i32(-1)` → None；`as_i32()` 与 repr 一致。
- `AuditClass`：JSON "message"/"room"/"admin" 往返；`as_str()` 与 serde rename 一致。
- 词表 pin：`MODERATION_OUTBOUND_ACTION == "admin.content.flag"`、`LOCAL_ACTION_MODERATED == "message.moderated"`、`GOVERNANCE_CLASS_* == AuditClass::*.as_str()` —— governance.rs:197 的字面量断言迁到这里（§4 R5）。
- 现有 `cargo test -p aero-common` 基线（3 passed/1 ignored）不回归。

### R3 — connector 依赖 aero-common + `Claim.event_id: AuditId`（编译期 1:1）

- `crates/aero-audit-connector/Cargo.toml` `[dependencies]` 增 `aero-common.workspace = true`。
- `outbox.rs:34`：`Claim.event_id: AuditId`；doc :27-33 改写为「`event_id: AuditId` = `audit_events.id` 的编译期 1:1（P2）」。
- `OutboxRepo` trait：`settle`/`requeue`/`mark_dead` 的 `event_id: Uuid` 参数换 `AuditId`（fence 全链同不变量）；`reconcile`/`claim_due` 签名不动。
- 后果（verified 波及面）：`relay.rs:183` 直通零改动；`client.rs:143/:385` `to_string()` 走 Display 零改动；`fake.rs:79` 行键 `BTreeMap<AuditId, FakeRow>`、trait impl 换参、:231 `Claim { event_id: id, .. }` 直通；`pg.rs` `From<ClaimRow>` + SQL bind 在边界 `.to_uuid()`（`From<AuditId> for Uuid` 已存在，E1）；`tests/claim_validation.rs:39` helper 换 `AuditId::new()`/`from_uuid`。

### R4 — pg.rs 状态常量改派生别名（零字面量，pin 文本稳定）

- `pg.rs:28-31`：`pub const STATUS_ENQUEUED: i32 = OutboxStatus::Enqueued as i32;` 等四行（从 `aero_common::model::audit::OutboxStatus` 派生；常量名保留——0239 DDL 注释 pin "connector status machine (pg.rs:27-30)" 与潜在外部引用零破坏；仓内当前零代码消费，纯文档锚）。
- SQL 谓词/绑定处的 0/1/2/3 字面量（`status IN (0,1)` 等）**不动**（SQL 侧单源，见 §3）。

### R5 — governance.rs re-export 链单源（映射所有权不动）

- `crates/aero-ai/src/governance.rs`：删除本地 `MODERATION_OUTBOUND_ACTION`/`LOCAL_ACTION_MODERATED`/`GOVERNANCE_CLASS_*` 常量定义，改为 `pub use aero_common::model::audit::{MODERATION_OUTBOUND_ACTION, LOCAL_ACTION_MODERATED, GOVERNANCE_CLASS_ADMIN, GOVERNANCE_CLASS_MESSAGE, GOVERNANCE_CLASS_ROOM};` —— `aero_ai::governance::*`（worker/mod.rs 等 crate 内消费）与 `aero_ai::*`（lib.rs:36-39 re-export 链）双路径原样保留。
- `GovernanceLane` / `governance_lane_for` / `is_admin_class` / `GOVERNANCE_PRIORITY_*` **留在 governance.rs**（§3 红线）。
- :197 测试臂 `assert_eq!(MODERATION_OUTBOUND_ACTION, "admin.content.flag")` 删除——规范值 pin 迁至 R2 叶子测试；`outbound_action_is_single_contract_token` 其余断言（`lane.outbound_action == MODERATION_OUTBOUND_ACTION` 等）保留（无字面量）。
- :53/:159 doc 注释改写为引用叶子符号（如 "the single locked outbound token, see `aero_common::model::audit::MODERATION_OUTBOUND_ACTION`"），不拼写 token 文本。
- 现有 governance 6 项单测全绿（实跑基线：sibling spec E7 记 6 passed；本 spec 复跑确认）。

### R6 — `admin.content.flag` rg=0 清理（§5 AC4 的可执行前提）

以下 12 处字面量/引用全部改走叶子（枚举清单 = 2026-08-08 `rg -n 'admin\.content\.flag' crates/ --glob '*.rs'` 实测）：

| 文件 | 行 | 现况 | 改法 |
|---|---|---|---|
| `crates/aero-ai/src/governance.rs` | :53 | doc 拼写 token | 引叶子符号（R5） |
| 同上 | :56 | 常量定义 | `pub use`（R5） |
| 同上 | :159 | doc 拼写 token | 引叶子符号（R5） |
| 同上 | :197 | 测试断言字面量 | 删除（R5） |
| `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs` | :15 | doc 拼写 token | 引叶子符号 |
| 同上 | :68 | `const MODERATION_ACTION: &str = "admin.content.flag"` | `use aero_common::model::audit::MODERATION_OUTBOUND_ACTION`（crate 已依赖叶子，R3） |
| `crates/aero-storage/src/audit_governance.rs` | :17 | doc 引号 token | 引叶子符号 |
| 同上 | :243 | doc | 引叶子符号 |
| 同上 | :319 | `assert_eq!(gov[0].4["action"], "admin.content.flag")` | 对比 `MODERATION_OUTBOUND_ACTION`（aero-storage 依赖 aero-common 合法） |
| 同上 | :820 | doc | 引叶子符号 |
| 同上 | :858 | doc | 引叶子符号 |
| 同上 | :883 | `assert_eq!(gov.4["action"], "admin.content.flag")` | 对比叶子常量 |

### R7 — aero-storage 交叉断言（叶子 ↔ 0239 DDL 互钉）

- `audit_governance.rs` 六项 db_tests（`moderation_finalize_outbox_parity` / `ddl_contract_defaults_and_checks` / `moderation_finalize_runtime_disabled_commits_1_plus_0` / `non_moderation_action_passes_through_unmapped` / `duplicate_event_id_is_deduped_by_on_conflict` / `governance_reconcile_backfills_disabled_window`）在 R6 改动后保持全绿——这些测试断言 0239 触发器产出的 class/priority/action 字面量，改对比叶子常量后成为**叶子 ↔ DDL 的活交叉 pin**（b5-pin `audit_governance::` 槽非空转守卫继续成立）。

### R8 — aero-eng Q3 桶解析走叶子枚举

- `audit_provision.rs:271-284` `parse_buckets`：`usize` 索引解析改为 `OutboxStatus::from_i32(i32)` → `Some(status) => buckets[status as usize] = count`，`None`（越界）保持现行为忽略。行为语义不变（未知 status 依旧跳过），词表从叶子枚举解析。
- 既有套件不回归（本 spec 实跑基线）：`cargo test -p aero-eng --test audit_provision` → **25 passed**，含 `parse_buckets_fills_missing_statuses_with_zero`（`"0|3\n2|10"` → `[3,0,10,0]` 等 4 臂，:353-357）、`report_contains_greppable_lines_with_four_buckets_and_age` :195 等。

## 5. Acceptance（direction 验收逐条保留 + 可测试化）

### AC1 — 叶子模块与 serde 测试

`crates/aero-common/src/model/audit.rs` 存在（status 枚举 0/1/2/3、class 枚举 message/room/admin、action-token 词表），且：
- `cargo test -p aero-common` → 全绿（基线 3 passed/1 ignored + R2 新增测试）；
- R2 测试覆盖：status serde_json 整数往返 0..3、`from_i32` 越界拒绝、class "message"/"room"/"admin" 往返、`MODERATION_OUTBOUND_ACTION`/`LOCAL_ACTION_MODERATED`/`GOVERNANCE_CLASS_*` 规范值 pin。

### AC2 — 编译期 1:1（Claim 以 AuditId 定型）

`cargo check --workspace` 干净，且：
- `crates/aero-audit-connector/Cargo.toml` 含 `aero-common.workspace = true`；
- `Claim.event_id: AuditId`，`OutboxRepo::settle/requeue/mark_dead` 参数 `AuditId`；
- **编译期证明**：任意 `Claim { event_id: <Uuid>, .. }` 或 `repo.settle(<Uuid>, ..)` 构造为类型错误（1:1 规则从 doc 惯例升级为编译器强制——可验证：临时把 `event_id: AuditId::new()` 换成 `Uuid::new_v4()` 即编译失败）。

### AC3 — connector 既有套件不回归

- `cargo test -p aero-audit-connector --test state_machine` → **7 passed**（direction 引用 9 系计数漂移，§1.1 勘误；以实跑 7 为锚，套件构成不变）；
- `cargo test -p aero-audit-connector --test claim_validation` → **11 passed**（同勘误）；
- lib 侧 8 passed + 3 ignored（pg.rs 需 PG，`-- --ignored` + `DATABASE_URL` 门控，AGENTS.md §4.3）不回归。

### AC4 — `admin.content.flag` 单源

- `rg 'admin.content.flag' crates/ --glob '*.rs'` 在 `crates/aero-common` 之外 **返回 0**（R6 十二处清理点全部落地；叶子内仅剩常量定义 + 规范值 pin 测试）；
- `scripts/b5-pin.sh` 37/37 槽绿：`audit_governance::`（R7 六 db_tests 在 throwaway 已迁移库 `--ignored` 实跑全绿，非空转守卫 `test result: ok. [1-9][0-9]* passed` 成立）+ `t11-fail-closed` + `moderation-priority-drill` + `audit-provision-check` 槽位 verdict 齐全（`scripts/test-b5-pin-guard.sh` 绿）。

### AC5 — aero-eng 桶解析走叶子枚举

- `parse_buckets` 状态解析经 `OutboxStatus::from_i32`；`cargo test -p aero-eng --test audit_provision` → 25 passed（含 `parse_buckets_fills_missing_statuses_with_zero` 四臂原样通过——行为不变证明）。

## 6. Test placement

| 测试 | 位置 | 门 |
|---|---|---|
| OutboxStatus serde 往返/越界、AuditClass 往返、词表 pin | `crates/aero-common/src/model/audit.rs` `#[cfg(test)]`（in-module，或 `model/tests.rs` 同款） | `cargo test -p aero-common` |
| Claim/OutboxRepo 换型编译期证明 | 编译期（无新测试；既有 state_machine/claim_validation 即回归面） | `cargo check --workspace` + AC3 |
| governance re-export 链 | 既有 6 单测（无字面量断言改动面） | `cargo test -p aero-ai governance::` |
| aero-storage 交叉 pin（叶子 ↔ 0239） | 既有 6 db_tests（断言改对比叶子常量后语义更强） | `cargo test -p aero-storage --lib audit_governance:: -- --ignored`（throwaway 已迁移库） |
| aero-eng 桶解析 | 既有 audit_provision 测试（行为不变） | `cargo test -p aero-eng --test audit_provision`（25 passed） |
| rg 单源扫描 | 手动/CI 命令 | `rg 'admin.content.flag' crates/ --glob '*.rs'`（排除 aero-common）→ 0 |
| b5-pin | 既有 | `scripts/test-b5-pin-guard.sh` + `scripts/test-integration.sh` B5 段 |

## 7. Risks / [PROPOSED]

- **direction 计数 9/14 与仓内 7/11 不符**（高）：已在 §1.1 勘误并以「套件构成不变 + 全绿」为验收；若未来套件再增测试，计数锚点会继续漂移——验收措辞以命令 + 全绿为不变式，计数仅作核对参考。
- **rg=0 涉及 aero-storage/audit_governance.rs 六行 doc 注释改写**（低）：纯文本改动，无行为面；但该文件与 connector 全 crate、governance.rs、0239-0241 均 **untracked**——本 direction 交付必须连带提交，否则验收无基线（§8 顺序）。
- **`Claim.event_id` 与 trait 参数换型 = 公开 API 变更**（低）：connector `publish = false`、仅 workspace 内消费，无 semver 面；波及面已 enumerated（§1.1，构造 3 处 + 消费 4 处，全部 verified）。
- **叶子与 DDL 的互钉依赖 aero-storage db_tests 实跑**（中）：R7 的活 pin 需 PG 环境；CI 无 PG 时以 `--ignored` 门控 + b5-pin 非空转守卫兜底（现状即如此，无新增面）。
- **`[PROPOSED]`（不在本 direction）**：L1 聚合、`enqueue_in_tx`、`AppConfig::AuditRelay` 段、403/422/409 终端分类、GovernanceLane 叶子化——均属 sibling directions，本 spec 不实现。

## 8. Sequencing / verification

1. R1+R2：叶子 `model/audit.rs` + 接线 + 测试 → `cargo test -p aero-common`（先绿）；
2. R3+R4：connector 依赖 + Claim/trait 换型 + pg.rs 别名 → `cargo check --workspace` + `cargo test -p aero-audit-connector`（AC2/AC3）；
3. R5：governance.rs re-export → `cargo test -p aero-ai`；
4. R8：aero-eng 桶解析 → `cargo test -p aero-eng`；
5. R6+R7：drill bin + aero-storage 清理 → `rg` 扫描 = 0；PG 门控 db_tests（throwaway 库 `make migrate-smoke` 流程，AGENTS.md §4.3）；
6. 收尾：`cargo clippy --workspace --all-targets`（不新增警告）· `scripts/{truth-check,file-size-check,web-check}.sh` · **提交全部 untracked B5 文件（connector crate、governance.rs、audit_provision.rs、0239-0241、audit_governance.rs、drills、b5-pin.sh 改动）**——这是 AC4/AC5 的基线前提；
7. 全链：`scripts/test-integration.sh`（或 `make gate-b5` 等价）→ b5-pin 37/37 含 `audit_governance::`/`t11-fail-closed` 槽 verdict 齐全。
