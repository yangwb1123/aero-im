# Requirements Spec — land the `aero-audit-relay-probe` bin（激活 pre-wired B5-2 probe seam）

- **Module (analysis root)**: `crates/aero-cli` — aero-eng 工程 CLI；**唯一代码交付物落家 `crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs`（新 bin）**
- **Direction**: "Land the aero-audit-relay-probe bin to activate the pre-wired B5-2 probe seam"（value 9 / risk_reduction 8 / effort 7 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-cli-e99ec77a.json`（direction #1 of 3；#2 deps-audit 已在工作树落地，见 §2；#3 B5-3/B5-4 面不在本方向）
- **上游设计（已存在，本方向直接消费）**: `docs/design/2026-08-07-aero-cli-b5-2-relay-probe.design.md`（§2.1 场景矩阵 + §6 AC 映射）；sibling `docs/requirements/2026-08-07-aero-cli-b5-2-relay-probe.req.md`（原全切片 spec，R1-R6；本方向是其收窄重定界：**只交付 probe bin**，其余 seam 已预埋/已落地）
- **Campaign**: `aero-im-b5-outbox-relay`；contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（B5-2 行 :9、[PROPOSED] :13）
- **Status**: Requirements（下述证据全部经源码实读/实跑核对，核对日期 2026-08-08）
- **行号纪律**：行号是核对时锚点、会漂移（AGENTS.md §0）——**文件/符号才是稳定 grep 锚点**

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| V1 | `crates/aero-cli/src/main.rs` `Network_` 的 `relay-probe` 臂 pre-wired：直 spawn `aero-audit-relay-probe` bin、exit-code 透传 | ✅ **实读**。`"relay-probe"` 臂 :428-480：`AERO_RELAY_PROBE_BIN` env 覆盖（缺省 `cargo run --quiet -p aero-audit-connector --bin aero-audit-relay-probe`）；`stdout/stderr` inherit；`timeout(120s)` + `child.kill()`；码映射 `0→ok` / `1→error` / `2→warning(2)` / **契约外（panic 101、信号）→ error**。help 文本 :348 已含 `relay-probe [mock-url]`。`main.rs` :32-43 `Err(r) → std::process::exit(r.exit_code())`（warning 码穿透）；`aero-eng/src/outcome.rs:47` `Outcome::warning(exit_code, msg)` 存在；`aero-eng/src/run.rs` `run_cmd` 非零拍平为 1（:15-41）——**直 spawn 保 distinct 码的必要性成立** |
| V2 | `scripts/test-integration.sh:556-573` harness 命名 leg `relay-mock-probe`，file-gate + grep 恰 9 行 `probe: <name>: PASS` | ✅ **实读**（leg 现于 :558-580，行号微漂，符号稳定）：`:560` file-gate `[ -f crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs ]`；`:565` `grep -c '^probe: .*: PASS$'` 恒等 9；PASS/FAIL/SKIP 三 verdict（`:566/:570/:576/:580`，SKIP 文案 = "B5-2 relay probe not landed"）；`:590` relay_legs 计数含 `B5-CHECK relay-mock-probe: PASS`。**leg 名已入 `scripts/b5-pin.sh:43` 的 37/37 pin 槽位**（G6 总装门）——bin 落地即从 SKIP 转 PASS，无需改 harness |
| V3 | `crates/aero-audit-connector/src/bin/` 缺 `aero-audit-relay-probe.rs`，仅三 drill | ✅ **实读**：`src/bin/` = `aero-audit-priority-drill.rs` / `aero-audit-relay-drill.rs` / `aero-audit-t11-drill.rs`，**无 probe bin**。**实跑取证**：`cargo run --quiet -p aero-cli -- network relay-probe` → exit **1**，stderr `relay probe exited abnormally: exit status: 101`（cargo 找不到 bin target，列出的 available targets 恰为三 drill）。⚠️ 与 direction 问题陈述的 'cannot launch relay probe' 文案不同（该文案只在 `cmd.spawn()` 本身失败时触发，如 `AERO_RELAY_PROBE_BIN` 指向不存在路径）——**死 seam 结论一致（exit 1），文案以实跑为准** |
| V4 | connector 全 pub API 可驱动（FakeOutbox/StubSink/AuditRelay/AuditClient），零 src/ 改动 | ✅ **实读全部签名**：`fake.rs` `FakeOutbox::{new, set_now:111, insert, insert_lane, make_due_now:164, row:175}`；`FakeRowSnapshot` 全字段 pub（status/available_at/attempts/claim_token/lease_expires_at/last_error/priority/class）；`FakeStatus::code()` 0-3（Ready/Claimed/Delivered/Dead，镜像 0239）；`stub.rs` `StubSink::{start:133(127.0.0.1:0), token_url:158, events_url:163, posts:186, set_behavior:190, shutdown:197}` + `SinkBehavior` 全字段（含 `events_status`/`receipt_valid`/`delay_ms`）；`relay.rs` `AuditRelay::{new:101, dispatch_batch:164, deliver_claim:160}`、`audit_backoff:56`、`MAX_BACKOFF_SECONDS=300:33`、`PERMANENT_DEAD_AT=2:42`、`is_dead_at:48`；`client.rs` `AuditClient::{new:134, deliver:169}`；`config.rs` `RelayConfig` 全 pub 字段 + `check_lease_invariant:143`；`outbox.rs` `OutboxRepo` trait 全 pub（claim_due/settle/requeue/mark_dead/reconcile）。**`cargo check -p aero-audit-connector --all-targets` 实跑 ✅ 干净**（12.7s）——bin 有可直接构建的基线 |
| V5 | 状态机语义（403→dead attempt1；422/409/receipt→dead ≤1；transient→退避永不 dead；lease > 2×timeout+2s；stale token fencing） | ✅ **实读**：`client.rs:169-240` 分类——403→`Forbidden`；422→`Permanent(Unprocessable)`；409→`Permanent(Conflict)`；回执无效→`Permanent(ReceiptMismatch)`；payload guard→`Permanent(PayloadGuard)`；5xx/其他 4xx/传输错→`Transient`。`relay.rs:206-260` `deliver_claim`：Ok→settle；**Forbidden→无条件 mark_dead（T-11 fail-closed，注释明言不经 retry budget）**；Permanent→`is_dead_at(attempts)`（attempts≥2）mark_dead / 否则 requeue（**attempt1 requeue、attempt2 dead = dead ≤1 retry**）；Transient→恒 requeue。`config.rs:143` `check_lease_invariant`：`delivery_lease <= request_timeout*2 + 2s` → bail——**(5s,12s) Err（12≤12）、(5s,13s) Ok（13>12）** 精确成立。`audit_backoff(1..=11) = [1,2,4,8,16,32,64,128,256,300,300]`（`2^(n-1)` cap 300，attempts 9/10/11 → 256/300/300）✅ |
| V6 | in-crate 测试锚点 `tests/state_machine.rs` | ✅ **实读**（⚠️ 路径修正：在 **`crates/aero-audit-connector/tests/state_machine.rs`**，非 aero-cli/tests——aero-cli 无 tests/ 目录）：现有 **8** 个测试（原 spec 记 6，后增 `priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane`、`signature_rejected_dead_after_exactly_two_attempts`），含全部 6 个引用锚点：`stale_token_cannot_ack_after_reclaim` / `backoff_is_bounded_and_exponential` / `permanent_error_dead_after_exactly_two_attempts`（422/409/receipt 参数化）/ `forbidden_dead_on_first_attempt` / `happy_path_settles_and_removes_from_claimable` / `skew_gt_lease_cannot_livelock_claim_fence_settle`。`:36-82` 的 `claim_payload`/`test_config`/`relay` helper 即现成装配模板 |
| V7 | 设计文档与需求文档存在 | ✅ `docs/design/2026-08-07-aero-cli-b5-2-relay-probe.design.md`（九场景矩阵 §2.1、AC 映射 §6）与 `docs/requirements/2026-08-07-aero-cli-b5-2-relay-probe.req.md` 均在仓内；本 spec 的断言公式与 D1-D3 推导逐条复验（见 §4 矩阵） |
| V8 | **D1 假时钟算术**：claim 时 `attempts += 1`；requeue 设 `available_at = now + audit_backoff(attempts)`（attempts = 本次 claim 值） | ✅ **实读** `fake.rs:197-249`（claim_due：`row.attempts += 1`、lease 铸在假时钟上、claimable = status∈{Ready,Claimed} ∧ `available_at <= now` ∧ lease 过期）+ `fake.rs:270-298`（requeue：`available_at = now + audit_backoff(attempts)`）。→ attempt1 requeue = `t0+backoff(1) = t0+1s`；`set_now(t0+1s)` 后 claimable；attempt2 requeue = `t0+3s`。**第二次 dispatch 前必须 `set_now` 推进**（F3，否则 `available_at > now` 空转） |
| V9 | **D3 三 fence 同构**：settle/requeue/mark_dead 均栅栏于 `status==Claimed ∧ claim_token==Some(token) ∧ lease 未过`（requeue/mark_dead 另验 attempts） | ✅ **实读** `fake.rs:251-330` 三方法原文。→ stale token A 在 B 重领后全部 `false`；`make_due_now` 清 token/lease + `available_at=now` 使能重领 |
| V10 | **D2**：relay 对 202 恒 settle、失败恒 requeue/mark_dead——行不可能经 dispatch_batch 停留在 Claimed 态，fencing 场景须 `repo.claim_due` 直驱 | ✅ **实读** `relay.rs:206-260`（Ok→settle 单臂）；`OutboxRepo` trait 全 pub（`outbox.rs:59`）→ 直驱合法，且与 in-crate `stale_token_cannot_ack_after_reclaim` 同构（该测试同样直驱 repo） |
| V11 | **D4 payload guard**：payload 不得含 `tenant_id`，`source_system` 必须 == `config.source_system` | ✅ **实读** `client.rs:500-511` `validate_delivery_payload` 原文。→ probe 种子 payload 模板必须自带匹配 `source_system` 且无 `tenant_id`，否则全场景误入 PayloadGuard 永久支 |
| V12 | **D5 claims 契约**：`SinkBehavior.token_claims` 与 `RelayConfig.expected_*` 必须同值（`stub.rs:233-240` make_jwt 原样嵌入）；`jwks_uri=None` ⇒ 签名校验关闭（`config.rs` RelayConfig 字段注释） | ✅ **实读**。→ probe 常量自洽即过；in-crate `tests/state_machine.rs:43-62` `test_config`（`source_system="aero-im.source"`、`expected_iss="https://idp.example.test"` 等）是现成模板 |
| V13 | 零 connector src/ 与 Cargo.toml 改动（E7）；aero-cli 零新依赖 | ✅ **实读**：connector `Cargo.toml` `[dependencies]` 已含 tokio/time/uuid/serde_json/anyhow/reqwest/serde/thiserror（bin 用尽）；root `Cargo.toml:80` tokio `features=["full"]`（含 process）→ aero-cli `Cargo.toml` 不需动；`main.rs` 头注释 "engineering commands only (no DB dependencies)" |
| V14 | deps 审计（原 spec R4 / analysis direction #2） | ✅ **已落地（工作树未提交）——本方向只 verify 不交付**：`crates/aero-eng/src/checks.rs:257` `("aero-cli", &["aero-eng"])`、`:258` `("aero-audit-connector", &["aero-common","aero-auth"])` + :801-812 回归测试（断言 whitelist 与真实 deps 精确匹配）；`scripts/dependency-check.sh:63-64` 两行 `check_deps` 已加 |
| V15 | 共享触点（sibling B5-4） | ✅ **实读**：`main.rs:31` `AuditProvisionCheck_` 已注册（B5-4 CLI 已落地）；test-integration.sh :250-305/:369-445 B5-4 legs 均 presence-gated（help grep）；命令面互斥成立（`relay-probe`/`relay-mock-probe` vs `audit-provision-check`/`audit`）。⚠️ **`Gate_` 现无 `relay-mock` 臂、`scripts/relay-mock.sh` 不存在**（原 spec R3）——**不在本 direction 验收面内**（§3 出范围） |

### 1.1 核对偏差汇总（原文档行号/文案 vs 实况）

| 项 | 原文档 | 实况（2026-08-08） | 影响 |
|---|---|---|---|
| leg 行号 | test-integration.sh :556-573 | :558-580 | 无（符号 `relay-mock-probe` 稳定） |
| `check_lease_invariant` 行号 | design :112-122 | `config.rs:143` | 无（符号稳定） |
| `client.rs deliver` 行号 | req E7 :120 | :169 | 无 |
| 失败文案 | 'cannot launch relay probe' | `relay probe exited abnormally: exit status: 101`（exit 1） | 死 seam 结论不变；验收以 exit 码为准 |
| state_machine.rs 测试数 | req E8 "6 测试" | 8 测试 | 无（锚点名全部仍在） |
| deps 审计 | req R4「待加 2 行」 | 已落地（checks.rs:257-258 + dependency-check.sh:63-64） | **本方向不交付、仅 verify** |
| probe 种子 `source_system` | design §2.1 `"aero-im"` | in-crate `test_config` 用 `"aero-im.source"` | 自洽即可；推荐镜像 in-crate 模板（§4.2） |

## 2. Verified current state

```
已预埋/已落地（工作树，未提交——git status 实证）：
a) Network_ relay-probe 臂  main.rs:428-480 —— 完整（spawn/超时/码映射/help），只差 bin 文件
b) harness leg               test-integration.sh:558-580 —— file-gate + 9 行 PASS grep + verdict；
                             b5-pin.sh:43 槽位已含 relay-mock-probe（37/37 pin）
