# Design — land the `aero-audit-relay-probe` bin（激活 pre-wired B5-2 probe seam）

- **Module**: `crates/aero-cli`（aero-eng 工程 CLI）——**唯一代码交付物落家 `crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs`（新 bin，独立 target）**
- **上游**: `docs/requirements/2026-08-08-aero-cli-b5-2-relay-probe-bin-landing.req.md`（R1-R2、§4.3 场景矩阵、AC1-AC5）；`docs/design/2026-08-07-aero-cli-b5-2-relay-probe.design.md`（全切片设计，本方向消费其 §2.1 场景矩阵并收窄）
- **对象状态机**: `crates/aero-audit-connector/src/{relay,client,fake,stub,config,outbox}.rs`（已实现、已测——本设计零改动）
- **Status**: Design。证据核对日期 2026-08-08（全部实读/实跑，见 §1 账本）
- **行号纪律**: 行号是核对时锚点、会漂移——**文件/符号才是稳定 grep 锚点**（AGENTS.md §0）

## 0. 设计摘要（TL;DR）

单文件交付，零共享文件改动：

```
交付物  crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs（新 bin，唯一）
        └─ 真 AuditRelay + 真 AuditClient(reqwest) × loopback StubSink，
           行状态断言走 FakeOutbox（钉死假时钟）——9 场景，
           stdout 每场景一行 `probe: <name>: PASS|FAIL <detail>`，
           退出码 0=全 PASS / 1=任一 FAIL（不 fail-fast，9 行全打印）
        └─ 已预埋接线（本设计零改动，只 verify）：
           aero-cli main.rs Network_ relay-probe 臂（直 spawn + 120s 超时 + 码映射）
           test-integration.sh relay-mock-probe leg（file-gate + 9 行 PASS grep）
           b5-pin.sh relay-mock-probe 槽位（37/37 pin）
```

- **零生产代码改动**：connector `src/` 一行不动（只加 `src/bin/`）、connector `Cargo.toml` 零改动、aero-cli `Cargo.toml` 零改动、`main.rs` 零改动、harness 零改动、零迁移、零新第三方依赖（tokio/time/uuid/serde_json/anyhow/reqwest/aero-common 全在既有 `[dependencies]`）。
- **无 DB 前提**：probe 全 DB-free（`FakeOutbox`），不依赖 B5-1 0239 落库——**当前工作树即可全绿**。
- **bin 落地瞬间**：`network relay-probe` 从 exit 1（`relay probe exited abnormally: exit status: 101`，实跑取证）→ exit 0；harness leg 从 `SKIP (B5-2 relay probe not landed)` → `PASS`，37/37 pin 槽位 14/15 执行槽转绿。

## 1. 证据核对账本（untrusted evidence → 实读/实跑验证）

