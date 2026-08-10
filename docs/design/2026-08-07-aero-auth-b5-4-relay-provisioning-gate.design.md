# Design — B5-4 fail-closed relay-provisioning gate：builder-hook seam + `aero-cli audit-provision-check` 落家

- **Direction**: "Add a fail-closed relay-provisioning gate on AuthService (B5-4) using the existing builder-hook pattern"（value 7 / risk_reduction 9 / effort 5 / confidence 7）
- **Requirements**: `docs/requirements/2026-08-07-aero-auth-b5-4-relay-provisioning-gate.req.md`（R1–R8, AC1–AC3）
- **Status**: Design（证据全部逐条复核；两处实现偏差见 §7——装饰器落家 connector crate、drill 增 provision 模式）
- **Verification date**: 2026-08-07。行号为复核锚点，会漂移——**文件/符号**为准（AGENTS.md §0）

## 0. Evidence adjudication（untrusted claims → verified anchors）

| Evidence claim | Verdict | Verified anchor（实际） |
|---|---|---|
| `service.rs:102-127` builder-hook 模式（`with_login_throttle` :102 / `sweep_login_throttle` :116 / `with_pat_verifier` :134 / `with_bot_verifier`） | ✅ 精确命中（行号微漂） | `crates/aero-auth/src/service.rs`：`login_throttle: Option<Arc<LoginThrottle>>` :78；`new()` 置 `None` :93；`#[must_use] with_login_throttle` :102-113；`sweep_login_throttle(now) -> usize` :116-123（None→no-op 0）；`with_pat_verifier` :134；`with_bot_verifier` :152（req 记 :146，漂移 6 行）。接线 `crates/aero-server/src/bin/boot/services.rs:71`（env 门控）；sweep 驱动 `boot/serve.rs:203` |
| `extractor.rs` AuthUser 链 JWT→PAT→bot，无机器身份 | ✅ 精确命中（行号微漂） | `crates/aero-auth/src/extractor.rs`：`AuthUser{participant_id, session_id, exp}` :35-40（req 记 :31-37）；链 :119-132：JWT `svc.verify`+`assert_access_claims_active` :119-121 → `verify_pat` :127 → `verify_bot_token` :130-132（req 记 :97-119）。无机器 kind、无 scope 字段 |
| `migrations/0236` AFTER INSERT 触发器无条件入队 | ✅ 精确命中 | `migrations/0236_snaplink_governance_reconciliation.sql`：`aero_enqueue_snaplink_audit()` :68-130，唯一门 = `snaplink_commercial_runtime.enabled` :78-86（关则 `RETURN NEW` :82）；`AFTER INSERT ON audit_events FOR EACH ROW` :129-133。无 relay 健康参与 |
| `snaplink_commercial/runtime.rs` relay 运行期 in-server + readyz 不翻转 | ✅ 精确命中（行号微漂） | `crates/aero-server/src/snaplink_commercial/runtime.rs`：`from_env` :42（req 记 :26-63）、`ready()` :87、`run_delivery_relay` :174、`DELIVERY_BACKLOG_METRIC` :17。`crates/aero-server/src/routes/health.rs`：`deps_ok` 含 `matches!(commercial, "ok" \| "disabled")` :182（不翻转确认）|
| B5-4 `audit-provision-check` [PROPOSED] 无家；落家 = DB 能力 aero-cli | ✅ 精确命中 | `docs/proposals/audit-contract-batch-aero-im.md:11`；全仓 grep 仅 docs 命中。`crates/aero-server/src/bin/aero-cli.rs` = 702 行、15 命令、completion 串 :685；`aero-server/Cargo.toml:24-25` `[[bin]] name="aero-cli"`；`crates/aero-cli/Cargo.toml:12-13` `[[bin]] name="aero-eng"` |
| B5-2 connector 生产接线 + 403→dead（T-11）已实现 | ✅ 精确命中 | `main.rs:245-269`（`RelayConfig::from_env()` presence-gated :251 → `PgOutboxRepo` :253-256 → `AuditRelay` :259 → `tracker.spawn` :260）；`relay.rs::deliver_claim` Forbidden→`mark_dead` :177-190；`client.rs::SCOPE_AUDIT="audit:event:write"` :32、403 分类 :166；connector `Cargo.toml` 无 aero-storage（确认）|
| `Error::Forbidden` = 403 | ✅ 精确命中 | `crates/aero-common/src/error.rs`：variant :19、HTTP 403 :70、code "forbidden" :85 |
| 门禁/契约锚点（implementation-gate :64/:78；B5-1 §3；B5-2 §8.3） | ✅ 精确命中 | `docs/campaigns/implementation-gate.md` aero-im 行（T-11）:64、G6 :78；两份 req 文档存在且 §锚点自洽 |
| 0239 未落库（migrations 尾号 0238）、A3 drill 以 0239 文件存在为门 | ✅ 精确命中 | `ls migrations` 尾号 = `0238_message_recall.sql`；`scripts/test-integration.sh` :232-263（0239 文件门 + drill 段 :249-263）；`run_migrated_integration` :153-165（empty-filter 守卫 :187-193）|
| `OutboxRepo::settle` 返回 `Ok(true)` 仅当 fenced 成功（R3 前提） | ✅ 补充确认（关键） | `crates/aero-audit-connector/src/pg.rs` settle：fenced `UPDATE … WHERE claim_token=$2 AND status IN (0,1) AND lease_expires_at > clock_timestamp()`，`rows_affected()==1` 才 commit `Ok(true)`，否则 rollback `Ok(false)`。**「成功 settle」= 真实投递确认 + fence 成立** |
| T-11 回归测试存在（AC1.3 锚点） | ✅ 补充确认 | `crates/aero-audit-connector/tests/state_machine.rs` `forbidden_dead_on_first_attempt` :216-238（403→Dead 立即、不再被 claim）；claim_validation.rs 10 用例、relay.rs 内部 5 用例 |
| RelayConfig 全部构造点（新增字段的编译强制面） | ✅ 补充确认 | 4 处 struct literal：`bin/aero-audit-relay-drill.rs:73`、`relay.rs:235`（test_config）、`tests/state_machine.rs:34`、`tests/claim_validation.rs:16` |
| 依赖面（无新增依赖可行性） | ✅ 补充确认 | aero-auth `Cargo.toml`：aero-storage :14、async-trait :18、time :26、anyhow :24；aero-server `Cargo.toml`：aero-audit-connector :36；connector 已有 sqlx（pg.rs 使用）|
| **发现（req 未覆盖，本设计补）**：A3 drill 在 0239 缺席时 **exit 2（SKIP）** | ✅ 确认 | `bin/aero-audit-relay-drill.rs` :59-68 探测 `to_regclass('audit_governance_outbox')`，缺席 `std::process::exit(2)`——与 req §8「AC1/AC2 不依赖 0239 必须全绿」矛盾，本设计 §7 D2 解决 |
| **发现（req 未覆盖，本设计补）**：drill 直接构造 `AuditRelay`（connector bin 无 aero-storage 依赖） | ✅ 确认 | drill :119-122 `AuditRelay::new(repo, client, config)`——req R3 把装饰器放 aero-server 则 AC2.1 的「relay settle 驱动心跳」在 drill 路径不可测，本设计 §7 D1 解决 |