c) 探测对象                  connector 状态机全实现 + 8+11 in-crate 测试 + cargo check 干净（V4/V5/V6）
d) deps 审计（AC4 后半）     checks.rs:257-258 + :801-812 回归 + dependency-check.sh:63-64 —— 已落地
e) sibling B5-4              main.rs AuditProvisionCheck_ 已注册；harness legs presence-gated

唯一缺失（本 direction 全部交付物）：
   crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs —— 不存在
   └─ 落地的直接效果：network relay-probe 从 exit 1（101 码）→ exit 0；
      harness leg 从 SKIP → PASS（9 行具名 grep 恒等成立）
```

## 3. Scope

**In scope（唯一代码交付物）**：
- `crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs`（**新 bin**，DB-free 黑盒探测套件）：真 `AuditRelay` + 真 `AuditClient`（reqwest 过 loopback HTTP）对 scripted `StubSink`，行状态断言用 `FakeOutbox`（钉死假时钟）——九场景（§4.3 矩阵）+ `probe: <name>: PASS|FAIL` 行 + 退出码契约（0/1/2）。**connector `src/` 生产代码与 `Cargo.toml` 零改动**（V4 全 pub 面已够）。
- **verify-only（不改动）**：`Network_` 臂、test-integration.sh leg、b5-pin 槽位、deps 审计行——落地后实跑验证即绿。

**Out of scope（明确不造，防 scope 扩张）**：
- **`Gate_` `relay-mock` 臂 + `scripts/relay-mock.sh`**（原 spec R3）：本 direction 验收面没有 gate 条目——harness leg 走 `network relay-probe` 已覆盖 CLI/CI 可见性；gate 属 sibling 面，勿顺手加。
- **deps 审计任何改动**（原 spec R4）：已落地（V14），只 verify。
- **connector 状态机 / client / config / pg 改动**（B5-2 本体已实现，只消费 pub API）。
- **B5-1（0239 DDL/enqueue）**：probe 用 `FakeOutbox` 完全 DB-free，不依赖 0239；PG drill（`aero-audit-relay-drill`/`aero-audit-t11-drill`/`aero-audit-priority-drill`）原样。
- **B5-4 `audit-provision-check` / `gate audit`**：已落地/兄弟面，不碰。
- **migrations/、aero-ai、aero-server、snaplink_commercial/、aero-cli 代码**：一律不碰（aero-cli `Cargo.toml` 零改动；main.rs 一行不改）。
- **真实 sink/IdP 联调**（仓外）：probe 定位 = 本地 mock 先行（proposal :13）。

## 4. Requirements

### R1 — probe bin `aero-audit-relay-probe`（新，唯一交付物）

`crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs`，`#[tokio::main]` 多线程 rt；依赖全部已在 `[dependencies]`（tokio/time/uuid/serde_json/anyhow/reqwest——**零 Cargo.toml 改动**）。

