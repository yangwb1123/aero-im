# Design — aero-audit-connector B5-4：settle 语义 relay-health heartbeat + 单一入站 audit-scope 拒绝点（`assert_audit_scope_provisioned`）

> Module: `crates/aero-audit-connector`（heartbeat 装饰器 + config）+ `aero-storage`（0246 心跳表 + 仓储）+ `aero-auth`（拒绝点 seam）+ `aero-server`（接线）+ `aero-eng`（check 心跳臂）+ `scripts/test-integration.sh`（leg B3 + leg D 适配）。
> Requirement record: `docs/requirements/2026-08-09-aero-audit-connector-b5-4-settle-heartbeat-rejection-point.req.md`（R1–R8，AC1–AC4）。
> Design pins（继承）：`docs/design/2026-08-07-aero-auth-b5-4-relay-provisioning-gate.design.md`（心跳语义 = 成功 fenced settle，§2.4 装饰器落家，D1/D2/D3）+ `docs/requirements/2026-08-07-aero-auth-b5-4-relay-provisioning-gate.req.md`（R1–R8、AC1）。
> Sibling（勿撞）：`docs/requirements/2026-08-08-aero-ai-b5-4-relay-boot-provisioning-gate.req.md`（boot 门，requirements 态未落地）、`docs/design/2026-08-08-aero-audit-connector-b5-4-fail-closed-operational.design.md`（运行期 gauge 采样面，已落地）。
> Status: **proposed design**（2026-08-09）。全部行号 = 核对时锚点、会漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）。

## §0 Evidence verification（全部逐条复验，2 处对 req spec 的修正 + 2 处补全）