**勘误/细化汇总**：① 行号均微漂（service.rs bot_verifier :152、extractor 链 :119-132、runtime from_env :42）；② **关键新事实**：drill exit-2 语义 + drill 无 aero-storage 依赖 → 装饰器落家与 AC2 测试路径必须重排（§7）；③ `provision_state` 列为未来 seam，本设计 gate 只读 `verified_at`；④ 现有 `forbidden_dead_on_first_attempt` 即 AC1.3 回归锚点，无需新写。

## 1. Design overview

```
                    ┌─────────────────────────── 生产接线（aero-server）───────────────────────────┐
                    │  boot/services.rs::build          main.rs:245-269                            │
                    │  RelayConfig::from_env()?          PgOutboxRepo ─┐                            │
                    │   Some → with_relay_provision_      HeartbeatOutboxRepo<R>  (connector)      │
                    │          gate(PgRelayProvisionGate)     └─ recorder: PgHeartbeatRecorder     │
                    │                          │                    └─ AuditRelayProvisionRepo      │
                    │                          ▼                    （成功 fenced settle → UPSERT）   │
                    │  AuthService.relay_provision_gate      ┌─────────────────────────────┐        │
                    │    (Option<Arc<dyn RelayProvisionGate>>)│ audit_relay_provisioning    │        │
                    └─────────────────────────────────────────│ singleton PK · verified_at   │◄───────┘
                                                              │ · provision_state(seam)     │
                                                              └─────────────────────────────┘
                    aero-cli audit-provision-check（16 命令）── provision_check(freshness) ──┘
                    drill（AERO_AUDIT_DRILL_PROVISION=1）── raw-SQL recorder ──┘（AC2 测试面）
```

