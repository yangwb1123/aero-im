# Design — aero-cli B5-4: `aero-eng audit-provision-check`（psql-backed 变体，contract-designated seam）

- **Direction**: "Implement the contract-designated `aero-cli audit-provision-check` seam (B5-4): psql-backed gate subcommand verifying fail-closed scope provisioning and outbox status 0/1/2/3 health"（value 8 / risk 8 / effort 6 / confidence 7）
- **Module**: `crates/aero-cli/src`（package `aero-cli` → 唯一 `[[bin]] aero-eng`，677 行仅 main.rs）；逻辑落 `crates/aero-eng`（lib，可单测）
- **Sibling conflict**: in-tree sibling design `docs/design/2026-08-07-aero-cli-b5-4-audit-provision-check.design.md` 注册**同名** `audit-provision-check`（env-preflight 脚本变体，exit 0–6）。本 direction 的 acceptance 原文（pipeline.yaml，verbatim）钉死 psql-backed 语义 —— 集成时只能保留一个（§6.3）。

## §1 证据核验（全部逐条对照源码，含 3 处修正/新增事实）

| # | 证据声明 | 核验 | 修正/补充 |
|---|---|---|---|
| E1 | `a!()` 注册链 (:23-33) + `Completion_` cmds 串 (:315) 是两个新子命令槽；`Gate_` 用 `run_cmd("bash", &[script], timeout)` (:112-114)；checks.rs/run.rs 属 `aero-eng` | ✅ | `a!(Network_);` 在 main.rs:30；`let cmds = "check gate test integration skill doctor network completion help";` 在 :315；`async fn b(...) { run_cmd("bash", &[&p], ...) }` 在 :110-114；`aero-cli/src/` 只有 main.rs，`crates/aero-eng/src/` = checks.rs/command.rs/config.rs/context.rs/lib.rs/outcome.rs/registry.rs/run.rs/term.rs |
| E2 | `from_env` 在 runtime.rs:42-66（非 mod.rs）；`Unspecified → require_disabled → Ok(None)`；`ready()` :87-107；DB `runtime.enabled` 是 enforcement 事实源 → check 必须读 DB 不读 env | ✅ | `SnaplinkCommercialRuntime::from_env` 在 `snaplink_commercial/runtime.rs:42-66`，`:47-53` Unspecified 分支确认；`ready()` 在 :87-89；0235 建 `snaplink_commercial_runtime(singleton BOOLEAN PK CHECK(singleton), enabled BOOLEAN NOT NULL DEFAULT FALSE)` 恒 1 行；0236 触发器读同一开关 |
| E3 | `SCOPE_AUDIT = "audit:event:write"` 在 http.rs:23 | ✅ | 精确命中 `snaplink_commercial/http.rs:23`；`:146` 用于 Audit 投递；`validate_audit_receipt` :410 |
| E4 | 0236 触发器 :129-131（audit_events AFTER INSERT → outbox，relay 关时行照样产生） | ✅ | 0236 实际 :127-131：`DROP TRIGGER …; CREATE TRIGGER audit_events_snaplink_delivery AFTER INSERT ON audit_events …`（:129 为 AFTER INSERT 行），入 `snaplink_delivery_outbox` destination='audit' |
| E5 | proposal `docs/proposals/audit-contract-batch-aero-im.md:11` B5-4 seam 指定 | ✅ | B5-4 节原文 "本仓库交付配给验证 seam（`aero-cli audit-provision-check`，[PROPOSED]）+ fail-closed" |
| E6/E7 | `scripts/test-integration.sh:292-327` 已有该命令的等待契约：help-grep 检测、`DATABASE_URL`+`AERO__DATABASE__URL` 对 T-11 drill DB 调用、非零断言、`b5_check "audit-provision-check"` 槽已在 37/37 清单（`scripts/b5-pin.sh`） | ✅ | 契约在 :306-327（T-11 块内、0239 文件门内）；`B5_CONTRACT_TEST_LIST` = 15 executed + 22 [PROPOSED]，`assert_b5_contract_pin` 强校验 **恰好 37 槽** + 每个 executed 槽 ≥1 条 `B5-CHECK <name>: PASS|SKIP` 证据行 |
| E10 | 0239 status 0=pending/1=claimed/2=delivered/3=dead；表名漂移风险（`audit_governance_outbox` per T-11 drill vs `audit_outbox` per bus design）→ spec 要求候选探测 | ✅ | **0239 迁移尚不存在**（migrations/ 共 238 个，止于 0238）；`aero-audit-{t11,relay,priority}-drill.rs` 均 `to_regclass('audit_governance_outbox')`（缺表 exit 2 SKIP）；test-integration.sh 全程以文件名 `migrations/0239_audit_governance_outbox.sql` 作门；b5-1 bus design（`2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.design.md`）定表名 `audit_outbox` + 子表 `audit_outbox_frame` —— 漂移属实 |
| E12 | sibling req/design 定义同名命令的 env-preflight 变体；本 spec 按 direction acceptance 钉 psql-backed | ✅ | sibling design 注册 `AuditProvisionCheck_, "audit-provision-check"` 于**同一 registry**，spawn `scripts/audit-provision-check.sh`；两变体均满足 help-grep + 非零 leg A，但只有 psql-backed 满足 legs B/C（DB 状态断言）与 acceptance 原文（snaplink_delivery_outbox 行、0239 分布、dead 终态） |