**CLI 契约**：
- 无参数（场景自含）；stdout 每场景一行 **`probe: <name>: PASS`**（PASS 行**不得带尾随 detail**——harness `^probe: .*: PASS$` 锚定行尾）或 `probe: <name>: FAIL <detail>`（FAIL 可带 detail，不会误匹配 PASS 正则）。
- **不 fail-fast**：九场景全跑完再聚合退出码（CI 具名 grep 需要全部行）。
- 退出码：`0` = 全 PASS；`1` = 任一 FAIL；`2` = usage。
- 可选 `[mock-url]` 参数**由 aero-eng 侧消费**（TCP dial 预检），bin 自身忽略多余参数即可（无需实现 mock-url 逻辑）。

**装配纪律**（每场景独立实例，互不共享状态；模板直接镜像 `tests/state_machine.rs:36-82`）：
1. `StubSink::start()`（127.0.0.1:0 临时端口）+ `sink.set_behavior(SinkBehavior { … })` 脚本化；
2. `FakeOutbox::new()` + `repo.set_now(Some(t0)).await` 钉死单时钟域；
3. `probe_config()`：`RelayConfig` 全 pub 字段直构——**`SinkBehavior.token_claims` 与 `expected_iss/aud/scope/sub` 必须同值**（V12）；**`source_system` 与种子 payload 的 `"source_system"` 必须同值且 payload 无 `tenant_id`**（V11）；`jwks_uri: None`（签名校验关闭）；推荐整段复制 in-crate `test_config`（`source_system="aero-im.source"`）防 D4/D5 漂移；
4. 种子：`repo.insert(event_id, json!({"event_id": id, "source_system": …, "class": "admin", "priority": 0, "payload": …}), t0)`（`insert` 默认 lane 即够）；
5. 驱动：`AuditRelay::new(repo.clone(), AuditClient::new(config.clone())?, config)` → `dispatch_batch()`；断言 `repo.row(EVENT_ID)` 的 `FakeRowSnapshot` 全字段 + `sink.posts()`。

