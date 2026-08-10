# Adversarial review — B5-4 心跳新鲜度设计的并发/活性压力测试（settle-heartbeat 语义）

> 审查对象：`docs/design/2026-08-09-aero-audit-connector-b5-4-settle-heartbeat-rejection-point.design.md`（proposed）。
> 审查面：**并发与活性语义**——(1) settle Ok(true) 与 record_heartbeat 两个独立 DB round-trip 的崩溃窗口；(2) 多实例 last-writer-wins 与分区/陈旧实例假绿；(3) 三处年龄计算的单时钟域声明（含 `clock_timestamp()` vs 事务快照）；(4) 滚动升级矩阵未列交错；(5) gate 校验与配给签发间的 TOCTOU + 403-loop → mark_dead → 自愈因果链活性。
> 方法：逐条对照设计声明 × 已落地代码（`crates/aero-audit-connector/src/pg.rs` settle 事务、`relay.rs::deliver_claim`、`client.rs` deliver/receipt、`aero-eng/src/audit_provision.rs` verdict/run、`scripts/test-integration.sh` legs B/D）。行号会漂移，锚点一律用文件/符号。
> 结论：**设计核心不变量全部成立，无假绿路径；发现 2 个真实缺口（record 在投递关键路径上无超时、升级矩阵缺 server 轴）、2 个精度/配置分裂（Q6B 舍入、freshness 双 env）、1 个需文档化的运维依赖（dead 行无自动清扫 → 403 后 check 永红直至人工处置）**。

## §0 结论速览

| # | 问题 | 结论 | 缺口等级 |
|---|---|---|---|
| 1 | settle→record 崩溃窗口 | **只产生假闭，零假绿**；两趟 round-trip 的方向不可逆（record 仅在 settle Ok(true) 后） | 中：record 挂在投递关键路径上且无超时——**挂起**（非报错）会停摆整个 relay 循环（安全方向，活性损失） |
| 2 | 多实例 last-writer-wins / 分区陈旧实例 | **结构上无法假绿**：settle 需要 claim 新鲜 + sink 202+receipt + DB fence 三件事同时成立；DB 分区 = 天然分区检测器；陈旧实例的 fence 拒绝陈旧 claim | 低：心跳是身份无关的（证明「某个身份的 relay 投递过」，非「配给路径依赖的身份」）——被 per-row 403→dead 臂补偿，需文档化 |
| 3 | 单 DB 时钟域 | **三处年龄计算全部在 PG 时钟上成立**；`clock_timestamp()` 非快照冻结，即使包事务也正确 | 低×2：Q6B `::bigint` 舍入致边界 ≤0.5s 的 check/gate 分歧；freshness 是双进程 env（配置域分裂非时钟域）；单 PG 假设需记录 |
| 4 | 升级矩阵 | 2 轴（check×DB）矩阵**缺 server 轴**；两个未列行 = 新 check + 旧 server 的**假红窗口**（安全方向） | 中：部署顺序成为 load-bearing 约束（DB → server → check），矩阵须补第三轴 |
| 5 | TOCTOU + 403-loop 活性 | **无活锁**（因果图无环、每条边单调）；TOCTOU 有界于 freshness 且被三层补偿；403 后 check 永红直至人工清 dead 行（稳定停滞态，设计内） | 低：dead 行无自动清扫——人工补救流程须文档化 |

---

## §1 Q1 — settle Ok(true) 与 record_heartbeat 的崩溃窗口

### 1.1 结构（两趟独立 round-trip）

```
deliver_claim（relay.rs）:
  client.deliver → Ok(())        # POST 202 + validate_audit_receipt 通过（client.rs）
  → repo.settle(event_id, token) # pg.rs：事务内 fenced 重读 + UPDATE + COMMIT，rows_affected==1 才 Ok(true)
  → [装饰器] Ok(true) 时 recorder.record_heartbeat()  # UPSERT verified_at = clock_timestamp()
  → 返回 Ok(true)
```

关键事实（已核）：`client.deliver` 的 `Ok(())` = HTTP 202 **且** receipt 校验通过（`validate_audit_receipt`：`receipt.event_id` 值级匹配 claim + `tenant_id` 非空，client.rs）——所以 `Ok(true)` 不是「POST 发出去了」，而是「sink 确认接受了这条具体事件」。心跳的语义强度高于普通 liveness ping。

