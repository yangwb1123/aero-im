# Design — aero-cli B5-2 test vehicle：mock-sink `relay probe` 命令 + `gate relay-mock`

- **Module**: `crates/aero-cli`（aero-eng 工程 CLI）+ 探测引擎落家 `crates/aero-audit-connector`（新 bin）
- **上游**: `docs/requirements/2026-08-07-aero-cli-b5-2-relay-probe.req.md`（R1-R6）；sibling `docs/requirements/2026-08-07-aero-cli-b5-4-audit-provision-check.req.md`
- **对象状态机**: `crates/aero-audit-connector/src/relay.rs` + `client.rs`（已实现，本设计零改动）
- **Status**: Design。证据核对日期 2026-08-07（全部实跑/实读，见 §1 账本）

## 0. 设计摘要（TL;DR）

三层结构，自下而上：

```
L0 探测引擎   crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs（新 bin）
              └─ 真 AuditRelay + 真 AuditClient(reqwest) × loopback StubSink，
                 行状态断言走 FakeOutbox（钉死假时钟）——9 场景，退出码 0/1/2
L1 CLI 面     aero-eng network relay-probe [mock-url]（Network_ 臂，直 spawn 保码）
              + gate relay-mock（Gate_ 臂 → scripts/relay-mock.sh）
L2 CI/审计    test-integration.sh 命名条目（9 场景 PASS 行 grep，防 vacuous）
              + checks.rs ALLOWED_DEPS += aero-cli/aero-audit-connector（修存量红）
              + dependency-check.sh += 2 行
```

零生产代码改动：connector `src/` 一行不动（只加 bin）、aero-cli `Cargo.toml` 零改动、connector `Cargo.toml` 零改动、无迁移、无 B5-1/0239 依赖、无 B5-4 命名重叠。

## 1. 证据核对账本（untrusted evidence → 实跑验证）

| Evidence claim | Result |
|---|---|
| `main.rs` `Network_` :327-328（ping/dns/port TCP dial）、`a!()` :23-31 九命令、`Gate_` :104 + `b()` :105、`gate list` 串 :113 | ✅ 逐行命中；`port` 臂 `splitn(2, ':')` 解析 + `tokio::net::TcpStream::connect` |
| aero-cli deps = aero-eng/tokio/serde_json/async-trait（零 DB） | ✅ `Cargo.toml` 原文核对 |
| `config.rs:248` lease bail（`delivery_lease <= 2×timeout+2s` 拒） | ✅ `snaplink_commercial/config.rs:249` 精确命中；connector 克隆 `check_lease_invariant`（`connector/src/config.rs:112-122`） |
| `runtime.rs:221-277` claim/fencing/re-park；`http.rs:23 SCOPE_AUDIT`；`:410 validate_audit_receipt` | ✅ `dispatch_batch` :214-229、`deliver_claim` :247-277、:23、:410 全部精确 |
| `bot_delivery_outbox.rs` dead 终态 + `event_outbox.rs mark_failed` 退避 | ✅ `BOT_DELIVERY_MAX_ATTEMPTS=6` :15、`is_dead_at` :496、`mark_failed` :476-503、`outbox_backoff_delay` :520-524；**注**：300s cap 属 `event_outbox.rs:18`（bot_delivery 自身 cap 3600）——req doc E4 归置正确，evidence 摘要行略混 |
| §1.2 [PROPOSED]（proposal :9） | ✅ proposal 原文逐字核对 |
| connector crate 已落 workspace（未提交）、24/24 in-crate 测试 | ✅ `?? crates/aero-audit-connector/`；实跑 `cargo test -p aero-audit-connector --all-targets` = **24 passed + 1 ignored（PG 门控）**：7 unit + 11 claim_validation + 6 state_machine |
| pub API 面全可驱动（E7） | ✅ `FakeOutbox::{set_now,insert,make_due_now,row}`、`FakeRowSnapshot`、`FakeStatus::code()`、`StubSink::{start,token_url,events_url,posts,set_behavior,shutdown}`、`SinkBehavior` 六字段、`AuditRelay::{new,dispatch_batch}`、`RelayConfig` 全 pub 字段、`OutboxRepo` trait 全 pub 方法 |
| client 分类（E7/E8） | ✅ 403→Forbidden、422→Unprocessable、409→Conflict、回执错→ReceiptMismatch、payload guard→PayloadGuard、5xx/超时→Transient |
| `gate deps-native` 当前红（存量） | ✅ **实跑**：`{"checked": 19, "violations": 1, "details": ["aero-cli: unknown crate (not in architecture rules)"]}`；`ALLOWED_DEPS` :195-256 无两 crate |
| `run_cmd` 拍平码 + `Outcome::warning(code)` + tokio `features=["full"]` | ✅ `run.rs` 非零→`Outcome::error`（exit 1）；`outcome.rs:47 warning(exit_code)`；root `Cargo.toml:80` |
| test-integration.sh B5 接线（:33-34 / :54-55 / :226-270） | ✅ 0239 文件存在性门控 + throwaway DB 纪律原文核对 |
| probe bin 已存在？ | ❌ **不存在**——`src/bin/` 仅 `aero-audit-relay-drill.rs`（PG drill，A3 门控）。与 req doc 一致：probe bin 是交付物 |

