# Design — B5 acceptance gate harness：`aero-eng integration` + `gate b5` + 37/37 钉入 + T-11/moderation-priority drill（全走 `scripts/test-integration.sh`）

- **Module**: `crates/aero-cli`（aero-eng 工程 CLI，`[[bin]] name="aero-eng"`）+ 上游 `crates/aero-eng/src/run.rs` + `scripts/test-integration.sh` + 新 drill bin 落家 `crates/aero-audit-connector/src/bin/`（自动发现）
- **上游**: `docs/requirements/2026-08-07-aero-cli-b5-acceptance-gate-harness.req.md`（R1-R8）；sibling `docs/requirements/2026-08-07-aero-cli-b5-2-relay-probe.req.md`、`2026-08-07-aero-cli-b5-4-audit-provision-check.req.md`
- **对象状态机**: `crates/aero-audit-connector/src/relay.rs` + `client.rs` + `pg.rs`（已实现，**本设计零生产改动**——只加两个 drill bin）
- **Status**: Design。证据核对日期 2026-08-07（全部实读/实跑，见 §1 账本）

## 0. 设计摘要（TL;DR）

```
L0  drill bins   crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs        （新 bin，0239 门控）
                 crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs   （新 bin，0239+priority 列门控）
L1  CLI 面       aero-eng integration（Integration_ 命令 → run::test_integration()）
                 + gate b5（Gate_ 臂 → 同一 scripts/test-integration.sh，单一事实源）
L2  harness      test-integration.sh B5 段：B5_CONTRACT_TEST_LIST（37 槽）+ assert_b5_contract_pin
                 + T-11/priority drill 段 + relay 覆盖三腿断言（fan-out 恒跑 / A3 门控 / probe 存在性门控）
```

零生产代码改动：connector `src/` 一行不动（只加 bin，Cargo.toml 零改动）、aero-cli/aero-eng `Cargo.toml` 零改动、零迁移、零新 env。B5 段与 drill 断言全部走**已实现状态机的可达公共面**（`dispatch_batch`/`claim_due`/`settle`/`requeue`/`mark_dead` + 行快照 SQL 计数）。

## 1. 证据核对账本（untrusted evidence → 实读验证 + 设计新事实）

| Evidence claim | Result |
|---|---|
| `main.rs` 注册表 `a!()` :23-31（Check_/Gate_/Test_/…/Dashboard_，**无 integration**） | ✅ 逐行命中（:23-31 共 9 个 `a!`）；二进制名 `aero-eng`（`Cargo.toml` `[[bin]] name="aero-eng"`） |
| `Test_` :101 = 仅 `cargo_test_lib`；`Gate_` :104 + `b()` :105-107 = `run_cmd("bash", &[&p], t)`；`gate list` 串 :113 | ✅ 逐行命中；`b()` 经 `ctx.root.join("scripts")` 解析脚本路径 |
| `Dashboard_` :597-598 广告 "Run: aero-cli test --workspace --lib" / "Integration: aero-cli integration" | ✅ 逐行命中（:596 header 同款 "aero-cli test" 二进制名错——设计并入 R7 一并修） |
| `run.rs:54` = `cargo_test_lib`；无 integration wrapper；`run_cmd` 非零拍平 exit 1 | ✅ :54 精确命中；`run_cmd` :15-41 非零 → `Outcome::error`（exit 1，exit_code 进 detail）；`cargo_check` :44 / `cargo_clippy` :63 |
| `scripts/test-integration.sh` relay_tests skip（cited :204 → 现 :321-322）；fan-out 套件 :274；empty-filter guard :183 | ✅ **行号漂移勘误确认**。`--skip db_tests::relay_tests` = **:322**；`bash scripts/test-notification-fanout.sh` = **:274**（在 `SKIP_DB_CREATE` 空分支内——"恒跑"仅对 fresh 模式成立，设计 D7）；empty-filter guard `test result: ok\. [1-9][0-9]* passed` = **:183**；A3 pin 占位注释 = :265-269；脚本 `set -euo pipefail`（:21） |
| `docs/proposals/audit-contract-batch-aero-im.md:15` "37/37 需先把契约测试清单钉入 test-integration.sh" | ✅ :15 精确命中（:13 "37/37 测试清单不可独立核对，不冒充 Verified"） |
| `docs/campaigns/implementation-gate.md` G6 行 :78 = "37/37、T-11、moderation 优先级"；:63-66 aero-im 行 | ✅ :78 精确命中 |
| 0239 未落地（migrations 尾号 0238） | ✅ `ls migrations/0239*` 无；test-integration.sh :226-263 B5-1/A3 段均 `-f migrations/0239_audit_governance_outbox.sql` 门控 |
| connector 车辆就绪：A3 drill bin + `StubSink`/`FakeOutbox`；24/24 in-crate 测试 | ✅ `src/bin/aero-audit-relay-drill.rs`（170 行）在；`stub.rs::StubSink::{start,token_url,events_url,posts,set_behavior,shutdown}` 全 pub；connector `Cargo.toml` 依赖 tokio/reqwest/sqlx/serde_json/anyhow/uuid/time 等——**新 bin 零 Cargo.toml 改动** |

