# Design — L1-aggregation wire contract at the leaf（aero-common model/audit.rs）

- **Module**: `crates/aero-common`（leaf）— ids.rs + model/audit.rs + lib.rs；守卫脚本 `scripts/truth-check-lib.sh`；PG 门控 db_tests 落在 `aero-storage/src/audit_governance.rs` 与 `aero-audit-connector/src/pg.rs`
- **上游**: `docs/auto/specs/crates-aero-common-src-630e5499-l1-aggregation-wire-contract.md`（R1–R8，requirements-ready）；sibling `docs/auto/specs/crates-aero-common-4afe6237-typed-outbound-claim-payload.md`
- **对象状态机**: 0239/0240/0241 DDL + `OutboxRepo`/`Claim`/connector 四操作（**本设计零改动**）
- **Status**: Design。证据核对日期 2026-08-08（全部实读/实跑，见 §1 账本）

## 0. 设计摘要（TL;DR）

纯 leaf 词汇层交付：**零 DDL、零 connector/仓储 trait 改动、零 aero-ai 改动、零新依赖、无新 b5-pin 槽（37/37 保持）**。交付物：

```
R1  crates/aero-common/src/ids.rs            define_id!(AuditWindowId)（与 AuditId 类型不相交）
R2  crates/aero-common/src/model/audit.rs    DeliveryMode 枚举（0239 delivery_mode CHECK 的 Rust twin）
R3  crates/aero-common/src/model/audit.rs    AGGREGATED_MESSAGE_ACTION = "message.batch" 动作 token
R4  crates/aero-common/src/model/audit.rs    AuditWindowPayload 19 字段聚合信封 + new() + span_is_ordered()
R5  （R4 内）idempotency_key == window_id（窗口身份派生，跨 requeue/reclaim 稳定，与 1:1 键空间不相交）
R6  （决策）OutboxStatus 0/1/2/3 原样复用；Dead 即窗口终态；无新 variant
R7  scripts/truth-check-lib.sh rule 3e       "message.batch" 字面量 → leaf 文件守卫（AC4 模式，allowlist 初始空）
R8  db_tests ×2                              window 行过 0239 CHECK + priority DESC 下永不让窗口抢占 moderation
```

一句话：窗口行 = `audit_governance_outbox` 里**今天就能合法存在的一行**（event_id 是任意 UUID、class 'message'、priority 10、payload 是 19 字段 jsonb 对象），本设计把该行的**词汇**（ID 类型、信封、幂等键派生、状态复用）钉在 leaf，使 B5-1 的 [PROPOSED] L1 机制落地时不改任何既有线。

## 1. 证据核对账本（untrusted evidence → 实跑/实读验证）