**门的三层语义**（与 req §1.1 一致）：① `assert_audit_scope_provisioned`（403 拒绝点，未来机器令牌路径必过闸，R8）；② `aero-cli audit-provision-check` 退出码（运维验收）；③ connector 403→dead（已实现，回归保持）。**心跳 = 成功 fenced settle**（非轮询存活）：403-loop 的 relay 不产 settle → 门保持关闭。

## 2. API changes（逐 crate 签名）

### 2.1 `crates/aero-storage/src/audit_relay_provision.rs`（新模块，lib.rs `pub mod`）

```rust
pub enum ProvisionCheck {
    Verified(OffsetDateTime),   // 心跳存在且新鲜（now - verified_at <= freshness）
    NotVerified,                // 行缺失（含从未 settle）
    Stale(OffsetDateTime),      // 行存在但过期
}

pub struct AuditRelayProvisionRepo { pool: PgPool }
impl AuditRelayProvisionRepo {
    pub fn new(pool: PgPool) -> Self;
    /// UPSERT singleton；verified_at = clock_timestamp()（DB 时钟，单时钟域，
    /// 与 connector pg.rs 的 clock-domain 纠正同源）。不写 provision_state（未来 seam）。
    pub async fn record_heartbeat(&self) -> Result<(), sqlx::Error>;
    pub async fn verified_at(&self) -> Result<Option<OffsetDateTime>, sqlx::Error>;
    /// freshness 由调用方传入（仓储不做 env 解析）；DB 错误向上传播，调用方 fail-closed。
    pub async fn provision_check(&self, freshness: time::Duration)
        -> Result<ProvisionCheck, sqlx::Error>;
}
```

SQL（`provision_check` 全程 DB 时钟，避免应用/DB 时钟偏斜）：

```sql
SELECT verified_at,
       (verified_at + make_interval(secs => $1)) >= clock_timestamp() AS fresh
FROM audit_relay_provisioning
WHERE singleton = TRUE;   -- fetch_optional：None → NotVerified
```

### 2.2 `migrations/0240_audit_relay_provisioning.sql`（幂等，独立于 0239）

```sql
CREATE TABLE IF NOT EXISTS audit_relay_provisioning (
    singleton        BOOLEAN      PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    verified_at      TIMESTAMPTZ  NOT NULL,
    provision_state  TEXT         NOT NULL DEFAULT 'provisioned',  -- 未来 seam，本方向不读写
    updated_at       TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp()
);
```

**初始态 = 无行 = fail-closed**（无 backfill，正确默认）。迁移序号 0240（0239 留给 B5-1）。

### 2.3 `crates/aero-auth/src/relay_gate.rs`（新模块，lib.rs `pub mod` + re-export）