### 1.1 设计推导出的新事实（evidence 未覆盖/与 requirement 冲突，design 据此定断言）

| # | 事实 | 来源 | 设计影响 |
|---|---|---|---|
| D1 | **T-11 "零 claim / `SUM(attempts)==0`" 经公共面不可达**：`dispatch_batch`（relay.rs:143-154）**先 `claim_due` 再逐 claim `deliver_claim`**；token 在 `client.deliver`（client.rs:120 `access_token`）内按 claim 获取。token endpoint 连接被拒 → `transport_error` → `Transient` → `requeue`（pg.rs:167-190：status 回 0、attempts 保持 +1 后的值、available_at=now+backoff(attempts)）。**每个 dispatch 轮次必然 claim（attempts += 1）后重泊** | `relay.rs` / `client.rs` / `pg.rs` 实读 | T-11 drill 断言改为**可达不变量**：1 轮后 `COUNT(status=0)==N`、`COUNT(status IN (1,2,3))==0`、**`SUM(attempts)==N`**（证明 relay 真跑过——非 vacuous）、`COUNT(last_error LIKE '%audit connector HTTP transport failed%')==N`（证明 token endpoint 真被尝试）；睡 1.2s（> backoff(1)=1s，同机 PG 时钟）后第 2 轮 → `SUM(attempts)==2N`、仍零终态（retry-forever 姿势）。"永不静默成功、永不误判 dead" 是 T-11 核心，可达；"从未被 claim" 不是 |
| D2 | **priority seed 数值与 B5-3 语义冲突**：requirement R5 钉 moderation `priority=0`、backlog `priority=100`，但 B5-3 切片明确钉 **`ORDER BY priority DESC`（higher = more urgent，B5-3 req DR1 + R-1 风险注：不得混用 ai_job 的 ASC/lower-first 惯例）**。DESC 下 0 < 100 → 积压先行 → drill 必败 | `docs/requirements/2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md` DR1/R-1；aero-ai moderation req R3 "lane value must sort ahead of backlog，与 B5-3 claim_due 共享" | drill 常量改为 **`BACKLOG_PRIORITY=100`（默认积压道，ai_job 先例）**、**`MODERATION_PRIORITY=200`（严格高于积压道，DESC 下必先行）**；注释标注 [PROPOSED] 契约值、B5-3 改判只动这两个常量（§8 决策点 P2） |
| D3 | **priority drill 只门控 0239 文件不够**：claim 排序是 B5-3（aero-storage/connector claim）的交付物，0239 落地 ≠ 排序落地。若 0239 先落地而排序未落地，drill 会 FAIL（不是 SKIP） | `pg.rs::claim_due` 实读（当前 `ORDER BY candidate.available_at, candidate.created_at, candidate.event_id`，**无 priority**）；B5-3 req §3 dependency-owned | 门控分两级：harness 段 = 0239 文件门（同 A3 先例）；**drill 内部 = 运行期能力门**（`to_regclass` + `information_schema.columns` 查 `priority`、`class` 两列，缺任一 → exit 2 SKIP）。0239 落地但排序未落地 → drill FAIL = **诚实的 G6 红信号**（B5-3 未完成），排期 §6 注明 B5-3 必须先于本段转绿 |
| D4 | **首轮 claimed 集经公共面不可直接观测**：`dispatch_batch` 只返回 `usize`（claim 数），不返回 claim 列表。但 concurrency=1 串行 settle ⇒ **settle 序 == claim 序**，且 StubSink 恒 202 ⇒ 首轮 claimed 集 = 首轮后 `status=2` 的行集 | `relay.rs:143-154`（`for_each_concurrent(concurrency)`）；A3 drill 先例 | priority drill 首轮断言 = 轮 1 后 `COUNT(status=2)==100`（batch_size 生效）+ moderation 行 ∈ status-2 集且 `delivered_at == MIN(delivered_at)`（**claimed and delivered first**） |
| D5 | **pin guard 的 SKIP 语义**：requirement 要求 0239 窗口门保持绿（显式 SKIP 非 vacuous），又要求"每个非 [PROPOSED] 条目映射到已执行检查"。两者只有一种一致读法：**`B5-CHECK <name>: PASS\|SKIP` 具名判词行**——SKIP（带原因）算"已处理"，判词缺失才算"未映射" | req R3/R4/§7 交叉核对 | `assert_b5_contract_pin`：槽数/格式/去重/执行型 ≥1 = 无条件；判词证据检查 = 非 `SKIP_DB_CREATE` 模式（dev 卫生模式降级，§5 F13） |
| D6 | **37/37 钉入所有权与 sibling 重叠**：B5-4 sibling req E9/R5 也声明"把占位变真实具名清单"；B5-2 sibling 声明 relay-mock 接线入 test-integration.sh | sibling req docs 实读 | 所有权裁定：**本切片拥有 `assert_b5_contract_pin` + 清单骨架**（总装门语义，proposal :15 归 G6 总装）；B5-4 只往清单加 `audit-provision-check` 条目、B5-2 加 `relay-mock-probe` 条目；最终 37 槽组成在集成时对账（guard count==37 自动兜底，§8 P3） |
| D7 | **fan-out 腿"恒跑"仅对 fresh 模式成立**：`:274` 的 fan-out 在 `if [ -z "$SKIP_DB_CREATE" ]` 分支内；SKIP_DB_CREATE 模式 + probe 未落地 = 唯一 0 腿组合 | `test-integration.sh` 结构实读 | relay 覆盖断言：腿 = PASS 判词数 ≥1；probe 腿 DB-free（B5-2 落地后无条件可跑）；SKIP_DB_CREATE + probe 缺席 = 瞬态窗口，记录为已知红组合（§5 F13） |
| D8 | **`b()` 脚本路径经 `ctx.root`**：`aero-eng integration` 的 `run::test_integration()` 无 ctx（镜像 `cargo_test_lib`），脚本路径相对 CWD ⇒ **须 repo 根运行**（与全部 `gate` 臂同约束）；test-integration.sh 内部还相对引用 `scripts/test-notification-fanout.sh` | `main.rs:104-107`、run.rs 实读 | 兼容性约束 C4 |