### 1.1 设计推导出的新事实（evidence 未覆盖，design 据此定断言）

| # | 事实 | 来源 | 设计影响 |
|---|---|---|---|
| D1 | `claim_due` 在 claim 时 `attempts += 1`；`requeue` 设 `available_at = now + audit_backoff(attempts)`（attempts = 本次 claim 的 attempts 值，now = requeue 时刻的假时钟） | `fake.rs` claim_due/requeue 原文 | 假时钟精确等式：attempt1 requeue → `t0 + audit_backoff(1) == t0+1s`；**推进 `set_now(t0+1s)` 后** attempt2 requeue → `(t0+1s) + audit_backoff(2) == t0+3s`。**第二次 dispatch 前必须 `set_now(t0+1s)`**（否则 `available_at > now` 不可 claim——假时钟钉死会卡住场景）。任何 requeue 的 available_at = requeue 时刻的假时钟 + backoff(本次 claim 的 attempts)，不是 t0 + 序号 |
| D2 | relay 对 202 恒 `settle`（`deliver_claim` Ok 臂），失败臂全部 requeue/mark_dead（释放 token）——**行不可能经 dispatch_batch 停留在 Claimed 态** | `relay.rs:160-209` | `fencing_stale_token` 场景的 claim 必须直接走 `OutboxRepo::claim_due`（pub trait 方法）驱动，不能靠 dispatch_batch；断言 settle/requeue/mark_dead 的 fencing 语义 |
| D3 | `settle`/`requeue`/`mark_dead` 三 fence 相同：`status==Claimed && claim_token==Some(token) && attempts 匹配 && lease 未过` | `fake.rs` 三方法原文 | stale token A 在 B 重新 claim 后全部 `false`；`make_due_now` 是「lease 过期重领」的使能器（清 token/lease + available_at=now） |
| D4 | `validate_delivery_payload` 要求 payload 无 `tenant_id` 且 `source_system == config.source_system` | `client.rs:363-376` | probe 种子 payload 必须带匹配 `source_system`，否则所有场景误入 PayloadGuard 永久支（场景矩阵会全错） |
| D5 | `AuditClient::new` 用 `config.request_timeout` 建 reqwest client（`client.rs:108-117`）；StubSink JWT 是 `alg:none` + `make_jwt` 任意 claims | `client.rs`、`stub.rs:233-240` | `expected_iss/aud/scope/sub` 必须与 probe 自定 claims 常量一致（probe 同时写 SinkBehavior.token_claims 与 RelayConfig.expected_*） |
| D6 | aero-cli 无 `url` crate（deps 仅 4 个） | `Cargo.toml` | `mock-url` 解析手工字符串（剥 scheme → split host:port），零新依赖 |
| D7 | `FakeStatus` 枚举 code 0-3（Ready/Claimed/Delivered/Dead），行快照全字段 pub | `fake.rs:27-77` | 断言面 = `FakeRowSnapshot` 全字段相等断言（status/attempts/available_at/claim_token/lease_expires_at/last_error） |

## 2. API 变更（逐文件、逐符号）

### 2.1 新 bin `crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs`（L0，核心交付）