| Evidence claim | Result |
|---|---|
| `audit.rs` `GOVERNANCE_CLASS_MESSAGE` "L1-aggregatable"；`OutboxStatus`(:20)/`AuditClass`(:122) leaf-pin 模式；**零 window 类型** | ✅ 实质命中。`GOVERNANCE_CLASS_MESSAGE` 实际在 :166（doc 注释 :164-165，差 1 行属行号漂移）；`OutboxStatus` :20 / `AuditClass` :122；`rg -i "window" crates/aero-common/src` 仅 recall-window/markdown `windows(n)`/metrics 命中，**零 outbox window 类型** ✅ |
| `governance.rs:86-88` `is_admin_class` "[PROPOSED] L1 aggregation bypass" | ✅ 精确。doc :86-88、fn :90；`GOVERNANCE_PRIORITY_MODERATION=100`/`GOVERNANCE_PRIORITY_BACKLOG=10` :31/:33 实读确认（R3 的 priority 10 字面量锚） |
| `outbox.rs:42-43` `Claim { event_id: AuditId }` + 双形幂等（header=ULID base32 Display @ client.rs:197；信封字段=连字符 uuid） | ✅ 精确。`client.rs:197` `.header("Idempotency-Key", claim.event_id.to_string())`；`Claim { event_id: AuditId }` :42-44；`AuditId` = `define_id!`（ids.rs:122） |
| `pg.rs` claim/settle/requeue/mark_dead 全 `WHERE event_id = $1` 于 `AuditId`（:99/:141/:186/:220）；`:536` mixed_priority…；`:441` expired_lease… | ⚠️ **行号过期，实质成立**。实际生产 `WHERE event_id = $1` 位点 :148(settle fenced 重读)/:169(settle UPDATE——**settle 才是双查询**：重读+更新同事务)/:204(requeue)/:233(mark_dead) + claim 子查询 :129（`WHERE outbox.event_id = claimable.event_id`）；`:536` `mixed_priority_claim_orders_moderation_first_then_fifo` ✅、`:441` `expired_lease_is_reclaimed_with_fresh_token` ✅ 精确。claim_due ORDER BY `(priority DESC, available_at, created_at, event_id)` :117 实读确认（R8 依赖此已落地排序，零查询改动） |
| `0239:29` PK event_id 1:1；`:36-37` delivery_mode CHECK 'push' reserved，**无 Rust twin (rg=0)** | ✅ 结构成立，⚠️ **字面表述夸大**：全仓 `rg -n "delivery_mode" --glob '*.rs'` = **7 hits**，全部在 `aero-storage/src/audit_governance.rs`（ddl drill :609/:618/:630/:642/:677/:704/:712，断言 DEFAULT 'push' 与 'pull' 被拒）。spec 原文的限定表述（`rg "DeliveryMode\|delivery_mode" crates/aero-common crates/aero-audit-connector` → 0）**成立**。结论：**无 Rust 类型 twin 属实**（SQL CHECK 是唯一规范源，drill 行为性交叉钉），但「rg=0 hits」只对两 crate 范围成立。设计影响：R2 枚举是**首个 Rust 类型**，drill 的 7 处字面量是测试位点非契约位点，不受 rule 3e 管辖（R7 只管 `message.batch`） |
| 0240/0241 均严格 1:1 | ✅ 精确。0240 部分索引 `(priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)`；0241 `WHERE outbox.event_id = audit.id` + `ON CONFLICT (event_id) DO NOTHING` |
| truth-check-lib.sh rule 3d（AUDIT-FLAG allowlist + stale 机制 + exit 折叠） | ✅ 精确。`CLAIM_AUDIT_FILE` :126、`AUDIT_FLAG_ALLOWLIST` :137-140（含 drill :77/:379 两 pin）、`AUDIT_FLAG_ALLOWED` 构造 :166-168、stale 警告 :305（AUDIT-FLAG 专属循环；通用 CLAIM stale 在 :298）、`CLAIM_GUARD_VIOLATIONS` :171 + `claim_guard_scan` :196 折叠进 exit；负例 harness `scripts/test-claim-contract-guard.sh` 存在 |
| "37-test suite" = G6 "37/37、T-11、moderation 优先级"（implementation-gate.md:78） | ✅ 精确。`docs/campaigns/implementation-gate.md:78` 原文；`b5-pin.sh` 有 `audit_governance::`（前缀槽，新 db_test 落入该槽体）+ `t11-fail-closed`/`moderation-priority-drill` 槽；**无新槽 → 37 保持** |
| `audit_events` 无 seq 列（0007/0146 按 ULID id/created_at 排序） | ✅ 精确。0007 列：id/workspace_id/actor_id/action/target/detail/created_at，无 seq；0146 分区 PK `(id, created_at)`；索引 `(workspace_id, id DESC)`。「seq range」→ 含端 ULID 序 `(first_event_id, last_event_id)` 对 ✅ |
| spec 文件已落盘 | ✅ `docs/auto/specs/crates-aero-common-src-630e5499-l1-aggregation-wire-contract.md` 存在，R1–R8/AC1–AC5/§5 变更面/§6 范围完整 |
| `define_id!` 宏面（new/from_ulid/as_ulid/to_uuid/from_uuid/nil/Default/Display/FromStr/From<Ulid>/serde transparent） | ✅ ids.rs:12-118 实读：宏含 `new/from_ulid/as_ulid/to_uuid/from_uuid/nil/Default/Debug(Display=ULID base32)/FromStr/From<Ulid>/From<id> for Ulid/From<id> for uuid::Uuid` + `#[serde(transparent)]`；`PartialOrd/Ord` 由 derive 链提供（span_is_ordered 的 ULID 序基础） |
| lib.rs re-export 链（ids :28 / model :42-53） | ✅ 实读：ids 链字母序（AuditId 在 :28 区）；model 链含 `AuditClaimPayload/OutboxStatus/GOVERNANCE_CLASS_*/MODERATION_OUTBOUND_ACTION` 等；新符号按字母序插入 |
| `ddl_contract_defaults_and_checks` :614、`rust_produced_payload_matches_0239_envelope` :445 | ✅ 精确（aero-storage/src/audit_governance.rs） |