## 2. Verified current state

```
G6 无执行家现状（三条独立事实，全部复核）：
a) aero-eng（E1/E2）  9 命令；Test_ = 仅 cargo_test_lib；Gate_ 无 b5 臂；
                      dashboard 广告 'aero-cli integration'（命令不存在 + 二进制名错）
b) harness（E3）      主库 run --skip db_tests::relay_tests（:322，卫生注释 :317-320 指向执行家）；
                      im-core relay_tests 经 test-notification-fanout.sh（:274，fresh DB）已执行；
                      审计 relay 语义 = A3 drill（:238-263）→ 0239 未落地 → 恒 SKIP；
                      :265-269 37/37 钉入 = 占位注释（非真实清单）
c) 契约（E4/E5）      37/37 清单仓外 [PROPOSED]（proposal :13）；G6 = 37/37、T-11、moderation 优先级（:78）

已就绪车辆（本 direction 只接线）：
  A3 drill bin        真 relay + StubSink + 行状态断言（status 2 == N + event_id parity）——0239 门控
  fan-out 套件        im-core relay_tests 的 fresh-DB 执行家（:274）——BATCH_SERIAL + --test-threads=1
  B5-1 条目           audit_governance:: + moderation_finalize_outbox_parity——0239 门控 + empty-filter guard
  sibling B5-2        network relay-probe + gate relay-mock（DB-free 状态机探测）——未落地
  sibling B5-4        audit-provision-check + gate audit（配给预检）——未落地

对象状态机事实（D1-D4 的载体，全部实读）：
  dispatch_batch → claim_due（attempts += 1）→ 逐 claim deliver（token 按 claim 获取）→ settle/requeue/mark_dead
  claim_due ORDER BY available_at, created_at, event_id（无 priority——B5-3 交付面）
  requeue：status 0、available_at = clock_timestamp() + backoff(attempts)、last_error 记录、token/lease 清空
```

## 3. API 变更（逐文件、逐符号）

### 3.1 `crates/aero-eng/src/run.rs` — 新增 `test_integration()`（R1，2 行函数）

```rust
/// Run the integration harness (`scripts/test-integration.sh`) with a default
/// 30-minute timeout (mirrors `cargo_test_lib`; the harness owns fresh-DB
/// migration regressions, drill suites, and the full ignored workspace suite).
pub async fn test_integration() -> Outcome {
    run_cmd("bash", &["scripts/test-integration.sh"], Duration::from_secs(1800)).await
}
```

复用既有 `run_cmd`；非零退出拍平为 `Outcome::error`（exit 1，detail 带脚本 stderr）——与全部 `Gate_` 臂一致的门语义。**零新依赖**。

### 3.2 `crates/aero-cli/src/main.rs` — 命令面（R1/R2/R7）

| 位置 | 变更 |
|---|---|
| `a!()` 注册（:25 后） | `a!(Integration_);` |
| `Test_` 之后新增 | `c!(Integration_, "integration", "Run the integration harness (scripts/test-integration.sh)", \|_ctx, _args\| { aero_eng::run::test_integration().await });` |
| `Gate_` match（"all" 臂前） | `"b5" => b(f("test-integration.sh"), 1800).await,` |
| `gate list` 串（:113） | `"filesize truth web deps complexity filesize-native deps-native workspace-members todos metadata readme b5 all"` |
| `Completion_` cmds 串（:307） | `"check gate test integration skill doctor network completion help"` |
| `Dashboard_`（:596-598） | `Tests (run 'aero-eng test' to refresh)` / `Run: aero-eng test --workspace --lib` / `Integration: aero-eng integration`（三行同款二进制名修正；:597 原文 "aero-cli test" 也是错名——R7 顺带修） |

**不并入 `gate all`**（integration harness 重：PG+NATS+Redis+全 workspace 编译；与 sibling B5-2/B5-4 的 B5 门决策一致）。