| # | Cited evidence | 复验结果（本工作树） |
|---|---|---|
| E1 | `crates/aero-eng/src/audit_provision.rs`（788 行）；`Q0_SQL` :34；`verdict()` :134；`Healthy` :154；`:148` `"no audit:event:write grant issued"` 精确；src 零 `#[cfg(test)]`；relay health 仅 Q0（开关 + bindings） | ✅ **全部精确命中**。Q0_SQL :34 读 `runtime.enabled` + enabled bindings；`verdict` 4 臂 :134-156：`dead>0`→FailClosed / `!(enabled∧bindings>0)`∧undelivered>0→FailClosed / 同∧0→Consistent / **`enabled∧bindings>0∧dead=0`→Healthy（无条件，无心跳输入——假绿成立）**。src 内 `#[cfg(test)]` 0 命中 ✅。**修正（对 req spec）**：`crates/aero-eng/tests/audit_provision.rs` **已存在 487 行、28 个 `#[test]`** 的 verdict 矩阵单测（`snapshot()` helper :7 + 6 处直接字面量 :70/:88/:115/:196/:238/:290）——文件头注释明示「Kept out of the lib file to stay under the 800-line file-size WARN line」。**AC4 新矩阵测试必须落 `tests/audit_provision.rs`，不能照 req spec §6 建 src 内 `#[cfg(test)]` 模块**（src 788 行 + 测试模块必超 file-size-check.sh `MAX_LINES=800` WARN） |
| E2 | `main.rs:245-269` relay spawn 无 heartbeat writer | ✅ **精确**。:251 `RelayConfig::from_env()` → :254 `PgOutboxRepo::new(persistence.pg.clone())` → :258 `AuditRelay::new` → :259 `tracker.spawn`。settle 成功只进 `pg.rs` fenced UPDATE，无旁路持久状态 |
| E3 | `relay.rs` `deliver_claim` :179、fenced `settle` :184、`Forbidden → mark_dead` :196-219（唯一 fenced-settle 点） | ✅ **精确**。`Ok(()) → repo.settle` :184；403 臂 :199-219（:195-197 注释「Deliberately NOT is_dead_at: HTTP 403 is fail-closed immediate」原样）。`OutboxRepo` trait 五方法（reconcile/claim_due/settle/requeue/mark_dead，`outbox.rs`）；实现者恰 2 个：`PgOutboxRepo`（pg.rs:80）+ `FakeOutbox`（fake.rs:89）——装饰器 = 第 3 实现者，trait 不动 |
| E4 | `extractor.rs` JWT→PAT→bot 链无机器身份；三 seam 全仓零命中 | ✅ **精确**。`AuthUser{participant_id, session_id, exp}` :34-40；链 :116-130（`assert_access_claims_active` → `verify_pat` :127 → `verify_bot_token` :130）。`--glob '!docs/**'` 全仓 `record_heartbeat`/`RelayProvisionGate`/`assert_audit_scope_provisioned`/`audit_relay_provision` = **0 命中** |
| E5 | `Error::Forbidden` → HTTP 403 | ✅ `aero-common/src/error.rs` :19（variant）/ :70（403）/ :85（reason） |
| E6 | 迁移 0240 已被占，心跳迁移顺延 0246 | ✅ `ls migrations` 尾：0239/0240（`audit_governance_due_prio_idx.sql`）/0241/0242/0245——**下一个空号 = 0246** |
| E7 | b5-pin.sh 39/39（18 executed + 21 PROPOSED），`audit-provision-check` 槽在 executed 名单 | ✅ `scripts/b5-pin.sh` :4/:14/:23/:45/:87-91/:133——`assert_b5_contract_pin` 强校验**恰好 39**；`audit-provision-check` :45。本设计**不增槽不删槽** |
| E8 | RelayConfig 字面量 6 处 | ✅ **补全（9 处文本 / 6 个 helper 站点 / 6 文件）**：`bin/aero-audit-t11-drill.rs:127`、`bin/aero-audit-relay-drill.rs:104`、`bin/aero-audit-priority-drill.rs:175`、`relay.rs:297-298`（test_config）、`tests/state_machine.rs:43-44`、`tests/claim_validation.rs:21-22`——加字段后**编译强制同步全部 9 处**（req spec 记 6 处 = 按文件计，文本出现 9 次） |
| E9 | harness leg B :257-310、leg D :398-431（healthy 断言无心跳行） | ✅ **精确**。leg B 实际 :255-313：B1 :272-290（空库 → consistent）、B2 :290-308（1 undelivered → fail-closed）、drop :309-310、`b5_check "audit-provision-check"` :311/:313。leg D :398-431：T-11 DB 上 enable relay + INSERT binding → 断言 `verdict: healthy` + 0239 分布 + oldest-pending-age——**无心跳 seed，新臂落地后必红，须同 commit 适配**。leg C（dead 行 → fail-closed）在 leg D 后，不动 |
| E10 | 已落地 check = psql 变体（aero-eng），harness 经 `cargo run -p aero-cli -- audit-provision-check` 调用 | ✅ `crates/aero-eng/src/commands/audit.rs` :7-23 注册（`DATABASE_URL`/`AERO__DATABASE__URL` → `audit_provision::run` / `--priority` → `run_priority`）；harness 全走此命令。`run()` 内 snapshot 字面量 :641 + `format_report` + `PsqlRunner`（`psql -At -c` 子进程，无参数绑定通道——**新 SQL 只能是固定字面量，无用户输入插值**，Q2/Q3 同款模式） |
| E11 | aero-eng 零 connector 依赖 → freshness 只能 env 直读 | ✅ aero-eng deps = aero-common/tokio/serde_json/anyhow/async-trait/time/serde/toml；**无 sqlx、无 connector**。DB 访问走 `PsqlRunner::query(&str)`。`AERO_AUDIT_PROVISION_FRESHNESS_SECS` env 直读是唯一 seam |
| E12 | 依赖面零新增 | ✅ connector 已有 sqlx/async-trait/tracing/time；aero-auth 已有 aero-storage/sqlx/time；aero-server 已有 aero-audit-connector/aero-storage/aero-auth |
| E13 | `AuthService` builder 模式 + 测试锚点 | ✅ `service.rs` :57 结构体（pat/bot/login_throttle 三 Option 字段）、`new()` :85（全 None = 未注入即禁用）、`with_pat_verifier` :134 / `with_bot_verifier` :152 / `with_login_throttle`（`#[must_use]` builder）——`relay_gate` 逐点复刻。`tests/state_machine.rs` `relay()` helper :65、`forbidden_dead_on_first_attempt` :263-290、`happy_path_settles_and_removes_from_claimable` :292-317 均不动 |
| E14 | **补全（req spec 未列）**：`AuditSnapshot` 字面量站点 = **8 处** | `src/audit_provision.rs:641`（run()）+ `tests/audit_provision.rs` 7 处（`snapshot()` helper :8 + 直接字面量 :70/:88/:115/:196/:238/:290）——加 `heartbeat_age_secs`/`heartbeat_fresh` 字段后编译强制同步 8 处；`snapshot()` helper 是测试面的单一收口，改 helper 签名 + 6 处直接字面量补字段即可 |
| E15 | **补全**：boot 接线文件路径 | `crates/aero-server/src/bin/boot/services.rs`（req spec 写 `boot/services.rs`；实际在 `src/bin/boot/` 下）——`AuthService` 装配在 :55-60 区域（`let mut auth = n(deps.participants.clone(), jwt_codec)` + `with_pat_verifier`/`with_bot_verifier`/`with_login_throttle` 链） |