**CLI 契约**：无参数（场景自含）；stdout 每场景一行 `probe: <name>: PASS|FAIL <detail>`；退出码 `0`=全 PASS、`1`=任一 FAIL、`2`=usage。**不 fail-fast**：9 场景全跑完再聚合退出码（CI 具名 grep 需要全部行）。

**装配模板**（每场景独立实例，互不共享状态）：

```rust
let sink = StubSink::start().await?;              // 127.0.0.1:0 OS 临时端口
sink.set_behavior(SinkBehavior { events_status: 422, ..Default::default() }).await;
let repo = Arc::new(FakeOutbox::new());
repo.set_now(Some(t0)).await;                      // 钉死单时钟域
let config = probe_config(&sink, timeout, lease);  // RelayConfig 全 pub 字段直构
let relay = AuditRelay::new(repo.clone(), AuditClient::new(config.clone())?, config);
let claimed = relay.dispatch_batch().await?;
let row = repo.row(EVENT_ID).await.unwrap();
assert_eq!(row, expected_snapshot);                // FakeRowSnapshot 全字段相等
```

**`probe_config` 要点**（D4/D5）：`expected_iss="https://idp.test"`、`expected_aud="audit-sink"`、`expected_scope="audit:event:write"`、`expected_sub="svc-audit-relay"`——同值写入 `SinkBehavior.token_claims`（`make_jwt` 原样嵌入）；`source_system="aero-im"`；种子 payload = `json!({"event_id": id, "source_system": "aero-im", "class": "admin", "priority": 0, "payload": …})`（无 `tenant_id`）。

**场景矩阵**（断言公式全部由 D1-D3 推导，假时钟精确等式；`t0` = 场景钉死的起点）：

| # | 场景 | scripted 行为 | 断言（FakeRowSnapshot / posts() / claim 数） |
|---|---|---|---|
| 1 | `happy_path` | 202 + 有效回执 | `Delivered`、attempts==1、posts()==1、token/lease 清空、`last_error==None`；二次 dispatch 返回 0 |
| 2 | `forbidden_403` | `events_status=403` | `Dead`、attempts==1（exactly once）、posts()==1、`last_error` 含 "403"；二次 dispatch 0 行（dead 不在 claimable） |
| 3 | `permanent_422` | `events_status=422` | 第一次 dispatch：`Ready`、attempts==1、`available_at == t0+audit_backoff(1) == t0+1s`；**`set_now(t0+1s)`** 后再 dispatch：`Dead`、attempts==2、posts()==2 |
| 4 | `permanent_409` | `events_status=409` | 同 3 参数化（≤1 次重试后 dead） |
| 5 | `receipt_mismatch` | `receipt_valid=false` | 同 3/4 精确断言：第一次 `Ready`/attempts==1/`available_at==t0+1s`/`last_error` 含 "receipt"；`set_now(t0+1s)` 后第二次 `Dead`/attempts==2/posts()==2（ReceiptMismatch → dead ≤1） |
| 6 | `transient_500` | `events_status=500` | 第一次：`Ready`、attempts==1、`available_at==t0+1s`；`set_now(t0+1s)` 后再 dispatch：`Ready`、attempts==2、`available_at==(t0+1s)+audit_backoff(2)==t0+3s`——**永不 dead**；另纯函数断言 `audit_backoff(1..=11) == [1,2,4,8,16,32,64,128,256,300,300]`（cap 300s = `MAX_BACKOFF_SECONDS`） |
| 7 | `transient_timeout` | `delay_ms=2000`、timeout=200ms | 与 6 第一次同款精确断言：`Ready`、attempts==1、`available_at==t0+1s`、token/lease 清空、`last_error` 含 "transport failed"、posts()==1（唯一真 sleep 场景，10× 余量，界内） |
| 8 | `lease_invariant` | 纯函数，无 sink | `check_lease_invariant(5s, 12s)==Err`、`(5s, 13s)==Ok`——边界恰 `2×timeout+2s`（镜像 config.rs:248 拒绝式） |
| 9 | `fencing_stale_token` | 202 + 有效回执 | **claim 走 `repo.claim_due` 直驱（D2）**：claim A 后先断言 Claimed 快照 `{Claimed, attempts:1, token:A, lease:t0+lease}`（直驱观测点的意义所在）；`set_now(t0+lease+1s)` + `make_due_now` + 再 `claim_due`→token B（attempts==2，快照 `{Claimed, attempts:2, token:B}`）；`settle(id,A)==false`、`requeue(id,A,2,..)==false`、`mark_dead(id,A,2,..)==false`（stale 永不改写行——attempts 传当前值 2 以隔离 token 栅栏）；`settle(id,B)==true` |