```rust
#[async_trait]
pub trait RelayProvisionGate: Send + Sync {
    /// true 仅当 durable 心跳存在且新鲜。任何异常（行缺失/过期/DB 错误）→ false（fail-closed）。
    async fn verified(&self) -> bool;
}
pub type SharedRelayProvisionGate = Arc<dyn RelayProvisionGate>;

pub struct PgRelayProvisionGate { repo: aero_storage::AuditRelayProvisionRepo, freshness: time::Duration }
impl PgRelayProvisionGate {
    pub fn new(pool: sqlx::PgPool, freshness: time::Duration) -> Self;
}
// RelayProvisionGate for PgRelayProvisionGate:
//   verified() = matches!(repo.provision_check(freshness).await, Ok(ProvisionCheck::Verified(_)))
//   —— Err 一律 false（DB 错误绝不当作已验证）

// AuthService 增量（逐点复制 E1 模式）：
//   relay_provision_gate: Option<SharedRelayProvisionGate>   // new() 置 None（未注入 = fail-closed 拒绝）
//   #[must_use] pub fn with_relay_provision_gate(mut self, gate: SharedRelayProvisionGate) -> Self
//   pub async fn refresh_relay_provision(&self)              // 生命周期 hook；当前 no-op seam（§7 D3）
//   pub async fn assert_audit_scope_provisioned(&self) -> aero_common::Result<()> {
//       None            → Err(Error::Forbidden("audit:event:write is not provisioned: \
//                          relay-provisioning gate is not installed (T-11)"))
//       verified()==false → Err(Error::Forbidden("audit:event:write is not provisioned: \
//                          no fresh relay heartbeat (T-11)"))
//       else            → Ok(())
//   }   // 403 规范状态码（aero-common::error.rs:70）
```

**零 extractor 改动**（E2：无机器身份路径存在，不新增入站端点——R8 契约只钉未来路径）。无新增依赖（aero-storage :14 / async-trait :18 / time :26 已在）。

### 2.4 `crates/aero-audit-connector/`（新增 `heartbeat.rs`，additive；config 增字段）

```rust
// src/heartbeat.rs —— 零新依赖（sqlx 已有）
#[async_trait]
pub trait HeartbeatRecorder: Send + Sync {
    /// 记录「relay works」（一次 fenced settle 刚成功）。错误由调用方 log，
    /// 绝不反哺投递结果（投递已成功，失败 settle 会重投）。
    async fn record_heartbeat(&self) -> Result<(), sqlx::Error>;
}

pub struct HeartbeatOutboxRepo<R> { inner: Arc<dyn OutboxRepo>, recorder: Arc<R> }
impl<R: HeartbeatRecorder> HeartbeatOutboxRepo<R> {
    pub fn new(inner: Arc<dyn OutboxRepo>, recorder: Arc<R>) -> Self;
}
// impl<R: HeartbeatRecorder + 'static> OutboxRepo for HeartbeatOutboxRepo<R>：
//   claim_due / requeue / mark_dead → 纯委托（失败/死行绝不刷新心跳）
//   settle(event_id, claim_token) → match inner.settle().await {
//       Ok(true) => {           // fenced 成功 = 「relay works」
//           if let Err(e) = recorder.record_heartbeat().await {
//               tracing::warn!(%event_id, ?e, "heartbeat record failed; gate stays fail-closed until next settle");
//           }
//           Ok(true)
//       }
//       other => other,         // Ok(false)=租约丢失 / Err：不记心跳
//   }
```

```rust
// src/config.rs：RelayConfig 增字段（4 处 struct literal 编译强制同步：drill :73、
// relay.rs test_config :235、tests/state_machine.rs :34、tests/claim_validation.rs :16）
pub provision_freshness: Duration,
// from_env 内（既有 duration_secs 家族，presence-gated 不变量保持）：
let provision_freshness = duration_secs("AERO_AUDIT_PROVISION_FRESHNESS_SECS", 300, 60, 86400)?;
```

### 2.5 `crates/aero-server/`（接线 + CLI）

**boot/services.rs::build**（镜像 :71 登录节流接线，`:59-71` 之后）：