| Evidence claim | 验证结果 |
|---|---|
| `main.rs` `Network_` `"relay-probe"` 臂 fully pre-wired（直 spawn、`AERO_RELAY_PROBE_BIN` override、120s timeout+kill、码映射 0/1/2/other） | ✅ **实读**（main.rs :428-480 臂）：`AERO_RELAY_PROBE_BIN` env 覆盖（缺省 `cargo run --quiet -p aero-audit-connector --bin aero-audit-relay-probe`）；`stdout/stderr` inherit；`timeout(120s)` + `child.kill()`；映射 `0→ok / 1→error / 2→warning(2) / 其他→error`；help 文本含 `relay-probe [mock-url]` |
| `test-integration.sh:558-580` `relay-mock-probe` leg（file-gate + `grep -c '^probe: .*: PASS$'`==9 + PASS/FAIL/SKIP verdicts）；`b5-pin.sh` 槽位 | ✅ **实读**：file-gate `[ -f crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs ]`；`:565` grep 恒等 9；SKIP 文案 = `"B5-2 relay probe not landed"`；`:590` relay_legs 计数含 `B5-CHECK relay-mock-probe: PASS`。b5-pin.sh：`B5_CONTRACT_TEST_LIST` = **15 executed + 22 [PROPOSED] = 37 槽**，`relay-mock-probe` 在第 14 执行槽；`assert_b5_contract_pin` 强制 count==37 + 无重复 + 无 vacuous + verdict 证据 |
| `src/bin/` 仅 3 drill bins；实跑 `network relay-probe` → exit 1、`exit status: 101` | ✅ **实跑**（本设计核对时复跑）：`./target/debug/aero-eng network relay-probe` → stderr `error: no bin target named 'aero-audit-relay-probe' in 'aero-audit-connector' package` + `relay probe exited abnormally: exit status: 101`，**exit=1**。`src/bin/` = priority/relay/t11 三 drill，无 probe |
| connector 全 pub API（FakeOutbox/FakeRowSnapshot/StubSink/SinkBehavior/AuditRelay/AuditClient/RelayConfig/OutboxRepo）+ `cargo check --all-targets` 干净 | ✅ **实读全部签名** + **实跑** `cargo check -p aero-audit-connector --all-targets` 干净。`FakeOutbox::{new,set_now,insert,insert_lane,make_due_now,row}`；`FakeRowSnapshot` 全字段 pub；`StubSink::{start(127.0.0.1:0),token_url,events_url,jwks_url,posts,set_behavior,shutdown}`；`SinkBehavior` 全字段 + `Default`；`AuditRelay::{new,spawn,dispatch_batch,deliver_claim}`；`AuditClient::{new,with_key_provider,deliver}`；`RelayConfig` 全 pub 字段 + `check_lease_invariant`/`check_drain_invariant`；`OutboxRepo` trait 全 pub 方法 |
| 状态机语义（403→dead attempt1；Permanent→requeue@1/dead@2；Transient→never dead；`check_lease_invariant(5s,12s)` Err/(5s,13s) Ok；`audit_backoff(1..=11)==[1,2,4,8,16,32,64,128,256,300,300]`；claim 时 attempts+=1；requeue=now+backoff(attempts)；三 fence 同构） | ✅ **实读** relay.rs/client.rs/fake.rs/config.rs：`deliver_claim` Forbidden 臂 → 无条件 `mark_dead`（T-11，不经 retry budget）；Permanent 臂 → `is_dead_at(attempts)`（≥2 dead / 否则 requeue）；Transient 臂 → 恒 requeue。`fake.rs` claim_due `attempts += 1` + lease 铸在假时钟；requeue `available_at = now + audit_backoff(attempts)`；settle/requeue/mark_dead 三 fence 同构（status==Claimed ∧ token 匹配 ∧ requeue/mark_dead 另验 attempts ∧ lease 未过）。`audit_backoff` = `2^(attempts-1)` cap 300 → 序列 [1,2,4,8,16,32,64,128,256,300,300] 精确。`config.rs` 单测 `(5s,12s) Err`、`(5s,13s) Ok`（边界 = 2×timeout+2s = 12s） |
| `tests/state_machine.rs` 在 **aero-audit-connector/tests/**（非 aero-cli），**8** tests（非 6） | ✅ **实读**：connector/tests/ 下 `state_machine.rs`（8 个 `async fn` 测试，含全部 6 个原锚点 + `priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane` + `signature_rejected_dead_after_exactly_two_attempts`）+ `claim_validation.rs`（27 个 `#[test]` 类条目）。aero-cli 无 tests/ |
| deps 审计（analysis direction #2）**已落地** | ✅ **实读**：`checks.rs` `ALLOWED_DEPS` 已含 `("aero-cli", &["aero-eng"])`、`("aero-audit-connector", &["aero-common","aero-auth"])` + 回归测试；`dependency-check.sh` 已含两行 `check_deps`。**本方向只 verify 不交付** |
| sibling B5-4 已注册；`Gate_` 无 `relay-mock` 臂、`scripts/relay-mock.sh` 不存在——出范围 | ✅ **实读**：`AuditProvisionCheck_` 已注册（main.rs）；main.rs 全文无 `"relay-mock"` 臂；`scripts/relay-mock.sh` 不存在。**明确出范围**（harness leg 已提供 CLI/CI 可见性） |
| `receipt_mismatch` last_error 含 `"ReceiptMismatch"`（大写 R） | ✅ **实读** relay.rs Permanent 臂：`format!("audit delivery classified permanent: {kind:?}")`，`PermanentKind::ReceiptMismatch` 的 Debug 输出 = `ReceiptMismatch`（大写 R） |
| 零 connector src/、Cargo.toml、main.rs、harness 改动；AERO_RELAY_PROBE_BIN 是唯一新 env（且已存在） | ✅ **实读** connector `Cargo.toml` `[dependencies]`：tokio/time/uuid/serde_json/anyhow/reqwest/aero-common/aero-auth 全在，bin 用尽；`AERO_RELAY_PROBE_BIN` 仅在 main.rs 臂读取，harness 不设置 |

### 1.1 核对偏差汇总（evidence/原文档 vs 实况）

| 项 | 原文档/evidence | 实况（2026-08-08 复核） | 影响 |
|---|---|---|---|
| 失败文案 | 'cannot launch relay probe' | `relay probe exited abnormally: exit status: 101`（exit 1，实跑） | 死 seam 结论一致；验收只认 exit 码 |
| state_machine 测试数 | 6 | **8**（connector/tests/state_machine.rs） | 锚点名全部仍在，无影响 |
| deps 审计 | 「待加」 | 已落地（checks.rs + dependency-check.sh） | 本方向 verify-only |
| `SinkBehavior` 默认 claims | design §2.1 自定 `expected_iss="https://idp.test"` 等 | `SinkBehavior::default()` 的 token_claims 与 in-crate `test_config` 的 expected_* **逐值相等**（iss=`https://idp.example.test`/aud=`["audit-governance"]`/scope=`audit:event:write`/sub=`aero-im.source`） | probe 用 `SinkBehavior::default()` + 镜像 `test_config`（`source_system="aero-im.source"`）即满足 D5，**零 claims 覆盖** |
| probe 种子 `source_system` | design §2.1 `"aero-im"` | in-crate `test_config` 用 `"aero-im.source"` | 推荐镜像 in-crate 模板防 D4/D5 漂移 |

## 2. API 变更（逐文件、逐符号）

### 2.1 新 bin `crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs`（唯一交付物）

**CLI 契约**（与 pre-wired 臂的码映射精确对接）：
- 无参数（场景自含）；可选 `[mock-url]` 由 aero-eng 侧消费（TCP dial 预检），**bin 自身忽略多余参数**——不实现 mock-url 逻辑。
- stdout 每场景一行：`probe: <name>: PASS`（**PASS 行不得带尾随 detail**——harness `^probe: .*: PASS$` 锚定行尾）或 `probe: <name>: FAIL <detail>`。
- **不 fail-fast**：9 场景全跑完再聚合退出码（CI 具名 grep 需要全部行）。
- 退出码：`0` = 全 PASS；`1` = 任一 FAIL。`2`（usage）在 bin 内不可达（无参数消费），保留给 CLI 臂契约的映射面。

**代码结构**（每场景独立实例，互不共享状态；模板镜像 `tests/state_machine.rs:36-82` helper）：

```rust
//! B5-2 relay probe — DB-free black-box probe suite over the audit connector
//! state machine (mock sink instead of a global-state relay loop).
//! Contract: 9 scenarios, one `probe: <name>: PASS|FAIL <detail>` line each on
//! stdout; exit 0 = all PASS, 1 = any FAIL. Args are ignored (the aero-eng
//! CLI arm consumes an optional mock-url itself). Zero src/ changes.

use std::sync::Arc;
use std::time::Duration;

use aero_audit_connector::client::AuditClient;
use aero_audit_connector::config::{check_lease_invariant, RelayConfig};
use aero_audit_connector::fake::{FakeOutbox, FakeRowSnapshot, FakeStatus};
use aero_audit_connector::outbox::OutboxRepo;
use aero_audit_connector::relay::{audit_backoff, AuditRelay, MAX_BACKOFF_SECONDS};
use aero_audit_connector::stub::{SinkBehavior, StubSink};
use aero_common::AuditId;
use reqwest::Url;
use serde_json::json;
use time::{Duration as TimeDuration, OffsetDateTime};
use uuid::Uuid;

const LEASE: TimeDuration = TimeDuration::seconds(30); // 镜像 test_config.delivery_lease

type ScenarioResult = Result<(), String>;

#[tokio::main]
async fn main() {
    // 9 场景顺序执行（不 fail-fast），聚合退出码。
    let mut failed = 0_u32;
    macro_rules! probe {
        ($name:literal, $scenario:expr) => {
            match $scenario.await {
                Ok(()) => { println!("probe: {}: PASS", $name); }
                Err(detail) => { println!("probe: {}: FAIL {}", $name, detail); failed += 1; }
            }
        };
    }
    probe!("happy_path", happy_path());
    probe!("forbidden_403", forbidden_403());
    probe!("permanent_422", permanent_422());
    probe!("permanent_409", permanent_409());
    probe!("receipt_mismatch", receipt_mismatch());
    probe!("transient_500", transient_500());
    probe!("transient_timeout", transient_timeout());
    probe!("lease_invariant", lease_invariant());
    probe!("fencing_stale_token", fencing_stale_token());
    std::process::exit(if failed == 0 { 0 } else { 1 });
}

/// 装配模板（D4/D5：claims 同值、payload 无 tenant_id、source_system 匹配；
/// 镜像 in-crate test_config 防漂移）。
async fn probe_env(
    behavior: SinkBehavior,
    request_timeout: Duration,
) -> Result<(StubSink, FakeOutbox, AuditRelay, OffsetDateTime), String> {
    let sink = StubSink::start().await.map_err(|e| format!("start stub: {e}"))?;
    sink.set_behavior(behavior).await;
    let repo = Arc::new(FakeOutbox::new());
    let t0 = OffsetDateTime::now_utc();
    repo.set_now(Some(t0)).await;
    let config = RelayConfig {
        token_endpoint: Url::parse(&sink.token_url()).map_err(|e| format!("token URL: {e}"))?,
        events_url: Url::parse(&sink.events_url()).map_err(|e| format!("events URL: {e}"))?,
        resource: "audit-governance".into(),
        client_id: "drill-client".into(),
        client_secret: "0123456789abcdef0123456789abcdef".into(),
        expected_iss: "https://idp.example.test".into(),
        expected_aud: "audit-governance".into(),
        expected_scope: "audit:event:write".into(),
        expected_sub: "aero-im.source".into(),
        source_system: "aero-im.source".into(),
        request_timeout,
        delivery_lease: Duration::from_secs(30),
        poll_interval: Duration::from_secs(1),
        shutdown_drain: Duration::from_secs(2),
        batch_size: 100,
        concurrency: 4,
        jwks_uri: None, // JWKS-off：签名校验关闭（signature 面由 in-crate 测试覆盖）
    };
    let client = AuditClient::new(config.clone()).map_err(|e| format!("audit client: {e}"))?;
    let relay = AuditRelay::new(repo.clone(), client, config);
    Ok((sink, repo, relay, t0))
}

/// 种子 payload 模板（D4：无 tenant_id、source_system == config.source_system）。
fn seed_payload(event_id: Uuid) -> serde_json::Value {
    json!({
        "event_id": event_id.to_string(), // 0239 trigger 格式：hyphenated UUID
        "source_system": "aero-im.source",
    })
}

/// 断言助手：种子一行 → dispatch_batch → 返回 row snapshot。
/// 种子写法镜像 in-crate 模板（`tests/state_machine.rs`）：
/// `let id = Uuid::new_v4(); repo.insert(AuditId::from_uuid(id), seed_payload(id), t0).await;`
async fn dispatch_once(
    repo: &FakeOutbox, relay: &AuditRelay, event_id: AuditId, t0: OffsetDateTime,
) -> Result<FakeRowSnapshot, String> {
    relay.dispatch_batch().await.map_err(|e| format!("dispatch: {e}"))?;
    repo.row(event_id).await.ok_or_else(|| "row missing after dispatch".to_owned())
}
```

> `event_id` 由场景以 `let id = Uuid::new_v4(); let audit_id = AuditId::from_uuid(id);` 配对构造（D10：payload 内 `event_id` 保持 UUID 文本格式，claim id 是 `AuditId`——双形态 lockstep 见 §4 F10）。posts 计数由场景各自从 sink 读取。

**场景矩阵**（断言公式全部经 §1 复验；`t0` = 场景钉死起点；`backoff(n)` = `audit_backoff(n)`；**除场景 7 外零 wall-clock**）：

| # | 场景 | scripted 行为 | 断言（`FakeRowSnapshot` 全字段 / `posts()` / 直驱后态） |
|---|---|---|---|
| 1 | `happy_path` | `SinkBehavior::default()`（202 + 有效回执） | 首 dispatch：`Delivered`、attempts==1、`claim_token==None`、`lease_expires_at==None`、`last_error==None`、posts()==1；**二次 dispatch 返回 `Ok(0)`**（Delivered 不在 claimable 集） |
| 2 | `forbidden_403` | `events_status=403` | `Dead`、attempts==1（**exactly once**）、posts()==1（stub 收到 POST 后回 403）、last_error 含 `"403"`（relay 文案 `audit sink rejected the service identity (HTTP 403)`）；二次 dispatch `Ok(0)`（dead 不在 `status IN (0,1)` claimable 集） |
| 3 | `permanent_422` | `events_status=422` | 首 dispatch：`Ready`、attempts==1、`available_at == t0 + backoff(1) == t0+1s`、last_error 含 `"Unprocessable"`（`classified permanent: Unprocessable`）；**`set_now(t0+1s)` 推进后**（F3）二次 dispatch：`Dead`、attempts==2、posts()==2 |
| 4 | `permanent_409` | `events_status=409` | 同 3 参数化（last_error 含 `"Conflict"`；≤1 次重试后 dead） |
| 5 | `receipt_mismatch` | `receipt_valid=false` | 同 3/4 终态支：首 dispatch `Ready`/attempts==1/`available_at==t0+1s`/last_error 含 **`"ReceiptMismatch"`（大写 R，勿断言小写 "receipt"）**；`set_now(t0+1s)` 后二次 dispatch `Dead`/attempts==2/posts()==2 |
| 6 | `transient_500` | `events_status=500` | 首 dispatch：`Ready`、attempts==1、`available_at==t0+1s`；`set_now(t0+1s)` 后二次 dispatch：`Ready`、attempts==2、`available_at==(t0+1s)+backoff(2)==t0+3s`——**永不 dead**（状态始终非 Dead）；另纯函数断言：`audit_backoff(1..=11) == [1,2,4,8,16,32,64,128,256,300,300]` 且 `MAX_BACKOFF_SECONDS == 300` |
| 7 | `transient_timeout` | `delay_ms=2000`、`request_timeout=200ms` | 首 dispatch：`Ready`、attempts==1、`available_at==t0+1s`、last_error 含 `"transport failed"`（client.rs `transport_error` 文案 `audit connector HTTP transport failed: …`）、posts()==1（POST 已到 stub 才超时）——**唯一真 sleep 场景（stub 侧 2s，10× 余量）** |
| 8 | `lease_invariant` | 纯函数，无 sink | `check_lease_invariant(5s, 12s)` → **Err**（12 ≤ 2×5+2）；`check_lease_invariant(5s, 13s)` → **Ok**（13 > 12）——边界 = `2×timeout+2s`，镜像 config.rs 拒绝式。唯一无行状态断言场景（配置不变量，允许例外） |
| 9 | `fencing_stale_token` | `SinkBehavior::default()`；**claim 直驱 `repo.claim_due`（D2/V10，dispatch 路径 Claimed 态不可观测）** | ① claim A = `repo.claim_due(LEASE=30s, 10)` → 恰 1 claim，快照 `{Claimed, attempts:1, claim_token:Some(A), lease_expires_at:Some(t0+30s)}`；② `set_now(t0+31s)`（推进过 lease）+ `make_due_now(id)` + 再 `claim_due` → token B、attempts==2，快照 `{Claimed, attempts:2, claim_token:Some(B)}`；③ **stale 三操作全 false**：`settle(id,A)==false`、`requeue(id,A,2,"stale")==false`、`mark_dead(id,A,2,"stale")==false`（attempts 传当前值 2 以隔离 token 栅栏——stale 永不改写行）；④ `settle(id,B)==true` → 快照 `Delivered` |

**确定性纪律**：
- 每次重试前显式 `set_now` 推进到上一 requeue 的 `available_at`（F3：场景 3/4/5/6 第二次 = `t0+1s`；场景 6 若加第三次 = `t0+3s`）；requeue 结果 = 推进后时钟 + `backoff(本次 claim 的 attempts)`，**勿用序号外推**。
- 每场景独立 `StubSink`（127.0.0.1:0 临时端口）+ 独立 `FakeOutbox` + 独立 `RelayConfig`；场景结束 `sink.shutdown()`（错误路径进程即退，spawn 任务随进程消亡）。
- 场景 9 的 `requeue(id,A,2,..)` 中 attempts 必须传 **2**（当前行值），隔离 token 栅栏——传 1 会因 attempts 不匹配提前 false，无法证明 token 栅栏本身。
- 套件总时长 <15s（场景 7 的 2s 为最大单项；其余 8 场景 × (loopback HTTP + 断言) 各 <1s 量级）。

**不重复实现 in-crate 测试**：probe 与 `tests/state_machine.rs`/`claim_validation.rs` 断言同一状态机，但走「真 client + 真 loopback HTTP + 进程级退出码」黑盒面——互为冗余防线（in-crate 测试锚点不动，仅引用）。

### 2.2 已预埋接线（零改动，只 verify）

| 接线 | 位置 | 本方向动作 |
|---|---|---|
| `Network_` `"relay-probe"` 臂 | `crates/aero-cli/src/main.rs`（直 spawn、`AERO_RELAY_PROBE_BIN`、120s 超时、码映射） | 不改，落地后实跑验证 exit 0 |
| harness leg `relay-mock-probe` | `scripts/test-integration.sh`（file-gate + 9 行 PASS grep + verdict） | 不改，落地后从 SKIP 转 PASS |
| 37/37 pin 槽位 | `scripts/b5-pin.sh` `B5_CONTRACT_TEST_LIST` | 不改（槽位已含） |
| deps 审计 | `crates/aero-eng/src/checks.rs` + `scripts/dependency-check.sh` | verify-only，不改 |

## 3. 兼容性约束

| # | 约束 | 强制方式 |
|---|---|---|
| C1 | **唯一交付物**：只新增 `crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs` | `git diff --stat` 不得含 connector `src/{client,config,fake,lib,outbox,pg,relay,stub}.rs`、`main.rs`、任何 `scripts/`、任何 `Cargo.toml` |
| C2 | **connector `Cargo.toml` 零改动**：probe 只用既有 `[dependencies]`（tokio/time/uuid/serde_json/anyhow/reqwest/aero-common/aero-auth——§1 实读确认） | `git diff` 无 Cargo.toml；`Cargo.lock` 相对 HEAD 零变化 |
| C3 | **aero-cli 零改动**（main.rs 一行不改、`Cargo.toml` 不改——seam 已 pre-wired） | diff 为空 |
| C4 | **harness 零改动**（leg + pin 已 pre-wired） | diff 为空 |
| C5 | **零迁移、零 DB 依赖**：probe 全 DB-free（`FakeOutbox`），B5-1 0239 缺席下全绿 | 无新 `migrations/`；当前工作树即可验 AC1/AC2/AC4 |
| C6 | **输出契约**：PASS 行 `probe: <name>: PASS`（无尾随 detail）；FAIL 行 `probe: <name>: FAIL <detail>`（不会误匹配 PASS 正则）；退出码 0/1 | harness `^probe: .*: PASS$` 锚定 + 场景聚合退出码 |
| C7 | **新 env 仅可选 `AERO_RELAY_PROBE_BIN`**（main.rs 臂已读；harness 不设置；验收走默认 cargo 路径） | 文档标注 |
| C8 | **工程门禁**：`unsafe_code = "forbid"`（bin 内零 unsafe）、clippy pedantic 无新警告、文件尺寸 < 800 行 WARN（9 场景 + helper ≈ 600 行，保持断言助手紧凑） | 提交前 `cargo clippy --workspace --all-targets` + `scripts/file-size-check.sh` |
| C9 | **不扩张 sibling 面**：不建 `Gate_` `relay-mock` 臂、不建 `scripts/relay-mock.sh`（原 spec R3 出范围）、不碰 B5-4 `audit-provision-check` | 范围纪律（§7） |

## 4. 失败模式与缓解（设计自身的失效面）

| # | 失败模式 | 症状 | 缓解 |
|---|---|---|---|
| F1 | **vacuous green**（套件跑过但断言空转） | CI 绿但语义没钉住 | 每场景 ≥1 个 `FakeRowSnapshot` 全字段断言（场景 8 纯函数例外）；harness 对 9 个**具名** PASS 行逐一 `grep -F`；退出码由场景结果聚合而非「跑完即 0」 |
| F2 | **PASS 行尾随 detail**（`probe: happy_path: PASS ok`） | harness `^probe: .*: PASS$` grep 计数 <9 → leg FAIL | 输出契约 C6：PASS 分支只打印 `probe: {name}: PASS`，detail 只进 FAIL 分支 |
| F3 | **假时钟钉死导致二次 dispatch 空转**（`available_at > now` 不可 claim） | 场景 3/4/5/6 第二次 dispatch 返回 0 行 → 断言崩 | 每次重试前显式 `set_now` 推进到上一 requeue 的 `available_at`（§2.1 确定性纪律） |
| F4 | **payload 误入 PayloadGuard 永久支**（种子 payload 缺 `source_system` 或带 `tenant_id`） | 全场景错分类为永久支，矩阵全红 | D4：种子模板统一 `source_system="aero-im.source"` 且无 `tenant_id`；`receipt_valid=false` 是唯一回执支，与 payload guard 正交 |
| F5 | **fencing 场景经 dispatch_batch 驱动**（relay 对 202 恒 settle，Claimed 态不可观测） | 无法构造 stale-token 窗口 | D2：场景 9 claim 直接 `repo.claim_due` 直驱（trait 全 pub），与 in-crate `stale_token_cannot_ack_after_reclaim` 同构 |
| F6 | **StubSink 端口冲突 / 端口被占** | 场景启动失败 | `127.0.0.1:0` OS 临时端口（`StubSink::start` 已如此）；每场景独立 sink 互不干扰 |
| F7 | **probe 进程 panic（101）/ 信号死** | 退出码不在 {0,1} | CLI 臂契约外码一律 `Outcome::error`（exit 1）→ CI 红（臂已实现）；bin 内断言用 `Result<_, String>` 而非 panic |
| F8 | **`receipt_mismatch` 断言大小写漂移** | last_error 含 `ReceiptMismatch` 但断言写小写 → 假 FAIL | 断言钉 `"ReceiptMismatch"`（大写 R，§1 实读） |
| F9 | **墙钟抖动**（CI 机器慢） | 套件超时/偶发失败 | 场景 7：200ms timeout vs 2000ms delay = 10× 余量；其余场景零 wall-clock；总预算 <15s 含 2s sleep |
| F10 | **`Uuid`/`AuditId` 双形态错配**（payload 内 `event_id` 与 claim id 不 lockstep） | 回执校验 ReceiptMismatch 假红 | 镜像 in-crate 写法：`let id = Uuid::new_v4(); repo.insert(AuditId::from_uuid(id), json!({"event_id": id.to_string(), …}), t0)` |
| F11 | **超时挂死**（sink 不响应 / cargo run 卡编译） | `network relay-probe` 永不返回 | 120s 超时 + `child.kill()`（臂已实现，不改） |

## 5. 迁移 / 落地步骤（每步独立可验）

> 无 DB 迁移、无运行时配置迁移（新 env 仅可选 harness 旋钮且已存在）。以下为文件落地序列 + 每步验证命令。

| 步 | 动作 | 验证 |
|---|---|---|
| S1 | 写 `crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs`（§2.1 九场景 + 聚合退出码） | `cargo run -p aero-audit-connector --bin aero-audit-relay-probe; echo $?` → 9 行 `probe: …: PASS` + exit **0**；`grep -c '^probe: .*: PASS$'` == 9 |
| S2 | verify pre-wired CLI seam（不改） | `cargo run -p aero-cli -- network relay-probe; echo $?` → exit **0**（S1 前实跑基线 = exit 1 + `exit status: 101`，§1 留证） |
| S3 | verify harness leg（不改） | `bash scripts/test-integration.sh` → `B5-CHECK relay-mock-probe: PASS`（非 SKIP）；relay_legs ≥1；37/37 pin 全绿（需 PG/Redis/NATS + throwaway 库纪律） |
| S4 | 墙钟复验 | `cargo build -p aero-audit-connector --bin aero-audit-relay-probe -q` 后 `/usr/bin/time -f '%e' target/debug/aero-audit-relay-probe` → **< 15.0** |
| S5 | 全门禁 | `cargo check --workspace` · `cargo test --workspace --lib` · `cargo test -p aero-audit-connector --all-targets`（既有 24 passed + 1 ignored 基线不回归）· `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· no-touch 守卫（C1：`git diff --stat` 仅 +1 文件） |

## 6. Testable acceptance mapping（AC → 场景 → 机器断言 → CI 面 → 冗余防线）

> 断言公式的算术来源：D1（requeue = `now + audit_backoff(attempts)`，claim 时 attempts+=1）、D3（三 fence 同构）、relay.rs `deliver_claim`（Forbidden→attempt1 即 dead；Permanent→attempts<2 requeue / ≥2 dead；Transient→恒 requeue）、config.rs `check_lease_invariant`（`lease > 2×timeout+2s`）。

| AC（原文） | probe 场景 | 精确机器断言 | CI 面 | in-crate 冗余锚点（不动，仅引用） |
|---|---|---|---|---|
| AC1 — 直跑 bin：exit 0 + 恰 9 行 `probe: <name>: PASS`（含 forbidden_403 Dead/attempts==1/posts()==1；三永久支 attempt1 requeue@t0+1s → set_now(t0+1s) → attempt2 Dead/attempts==2/posts()==2；transient 永不 dead + `audit_backoff(1..=11)==[1,2,4,8,16,32,64,128,256,300,300]`；lease_invariant (5s,12s) Err/(5s,13s) Ok；fencing stale 三操作 false + fresh settle true） | 全部 9 场景 | ```cargo run -p aero-audit-connector --bin aero-audit-relay-probe > /tmp/probe.out 2>&1; echo "exit=$?"   # =0
grep -c '^probe: .*: PASS$' /tmp/probe.out                                    # =9
for n in happy_path forbidden_403 permanent_422 permanent_409 receipt_mismatch \
         transient_500 transient_timeout lease_invariant fencing_stale_token; do
  grep -F "probe: $n: PASS" /tmp/probe.out >/dev/null || exit 1                # 9 个具名行逐一锚定（防 vacuous）
done``` | 场景内断言任一 FAIL → `probe: <name>: FAIL <detail>` + exit 1（fail-fast 禁止） | `state_machine.rs` 8 测（`forbidden_dead_on_first_attempt` / `permanent_error_dead_after_exactly_two_attempts` / `backoff_is_bounded_and_exponential` / `stale_token_cannot_ack_after_reclaim` / `skew_gt_lease_cannot_livelock_claim_fence_settle` / `happy_path_settles_and_removes_from_claimable` 等）+ `config.rs` 单测 |
| AC2 — `cargo run -p aero-cli -- network relay-probe` exits 0 | 全部 9 场景（经 CLI 臂） | `cargo run -p aero-cli -- network relay-probe; echo "exit=$?"` → **0**；stdout/stderr 继承透传（9 行 PASS 可见） | 臂已实现（直 spawn 保 distinct 码 + 120s 超时 + 码映射）；S1 前基线 exit 1 已留证 | — |
| AC3 — harness `gate b5` leg `relay-mock-probe` = PASS（not SKIP） | file-gate 命中 → AC1+AC2 组合面 | `bash scripts/test-integration.sh 2>&1 \| grep -E '^B5-CHECK relay-mock-probe:'` → `B5-CHECK relay-mock-probe: PASS`；**不得**输出 `SKIP (B5-2 relay probe not landed)` | leg DB-free（test-integration.sh）；`B5-CHECK relay-mock-probe: PASS` 计入 relay_legs + 满足 b5-pin 37/37 槽位 | — |
| AC4 — suite wall-clock <15s（仅 transient_timeout 真 sleep，10× 余量） | 场景 7（2s）+ 其余零 wall-clock | `cargo build -p aero-audit-connector --bin aero-audit-relay-probe -q; /usr/bin/time -f '%e' target/debug/aero-audit-relay-probe 2>&1 \| tail -1` → **< 15.0** | 确定性纪律（假时钟推进，勿用序号外推） | — |
| AC5 — t11-fail-closed + moderation-priority-drill legs 不回归 | 无（独立 target，无共享代码） | `bash scripts/test-integration.sh 2>&1 \| grep -E '^B5-CHECK (t11-fail-closed\|moderation-priority-drill):'` → 两 leg 均 **PASS 或带原因的显式 SKIP**（现状：`SKIP (0239 not landed)` / `SKIP (priority/class not landed)`——SKIP-with-reason 算 handled，b5-pin 纪律），禁止 FAIL/exit 1 | probe bin 是独立 target，无干扰路径 | `cargo test -p aero-audit-connector --all-targets`（24 passed + 1 ignored 基线不回归） |

**验收面自证非 vacuous**：probe 退出码由 9 场景聚合（任一 FAIL → 1）；harness 对 9 个具名 PASS 行逐一 grep（缺行 = fail）；每场景 ≥1 个 `FakeRowSnapshot` 全字段断言——「跑过」与「断言过」不可分离。

## 7. 风险 / 决策点 / 范围记录

- **[PROPOSED] 契约值钉法**（proposal :13）：300s cap、dead ≤1、403→dead attempt1 为仓外未验证值——probe 钉为仓内规范性读法，每值都有仓内同值先例（`event_outbox.rs`、`snaplink_commercial.rs`、config.rs `check_lease_invariant`、`bot_delivery_outbox.rs`）。契约改判只动 `audit_backoff` 常量 + 场景 6 断言表，结构不动。
- **D2 决策（fencing 场景直驱 repo）**：`dispatch_batch` 对 202 恒 settle、失败恒 requeue/dead（释放 token）——Claimed 态在 dispatch 路径不可观测；场景 9 用 `OutboxRepo::claim_due` 直驱是唯一能构造 stale-token 窗口的方式，与 in-crate `stale_token_cannot_ack_after_reclaim` 同构。probe 的黑盒增量在「真 client + 真 loopback HTTP + 进程级退出码」面，不在 claim 驱动方式。
- **探测引擎落家 = connector bin 而非 aero-cli 代码**：aero-cli 零 DB 约束禁止链接 sqlx/reqwest；`network relay-probe` = 直 spawn（保 distinct 码）。方向标题的 "command in aero-cli" 以命令/CI 接线兑现。
- **`mock-url` 语义**：可选参数的 TCP dial 只做「外部部署 mock sink 可达性」预检（臂已实现）；分支矩阵本体自含——行状态可观测需要进程内 `FakeOutbox` snapshot 面，外部 sink 暴露不了 attempts/available_at/claim_token。
- **`SinkBehavior::default()` 即满足 D5**（§1.1 偏差表）：token_claims 与 `test_config` expected_* 逐值相等，probe 场景 1-7/9 零 claims 覆盖；`receipt_valid`/`events_status`/`delay_ms` 是仅需的覆盖字段。
- **工作树状态**：connector crate + main.rs + checks.rs + test-integration.sh 等改动均未提交（在途切片）；本方向只新增 bin 文件，与 B5-2 切片一并提交；**不碰 B5-1 的 aero-ai 文件、不碰 B5-4 已落地面**。
- **范围纪律（出范围，明确不造）**：`Gate_` `relay-mock` 臂 + `scripts/relay-mock.sh`（原 spec R3——harness leg 已提供 CLI/CI 可见性）；deps 审计任何改动（已落地，verify-only）；connector 状态机/client/config/pg 改动；B5-1（0239 DDL/enqueue）；B5-4 `audit-provision-check`；migrations/、aero-ai、aero-server、snaplink_commercial/、aero-cli 代码。
- **集成纪律**（AGENTS.md §4.1）：本方向无共享文件改动（单一新文件），无需 worktree 协调；合后 `cargo check --workspace` + 全门禁（S5）。