**套件纪律**：总时长 <15s（transient_timeout 2s 为最大单项）；除场景 7 外零 wall-clock 依赖；每个场景有 ≥1 个可观察行状态断言（非 vacuous；场景 8 是唯一例外——纯配置不变量，无行状态可断言）；场景间无共享状态（各自 sink/repo/config）。**dispatch_batch 不可观测面清单（直驱 repo 才能观测，场景 9 全覆盖）**：① Claimed 在途态——relay 对每个 claim 在同一调用内恒落一终/重试转移，假时钟下栅栏不可能失败，行不会以 Claimed 存活；② 栅栏拒绝结果——settle/requeue/mark_dead 返 `false` 只能以 stale token/attempts 直驱构造；③ make_due_now / 租约过期重领（不经 requeue 周期）。PayloadGuard / claim-drift / 401-refresh 均经 dispatch_batch 可观测（last_error 文本 / posts() 计数），无需直驱。

### 2.2 `crates/aero-cli/src/main.rs`（L1）

- `Network_` match 增 `"relay-probe"` 臂（:327 块内）：
  - 可选 `<mock-url>`：手工解析（D6：剥 `http://`/`https://` 前缀 → `splitn(2, ':')`，缺省端口 80/443）→ `tokio::net::TcpStream::connect` 2s 超时；失败 → `Outcome::error("relay mock unreachable: …")`（exit 1，套件不跑）；缺省跳过 dial。
  - 直 spawn（保 distinct 码，`run_cmd` 拍平码不可用）：`tokio::process::Command`，程序 = `AERO_RELAY_PROBE_BIN`（env 覆盖，CI 预编译直跑）或 `cargo run --quiet -p aero-audit-connector --bin aero-audit-relay-probe`；`stdout/stderr` 继承透传；`tokio::time::timeout(120s)` 超时 `child.kill()`。
  - 退出码映射：`0→Outcome::ok`、`1→Outcome::error`、`2→Outcome::warning(2)`；**其他码（panic 101、信号等）一律 `Outcome::error`**（契约外即红）。
  - help 文本（`"help"` 臂）加 `relay-probe [mock-url] - run the audit relay mock-sink probe suite`。
- `Gate_` match 增 `"relay-mock" => b(f("relay-mock.sh"), 300).await`；`"list"` 串 :113 追加 `relay-mock`。**不并入 `gate all`**（编译重 + B5 门上下文，与 sibling B5-4 同决策）。
- 零 `Cargo.toml` 改动（tokio features=["full"] 已含 process）。

### 2.3 新脚本 `scripts/relay-mock.sh`（L1 门面）

`set -euo pipefail`；解析 probe bin（`AERO_RELAY_PROBE_BIN` 优先，否则 `cargo build -p aero-audit-connector --bin aero-audit-relay-probe -q` 后取 `target/debug/aero-audit-relay-probe`）→ 运行 → **`rc=$?`，`exit $rc` 无条件传播 probe 退出码（仅 0 绿）**——绿/红判定只认退出码；`FAIL` 行转 stderr 只是提示（若只扫 `FAIL` 行，probe 打印完 PASS 后崩溃将无 FAIL 行可扫 = fail-open）；零临时文件、零 env 污染。

### 2.4 deps 审计（L2，AC4 后半——修 E9 存量红）

- `crates/aero-eng/src/checks.rs` `ALLOWED_DEPS` 追加（:195-256 表尾）：`("aero-cli", &["aero-eng"])`、`("aero-audit-connector", &[])`——aero-cli 红修复 + connector 从「零内部依赖漏检」变「入审计面」（`&[]` = 白名单闭合）。
- `scripts/dependency-check.sh` 追加：`check_deps "aero-cli" "aero-eng"`、`check_deps "aero-audit-connector" ""`（:49-62 序列尾）。
- 零新第三方依赖断言（进 CI 条目）：`cargo tree -p aero-audit-connector --edges normal --depth 1` 的直接依赖要么 `(workspace)` 要么 `base64 v0.22.x`（lockfile 既有；相对 HEAD 的 `Cargo.lock` diff 只允许 +1 package = connector 自身）。