### 3.3 `scripts/test-integration.sh` — B5 验收段（R3-R6，替换 :265-269 占位）

**(a) `B5_CONTRACT_TEST_LIST`**（bash 数组，**恰好 37 槽**，条目格式 `NAME` / `NAME[PROPOSED]`，regex `^[A-Za-z0-9_:]+(\[PROPOSED\])?$`）。初始组成（集成时与 sibling + 仓外契约对账，D6/P3）：

```
执行型 15 槽（= 本切片 + sibling 已接线项，每个都有判词行/empty-filter guard 背书）：
  rolling_upgrade_fences_are_atomic_before_0176_reasserts_them      # run_migration_regression
  migration_0192_repairs_attempted_cross_room_scheduled_replies
  migration_0228_backfills_workspace_bot_membership
  migration_0233_backfills_and_constrains_human_identity_issuer
  migration_0237_backfills_before_installing_destination_guard
  message_quota_and_snaplink_outboxes_are_transactional             # run_migrated_integration
  scim_inactive_first_nil_workspace_member_rolls_back_owner_bootstrap
  audit_governance::                                                # 0239 门控（B5-1）
  moderation_finalize_outbox_parity                                 # 0239 门控（B5-1）
  a3-relay-drill                                                    # 0239 门控
  t11-fail-closed                                                   # 0239 门控（本切片）
  moderation-priority-drill                                         # 0239+priority/class 列门控（本切片）
  notification-fanout                                               # fresh 模式恒跑
  relay-mock-probe                                                  # sibling B5-2 存在性门控
  audit-provision-check                                             # sibling B5-4 存在性门控
占位 22 槽：contract-test-01[PROPOSED] … contract-test-22[PROPOSED]（仓外契约名，落仓后逐字替换）
```

**(b) 判词行**（每执行段成功/显式 SKIP 时回声，pin guard 的映射证据）：
- `run_migration_regression` / `run_migrated_integration` 各调用点成功后加 `echo "B5-CHECK <filter>: PASS"`（helper 内加 `echo "B5-CHECK ${2}: PASS"` 更省——选 helper 内，单一触点）；
- A3 段成功加 `B5-CHECK a3-relay-drill: PASS`；0239 缺门 SKIP 时加 `B5-CHECK a3-relay-drill: SKIP (0239 not landed)`；
- fan-out 段成功加 `B5-CHECK notification-fanout: PASS`（其成功行 `✓ Notification fan-out suite passed and database dropped` 已存在，判词行并列）。

**(c) T-11 段**（0239 文件门控，A3 段同构）：

```bash
if [ -f "migrations/0239_audit_governance_outbox.sql" ]; then
    create_throwaway_database "$T11_DRILL_DB"                       # aero_t11_drill_$$（+assert_disposable_db_name）
    T11_URL="${BASE_URL}/${T11_DRILL_DB}"
    DATABASE_URL="$T11_URL" AERO__DATABASE__URL="$T11_URL" cargo run --bin aero-cli -- migrate 2>&1 | tail -1
    DATABASE_URL="$T11_URL" cargo run --quiet -p aero-audit-connector --bin aero-audit-t11-drill
    drop_created_database "$T11_DRILL_DB"
    echo "B5-CHECK t11-fail-closed: PASS"
else
    echo "B5-CHECK t11-fail-closed: SKIP (0239 not landed)"
fi
```

**(d) moderation-priority 段**：同上，bin = `aero-audit-priority-drill`，判词 `B5-CHECK moderation-priority-drill: PASS|SKIP (0239 not landed)`。

**(e) relay 覆盖断言**（R6，B5 段尾部、pin guard 前）：

```bash
relay_legs=0
grep -q "B5-CHECK notification-fanout: PASS" <<<"$B5_LOG" && relay_legs=$((relay_legs+1))
grep -q "B5-CHECK a3-relay-drill: PASS"       <<<"$B5_LOG" && relay_legs=$((relay_legs+1))
if [ -f "crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs" ]; then
    cargo run --quiet -p aero-cli -- network relay-probe            # sibling B5-2 命令
    # 9 个 probe: <name>: PASS 具名场景行（403/422/409/receipt/500/timeout/lease/fencing）
    [ "$(grep -c '^probe: .*: PASS$' <<<"$PROBE_LOG")" -eq 9 ] && relay_legs=$((relay_legs+1))
    echo "B5-CHECK relay-mock-probe: PASS"
else
    echo "B5-CHECK relay-mock-probe: SKIP (B5-2 relay probe not landed)"
fi
[ "$relay_legs" -ge 1 ] || { echo "✗ B5 relay coverage: zero legs executed (wholesale skip)" >&2; exit 1; }
```

（probe 输出经 `set +e` 捕获；失败 → `B5-CHECK relay-mock-probe: FAIL` + 非零。）

**(f) `assert_b5_contract_pin`**（无条件，脚本末尾；判词证据段在 `SKIP_DB_CREATE` 下降级，D5/F13）：