**新增关键事实（证据未述，设计已吸收）**：
- **F1 — 本机无 `psql` 二进制**（`command -v psql` 空），harness 在 container 模式跑（`docker exec -i -e PGPASSWORD=… aero-postgres psql`，容器存在）。psql-backed check 必须实现 harness 同款 PSQL_MODE 回退（local → container），否则本环境不可测（§2.4）。
- **F2 — 两个包都有名为 aero-cli 的 bin**：package `aero-cli` → bin `aero-eng`（本模块）；package `aero-server` → bin `aero-cli`（`src/bin/aero-cli.rs`，702 行 DB CLI，含 migrate）。harness 的 `cargo run -p aero-cli -- help` 是**包限定**的（确定性 → aero-eng）；`cargo run --bin aero-cli -- migrate` 无 `-p`，解析到 aero-server 的 bin。新增命令只动 `crates/aero-cli/src/main.rs`，不加 `[[bin]]`。
- **F3 — `run_cmd` 用 `cmd.output()` 吞 stdout**（`run.rs:20-48`）→ 报告线会被吞进 detail，D1 成立：必须直 spawn（`tokio::process::Command`，root tokio `features=["full"]`，aero-eng 零 Cargo.toml 改动）。
- **F4 — `snaplink_delivery_outbox` 无 status 列**（0235:161-190：`delivered_at`/`claim_token`/`lease_expires_at`/`attempts`/`last_error`），无 FK（可直插 seed）；v1 无 dead 终态（退避重试直到永远）。undelivered 语义 = `delivered_at IS NULL` —— 与仓储自身 `pending_count()`（`snaplink_commercial.rs:387-390`）完全一致。
- **F5 — Outcome 退出码**：`ok→0 / error→1 / warning(code)`；main.rs `Ok(r)→exit 0`、`Err(r)→exit(r.exit_code())`，error message 走 stderr。
- **F6 — workspaces 表极简**（id/name/slug/created_by/created_at）→ 健康态 drill 可直插 binding（bindings 无 FK 到 workspaces）。

## §2 API 变更

### 2.1 `crates/aero-cli/src/main.rs`（3 处，全是已核验的槽位）

1. 注册链：`a!(Network_);`（:30）之后加 `a!(AuditProvisionCheck_);`。
2. 新命令（`c!` 宏，跟在 `Network_` 定义后）：

```rust
c!(
    AuditProvisionCheck_,
    "audit-provision-check",
    "Verify audit relay provisioning (psql-backed): fail-closed grant check + outbox status distribution",
    |_ctx, _args| {
        let url = std::env::var("DATABASE_URL")
            .or_else(|_| std::env::var("AERO__DATABASE__URL"));
        match url {
            Ok(url) => aero_eng::audit_provision::run(&url).await,
            Err(_) => Outcome::error(
                "audit-provision-check: no database URL — set DATABASE_URL or AERO__DATABASE__URL",
            ),
        }
    }
);
```

3. completion 串（:315）：`"check gate test integration skill doctor network audit-provision-check completion help"`。

注册后 `aero-eng help` 自动列出该名（registry.rs `print_help` 打印 `cmd.name()`）→ harness help-grep 检测契约满足。

