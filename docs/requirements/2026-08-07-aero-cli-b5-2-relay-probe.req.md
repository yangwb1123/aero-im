# Requirements Spec — aero-cli B5-2 test vehicle：mock-sink `relay probe` 命令 + `gate relay-mock`（落家 = `crates/aero-cli` aero-eng 二进制）

- **Module (analysis root)**: `crates/aero-cli` — aero-eng 工程 CLI（`[[bin]] name="aero-eng"`，无 DB 依赖设计）；本 direction 交付物 = 黑盒 mock-sink 探测命令 `network relay-probe` + 门 `gate relay-mock` + 探测套件接线
- **Direction**: "B5-2 test vehicle: mock-sink `relay probe` command in aero-cli exercising the aero-id connector semantics locally (lease/backoff/422-409→dead/403→dead)"（value 8 / risk_reduction 8 / effort 7 / confidence 8）
- **Source analysis**: `docs/auto/analyses/crates-aero-cli-e99ec77a.json`（direction #2）
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml`）；contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（B5-2 行 :9、[PROPOSED] 标注 :13）；gate anchor `docs/campaigns/implementation-gate.md`（:64 "T-11（无 relay 配给被拒）"、:78 G6）
- **Sibling spec（同模块并行切片，命令面互斥）**: `docs/requirements/2026-08-07-aero-cli-b5-4-audit-provision-check.req.md`（`audit-provision-check` + `gate audit` = 配给预检；本 direction = 状态机探测，两 seam 互补不重叠）
- **Status**: Requirements（下述证据全部经源码 grep 核对；`gate deps-native` 实跑取证）
- **Verification date**: 2026-08-07。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-cli/src/main.rs` — `Network_` 命令（ping/dns/port TCP 探测）是扩展样板：`network relay-probe <mock-url>` | ✅ **Verified**。`Network_` :327-328（name "network"），子命令 help/ping/dns/port，全部经 `tokio::net::TcpStream::connect` 直连探测（`port` 臂 :392 两段式 `host:port` 解析 + 建连）。`a!()` 注册表 :23-31（9 命令）；`Gate_` :104（`b()` helper :105 = `run_cmd("bash", &[path], timeout)` 跑 `scripts/*.sh`，`gate list` = "filesize truth web deps complexity filesize-native deps-native workspace-members todos metadata readme all"）；`c!` 宏 :48-55；main 退出路径 :32-43（`Err` → `std::process::exit(r.exit_code())`）。`crates/aero-cli/Cargo.toml` deps 仅 aero-eng/tokio/serde_json/async-trait——**零 DB 依赖**（文件头注释 "engineering commands only (no DB dependencies)"），因此探测引擎**不能链接 connector crate**（其依赖 sqlx/reqwest），只能 spawn 二进制 |
| E2 | `crates/aero-server/src/snaplink_commercial/config.rs:247-248` — lease 规则：`delivery_lease <= request_timeout*2 + 2s` 被拒（lease > 2×timeout 已被 v1 内嵌 relay 强制） | ✅ **Verified**。config.rs:248 精确命中：`if delivery_lease <= request_timeout.saturating_mul(2) + Duration::from_secs(2) { bail!("AERO_SNAPLINK_DELIVERY_LEASE_SECS must exceed two request timeouts by more than two seconds"); }`（:244-251 区域，drain 不变量 :253-255 同款）。connector 已克隆同款 `check_lease_invariant`（`crates/aero-audit-connector/src/config.rs:112-122`，12s/13s 边界测试 :235-239） |
| E3 | `crates/aero-server/src/snaplink_commercial/runtime.rs:221-277` — lease/claim/fencing + re-park-on-failure 语义；`http.rs:23 SCOPE_AUDIT` + `http.rs:410 validate_audit_receipt` 给 cc + receipt 契约 | ✅ **Verified**。`dispatch_batch` :214-229（`claim_due(lease, batch)` → `for_each_concurrent` → `deliver_claim`）；`deliver_claim` :247-277：Ok → `mark_delivered`（`Ok(false)` → warn "lost its lease before acknowledgement"——fence 丢失只 warn，lease expiry 重领）；Err → `mark_failed` 显式 re-park（失败则 warn "explicit re-park lost its fence"）。`http.rs:23` `SCOPE_AUDIT = "audit:event:write"`；`validate_audit_receipt` :410-423（event_id 匹配 + tenant_id + accepted_at + !conflict + status ∈ {ledgered, indexed, archived}）——connector `client.rs` 同款分类 |
| E4 | `crates/aero-storage/src/bot_delivery_outbox.rs` — dead-terminal 先例：`claim_token` fencing + `BOT_DELIVERY_MAX_ATTEMPTS` → DLQ；`event_outbox.rs mark_failed`（backoff re-park）为 retry 路径 | ✅ **Verified**。`BOT_DELIVERY_MAX_ATTEMPTS = 6` :15；`claim_due` :188（轮换 `gen_random_uuid()` token）；`mark_failed`/`mark_dead` 均栅栏于 `id + claim_token + claimed_at IS NOT NULL + status='pending'`（:258-306, :310-342）；dead 判定 `attempts >= BOT_DELIVERY_MAX_ATTEMPTS` :496（`is_dead_at` 边界测试 :580-581）；`event_outbox.rs` `mark_failed` :476-503（栅栏于 `attempts`，`available_at = now + outbox_backoff_delay(attempts)`）；`outbox_backoff_delay` :520-524（`BASE_BACKOFF_SECONDS=1` :17、`MAX_BACKOFF_SECONDS=300` :18——指数退避 cap 300s 的仓内先例）；另 `snaplink_commercial.rs` `MAX_BACKOFF_SECS=300` :18 + `commercial_delivery_backoff` :710-719（jitter + cap） |
| E5 | NOTE：backoff cap 300s、422/409→dead ≤1、403→dead 源自契约 §1.2（proposal line 9，v2 docs 不在仓内）——[PROPOSED]，由 probe 测试自己钉死 | ✅ **Verified（标注照实）**。`docs/proposals/audit-contract-batch-aero-im.md` 共 15 行，:9 原文："**B5-2**：新 crate（候选 `crates/aero-audit-connector`，无新第三方依赖）—— §1.2 语义（lease > 2×timeout、退避 cap 300s、**422/409/回执错 → dead ≤1 次**、403 → dead = T-11 fail-closed）"；:13 "8 处不可验证/[PROPOSED] 明确列出"——**语义数值保持 [PROPOSED]**，本 direction 的 probe 套件把它们钉成仓内规范性读法（E2/E4 的仓内先例 + connector 已实现语义 = 同值） |