```bash
assert_b5_contract_pin() {
    local count=${#B5_CONTRACT_TEST_LIST[@]}
    [ "$count" -eq 37 ] || { echo "✗ B5 contract pin: 37/$count" >&2; return 1; }
    # 格式 + 去重（关联数组）
    # 执行型条目 ≥ 1（静态清单检查：全 [PROPOSED] = FAIL）
    # 非 SKIP_DB_CREATE：每个非 [PROPOSED] 条目必须命中一行 "B5-CHECK <name>: PASS|SKIP"（$B5_LOG）
    echo "B5 contract pin: 37/37 ($executed executed, $proposed [PROPOSED]): PASS"
}
```

### 3.4 新 bin `crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs`（R4，A3 drill 同构）

**CLI 契约**：无参数；env `DATABASE_URL` 必填、`AERO_AUDIT_DRILL_ROWS`（默认 3）；退出码 `0`=断言全过、`1`=任一断言失败（anyhow bail）、`2`=0239 表缺 SKIP。

**装配**（D1 可达断言；无 StubSink——token endpoint 就是关闭端口，events 端点永不触达）：

```rust
// 确定性关闭端口：bind 127.0.0.1:0 → 取 port → drop listener（连接必被拒，无 wall-clock 窗口）
let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
let port = closed.local_addr()?.port();
drop(closed);
let token_endpoint = Url::parse(&format!("http://127.0.0.1:{port}/token"))?;
// RelayConfig 全 pub 字段直构（其余同 A3 drill：events_url 任意可解析 URL——永不触达；
// request_timeout 5s / delivery_lease 30s / batch_size 100 / concurrency 4）
// seed N 行：INSERT (event_id, payload, available_at, attempts, status) VALUES (…, clock_timestamp(), 0, 0)
```

**断言序列**（每轮后 SQL 计数；轮 2 前 `sleep 1.2s` > backoff(1)=1s，同机 PG 时钟保证可再 claim）：

| 轮 | 断言 | 证明 |
|---|---|---|
| 轮 1 `dispatch_batch`（claimed == N）后 | `COUNT(status=0) == N`；`COUNT(status IN (1,2,3)) == 0`；**`SUM(attempts) == N`**；`COUNT(last_error LIKE '%audit connector HTTP transport failed%') == N` | 行重泊 Ready；零 claimed-at-rest/零 delivered/零 dead（**永不静默成功、永不误判 dead**）；relay 真跑过（非 vacuous）；token endpoint 真被尝试 |
| 轮 2（sleep 1.2s 后）后 | 同上 + `SUM(attempts) == 2N` | retry-forever 姿势：退避后重新可 claim、重泊，仍零终态 |

具名 PASS 行：`drill: t11-pending: PASS`（轮 2 后）。

### 3.5 新 bin `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（R5，A3 drill 同构）

**CLI 契约**：无参数；`DATABASE_URL` 必填；退出码 0/1/2（2 = 0239 表缺 **或 `priority`/`class` 列缺**——运行期能力门，D3）。

**装配**：StubSink（202 + 合法回执，同 A3）；`RelayConfig { batch_size: 100, concurrency: 1, ..A3 同款 }`（concurrency=1 ⇒ settle 序 == claim 序，D4）。

**Seed**（D2 常量 + 顺序证据）：

```rust
const BACKLOG_PRIORITY: i64 = 100;      // 默认积压道（ai_job 先例；B5-3 DESC 语义下"低优先"）
const MODERATION_PRIORITY: i64 = 200;   // moderation 道，严格高于积压道（DESC 下必先行）
                                        // [PROPOSED] 契约值：与 B5-3 claim 测试共享，改判只动此常量
// 1) 500 积压行：INSERT (event_id, payload, available_at, status, priority) VALUES (…, clock_timestamp(), 0, 100)
//    每行 payload 带 source_system（client.rs validate_delivery_payload 门）——先入，available_at 更早
// 2) 1 moderation 行：class='admin'、priority=200、payload 含 action='admin.content.flag'（单常量，
//    [PROPOSED] 二选一）——后入，available_at 更晚 ⇒ 顺序证据只能来自 priority
```

**断言序列**（MAX_ROUNDS=10 覆盖 6 轮 drain）：

| 步骤 | 断言 | 证明 |
|---|---|---|
| 轮 1 后 | `COUNT(status=2) == 100`；moderation 行 ∈ status-2 集 且 `delivered_at == (SELECT MIN(delivered_at) FROM … WHERE status=2)` | **第一轮 claimed 集含 moderation 行且最先投递**（priority DESC 压过 500 积压的 FIFO/available_at 序，与入队顺序无关） |
| 全 drain 后 | `COUNT(status=2) == 501`；event_id set-parity（delivered 集 == seeded 集）；moderation `delivered_at < MIN(backlog delivered_at)` | 501 全投递、无孤儿无重复、moderation 严格最先 |

具名 PASS 行：`drill: moderation-first: PASS`（轮 1）/ `drill: drain-501: PASS` / `drill: parity-501: PASS`。

## 4. 兼容性约束