### 1.1 设计推导出的新事实（evidence 未覆盖，design 据此定约束）

| # | 事实 | 来源 | 设计影响 |
|---|---|---|---|
| D1 | **audit.rs 现 550 行**，800 WARN / 1200 HARD（file-size-check.sh:6-7） | `wc -l` 实跑 | R4 结构体+构造函数 ~100 行、测试 ~150 行、doc ~60 行 ⇒ 逼近 800 WARN。**测试必须紧凑**（exact-wire 单断言、8 函数合并共用 fixture），预算上限 ~240 行；超限则测试**整体拆 `crates/aero-common/tests/` 目录**（skip_tests_component 豁免 tests 路径组件 + truth-check §1 只扫 `crates/*/src` + rule 3e 跳过；命令改 `cargo test -p aero-common`，见 §3.7） |
| D2 | `claim_due` 的 lease 与 fence 全读 `clock_timestamp()`（单时钟域）；claim 子查询在 `WITH claimable` 内 `FOR UPDATE SKIP LOCKED LIMIT` | pg.rs:104-132 | AC5 的 requeue→reclaim 需真 1s 租约过期（`expired_lease_is_reclaimed_with_fresh_token` :441 同款）；断言 payload 字节不变即可，不依赖假时钟 |
| D3 | `mark_dead` 的 fence = `claim_token AND attempts AND status IN (0,1) AND lease 未过`；`requeue` 同样四元组 + `available_at` 回退 | pg.rs:195-240 | 窗口行走同一 fence 面——**R6 状态复用零成本**：不加 variant 则 settle/requeue/mark_dead SQL 与 Rust 一行不动 |
| D4 | `payload` CHECK = `jsonb_typeof(payload) = 'object'`（0239:39） | 0239 实读 | R4 信封 `serde_json::to_value` 必为 object —— 窗口行 INSERT 天然满足；AC4 的 INSERT 测试无需绕过任何 CHECK |
| D5 | b5-pin.sh 槽体 `audit_governance::` 是前缀匹配（b5-pin.sh:37；15 executed + 22 [PROPOSED] = 37） | b5-pin.sh:37 | storage 新 db_test `window_row_passes_0239_checks` 落该前缀槽即被覆盖；connector 两个新测试**仅经 §5 step 6 手工命令执行**——`t11-fail-closed`/`moderation-priority-drill` 槽体的判定行来自 aero-cli drill 执行（test-integration.sh:443/:445/:564-577），非 cargo-test 过滤器（语义复用，非 harness 执行）；**不加槽、37 保持** |

## 2. API 变更（逐文件、逐符号）

### 2.1 `crates/aero-common/src/ids.rs`（R1）

```rust
define_id!(
    /// Identifies one L1 aggregation window over high-volume `message.*`
    /// audit rows (aero_governance_outbox row keyed by this UUID, NOT an
    /// AuditId — type-disjoint from every 1:1 row's key).
    AuditWindowId
);
```

插入 `AuditId`（:122）之后。宏面全部继承：`new/from_ulid/as_ulid/to_uuid/from_uuid/nil/Default/Debug/Display(=ULID base32，header 形)/FromStr/From<Ulid>/From<AuditWindowId> for Ulid/From<AuditWindowId> for Uuid/serde transparent`。**类型不相交是编译期事实**：窗口 UUID 永远无法绑定到 `AuditId` 形参（`settle(event_id: AuditId, …)` 拒绝窗口键），`is_admin_class` 的 1:1 保证在类型层成立。

### 2.2 `crates/aero-common/src/model/audit.rs`（R2/R3/R4/R5/R6）