**结论**：req spec 的全部核心主张（假绿、零心跳、零拒绝点、迁移序号冲突、39/39 钉、6 文件 RelayConfig 同步、零新依赖）**全部成立**。本设计对 req spec 做 **2 处修正**（① AC4 矩阵测试落 `tests/audit_provision.rs` 而非 src `#[cfg(test)]`——800 行 WARN；② 其余为补全清单：RelayConfig 9 处文本、AuditSnapshot 8 处字面量、boot 路径），**不改变任何验收语义**。

## §1 API changes（逐 crate，全部 additive，零新依赖）

### 1.1 `aero-audit-connector`（新 `src/heartbeat.rs` + `config.rs` 加字段）

```rust
// src/heartbeat.rs（lib.rs 增 `pub mod heartbeat;`，`pub mod` 字母序在 fake 后）
#[async_trait]
pub trait HeartbeatRecorder: Send + Sync {
    /// 记录一次「relay works」事实。仅在 fenced settle `Ok(true)` 后调用。
    /// 错误由调用方 log，绝不反哺投递结果（投递已成功；record 失败只让
    /// 门保持 fail-closed 至下个 settle）。
    async fn record_heartbeat(&self) -> Result<(), sqlx::Error>;
}

/// OutboxRepo 装饰器：唯一行为 = settle `Ok(true)` 时记心跳，其余四方法纯委托。
pub struct HeartbeatOutboxRepo<R> {
    inner: Arc<dyn OutboxRepo>,
    recorder: Arc<R>,
}
impl<R: HeartbeatRecorder + 'static> HeartbeatOutboxRepo<R> {
    pub fn new(inner: Arc<dyn OutboxRepo>, recorder: Arc<R>) -> Self;
}
#[async_trait]
impl<R: HeartbeatRecorder + 'static> OutboxRepo for HeartbeatOutboxRepo<R> {
    // settle: Ok(true) → record_heartbeat()（Err 仅 tracing::warn!，settle 返回原值
    //         Ok(true) 不变）；Ok(false)/Err → 原样透传，绝不记心跳
    // reconcile / claim_due / requeue / mark_dead → 纯委托
}
```

```rust
// config.rs — RelayConfig 增字段（struct 尾、jwks_uri 后）
pub provision_freshness: Duration,
// from_env 内 duration_secs 家族（:82-88 之后）：
let provision_freshness = duration_secs("AERO_AUDIT_PROVISION_FRESHNESS_SECS", 300, 60, 86400)?;
// 语义：presence-gated 家族不变量自动成立——无 AERO_AUDIT_TOKEN_ENDPOINT 时设该
// 变量 = 既有 stray 扫描（:49-63）boot Err，无需新代码。
```

编译强制同步：**9 处 `RelayConfig {` 文本 / 6 个 helper 站点**（t11-drill :127 / relay-drill :104 / priority-drill :175 / relay.rs test_config :298 / state_machine.rs :44 / claim_validation.rs :22）各补 `provision_freshness: Duration::from_secs(300)`。

### 1.2 `aero-storage`（迁移 0246 + 新 `src/audit_relay_provision.rs`）

```sql
-- migrations/0246_audit_relay_provisioning.sql（0240 已被 B5-3 占用；幂等；初始无行 = fail-closed）
CREATE TABLE IF NOT EXISTS audit_relay_provisioning (
    singleton       BOOLEAN      PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    verified_at     TIMESTAMPTZ  NOT NULL,
    provision_state TEXT         NOT NULL DEFAULT 'provisioned',  -- 未来 seam，本方向不读写
    updated_at      TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp()
);
```