| # | 约束 | 说明 |
|---|---|---|
| C1 | **零新依赖** | aero-cli（aero-eng/tokio/serde_json/async-trait）与 aero-eng `Cargo.toml` 零改动；connector `Cargo.toml` 零改动（src/bin 自动发现，deps 全已有） |
| C2 | **零生产代码改动** | connector `src/` 一行不动（状态机/客户端/config/pg/stub 全部保持）；migrations/、aero-storage、aero-server、aero-bus、aero-ai 不碰；A3 drill bin、probe bin（sibling）、`audit-provision-check` 命名不碰 |
| C3 | **退出码契约** | drill bin：0=过 / 1=败 / 2=SKIP（0239 或列缺）；harness 段以 0239 文件门控 + `set -e`，SKIP 只经判词行表达；`aero-eng integration` / `gate b5`：`run_cmd` 拍平 → 0=全绿 / 1=任何失败（stderr 带脚本输出） |
| C4 | **CWD = repo 根** | `run::test_integration()` 无 ctx（镜像 `cargo_test_lib`），`scripts/test-integration.sh` 相对路径 + 脚本内部相对引用 `scripts/test-notification-fanout.sh` ⇒ 与既有 `gate` 臂同约束（从 repo 根运行）；env 旋钮透传（`DATABASE_URL`/`SKIP_DB_CREATE`/`AERO__NATS__URL`/`REDIS_URL` 等） |
| C5 | **命令面互斥** | 本切片独占 `integration`（顶层）+ `gate b5`；sibling B5-2 独占 `network relay-probe` + `gate relay-mock`；B5-4 独占 `audit-provision-check` + `gate audit`——共享字符串（`gate list`、`Completion_` cmds、`Gate_` match）append-only 合并，无覆盖 |
| C6 | **单一时钟域** | drill 断言全部走 PG `clock_timestamp()`（行状态计数）；轮 2 的 sleep 只做保守上界（1.2s > backoff(1)=1s），不参与断言算术 |
| C7 | **`SKIP_DB_CREATE` 模式** | 既有 dev 卫生模式（复用主库、跳过 fresh-DB 段）：pin guard 的判词证据检查与 relay ≥1 腿断言降级（见 F13），count/格式/去重仍无条件强制 |

## 5. 失败模式

| # | 失败模式 | 行为 | 处置 |
|---|---|---|---|
| F1 | 0239 未落地（phase-1 窗口） | T-11/priority/A3 段文件门 SKIP；drill exit 2 不触达 harness（段不运行它） | 判词 `SKIP (0239 not landed)` 算已处理 → pin 绿；fan-out + 迁移回归恒跑 → relay ≥1 腿成立 |
| F2 | 0239 落地但 B5-3 排序未落地 | priority drill FAIL（moderation 非首轮） | **诚实的 G6 红**；排期要求 B5-3 先于集成绿；不是 SKIP——排序是 G6 必要条件 |
| F3 | T-11 "零 claim" 不可达（D1） | 若按 requirement 原文断言 `SUM(attempts)==0` → 必然红 | 设计已改断言为可达不变量（attempts==N 每轮、零终态、last_error 证据）——**这是 requirement 的修正，不是弱化**（"never dead/never delivered" 全保留） |
| F4 | priority seed 0/100 与 DESC 冲突（D2） | 若按原文 0/100 → 积压先行 → drill 必败 | 常量改 100/200，注释标注 [PROPOSED]；B5-3 改判只动 drill 常量 |
| F5 | B5-1 DDL 形状漂移（列缺/CHECK 约束拒 200） | INSERT 报错 → drill exit 1（anyhow 上下文） | 运行期能力门（`priority`/`class` 列缺 → exit 2 SKIP）兜底列缺；CHECK 冲突在集成时对账常量（P2） |
| F6 | 关闭端口竞态（drop 后被他进程占用） | token 请求打到别的服务（非 200 → bail Transient → requeue，断言仍过；若恰好返回合法 token → 行投递 → 断言红） | 概率可忽略（127.0.0.1 随机高端口）；红 = 大声失败，无静默 |
| F7 | 判词缺失（段被跳过/改名） | pin guard 找不到 `B5-CHECK <name>:` 行 → FAIL | 防 vacuous 的核心机制；段改名须同步判词（真名以判词为锚） |
| F8 | harness 超时（1800s） | `run_cmd` timeout → `Outcome::error` | 门红带 "timed out after 1800s"；无挂死 |
| F9 | sibling 合并冲突（gate list/completion/match/test-integration.sh） | 三切片同时 append | 协议：append-only + 命令名互斥（C5）；集成时手接共享文件（AGENTS.md §4.1） |
| F10 | 37 计数漂移（sibling 增条目/契约落仓替换占位） | guard count≠37 → FAIL | 集成时对账清单组成（P3）；guard 自动兜底 |
| F11 | `run_cmd` 拍平 drill SKIP 码 | drill exit 2 若意外触达 `run_cmd` → exit 1 | 不触达：harness 段文件门控 drill 运行；文件在而表缺 = 迁移 bug，红正确 |
| F12 | `gate b5` 与 `integration` 行为分叉 | 同脚本同超时 → 不可能分叉（单一事实源） | 无 |
| F13 | `SKIP_DB_CREATE` dev 模式 | fresh 段全跳 → 判词证据缺失 + relay 腿可能 0 | pin 判词检查与 ≥1 腿断言在该模式下降级（记录 `B5 pin: verdict evidence degraded (SKIP_DB_CREATE)`，仍非 vacuous——count/格式/去重强制）；CI 恒用 fresh 模式全量强制 |