```rust
// ---- R2: delivery_mode Rust twin（0239:36-37 CHECK 的唯一 Rust 拼写）----
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryMode { Push }
impl DeliveryMode {
    #[must_use] pub const fn as_str(self) -> &'static str { "push" }
}
pub const DELIVERY_MODE_PUSH: &str = DeliveryMode::Push.as_str();

// ---- R3: 聚合动作 token（domain.verb 形，与 MODERATION_OUTBOUND_ACTION 同款）----
pub const AGGREGATED_MESSAGE_ACTION: &str = "message.batch";
// doc: window 行 lane = class 'message'（GOVERNANCE_CLASS_MESSAGE）/ priority 10
//（= GOVERNANCE_PRIORITY_BACKLOG，leaf 不能 import aero-ai → 重复字面量 + 交叉注释）
// / delivery_mode 'push'；绝不 class 'admin'（governance.rs:86-88 bypass 不动）。

// ---- R4: 19 字段聚合信封（next to AuditClaimPayload :268）----
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditWindowPayload {
    pub window_id: String,        // uuid 文本 = AuditWindowId::to_uuid().to_string()
    pub count: u32,               // JSON number（N 条折叠；B5-1 A4）
    pub first_event_id: String,   // uuid 文本，窗口最早成员（ULID 序）
    pub last_event_id: String,    // uuid 文本，窗口最晚成员
    pub source_system: String,
    pub event_type: String,       // AUDIT_EVENT_TYPE
    pub schema_id: String,        // AUDIT_SCHEMA_ID
    pub schema_version: u32,      // JSON number = AUDIT_SCHEMA_VERSION
    pub occurred_at: String,      // PG timestamptz→jsonb ISO-8601-with-offset 拼写
    pub actor: AuditActor,        // 窗口行为 server 构建 → {id:'system',type:'system'}
    pub targets: Vec<AuditTarget>,// []（无单一资源；成员在 first/last_event_id + payload）
    pub aggregate_type: String,   // AUDIT_AGGREGATE_TYPE ("workspace")
    pub aggregate_id: String,     // workspace_id::text
    pub action: String,           // AGGREGATED_MESSAGE_ACTION
    pub outcome: String,          // AUDIT_OUTCOME_SUCCESS
    pub payload: serde_json::Value, // 窗口明细（生产者定义）
    pub data_classification: String, // AUDIT_DATA_CLASSIFICATION
    pub retention_class: String,  // AUDIT_RETENTION_CLASS
    pub idempotency_key: String,  // == window_id（R5，构造器强制）
}

impl AuditWindowPayload {
    #[allow(clippy::too_many_arguments)] // 8 可变入参；audit.rs:293 先例
    #[must_use]
    pub fn new(
        window_id: AuditWindowId,
        count: u32,
        first_event_id: AuditId,  // 成员是 audit_events.id → 编译期事实
        last_event_id: AuditId,
        source_system: String,
        occurred_at: String,
        aggregate_id: String,
        payload: serde_json::Value,
    ) -> Self { /* 不变量字段填 AUDIT_* 常量；idempotency_key = window_id.to_uuid().to_string() */ }

    /// 含端 ULID 序区间 pin（audit_events 无 seq 列；0007/0146 按 ULID id 序）
    #[must_use]
    pub fn span_is_ordered(&self) -> bool {
        self.first_event_id <= self.last_event_id   // AuditId derive Ord，uuid 文本序 = ULID 序
    }
}
```

R5 幂等语义（构造器 + 文档钉死）：窗口 `Idempotency-Key`（header 形）= `AuditWindowId` Display（ULID base32）；信封 `idempotency_key` = `window_id` uuid 文本。**仅由窗口身份派生**——不取自任何成员 event_id、不被 claim 机制改写（connector 的 requeue/mark_dead 不碰 payload），at-least-once 重投携同一键（T-11 语义）。键空间与 1:1 行（`AuditId`）类型不相交，sink 永不碰撞。

R6 状态复用（纯决策，无代码）：`OutboxStatus` 0/1/2/3 原样；`Dead`=3 即窗口终态；0239 `status IN (0,1,2,3)` CHECK 与两个部分索引（`status IN (0,1)`，0239:56-59 / 0240）天然覆盖窗口行；connector 四操作零改动。

### 2.3 `crates/aero-common/src/lib.rs`（re-export 链）

- ids 链：`AuditWindowId` 插在 `AuditId` 后（字母序）。
- model 链：`DeliveryMode, DELIVERY_MODE_PUSH, AuditWindowPayload, AGGREGATED_MESSAGE_ACTION` 按字母序并入现有块。**无撞名风险**（四名全仓唯一，`rg` 已验）。