```rust
// src/audit_relay_provision.rs（lib.rs `pub mod` + `pub use`，字母序 audit_governance 后）
pub enum ProvisionCheck { Verified(OffsetDateTime), NotVerified, Stale(OffsetDateTime) }
pub struct AuditRelayProvisionRepo { /* PgPool */ }
impl AuditRelayProvisionRepo {
    pub fn new(pool: PgPool) -> Self;
    pub async fn record_heartbeat(&self) -> Result<(), sqlx::Error>;
        // UPSERT: verified_at = clock_timestamp()（DB 时钟单时钟域）
    pub async fn verified_at(&self) -> Result<Option<OffsetDateTime>, sqlx::Error>;
    pub async fn provision_check(&self, freshness: time::Duration)
        -> Result<ProvisionCheck, sqlx::Error>;
        // SELECT verified_at, (verified_at + make_interval(secs => $1)) >= clock_timestamp() AS fresh
        //   FROM audit_relay_provisioning WHERE singleton = TRUE;  fetch_optional: None → NotVerified
        // freshness 由调用方传入（仓储不做 env 解析）；DB 错误向上传播，调用方 fail-closed
}
```

### 1.3 `aero-auth`（新 `src/relay_gate.rs` + `service.rs` 增量；**零 extractor 改动**）

```rust
// src/relay_gate.rs（lib.rs `pub mod relay_gate;` + re-export）
#[async_trait]
pub trait RelayProvisionGate: Send + Sync {
    /// true 仅当 durable 心跳存在且新鲜（now - verified_at ≤ freshness）。
    /// 任何异常（行缺失/过期/DB 错误）→ false（fail-closed，绝不 "verified on error"）。
    async fn verified(&self) -> bool;
}
pub type SharedRelayProvisionGate = Arc<dyn RelayProvisionGate>;   // pat.rs/bot.rs 同款惯例
pub struct PgRelayProvisionGate { repo: AuditRelayProvisionRepo, freshness: time::Duration }
impl PgRelayProvisionGate {
    pub fn new(pool: sqlx::PgPool, freshness: time::Duration) -> Self;
    // verified() = matches!(repo.provision_check(freshness).await, Ok(ProvisionCheck::Verified(_)))
}

// service.rs — AuthService 增量（pat/bot/login_throttle 同款）：
pub struct AuthService {
    // ...既有字段
    relay_provision_gate: Option<SharedRelayProvisionGate>,  // new() 置 None = 未注入即 403
}
impl AuthService {
    #[must_use] pub fn with_relay_provision_gate(mut self, gate: SharedRelayProvisionGate) -> Self;
    /// 生命周期 hook：pub no-op seam，不挂 timer（D3：gate 状态 durable + freshness 有界，
    /// 无 per-key map 可驱逐——与 login_throttle 的 sweep 需求不同）。
    pub async fn refresh_relay_provision(&self) {}
    /// 单一入站 audit-scope 拒绝点：
    ///   None → Err(Forbidden("audit:event:write is not provisioned: relay-provisioning gate is not installed (T-11)"))
    ///   verified()==false → Err(Forbidden("audit:event:write is not provisioned: no fresh relay heartbeat (T-11)"))
    ///   否则 Ok(())。HTTP 403（aero-common::error.rs:70）。
    pub async fn assert_audit_scope_provisioned(&self) -> aero_common::Result<()>;
}
// R8 契约文字入 relay_gate.rs 模块头注释：任何未来 audit:event:write-scoped 机器令牌
// 验收点（新入站端点 / AuthUser 扩展 / 内部 API）必须在签发/接受前调用本拒绝点或等价 gate。
```

### 1.4 `aero-server`（接线；`routes/health.rs` / `extractor.rs` 零改动）