### 1.1 补充证据（方向外事实，决定设计形态）

| # | Supplementary evidence | Verification result |
|---|---|---|
| E6 | **connector crate 已在 workspace（工作树，未提交）**——direction 问题陈述里的 "candidate" 已成现实，probe 套件的对象 = **已实现的真实 relay/client 代码** | ✅ **Verified**。`crates/aero-audit-connector/` 存在：src/{client,config,fake,lib,outbox,pg,relay,stub}.rs + src/bin/aero-audit-relay-drill.rs + tests/{state_machine,claim_validation}.rs；root `Cargo.toml` members + `[workspace.dependencies]` 均已加（git diff 实证 +2 行）。B5-2 设计文档（`docs/design/2026-08-07-aero-ai-b5-2-audit-connector-design.md`）记录 `cargo test -p aero-audit-connector --all-targets` = 24/24 绿、`Cargo.lock` diff 仅 +1 package（connector 自身） |
| E7 | probe 可驱动的 pub API 面（全部 `pub`，bin 可直接用，**零 connector 代码改动**） | ✅ **Verified**。`lib.rs`：`pub mod {client, config, fake, outbox, pg, relay, stub}`。`fake.rs`：`FakeOutbox::{set_now :101, insert, make_due_now, row}` + `FakeRowSnapshot{status, available_at, attempts, claim_token, lease_expires_at, last_error}` + `FakeStatus::{Ready,Claimed,Delivered,Dead}.code() 0-3`（镜像 0239 status 枚举）；单时钟域：`set_now(Some(t0))` 钉死全部 lease/backoff/fence 算术。`stub.rs`：`StubSink::start :80`（127.0.0.1 临时端口）、`set_behavior :120`、`posts()`、`token_url()/events_url()`；`SinkBehavior{token_claims, token_claims_after_first, events_status, receipt_valid, unauthorized_once, delay_ms}`——**scripted mock sink 即此**。`relay.rs`：`audit_backoff :43`（`2^(attempts-1)` cap 300s）、`MAX_BACKOFF_SECONDS=300` :33、`clamped_lease`、`AuditRelay::{new :88, dispatch_batch :143, deliver_claim :160}`。`client.rs`：`AuditClient::deliver :120` 分类（202+有效回执→Ok；401 首答→刷新一次；**403→Forbidden**；**422→Permanent(Unprocessable)**；**409→Permanent(Conflict)**；回执不匹配→Permanent(ReceiptMismatch)；payload guard→Permanent(PayloadGuard)；5xx→Transient；其他 4xx→Transient）、`validate_token_claims :197`、`check_lease_invariant :112` |
| E8 | 各分支的 in-crate 测试锚点（probe 套件的黑盒镜像对象，**不重复实现**，只补 CLI/CI 黑盒面） | ✅ **Verified**。`tests/state_machine.rs` 6 测试：`stale_token_cannot_ack_after_reclaim` / `backoff_is_bounded_and_exponential` / `permanent_error_dead_after_exactly_two_attempts`（422/409/receipt 三参数化）/ `forbidden_dead_on_first_attempt` / `happy_path_settles_and_removes_from_claimable` / `skew_gt_lease_cannot_livelock_claim_fence_settle`；`tests/claim_validation.rs` 11 测试。relay 策略（relay.rs 文档 + deliver_claim :160-）：Ok→settle；Transient→requeue（永不 dead）；Permanent→attempt1 requeue / attempt≥2 mark_dead（**dead ≤1 次重试**）；Forbidden→**attempt1 即 mark_dead** |
| E9 | **`gate deps-native` 当前红（存量债，AC4 直接相关）**：`aero-cli: unknown crate (not in architecture rules)` | ✅ **实跑取证**。`cargo run --quiet -p aero-cli -- gate deps-native` → `{"checked": 19, "violations": 1, "details": ["aero-cli: unknown crate (not in architecture rules)"]}`。`crates/aero-eng/src/checks.rs` `ALLOWED_DEPS` :195-256 无 `aero-cli` 也无 `aero-audit-connector`；"unknown crate" 分支只在成员**有 ≥1 个 workspace 内部依赖**时触发（aero-cli→aero-eng 命中；connector 零内部依赖所以当前漏检）。HEAD 的 `Cargo.toml` 已含 aero-cli 成员 → **存量红，非本批次引入**；AC4 "Cargo.toml audit via `Gate_` deps check" 要求该门转绿且 connector 入审计面（§R4） |
| E10 | `scripts/test-integration.sh` 的 B5 接线已存在，relay-mock 条目有家 | ✅ **Verified**。:33-34 throwaway DB 名（`AUDIT_CONNECTOR_INTEGRATION_DB="aero_audit_connector_$$"` 等）+ :54-55 `assert_disposable_db_name`；:226-270 B5-1/A3 段（0239 文件存在性门控，缺表显式 SKIP）；:265-269 "A3 part 1 (pin)" 占位。relay-mock 条目（**无 DB**）放 :226 之前的 no-DB 区 |
| E11 | sibling B5-4（同模块）将加 `audit-provision-check` + `gate audit`——命令面互斥，共享触点需协调 | ✅ **Verified**。`docs/requirements/2026-08-07-aero-cli-b5-4-audit-provision-check.req.md` R2/R3。共享触点：`Gate_` match 臂、`gate list` 串、`Completion_` cmds 串、test-integration.sh B5 段——本 direction 只加 `relay-probe`/`relay-mock`，不碰 audit-provision-check 命名（§7 协调记录） |
| E12 | 退出码机制与 spawn 约束 | ✅ **Verified**（B5-4 sibling 同款取证）。`Outcome::warning(code)` 经 `execute_with_ctx` Err 分支 → `main.rs exit(code)` 穿透；但 `run::run_cmd` :15-41 把脚本非零码**拍平为 1** → 保 distinct 码必须 `tokio::process::Command` 直 spawn（`Network_` 直用 tokio 的先例）。root `Cargo.toml` tokio = `features = ["full"]`（含 process）→ **aero-cli Cargo.toml 零改动** |