### 1.2 交错枚举（全部可区分状态）

| # | 交错 | 结果 | 方向 |
|---|---|---|---|
| A | settle COMMIT → 进程崩溃（record 前） | 行 status=2 已投递；心跳未刷新 → 门在 freshness 窗口后 fail-closed | **假闭**（安全） |
| B | settle COMMIT → record UPSERT 报错（DB 抖动） | `tracing::warn!` + settle 返回原值 `Ok(true)`；门保持 fail-closed 至下个成功 settle | **假闭**（安全） |
| C | settle COMMIT → record COMMIT → 崩溃 | 行已投递 + 心跳新鲜，两事实均真 | 一致 |
| D | record 先于 settle 可见 | **不可能**：record 仅在 settle 返回 Ok(true)（= commit 后）被调用；生产唯一调用点是装饰器 | — |
| E | 并发 settle（同/跨实例，concurrency ≤32） | 并发 UPSERT 在 singleton 行锁上串行，last-writer-wins；每个 writer 都是已验证投递者 | 一致 |
| F | settle 返回 Err 但服务端已 commit（commit-ack 丢失） | 无 record；行实际已投递。relay 警告「idempotent retry will recover」实为 no-op（status=2 不可再 claim）——**既有** at-least-once 伪影，非本设计引入 | 假闭（安全） |
| G | settle Ok(false)（租约丢失/陈旧 token） | 不记心跳（设计钉死） | 假闭（安全） |

### 1.3 结论

- **不变量「心跳 ⟹ ∃ fenced settle ⟹ ∃ sink-acked 投递」在全部交错下成立**——record 是 settle Ok(true) 的严格后继，无任何路径可无 settle 写心跳（生产调用点唯一：装饰器）。
- **反向方向（投递 ⟹ 心跳）不是不变量**——A/B 表明：settle 已 commit 的事件其心跳贡献永久丢失（status=2 行永不再 claim，无人为它补 record），门靠**后续**事件的 settle 自愈。崩溃循环（每次 settle 后、record 前崩溃）可让门在 relay 持续投递时保持关闭——与 §3 故障表「record 失败只让门保持 fail-closed 至下个 settle」一致，但**设计应明示此单向性**（交付成功但门关 = 假闭，非缺陷）。
- **真实缺口（中）**：`record_heartbeat` 的 await 位于 `deliver_claim` → `dispatch_batch` 的 `for_each_concurrent` 汇合点上——**整个批次等所有 settle+record 完成**。报错是 warn-only，但**挂起**（sqlx 查询无超时，pg.rs 现状即如此）会让 relay 循环停摆：不再 claim、不再投递、心跳过期 → fail-closed（安全方向，但这是**投递面**的活性损失，设计故障表只列了 record 报错、没列挂起）。**建议：装饰器内 `tokio::time::timeout` 包裹 record（如 poll_interval 或 5s），超时 = 不记心跳 = fail-closed，且不阻塞投递循环**。
- **次要**：singleton 行是全集群写热点（每 settle 一次行锁获取）。默认 300s 新鲜度只需每窗 ≥1 次 settle，全量 record 是过度供给。可选优化：per-process 节流（如 ≥1s 才记一次），非正确性项。

---

## §2 Q2 — 多实例 last-writer-wins：分区/陈旧实例能否假绿？

### 2.1 谁有资格写心跳？（settle 的隐含前置条件）

settle 成功需要三件事**同时**新鲜（pg.rs 已核）：

1. **有效 claim**：`claim_token` 匹配 + `status IN (0,1)` + `lease_expires_at > clock_timestamp()`（fence 全部 DB 侧）；
2. **sink 确认**：`client.deliver` 202 + receipt 校验通过；
3. **DB 可写**：fence + UPDATE + COMMIT 在同一事务内。

### 2.2 各类「坏实例」逐一推演