### 2.5 `scripts/test-integration.sh` 命名条目 "audit relay-mock probe suite"（L2，no-DB 区，:226 B5 段之前）

1. `cargo build -p aero-cli -p aero-audit-connector --bins`
2. `cargo run -p aero-cli -- network relay-probe` → exit 0（cargo run 透传子进程退出码；裸 `aero-eng` 不在 CI PATH，勿用）
3. **具名 PASS 行 grep**：9 行 `probe: <name>: PASS` 逐一 `grep -F` 锚定（防 vacuous green；缺行即 fail；FAIL 行 `probe: <name>: FAIL …` 不会被 `grep -F 'probe: <name>: PASS'` 误匹配）
4. 负例：`cargo run -p aero-cli -- network relay-probe http://127.0.0.1:1`（端口 1 恒闭；勿用随机高端口防偶发占用假红）→ exit 1 且输出含 `unreachable`
5. `aero-eng gate relay-mock` → exit 0
6. `aero-eng gate deps-native` + `aero-eng gate deps` → exit 0（AC4）
7. `cargo tree` 依赖面断言（§2.4）

无 DB、无 server boot、无 `AERO_AUDIT_*` env。

## 3. 兼容性约束

| # | 约束 | 强制方式 |
|---|---|---|
| C1 | **aero-cli 零新依赖**：`Cargo.toml` 一行不改（D6 手工 URL 解析即为此） | `git diff --stat crates/aero-cli/Cargo.toml` 为空 |
| C2 | **connector `src/` 生产代码零改动**：只新增 `src/bin/`（独立 target，不编入 lib） | no-touch 守卫：`git diff --stat` 不含 `connector/src/{client,config,fake,lib,outbox,pg,relay,stub}.rs` |
| C3 | **connector `Cargo.toml` 零改动**：probe 只用既有 `[dependencies]`（tokio/time/uuid/serde_json/anyhow/reqwest） | diff 为空 + Cargo.lock 相对 HEAD 仅 +1 package |
| C4 | **零迁移**：无新 `migrations/NNNN_*.sql`；probe 全 DB-free（FakeOutbox），不依赖 B5-1 0239 落库；PG drill（`aero-audit-relay-drill`）原样 + 0239 文件存在性门控不变 | CI 条目在 0239 缺席时也必须全绿（现状即可验） |
| C5 | **B5-4 命名互斥**：本方向只加 `relay-probe`/`relay-mock`；`audit-provision-check`/`gate audit` 归 sibling。共享触点（`Gate_` match、`gate list` 串、`Completion_` cmds 串、test-integration.sh B5 段）集成时手接（AGENTS.md §4.1 多 agent 纪律） | §7 协调记录 + 集成 checklist |
| C6 | **退出码契约**：0/1/2 语义固定；aero-eng 侧直接 spawn（不经 `run_cmd`，其拍平非零为 1）；契约外码（panic 101）→ error 红 | R2 映射表 + CI 步骤 2/4 断言 |
| C7 | **`gate all` 不扩**；`gate list` 串与 `Gate_` 臂同步（两处都改，漏一处 = 文档/行为漂移） | 冒烟 `aero-eng gate list` 含 relay-mock |
| C8 | **假时钟纪律**：场景内除 `set_now` 推进外零 wall-clock；`transient_timeout` 是唯一真 sleep（10× 余量） | 套件时长断言 <15s |
| C9 | 新 env 仅可选 `AERO_RELAY_PROBE_BIN`（CLI harness 旋钮，不进 server 配置面） | 文档标注 |

## 4. 失败模式与缓解（设计自身的失效面）