```rust
// 新 src/audit_relay_heartbeat.rs（5 行适配，aero-auth design D1 落家）
pub struct PgHeartbeatRecorder { repo: aero_storage::audit_relay_provision::AuditRelayProvisionRepo }
impl aero_audit_connector::heartbeat::HeartbeatRecorder for PgHeartbeatRecorder { /* 纯委托 */ }

// src/bin/main.rs:245-269 relay 装配处（签名不变）：
let pool = persistence.pg.clone();
let inner: Arc<dyn OutboxRepo> = Arc::new(PgOutboxRepo::new(pool.clone()));
let recorder = Arc::new(aero_server::audit_relay_heartbeat::PgHeartbeatRecorder::new(
    aero_storage::audit_relay_provision::AuditRelayProvisionRepo::new(pool)));
let repo: Arc<dyn OutboxRepo> =
    Arc::new(aero_audit_connector::heartbeat::HeartbeatOutboxRepo::new(inner, recorder));
let relay = aero_audit_connector::relay::AuditRelay::new(repo, client, relay_cfg);

// src/bin/boot/services.rs（AuthService 装配链尾追加）：
if let Some(relay_cfg) = aero_audit_connector::config::RelayConfig::from_env()? {
    auth = auth.with_relay_provision_gate(Arc::new(
        aero_auth::relay_gate::PgRelayProvisionGate::new(deps.pg.clone(), relay_cfg.provision_freshness)));
}
// 未配置 → 不注入 → None → 拒绝点一律 403（fail-closed 默认）。from_env 与 main.rs:251
// 同一幂等纯 env 读，错误路径一致。
```

### 1.5 `aero-eng`（`src/audit_provision.rs` 心跳臂；`verdict()` 签名不变）

```rust
// 新查询（Q2 同款固定字面量；PsqlRunner 无参数绑定通道，无用户输入插值）：
const Q6A_SQL: &str = "SELECT to_regclass('audit_relay_provisioning')::text";  // 表存在性探测
const Q6B_SQL: &str = "SELECT extract(epoch FROM (clock_timestamp() - verified_at))::bigint
                       FROM audit_relay_provisioning WHERE singleton = TRUE";  // 空结果 = 无行 = absent

// AuditSnapshot 增两字段（8 处字面量编译强制同步：src run() :641 + tests 7 处）：
pub heartbeat_age_secs: Option<i64>,   // None = 表缺失/无行（absent）
pub heartbeat_fresh: bool,             // run() 内按 age_secs <= freshness 预计算（verdict() 签名不变）

// run() 流程：Q6A 探测 → 命中才 Q6B → 解析 age（空 = absent；负值按 fresh）→ snapshot 字段
// freshness：env AERO_AUDIT_PROVISION_FRESHNESS_SECS（默认 300，界 [60,86400]）；解析失败/越界
//            → Outcome::error（exit 1，garbage 绝不静默取默认）。字面量镜像 RelayConfig（双注释互引）。

// verdict() 五臂（dead 优先顺序不变，第 4 臂新增；arm 4 原无条件 Healthy 被替换为心跳敏感）：
// 1. dead > 0 → FailClosed（原样）
// 2. !(enabled∧bindings>0) ∧ undelivered>0 → FailClosed（原样）
// 3. !(enabled∧bindings>0) ∧ undelivered==0 → Consistent（原样）
// 4. (enabled∧bindings>0) ∧ !heartbeat_fresh → FailClosed（新）
//    reason: "relay enabled (bindings=N) but no fresh relay heartbeat (no fenced settle within
//             {freshness}s; age={age}s|never); no audit:event:write grant issued"
// 5. (enabled∧bindings>0) ∧ heartbeat_fresh → Healthy（原样文案；积压属正常，exit 0）

// format_report 增一行（信息面；verdict 行是断言面）：
// audit-provision-check: heartbeat: absent|fresh (age=Ns)|stale (age=Ns)
```

### 1.6 `scripts/test-integration.sh`（leg B3 + leg D 适配；39/39 钉不变）

leg B 内、`drop_created_database` 前插 **leg B3**（复用 `audit-provision-check` 槽，`b5_check` 仍 PASS）：
1. **B3a（无心跳行）**：DELETE B2 的 undelivered 行 → `UPDATE snaplink_commercial_runtime SET enabled = TRUE` → INSERT binding（leg D 同款）→ 跑 check → **非零** ∧ grep `verdict: fail-closed` ∧ grep `no audit:event:write grant issued`
2. **B3b（过期心跳）**：`INSERT INTO audit_relay_provisioning (verified_at) VALUES (clock_timestamp() - interval '600 seconds');`（2× 默认 freshness 300s）→ 跑 check → **非零** + 同上两个 grep
3. **B3c（正控）**：`UPDATE audit_relay_provisioning SET verified_at = clock_timestamp() WHERE singleton;` → 跑 check → **exit 0** ∧ grep `verdict: healthy`