| 坏实例形态 | 能否刷新心跳？ | 机制 |
|---|---|---|
| DB 分区 | **否** | claim/settle 都要 DB——settle 的 DB round-trip 本身就是分区检测器 |
| sink 分区（DB 通、sink 不通） | **否** | claim 成功但 POST 失败 → requeue（transient，永不 dead）；租约 30s 过期 → 行被健康实例回收。**分区实例连「拖住行」都被租约界住** |
| 暂停后恢复（VM freeze / GC 停顿） | **否**（陈旧 claim） | 暂停 > 租约 → 恢复时 fence `lease_expires_at > now` 失败 → `Ok(false)` → 不记心跳。**必须重新 claim（新 token）+ 重新投递**——暂停前的旧工作无法铸成心跳 |
| runtime wedged（tokio 死锁/卡死） | **否** | POST 无法完成 → 无 settle；其占住的行租约过期后被回收 |
| 健康 replica（非 primary） | **是** | 它确实投递了——门绿**语义正确** |

### 2.3 结论

- **分区/陈旧/卡死实例在结构上无法保持门绿**——心跳不是活性 ping，是**投递收据**（fence + 202 + receipt 三件套）。「some relay is delivering」语义精确成立，且比字面更强：「**某个 relay 在最近 lease（30s 默认）内完成过 sink 确认的投递**」。
- primary 死了但 replica 在投递 → 门绿：**正确**——配给签发关心的只是「sink 接受投递」，不关心哪个进程。设计与语义一致。
- **残留语义缺口（低，需文档化）**：心跳**身份无关**——证明「某个配置身份的 relay 投递过」，不证明「配给路径所依赖的那个身份/实例投递过」。滚动升级期新旧 relay 并存（不同 client_secret）时，旧身份 settle 可保持门绿而新身份配给路径是坏的。补偿：per-row 403→dead 臂是身份特定的信号（实际身份每事件触发）。分层健全，但「心跳 ≠ 当前配给身份健康」应写入模块头注释（R8 契约旁）。
- 吞吐盲区（设计内）：每窗 1 次 settle 即绿——「有投递」≠「积压在排空」。arm 5 明示「积压属正常」+ `oldest-pending-age` 报告行给出运维信号。一致，无需改。
- 措辞勘误：设计 §3 行「任一无 fencing 的实例 settle 即证明…」——**每个 settle 都是 fenced 的**（`Ok(true)` 的必要条件），建议改为「任一 fenced settle」。

---

## §3 Q3 — 单 DB 时钟域：三处年龄计算

### 3.1 三处计算（全部已核）

| 面 | 表达式 | 位置 |
|---|---|---|
| 写入 | `verified_at = clock_timestamp()`（UPSERT） | `AuditRelayProvisionRepo::record_heartbeat`（设计） |
| 门的新鲜度判定 | `(verified_at + make_interval(secs => $1)) >= clock_timestamp()` | `provision_check`（设计） |
| check 年龄 | `extract(epoch FROM (clock_timestamp() - verified_at))::bigint` | Q6B（设计，Q4 同款模式） |

### 3.2 声明成立性

- **三处全部在同一个 PG server 时钟上求值**。relay 无 app 时钟（`OutboxRepo` trait 不收 `now`，pg.rs/outbox.rs 模块头双写）；gate 无 app 时钟（freshness 只做区间参数）；check 是纯 psql 子进程 SQL。✓
- **`clock_timestamp()` vs 事务快照**：`clock_timestamp()` 返回**调用瞬间的实际时间**，不被事务快照冻结（与 `now()`/`CURRENT_TIMESTAMP` 相反）。因此即使 UPSERT/SELECT 未来被包进事务，语义不变——单语句 autocommit 形态更是平凡正确。✓
- **边界自洽**：db_tests 钉的 `age==freshness → Verified` / `age==freshness+1 → Stale` 与 SQL 代数一致；`make_interval(secs => $1)` 纯秒区间 = 绝对时间算术，无 DST/月日分量问题。✓
- **负年龄统一**：时钟回拨 → `verified_at > now` → 门：`verified_at + interval >= now` → Verified；check：负 age ≤ freshness → fresh。两处一致（设计「负值按 fresh」成立）。✓

### 3.3 发现的两个分裂（均非时钟域破坏）