## 2. Verified current state

```
现状（aero-eng 对 relay 状态机零感知；connector 语义只有 in-crate 单测钉）：
a) connector 状态机（E6-E8） 已实现 + 24/24 in-crate 测试（FakeOutbox + StubSink 进程内双打）
   └─ 每分支语义：403→dead attempt1；422/409/回执错→requeue 一次→dead；
      transient→requeue（2^(attempts-1) cap 300s，永不 dead）；lease>2×timeout+2s 强制
b) aero-eng（E1）  9 命令；Network_ 只有 ping/dns/port；gate list 无 relay-mock；零 relay 符号
c) 黑盒面缺口      无任何 CLI/CI 可见的探测：真 relay+真 client 过 loopback HTTP 的
                  逐分支驱动 + 命名 PASS 断言 + CI 门 —— 本 direction 补
d) deps 审计（E9） gate deps-native 红（aero-cli unknown）；connector 未入 ALLOWED_DEPS/
                   dependency-check.sh（零内部依赖故漏检）—— AC4 要求转绿且入审计
e) CI 接线（E10）  test-integration.sh 已有 B5 段与 throwaway DB 纪律；no-DB 条目区可用

aero-eng 扩展样板（本 direction 复制的两条线，与 B5-4 sibling 相同）：
  Gate_   → run_cmd("bash", scripts/*.sh) → Outcome（拍平码，门=pass/fail）
  Network_ → tokio 直连（直 spawn 保 distinct 码）；Outcome::warning(code) 穿透
```