### 2.2 `crates/aero-eng/src/audit_provision.rs`（新模块，`lib.rs` 加 `pub mod audit_provision;`）

纯函数层（可单测，零 IO）：

```rust
pub struct V1OutboxCounts { pub pending: i64, pub claimed: i64, pub delivered: i64 }
pub struct G0239Counts {
    pub table: Option<&'static str>,          // 解析到的候选名
    pub pending: i64, pub claimed: i64, pub delivered: i64, pub dead: i64,
    pub oldest_pending_secs: Option<i64>,
}
pub struct AuditSnapshot {
    pub relay_enabled: bool, pub enabled_bindings: i64,
    pub v1: V1OutboxCounts, pub g0239: Option<G0239Counts>,
}
pub enum Verdict { FailClosed(String), Consistent, Healthy }
pub fn verdict(s: &AuditSnapshot) -> Verdict;          // 3 态矩阵，dead 优先
pub fn format_report(s: &AuditSnapshot, v: &Verdict) -> String;  // greppable 行
pub fn parse_psql_bool_line(&str) -> Result<bool, String>;      // "t"/"f"
pub fn parse_probe_line(&str) -> Option<&'static str>;          // to_regclass 候选解析
pub fn parse_buckets(&str) -> [i64; 4];                        // "0|3\n2|10" → 0..3 桶
```

执行层（`run`，直 spawn，**不用 run_cmd**）：

```rust
pub async fn run(db_url: &str) -> Outcome;
```

`run` 流程：解析 URL → 确定 psql 启动方式（§2.4）→ 顺序执行 Q0-Q5（§2.3，每条 30s 超时）→ `verdict` + `format_report` → println 报告 → 返回 Outcome。**任何非判定路径返回 `Outcome::error`（exit 1，永不为 0）**；判定映射：FailClosed → `Outcome::error(reason)`（reason 进 stderr，leg 以 `2>&1` 捕获），Consistent/Healthy → `Outcome::ok("")`（报告已在命令内打印，避免 main.rs 重复打印）。

### 2.3 SQL 契约（psql `-At -v ON_ERROR_STOP=1 -c …`，每条独立 spawn）

| # | SQL | 输出 | 用途 |
|---|---|---|---|
| Q0 | `SELECT runtime.enabled, (SELECT count(*) FROM snaplink_commercial_bindings WHERE enabled) FROM snaplink_commercial_runtime runtime WHERE runtime.singleton` | `t\|2` | relay 健康谓词（**DB 开关 + bindings 数，刻意不用 `ready()`**：ready() 含 entitlement projection 语义，非配给判据） |
| Q1 | `SELECT count(*) FILTER (WHERE delivered_at IS NULL AND claim_token IS NULL), count(*) FILTER (WHERE claim_token IS NOT NULL), count(*) FILTER (WHERE delivered_at IS NOT NULL) FROM snaplink_delivery_outbox WHERE destination = 'audit'` | `3\|0\|10` | v1 派生列（pending/claimed/delivered）；`destination='audit'` 过滤（usage 是独立 relay 路径，不参与审计配给判定） |
| Q2 | `SELECT to_regclass('audit_governance_outbox')::text, to_regclass('audit_outbox')::text` | `audit_governance_outbox\|` | 0239 表名候选探测（**漂移单点**：顺序 = T-11 drill/harness 钉名在前，bus design 名在后；两缺 → 0239 段跳过） |
| Q3 | `SELECT status, count(*) FROM <t> GROUP BY status ORDER BY status` | `0\|3`⏎`2\|10` | 0239 四桶（缺 status 补 0） |
| Q4 | `SELECT extract(epoch FROM (clock_timestamp() - min(available_at)))::bigint FROM <t> WHERE status = 0` | `0` | oldest-pending 秒数（无 pending → 空行 = None） |
| Q5 | `SELECT event_id::text, left(coalesce(last_error, ''), 120) FROM <t> WHERE status = 3 ORDER BY available_at LIMIT 5` | `uuid\|403 …` | dead 明细（仅 dead>0 时跑） |

表名 `<t>` 只允许取自已探测的**固定候选字面量**（`to_regclass` 结果回映射到已知名），绝无用户输入插值。