```rust
if let Some(relay_cfg) = aero_audit_connector::config::RelayConfig::from_env()? {
    tracing::info!(
        freshness_secs = relay_cfg.provision_freshness.as_secs(),
        "audit relay provisioning gate enabled"
    );
    auth = auth.with_relay_provision_gate(Arc::new(
        aero_auth::relay_gate::PgRelayProvisionGate::new(
            deps.pg.clone(), relay_cfg.provision_freshness,
        ),
    ));
}
// 未配置 → 不注入 → None → assert 一律 403（fail-closed，R1）
// `?` 传播半配置 Err——与 main.rs:251 同一 from_env 幂等纯 env 读，错误路径一致
```

**main.rs:245-269**（包装，签名不变）：

```rust
let pool = /* 既有 connect_pg 结果（提取为绑定） */;
let inner: Arc<dyn OutboxRepo> = Arc::new(PgOutboxRepo::new(pool.clone()));
let recorder = Arc::new(PgHeartbeatRecorder::new(
    aero_storage::audit_relay_provision::AuditRelayProvisionRepo::new(pool),
));
let repo: Arc<dyn OutboxRepo> = Arc::new(HeartbeatOutboxRepo::new(inner, recorder));
let relay = AuditRelay::new(repo, client, relay_cfg);   // 后续原样
```

**aero-cli.rs 第 16 命令**（`c!` 宏，薄壳——判定逻辑全在仓储层）：

```rust
c!(
    AuditProvisionCheck,
    "audit-provision-check",
    "Check audit relay provisioning (exit 0 = provisioned, 1 = not)",
    |_ctx, _args| {
        let relay_cfg = match aero_audit_connector::config::RelayConfig::from_env() {
            Ok(Some(cfg)) => cfg,
            Ok(None) => return Outcome::error(
                "audit relay is not configured (AERO_AUDIT_TOKEN_ENDPOINT unset)"),
            Err(e) => return Outcome::error(format!("audit relay config invalid: {e}")),
        };
        let Some(c) = load_cfg().await else { return Outcome::error("config required"); };
        let p = match aero_storage::connect_pg(&c.database.url, 1).await {
            Ok(p) => p, Err(e) => return Outcome::error(format!("db: {e}")),
        };
        let repo = aero_storage::audit_relay_provision::AuditRelayProvisionRepo::new(p);
        match repo.provision_check(relay_cfg.provision_freshness).await {
            Ok(ProvisionCheck::Verified(ts)) => {
                println!("audit relay provisioned (verified_at={ts}, freshness={}s)",
                         relay_cfg.provision_freshness.as_secs());
                Outcome::ok("provisioned")
            }
            Ok(ProvisionCheck::NotVerified) =>
                Outcome::error("audit relay has no verified heartbeat (T-11)"),
            Ok(ProvisionCheck::Stale(ts)) =>
                Outcome::error(format!("audit relay heartbeat is stale (verified_at={ts})")),
            Err(e) => Outcome::error(format!("db: {e}")),
        }
    }
);
// completion cmds 串（:685）同步加 "audit-provision-check"（15 → 16 命令）
```

退出码契约 = R4 五分支（非零 = 未配给，fail-closed；DB 错误绝不误报 verified）。

## 3. Compatibility constraints