**Gaps this direction closes**（all verified）：① 无黑盒探测命令——`network relay-probe`（E1b）；② 无门——`gate relay-mock`（R3）；③ 探测引擎——DB-free 全场景 probe bin（E7 的 pub API 直接驱动，不依赖 B5-1 的 0239）；④ deps 审计门转绿 + connector 入审计面（E9，AC4 后半）；⑤ CI 接线——test-integration.sh 命名条目（E10）。

## 3. Scope

**In scope（B5-2 test vehicle，effort 7 的完整切片）**：
- `crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs`（**新 bin**，DB-free 黑盒探测套件）：真 `AuditRelay` + 真 `AuditClient`（reqwest 过 loopback HTTP）对 scripted `StubSink`，行状态断言用 `FakeOutbox`（钉死假时钟）；逐场景命名 PASS/FAIL 行 + 退出码契约（§R1）。**connector `src/` 状态机与 `Cargo.toml` 零改动**（E7 全 pub 面已够）。
- `crates/aero-cli/src/main.rs`：`Network_` 增 `relay-probe` 子命令（直 spawn probe bin，保码）+ help 文本；`Gate_` 增 `relay-mock` 臂 + `gate list` 串同步（§R2/R3）。
- `scripts/relay-mock.sh`（新，bash）：build probe → run → 断言 exit 0；`gate relay-mock` 的脚本面（§R3）。
- `crates/aero-eng/src/checks.rs`：`ALLOWED_DEPS` += `("aero-cli", &["aero-eng"])`、`("aero-audit-connector", &[])`——修存量红 + connector 入审计面（§R4）。
- `scripts/dependency-check.sh`：+= `check_deps "aero-audit-connector" ""` 与 `check_deps "aero-cli" "aero-eng"` 行（§R4）。
- `scripts/test-integration.sh`：no-DB 命名条目 "audit relay-mock probe suite"（§R5）。

**Out of scope（并行切片 / 其他模块——勿在本 direction 建造）**：
- **connector 状态机 / client / config / pg 任何改动**（B5-2 本体已实现，E6；本 direction 只消费 pub API）。
- **B5-1（0239 `audit_governance_outbox` DDL/enqueue）**：probe 用 `FakeOutbox` 完全 DB-free，**不依赖 0239 落库**；PG 侧 drill（`aero-audit-relay-drill`）保持原样与门控。
- **B5-4 `audit-provision-check` / `gate audit`**（sibling spec）：本 direction 只做状态机探测，不做配给预检；两个门并存（E11）。
- **B5-3 priority / moderation 优先**、v1 `snaplink_commercial/` 运行时、migrations/、aero-ai、aero-server 生产代码：一律不碰。
- **真实 sink/IdP 联调**（仓外）：probe 的定位就是「本地骨架 mock 先行」（proposal :13），真实端点不在本 direction 验收面。

## 4. Requirements

### R1 — probe bin `aero-audit-relay-probe`（新，DB-free 黑盒套件，场景矩阵 = 验收 1-3 的机器化）

`crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs`（`#[tokio::main]`，多线程 rt；依赖全部已在 `[dependencies]`：tokio/time/uuid/serde_json/anyhow——**零 Cargo.toml 改动**）。装配模式：每场景 `StubSink::start()`（临时 loopback 端口）+ `SinkBehavior` 脚本化 + `FakeOutbox::set_now(Some(t0))` 钉假时钟 + `RelayConfig`（timeout/lease 按场景）+ `AuditRelay::new(repo, client, config)` 驱动 `dispatch_batch`（真 reqwest 过 loopback HTTP）。

**场景矩阵**（每场景一行 `probe: <name>: PASS|FAIL <detail>`；全部场景跑完才退出）：