### 2.4 psql 启动方式（F1 适配，镜像 harness `PSQL_MODE`）

1. `AERO_PSQL_MODE` env：`local` \| `container` \| 缺省 auto。
2. auto：先 `psql`（NotFound 再试 container）；container = `AERO_POSTGRES_CONTAINER`（缺省 `aero-postgres`）→ `docker exec -i -e PGPASSWORD=<pass> <container> psql`。
3. 连接参数：解析 `postgres://user:pass@host:port/db`（harness 同款解析：strip scheme → 末 `@` 分凭据/host → `:` 分 user/pass → `/` 分 db）；`PGPASSWORD` env 传口令（**不进 argv**），参数 `-h -p -U -d`；host 缺省 localhost、port 缺省 5432。
4. 本地/容器两模式均不可用 → `Outcome::error`（exit 1，消息含 AERO_PSQL_MODE/AERO_POSTGRES_CONTAINER 提示）。

### 2.5 报告格式（全部 `audit-provision-check:` 前缀，leg grep 面）

```
audit-provision-check: relay: enabled=true bindings=2
audit-provision-check: v1-outbox: pending=0 claimed=0 delivered=0
audit-provision-check: outbox-0239: table=audit_governance_outbox pending=2 claimed=0 delivered=0 dead=0
audit-provision-check: oldest-pending-age: 0s
audit-provision-check: dead: <event_id> <last_error(≤120)>
audit-provision-check: verdict: <verdict> — <reason>
```

- 0239 缺表：`audit-provision-check: outbox-0239: not migrated`（无 oldest-pending-age / dead 行）；v1 判定照常。
- 判定 3 态（dead 优先）：
  - `fail-closed — N dead row(s): the audit:event:write grant was refused (403/provisioning); dead is never counted delivered`（dead>0，relay 开或关都算）
  - `fail-closed — relay disabled (bindings=M) with K undelivered audit row(s); no audit:event:write grant issued`（relay 不健康 ∧ v1 undelivered(=pending+claimed) + 0239 pending/claimed > 0）
  - `consistent — relay disabled, zero undelivered audit rows`（relay 不健康 ∧ 全空）
  - `healthy — relay enabled (N bindings)`（relay 健康 ∧ dead=0；积压 pending 属正常，exit 0）
- 退出码：fail-closed=1 / consistent=0 / healthy=0 / 一切操作错误=1（永不为 0）。

## §3 Harness legs（test-integration.sh；leg A 原样不动）

- **leg A（既有，零改动）**：T-11 块内 :306-327 —— help-grep → 对 `T11_DRILL_URL` 调 check 断言非零 → `b5_check "audit-provision-check" "PASS|SKIP"`。
- **leg B（新增，always-run，v1）**：插在 SCIM regression（:245）与 B5-1 0239 门（:247）之间，独立 throwaway DB（`AUDIT_PROVISION_DB="aero_audit_provision_$$"` + `assert_disposable_db_name`）：
  1. help-grep 检测命令（同 leg A 守卫）→ 缺失则 `b5_check "audit-provision-check" "SKIP (B5-4 audit-provision-check not landed)"`。
  2. B1：建库 → `cargo run --bin aero-cli -- migrate` → 跑 check → **exit 0** 且 grep `verdict: consistent`（空库 = relay off + 零 undelivered → consistent）。
  3. B2：`run_psql` 直插 1 行 v1 audit 行（`delivery_id='audit:b5-leg-b'`、`destination='audit'`、payload `{"event_id":…}`，无 FK 约束，CHECK 全满足）→ 跑 check → **非零** 且 grep `verdict: fail-closed`（relay off ∧ undelivered=1 → 拒发 grant）。
  4. drop DB；`b5_check "audit-provision-check" "PASS"`（**复用既有槽，37/37 不变**）。
- **leg D（新增，0239 门内，healthy）**：在 leg A 之后、`drop_created_database "$T11_DRILL_DB"` 之前：
  1. `run_psql`：`UPDATE snaplink_commercial_runtime SET enabled = TRUE;` + 插 binding（`gen_random_uuid(), 'tenant-b5d', 'client-b5d', 'aero-im.source', 1, TRUE`）。
  2. 跑 check → **exit 0** 且 grep `verdict: healthy` + `outbox-0239: table=audit_governance_outbox pending=[0-9]+ claimed=0 delivered=0 dead=0` + `oldest-pending-age:`（drill 已留 N 行 pending，available_at=clock_timestamp()）。
