# Requirements Spec — aero-audit-connector B5-4 完成切片：settle 语义 relay-health heartbeat + 单一入站 audit-scope 拒绝点（`assert_audit_scope_provisioned`）

- **Module (analysis root)**: `crates/aero-audit-connector` — heartbeat 装饰器落家（`src/heartbeat.rs`）；落地动作跨 aero-audit-connector（装饰器 + config）/ aero-storage（0246 心跳表 + 仓储）/ aero-auth（拒绝点 seam）/ aero-server（接线）/ aero-eng（psql check 心跳臂）/ scripts/test-integration.sh（leg B3 + leg D 适配）
- **Direction**: "B5-4: implement the design-pinned settle-based relay-health heartbeat and the single inbound audit-scope rejection point (assert_audit_scope_provisioned)"（value 8 / risk_reduction 8 / effort 6 / confidence 8）
- **Source analysis**: `docs/auto/analyses/crates-aero-audit-connector-40d338d1.json`（direction #2）
- **Campaign**: `aero-im-b5-outbox-relay`；contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（B5-4 行 :11）；gate anchor `docs/campaigns/implementation-gate.md`（:64 "T-11（无 relay 配给被拒）"、:78 G6）
- **Design pins**: `docs/design/2026-08-07-aero-auth-b5-4-relay-provisioning-gate.design.md`（心跳语义 + §2.4 装饰器落家 + D1/D2/D3）；`docs/requirements/2026-08-07-aero-auth-b5-4-relay-provisioning-gate.req.md`（R1-R8、AC1-AC3）；实现态 baseline = `docs/design/2026-08-07-aero-cli-b5-4-audit-provision-check-psql.design.md`（psql-backed check 是已落地变体）
- **Status**: Requirements（下述证据全部经源码 grep 核对，2026-08-09）
- **Verification date**: 2026-08-09。行号是核对时锚点，会漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## §1 Evidence verification（direction 引用逐条核对，含 6 处漂移/修正）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-eng/src/audit_provision.rs`（788 行）——Q0_SQL 读 `snaplink_commercial_runtime.enabled` + enabled bindings（:44-47）；`verdict()` 返回 Healthy whenever relay_enabled ∧ dead=0（:134-152） | ✅ **Verified（行号漂移）**。文件 788 行精确；`Q0_SQL` 实际在 **:34**（引用 :44-47 漂移 10 行）；`pub fn verdict` 实际 :134、Healthy 分支 :154、`:148` `"no audit:event:write grant issued"` **精确命中**。dead-priority 矩阵注释 :136-141、`Verdict` enum :121-127（引用 :117-134 大致覆盖）。**Q0 谓词只读 DB 开关 + bindings，无任何心跳/新鲜度输入——「crash/SQL-error-looping relay + 空 outbox + enabled 开关 → 'verdict: healthy'」的假绿成立**。补充：`audit_provision.rs` **零单测**（`#[cfg(test)]` 全文件 0 命中）——新 verdict 臂必须自带矩阵单测 |
| E2 | `crates/aero-server/src/bin/main.rs:251-264` — relay spawn 无 heartbeat writer | ✅ **Verified（行号漂移）**。Audit connector relay 段实际 :245-269：`RelayConfig::from_env()` :251 → `PgOutboxRepo::new(persistence.pg.clone())` :254 → `AuditRelay::new` :258 → `tracker.spawn(relay.spawn(...))` :259。**零心跳写入**——settle 成功只进 `pg.rs` 的 fenced UPDATE，无旁路状态 |
| E3 | `crates/aero-audit-connector/src/relay.rs` — `deliver_claim` settle/403→mark_dead 是唯一能盖心跳的 fenced settle 点 | ✅ **Verified**。`deliver_claim` :179；`Ok(()) → repo.settle` :184；`DeliveryError::Forbidden → mark_dead` :196-219（"Deliberately NOT is_dead_at: HTTP 403 is fail-closed immediate" 注释 :195-197 原样在）。trait 全貌 `outbox.rs`：reconcile/claim_due/settle/requeue/mark_dead 五方法——装饰器须纯委托其余四者 |
| E4 | `crates/aero-auth/src/extractor.rs` — AuthUser 链 JWT→PAT→bot，无 machine identity——gate 无入站消费者 | ✅ **Verified**。`AuthUser{participant_id, session_id, exp}` :34-40；链 :116-130（`svc.verify`+`assert_access_claims_active` → `verify_pat` :127 → `verify_bot_token` :130）；无机器身份 kind、无 scope 字段。**全仓 grep `assert_audit_scope_provisioned`/`RelayProvisionGate`/`record_heartbeat`/`audit_relay_provision`（docs 外）= 0 命中——heartbeat + 拒绝点均未实现** |
| E5 | `scripts/test-integration.sh:252-305` — leg B verdict greps；slot pinned at **37/37** | ✅ **Verified（两处漂移）**。leg B 实际 :257-310（B1 :272、B2 :290、drop :309-310）；`b5_check "audit-provision-check"` 槽 :311/:313。**钉死槽数已从 37/37 漂移到 39/39**：`scripts/b5-pin.sh` `B5_CONTRACT_TEST_LIST` = 18 executed + 21 [PROPOSED]，`assert_b5_contract_pin` 强校验**恰好 39 槽**（"audit-provision-check" 在 executed 名单内）。leg D（0239 门内、healthy 断言）实际 :398-431——**在 T-11 DB 上 enable relay + 1 binding 后断言 `verdict: healthy`，无心跳行**：新心跳臂落地后此腿必红，须同批次 seed 心跳行（§5 AC1 附注） |
| E6 | `docs/design/2026-08-07-aero-cli-b5-4-audit-provision-check.design.md` — settle-as-heartbeat pin、403-loop door-open rationale、E7/E9 evidence | ✅ **Verified（归属修正）**。settle-as-heartbeat 的**权威钉**在 sibling 文档：`2026-08-07-aero-auth-b5-4-relay-provisioning-gate.req.md` §1.1（"心跳语义钉死为「成功 settle」而非「轮询存活」…403-loop 的 relay 也会 Ok…与 T-11 fail-closed 相悖"）+ §7；`2026-08-07-aero-auth-b5-4-relay-provisioning-gate.design.md` §1（"心跳 = 成功 fenced settle（非轮询存活）：403-loop 的 relay 不产 settle → 门保持关闭"）+ §2.4（装饰器落家 connector `heartbeat.rs`）。aero-cli design 仅交叉引用（:70 "202 receipt / settle = sibling spec aero-auth B5-4 心跳的领域"）。E7/E9 证据 = req 文档 E7（connector 无心跳）/E9（门禁锚点）——均在 |
| E7 | `docs/requirements/2026-08-07-aero-auth-b5-4-relay-provisioning-gate.req.md` — R8 `assert_audit_scope_provisioned` → 403；AC1 | ✅ **Verified**。R1（拒绝点三态 + 403 消息文案）、R2（0240 迁移 + `AuditRelayProvisionRepo`）、R3（心跳记录）、R8（未来机器令牌路径必过闸契约）全在；AC1.1（三态单测）为设计钉的测试落点 |
| E8 | **修正 1：迁移序号 0240 已被占用** | `ls migrations` 尾号：0239/0240/0241/0242/**0245**——`0240_audit_relay_provisioning.sql`（req/design 钉的序号）被 `0240_audit_governance_due_prio_idx.sql`（B5-3）占用。**心跳迁移必须顺延为 `0246_audit_relay_provisioning.sql`**（下一个空号） |
| E9 | **修正 2：RelayConfig struct literal 站点 4 → 6** | 全仓 `RelayConfig {` 字面量 = `bin/aero-audit-t11-drill.rs:127`、`bin/aero-audit-relay-drill.rs:104`、`bin/aero-audit-priority-drill.rs:175`、`relay.rs:297`（test_config）、`tests/state_machine.rs:43`、`tests/claim_validation.rs:21`——加 `provision_freshness` 字段后**编译强制同步 6 处**（design 记 4，漂移） |
| E10 | **修正 3：已落地变体 = psql-backed** | `crates/aero-eng/src/commands/audit.rs` 注册 `AuditProvisionCheck_`（`DATABASE_URL`/`AERO__DATABASE__URL` → `audit_provision::run`；`--priority` 臂）；实现跟随 `2026-08-07-aero-cli-b5-4-audit-provision-check-psql.design.md`（非 env-preflight 脚本变体）。本切片的新心跳臂落在 psql 变体上 |
| E11 | **修正 4：aero-eng 无 connector 依赖** | `crates/aero-eng/Cargo.toml` deps = aero-common/tokio/serde_json/anyhow/async-trait/time/serde/toml——**check 的 freshness 只能读 env**（`AERO_AUDIT_PROVISION_FRESHNESS_SECS`，默认 300 镜像 `RelayConfig`），零新依赖约束保持 |
| E12 | 依赖面（零新增依赖可行性） | connector `Cargo.toml` 已有 **sqlx**（heartbeat.rs trait 的 `sqlx::Error` 可用）；aero-auth `Cargo.toml` 已有 aero-storage :14 / sqlx :49 / time :26；aero-server `Cargo.toml` 已有 aero-audit-connector :36 / aero-storage :32 / aero-auth :33；`Error::Forbidden` → HTTP **403**（`aero-common/src/error.rs` :19/:70/:85）——全部零新增依赖 |
| E13 | 既有测试锚点（回归面） | `tests/state_machine.rs`：`relay()` helper :65、`forbidden_dead_on_first_attempt` :263-290（403→Dead attempt 1）、`happy_path_settles_and_removes_from_claimable` :292-317——**均不动**；`fake.rs` `FakeOutbox`（settle fenced :247-262、mark_dead :295+）+ `stub.rs` `StubSink`（`events_status: 403`）可直供新测试 |

**勘误/细化汇总**：① 行号微漂（Q0_SQL :34、leg B :257-310、main.rs relay 段 :245-269）；② **37/37 → 39/39**（b5-pin.sh 现行钉，本切片不增槽）；③ **迁移 0240 → 0246**（0240-0242/0245 已占用）；④ RelayConfig 字面量 6 处；⑤ 已落地 check 是 psql 变体（aero-eng）；⑥ aero-eng 零 connector 依赖 → freshness env 直读；⑦ `audit_provision.rs` 现无任何单测——新臂矩阵测试是新工作。

## §2 Verified current state（三条独立事实 + 一个缺口）

```
a) 投递面（E3/E2）  main.rs:245-269：RelayConfig::from_env presence-gated → PgOutboxRepo → AuditRelay
                   └─ deliver_claim：202 → fenced settle；403 → mark_dead ≤1（T-11 已实现）
                   └─ ✗ 无心跳写入：settle 成功不留任何持久「relay works」事实
b) 检查面（E1）    aero-eng audit-provision-check（psql 变体，788 行）
                   └─ Q0 谓词 = runtime.enabled ∧ enabled bindings（DB 开关，非 ready()）
                   └─ verdict 4 臂：dead>0 → fail-closed；off ∧ undelivered>0 → fail-closed；
                     off ∧ 0 → consistent；on ∧ dead=0 → healthy
                   └─ ✗ 假绿：on ∧ dead=0 ∧ relay 实际已死（crash/SQL-error-loop/403-loop）
                     且 outbox 空 → healthy（门开着，T-11 相悖）
c) 验收面（E4）    AuthUser extractor：JWT→PAT→bot（无机器身份）；无 audit-scope 机器令牌验收路径
                   └─ ✗ assert_audit_scope_provisioned 不存在（全仓 0 命中）
d) 持久面（E8）    ✗ audit_relay_provisioning 表/仓储不存在（0246 未写；0240 已被 B5-3 占用）
```

**本 direction 关闭的缺口**（design 已钉，全部 verified）：① relay 无持久心跳（a) → R3/R5 新建 0246 表 + 装饰器，**成功 fenced settle 即「relay works」**）；② 检查面假绿（b) → R6 心跳臂：relay on ∧ 无新鲜心跳 → fail-closed "no audit:event:write grant issued"）；③ 无入站拒绝点（c) → R4 `assert_audit_scope_provisioned` 403，R8 契约）；④ 403-loop 永不刷新心跳（settle 语义天然保证，R1/R8 钉死）。

## §3 Scope

**In scope（direction acceptance 四项 + 其实现前提）**：
- `crates/aero-audit-connector/src/heartbeat.rs`（新，additive）：`HeartbeatRecorder` trait + 泛型 `HeartbeatOutboxRepo<R>` 装饰器（**仅 fenced settle `Ok(true)` 记心跳**，其余纯委托）。
- `RelayConfig.provision_freshness`（config.rs，`AERO_AUDIT_PROVISION_FRESHNESS_SECS` 默认 300 / 界 [60,86400]，duration_secs 家族）+ **6 处** struct literal 同步（编译强制）。
- 迁移 `0246_audit_relay_provisioning.sql`（**0240 已占用，顺延 0246**；幂等 singleton 心跳行）+ `aero-storage/src/audit_relay_provision.rs`（`AuditRelayProvisionRepo`：record_heartbeat / verified_at / provision_check 三态）+ lib.rs `pub mod` + db_tests。
- `crates/aero-auth/src/relay_gate.rs`（新）：`RelayProvisionGate` trait + `SharedRelayProvisionGate` + `PgRelayProvisionGate` + `AuthService.relay_provision_gate` 字段/builder/`refresh_relay_provision` no-op hook + **`assert_audit_scope_provisioned` 403 拒绝点**；lib.rs re-export。**零 extractor 改动**。
- aero-server 接线：`PgHeartbeatRecorder`（新小模块）+ main.rs:245-269 包装（`HeartbeatOutboxRepo::new(PgOutboxRepo, PgHeartbeatRecorder)`）+ `boot/services.rs` gate 注入（`RelayConfig::from_env()?.is_some()` 时 `.with_relay_provision_gate(...)`）。
- `crates/aero-eng/src/audit_provision.rs` 心跳臂：Q6a/Q6b 查询 + `AuditSnapshot.heartbeat` + verdict 第 4 臂 + report 行 + **矩阵单测**（文件现零单测）；freshness 读 env（aero-eng 零 connector 依赖约束）。
- `scripts/test-integration.sh`：leg B 第三子腿 B3（enabled + 零 dead + 无/过期心跳 → fail-closed）+ **leg D 适配**（healthy 断言前 seed 心跳行——否则新臂使 leg D 必红）。**复用 `audit-provision-check` 槽，39/39 钉不变**。
- 门禁：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh` · `scripts/test-integration.sh`。

**Out of scope（并行切片/其他 direction——勿在本 direction 建造）**：
- aero-server bin `aero-cli audit-provision-check` 命令（sibling req R4 的落家面）——本 direction 的 harness 面是**已落地**的 aero-eng psql check；R4 CLI 属 sibling direction 交付，本切片不实现。
- drill `AERO_AUDIT_DRILL_PROVISION=1` 模式（aero-auth design §7 D2，sibling AC2.1 的 settle→心跳端到端测试面）——本 direction 的心跳测试面 = heartbeat.rs 单测 + state_machine.rs 403-loop 测试（AC2）；drill 模式留给 sibling。
- 入站 audit-scope 机器令牌 HTTP 验收端点 / `AuthUser` 机器 kind（R8 只钉契约，不建路径）。
- 0239/0240/0241/0242/0245 迁移、`snaplink_commercial/*`、0236 触发器、`routes/health.rs`（readyz 零接触，R6 sibling 约束保持）、`relay.rs`/`outbox.rs`/`fake.rs`/`stub.rs` 状态机。
- `B5_CONTRACT_TEST_LIST` 槽数变更（39/39 保持；新 db_tests 走既有 `--ignored` 全量 sweep，不新增命名槽）。

## §4 Requirements

### R1 — connector 心跳 seam（`crates/aero-audit-connector/src/heartbeat.rs`，additive，零新依赖）
逐点实现 aero-auth design §2.4：
- `#[async_trait] pub trait HeartbeatRecorder: Send + Sync { async fn record_heartbeat(&self) -> Result<(), sqlx::Error>; }`——「记录 relay works（一次 fenced settle 刚成功）」。错误由调用方 log，**绝不反哺投递结果**（投递已成功；record 失败只会让门保持 fail-closed 至下个 settle）。
- `pub struct HeartbeatOutboxRepo<R> { inner: Arc<dyn OutboxRepo>, recorder: Arc<R> }`；`pub fn new(inner: Arc<dyn OutboxRepo>, recorder: Arc<R>) -> Self`。
- `impl<R: HeartbeatRecorder + 'static> OutboxRepo for HeartbeatOutboxRepo<R>`：
  - `settle(event_id, token)` → `match inner.settle().await { Ok(true) => { record_heartbeat()，Err → tracing::warn!(%event_id, ?e, "heartbeat record failed; gate stays fail-closed until next settle")，**settle 返回原值 Ok(true) 不变**；other => other }`——**`Ok(false)`（租约丢失/竞态）与 `Err` 绝不记心跳**（未 fenced 不是「relay works」）；
  - `reconcile` / `claim_due` / `requeue` / `mark_dead` → **纯委托**（claim/requeue/403→dead 绝不刷新心跳）。
- 零新依赖：sqlx（:19 已有）、async-trait、tracing 全在 connector Cargo.toml。

### R2 — `RelayConfig.provision_freshness`（config.rs）
- 结构体增 `pub provision_freshness: Duration`；`from_env` 内既有 `duration_secs` 家族（:82-88 之后）加 `let provision_freshness = duration_secs("AERO_AUDIT_PROVISION_FRESHNESS_SECS", 300, 60, 86400)?;`——presence-gated 家族不变量保持（无 token endpoint 时设该变量 = boot Err，既有 stray 扫描行为，文档化即可）。
- **6 处 struct literal 编译强制同步**（§1 E9）：t11-drill :127 / relay-drill :104 / priority-drill :175 / relay.rs test_config :297 / state_machine.rs :43 / claim_validation.rs :21——各补 `provision_freshness: Duration::from_secs(300)`。

### R3 — 持久心跳状态（迁移 0246 + 仓储）
- `migrations/0246_audit_relay_provisioning.sql`（**0240 已被 `0240_audit_governance_due_prio_idx.sql` 占用，序号顺延**；幂等 `CREATE TABLE IF NOT EXISTS`）：
  ```sql
  CREATE TABLE IF NOT EXISTS audit_relay_provisioning (
      singleton        BOOLEAN      PRIMARY KEY DEFAULT TRUE CHECK (singleton),
      verified_at      TIMESTAMPTZ  NOT NULL,
      provision_state  TEXT         NOT NULL DEFAULT 'provisioned',  -- 未来 seam，本方向不读写
      updated_at       TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp()
  );
  ```
  **初始态 = 无行 = fail-closed**（无 backfill）。⚠️ 加迁移后**必先 `cargo build` 再 `aero-cli migrate`**（AGENTS.md §4.2 编译期嵌入）。
- `crates/aero-storage/src/audit_relay_provision.rs`（AGENTS.md「每功能一个 XRepo」）+ lib.rs `pub mod`（`audit_governance` 之后字母序）：
  - `record_heartbeat(&self) -> Result<(), sqlx::Error>`——singleton UPSERT：`verified_at = clock_timestamp()`（DB 时钟单时钟域）；
  - `verified_at(&self) -> Result<Option<OffsetDateTime>, sqlx::Error>`；
  - `provision_check(&self, freshness: time::Duration) -> Result<ProvisionCheck, sqlx::Error>`——`ProvisionCheck { Verified(OffsetDateTime), NotVerified, Stale(OffsetDateTime) }`；SQL：`SELECT verified_at, (verified_at + make_interval(secs => $1)) >= clock_timestamp() AS fresh FROM audit_relay_provisioning WHERE singleton = TRUE`（fetch_optional：None → NotVerified）。freshness 由调用方传入，仓储不做 env 解析；DB 错误向上传播，调用方 fail-closed。
- db_tests（PG `#[ignore]`）：UPSERT 幂等/重复刷新、provision_check 三态、freshness 边界（age = freshness 恰 fresh / freshness+1 stale）。

### R4 — aero-auth 拒绝点（`crates/aero-auth/src/relay_gate.rs`，逐点复制既有 builder-hook 模式）
- `#[async_trait] pub trait RelayProvisionGate: Send + Sync { async fn verified(&self) -> bool; }`——`true` 仅当 durable 心跳存在且新鲜（`now - verified_at ≤ freshness`）；**任何异常（行缺失/过期/DB 错误）→ `false`**（fail-closed，绝不 "verified on error"）。
- `pub type SharedRelayProvisionGate = Arc<dyn RelayProvisionGate>`（pat.rs/bot.rs 同款 `Shared*` 惯例）。
- `pub struct PgRelayProvisionGate { repo: aero_storage::audit_relay_provision::AuditRelayProvisionRepo, freshness: time::Duration }`；`pub fn new(pool: sqlx::PgPool, freshness: time::Duration) -> Self`；`verified()` = `matches!(repo.provision_check(freshness).await, Ok(ProvisionCheck::Verified(_)))`。
- `AuthService` 增量（service.rs :78/:102 同款）：`relay_provision_gate: Option<SharedRelayProvisionGate>`（`new()` 置 `None`——**未注入 = fail-closed 拒绝**）；`#[must_use] pub fn with_relay_provision_gate(mut self, gate: SharedRelayProvisionGate) -> Self`；生命周期 hook `pub async fn refresh_relay_provision(&self)`（**pub no-op seam，不挂 timer**——aero-auth design D3：gate 状态 durable + freshness 有界，无 per-key map 可驱逐）。
- 拒绝点：`pub async fn assert_audit_scope_provisioned(&self) -> aero_common::Result<()>`：
  - `None` → `Err(Error::Forbidden("audit:event:write is not provisioned: relay-provisioning gate is not installed (T-11)"))`；
  - `verified()==false` → `Err(Error::Forbidden("audit:event:write is not provisioned: no fresh relay heartbeat (T-11)"))`；
  - 否则 `Ok(())`。**HTTP 403 规范状态码**（aero-common::error.rs:70）。
- lib.rs `pub mod relay_gate;` + re-export；**零 extractor 改动**；零新依赖（aero-storage :14 / sqlx :49 / time :26 已有）。
- **R8 契约声明**（继承 sibling req）：任何未来 audit:event:write-scoped 机器令牌验收点（新入站端点 / AuthUser 扩展 / 内部 API）**必须**在签发/接受前调用 `assert_audit_scope_provisioned()`（403）或等价 gate 判定——本 direction 不实现该路径，只交付单一测试过的拒绝点。

### R5 — aero-server 接线（main.rs 包装 + boot gate 注入）
- 新小模块（如 `crates/aero-server/src/audit_relay_heartbeat.rs`）：`pub struct PgHeartbeatRecorder { repo: aero_storage::audit_relay_provision::AuditRelayProvisionRepo }`，`impl HeartbeatRecorder for PgHeartbeatRecorder`（`record_heartbeat` 纯委托；5 行适配，aero-auth design D1 落家）。
- `main.rs:245-269` relay 装配处包装（签名不变，`Arc<dyn OutboxRepo>` 已是 trait object）：
  ```rust
  let pool = persistence.pg.clone();                                   // 提取为绑定
  let inner: Arc<dyn aero_audit_connector::outbox::OutboxRepo> =
      Arc::new(aero_audit_connector::pg::PgOutboxRepo::new(pool.clone()));
  let recorder = Arc::new(aero_server::audit_relay_heartbeat::PgHeartbeatRecorder::new(
      aero_storage::audit_relay_provision::AuditRelayProvisionRepo::new(pool),
  ));
  let repo: Arc<dyn aero_audit_connector::outbox::OutboxRepo> =
      Arc::new(aero_audit_connector::heartbeat::HeartbeatOutboxRepo::new(inner, recorder));
  let relay = aero_audit_connector::relay::AuditRelay::new(repo, client, relay_cfg);
  ```
- `boot/services.rs::build`（:71 登录节流接线同款，其后追加）：`if let Some(relay_cfg) = aero_audit_connector::config::RelayConfig::from_env()? { auth = auth.with_relay_provision_gate(Arc::new(aero_auth::relay_gate::PgRelayProvisionGate::new(deps.pg.clone(), relay_cfg.provision_freshness))); }`——未配置 → 不注入 → None → 拒绝点一律 403（fail-closed 默认）。`from_env` 与 main.rs:251 同一幂等纯 env 读，错误路径一致。
- **零改动文件**：`routes/health.rs`（readiness_decision/probe_commercial/deps_ok 一行不动——readyz 永不因 gate 翻转，sibling R6 约束）、`extractor.rs`、`snaplink_commercial/*`、`relay.rs`/`outbox.rs`/`fake.rs`/`stub.rs`。

### R6 — aero-eng check 心跳臂（`crates/aero-eng/src/audit_provision.rs`）
- **Q6a**（候选探测，Q2 同款容忍漂移）：`SELECT to_regclass('audit_relay_provisioning')::text`——表缺失 → heartbeat = absent（**不是错误**；见下）。
- **Q6b**（仅 Q6a 命中时）：`SELECT extract(epoch FROM (clock_timestamp() - verified_at))::bigint FROM audit_relay_provisioning WHERE singleton = TRUE`——空结果 = 无行 = absent；年龄负值（时钟怪异）按 fresh 处理（age ≤ freshness 即 fresh）。**全程 DB 时钟**（Q4 oldest-pending 同款，无 SQL 插值）。
- `AuditSnapshot` 增 `pub heartbeat_age_secs: Option<i64>` + `pub heartbeat_fresh: bool`（fresh 在 `run()` 内按 `age_secs <= freshness` 计算——`verdict()` 签名不变，回归安全）。
- **freshness 来源**：env `AERO_AUDIT_PROVISION_FRESHNESS_SECS`（默认 300，界 [60,86400]，镜像 R2 的 RelayConfig 语义）；解析失败/越界 → `Outcome::error`（exit 1，**garbage 绝不静默取默认**）。aero-eng 零 connector 依赖（E11）——env 直读是唯一 seam；两处默认值 300/界 [60,86400] 为**字面量镜像**（§7 风险注记）。
- **verdict 五臂矩阵**（dead 优先顺序不变，**第 4 臂新增**）：
  1. `g0239.dead > 0` → FailClosed（原样——dead 是终态，relay 开关/心跳均不豁免）；
  2. `!(relay_enabled ∧ enabled_bindings>0)` ∧ undelivered>0 → FailClosed（原样："relay disabled (bindings=N) with K undelivered audit row(s); no audit:event:write grant issued"）；
  3. `!(relay_enabled ∧ enabled_bindings>0)` ∧ undelivered==0 → Consistent（原样）；
  4. **`relay_enabled ∧ enabled_bindings>0` ∧ `!heartbeat_fresh` → FailClosed**（新）——reason 含 `"relay enabled (bindings=N) but no fresh relay heartbeat (no fenced settle within {freshness}s; age={age}s|never); no audit:event:write grant issued"`（**leg grep 面 = `verdict: fail-closed` + `no audit:event:write grant issued`**）；表缺失/无行按 absent → 同臂（fail-closed 方向：无法证明 relay works）；
  5. `relay_enabled ∧ enabled_bindings>0` ∧ heartbeat_fresh → Healthy（原样文案；pending 积压属正常，exit 0）。
- report 增一行：`audit-provision-check: heartbeat: absent|fresh (age=Ns)|stale (age=Ns)`（leg 信息面；verdict 行是断言面）。
- **单测（新——文件现零 `#[cfg(test)]`）**：上述五臂全矩阵 + dead 优先回归 + 边界（age==freshness → fresh；age==freshness+1 → stale）+ 表缺失=absent 语义 + parse（Q6b 空/畸形行）。
- 表缺失时 Q6b **不跑**（Q6a 探测先行）——leg B1（fresh DB 已迁移含 0246，表在）与任何 pre-0246 旧库（表缺 → 仅 relay on 时 fail-closed；relay off 时 consistent 语义不变）都不破坏既有 verdict。

### R7 — harness（`scripts/test-integration.sh`，leg B 第三子腿 + leg D 适配）
**leg B3**（插在 leg B2 之后、`drop_created_database` 之前，复用 `audit-provision-check` 槽；39/39 钉不变）：
1. **B3a（无心跳行）**：`DELETE FROM snaplink_delivery_outbox WHERE delivery_id = 'audit:b5-leg-b';`（清 B2 的 undelivered 行，隔离新臂）→ `UPDATE snaplink_commercial_runtime SET enabled = TRUE, updated_at = clock_timestamp() WHERE singleton;` → INSERT binding（leg D 同款：`gen_random_uuid(), 'tenant-b5d', 'client-b5d', 'audit-client-b5d', 'aero-im.source', 1, TRUE`）→ 跑 check → **非零** 且 grep `verdict: fail-closed` 且 grep `no audit:event:write grant issued`（当前实现此处返回 Healthy——本腿钉死假绿修复）。
2. **B3b（过期心跳）**：`INSERT INTO audit_relay_provisioning (verified_at) VALUES (clock_timestamp() - interval '600 seconds');`（2× 默认 freshness 300s）→ 跑 check → **非零** + 同上两个 grep（"relay 实际已死：freshness 窗口内无 settle"）。
3. **B3c（正控）**：`UPDATE audit_relay_provisioning SET verified_at = clock_timestamp() WHERE singleton;` → 跑 check → **exit 0** 且 grep `verdict: healthy`（证明第 4 臂是心跳敏感的，而非「enabled 恒 fail」）。
4. 顺延既有 drop + `b5_check "audit-provision-check" "PASS"`。
**leg D 适配（强制伴随，否则新臂使 leg D 必红）**：leg D 在 enable relay + binding 后、跑 healthy 断言前，先 `INSERT INTO audit_relay_provisioning (verified_at) VALUES (clock_timestamp());`（T-11 DB 从未跑过 relay，无 settle 心跳；seed 即「relay works」仿真）——healthy 断言与 grep 原样保持。**leg C 不动**（dead 行 → fail-closed，dead-priority 回归面）。

### R8 — 心跳契约（settle 语义钉死，design §4 F4/F6/F7 行为落码）
- **只在 fenced settle `Ok(true)` 记心跳**；claim_due/requeue/mark_dead（含 403→dead）/settle `Ok(false)`（租约丢失）/settle `Err` **一律不记**——403-loop、crash、SQL-error-loop 的 relay 永不刷新心跳，freshness 窗口后门自动关（T-11 fail-closed）。
- record 失败（F6）：warn + 不反哺 settle 结果（投递已成功，失败 settle 会导致重投）——门保持 fail-closed 至下个成功 settle；UPSERT 幂等（at-least-once 安全）。
- 多实例：singleton UPSERT last-writer-wins，良性。
- 安静期（无事件 → 无 settle → 心跳过期）：**只影响配给验收，从不阻塞投递**（R7 消费面边界：入队 0236/0239 触发器、投递 relay 状态机、B5-3 排序均不 consult gate）——事件到达并成功投递后自愈。

## §5 Acceptance checks（direction 四项原样保留，逐条 testable）

### AC1 — T-11 mapping：leg B 第三子腿（enabled + 零 dead + relay 实际已死 → fail-closed）
测试 = `scripts/test-integration.sh` leg B3（§4 R7 的 B3a/B3b/B3c 三步）：
- B3a：enabled 开关 + 1 binding + **零 dead 行 + 零 undelivered + 无心跳行** → `verdict: fail-closed — ... no audit:event:write grant issued`（`grep -q "verdict: fail-closed"` ∧ `grep -q "no audit:event:write grant issued"`）+ **非零退出**（当前实现返回 Healthy/exit 0——本腿钉死假绿）。
- B3b：同构 + 过期心跳（`verified_at = now - 2×freshness`）→ 同样 fail-closed + 非零。
- B3c（正控）：心跳新鲜 → exit 0 + `verdict: healthy`。
- **钉位保持**：复用既有 `audit-provision-check` 槽（`b5_check "audit-provision-check" "PASS"`，leg B 既有 :311 行）；`B5_CONTRACT_TEST_LIST` **39/39 不变**（direction 原词 "37/37" 已漂移，见 §1 E5——意图 = 不增槽、不删槽）。
- 回归：leg B1（空库 → consistent）、leg B2（relay off + 1 undelivered → fail-closed）grep 与退出码**一字不变**；leg A（T-11 块内非零断言）不动。

### AC2 — 心跳实现测试：仅 fenced settle 盖心跳（claim/requeue/403 永不）
**AC2.1（unit，heartbeat.rs tests）**：`HeartbeatOutboxRepo` 包假 inner repo（可编程 settle 返回值）+ 计数 `HeartbeatRecorder`——
- `settle → Ok(true)` → recorder 计数 +1、settle 返回值原样 `Ok(true)`；
- `settle → Ok(false)`（租约丢失）→ 计数 0；
- `settle → Err` → 计数 0；
- `requeue` / `mark_dead` / `claim_due` / `reconcile` → 计数 0（纯委托 + 不记）；
- recorder 自身 Err → settle 仍返回 `Ok(true)`（F6：record 失败不反哺投递结果）。
**AC2.2（state_machine.rs 形状，403-loop 闭环）**：`HeartbeatOutboxRepo` 包 `FakeOutbox` + 计数 recorder + `StubSink events_status: 403`——seed N 行 → `relay.dispatch_batch()` → 全部 `FakeStatus::Dead`（attempts==1）→ **recorder 计数 == 0**（403-loop 零心跳行）；随后 `StubSink` 回 202 + 新行 → dispatch → `FakeStatus::Delivered` → **recorder 计数 == 1**（一次 fenced settle = 一次心跳）。既有 `forbidden_dead_on_first_attempt`（:263-290）断言**零改动**保持绿。运行：`cargo test -p aero-audit-connector --test state_machine` + `cargo test -p aero-audit-connector --lib`。

### AC3 — 单一拒绝点：gate 缺席/过期 → HTTP 403 "audit:event:write is not provisioned"
测试 = `crates/aero-auth/src/relay_gate.rs` tests（三态参数化，无 DB）：
- (a) `AuthService::new` 裸构造（未注入 gate）→ `assert_audit_scope_provisioned()` → `Err(Error::Forbidden)`，消息含 **"audit:event:write is not provisioned"**，`status_code() == 403`（aero-common::error.rs:70）；
- (b) 注入恒 `false` gate（模拟无/过期心跳）→ 同 `Err(Forbidden)` + 同消息前缀；
- (c) 注入恒 `true` gate → `Ok(())`。
- **无路径可绕过**：`extractor.rs` 零改动守卫（git diff 不含 extractor.rs）+ 全仓 grep 断言——除 `assert_audit_scope_provisioned` 自身外，不存在任何 audit-scope 机器令牌验收路径（现状已核验，E4）；R8 契约文字入 relay_gate.rs 模块头注释。
- 仓储面（PG `#[ignore]` db_tests，走既有 `--ignored` sweep）：`record_heartbeat` UPSERT 幂等、`provision_check` 三态（Verified/NotVerified/Stale）、freshness 边界（age==freshness → Verified；age==freshness+1 → Stale）。

### AC4 — 回归：dead 行仍最先 fail-closed（dead-priority 矩阵不变）
- `verdict()` 单测（audit_provision.rs 新矩阵）：`dead>0` × {relay on/off} × {心跳 fresh/stale/absent} → **全部 FailClosed**，reason 文案与现状一致（"dead is never counted delivered"）——第 4 臂必须排在 dead 臂之后；
- 既有 2/3/5 臂语义逐臂回归（off+undelivered → fail-closed；off+0 → consistent；on+fresh heartbeat+0 undelivered → healthy；on+fresh heartbeat+undelivered>0 → healthy——积压属正常）；
- harness leg C（0239 门内 dead 报告腿）**一字不改**：`UPDATE ... SET status = 3, last_error = '403 provisioning refusal (drill)'` → check 非零 + grep `dead=1` ∧ `delivered=0` ∧ `verdict: fail-closed`。
- no-touch diff 守卫：变更集**不含** `migrations/0236_*`、`crates/aero-audit-connector/src/relay.rs`、`outbox.rs`、`fake.rs`、`stub.rs`、`snaplink_commercial/*`、`routes/health.rs`、`crates/aero-auth/src/extractor.rs`。

## §6 Test placement

| Test | Location | Harness |
|---|---|---|
| 装饰器委托矩阵 + record 失败不反哺（AC2.1） | `crates/aero-audit-connector/src/heartbeat.rs` tests（假 inner repo + 计数 recorder） | 纯单测，无 DB |
| 403-loop 零心跳 + settle 一次一心跳（AC2.2） | `crates/aero-audit-connector/tests/state_machine.rs` 新测试（`HeartbeatOutboxRepo` + `FakeOutbox` + `StubSink`，relay() helper 同款） | 无 DB（fake/stub） |
| 拒绝点三态（AC3a-c） | `crates/aero-auth/src/relay_gate.rs` tests | 纯单测，无 DB（fake gate） |
| 仓储三态 + UPSERT 幂等 + freshness 边界（AC3 仓储面） | `crates/aero-storage/src/audit_relay_provision.rs` db_tests | PG `#[ignore]`（既有 `--ignored --test-threads=1` sweep，无需新命名槽） |
| verdict 五臂矩阵 + dead 优先回归（AC4） | `crates/aero-eng/src/audit_provision.rs` tests（新 `#[cfg(test)]` 模块） | 纯单测，无 DB |
| leg B3 三子步 + leg D 心跳 seed 适配（AC1） | `scripts/test-integration.sh`（leg B 内、drop 前；leg D 断言前） | shell + throwaway DB |
| readyz 不翻转（R6 sibling 回归） | `routes/health.rs` 既有 `readiness_decision` 单测保持绿 | 单测（零改动） |
| no-touch 守卫（AC4） | review gate + `git diff --stat` | 人工/CI |

## §7 Risks / decisions / [PROPOSED]

- **迁移序号顺延（本切片已裁决）**：req/design 钉的 `0240_audit_relay_provisioning.sql` 与已落地 `0240_audit_governance_due_prio_idx.sql` 冲突——**0246** 为最终序号；DDL/语义不变，只换号。
- **37/37 → 39/39 钉位（本切片已裁决）**：direction acceptance 原词 "37/37" 已过时；本切片承诺 = 复用 `audit-provision-check` 槽、`B5_CONTRACT_TEST_LIST` 槽数 39 不变（b5-pin.sh `assert_b5_contract_pin` 恰好 39 强校验）。
- **freshness 双字面量镜像**：`AERO_AUDIT_PROVISION_FRESHNESS_SECS` 默认 300 / 界 [60,86400] 同时在 connector `config.rs`（duration_secs 家族）与 aero-eng `audit_provision.rs`（env 直读）——aero-eng 零 connector 依赖约束使镜像成为唯一 seam；两处各留注释互相引用（"mirror of RelayConfig::from_env" / "mirror of aero-eng check"）。改动任一处必须同步另一处（单测边界值互证：leg B3 用默认 300 的 2×=600s 过期 seed 即隐式钉住默认值）。
- **leg D 依赖新臂（强制伴随）**：leg D 现断言 `verdict: healthy` 于无心跳 DB——新臂落地后必红；R7 的 seed 步骤与本切片同 commit（防半绿 CI）。
- **`verdict()` 签名不变**（fresh 预计算进 `AuditSnapshot`）：既有无调用点（run/run_priority）零改动，回归面最小。
- **心跳表缺失容忍**（Q6a 探测）：pre-0246 旧库 → heartbeat=absent → 仅 relay on 时 fail-closed（fail-closed 方向正确）；relay off 时 consistent 语义不变——leg B1 与任何未迁移库都不误伤。
- **安静期门关**（settle 语义代价）：无事件 → 无 settle → freshness 后门关——只影响配给验收，从不阻塞投递（R8）；默认 300s ≫ poll 5s，抖动窗口可控。chicken-and-egg（relay 自证：首次投递先于任何心跳）是设计内顺序——gate 不拦 relay 自己的投递（投递面不 consult gate）。
- **`refresh_relay_provision` 不接线**（aero-auth design D3 保持）：pub no-op seam，不挂 boot/serve.rs timer（该行 login_throttle 专用）。
- **aero-cli（aero-server bin）16 命令不实现**：sibling req R4 的面，本 direction acceptance 未要求；harness 面全部走已落地的 aero-eng psql check。
- **「403-loop 门开着」的仓内钉死读法**：AC2.2（零心跳行）+ AC1/B3a（verdict fail-closed）双面钉；403→dead ≤1 状态机（B5-2 已实现）不动，T-11 因果链（gate 不健康 ⇒ IdP 拒配给 ⇒ 403 ⇒ dead）回归保持。

## §8 Sequencing

1. **迁移 + 仓储**：`0246_audit_relay_provisioning.sql` + `AuditRelayProvisionRepo` + db_tests（**先 `cargo build` 再 migrate**）。
2. **connector**：`heartbeat.rs`（trait + 装饰器 + AC2.1 单测）+ `RelayConfig.provision_freshness` + 6 处字面量同步。
3. **aero-auth seam**：`relay_gate.rs`（trait + Pg 实现 + AuthService 字段/builder/hook + 拒绝点）+ lib.rs re-export + AC3 三态单测。
4. **aero-server 接线**：`PgHeartbeatRecorder` + main.rs 包装 + boot/services.rs gate 注入；health.rs 零改动确认。
5. **aero-eng 心跳臂**：Q6a/Q6b + `AuditSnapshot` 扩展 + verdict 第 4 臂 + report 行 + freshness env 解析 + AC4 矩阵单测。
6. **harness**：leg B3（B3a/B3b/B3c）+ leg D seed 适配（同 commit）；验 `bash scripts/test-b5-pin-guard.sh` → `bash scripts/test-integration.sh` → 期望 `B5-CHECK audit-provision-check: PASS` + `B5 contract pin: 39/39 ... PASS`。
7. **门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· `scripts/test-integration.sh` · no-touch diff 守卫（AC4）。