| 场景 | scripted sink 行为 | 断言（对 `FakeRowSnapshot` / `posts()` / `claim_due` 后态） | 对应验收 / in-crate 锚点（E8） |
|---|---|---|---|
| `happy_path` | 202 + 有效回执 | status==Delivered(2)、attempts==1、posts()==1、再次 dispatch 0 行、token/lease 清空 | AC 基础；`happy_path_settles_and_removes_from_claimable` |
| `forbidden_403` | `events_status=403` | **status==Dead(3)、attempts==1、posts()==1、第二次 dispatch 0 行（无重试无再尝试）、last_error 含 "403"/Forbidden** | **AC1**；`forbidden_dead_on_first_attempt` |
| `permanent_422` | `events_status=422` | attempt1 → Ready、attempts==1、`available_at == t0 + audit_backoff(1) == t0+1s`（假时钟**精确等式**）；再次 dispatch → **Dead、attempts==2**；posts()==2 | **AC2**；`permanent_error_dead_after_exactly_two_attempts` |
| `permanent_409` | `events_status=409` | 同 422 参数化（attempt1 requeue → attempt2 dead，≤1 次重试） | **AC2** |
| `receipt_mismatch` | `receipt_valid=false`（回执 event_id 损坏 = malformed/absent receipt 同支） | 同 422/409 终态支（ReceiptMismatch → dead ≤1） | **AC2** |
| `transient_500` | `events_status=500` | requeue（Ready、attempts==1、`available_at==t0+1s`）；再 dispatch → requeue（attempts==2、`==t0+2s`）——**永不 dead**；另断言 `audit_backoff(1..=11) == [1,2,4,8,16,32,64,128,256,300,300]`（**cap 300s**，`MAX_BACKOFF_SECONDS==300` :33 常量） | **AC2 后半**；`backoff_is_bounded_and_exponential` |
| `transient_timeout` | `delay_ms > request_timeout`（如 timeout=200ms、delay=2000ms） | Transient（transport error）→ requeue、永不 dead、posts()==1 | **AC2 后半**（network error 支） |
| `lease_invariant` | 纯函数 | `check_lease_invariant(5s, 12s)==Err`、`(5s, 13s)==Ok`——**边界 = 2×timeout+2s**（镜像 config.rs:248 拒绝式） | **AC3**；config.rs :235-239 |
| `fencing_stale_token` | 202 有效回执 | claim（token A）→ 假时钟推进过 lease + `make_due_now` → 再 claim（token B，attempts==2）→ `settle(id, tokenA)==false`（**stale token 永不完成**）、`settle(id, tokenB)==true`；另断言 `requeue/mark_dead` 携 stale token 均 `false`（行保持在新 claim 下） | **AC3**；`stale_token_cannot_ack_after_reclaim`、`skew_gt_lease_cannot_livelock_claim_fence_settle` |

- **确定性纪律**：除 `transient_timeout`（真 sleep，界内）外全部断言用假时钟精确值，无 wall-clock 窗口；StubSink 临时端口无冲突；套件总时长有界（<15s）。
- **退出码契约**：`0` = 全部 PASS；`1` = 任一场景 FAIL（失败行具名，套件自验证非 vacuous——每个场景都有**可观察状态断言**，不是只看 HTTP 状态码）；`2` = usage。
- **不重复实现 in-crate 测试**：probe 与 `tests/state_machine.rs` 断言同一状态机，但走「真 client + 真 loopback HTTP + CLI 进程」黑盒面——in-crate 单测（进程内双打）与 probe（黑盒）互为冗余防线。

### R2 — `aero-eng network relay-probe [mock-url]`（Network_ 子命令，distinct 码穿透）

`crates/aero-cli/src/main.rs` `Network_` match 增 `"relay-probe"` 臂（help 文本同步加一行 `relay-probe [mock-url] - run the audit relay mock-sink probe suite`）：
- **可选 `<mock-url>`**：提供时先做 `Network_` 同款 TCP dial（`tokio::net::TcpStream::connect`，2s 超时）——dial 失败 → `Outcome::error("relay mock unreachable: …")`（exit 1），套件不跑；缺省跳过 dial（套件自含，见 §7）。
- **直 spawn probe bin**（E12：`run_cmd` 拍平码，必须 `tokio::process::Command`）：默认 `cargo run --quiet -p aero-audit-connector --bin aero-audit-relay-probe`；env 覆盖 `AERO_RELAY_PROBE_BIN`（CI 预编译直接路径，省编译）。**aero-cli 不链接 connector**（E1 零 DB 约束）。
- 退出码映射：0 → `Outcome::ok`；1 → `Outcome::error`（exit 1）；2 → `Outcome::warning(2)`。超时 120s；stdout/stderr 透传。

### R3 — `scripts/relay-mock.sh` + `aero-eng gate relay-mock`（Gate_ bash-script 模式）