### 2.4 `scripts/truth-check-lib.sh`（R7，rule 3e）

镜像 rule 3d 完整机制：

```bash
# AUDIT-WINDOW allowlist (rule 3e) — "message.batch" 字面量唯一合法位点 =
# CLAIM_AUDIT_FILE（audit.rs）；初始为空，未来 storage parity drill 的 pin 位点
# 由 re-pin 加入（同 AUDIT_FLAG_ALLOWLIST 机制）。
AUDIT_WINDOW_ALLOWLIST=()
# claim_guard_scan 增第五扫描：rg -n -F '"message.batch"' "$root/crates"
#   - 合法：$CLAIM_AUDIT_FILE 或 AUDIT_WINDOW_ALLOWED 命中
#   - skip：tests 组件 / 注释引导行（继承 skip_tests_component / 注释剥离）
#   - stale 条目 → ⚠️ 警告（同 :305）
#   - 违规 → CLAIM_GUARD_VIOLATIONS += 1 → truth-check.sh 非零 exit
```

`scripts/test-claim-contract-guard.sh` 增负例（沿用既有注入模式：**append 字面量至既有非豁免文件**（如 `crates/aero-auth/src/jwt.rs`，harness n1 同款）——**禁新建临时 `.rs` 于 `crates/*/src`**：会触发 truth-check §1 orphan 扫描使 exit 由 1 变 2、破坏折叠断言；注入 `let _u = "message.batch";` → 恰 1 违例 + 非零 exit；leaf 位点（audit.rs）→ clean；**注释行注入 `// "message.batch"` → clean**（is_comment_line 剥离——补 rule 3d 一直缺的显式用例）；**stale 警告种子**：`AUDIT_WINDOW_ALLOWLIST` 初始为空 ⇒ 先向 fixture 的 lib 副本 sed 注入一条 `<path>:<line>` 条目（n8/n9 同手法），再破坏该位点字面量 → 期望 ⚠️ STALE 警告）。

### 2.5 PG 门控 db_tests（R8，`#[ignore = "requires live Postgres"]`）

| 测试 | 位置 | 断言核心 |
|---|---|---|
| `window_row_passes_0239_checks` | `crates/aero-storage/src/audit_governance.rs`（`ddl_contract_defaults_and_checks` :614 形状） | INSERT `(event_id = AuditWindowId::to_uuid(), status 0, class 'message', priority 10, delivery_mode 'push', payload = to_value(AuditWindowPayload::new(…)))` 成功；读回默认；既有负探针（status 4 / 'administrator' / priority 0 / 'pull'）仍红 |
| `window_backlog_never_preempts_moderation` | `crates/aero-audit-connector/src/pg.rs`（:536 形状） | 40 行 backlog 混入窗口形行 + 10 行 admin（priority 100，更晚 available_at）；`claim_due(30s, 25)` → claimed 集合 = {全 10 admin} ∪ {15 最早 backlog∪window} |
| `window_row_idempotency_key_stable_across_requeue_and_reclaim` | `crates/aero-audit-connector/src/pg.rs`（:441 形状） | claim(attempts 1, token A) → requeue（fenced，lease 清空）→ 睡过 **≥ max(backoff(1)=1s, 1s 租约)**（`audit_backoff` 重泊 `available_at`，relay.rs:56；attempts=1 时两者等值，同 1.2s 余量——未来 attempts>1 种子按此式不 flake）→ reclaim(attempts 2, 新 token B)；payload 字节不变（`payload["idempotency_key"]` 未改写）、派生 header 键（`AuditWindowId` Display）两次一致、旧 token A 不可再 ack |

## 3. 兼容性约束（破坏面 = 零，逐条钉死）