| # | 失败模式 | 症状 | 缓解 |
|---|---|---|---|
| F1 | **vacuous green**（套件跑过但断言空转） | CI 绿但语义没钉住 | 每场景 ≥1 个 `FakeRowSnapshot` 全字段断言（D7）；CI 具名 9 行 PASS grep；probe 退出码由场景结果聚合而非「跑完即 0」 |
| F2 | **退出码拍平**（`run_cmd` 把 probe exit 1/2 都变 1） | `relay-probe` 与 gate 语义混淆 | 直 `tokio::process::Command` spawn（C6）；`Outcome::warning(2)` 走 `execute_with_ctx` Err 分支穿透（main.rs :32-43） |
| F3 | **假时钟钉死导致二次 dispatch 空转**（`available_at > now` 不可 claim） | 场景 3/4/5/6 第二次 dispatch 返回 0 行 → 断言崩 | D1 推导：每次重试前显式 `set_now` 推进到上一 requeue 的 available_at（S3/4/5/6 第二次 = `t0+1s`；S6 若加第三次 = `t0+3s`）；requeue 结果 = 推进后时钟 + backoff(attempts)，勿用序号外推 |
| F4 | **payload 误入 PayloadGuard 永久支**（种子 payload 缺 `source_system` 或带 `tenant_id`） | 所有场景错分类为永久支，矩阵全红 | D4：probe 种子 payload 模板统一带 `source_system="aero-im"` 且无 `tenant_id`；场景 5 的 `receipt_valid=false` 是唯一回执支，与 payload guard 正交 |
| F5 | **fencing 场景经 dispatch_batch 驱动**（relay 对 202 恒 settle，行永远到不了 Claimed 态可观测点） | 断言无法构造 stale token 窗口 | D2：场景 9 的 claim 直接 `repo.claim_due` 直驱（trait 全 pub，E7 允许）；dispatch_batch 只用于场景 1-7 |
| F6 | **StubSink 端口冲突 / 端口被占** | 场景启动失败 | `127.0.0.1:0` OS 临时端口（`StubSink::start` 已如此）；每场景独立 sink，互不干扰 |
| F7 | **probe 进程 panic（101）或信号死** | 退出码不在 {0,1,2} | aero-eng 映射：契约外码一律 `Outcome::error`（exit 1）→ CI 红 |
| F8 | **CI 编译陈旧二进制**（改了 probe 但条目跑旧产物） | 断言与实际代码脱节 | 条目步骤 1 先 `cargo build --bins`；`AERO_RELAY_PROBE_BIN` 仅当显式设置时跳过编译 |
| F9 | **deps 门回归**（其他切片动 ALLOWED_DEPS / 依赖） | `gate deps-native` 再红 | 修复是纯追加（2 行 + 2 行）；CI 步骤 6 常驻断言；提交前实跑 |
| F10 | **与 sibling B5-4 共享串冲突**（`gate list`、`Completion_` cmds、test-integration.sh B5 段同时改） | 合并冲突 / 漏一半 | 命令名互斥（C5）；集成时手接共享文件 + `cargo check --workspace`；本设计 §7 记录触点清单 |
| F11 | **transient_timeout 真 sleep 抖动**（CI 机器慢） | 套件超时/偶发失败 | 200ms timeout vs 2000ms delay = 10× 余量；套件总预算 <15s 含 2s sleep；其余场景零 wall-clock |
| F12 | **超时挂死**（sink 不响应、cargo run 卡编译） | `network relay-probe` 永不返回 | 120s 超时 + `child.kill()`（§2.2）；gate 侧 `b()` 300s 超时（`run_cmd` 自带 timeout） |

## 5. 迁移 / 落地步骤（每步独立可验）

> 无 DB 迁移、无运行时配置迁移（新 env 仅可选 harness 旋钮）。以下为代码落地序列 + 每步验证命令。