| 面 | 约束 |
|---|---|
| **零改动文件** | `extractor.rs`、`migrations/0236_*`、`snaplink_commercial/*`、`routes/health.rs`（`readiness_decision`/`probe_commercial`/`deps_ok` 全不动——readyz 永不因 gate 翻转，R6）、`relay.rs` 状态机、`outbox.rs` trait、`fake.rs`、`stub.rs` |
| **connector crate** | 仅 additive（`heartbeat.rs`）+ `RelayConfig` 一字段；**无新依赖、无 aero-storage 依赖**（recorder 是 trait）；既有 22 项测试与 A3 drill 默认行为原样绿 |
| **RelayConfig 字段** | 4 处 struct literal 编译强制同步（drill/relay 测试/state_machine/claim_validation）；`from_env` 新 var 缺省 300，presence-gated 家族不变量保持（无 token endpoint 时设 `AERO_AUDIT_PROVISION_FRESHNESS_SECS` = boot Err，文档化，无行为回退） |
| **迁移** | 0240 幂等、additive、无 backfill；初始无行 = fail-closed（正确默认）；**加迁移必先 `cargo build` 再 migrate**（AGENTS §4.2 编译期嵌入） |
| **aero-cli** | 15→16 命令 + completion 串；文件 702 → ~750 行（< 800 WARN 阈值，命令保持薄壳） |
| **lint 面** | truth-check：`with_relay_provision_gate` 有 boot 调用点（非 unwired）；新模块必须 `pub mod` 声明（防孤儿）；`refresh_relay_provision` 非 `with_*` builder 不触发。authz_lint：新代码无 RoomId/WorkspaceId 不涉。clippy：无新警告 |
| **消费面边界（R7）** | gate 永不 consult：入队（0236/0239 触发器）、投递（relay 状态机）、排序（B5-3）；moderation 不阻塞 |
| **多实例** | 多 server/多 relay 各自 settle 各自记心跳 → singleton UPSERT 幂等，last-writer-wins，良性 |
| **时钟域** | 全部 DB 时钟（`clock_timestamp()`）：心跳写入、freshness 判定、fence 校验同一时钟域（对齐 connector pg.rs 先例）；无应用时钟参与 |

## 4. Failure modes（含处置）

| # | 故障 | 行为 | 处置/保证 |
|---|---|---|---|
| F1 | gate 未注入（未配置 connector） | `assert_audit_scope_provisioned` → 403 "not installed" | fail-closed 默认；CLI exit 1 "not configured" |
| F2 | 心跳行缺失（从未成功 settle） | `NotVerified` → 403 / CLI exit 1 | 初始态即此；首次成功 settle 自愈 |
| F3 | 心跳过期（安静期 > freshness） | `Stale` → 403 / CLI exit 1 | **只影响配给验收，从不阻塞投递**（R7）；下个成功 settle 自愈；freshness 默认 300s ≫ poll 5s，抖动窗口可控 |
| F4 | relay 403-loop（配给被拒） | 无 settle → 心跳不刷新 → freshness 后门关 | 正是 T-11 fail-closed 语义；B5-2 403→dead 状态机不变（回归保持） |
| F5 | `verified()` 读 DB 出错 | Err → `false` → 403 | fail-closed：错误绝不当作已验证；CLI 侧同错 → exit 1 |
| F6 | `record_heartbeat` 写失败（装饰器内） | log warn，**不反哺 settle 结果**（投递已成功，失败 settle 会导致重投） | 门保持 fail-closed 至下个成功 settle；心跳 at-least-once |
| F7 | settle `Ok(false)`（租约丢失/竞态） | 不记心跳（未 fenced） | 正确：另一会话可能正在投递；行会重投，fenced 的那次记心跳 |
| F8 | 半配置 env（无 token endpoint 但有其他 `AERO_AUDIT_*`） | boot Err（既有家族不变量）；CLI exit 1 | 新 var 加入同一家族，行为无回退 |
| F9 | CLI：PG 不可达 / 0240 未迁移（表缺失） | provision_check Err → exit 1 | 错误不误报 verified；消息提示 db 错误 |
| F10 | 0239 缺席时 relay 生产运行 | claim 报错（既有 F13 降级），无 settle → 心跳不建立 → 门关 | 正确初始态；AC2 drill 用 `AERO_AUDIT_DRILL_PROVISION=1` 自建表绕过（§7 D2） |
| F11 | 并发多实例同时记心跳 | UPSERT 单行，last-writer-wins | 良性；gate 判定只看 verified_at 新鲜度 |
| F12 | freshness 误配（0 / 巨大值） | 配置校验界 [60, 86400]（duration_secs 家族）拒绝 | 防「永远 stale」或「门永不关」 |

## 5. Migration steps & sequencing