### 4.3 场景矩阵（断言公式全部经 V8-V11 复验；`t0` = 场景钉死起点；每场景 ≥1 个可观察行状态断言）

| # | 场景 | scripted 行为 | 断言（`FakeRowSnapshot` / `posts()` / 直驱后态） |
|---|---|---|---|
| 1 | `happy_path` | 202 + 有效回执 | `Delivered`、attempts==1、posts()==1、claim_token/lease 为 None、last_error==None；二次 `dispatch_batch` 返回 Ok(0) |
| 2 | `forbidden_403` | `events_status=403` | `Dead`、attempts==1（**exactly once**）、posts()==1、last_error 含 `"403"`；二次 dispatch 0 行（dead 不在 claimable） |
| 3 | `permanent_422` | `events_status=422` | 首次 dispatch：`Ready`、attempts==1、`available_at == t0+audit_backoff(1) == t0+1s`、last_error 含 `"Unprocessable"`；**`set_now(t0+1s)` 后**二次 dispatch：`Dead`、attempts==2、posts()==2 |
| 4 | `permanent_409` | `events_status=409` | 同 3 参数化（last_error 含 `"Conflict"`；≤1 次重试后 dead） |
| 5 | `receipt_mismatch` | `receipt_valid=false` | 同 3/4 终态支（last_error 含 **`"ReceiptMismatch"`**——注意大小写，勿断言小写 "receipt"）；首次 `Ready`/attempts==1/`available_at==t0+1s`；`set_now(t0+1s)` 后 `Dead`/attempts==2/posts()==2 |
| 6 | `transient_500` | `events_status=500` | 首次：`Ready`、attempts==1、`available_at==t0+1s`；`set_now(t0+1s)` 后二次：`Ready`、attempts==2、`available_at==t0+3s`——**永不 dead**；纯函数断言 `audit_backoff(1..=11) == [1,2,4,8,16,32,64,128,256,300,300]` 且 `MAX_BACKOFF_SECONDS == 300` |
| 7 | `transient_timeout` | `delay_ms=2000`、`request_timeout=200ms` | `Transient`（transport）→ requeue：`Ready`、attempts==1、`available_at==t0+1s`、last_error 含 `"transport failed"`（`client.rs:617-620` transport_error 文案）、posts()==1；**唯一真 sleep 场景（2s，10× 余量）** |
| 8 | `lease_invariant` | 纯函数，无 sink | `check_lease_invariant(5s, 12s)` → **Err**、`(5s, 13s)` → **Ok**（边界 = `2×timeout+2s` = 12s，镜像 `config.rs:143` 拒绝式）；唯一无行状态断言场景（配置不变量，允许例外） |
| 9 | `fencing_stale_token` | 202 + 有效回执 | **claim 直驱 `repo.claim_due(lease, n)`（D2/V10）**：claim A → 快照 `{Claimed, attempts:1, token:Some(A), lease:Some(t0+lease)}`；`set_now(t0+lease+1s)` + `make_due_now` + 再 `claim_due` → token B、attempts==2（快照 `{Claimed, attempts:2, token:Some(B)}`）；**`settle(id,A)==false`、`requeue(id,A,2,..)==false`、`mark_dead(id,A,2,..)==false`**（attempts 传当前值 2 以隔离 token 栅栏；stale 永不改写行）；**`settle(id,B)==true`** |