| 步 | 动作 | 验证 |
|---|---|---|
| S1 | 写 `aero-audit-relay-probe.rs`（§2.1 九场景 + 聚合退出码） | `cargo run -p aero-audit-connector --bin aero-audit-relay-probe; echo $?` → 9 行 PASS + exit 0；`echo $?` 复验 |
| S2 | `Network_` 增 `relay-probe` 臂 + help 文本（§2.2） | `cargo run -p aero-cli -- network relay-probe; echo $?` → 9 行 PASS + 0；`network help` 显示新行 |
| S3 | `scripts/relay-mock.sh` + `Gate_` `relay-mock` 臂 + `gate list` 串 | `cargo run -p aero-cli -- gate relay-mock; echo $?` → 0；`gate list` 含 relay-mock |
| S4 | `checks.rs` ALLOWED_DEPS 两行 + `dependency-check.sh` 两行（§2.4） | `cargo run -p aero-cli -- gate deps-native; echo $?` → **0**（S4 前实跑红 = 1 作基线，见 §1 账本）；`gate deps` → 0 |
| S5 | test-integration.sh 命名条目（§2.5 步骤 1-7） | `bash scripts/test-integration.sh` 条目段全绿（0239 缺席下同样全绿——无 DB 依赖） |
| S6 | 全门禁：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· no-touch 守卫（C2：diff 不含 connector `src/` 生产文件、migrations/、aero-ai、snaplink_commercial/） | 逐条绿；`git diff --stat` 审计 |
| S7 | 与 B5-2 connector 切片 + sibling B5-4 集成（§4.1 纪律）：手接共享文件后 `cargo check --workspace` 全绿 | 集成 checklist（§7） |

## 6. Testable acceptance mapping（AC → 机器断言 → CI 面 → 冗余防线）

> 断言公式的算术来源：D1（requeue = `now + audit_backoff(attempts)`）、D3（三 fence 相同）、relay.rs `deliver_claim`（Forbidden → attempt1 即 dead；Permanent → attempts<2 requeue / ≥2 dead；Transient → 恒 requeue）。

| AC（原文） | probe 场景（§2.1） | 精确机器断言 | CI 面（§2.5） | in-crate 冗余锚点（不动，仅引用） |
|---|---|---|---|---|
| AC1 403 → dead exactly once，无重试无再尝试 | `forbidden_403` | `FakeStatus::Dead`、attempts==1、posts()==1、last_error 含 "403"、二次 dispatch 0 行（dead 不在 `status IN (0,1)` claimable 集） | 步骤 2/3 grep `probe: forbidden_403: PASS` + exit 0 | `forbidden_dead_on_first_attempt` |
| AC2 422/409/回执错 → dead ≤1 retry；transient → 指数退避 cap 300s | `permanent_422` / `permanent_409` / `receipt_mismatch` / `transient_500` / `transient_timeout` | 三永久支同终态机：attempt1 requeue（`available_at == t0+audit_backoff(1) == t0+1s`）→ `set_now(t0+1s)` 后 attempt2 `Dead`/attempts==2/posts()==2；transient 支 requeue ×2（`t0+1s`→`t0+3s`）永不 dead；纯函数 `audit_backoff(1..=11)==[1,2,4,8,16,32,64,128,256,300,300]` + `MAX_BACKOFF_SECONDS==300`；timeout 支（200ms vs 2000ms）→ Transient requeue | 步骤 2/3 grep 五场景 PASS 行 + exit 0 | `permanent_error_dead_after_exactly_two_attempts`（422/409/receipt 参数化）、`backoff_is_bounded_and_exponential` |
| AC3 lease > 2×timeout 不变量 + stale token 永不完成 | `lease_invariant` / `fencing_stale_token` | `check_lease_invariant(5s,12s)==Err`、`(5s,13s)==Ok`（边界 = `2×timeout+2s`，镜像 config.rs:248）；fencing：`repo.claim_due` 直驱 claim A → 时钟推进过 lease + `make_due_now` → claim B（attempts==2）→ `settle(id,A)==false`、`requeue(id,A,2,..)==false`、`mark_dead(id,A,2,..)==false`、`settle(id,B)==true`（stale token 永不改写行） | 步骤 2/3 grep 两场景 PASS 行 + exit 0 | `stale_token_cannot_ack_after_reclaim`、`skew_gt_lease_cannot_livelock_claim_fence_settle`、config.rs :235-239 |
| AC4 `gate relay-mock` 跑套件 + connector 零新第三方依赖 | 门面（§2.3/§2.4） | `gate relay-mock` exit 0（`b()` 非零拍平 1）；`gate deps-native`/`gate deps` 转绿（修 E9 存量红：ALLOWED_DEPS += aero-cli/aero-audit-connector）；`cargo tree --depth 1` 直接依赖 ∈ {(workspace), base64 v0.22.x}；`Cargo.lock` 相对 HEAD 仅 +1 package | 步骤 5/6/7 exit 0 断言 | `cargo test -p aero-audit-connector --all-targets`（24 passed 基线，§1 账本实跑） |