1. **无 DDL**：0239/0240/0241 一行不动。窗口行今天就能过全部 CHECK（D4：`event_id` 是任意 UUID、`payload` 是 object、class/priority/delivery_mode/status 全在枚举内）——AC4 证明「可表示」，不是「需要迁移才可表示」。
2. **无 trait/仓储改动**：`OutboxRepo` 五方法签名、`Claim` 五字段、`FakeOutbox`、`PgOutboxRepo` SQL 文本、`client.rs`/`relay.rs`/`stub.rs` 全部不动。窗口行如何被 claim（窗口键操作 vs 1:1 去重）是后续 connector/storage slice 的决策，本设计只保证词汇与幂等键稳定性（AC5 钉住任何 keying 必须保留的性质）。
3. **无状态机分叉**：R6 复用 `OutboxStatus` 0/1/2/3——`STATUS_*` 派生常量（pg.rs:32-35）、settle/requeue/mark_dead 的 fence SQL、0239 CHECK 全部原样。引入窗口专属终态将迫使 connector 逻辑分叉，零收益。
4. **aero-ai 不动**：`is_admin_class` bypass、`GOVERNANCE_PRIORITY_*`（:31/:33）原样；leaf 以重复字面量 10 + 交叉注释锚定 `GOVERNANCE_PRIORITY_BACKLOG`（leaf 无法 import aero-ai，此为既定模式）。
5. **既有 drill 保持绿**：`rust_produced_payload_matches_0239_envelope`（:445）、`ddl_contract_defaults_and_checks`（:614）不受影响；storage drill 的 7 处 `delivery_mode` 字面量是测试位点，rule 3e 只锁 `message.batch`。
6. **G6 37/37 保持**（15 executed + 22 [PROPOSED]）：storage 新测试落 `audit_governance::` 前缀槽；connector 两个新 `#[ignore]` 测试**仅由 §5 step 6 手工命令执行**——`t11-fail-closed`/`moderation-priority-drill` 槽体的判定行来自 aero-cli drill 执行（test-integration.sh:443/:445/:564-577），非 cargo-test 过滤器，是语义复用而非 harness 执行；b5-pin.sh 零改动。
7. **尺寸预算（D1）**：audit.rs 550 → 上限 ~790。测试共用 fixture（`sample_window_payload()`）、exact-wire 单断言；超 800 WARN 则测试**整体拆 `crates/aero-common/tests/` 目录——唯一可行兜底**：skip_tests_component 豁免 `tests` 路径组件、truth-check §1 orphan 扫描只走 `crates/*/src`、rule 3e 跳过，新文件另有独立 800 行预算。**排除的 leg**：内联 `#[cfg(test)] mod tests` 零尺寸缓解（仍在 audit.rs 内）；`audit.rs` → `audit/mod.rs` 目录化会破坏 `CLAIM_AUDIT_FILE="crates/aero-common/src/model/audit.rs"` 字面路径 pin（truth-check-lib.sh:126），rule 3d/3e 全量误报。**命令涟漪**：拆出后 AC1/AC2 命令由 `cargo test -p aero-common --lib` 改为 `cargo test -p aero-common`（integration tests 不走 `--lib`）。
8. **双形幂等契约延续**：1:1 信封不变量 `idempotency_key == event_id` 不被违反（窗口行无 event_id；其不变量为 `== window_id`）；header 形 = Display（ULID base32）两形统一（client.rs:197 模式）。

## 4. 失败模式（F1–F10，含缓解）