1. **Q6B 舍入分裂（≤0.5s 边界窗）**：`extract(epoch FROM interval)` 返回 numeric，`::bigint` 转换是**四舍五入**（PG numeric→int 取最近整数），不是 floor。age=300.4s 时：Q6B 报 300 → check 的 `age <= freshness`（300 ≤ 300）→ **fresh**；而门：300.4 > 300 → **Stale**。即 check 报 healthy 时门已拒——每跨一次边界有 ≤0.5s 的分歧窗（安全方向：门是执法者，check 是监控者，各自 fail-closed）。`::bigint` 本身是 psql 文本解析路径所必需（Q4 先例），**建议改为 `floor(extract(epoch FROM (...)))::bigint`**，使 check 与门在边界上同向（check 至多提前 1s 报 stale，永不晚报）。
2. **freshness 双 env 配置分裂**：server gate 读 server 进程 env，check 读 check 进程 env——**默认值镜像 ≠ 部署值一致**。server 配 600s、check 配 300s（或反之）时两者判定永久分歧且无检测。这是配置域分裂（非时钟域），但运维上会制造「check 红/门绿」的困惑信号。**建议**：check 的 reason 字符串已含 `{freshness}`（设计如此）——补一条文档不变量「freshness 必须全部署一致」，并考虑让 check 报告行显式输出所用 freshness 值。
3. **单 PG 假设**：时钟域声明要求恰好一个 PG。若引入读副本/落后 standby 接入 gate 读路径 → 读到旧 `verified_at` → 假闭（安全方向，永不假绿）。文档化该假设。

---

## §4 Q4 — 滚动升级矩阵：缺 server 轴

### 4.1 设计矩阵（2 轴）→ 实际系统 3 轴

系统有三个独立二进制/迁移面：**check**（aero-cli）、**server**（relay 写心跳 + gate）、**DB**（0246 迁移）。设计 §2.3 矩阵只有 check×DB 两轴，**server 轴未列**。基线（relay on、bindings>0、dead=0、零 undelivered）全枚举：

| check | server | DB | 结果 | 状态 |
|---|---|---|---|---|
| 旧 | 旧 | 旧 | Healthy | 今天基线 |
| 旧 | 旧 | 新 | Healthy（无第 4 臂） | **已列**（假绿窗，至 check 升级止） |
| 旧 | 新 | 旧 | Healthy；relay 每 settle 一条 warn（UPSERT 表缺失） | **未列**（假绿 + 日志刷屏，安全） |
| 旧 | 新 | 新 | Healthy（无第 4 臂） | **已列**（假绿窗，至 check 升级止） |
| 新 | 旧 | 旧 | Q6A 探测 → absent → relay on → **FailClosed** | **未列：假红**（健康部署被报红——安全方向） |
| 新 | 旧 | 新 | 表存在但空（旧 server 从不写、无 backfill）→ Q6B 空 → absent → **FailClosed** | **未列：假红**（设计「新 check+新 DB」行隐含新 server） |
| 新 | 新 | 旧 | Q6A absent → relay on → FailClosed；relay 每 settle warn | **已列**（方向正确） |
| 新 | 新 | 新 | 五臂全活 | **已列** |

### 4.2 未列交错的分析

- **两个假红行（新 check + 旧 server）**：check 要求一个**当前没有二进制能产生**的心跳——旧 server 无心跳写入器，表空/表缺 → absent → relay on 即 fail-closed。这是**安全的假红**，但 ops 会看到部署后 CI/监控全红直到 server 落地（首个 settle 写行后自愈）。
- **唯一干净过渡序 = DB → server → check**：先迁 0246（表空无害——旧 check 不看、旧 server 不写）→ 升级 server（心跳开始流动）→ 最后升级 check。任何其它顺序进入有界假绿（旧 check）或有界假红（新 check）窗。**矩阵应补第三轴并把部署顺序写成 load-bearing 约束**。
- **「旧 check + 新 DB」假绿窗的结束条件**：至 check 升级止；但 check 升级瞬间若 server 仍旧 → 直接落入假红行。所以该窗实际在「check 升级且 server 已新」才真正关闭——设计「升级完成即消」表述含糊，须明确「两二进制均新」。
- **新 gate + 旧 DB**：boot 注入 `PgRelayProvisionGate`（env 驱动）→ `assert_audit_scope_provisioned` → `provision_check` → 表缺失 SQL 错 → Err → false → 403。本切片无调用点（R8 未来路径），今日零影响；未来调用点落地时须知 pre-0246 库的 gate 路径恒 403（fail-closed，正确）。
- **双 relay（新旧）并存**：旧 relay 不写心跳；若新 relay 崩 → 心跳过期 → 假闭（安全），新 relay 恢复后自愈。✓
- **leg 种子幂等性（次要）**：harness 每跑建新 throwaway DB（`$$` PID 后缀，test-integration.sh:36/:109），leg D seed / B3b 的裸 `INSERT` 跨跑不可能撞 singleton PK；同跑内 B3b 是首插。但仓储自身惯例是幂等种子（`ON CONFLICT DO NOTHING`）——建议 seed 用 UPSERT，防手工复用 DB 调试时二次跑红。
- **leg B1/B2 不误伤（已核）**：新 check + 空表 + relay off → arm 3 Consistent（grep 不变）；B2 relay off + undelivered>0 → arm 2 FailClosed（relay-disabled 文案不变）。leg C 在 leg D seed 之后仍红：dead>0 臂在心跳臂之前（AC4 钉死），heartbeat fresh 不 rescue dead。✓