**leg D 适配（强制伴随，同 commit）**：leg D enable relay + binding 后、healthy 断言前，先 `INSERT INTO audit_relay_provisioning (verified_at) VALUES (clock_timestamp());`（T-11 DB 从未跑过 relay，无 settle 心跳；seed 即「relay works」仿真）。leg C 一字不动。

## §2 Compatibility constraints

1. **迁移序号**：0246 是最终号（0240/0241/0242/0245 已占用）；DDL 与 req/design 钉的 0240 版本语义不变，只换号。
2. **编译强制同步清单**（加字段即编译错，编译器兜底）：`RelayConfig {` **9 处**（6 文件）；`AuditSnapshot {` **8 处**（src 1 + tests 7）；`OutboxRepo` 实现者 2→3（装饰器新实现，trait 不动）；`AuthService::new()` 字段初始化 1 处。
3. **检查/中继滚动升级矩阵**（check = aero-cli，relay = aero-server，独立二进制）：
   | 旧 check + 新 DB | 无心跳臂，行为 = 今天（假绿窗口存在，升级完成即消） |
   |---|---|
   | 新 check + 旧 DB（表缺） | Q6A 探测 → absent → 仅 relay on 时 fail-closed（方向正确）；relay off 时 consistent 语义不变——leg B1 与任何 pre-0246 库不误伤 |
   | 新 check + 新 DB | 五臂全活 |
4. **presence-gated 不变量**：`AERO_AUDIT_PROVISION_FRESHNESS_SECS` 属 `AERO_AUDIT_*` 家族——无 `AERO_AUDIT_TOKEN_ENDPOINT` 时设置 = 既有 stray 扫描 boot Err（fail-loud，非静默忽略）。
5. **no-touch 文件（diff 守卫）**：`migrations/0236_*`–`0245`、`relay.rs`、`outbox.rs`、`fake.rs`、`stub.rs`、`snaplink_commercial/*`、`routes/health.rs`（readyz 永不因 gate 翻转）、`extractor.rs`（无机器身份路径）、`B5_CONTRACT_TEST_LIST` 槽数（39/39 不变）。
6. **投递面永不 consult gate**：0236/0239 触发器、relay 状态机、B5-3 排序均不读心跳表——安静期门关只影响配给验收，不阻塞投递（自愈：事件到达并成功 settle 后刷新）。
7. **时钟域**：心跳写入（`clock_timestamp()`）、新鲜度判定（DB 时钟）、check 年龄（DB 时钟）三处同一 DB 时钟域；无应用时钟插值（`pg.rs` 单时钟域不变量保持）。
8. **freshness 双字面量镜像**（connector config + aero-eng env 直读，默认 300 / 界 [60,86400]）：aero-eng 零 connector 依赖使镜像成为唯一 seam；两处注释互相引用（"mirror of RelayConfig::from_env" / "mirror of aero-eng check"）；leg B3b 用 2×300=600s seed 隐式钉住默认值。

## §3 Failure modes

| 故障 | 行为 | 门状态 |
|---|---|---|
| relay crash / SQL-error-loop | 无 settle → 心跳过期 | fail-closed（freshness 窗口后） |
| 403-loop（IdP 拒配给） | settle 永不 Ok(true)；403 → mark_dead ≤1 | 永不刷新 → fail-closed（T-11 因果链保持：gate 不健康 ⇒ IdP 拒 ⇒ 403 ⇒ dead 回归） |
| settle `Ok(false)`（租约丢失/竞态） | 未 fenced，**不记心跳** | 维持现状 |
| record_heartbeat 失败（DB 抖动） | `tracing::warn!` + settle 返回原值 `Ok(true)`（不反哺投递结果——失败 settle 会导致重投） | 保持 fail-closed 至下个成功 settle |
| 安静期（无事件 → 无 settle） | 心跳自然过期 | 仅配给验收 fail-closed；投递不阻塞；事件到达自愈 |
| 表缺失（pre-0246 库） | Q6A 探测 → absent（非错误） | 仅 relay on 时 fail-closed（方向正确） |
| freshness env 非法（check 侧） | `Outcome::error` exit 1（garbage 绝不静默取默认） | fail-closed（显式红） |
| 时钟怪异（age 负值） | age ≤ freshness → 按 fresh 处理 | 不误伤（单时钟域内罕见） |
| 多实例 relay | singleton UPSERT last-writer-wins | 良性；任一无 fencing 的实例 settle 即证明「有 relay 在投递」 |
| 旧 check 二进制 + 新 DB | 无心跳臂，行为 = 今天 | 假绿窗口存在至升级完成（滚动升级固有窗口，文档化） |