| # | 失败模式 | 触发 | 缓解（已内建于设计） |
|---|---|---|---|
| F1 | 信封漂移：未来 SQL 窗口生产者加/改键、`count`/`schema_version` 序列化成字符串 | `deny_unknown_fields` + exact-wire 测试 | 有意为之的漂移警报（sibling 模式）：任何 SQL 侧信封变更先红在 leaf 测试；B5-3 生产者落地时上 `rust_produced_payload_matches_0239_envelope` 式 parity drill 交叉钉 |
| F2 | 幂等键碰撞：窗口键撞 1:1 键 → sink 去重误吞 | 两个键空间都是 UUID 文本 | `AuditWindowId` ≠ `AuditId` **类型不相交**（编译期）；sink 侧键空间按类型分离；AC5 断言重投同键 |
| F3 | 窗口行抢占 moderation：priority DESC 排序被绕过/改写 | 未来 claim 排序回归 | 已落地的 0240 索引 + pg.rs:117 ORDER BY 原样；AC4 `window_backlog_never_preempts_moderation` 锁住「窗口永在 admin 之后」 |
| F4 | 状态机分叉：窗口终态 ≠ Dead → claim 过滤/索引漏行 | 未来加 variant | R6 决策文档化 + AC1 `window_rows_reuse_outbox_status_machine`（含「信封无 status 键」断言） |
| F5 | token 拼写漂移：`"message.batch"` 被改拼 | 字面量散落 | rule 3e 守卫 = 常量名 + 字面量位点双锁（同 `admin.content.flag`）；改拼是一行编辑 + 交叉钉自动跟随 |
| F6 | 「seq range」误解：有人在 leaf 发明 seq 列 | `audit_events` 无 seq | `span_is_ordered()` + AC2 `audit_window_id_roundtrips`（ULID Ord 单调）+ R4 doc 钉「(first,last) 含端对即区间」 |
| F7 | 守卫漏网：`"message.batch"` 出现在注释/测试组件 | 扫描正则误豁免 | 继承 rule 3d 的注释剥离（`is_comment_line`）+ skip_tests_component；负例 harness（append 至既有非豁免文件 → 恰 1 违例 + 非零 exit；注释行注入 → clean） |
| F8 | allowlist 陈旧：未来 pin 位点行号漂移 | 行号变化 | stale 条目 ⚠️ 警告 + re-pin 流程（rule 3d 同款，:305） |
| F9 | 构造器被绕过：struct 字面量直构使 `idempotency_key ≠ window_id` | 字段 pub | 与 `AuditClaimPayload` 同款风险面（既有模式）；exact-wire 测试只经 `new()` 构造；doc 钉不变量 |
| F10 | AC4/AC5 在共享 dev 库误跑 | `#[ignore]` + DATABASE_URL | AGENTS §4.3 throwaway 库纪律；测试全部 `#[ignore = "requires live Postgres"]`，默认 `cargo test` 门不跑 |

## 5. 迁移步骤（无 DDL；代码顺序 + 验证命令，逐步绿）

> AGENTS §4.2/4.3：本 slice 无迁移文件，但 db_tests 需 throwaway 库（`CREATE DATABASE` → `cargo build` → `aero-cli migrate` → 跑 → `DROP DATABASE`）。**加迁移后必先 build 再 migrate** 的纪律此处仅适用于 throwaway 库初始化。

1. **ids.rs**：`define_id!(AuditWindowId)` → `cargo check -p aero-common`（宏面编译期验证）。
2. **model/audit.rs**：`DeliveryMode` + `DELIVERY_MODE_PUSH` + `AGGREGATED_MESSAGE_ACTION` + `AuditWindowPayload` + `span_is_ordered` + AC1 单元测试 → `cargo test -p aero-common --lib`（不依赖 DB）。
3. **lib.rs**：四符号 + `AuditWindowId` 入 re-export 链 → `cargo check --workspace`（撞名/孤儿检测；`truth-check.sh` 稍后一并验）。
4. **truth-check-lib.sh**：rule 3e + `AUDIT_WINDOW_ALLOWLIST`（空）；**test-claim-contract-guard.sh** 负例 → `bash scripts/test-claim-contract-guard.sh` 全绿（负例恰 1 违例、正例 clean、stale 警告）。
5. **db_tests**：`aero-storage/src/audit_governance.rs` 1 测试 + `aero-audit-connector/src/pg.rs` 2 测试 → `cargo check --workspace`（`#[ignore]` 编译通过即绿）。
6. **throwaway 库**：`CREATE DATABASE` → `cargo build` → `aero-cli migrate`（到 0241）→ `DATABASE_URL=… cargo test -p aero-storage --lib -- --ignored audit_governance --test-threads=1` 与 `DATABASE_URL=… cargo test -p aero-audit-connector --lib -- --ignored --test-threads=1`（**共享表并发 TRUNCATE 竞态**：既有 3 connector 测试并行即红/串行全绿、storage 6/7 vs 7/7——test-integration.sh:170/:193/:623 已编码此约定，新测试同守）→ `DROP DATABASE`。
7. **全门**：`cargo test --workspace --lib`（全绿底线）· `cargo clippy --workspace --all-targets`（不新增警告；`too_many_arguments` 已 allow，先例 audit.rs:293）· `scripts/truth-check.sh`（exit 0、`CLAIM_GUARD_VIOLATIONS == 0`）· `scripts/{file-size-check,web-check}.sh`（0 违规；audit.rs ≤ 800，见 D1）· `scripts/truth-check-lib.sh` 由 truth-check 链入。