**确定性纪律**：除场景 7（2s 真 sleep）外零 wall-clock 依赖；每次重试前显式 `set_now` 推进到上一 requeue 的 `available_at`（F3：场景 3/4/5/6 第二次 = `t0+1s`；场景 6 若加第三次 = `t0+3s`）；requeue 结果 = 推进后时钟 + `backoff(attempts)`，勿用序号外推；套件总时长 <15s（设计 F11/C8）。

**不重复实现 in-crate 测试**：probe 与 `tests/state_machine.rs` 断言同一状态机，但走「真 client + 真 loopback HTTP + 进程级退出码」黑盒面——互为冗余防线（design §6）。

### R2 — 约束（no-touch 守卫）

- **connector `src/` 生产代码零改动**：只新增 `src/bin/aero-audit-relay-probe.rs`；`git diff --stat` 不得含 `src/{client,config,fake,lib,outbox,pg,relay,stub}.rs`。
- **connector `Cargo.toml` 零改动**；**aero-cli `Cargo.toml` 零改动**；**`crates/aero-cli/src/main.rs` 零改动**（seam 已 pre-wired，一行不改）。
- **零迁移**、**零新第三方依赖**（probe 只用既有依赖；Cargo.lock 相对 HEAD 不得新增 package）。
- 新 env：仅可选 `AERO_RELAY_PROBE_BIN`（已存在于 main.rs 臂，harness 不设置；验收走默认 cargo 路径）。
- 提交前门禁：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）。

