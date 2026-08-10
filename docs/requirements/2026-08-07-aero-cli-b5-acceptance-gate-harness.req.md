# Requirements Spec — B5 acceptance gate harness：`aero-eng integration` + `gate b5`（37/37 钉入 + T-11 fail-closed + moderation 优先级 drill，全走 `scripts/test-integration.sh`）

- **Module (analysis root)**: `crates/aero-cli` — aero-eng 工程 CLI（`[[bin]] name="aero-eng"`，无 DB 依赖设计）；本 direction 交付物 = `integration` 命令 + `gate b5` 臂 + test-integration.sh 内 B5 验收段（37/37 具名清单钉入、T-11 drill、moderation-priority drill、relay 覆盖断言）
- **Direction**: "B5 acceptance gate harness: add `aero-eng integration` + `gate b5` that runs the pinned 37/37 list, T-11 fail-closed case, and moderation-priority drill through scripts/test-integration.sh"（value 9 / risk_reduction 9 / effort 4 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-cli-src-864950e4.json`（direction #1）
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml`）；contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（:15 "37/37 需先把契约测试清单钉入 test-integration.sh"）；gate anchor `docs/campaigns/implementation-gate.md`（:63-66 aero-im 行 1-4 + :78 G6 = "37/37、T-11、moderation 优先级"）
- **Sibling specs（同模块并行切片，命令面互斥）**: `docs/requirements/2026-08-07-aero-cli-b5-2-relay-probe.req.md`（`network relay-probe` + `gate relay-mock` = 状态机黑盒探测）；`docs/requirements/2026-08-07-aero-cli-b5-4-audit-provision-check.req.md`（`audit-provision-check` + `gate audit` = 配给预检）；本 direction = 两者的**总装门**（G6），只接线不重复实现
- **Status**: Requirements（下述证据全部经源码 grep 核对）
- **Verification date**: 2026-08-07。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-cli/src/main.rs` — CommandRegistry 注册；`Gate_` 壳到 scripts/；`Test_` 仅 cargo_test_lib；`Dashboard_` 广告不存在的 'aero-cli integration' | ✅ **Verified**。注册表 `a!()` :23-31（Check_ :23 / Gate_ :24 / Test_ :25 / Skill_ / Doctor_ / Completion_ / Network_ / Bench_ :30 / Dashboard_ :31——**无 integration**）。`c!(Test_, "test", …)` :101-102 = 仅 `aero_eng::run::cargo_test_lib().await`。`c!(Gate_, "gate", …)` :104；`b()` helper :105-107 = `run_cmd("bash", &[&p], Duration)` 跑 `scripts/*.sh`；`gate list` 串 :113 = "filesize truth web deps complexity filesize-native deps-native workspace-members todos metadata readme all"。`Dashboard_` :543，:597-598 文本 "Run: aero-cli test --workspace --lib" + **"Integration: aero-cli integration"**——命令不存在（registry 无 integration），且二进制名是 `aero-eng`（`crates/aero-cli/Cargo.toml` `[[bin]] name="aero-eng"`），文本两处都错。`Completion_` cmds 串 :307 = "check gate test skill doctor network completion help"。deps 仅 aero-eng/tokio/serde_json/async-trait——**零 DB 依赖** |
| E2 | `crates/aero-eng/src/run.rs` — cargo_check/cargo_test_lib/cargo_clippy，无 integration wrapper | ✅ **Verified**。`run_cmd` :15-41（超时 + 输出捕获；**非零退出拍平为 exit-1 error Outcome**，exit_code 进 detail）；`cargo_check` :44、`cargo_test_lib` :54（= 引用的 :54 精确命中）、`cargo_clippy` :63。无任何 test-integration.sh wrapper。`Outcome`（outcome.rs）：`ok/warning(code,msg)/error/skip` + `merge` + `exit_code` |
| E3 | `scripts/test-integration.sh:204` — relay_tests 被 skip；'its relay loops claim global state' | ⚠️ **行号漂移，事实核验 + 一处勘误**。`--skip db_tests::relay_tests` 现位于 **:322**（:321 是 `--skip db_tests::notifications_tests`；注释 :317-320："db_tests:: (aero-im-core) is excluded because it runs on its own fresh throwaway DB in the notification fan-out step above (its relay loops claim global state and need serial, isolated execution)"——文件自分析后增长，skip 是**主库运行卫生**而非唯一处理）。**勘误**：im-core `relay_tests`（`crates/aero-im-core/src/db_tests.rs:33` `mod relay_tests;`，= AT-6a/6b/7 通知 relay，全局 claimer + `BATCH_SERIAL` :144 + `--test-threads=1`）**今天已在 harness 中执行**——`:274` `bash scripts/test-notification-fanout.sh` 在全新 throwaway DB 上跑 `cargo test -p aero-im-core --lib --locked db_tests:: -- --ignored --test-threads=1`。**真正无执行家的 = 审计 relay（B5-2 connector）语义**：A3 drill（:238-263）以 `migrations/0239_audit_governance_outbox.sql` 存在为门，而该迁移**未落地**（migrations 尾号 0238）→ 当前恒 SKIP。另 :265-269 "A3 part 1 (pin)" 占位注释承诺钉 37/37 清单 |
| E4 | `docs/proposals/audit-contract-batch-aero-im.md:15` — 37/37 钉入先决 | ✅ **Verified（:15 精确命中）**。"门禁核对：T-11 与 moderation drill 本仓库可全绿；37/37 需先把契约测试清单钉入 `test-integration.sh`；B5 起始条件 = B1（sink G2）+ B4-2（scope registry）就绪，本地骨架可用 mock 先行"。:13 "8 处不可验证/[PROPOSED] 明确列出——…'37/37' 测试清单…全部不可独立核对"——**37 个契约名在仓外，不臆造** |
| E5 | `docs/campaigns/implementation-gate.md` G6 行（B5-1..4 = 37/37 + T-11 + moderation 优先级） | ✅ **Verified**。:63 aero-im 行 1（"30 个忽略测试 CI 全绿（37/37）；P2 parity"）；:64 行 2（"T-11（无 relay 配给被拒）"）；:65 行 3（"注入积压 drill：moderation 先达 sink"）；:66 行 4（"矩阵配给端到端 403-free；无 relay 时 fail-closed 拒绝"）；**:78 G6（B5）| B5-1..4 | 37/37、T-11、moderation 优先级** |

### 1.1 补充证据（方向外事实，决定设计形态）

| # | Supplementary evidence | Verification result |
|---|---|---|
| E6 | **B5-1（0239）未落地；test-integration.sh 已有 0239 文件存在性门控先例** | ✅ **Verified**。`ls migrations/ | tail` = 0238 止，0239 不存在（工作树亦无）。:226-238 B5-1 条目（`audit_governance::` + `moderation_finalize_outbox_parity`）与 :238-263 A3 drill 均 `if [ -f "migrations/0239_audit_governance_outbox.sql" ]` 门控，缺表显式 SKIP（"stays green during the phase-1 window"）。`run_migrated_integration` :160-189 带 **empty-filter guard**（B5-1 A1.3）：`grep -Eq 'test result: ok\. [1-9][0-9]* passed'`，命名条目 vacuous green = fail |
| E7 | **connector crate（工作树，未提交）已提供 A3 drill 车辆 + mock sink** | ✅ **Verified**。`crates/aero-audit-connector/src/bin/aero-audit-relay-drill.rs`：0239 缺表 → exit 2 SKIP；直插 N 行 status-0 进 `audit_governance_outbox`；`StubSink::start()`（loopback 临时端口，token + events 端点）→ `AuditRelay` + `dispatch_batch`（MAX_ROUNDS=10，batch_size=100，concurrency=4）→ 断言 `COUNT(status=2)==N` + event_id set-parity。`stub.rs::StubSink` / `fake.rs::FakeOutbox`（sibling B5-2 E7 已盘点）——**"mock sink 代替全局态 relay loop"的现成车辆** |
| E8 | **B5-3 moderation drill 语义归属 + 夹具规格（仓内锚点）** | ✅ **Verified**。proposal :10："B5-3：`priority` 列 + `claim_due` 按 `priority DESC` 排序；本地 `message.moderated` → 出站 `admin.content.flag`/`admin.moderation.action` 映射表；注入积压 drill（500 积压 + 1 moderation → 先达 sink）+ 反饥饿上限"。`docs/requirements/2026-08-06-aero-ai-moderation-governance-outbox.req.md` :108："First `claim_due` call returns the moderation row (priority-ordered ahead of all 500 backlog rows regardless of enqueue order)"；:48-49 夹具 = `class='admin'` + moderation priority + mapped outbound action + status 0；:53 claim/反饥饿 = **B5-3 (aero-storage)** 交付——本 direction 只消费，不实现。出站 token 清单仓外 [PROPOSED]（:19 已核："admin.content.flag / admin.moderation.action 在代码与迁移中零出现"） |
| E9 | **T-11 语义仓内落点** | ✅ **Verified**。投递侧：connector `deliver_claim` Forbidden→`mark_dead` attempt1（B5-2 已实现，`tests/state_machine.rs::forbidden_dead_on_first_attempt`）。配给侧：B5-4 sibling（aero-auth + aero-server bin aero-cli `audit-provision-check` + 0240 心跳 + `assert_audit_scope_provisioned` 403 fail-closed）——**未落地**；本 direction 的 T-11 drill 以「relay 缺席 → 行保持 status 0 pending + 配给 seam 拒绝（fail-closed）」为仓内可执行读法，配给 leg 以 B5-4 seam 存在为门 |
| E10 | **direction 引用的 :204 行号漂移范围** | ✅ **说明**。`scripts/test-integration.sh` 当前 332 行（工作树含在途 B5 切片）；skip 在 :321-322。本 spec 全部以符号/文件为锚（AGENTS.md §0） |
| E11 | **dashboard 广告文本 vs 真实二进制** | ✅ **Verified**。:598 "Integration: aero-cli integration"——命令不存在 + 二进制名错误（`aero-eng`）；:597 "Run: aero-cli test --workspace --lib" 同款二进制名错误（`aero-eng test` 存在，跑 `cargo test --workspace --lib`，run.rs:54）。修 = 文本对齐真实命令（R8） |

## 2. Verified current state

```
G6 无执行家现状（三条独立事实）：
a) aero-eng（E1/E2）  9 命令；Test_ = 仅 cargo_test_lib；Gate_ 只有静态检查脚本臂；
                      dashboard 广告 'aero-cli integration'（不存在，且二进制名错）
b) harness（E3）      test-integration.sh 主库 run --skip db_tests::relay_tests（:322，卫生）；
                      im-core relay_tests 经 test-notification-fanout.sh（:274）在 fresh DB 已执行；
                      审计 relay（B5-2 connector）语义 = A3 drill（:238-263）→ 0239 未落地 → 恒 SKIP；
                      :265-269 37/37 钉入 = 占位注释（非真实清单）
c) 契约（E4/E5）      37/37 清单仓外 [PROPOSED]（proposal :13）；G6 通过条件 = 37/37、T-11、
                      moderation 优先级（implementation-gate.md :78）；钉入先决（proposal :15）

已就绪的车辆（本 direction 只接线）：
  A3 drill bin（E7）  真 relay + StubSink mock sink + 行状态断言（status 2 == N + parity）——0239 门控
  fan-out 套件（E3）  im-core relay_tests 的 fresh-DB 执行家——已接线 :274
  B5-1 条目（E6）     audit_governance:: + moderation_finalize_outbox_parity——0239 门控，empty-filter guard
  sibling B5-2        network relay-probe + gate relay-mock（mock sink 黑盒状态机，DB-free）——未落地
  sibling B5-4        audit-provision-check + gate audit（配给预检）——未落地

aero-eng 扩展样板（本 direction 复制的两条线，与 sibling 相同）：
  Gate_   → b() = run_cmd("bash", scripts/*.sh) → Outcome（非零拍平 exit 1，门 = pass/fail）
  命令注册 → a!() + c! 宏；Completion_ cmds 串 + gate list 串同步
```

**Gaps this direction closes**（all verified）：① `integration` 命令无家（E1 dashboard 广告了它）——新增 `Integration_` + `run::test_integration()`（E2 的 "no integration wrapper"）；② G6 门无臂——`gate b5`（R3）；③ 37/37 占位非清单——真实 37 槽具名常量 + 钉入 guard（R4，proposal :15 先决）；④ T-11 无 drill——`aero-audit-t11-drill`（R5）；⑤ moderation-priority 无 drill——`aero-audit-priority-drill`（R6）；⑥ relay 覆盖断言——A3/fan-out/relay-mock 三腿接线 + 非 vacuous 证据（R7）；⑦ dashboard/completion/gate-list 文本对齐（R8）。

## 3. Scope

**In scope（B5 acceptance gate harness，effort 4 的完整切片）**：
- `crates/aero-cli/src/main.rs`：`Integration_` 命令（`a!(Integration_)` 注册 + `c!` 定义）；`Gate_` 增 `"b5"` 臂；`gate list` 串 += `b5`；`Completion_` cmds 串 += `integration`；`Dashboard_` :597-598 文本修正（R1/R2/R3/R8）。
- `crates/aero-eng/src/run.rs`：`test_integration()` wrapper（`run_cmd("bash", ["scripts/test-integration.sh"], 1800s)`，镜像 `cargo_test_lib`；E2 缺口的直接补位）。
- `scripts/test-integration.sh`：B5 验收段——`B5_CONTRACT_TEST_LIST`（37 槽具名常量）+ `assert_b5_contract_pin` guard（替换 :265-269 占位）+ T-11 drill 段 + moderation-priority drill 段 + relay 覆盖断言段（R4-R7）。
- `crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs`（新 bin，src/bin/ 自动发现，**零 Cargo.toml 改动**）：T-11 fail-closed drill（R5）。
- `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（新 bin）：moderation-priority drill（R6）。

**Out of scope（并行切片 / 其他模块——勿在本 direction 建造）**：
- **B5-1（0239 DDL/enqueue/repo）**：本 direction 只以迁移文件存在为门消费之；不写迁移、不碰 aero-storage。
- **B5-3（priority 列语义 / `claim_due ORDER BY priority DESC` / 反饥饿上限）**：aero-storage 交付（E8）；本 direction 的 drill 只**断言**其行为。
- **B5-2 connector 状态机 / client / config / pg / 既有 drill bin 任何改动**：A3 drill 保持原样与门控（B5-2 sibling 钉死）；probe 套件（`aero-audit-relay-probe`）是 sibling 交付物，本 direction 只做存在性门控接线。
- **B5-4 `audit-provision-check` / `gate audit` / 0240 / aero-auth 配给门**：sibling 交付物；本 direction 的 T-11 drill 只在 seam 存在时调用其 CLI 断言 fail-closed（E9）。
- **aero-server 生产代码、migrations/、aero-ai、aero-bus**：一律不碰。
- **真实 sink/IdP 联调**（仓外）：本地骨架 mock 先行（proposal :15 "本地骨架可用 mock 先行"）。

## 4. Requirements

### R1 — `aero-eng integration` 命令（Integration_，总装门）

`crates/aero-cli/src/main.rs`：
- `crates/aero-eng/src/run.rs` 增 `pub async fn test_integration() -> Outcome`：`run_cmd("bash", &["scripts/test-integration.sh"], Duration::from_secs(1800)).await`（镜像 `cargo_test_lib` :54 模式；30 分钟覆盖全 harness：迁移回归 + fresh-DB drills + 全 workspace ignored 套件）。
- `main.rs` 注册 `a!(Integration_)`（Test_ 之后）；`c!(Integration_, "integration", "Run the integration harness (scripts/test-integration.sh)", …)` → `aero_eng::run::test_integration().await`。零参数；env 旋钮（`DATABASE_URL`/`SKIP_DB_CREATE`/`AERO__NATS__URL`/`REDIS_URL` 等）透传。
- `Completion_` cmds 串 :307 += `"integration"`。
- 退出码：0 = harness 全绿；非零 = harness 失败（`run_cmd` 拍平 exit 1，门语义 pass/fail——与 `Gate_` 全部臂一致）。

### R2 — `Gate_` 增 `b5` 臂（G6 门）

- `gate b5` = `b(f("test-integration.sh"), 1800).await`（复用现有 `b()` helper :105-107，**不新增脚本**——B5 验收段就在 test-integration.sh 内，单一事实源）。
- `gate list` 串 :113 += `" b5"`。
- **不并入 `gate all`**（integration harness 重：DB + NATS + Redis + 全 workspace 编译；同 sibling B5-2/B5-4 的 B5 门决策）。
- 前置条件（description 注明）：本地 Postgres + NATS + Redis 可达（test-integration.sh 的既有环境要求）。

### R3 — 37/37 契约清单钉入（acceptance a，proposal :15 先决）

`scripts/test-integration.sh` 的 "A3 part 1 (pin)" 占位（:265-269）替换为真实机制：
- **`B5_CONTRACT_TEST_LIST`**：bash 数组，**恰好 37 个具名槽**。条目格式 `NAME`（仓内可执行项，名字 = 执行面标识：cargo-test filter / drill bin / 套件名）或 `NAME[PROPOSED]`（仓外契约项占位，显式标注）。**不臆造契约项**（proposal :13）：仓内可核子集 = 本 direction + sibling 已接线项（如 `audit_governance::`、`moderation_finalize_outbox_parity`、`a3-relay-drill`、`t11-fail-closed`、`moderation-priority-drill`、`relay-mock-probe`、`notification-fanout`、6 个迁移回归条目等）；仓外契约名以 `[PROPOSED]` 占位，契约文本落仓后**仅替换占位为逐字契约名**（seam 就位，B5-4 sibling E9 同款决策）。
- **`assert_b5_contract_pin` guard**（无条件执行，无 DB）：
  1. 槽数 == 37，否则 FAIL（`B5 contract pin: 37/N`）；
  2. 无重复名；条目匹配 `^[A-Za-z0-9_:]+(\[PROPOSED\])?$`；
  3. **每个非 `[PROPOSED]` 条目必须映射到一个已执行检查**——映射表 = 条目名出现在其执行段的 `B5-CHECK <name>: PASS` 回声行（drill/套件段）或 `run_migrated_integration` 命名条目（cargo-test filter，**empty-filter guard** :183 已强制 ≥1 测试实跑）；
  4. 执行条目 ≥1（防 vacuous green——全 [PROPOSED] 清单 = FAIL）；
  5. 收尾行 `B5 contract pin: 37/37 (<E> executed, <P> [PROPOSED]): PASS`。

### R4 — T-11 fail-closed drill（acceptance b）

**新 bin `crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs`**（A3 drill 同构，src/bin/ 自动发现，零 Cargo.toml 改动）：
- 0239 表缺（`to_regclass('audit_governance_outbox')` NULL）→ stderr SKIP + **exit 2**（A3 drill 同款；test-integration.sh 段捕获为显式 SKIP 行，phase-1 窗口保持绿）。
- Seed N 行（默认 3，`AERO_AUDIT_DRILL_ROWS` 可调，同 A3 drill）status-0 直插 `audit_governance_outbox`。
- **relay 缺席构造**：`RelayConfig` 的 `token_endpoint` = 确定性关闭端口（bind `127.0.0.1:0` 取端口后 drop listener——连接必被拒，无 wall-clock 窗口）；token 不可铸 → 零 claim。
- 有界轮次（≤3 round）`dispatch_batch()` 后断言（psql/`sqlx` 计数）：
  - `COUNT(status=0) == N`（行保持 pending）；
  - `COUNT(status IN (1,2,3)) == 0`（零 claimed/delivered/dead——**永不静默成功、永不误判 dead**）；
  - `SUM(attempts) == 0`（从未被 claim）。
- 具名 PASS 行：`drill: t11-pending: PASS`。
- **配给拒绝 leg（harness 侧，B5-4 seam 存在性门控）**：若 aero-cli bin 有 `audit-provision-check` 命令（B5-4 sibling 落地，检测 = `cargo run --bin aero-cli -- help` 输出含该名或文件门），则对同一 DB 运行之并**断言非零退出**（fail-closed：relay 缺席 ⇒ 无 `audit:event:write` 配给）；seam 未落地 → 显式 `SKIP: t11-no-grant (B5-4 audit-provision-check not landed)` 行（非 vacuous skip，同 0239 门先例）。

`scripts/test-integration.sh` 段（0239 文件存在性门控，A3 段 :238-263 同款）：fresh DB → migrate → `DATABASE_URL=… cargo run --quiet -p aero-audit-connector --bin aero-audit-t11-drill` → 成功回声 `B5-CHECK t11-fail-closed: PASS`；缺表回声 `B5-CHECK t11-fail-closed: SKIP (0239 not landed)`。

### R5 — moderation-priority drill（acceptance c）

**新 bin `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`**：
- 0239 缺 → exit 2 SKIP（同 R4）。
- Seed：**500 积压行**（`priority = 100`——ai_job 默认约定，数值是仓内钉死的 drill 常量，注释标注 [PROPOSED] 契约值，B5-3 改判只动常量）+ **1 moderation 行**（`class='admin'`、`priority = 0`（DESC = 高优先行，必须压过 100 且**与入队顺序无关**）、`payload.action = 'admin.content.flag'`——单常量，B5-3 映射表 [PROPOSED] 二选一，按 moderation-governance spec "one constant, not a runtime choice"）。**Seed 顺序：积压先、moderation 后**（moderation 的 `available_at` 更晚——顺序证据只能来自 priority，非 FIFO）。
- `RelayConfig`：`batch_size = 100`（< 501，首批不可能含全部 ⇒ 顺序必须来自 priority）、**`concurrency = 1`**（串行按 claim 序投递 ⇒ `delivered_at` 排序确定性；镜像 BATCH_SERIAL 纪律）。其余同 A3 drill。
- 断言：
  1. **第一轮 `dispatch_batch` 的 claimed 集含 moderation 行**（claim order by priority DESC——E8 :108 "regardless of enqueue order"）；
  2. 全 drain 后 moderation 行 `delivered_at < min(backlog delivered_at)`（**claimed and delivered first**）；
  3. `COUNT(status=2) == 501` + event_id set-parity（无孤儿无重复）。
- 具名 PASS 行：`drill: moderation-first: PASS` / `drill: drain-501: PASS` / `drill: parity-501: PASS`。
- `scripts/test-integration.sh` 段：同 R4 门控与回声（`B5-CHECK moderation-priority: PASS` / `SKIP`）。

### R6 — relay 覆盖断言（acceptance d：relay_tests 不再只有 wholesale skip）

`scripts/test-integration.sh` B5 段新增 relay 覆盖断言（三条腿，**每次调用 ≥1 腿实跑**——防 vacuous green）：
1. **fan-out 套件**（:274，恒跑）：im-core `relay_tests`（AT-6a/6b/7）在 fresh DB + `--test-threads=1` 的执行家；断言其成功行 `✓ Notification fan-out suite passed and database dropped` 出现（E3 勘误：这已是 relay_tests 的执行家，主库 `--skip db_tests::relay_tests` :322 只是卫生）。
2. **A3 drill**（0239 门控，E7）：审计 relay + StubSink mock sink 行状态断言；断言 `✓ Audit connector drill passed (status 2 == N, event_id parity)`。
3. **relay-mock probe 腿**（sibling B5-2 存在性门控）：若 `crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs` 存在 → `aero-eng network relay-probe` 并 grep 9 个 `probe: <name>: PASS` 具名场景行（403/422/409/receipt/500/timeout/lease/fencing——mock sink 代替全局态 relay loop 的状态机覆盖）；bin 未落地 → 显式 SKIP 行。
- 语义钉死：`--skip db_tests::relay_tests` 保留为**已注释卫生**（:317-320 注释指向执行家），B5 段以三条腿的执行证据取代"skipped wholesale"作为覆盖故事。

### R7 — Dashboard_ / 文本对齐

`crates/aero-cli/src/main.rs` `Dashboard_` :597-598 修正为真实命令（R1 落地后命令存在）：
- `Run: aero-eng test --workspace --lib`（原 "aero-cli test"——二进制名错）；
- `Integration: aero-eng integration`（原 "aero-cli integration"——命令不存在 + 二进制名错）。

### R8 — 约束

- **零新依赖**：aero-cli（aero-eng/tokio/serde_json/async-trait）与 aero-eng `Cargo.toml` 均零改动；`test_integration()` 只用既有 `run_cmd`。
- **connector 生产代码零改动**：`src/` 一行不动；只新增 `src/bin/aero-audit-t11-drill.rs` + `src/bin/aero-audit-priority-drill.rs`（自动发现，Cargo.toml 零改动）。
- **零新迁移**；**零新 env**（复用 `AERO_AUDIT_DRILL_ROWS`；priority drill 固定 500+1）。
- **不触碰**：migrations/、aero-storage、aero-server 生产代码、aero-ai、aero-bus、A3 drill bin、probe bin（sibling 交付物）、`audit-provision-check` 命名。
- 共享触点手接（AGENTS.md §4.1）：`Gate_` match、`gate list` 串、`Completion_` cmds 串、test-integration.sh B5 段——与 sibling B5-2/B5-4 命令名互斥（`b5`/`integration` vs `relay-mock`/`audit`），集成时拉新文件 + 手接共享文件。
- 提交前门禁：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规；新代码小体量）· `aero-eng gate b5` + `aero-eng integration` 实跑绿（本地 PG/NATS/Redis 就绪时）。

## 5. Acceptance checks（direction 原样保留，逐条 testable）

> direction acceptance 原文四条 + 顶层退出契约，逐条保留并钉死测试面。机器断言面 = B5 段 guard/回声行（R3-R6）+ drill bin 具名 PASS 行（R4/R5）+ aero-eng 命令退出码（R1/R2）。

### 顶层 — `aero-eng integration`（或 `gate b5`）exit 0 仅当 (a)-(d) 全成立
- `Integration_` = `run::test_integration()` → `scripts/test-integration.sh`（含 B5 段）；`gate b5` = 同脚本。二者任一 exit 0 ⇔ harness 全绿（含 B5 段四条）；任一失败 → 非零 + stderr 具名。
- 测试：`aero-eng integration; echo $?` 与 `aero-eng gate b5; echo $?`（本地环境）均 0；人为破坏一个 drill 断言（如 priority drill 的 moderation-first）→ 两命令均非零。

### AC1 — 37/37 contract test names listed verbatim in scripts/test-integration.sh，each maps to an executed check
**测试 = `B5_CONTRACT_TEST_LIST` + `assert_b5_contract_pin`**（R3）：
- 常量恰好 **37 槽**（guard 断言 `count == 37`，否则 FAIL `B5 contract pin: 37/N`）；无重复；格式 `NAME` / `NAME[PROPOSED]`；
- **每个非 `[PROPOSED]` 条目映射到已执行检查**：cargo-test filter 条目经 `run_migrated_integration` empty-filter guard（:183 `test result: ok. [1-9][0-9]* passed`——命名条目 vacuous green = fail）；drill/套件条目经 `B5-CHECK <name>: PASS` 回声 grep；
- 仓外契约名 [PROPOSED] 显式标注、只计数（proposal :13 不臆造；契约落仓后逐字替换占位，guard 自动接管执行证据要求）；
- 执行条目 ≥1（全 [PROPOSED] = FAIL）；收尾行 `B5 contract pin: 37/37 …: PASS`。
- CI：`gate b5` 输出 grep `B5 contract pin: 37/37` + `: PASS`；计数被改（36/38）→ FAIL。

### AC2 — T-11 case runs：relay/provisioning absent → audit outbox rows stay pending (status 0)，any `audit:event:write` grant refused (fail-closed)
**测试 = `aero-audit-t11-drill` + harness 段**（R4，0239 门控）：
- relay 缺席（token endpoint = 确定性关闭端口）：`COUNT(status=0) == N`、`COUNT(status IN (1,2,3)) == 0`、`SUM(attempts) == 0`——**pending 永不静默成功、永不误判 dead**；
- 配给拒绝 leg（B5-4 seam 存在时）：`aero-cli audit-provision-check` 对同一 DB **非零退出**（fail-closed：无 relay ⇒ 无 grant）；seam 未落地 → 显式 SKIP 行；
- CI：grep `drill: t11-pending: PASS`（+ `B5-CHECK t11-fail-closed: PASS`）；0239 未落地窗口 = 显式 SKIP（非 vacuous，同 A3 门先例，门保持绿）。

### AC3 — moderation-priority drill runs：500 backlog + 1 admin.content.flag/admin.moderation.action row → moderation row claimed and delivered first（claim order by priority DESC）
**测试 = `aero-audit-priority-drill` + harness 段**（R5，0239 门控）：
- Seed 500（priority=100，先入）+ 1（class='admin'，priority=0，`payload.action='admin.content.flag'`，后入——顺序只能来自 priority）；
- `batch_size=100`：**第一轮 claimed 集含 moderation 行**（priority DESC 压过 FIFO）；
- `concurrency=1` 串行 drain：moderation 行 `delivered_at < min(backlog delivered_at)`（**claimed and delivered first**）；
- 终态 `COUNT(status=2)==501` + event_id set-parity；
- CI：grep `drill: moderation-first: PASS` / `drill: drain-501: PASS` / `drill: parity-501: PASS` + `B5-CHECK moderation-priority: PASS`。

### AC4 — relay_tests no longer skipped wholesale（mock sink used instead of global-state relay loop）
**测试 = B5 段 relay 覆盖断言**（R6）：
- 三条腿每次调用 **≥1 实跑**：① fan-out 套件（:274，恒跑；im-core relay_tests 的 fresh-DB 执行家，成功行 `✓ Notification fan-out suite passed`）；② A3 drill（0239 门控；审计 relay + StubSink mock sink，`✓ Audit connector drill passed (status 2 == N, event_id parity)`）；③ relay-mock probe（sibling 存在性门控；9 个 `probe: <name>: PASS` 场景）；
- 主库 `--skip db_tests::relay_tests`（:322）保留为已注释卫生（:317-320 注释指向执行家），不再是覆盖故事；
- CI：三条腿全 SKIP = FAIL（防 vacuous）；任一腿实跑且 PASS = 绿。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| `integration` 命令 + `gate b5` 臂 + 注册/help/completion | `crates/aero-cli/src/main.rs`（Integration_ / Gate_ "b5" / Completion_ 串 / Dashboard_ 文本） | `aero-eng integration` / `aero-eng gate b5` 冒烟 + `aero-eng help` + `cargo test --workspace --lib` |
| integration wrapper | `crates/aero-eng/src/run.rs` `test_integration()` | `cargo test -p aero-eng --lib`（run.rs 既有 run_cmd 单测面） |
| 37/37 钉入 guard | `scripts/test-integration.sh`（`B5_CONTRACT_TEST_LIST` + `assert_b5_contract_pin`） | bash；计数/格式/执行证据断言（R3） |
| T-11 drill（AC2） | `crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs`（新 bin）+ harness 段 | throwaway DB + migrate + bin（exit 0/2）+ `B5-CHECK` 回声 |
| Moderation-priority drill（AC3） | `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（新 bin）+ harness 段 | 同上；具名 PASS 行 grep |
| relay 覆盖（AC4） | test-integration.sh B5 段（fan-out/A3/probe 三腿断言） | 既有 :274 + :238-263 + sibling 门控接线 |
| 状态机语义冗余防线（不动，仅接线） | connector `tests/{state_machine,claim_validation}.rs`（24/24 既有）+ `aero-audit-relay-probe`（sibling） | `cargo test -p aero-audit-connector --all-targets`；probe 存在性门控 |

## 7. Risks / [PROPOSED] / 决策点

- **37/37 清单内容仓外（proposal :13）**：本 direction 钉「37 槽具名常量 + 仓内项可执行 + 仓外项显式 [PROPOSED]」，**不臆造契约项**；契约文本落仓后仅替换占位（seam 已就位，B5-4 sibling E9 同款决策）。"37" 计数本身也是契约来源（[PROPOSED]），钉为常量便于契约落地时核对。
- **priority 数值（100/0）与 outbound token（admin.content.flag）是 [PROPOSED] 契约值**：由 drill 钉为仓内规范性读法（每值有仓内先例：ai_job 默认 100 / DESC 高优先行 / moderation-governance spec "one constant"）；B5-3 改判只动 drill 常量与 seed 注释，结构不动。
- **行号漂移**：direction 引用的 test-integration.sh:204 现为 :321-322（文件自分析后增长）；本 spec 以符号/文件为锚（AGENTS.md §0）。
- **0239 未落地窗口**：T-11 / moderation / A3 段全部显式 SKIP（0239 文件存在性门 + drill exit-2），门保持绿（非 vacuous——guard 恒跑 + fan-out 恒跑）；0239 落地后 empty-filter guard + 具名 PASS grep 自动转 load-bearing。
- **与 sibling 的共享触点**：`Gate_` match / `gate list` 串 / `Completion_` cmds 串 / test-integration.sh B5 段由三个 direction（B5-2 relay-mock、B5-4 audit、本 direction）同时触碰——命令名互斥（`b5`/`integration` vs `relay-mock` vs `audit`），集成时手接共享文件（AGENTS.md §4.1）；probe/audit-provision-check 腿以存在性门控接线，**不依赖 sibling 落地即可绿**。
- **`gate b5` 与 `integration` 等价（同脚本）**：决策 = 单一事实源（B5 段在 test-integration.sh 内），不引入 B5_ONLY 控制流（effort 4 的最小面）；G6 门本来就要求全 harness。
- **`gate deps-native` 存量红**（aero-cli unknown crate）= sibling B5-2 R4 的修复面；本 direction 零新依赖、不触碰 checks.rs，不引入新红。
- **dashboard 文本**：R7 修正两行（:597-598）——"aero-cli" → "aero-eng"，且广告的命令在 R1 落地后真实存在。
- **concurrency=1 的确定性**：priority drill 用串行投递换 `delivered_at` 排序确定性（镜像 BATCH_SERIAL 纪律）；claim 序断言（第一轮含 moderation 行）不依赖 concurrency，双保险。

## 8. Sequencing

1. **aero-eng wrapper + Integration_**：`run::test_integration()`（R1）+ `main.rs` `Integration_` 命令 + Completion_ 串——独立可验：`cargo run -p aero-cli -- integration`（本地环境）。
2. **gate b5 臂**：`Gate_` match + `gate list` 串（R2）。
3. **37/37 钉入**：`B5_CONTRACT_TEST_LIST` + `assert_b5_contract_pin` 替换 :265-269 占位（R3）——无 DB 即可验：`bash scripts/test-integration.sh`（或 gate b5）输出 `B5 contract pin: 37/37 …: PASS`。
4. **T-11 drill**：`aero-audit-t11-drill.rs` + harness 段（R4）。
5. **moderation-priority drill**：`aero-audit-priority-drill.rs` + harness 段（R5）。
6. **relay 覆盖断言 + dashboard 文本**（R6/R7）。
7. **门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· `aero-eng gate b5` + `aero-eng integration` 实跑绿 · no-touch 守卫（`git diff --stat` 不含 migrations/、aero-storage、aero-server 生产代码、connector `src/`、A3 drill bin）。