- `scripts/relay-mock.sh`（新，`set -euo pipefail`）：解析 probe bin（`AERO_RELAY_PROBE_BIN` 或 `cargo build -p aero-audit-connector --bin aero-audit-relay-probe` 定位 target 产物）→ 运行 → **断言 exit 0**；任何场景 FAIL 行转 stderr 且非零退出；零临时文件、零 env 污染。
- `Gate_` match 增 `"relay-mock" => b(f("relay-mock.sh"), 300).await`（**现有 `b()` helper 原样**——门 = pass/fail，非零拍平 exit 1）；`gate list` 串 += `"relay-mock"`。
- **不并入 `gate all`**：relay-mock 要编译 connector crate（重），且语义属 B5 门上下文（同 B5-4 sibling §7 决策）；CI 在 test-integration.sh 显式调用。

### R4 — deps 审计接线（AC4 后半："Cargo.toml audit via `Gate_` deps check"）

- `crates/aero-eng/src/checks.rs` `ALLOWED_DEPS` += `("aero-cli", &["aero-eng"])`、`("aero-audit-connector", &[])`——修 E9 存量红（`gate deps-native` 转绿）+ connector 从「漏检」变「入审计」（零内部依赖 = `&[]` 白名单，规则面闭合）。
- `scripts/dependency-check.sh` += `check_deps "aero-audit-connector" ""`、`check_deps "aero-cli" "aero-eng"` 行（依赖方向规则面同步）。
- **零新第三方依赖钉死**（probe 不新增任何依赖，connector 维持 B5-2 已验状态）：断言 = `cargo tree -p aero-audit-connector --edges normal --depth 1` 的每个直接依赖要么 `(workspace)` 要么 `base64 v0.22.x`（lockfile 已含，`Cargo.lock` 相对 HEAD 只允许 +1 package = connector 自身——E6/B5-2 设计已验）。该断言进 test-integration 条目（§R5）。

### R5 — test-integration.sh 命名条目 "audit relay-mock probe suite"（no-DB，CI 接线）

`scripts/test-integration.sh` no-DB 区（B5 段 :226 之前）新增命名条目：
1. `cargo build -p aero-cli -p aero-audit-connector --bins`（一次编译）；
2. `aero-eng network relay-probe` → **断言 exit 0**；
3. **场景名清单 grep**（防 vacuous green，B5-4 sibling 具名清单纪律同款）：对输出逐名断言 `probe: happy_path: PASS` … `probe: fencing_stale_token: PASS`（9 场景具名常量；缺任何一行 = fail）；
4. `aero-eng network relay-probe http://127.0.0.1:<已关闭端口>` → **断言 exit 1 + 输出含 "unreachable"**（mock-url dial 负例）；
5. `aero-eng gate relay-mock` → **断言 exit 0**；
6. `aero-eng gate deps-native` 与 `aero-eng gate deps` → **断言 exit 0**（AC4 deps 审计门，R4）；
7. `cargo tree -p aero-audit-connector --edges normal --depth 1` 依赖面断言（R4）。
条目无 DB、无 server boot、无 AERO_AUDIT_* env（probe 全自含）。

### R6 — 约束

- **aero-cli 零新依赖**（E1：aero-eng/tokio/serde_json/async-trait 不变，`Cargo.toml` 零改动——tokio features=["full"] 已含 process）。
- **connector 状态机零改动**：`src/` 生产代码一行不动；只新增 `src/bin/aero-audit-relay-probe.rs`；`Cargo.toml` 零改动（dev-dependencies 不动——bin 走 `[dependencies]`）。
- **零新第三方依赖**（全仓 Cargo.lock 相对 HEAD 仅允许 connector 自身，R4 断言）。
- **不触碰**：migrations/、aero-ai、aero-server 生产代码、v1 `snaplink_commercial/`、B5-1 的 0239、B5-4 的 audit-provision-check 命名。
- 新 env：仅可选 `AERO_RELAY_PROBE_BIN`（CLI harness 旋钮，非 server 配置面）。
- 提交前门禁：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规；新 bin/脚本小体量，不触 file-size 阈值）· `gate deps`/`gate deps-native` 绿（R4 修复后）。

## 5. Acceptance checks（direction 原样保留，逐条 testable）

> direction acceptance 原文四条，逐条保留并钉死测试面。机器断言面 = probe bin 场景行（R1）+ test-integration.sh 命名条目（R5）；in-crate 单测（E8）为同语义冗余防线。