---

## §5 Q5 — TOCTOU 与 403-loop → mark_dead → 自愈活性

### 5.1 gate 校验 ↔ 配给签发 TOCTOU

- 时序：`assert_audit_scope_provisioned()`（读 freshness）→ 签发/接受机器令牌。校验与签发在同一请求内毫秒级；但**令牌有效期远超校验瞬间**——校验后 relay 死亡 → 窗口内（≤ freshness=300s）新令牌仍被接受。这是任何 freshness gate 的固有 TOCTOU，有界且被三层补偿：
  1. **投递面永不 consult gate**（设计 §2.6）：已签发令牌的事件照常入队，投递不因门关而停；
  2. **sink 是最终权威**：身份被撤 → 实际投递 403 → mark_dead → dead 臂红——per-event 的执法不依赖门；
  3. **relay 死亡可检测**：relay 死 + 事件堆积 → 心跳过期 → arm 4 FailClosed（enabled∧bindings∧无 fresh 心跳）——**先前签发的令牌不阻止系统变红**。
- `verified()` 错误 fail-closed：DB 错 → false → 403，无「error 上放行」。✓
- **建议**：把「gate 管接受、sink 管投递、freshness 界定接受决策的陈旧度上限」写成契约文字（R8 旁已有雏形），并明示 freshness-有界 TOCTOU 为**已接受**的设计内窗口。

### 5.2 403-loop → mark_dead → 自愈：活锁分析

因果链各边（全部已核代码）：

```
IdP 拒配给 → POST 403 → mark_dead（fenced，≤1/行）→ dead 行 → check arm 1 永红
                └→ 无 settle → 心跳过期 → gate 关（freshness 后）
IdP 恢复   → 新事件 settle → 心跳新鲜 → gate 开（新事件）
```

- **无环**：`mark_dead` 是终态（status=3，claim 过滤永不再暴露——pg.rs claim_due `status IN (0,1)`），**一行最多 403 一次**；「403-loop」按行数有界（每事件恰一 403），不是无限循环。`mark_dead` 丢 fence（`Ok(false)`）→ 行仍可 claim → 重投 → 要么 202 settle 要么 403 终态——有界。
- **单调**：行状态 0/1 → 2/3 单向；心跳新鲜度随时间单调衰减、被 settle 单调刷新；门开合是周期 ≥ freshness 的**有界振荡**（每次开启都要求一次真实投递），非忙循环（relay 按 poll_interval 轮询，403 批每 claim 周期处理一次）。
- **因果图无反馈**：gate 关不导致 IdP 拒（投递面不 consult gate）；「gate 不健康 ⇒ IdP 拒 ⇒ 403 ⇒ dead」是**证据流**（gate 关是被 IdP 拒**造成**的，不是成因）。**无活锁。**
- **稳定停滞态（设计内，须文档化）**：**dead 行无自动清扫**（已核：生产代码零 `DELETE … status=3`；仅测试/drill 重置表）。任何 403 事件后，即使 IdP 恢复、心跳新鲜、gate 重开，check 因 arm 1 保持**永红直至人工清 dead 行**。且 403 窗内的事件**永久丢失**（dead 是终态，不重放——sink 拒收过，重试徒劳；IdP 恢复后也不补投）。设计选择（dead = 审计证据 + 人工补救）可辩护（AC4 钉死 dead 优先），但 **§3 故障表「自愈」表述只对 gate 成立，对 check 不成立**——须补运维流程：修复 IdP 后人工清理 dead 行（或未来加显式 sweep，本设计明确不做）。
- 403 风暴期间 dead 表无上限增长（事件速率有界即增长有界，Q5 只报 5 行）——存储面有界，非 DoS 放大器。