## §4 Migration steps（顺序即依赖序）

1. **迁移**：写 `migrations/0246_audit_relay_provisioning.sql` → ⚠️ **先 `cargo build` 再 `aero-cli migrate`**（AGENTS.md §4.2 编译期嵌入，否则静默 no-op）。
2. **仓储**：`aero-storage/src/audit_relay_provision.rs`（`AuditRelayProvisionRepo` + `ProvisionCheck`）+ lib.rs `pub mod`/`pub use`（audit_governance 后）+ PG `#[ignore]` db_tests（UPSERT 幂等 / 三态 / freshness 边界：age==freshness → Verified、age==freshness+1 → Stale）。
3. **connector**：`heartbeat.rs`（trait + 装饰器 + AC2.1 单测）+ `RelayConfig.provision_freshness`（from_env + struct 字段）+ **9 处字面量同步**。
4. **aero-auth**：`relay_gate.rs`（trait + `SharedRelayProvisionGate` + `PgRelayProvisionGate` + `AuthService` 字段/builder/no-op hook + `assert_audit_scope_provisioned`）+ lib.rs re-export + AC3 三态单测。
5. **aero-server**：`audit_relay_heartbeat.rs`（`PgHeartbeatRecorder`）+ main.rs 包装（`HeartbeatOutboxRepo`）+ `boot/services.rs` gate 注入；确认 health.rs/extractor.rs 零改动。
6. **aero-eng**：Q6A/Q6B + `AuditSnapshot` 两字段（8 处字面量同步）+ verdict 第 4 臂 + report 行 + freshness env 解析；**AC4 矩阵测试落 `tests/audit_provision.rs`**（既有 28 测试文件扩展，非 src `#[cfg(test)]`——800 行 WARN）。
7. **harness（同 commit）**：leg B3（B3a/B3b/B3c）+ leg D seed 适配；验 `bash scripts/test-b5-pin-guard.sh` → `bash scripts/test-integration.sh` → 期望 `B5-CHECK audit-provision-check: PASS` + `B5 contract pin: 39/39 ... PASS`。
8. **门禁**：`cargo check --workspace` · `cargo test --workspace --lib`（PG 门控 `-- --ignored` 需 `DATABASE_URL`+已迁移）· `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· no-touch diff 守卫。

## §5 Testable acceptance mapping（AC1–AC4 → 具体测试）

| Acceptance | 测试面 | 断言 | 位置 |
|---|---|---|---|
| **AC1**（T-11 mapping：enabled + 零 dead + relay 实际已死 → fail-closed） | harness leg B3 三子步 + leg D 适配 | B3a/B3b：非零 exit ∧ grep `verdict: fail-closed` ∧ grep `no audit:event:write grant issued`；B3c：exit 0 ∧ grep `verdict: healthy`；leg D：seed 后 healthy 断言原样保持；leg B1/B2 grep 与退出码一字不变；39/39 钉不变 | `scripts/test-integration.sh` |
| **AC2.1**（仅 fenced settle 盖心跳） | heartbeat.rs 单测（假 inner repo 可编程 settle + 计数 recorder） | `Ok(true)` → +1 且返回值原样；`Ok(false)`/`Err` → 0；requeue/mark_dead/claim_due/reconcile → 0；recorder 自身 Err → settle 仍 `Ok(true)` | `crates/aero-audit-connector/src/heartbeat.rs` tests |
| **AC2.2**（403-loop 闭环） | state_machine.rs 新测试（`HeartbeatOutboxRepo` + `FakeOutbox` + `StubSink events_status:403`） | 403 批 → 全 Dead ∧ 计数 0；回 202 + 新行 → Delivered ∧ 计数 1；既有 `forbidden_dead_on_first_attempt` 零改动保持绿 | `crates/aero-audit-connector/tests/state_machine.rs` |
| **AC3**（单一拒绝点三态） | relay_gate.rs 单测（无 DB，fake gate）+ 仓储 PG `#[ignore]` db_tests | (a) 未注入 → `Err(Forbidden)` ∧ 消息含 `"audit:event:write is not provisioned"` ∧ `status_code()==403`；(b) 恒 false gate → 同；(c) 恒 true → `Ok(())`；extractor.rs 零改动（git diff 守卫）；仓储三态 + UPSERT 幂等 + freshness 边界 | `crates/aero-auth/src/relay_gate.rs` tests + `crates/aero-storage/src/audit_relay_provision.rs` db_tests |
| **AC4**（dead 优先回归） | verdict 矩阵单测 + harness leg C | `dead>0` × {relay on/off} × {心跳 fresh/stale/absent} → 全 FailClosed（第 4 臂排在 dead 臂后）；2/3/5 臂逐臂回归；leg C 一字不改（`status=3` → 非零 ∧ grep `dead=1` ∧ `delivered=0` ∧ `verdict: fail-closed`） | `crates/aero-eng/tests/audit_provision.rs`（既有文件扩展，**非 src `#[cfg(test)]`**）+ `scripts/test-integration.sh` |
| 附加回归 | readyz 不翻转 | `routes/health.rs` 既有 `readiness_decision` 单测保持绿（零改动） | 既有 |