## 6. 验收映射（AC → 测试 → 命令）

| 验收（spec §4） | 落点 | 可运行命令 |
|---|---|---|
| AC1 leaf 单元测试族（**7 条目 8 函数，命名即 spec，与 spec §4 AC1 一一对应**）：`audit_window_payload_serializes_exact_wire_text` / `audit_window_payload_roundtrips_through_json_value` / `audit_window_payload_serde_rejects_unknown_field_names` / `audit_window_payload_span_is_ordered` / `window_rows_reuse_outbox_status_machine` / `delivery_mode_roundtrips_lowercase` + `delivery_mode_serde_rejects_unknown_values` / `aggregated_action_token_is_pinned`。**envelope 形幂等**（`idempotency_key == window_id`）在 exact-wire 断言；**header 形幂等**（`AuditWindowId` Display = ULID base32，client.rs:197 拼写）归 **AC2**（ids.rs round-trip）+ **AC5**（PG 跨投递稳定） | audit.rs tests 模块 | `cargo test -p aero-common --lib` |
| AC2 `audit_window_id_roundtrips`（**含 header 形幂等**：`Display`/`FromStr` round-trip ULID base32、serde transparent、`nil()`、ULID `Ord` 单调） | ids.rs tests | `cargo test -p aero-common --lib` |
| AC3 rule 3e 守卫（含负例） | truth-check-lib.sh + test-claim-contract-guard.sh | `bash scripts/test-claim-contract-guard.sh`；`bash scripts/truth-check.sh`（exit 0） |
| AC4 `window_row_passes_0239_checks` + `window_backlog_never_preempts_moderation` | storage/audit_governance.rs + connector/pg.rs（`#[ignore]`） | `DATABASE_URL=<throwaway> cargo test -p aero-storage --lib -- --ignored audit_governance --test-threads=1`；`DATABASE_URL=<throwaway> cargo test -p aero-audit-connector --lib -- --ignored --test-threads=1` |
| AC5 `window_row_idempotency_key_stable_across_requeue_and_reclaim`（T-11 形） | connector/pg.rs（`#[ignore]`） | `DATABASE_URL=<throwaway> cargo test -p aero-audit-connector --lib -- --ignored --test-threads=1`（同 AC4 connector 命令） |

证据附带的五项验收检查全部保留为可运行测试，命名一一对应（AC1 7 条目 8 函数、AC2/AC4/AC5 各一命名测试）；37 计数不增——storage 新测试落 `audit_governance::` 既有前缀槽，connector 两测试由 §5 step 6 手工命令执行（非 G6 harness），b5-pin.sh 零改动。

## 7. 范围外（明确非目标）

- **窗口机制**（builder/批量尺寸/EWMA/计数）：[PROPOSED]，B5-1 storage slice 所有；B5-1 A4 的 `count == N` 断言随其落地。本设计只给该 slice 提供词汇。
- **DDL 变更**：窗口列、PK 形变、delivery_mode CHECK 扩展均属 B5-3 delivery-policy 迁移；AC4 已证今日 CHECK 可表示窗口行。
- **connector claim 键控**：`Claim.event_id: AuditId` 不变；窗口行如何被 claim（窗口键操作、与 1:1 去重）是 connector/storage slice 决策；AC5 只钉任何 keying 必须保留的幂等稳定性。
- **aero-bus seam**：不复活（superseded 分析：G6 经 `claim_due` priority DESC 可满足，无需 NATS 跳）。
- **delivery_mode 消费**：保留 lane 保持 reserved；storage drill 7 处字面量不动。**B5-3 gate item `delivery_mode_drill_pins_leaf_constant`**：drill（audit_governance.rs :609/:618/:630/:642/:677/:704/:712 共 7 处）改引 `DELIVERY_MODE_PUSH`，把现**单向**钉（leaf 自钉 + DDL 自钉，协调改名 `"push"`→`"push-v2"` 双绿可过）升级为 leaf↔DDL **交叉**钉——aero-storage 已依赖 aero-common，refactor 便宜，非「可选后续」；`message.batch` 同类 cross-pin 待 B5-1 SQL 生产者落地后的 parity drill（rule 3e 只扫 `crates/`，migrations/ 拼写未钉）。
- **新 b5-pin 槽 / 37 重基线**：不加。