---

## §6 建议清单（按优先级，全部可测）

1. **（中）record 超时**：装饰器用 `tokio::time::timeout`（如 `poll_interval`）包裹 `record_heartbeat`——挂起 UPSERT 不得停摆投递循环；超时 = 不记心跳 = fail-closed。测试：fake recorder 挂起 + 超时注入，断言 settle 在超时后仍返回 `Ok(true)` 且投递循环继续。**建议并入 heartbeat.rs 单测矩阵（AC2.1 扩展）**。
2. **（中）升级矩阵补 server 轴**：§4.1 全 8 行表 + 部署顺序（DB → server → check）写入设计 §2.3；「升级完成即消」改为「两二进制均升级」。
3. **（低）Q6B floor**：`floor(extract(epoch FROM (clock_timestamp() - verified_at)))::bigint`——消除边界 ≤0.5s 的 check/gate 分歧（check 至多提前 1s 报 stale，永不晚报）。
4. **（低）文档化**：① 心跳单向性（投递成功但门假闭 = 设计内）；② 心跳身份无关性（403→dead 臂是身份特定信号）；③ freshness 全部署一致不变量；④ 单 PG 假设；⑤ 403 后 dead 行人工补救流程 + 事件永久丢失语义；⑥ 措辞「无 fencing 的实例 settle」→「任一 fenced settle」。
5. **（低）leg seed 幂等**：B3b / leg D seed 改 UPSERT（`ON CONFLICT (singleton) DO UPDATE`），对齐仓储幂等种子惯例。
6. **（可选）record 节流**：per-process 每 ≥1s 一次 record——singleton 行锁是全集群写热点，新鲜度语义只需每窗 ≥1 次 settle。

## §7 证据核对锚点（本审查复核过的代码事实）

| 声明 | 锚点（文件/符号） |
|---|---|
| settle = fenced 事务，`rows_affected==1` 才 `Ok(true)` | `crates/aero-audit-connector/src/pg.rs` `OutboxRepo for PgOutboxRepo::settle`（fence：`claim_token` + `status IN (0,1)` + `lease_expires_at > clock_timestamp()`） |
| 403 → mark_dead 立即、终态；transient → requeue 永不 dead | `relay.rs::deliver_claim`（Forbidden 臂 / Transient 臂）；`pg.rs::claim_due` 过滤 `status IN (0,1)` |
| `deliver` Ok = 202 + receipt 校验 | `client.rs`（`StatusCode::ACCEPTED` → `validate_audit_receipt`：receipt.event_id 值级匹配 + tenant_id 非空） |
| 无 app 时钟 | `outbox.rs` / `pg.rs` / `fake.rs` 模块头「repo owns the clock exclusively」；`OutboxRepo` trait 五方法无 `now` 参数 |
| 无 dead 行清扫 | 全仓 `--glob '!tests/**'` 无 `DELETE … status=3`；仅 `aero-storage/src/audit_governance.rs` 测试 reset 与 drill 重置 |
| harness 每跑新 throwaway DB | `scripts/test-integration.sh` `T11_DRILL_DB="aero_t11_drill_$$"`、`AUDIT_PROVISION_DB` 同款 + `create_throwaway_database` |
| leg C 在 leg D 后同库跑、dead 臂先于心跳臂 | `test-integration.sh` leg D seed 位（设计 §1.6）；`aero-eng/src/audit_provision.rs::verdict` arm 1 先于 arm 4 |
| Q4/Q6B psql 文本解析需 `::bigint` | `audit_provision.rs::run`（Q4 `q4.parse::<i64>()`）——`::bigint` 是既有模式，floor 是增量 |
| `clock_timestamp()` 非快照冻结 | PG 语义（与 `now()`/`CURRENT_TIMESTAMP` 对照）——设计三处表达式均为调用瞬间实际时间 |