## §6 Risks / decisions

- **AC4 测试落点修正（本设计裁决）**：req spec §6 写「`crates/aero-eng/src/audit_provision.rs` tests（新 `#[cfg(test)]` 模块）」——src 已 788 行，加测试模块必超 file-size-check.sh `MAX_LINES=800` WARN（AGENTS.md：800 WARN / 1200 HARD，唯一尺寸来源）。既有 `tests/audit_provision.rs`（487 行、28 测试）文件头注释已明示「Kept out of the lib file to stay under the 800-line file-size WARN line」——**新矩阵测试扩展该文件**（+~150 行仍 < 800），`snapshot()` helper 收口新字段默认值。验收语义不变。
- **`verdict()` 签名不变**（fresh 预计算进 `AuditSnapshot`）：`run()`/`run_priority()` 两调用点零改动，回归面最小。
- **freshness 双字面量镜像**：connector config 与 aero-eng env 直读各存一份默认 300 / 界 [60,86400]——aero-eng 零 connector 依赖约束下的唯一 seam；双注释互引 + leg B3b 600s seed 隐式钉默认值。
- **leg D 依赖新臂（强制伴随）**：leg D 现断言 healthy 于无心跳 DB，新臂落地必红——seed 步骤与本切片同 commit（防半绿 CI）。
- **安静期门关**（settle 语义代价）：只影响配给验收，从不阻塞投递；默认 300s ≫ poll 5s。chicken-and-egg（首次投递先于任何心跳）是设计内顺序——投递面不 consult gate。
- **`refresh_relay_provision` 不接线**：pub no-op seam，不挂 boot/serve.rs timer（D3 保持；gate 状态 durable + freshness 有界，无 per-key map 需驱逐——与 login_throttle 的 sweep 需求不同类）。
- **aero-cli `audit-provision-check` 命令与 `AERO_AUDIT_DRILL_PROVISION=1` drill 模式不实现**：sibling-direction 面（req spec §3 已界定）。
- **旧 check + 新 DB 滚动窗口**：假绿窗口存在至两二进制均升级；harness 单机全绿不覆盖（文档化，非缺陷）。

## §7 Sequencing

1. 迁移 0246 + 仓储 + db_tests（**先 `cargo build` 再 migrate**）
2. connector：heartbeat.rs + config 字段 + 9 处字面量同步
3. aero-auth：relay_gate.rs + AuthService 增量 + 三态单测
4. aero-server：PgHeartbeatRecorder + main.rs 包装 + boot gate 注入
5. aero-eng：Q6A/Q6B + snapshot 字段（8 处同步）+ verdict 第 4 臂 + 矩阵测试（tests/audit_provision.rs）
6. harness：leg B3 + leg D seed（同 commit）→ `scripts/test-b5-pin-guard.sh` → `scripts/test-integration.sh`
7. 门禁全量：check / test / clippy / truth-check / file-size-check / web-check / no-touch diff 守卫