### AC1 — Scripted mock sink returns 403 → connector parks row dead exactly once, no further attempts, no retry（T-11 fail-closed：audit event never silently dropped as success）
**测试 = probe 场景 `forbidden_403`**（`SinkBehavior{events_status=403}`，真 reqwest 过 loopback HTTP）+ CI 层断言：
- 行终态 `Dead`（FakeStatus::Dead，code 3）、`attempts == 1`（**exactly once**）、`stub.posts() == 1`、`last_error` 含 "403"；
- **无重试无再尝试**：第二次 `dispatch_batch()` 返回 0 行（dead 行不在 claimable 集，镜像 `claim_due` 的 `status IN (0,1)` 过滤）——"never silently dropped as success" = 行是显式 dead 终态，非 settle 非丢失；
- CI：test-integration 条目 grep `probe: forbidden_403: PASS` + 套件 exit 0（R5）。in-crate 冗余锚点：`forbidden_dead_on_first_attempt`。

### AC2 — 422 and 409 responses → dead ≤1 retry; malformed/absent receipt → same terminal branch; transient 5xx/network error → exponential backoff capped at 300s（[PROPOSED] contract value）
**测试 = probe 场景 `permanent_422` / `permanent_409` / `receipt_mismatch` / `transient_500` / `transient_timeout`**：
- 422/409/receipt 三支同一终态机：attempt1 → requeue（假时钟精确 `available_at == t0+audit_backoff(1)`）；attempt2 → `Dead`（attempts==2、posts()==2）——**dead ≤1 retry**（"≤1" = 恰一次 requeue 后 dead）；
- `receipt_mismatch`（`receipt_valid=false` 回执 event_id 损坏 = malformed/absent receipt 同支）→ 同终态支；
- 5xx/network：`transient_500`（requeue ×2 均非 dead、退避 1s→2s）+ `transient_timeout`（delay>timeout → transport Transient requeue）；**300s cap** = `audit_backoff` 序列断言 `[1,2,4,…,256,300,300]` + `MAX_BACKOFF_SECONDS==300`（仓内同值先例：`event_outbox.rs:18`、`snaplink_commercial.rs:18`——[PROPOSED] 契约值由 probe 测试钉为仓内规范性读法）；
- CI：grep 五个场景 PASS 行 + exit 0。in-crate 冗余锚点：`permanent_error_dead_after_exactly_two_attempts`、`backoff_is_bounded_and_exponential`。

### AC3 — Test asserts lease > 2×timeout invariant（mirrors config.rs:248 rejection）and fencing: a re-claimed delivery with a stale claim token never completes
**测试 = probe 场景 `lease_invariant` + `fencing_stale_token`**：
- `check_lease_invariant(5s, 12s)==Err`、`(5s, 13s)==Ok`——边界 2×timeout+2s 精确镜像 `snaplink_commercial/config.rs:248` 拒绝式（connector `config.rs:112` 同款）；
- fencing：claim（token A）→ 假时钟推进过 lease + 再 claim（token B）→ `settle(id, tokenA)==false`（**stale claim token 永不完成**）、`settle(id, tokenB)==true`；`requeue/mark_dead` 携 stale token 均 false（行不被旧 claim 改写，lease expiry 重领语义）；
- CI：grep 两场景 PASS 行 + exit 0。in-crate 冗余锚点：`stale_token_cannot_ack_after_reclaim`、`skew_gt_lease_cannot_livelock_claim_fence_settle`、config.rs :235-239。

### AC4 — `aero-eng gate relay-mock` runs the probe suite in CI; connector crate adds zero third-party dependencies（Cargo.toml audit via `Gate_` deps check）
- **`gate relay-mock`**：Gate_ 臂（R3）→ `scripts/relay-mock.sh` → probe 套件，exit 0/1（门语义）；CI = test-integration 命名条目步骤 5（R5）；
- **deps audit**：`gate deps-native` / `gate deps` 转绿（R4：ALLOWED_DEPS += aero-cli + aero-audit-connector，修 E9 存量红）+ connector 入两个依赖检查面；零新第三方 = `cargo tree --depth 1` 断言（R4，CI 步骤 6/7）；
- CI：条目全步骤 exit 0 断言（R5 步骤 2-7）。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| 9 场景黑盒矩阵（AC1/AC2/AC3 全部机器断言） | `crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs`（新 bin） | 自跑：`cargo run -p aero-audit-connector --bin aero-audit-relay-probe`（无 DB、无外部依赖、loopback 仅） |
| CLI 命令 + gate + 命名清单 + deps 审计（AC1-AC4 CI 面） | `scripts/test-integration.sh` 命名条目 "audit relay-mock probe suite"（no-DB 区） | bash（`set -euo pipefail`、throwaway 纪律、`assert_disposable_db_name` 先例区） |
| gate 脚本 | `scripts/relay-mock.sh`（新） | bash，被 `Gate_` `b()` 调用 |
| deps 审计修复 | `crates/aero-eng/src/checks.rs`（ALLOWED_DEPS）+ `scripts/dependency-check.sh`（check_deps 行） | `aero-eng gate deps-native` / `gate deps` |
| 命令注册/help/completion 不破坏 | `aero-eng help`、`aero-eng network help` 冒烟 + `cargo test --workspace --lib` | cargo + 二进制冒烟 |
| 状态机语义冗余防线（不动，仅引用） | `crates/aero-audit-connector/tests/{state_machine,claim_validation}.rs`（24/24 既有） | `cargo test -p aero-audit-connector --all-targets` |