## 5. Acceptance checks（direction 原文保留，逐条 testable）

### AC1 — `cargo run -p aero-audit-connector --bin aero-audit-relay-probe` exits 0 and prints exactly 9 `probe: <name>: PASS` lines including forbidden_403 (Dead, attempts==1, posts()==1), permanent_422/permanent_409/receipt_mismatch (attempt1 requeue at t0+1s → set_now(t0+1s) → attempt2 Dead, attempts==2, posts()==2), transient_500/transient_timeout (requeue, never dead, audit_backoff(1..=11)==[1,2,4,8,16,32,64,128,256,300,300]), lease_invariant (5s/12s Err, 5s/13s Ok), fencing_stale_token (stale token settle/requeue/mark_dead all false, fresh token settles)

**测试面 = R1 场景矩阵逐行断言**。机器断言命令：
```bash
cargo run -p aero-audit-connector --bin aero-audit-relay-probe > /tmp/probe.out 2>&1
echo "exit=$?"                                   # 必须 0
grep -c '^probe: .*: PASS$' /tmp/probe.out       # 必须 9
for n in happy_path forbidden_403 permanent_422 permanent_409 receipt_mismatch \
         transient_500 transient_timeout lease_invariant fencing_stale_token; do
  grep -F "probe: $n: PASS" /tmp/probe.out >/dev/null || exit 1   # 9 个具名行逐一锚定（防 vacuous）
done
```
- 场景内断言（`FakeRowSnapshot` 全字段 + `posts()`）在 bin 内实现、任一 FAIL → `probe: <name>: FAIL <detail>` 行 + 退出码 1（fail-fast 禁止：九行全打印）。
- 断言细节按 §4.3 矩阵（含 `forbidden_403` Dead/attempts==1/posts()==1、三永久支 `set_now(t0+1s)` 后 attempts==2/posts()==2、`audit_backoff(1..=11)==[1,2,4,8,16,32,64,128,256,300,300]`、`lease_invariant` 5s/12s Err + 5s/13s Ok、`fencing_stale_token` 三 stale 操作全 false + fresh settle true）。