## 6. 迁移/落地步骤（Sequencing）

1. **run.rs wrapper + Integration_**（R1）：`test_integration()` + `a!(Integration_)` + `Completion_` 串。独立可验：`cargo run -p aero-cli -- integration`（本地 PG/NATS/Redis 就绪时）。
2. **gate b5 臂**（R2）：`Gate_` match + `gate list` 串。可验：`aero-eng gate b5` 与 `integration` 同脚本同退出。
3. **37/37 钉入**（R3）：`B5_CONTRACT_TEST_LIST` + 判词行 + `assert_b5_contract_pin` 替换 :265-269 占位。无 DB 可验（语法 + `SKIP_DB_CREATE=1` 干跑）：输出 `B5 contract pin: 37/37 …: PASS`。
4. **T-11 drill**（R4）：`aero-audit-t11-drill.rs` + harness 段。
5. **priority drill**（R5）：`aero-audit-priority-drill.rs` + harness 段。**依赖：B5-3 排序落地前该段保持红（F2）——批次集成顺序 = B5-1 → B5-3 → B5-2/B5-4 sibling → 本段转绿。**
6. **relay 覆盖断言 + dashboard 文本**（R6/R7）。
7. **门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规；两 drill bin 各 ~150-180 行、run.rs +2 行、main.rs 文本/注册行——体量安全）· `cargo test -p aero-audit-connector --all-targets`（24/24 仍绿——新 bin 不碰状态机）· no-touch 守卫（`git diff --stat` 不含 migrations/、aero-storage、aero-server 生产代码、connector `src/`、A3 drill bin、probe bin）。
8. **活验证**：全新一次性库跑 `bash scripts/test-integration.sh` 全链（fresh 模式）；`aero-eng gate b5` 与 `aero-eng integration` 各实跑一遍；人为破坏一处断言（如把 `assert_b5_contract_pin` 的 37 改 36）→ 两命令均非零。

## 7. 可测验收映射（Testable acceptance mapping）

> 机器断言面 = 判词行/具名 PASS 行（harness 内 grep）+ drill 退出码 + 命令退出码。每行给正例与反例。

| Acceptance（req §5） | 设计元素 | 正例（PASS 判据） | 反例（负向测试） |
|---|---|---|---|
| 顶层：`integration`/`gate b5` exit 0 ⇔ (a)-(d) 全成立 | 3.1/3.2：同脚本 + `run_cmd` | 本地环境 `aero-eng integration; echo $?` == 0 且 `aero-eng gate b5; echo $?` == 0 | 破坏 priority drill 的 moderation-first 断言 → 两命令均非零（stderr 含 drill FAIL 行） |
| AC1：37/37 具名清单，每非 [PROPOSED] 条目映射到已执行检查 | 3.3(a)(b)(f)：`B5_CONTRACT_TEST_LIST` + 判词行 + guard | 输出含 `B5 contract pin: 37/37 (15 executed, 22 [PROPOSED]): PASS`；`grep -c '^B5-CHECK .*: PASS$\|^B5-CHECK .*: SKIP'` == 15 | 槽数改 36/38 → `B5 contract pin: 37/N` + 非零；删一个判词行 → FAIL（F7）；全 [PROPOSED] 清单 → FAIL；重复名 → FAIL |
| AC2：T-11 fail-closed（relay 缺席 → 行 pending、grant 拒绝） | 3.4 drill + 3.3(c) 段 | 0239 落地：drill exit 0 + `drill: t11-pending: PASS` + harness `B5-CHECK t11-fail-closed: PASS`；未落地：`SKIP (0239 not landed)` + pin 绿 | 断言任一改弱（如漏查 status=3）→ 人为 seed 一行 status=3 → drill FAIL；token endpoint 换可达 stub → 行投递 → `SUM(attempts)!=N`/status 非 0 → FAIL（证明断言真在抓） |
| AC2 配给腿 | 3.3(c) 后（B5-4 seam 存在性门控） | `aero-eng audit-provision-check`（或 aero-server bin aero-cli 同名命令，按 sibling 落家）对同 DB **非零退出** + `B5-CHECK audit-provision-check: PASS`；seam 未落地 → `SKIP (B5-4 audit-provision-check not landed)` | 检测用 `--help` 输出含命令名（防"unknown command 也非零"误判为 PASS） |
| AC3：moderation-priority（500 积压 + 1 moderation → 先达 sink） | 3.5 drill + 3.3(d) 段 | 0239+列落地：exit 0 + `drill: moderation-first: PASS` / `drill: drain-501: PASS` / `drill: parity-501: PASS` + `B5-CHECK moderation-priority-drill: PASS` | seed 顺序反转（moderation 先入）→ 断言仍过（顺序证据只能来自 priority——反例证明断言非 FIFO 依赖）；batch_size 改 501 → `COUNT(status=2)==100` 首轮断言 FAIL |
| AC4：relay_tests 不再 wholesale skip（三腿 ≥1 实跑） | 3.3(e)：fan-out/A3/probe 腿 | fresh 模式：fan-out PASS（恒）→ `relay_legs ≥ 1`；0239 落地后 A3 也 PASS；probe 落地后三腿全 PASS | 三条腿全 SKIP（构造 = SKIP_DB_CREATE + probe 未落地）→ `✗ B5 relay coverage: zero legs executed` + 非零 |
| 状态机语义冗余防线（不动） | connector `tests/{state_machine,claim_validation}.rs`（24/24 既有） | `cargo test -p aero-audit-connector --all-targets` 全绿 | 不改状态机 → 无新测试需求；drill 是黑盒镜像，不重复单测面 |