- **leg C（新增，0239 门内，dead 报告）**：leg D 之后、drop 之前：
  1. `run_psql`：`UPDATE audit_governance_outbox SET status = 3, last_error = '403 provisioning refusal (drill)' WHERE event_id = (SELECT event_id FROM audit_governance_outbox WHERE status = 0 LIMIT 1);`
  2. 跑 check → **非零** 且 grep `dead=1` + `delivered=0` + `audit-provision-check: dead:` + `verdict: fail-closed`（403 → dead 终态被报告，**永不折入 delivered**）。
  3. `b5_check "audit-provision-check" "PASS"`。

三条腿全部复用 `audit-provision-check` 槽（多行 verdict 证据合法：pin guard 只要求 ≥1）；`B5_CONTRACT_TEST_LIST` 保持恰好 37。

## §4 兼容性约束

1. **零新 crate 依赖**：aero-cli 保持 {aero-eng, tokio, serde_json, async-trait}；aero-eng 现成 tokio(full)/serde_json/time/anyhow；root tokio `features=["full"]` → `tokio::process::Command` 直 spawn 零 Cargo.toml 改动（F3）。
2. **零 SQL 迁移**：0239 归 B5-1 sibling 交付；本 check 以候选探测容忍表名漂移（E10），0239 缺表窗口内 v1 判定照常、leg A 保持 SKIP 绿。
3. **不加 `[[bin]]`**：package aero-cli 仍只有 `aero-eng`；`cargo run -p aero-cli -- <cmd>` 包限定解析不变（F2）。
4. **psql 可用性**：local/container 双模式（§2.4），两缺 → exit 1 可操作消息，**永不 exit 0**。
5. **`run_cmd` 禁用**（D1）：吞 stdout；直 spawn 保报告上 stdout。
6. **37/37 钉死**：不增槽；legs B/D/C 复用 `audit-provision-check` 槽。
7. **leg A 原样**：既有 T-11 腿（help-grep 检测、双 URL 调用、非零断言）一字不改。
8. **判定语义对齐仓储**：v1 undelivered = `delivered_at IS NULL`（= `pending_count()`）；`destination='audit'` 过滤；relay 谓词 = DB `runtime.enabled` ∧ bindings>0（E2：DB 是 enforcement 事实源，env 缺省不代表 DB 被另一副本激活，读 env 会假绿）。

## §5 失败模式

| # | 失败 | 行为 | 缓解 |
|---|---|---|---|
| FM1 | psql 缺失 + docker/容器缺失 | exit 1，消息指 AERO_PSQL_MODE/AERO_POSTGRES_CONTAINER | §2.4 双模式 + auto 回退 |
| FM2 | DB 不可达/认证失败 | psql 非零 → exit 1（回显 stderr） | 永不为 0（R6） |
| FM3 | 0235 未迁移（schema 缺） | Q0 psql error → exit 1 | 无假 "consistent" |
| FM4 | 0239 未迁移 | Q2 两候选均空 → `outbox-0239: not migrated`，v1 照判 | 窗口期 harness 绿（SKIP） |
| FM5 | 0239 表名漂移 | Q2 候选解析；B5-1 若落第三个名 → 改候选表（单点） | E10 探测契约 |
| FM6 | DATABASE_URL/AERO__DATABASE__URL 双缺 / URL 畸形 | exit 1 可操作消息 | §2.1 预检 |
| FM7 | psql 挂死（网络黑洞） | 30s 超时 → exit 1 | §2.2 timeout |
| FM8 | dead 折入 delivered（假绿） | 构造上杜绝：dead 独立桶、verdict dead 优先、leg C grep `dead=1`∧`delivered=0` | A3 + 单测 |
| FM9 | 读 env 而非 DB 判 relay 健康（假绿） | 谓词只读 DB 开关 + bindings | E2/F2 |
| FM10 | sibling env-preflight 先落同名命令 | registry 首匹配歧义；集成须只留一个 | §6.3 集成决策 |
| FM11 | 过期 lease 的 claimed 行 | 计 undelivered（保守 fail-closed 方向） | 与仓储语义一致 |
| FM12 | 操作错误 exit 0 | 非判定路径全部 `Outcome::error` | §2.2 run |