### AC2 — `cargo run -p aero-cli -- network relay-probe` exits 0

```bash
cargo run -p aero-cli -- network relay-probe; echo "exit=$?"   # 必须 0
```
- stdout/stderr 继承透传（bin 的 9 行 PASS 可见）；120s 超时 + kill 已在臂内；契约外码（panic/信号）→ exit 1（arm 已实现，不改）。

### AC3 — harness `gate b5` leg `relay-mock-probe` = PASS (not SKIP)

```bash
# 完整 harness（需 PG/Redis/NATS + throwaway 库纪律）：
bash scripts/test-integration.sh 2>&1 | grep -E '^B5-CHECK relay-mock-probe:'
# 必须输出:  B5-CHECK relay-mock-probe: PASS
# 不得输出:  B5-CHECK relay-mock-probe: SKIP (B5-2 relay probe not landed)
```
- leg 本身 DB-free（test-integration.sh:558-580）；file-gate 命中后跑 `cargo run --quiet -p aero-cli -- network relay-probe` 并 grep 9 行 PASS（AC1+AC2 的组合面）；`B5-CHECK relay-mock-probe: PASS` 计入 relay_legs（:590）且满足 b5-pin 37/37 槽位（b5-pin.sh:43）。

### AC4 — suite wall-clock <15s (only transient_timeout sleeps, 10x margin)

```bash
cargo build -p aero-audit-connector --bin aero-audit-relay-probe -q   # 排除编译时间
/usr/bin/time -f '%e' target/debug/aero-audit-relay-probe 2>&1 | tail -1   # 必须 < 15.0
```
- 唯一真 sleep = 场景 7（delay 2000ms vs timeout 200ms，10× 余量）；其余场景零 wall-clock（假时钟）；8 场景 × (loopback HTTP + 断言) 各 <1s 量级——<15s 预算宽裕（设计 F11）。

### AC5 — aero-audit-t11-drill (T-11 fail-closed, rows stay pending without relay) and moderation-priority-drill legs stay green

```bash
bash scripts/test-integration.sh 2>&1 | grep -E '^B5-CHECK (t11-fail-closed|moderation-priority-drill):'
# 两个 leg 必须均为 PASS 或带原因的显式 SKIP，禁止 FAIL/exit 1
```
- 现状（0239 未落地）：`t11-fail-closed` = `SKIP (0239 not landed)`（test-integration.sh:451）、`moderation-priority-drill` = `SKIP (priority/class not landed)`（:481）——显式 SKIP 即绿（b5-pin 纪律：SKIP-with-reason 算 handled）。
- 本 direction 不动这两个 bin、不动 0239 门控、不动共享代码——probe bin 是独立 target，无干扰路径；回归面 = harness 全绿 + `cargo test -p aero-audit-connector --all-targets`（既有 24 passed + 1 ignored 基线不回归）。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| 九场景黑盒矩阵（AC1 全部机器断言） | `crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs`（新 bin） | 自跑：`cargo run -p aero-audit-connector --bin aero-audit-relay-probe`（无 DB、loopback 仅） |
| CLI 组合面（AC2） | pre-wired `Network_` 臂（不改） | `cargo run -p aero-cli -- network relay-probe; echo $?` |
| harness leg（AC3/AC5） | pre-wired `scripts/test-integration.sh:558-580` + b5-pin.sh:43（不改） | `bash scripts/test-integration.sh`（`gate b5` = `b(f("test-integration.sh"), 1800)`） |
| 墙钟（AC4） | bin 内确定性纪律 | `/usr/bin/time` 实测量化 |
| 状态机语义冗余防线（不动，仅引用） | `crates/aero-audit-connector/tests/{state_machine,claim_validation}.rs` | `cargo test -p aero-audit-connector --all-targets` |