1. **迁移**：写 `migrations/0240_audit_relay_provisioning.sql` → **`cargo build`（必须，编译期嵌入）** → throwaway 库 `aero-cli migrate` 验证。
2. **仓储**：`aero-storage/src/audit_relay_provision.rs` + `lib.rs` `pub mod`；db_tests（PG `#[ignore]`：UPSERT 幂等/重复刷新、provision_check 三态、freshness 边界 `now-1s` vs `now+1s`、行缺失）。
3. **seam**：`aero-auth/src/relay_gate.rs` + `lib.rs` re-export；AC1.1 三态单测（None/false/true gate）。
4. **connector**：`config.rs` 字段 + 4 处 literal；`heartbeat.rs`（trait + 装饰器）+ 单测（fake recorder：settle Ok(true)→recorded；Ok(false)/Err→不 recorded；requeue/mark_dead→不 recorded；record 失败不改变 settle 返回值）。
5. **aero-server 接线**：`boot/services.rs` gate 注入 + `main.rs` 包装（PgHeartbeatRecorder）；`health.rs` 零改动。
6. **CLI**：`audit-provision-check` 命令 + completion 串；`test-integration.sh` 命名条目（§6）。
7. **drill 扩展**：`AERO_AUDIT_DRILL_PROVISION=1` 模式（§7 D2）——`ensure_outbox_table` 提 `pub`（doc: drill seam）、装饰器 + raw-SQL recorder、心跳新鲜断言。
8. **交叉切片**（0239 落库后）：AC3.1/AC3.2 门解开（B5-1 A2 parity + B5-3 claim 排序在 gate 不健康态下跑）。
9. **门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check}.sh` · `scripts/test-integration.sh`（AC1/AC2 全绿）· AC3.4 no-touch diff 守卫。

## 6. Testable acceptance mapping

| AC | 断言 | 落点（具体） |
|---|---|---|
| **AC1.1** 拒绝点三态 | 未注入 → `Err(Forbidden)` 含 "not provisioned"；恒 false gate → 同；恒 true gate → `Ok(())` | `crates/aero-auth/src/relay_gate.rs` tests（fake gate，无 DB） |
| **AC1.2** CLI 退出码五子例 | ① 无 `AERO_AUDIT_*` → exit≠0 含 "not configured"；② 全 env 无心跳行 → exit≠0 含 "no verified"；③ `UPDATE verified_at = clock_timestamp() - 2*freshness` → exit≠0 含 "stale"；④ 心跳新鲜 → exit 0 含 "verified_at"；⑤ 坏 DB URL → exit≠0 | `scripts/test-integration.sh` 新命名条目（throwaway 已迁移库 + `cargo run --bin aero-cli -- audit-provision-check`；env 矩阵：`AERO_AUDIT_TOKEN_ENDPOINT/EVENTS_URL/RESOURCE/CLIENT_ID/CLIENT_SECRET/EXPECTED_ISS/EXPECTED_AUD/EXPECTED_SUB/SOURCE_SYSTEM` 全 https 哑值 + `AERO_AUDIT_PROVISION_FRESHNESS_SECS=60`） |
| **AC1.3** T-11 回归保持 | `forbidden_dead_on_first_attempt` 等全绿 | `cargo test -p aero-audit-connector`（既有 22 项，零改动）+ A3 drill 默认模式（exit-2 SKIP 行为不变） |
| **AC2.1** 心跳建立 | `AERO_AUDIT_DRILL_PROVISION=1 AERO_AUDIT_DRILL_ROWS=3` → N 行全 settle → `audit_relay_provisioning.verified_at` 存在且新鲜 → 随即 `audit-provision-check` exit 0 | drill（§7 D2 模式）+ test-integration.sh 同一条目顺序执行（AC2.3 防假绿） |
| **AC2.2** matrix 403-free | `COUNT(status=2)==N`；event_id set-parity；stub 无 403（无 `Forbidden` 分类、无行 `last_error` 含 403） | drill 既有断言 + 扩展（心跳断言同条目） |
| **AC2.3** 先投递后检查 | 同上条目的顺序（先 drill 后 CLI） | test-integration.sh 条目结构保证 |
| **AC3.1** 入队不阻塞 | gate 表存在但无心跳行时，B5-1 A2 parity db_test 原样绿（恰 1 audit + 1 outbox、event_id 1:1、rollback 归零） | 复用 B5-1 A2（0239 门控；断言本质 = 入队路径不读 gate 表） |
| **AC3.2** 优先不阻塞 | 500 积压 + 1 moderation 行在 gate 不健康下仍先 claim | B5-3 claim 排序 db_test 扩展（0239 + priority 门控，SKIP 模式沿用 :250-270 先例） |
| **AC3.3** 投递不阻塞 + 自愈 | 心跳预置过期（`UPDATE verified_at = now - 2×freshness`）后跑 drill → 行仍 settle（status=2）且心跳刷新为新鲜 | drill provision 模式变体（同 AC2 条目：先 UPDATE 过期再跑） |
| **AC3.4** no-touch 守卫 | 变更集不含 `migrations/0236_*`、`connector/src/relay.rs`、`snaplink_commercial/runtime.rs`、`routes/health.rs::readiness_decision` | review gate + `git diff --stat` |
| **R6** readyz 不翻转 | 既有 `readiness_decision` 单测绿 + 新单测：gate 缺席/不健康不影响判定 | `routes/health.rs` tests（仅新增测试，不动判定代码） |
| **R2 仓储** | UPSERT 幂等、三态、freshness 边界 | `aero-storage/src/audit_relay_provision.rs` db_tests（PG `#[ignore]`，`run_migrated_integration` 模式 :153） |