## 8. Risks / [PROPOSED] / 决策点

- **P1（决策）T-11 断言修正**：requirement 的 `SUM(attempts)==0`（零 claim）经公共面不可达（D1：claim 先行是 B5-2 已实现状态机的事实）。本设计改为"每轮恰一次 claim + 重泊、零终态、last_error 证据"——T-11 核心（pending 不静默成功、不误判 dead）原样保留；`SUM(attempts)==N` 反而把"relay 真跑了"变成非 vacuous 证据。若未来想实现字面"零 claim"，须改 relay 为 fail-before-claim（token 门先行）——那是 B5-2 切片的生产语义变更，本切片不背（R8 零生产改动优先）。
- **P2（决策）priority 常量 100/200**：requirement R5 的 moderation=0 与 B5-3 钉死的 `ORDER BY priority DESC`（higher=more urgent）冲突（D2）。裁定：以 B5-3 语义为准（它拥有 claim 查询），drill 钉 `BACKLOG=100` / `MODERATION=200`（严格高于积压道，满足 moderation-governance R3 "lane must sort ahead of backlog"）。绝对值是 [PROPOSED] 契约值——B5-3 实现若钉不同 lane 值，集成时只改 drill 两个常量 + 注释；aero-ai 映射切片须在 R1 映射里钉同一 lane 值（sibling 测试背书）。
- **P3（决策）37 槽所有权**：B5-4 sibling 也声明"占位变清单"（其 E9/R5）。裁定：guard + 骨架归本切片（总装门）；sibling 只贡献条目（`relay-mock-probe` / `audit-provision-check`）。初始 15 执行 + 22 [PROPOSED] = 37；仓外契约落仓后**仅替换占位为逐字契约名**（不臆造，proposal :13），guard 的判词证据要求自动接管新条目。最终组成在集成对账（F10 兜底）。
- **P4（风险）priority drill 在 0239 落地、B5-3 未落地窗口红**：这是设计意图（F2）——"moderation 优先级"是 G6 必要条件，半落地状态就该红；与 A3/T-11 的 SKIP 窗口（依赖纯缺席）不同。排期 §6 步骤 5 已注明。
- **P5（风险）`gate deps-native` 存量红**（aero-cli unknown crate）= sibling B5-2 的修复面（其 §R4）；本切片零新依赖、不碰 checks.rs，不引入新红。
- **P6（风险）drill 对 0239 DDL 形状的隐式依赖**：seed INSERT 只写 proposal 承诺的列（event_id/payload/status/available_at/attempts/priority/class）；列缺 → 能力门 SKIP（F5），CHECK 拒值 → 集成对账（P2）。
- **行号漂移纪律**：本设计全部以文件/符号为锚（AGENTS.md §0）；requirement 引用的 test-integration.sh:204 已确证漂移到 :321-322。

## 9. 与 sibling 的共享触点协调（AGENTS.md §4.1）

| 共享文件 | 本切片 | sibling B5-2 | sibling B5-4 | 合并协议 |
|---|---|---|---|---|
| `main.rs` `Gate_` match + `gate list` 串 | `"b5"` 臂 + ` b5` | `"relay-mock"` 臂 + ` relay-mock` | `"audit"` 臂 + ` audit` | append-only 臂与串；命令名互斥（C5） |
| `main.rs` `Completion_` cmds 串 | `+ integration` | —（network 已列） | `+ audit-provision-check`（按 sibling 落家裁定） | append-only |
| `main.rs` `a!()` 注册 | `+ Integration_` | —（Network_ 子命令） | `+ AuditProvisionCheck_` | 各加各的 |
| `test-integration.sh` B5 段 | pin guard + 清单骨架 + T-11/priority 段 + relay 断言 | relay-mock 条目 + probe 存在性门控 | audit-provision-check 条目 | 判词行协议（`B5-CHECK <name>: PASS|SKIP`）是共享契约；集成时拉新文件 + 手接 |
| connector `src/bin/` | `aero-audit-t11-drill.rs` + `aero-audit-priority-drill.rs`（新） | `aero-audit-relay-probe.rs`（新） | — | 文件名互斥，自动发现无注册冲突 |