## 7. Risks / [PROPOSED] / 决策点

- **§1.2 契约数值 [PROPOSED]（proposal :13，v2 docs 仓外）**：backoff cap 300s、422/409/回执错 → dead ≤1、403 → dead attempt1——本 direction 的 probe 测试把它们钉成**仓内规范性读法**，且每个数值都有仓内同值先例（`event_outbox.rs:18` 300s、`snaplink_commercial.rs:18` 300s、`config.rs:248` lease 规则、`bot_delivery_outbox.rs` dead 终态）。契约若改判：只动 `audit_backoff` 常量与场景断言表，probe/门结构不动（与 B5-2 设计 §7 同决策）。
- **探测引擎落家 = connector bin，非 aero-cli 代码**：aero-cli 零 DB 约束（E1）禁止链接 sqlx/reqwest 依赖树；`network relay-probe` = 直 spawn（E12 保码机制）。方向标题的 "command in aero-cli" 以命令/门/接线兑现，引擎在 connector crate（其 fake/stub 双打本就是为这种驱动造的，E7）。
- **`mock-url` 语义**：可选参数 = `Network_` 同款 TCP dial 预检（对**外部**部署的 mock sink 做可达性判定），分支矩阵本体自含（行状态可观测性需要进程内 `FakeOutbox` 的 snapshot 面——外部 python sink 无法暴露 attempts/available_at/claim_token，故不自造第二个 sink 实现；StubSink 即 scripted mock sink，E7）。
- **与 sibling B5-4 的共享触点**：`Gate_` match / `gate list` 串 / `Completion_` cmds 串 / test-integration.sh B5 段由两个 direction 同时触碰——命令名互斥（`relay-mock` vs `audit`，`relay-probe` vs `audit-provision-check`），集成时手接共享文件（AGENTS.md §4.1 多 agent 并行纪律）；`gate all` 两者都不并入（决策同 B5-4 §7）。
- **E9 存量红属 AC4 修复面**：`gate deps-native` 的 aero-cli 红是 HEAD 既有（非本批次引入），但不修则 AC4 "Cargo.toml audit via Gate_ deps check" 无法为绿——R4 是最小修复（只加两行 ALLOWED_DEPS + 两行 dependency-check.sh，不碰其他条目）。
- **probe 与 in-crate 测试的关系**：互为冗余而非替代——in-crate 钉精确转换语义（假时钟进程内），probe 钉「真 client 过真 loopback HTTP + 进程级退出码 + CI 门」黑盒面；场景名清单 grep（R5 步骤 3）防 vacuous green（缺场景行 = fail）。
- **无 DB 前提**：probe 不依赖 B5-1 的 0239 落库（FakeOutbox），因此**当前即可全绿**；PG 侧 drill 保持 0239 门控（现状不变）。
- **工作树状态**：connector + aero-ai（B5-1）+ test-integration.sh 改动均未提交（在途切片）；本 direction 的 bin/命令/脚本与 B5-2 切片一并提交，不碰 B5-1 的 aero-ai 文件。

## 8. Sequencing

1. **probe bin**：`aero-audit-relay-probe.rs`（R1 九场景 + 退出码契约）——独立可验：`cargo run -p aero-audit-connector --bin aero-audit-relay-probe; echo $?`。
2. **aero-eng 命令**：`Network_` `relay-probe` 臂（R2，直 spawn 保码）+ help 文本。
3. **gate**：`scripts/relay-mock.sh`（R3）+ `Gate_` `relay-mock` 臂 + `gate list` 串。
4. **deps 审计**：ALLOWED_DEPS 两行 + dependency-check.sh 两行（R4）→ `gate deps-native` 转绿实跑验证。
5. **CI 接线**：test-integration.sh 命名条目（R5 步骤 1-7）。
6. **门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· `aero-eng gate relay-mock` + `gate deps` + `gate deps-native` 绿 · test-integration 条目全绿 · no-touch 守卫（`git diff --stat` 不含 migrations/、aero-ai、snaplink_commercial/、connector `src/` 生产代码）。