## 7. Deviations from requirements spec & decisions

- **D1（实现落家偏差）**：R3 把 `HeartbeatOutboxRepo` 放 aero-server——但 AC2.1/AC2.2 的 drill 是 connector crate 的 bin（无 aero-storage 依赖），无法使用 aero-server 的装饰器，settle→心跳不可观测。**改为**：trait `HeartbeatRecorder` + 泛型 `HeartbeatOutboxRepo<R>` 落 `connector/src/heartbeat.rs`（additive、零新依赖）；aero-server 只实现 `PgHeartbeatRecorder`（5 行适配）；drill 用 raw-SQL recorder。R3 语义原样保留（仅 fenced settle `Ok(true)` 记心跳、其余纯委托、relay.rs 零改动）。
- **D2（AC2 的 0239 独立性）**：req §8 声称「AC1/AC2 不依赖 0239 必须全绿」，但 A3 drill 在 0239 缺席时 exit-2（:59-68）。**改为**：drill 增 `AERO_AUDIT_DRILL_PROVISION=1` 模式——`pg.rs::ensure_outbox_table` 提 `pub`（doc: drill seam）并在该模式下调用（不再 exit-2），包装饰器 + raw-SQL recorder + 心跳新鲜断言；**默认模式逐位不变**（exit-2 SKIP，B5-2 契约保持）。
- **D3（lifecycle hook 不接线）**：`refresh_relay_provision` 保持 pub no-op seam，**不**在 `boot/serve.rs:203` 挂 timer（该行是 login_throttle 专用；gate 状态 durable + freshness 有界，无 per-key map 可驱逐——req §7 已记录此偏差）。将来若做 TTL 缓存化再驱动它。
- **D4（R6 可选健康体字段不实现）**：`/health` 体增 `audit_relay_provisioning` 字段标为 follow-up，本切片**零 health.rs 改动**（AC3.4 守卫更干净）；运维观测由 CLI + drill 承担。
- **决策点保持**：心跳语义 = settle（非 poll-alive，req §7）；chicken-and-egg（relay 自证）设计内顺序；v1 snaplink relay 不参与本门；「matrix 端到端」仓内钉死读法 = drill 扩展。

## 8. Out of scope（勿在本 direction 建造）

入站 audit-scope 机器令牌验收端点 / `AuthUser` 机器 kind（R8 只钉契约）；0239 治理 outbox DDL + class/priority + B5-3 排序；`admin.content.flag` 映射 token 清单（仓外契约）；IdP scope registry 本体（仓外）；v1 `snaplink_commercial/` 任何改动。