## §6 迁移步骤（落地顺序，每步独立可验）

1. **aero-eng 新模块**：`audit_provision.rs`（纯类型 + verdict/report/parse + 单测）+ `lib.rs` `pub mod`。验：`cargo test -p aero-eng`。
2. **main.rs 注册**：`a!(AuditProvisionCheck_);` + 命令体 + completion 串。验：`cargo build -p aero-cli`；`cargo run -p aero-cli -- help | grep audit-provision-check`。
3. **手烟**（throwaway 库，先 build 后 migrate —— 本批无新迁移，0235+ 已足）：建库迁移 → check（consistent）→ 插 v1 audit 行 → check（fail-closed）→ `UPDATE runtime SET enabled=TRUE` + 插 binding + 插 0239 行（若 0239 已落）→ check（healthy + 分布 + oldest-pending-age）。
4. **harness**：leg B 段 + DB var/assert 注册；T-11 块尾接 legs D/C。验：`bash scripts/test-b5-pin-guard.sh`（pin guard 独立自测）→ `bash scripts/test-integration.sh`（全量）→ 期望 `B5-CHECK audit-provision-check: PASS` + `B5 contract pin: 37/37 … PASS`。
5. **门禁**：`cargo clippy --workspace --all-targets`（零新警告）· `cargo test --workspace --lib` · `scripts/{truth-check,file-size-check,dependency-check}.sh`（audit_provision.rs < 800 WARN 线；aero-cli deps 不变）。
6. **sibling 集成**：合并时删除/改名 sibling 的 `AuditProvisionCheck_` + `scripts/audit-provision-check.sh`；其 `Gate_ audit` 臂仅当改名（如 `gate audit-preflight`）后保留 —— 本 direction 不实现 Gate_ 臂（scope 内无此项；`gate b5` 已承载 harness 门）。

## §7 可测试验收映射（direction acceptance 原句 → 断言）

| 验收 | 断言（greppable / 可执行） |
|---|---|
| **A1** "exits non-zero with actionable message when: relay disabled/bindings empty while snaplink_delivery_outbox has undelivered rows (fail-closed preserved — no audit:event:write grant issued)" | leg B2：空库插 1 行 v1 audit 行 → `cargo run -p aero-cli -- audit-provision-check` 退出非零，输出含 `verdict: fail-closed` 与 `no audit:event:write grant issued`；单测：`verdict(relay off, v1.undelivered=1) == FailClosed` |
| **A2** "exits 0 and prints status distribution (pending/claimed/delivered/dead per the new 0239 status 0/1/2/3 DDL) and oldest-pending age when relay healthy" | leg D：T-11 DB 上 enable relay + 1 binding → check **exit 0**，输出含 `verdict: healthy`、`outbox-0239: table=audit_governance_outbox pending=[0-9]+ claimed=0 delivered=0 dead=0`、`oldest-pending-age:`；单测：report 格式含四桶 + age 行 |
| **A3** "T-11 variant: simulated 403/provisioning refusal → row reaches dead terminal and check reports it, never 'delivered'" | leg C：1 行翻 status=3 + last_error '403 …' → check 非零，输出含 `dead=1`、`delivered=0`、`audit-provision-check: dead:`、`verdict: fail-closed`；单测：`verdict(dead=1, relay on) == FailClosed` 且 delivered 桶不含 dead |
| **A4** "Check runs inside test-integration.sh so it contributes to the 37/37 gate" | legs B/D/C 各发 `b5_check "audit-provision-check" "PASS"`（复用钉死槽）；`assert_b5_contract_pin` 通过（37/37、无 vacuous、verdict 证据齐） |

## §8 范围红线

- 不改 aero-server / aero-audit-connector / aero-bus / aero-storage 生产代码；不实现 `Gate_` 新臂；不加 `[[bin]]`；不改 `B5_CONTRACT_TEST_LIST` 槽数；不改既有 leg A。
- 不实现 sibling 的 env-preflight 语义（`AERO_AUDIT_EVENTS_URL` 等 env 探测、0–6 退出矩阵）—— 那是 sibling direction 的落点；本 direction acceptance 为 psql-backed 权威。