**验收面自证非 vacuous**：probe 退出码由场景聚合（任一 FAIL → 1）；CI 步骤 3 对 9 个具名 PASS 行逐一 grep（缺行 = fail）；每个场景至少一个 `FakeRowSnapshot` 全字段断言——「跑过」与「断言过」不可分离。

## 7. 风险 / 决策点 / 协调记录

- **[PROPOSED] 契约值钉法**（proposal :13）：300s cap、dead ≤1、403→dead attempt1 为 §1.2 未验证值——probe 测试钉为仓内规范性读法，且每个数值都有仓内同值先例（`event_outbox.rs:18`、`snaplink_commercial.rs:18`、config.rs:248、`bot_delivery_outbox.rs`）。契约改判只动 `audit_backoff` 常量 + 场景 6 断言表，结构不动。
- **D2 决策（fencing 场景直驱 repo）**：`dispatch_batch` 对 202 恒 settle、失败恒 requeue/dead（释放 token）——Claimed 态在 dispatch 路径不可观测；场景 9 用 `OutboxRepo::claim_due` 直驱是唯一能构造 stale-token 窗口的方式。与 in-crate `stale_token_cannot_ack_after_reclaim` 同构（该测试同样直驱 repo），probe 的黑盒增量在「真 client + 真 loopback HTTP + 进程级退出码」面，不在 claim 驱动方式。
- **探测引擎落家 = connector bin 而非 aero-cli 代码**：aero-cli 零 DB 约束（E1）禁止链接 sqlx/reqwest；`network relay-probe` = 直 spawn（C6 保码）。方向标题的 "command in aero-cli" 以命令/门/CI 接线兑现。
- **`mock-url` 语义**：可选参数的 TCP dial 只做「外部部署 mock sink 可达性」预检；分支矩阵本体自含（行状态可观测需要进程内 `FakeOutbox` snapshot 面——外部 sink 暴露不了 attempts/available_at/claim_token）。
- **与 sibling B5-4 共享触点（集成 checklist）**：`Gate_` match 臂、`gate list` 串 :113、`Completion_` cmds 串、test-integration.sh B5 段——两个 direction 都触碰；命令名互斥（`relay-mock` vs `audit`、`relay-probe` vs `audit-provision-check`）；集成时手接共享文件（AGENTS.md §4.1），合后 `cargo check --workspace`。`gate all` 两者都不并入（决策同 B5-4 §7）。
- **E9 存量红属 AC4 修复面**：修复为纯追加（ALLOWED_DEPS 两行 + dependency-check.sh 两行），不碰其他条目；基线红已在 §1 账本实跑留证。
- **工作树状态**：connector + aero-ai（B5-1）+ test-integration.sh 改动均未提交（在途切片）；本 direction 的 bin/命令/脚本与 B5-2 connector 切片一并提交，不碰 B5-1 的 aero-ai 文件。
- **无 DB 前提**：probe 不依赖 0239，当前即可全绿（S1-S5 在 0239 缺席下全绿）；PG drill 保持 0239 门控（现状不变）。

## 8. 与 requirements 的差异说明（req → design 落定）

| req 表述 | design 落定 | 理由 |
|---|---|---|
| R1「再次 dispatch → Dead、attempts==2」 | 显式 `set_now(t0+1s)` 推进后再 dispatch | D1：假时钟下不推进则 `available_at > now`，claim 空转（F3） |
| R1 fencing「再 claim（token B）」 | claim 直驱 `repo.claim_due`，非 dispatch_batch | D2/F5：dispatch 路径 Claimed 态不可观测 |
| R2「mock-url dial」 | 手工字符串解析（无 url crate） | D6/C1：aero-cli 零新依赖 |
| R2 退出码 | 契约外码（101/信号）一律 error | F7：panic 不得静默变绿 |
| R5 步骤 3 具名 grep | 9 行逐一断言（含 `forbidden_403` 等全名） | F1：防 vacuous green |
| R4 依赖面断言 | `cargo tree --depth 1` ∈ {(workspace), base64} | E6：lockfile 相对 HEAD 仅 +1 package 已验 |