## 7. Risks / 决策点 / 差异说明

- **与上游 design/req 的关系**：本 direction 是原 spec 的**收窄重定界**——原 R2（Network_ 臂）/R5（harness leg）/R4（deps 审计）已预埋或已落地（V1/V2/V14），**唯一交付物 = probe bin**；原 R3（`gate relay-mock` + `relay-mock.sh`）**明确出范围**（不在 direction 验收面；harness leg 已提供 CLI/CI 可见性，勿顺手扩张）。design §2.2-2.4 的 L1/L2 层内容本 direction 一律不执行（除 verify）。
- **失败文案差异**（V3）：direction 问题陈述 'cannot launch relay probe' vs 实跑 `relay probe exited abnormally: exit status: 101`——验收只认 exit 码，文案不具约束力。
- **`receipt_mismatch` last_error 断言大小写**：`mark_dead` 写 `"audit delivery classified permanent: ReceiptMismatch"`——断言须用 `"ReceiptMismatch"`（大写 R），勿用小写 "receipt"（§4.3 场景 5 已钉）。
- **fencing 场景直驱 repo**（D2）：`dispatch_batch` 对 202 恒 settle、失败恒 requeue/dead——Claimed 态在 dispatch 路径不可观测；场景 9 用 `OutboxRepo::claim_due` 直驱是唯一能构造 stale-token 窗口的方式，与 in-crate `stale_token_cannot_ack_after_reclaim` 同构（V10）。
- **[PROPOSED] 契约值钉法**（proposal :13）：backoff cap 300s、dead ≤1、403→dead attempt1 为仓外未验证值——probe 钉为仓内规范性读法，每值都有仓内同值先例（`event_outbox.rs:18`、`snaplink_commercial.rs:18`、`config.rs:143`、`bot_delivery_outbox.rs`）；契约改判只动 `audit_backoff` 常量 + 场景 6 断言表。
- **工作树状态**：connector crate + main.rs + checks.rs + test-integration.sh 等改动均未提交（在途切片）；本 direction 只新增 bin 文件，与 B5-2 切片一并提交；**不碰 B5-1 的 aero-ai 文件、不碰 B5-4 已落地面**。
- **无 DB 前提**：probe 不依赖 0239，当前即可全绿（AC1/AC2/AC4 现在就能验）；harness AC3/AC5 在 0239 缺席下经显式 SKIP verdict 保持绿（V5 纪律）。
- **集成纪律**（AGENTS.md §4.1）：本 direction 无共享文件改动（单一新文件），无需 worktree 协调；合后 `cargo check --workspace` + 全门禁（R2）。

## 8. Sequencing

1. **写 bin**：`aero-audit-relay-probe.rs`（R1 九场景 + 退出码契约）→ `cargo run -p aero-audit-connector --bin aero-audit-relay-probe; echo $?` = 9 行 PASS + 0（AC1）。
2. **verify seam**：`cargo run -p aero-cli -- network relay-probe; echo $?` = 0（AC2）。
3. **verify harness**：`bash scripts/test-integration.sh` → `B5-CHECK relay-mock-probe: PASS`（AC3）+ t11/moderation-priority legs 不回归（AC5）。
4. **墙钟**：`/usr/bin/time` 复验 <15s（AC4）。
5. **门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· no-touch 守卫（**本 direction 的贡献 = 仅 +1 文件 `src/bin/aero-audit-relay-probe.rs`**；工作树既有的在途改动（main.rs/checks.rs/test-integration.sh 等）保持原样，不得再改）。
