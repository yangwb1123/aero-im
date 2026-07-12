现在我已完整阅读了 130+ 份分析文档中的关键发现，以下提供系统性架构分析。

---

# Aero IM 架构分析报告：从功能完备到生产就绪的跨越

> **分析范围**：16 crate / 157 SQL 迁移 / ~46K Rust / ~5.3K Web SPA / 130+ 份既有分析文档
> **日期**：2026-07-12 | **视角**：系统架构师

---

## 1. 架构评估

### 1.1 当前架构的优势

**事件驱动脊柱是正确选择。** NATS JetStream 作为中心总线（§1 骨架图）已经被多条消费路径验证：`aero-server`（WS 扇出）、`aero-bot`（5+ 个 bot）、`aero-webhooks`（出站投递）、`aero-push`（移动推送）全部作为独立 durable consumer 工作——这是**经过生产验证的扇出模式**。

分层清晰度值得肯定：

| 层 | 优势 |
|---|---|
| **bus 层** | `EventBus` trait 使测试可 mock；per-subject seq 有单调递增保证；durable consumer 提供 at-least-once 交付 |
| **Hub 扇出** | 有界 `mpsc` 背压——慢消费者被断开而非无限积压 |
| **Redis 状态分离** | presence/roster/viewer count 走 Redis sorted-set，避免进程内存膨胀 |
| **迁移管道** | 编译期嵌入 migration 避免 drift；`_sqlx_migrations` 账本校验 |
| **AI 成本治理** | `CostBudget` + `KeyedCostBudget` + weighted 计费已在，是同类产品的前沿设计 |

### 1.2 关键局限性

**架构成熟度存在「两层断裂」：上层（业务功能）非常丰满，下层（运营基础设施）多处裸奔。**

```
功能层：消息/搜索/AI/通话/直播/推送  —  80% 就绪
中间层：配置/限流/投递台账/安全守卫  —  50% 就绪
基础设施层：生命周期/可观测/DR/测试  —  20% 就绪
```

具体来说：

**① 无监督的后台任务生态（最危险）。** 23+ 个 `tracker.spawn()` 全部用 `if let Err(e) = ... { tracing::error!()` 模式——自诩为「崩溃勿扰」模式。**没有健康聚合、没有自动恢复、没有启动就绪门控。** 一个 `run_bus_listener` 在启动后 200ms 静默崩溃，系统/health/ready 依然返回 200，用户所有实时消息丢失直到下一次部署。

**② 启动/关闭窗口期有确定性数据丢失。** 当前线性启动流水线（`main.rs:95-140`）中，`spawn_background` 在第 10 步开始消费 NATS，而 HTTP server 在第 13 步才接受连接——这之间 50ms–2s 的窗口内，bus listener 已开始推进 NATS cursor 但 WebSocket 尚未就绪。关闭时的对称窗口（HTTP 先关 → tracker 再关 → bus listener 最后停）更大（~15s）。**在滚动更新场景下，旧实例关闭 + 新实例启动 = 累计 15–30s 的事件黑洞。**

**③ 限流器设计存在「单点正确、多点失效」问题。** HTTP 限流器（`rate_limit.rs`）和登录限流器（`login_throttle.rs`）是纯 in-process `HashMap`/`DashMap`。WS 限流器反而是正确的 Redis-backed——说明团队知道正确模式但只部署在了 WS 路径。配置节 `AERO_AUTH_RATE_LIMIT_PER_SEC=20` 在 5 节点下退化为 100 req/s，直接暴露密码爆破面。

**④ 安全守卫只在主消息路径完整。** `assert_room_access` 链（`messages.rs`）校验了成员资格/停用/2FA/信息隔离墙，但 push/thread notification/webhook/bot dispatch 四条侧路径跳过了同一套守卫。一个已停用用户的 push token 仍会收到交互通知——**安全和合规边界不一致。**

**⑤ 前端测试真空 + API 版本化缺失的组合风险。** 5.3K JS 零测试 + 100+ 端点无版本控制 + WS 帧无 `schema_version` = **任何服务端侧字段重命名都可以静默破坏前端。** 在缺乏 `serde(deny_unknown_fields)` 审计的情况下，新字段加入即 breaking change。

### 1.3 已经做出的正确设计决策

- **NATS 作为中心总线而非 Kafka**：10 个 durable consumer 的运维负担远低于 Kafka consumer group 管理
- **Redis 而非进程内存做集群级状态**：presence/call roster 不走 gossip，避免了脑裂
- **Ephemeral vs Durable 分离**：直播走 ephemeral（丢弹幕无碍），IM 走 durable（消息必达）
- **AI 加权 budget**：Embed=1 / Mod=2 / Sum=3 / Ans=5 的加权系统比纯 token 计数更实用
- **`assert_room_access` 统一守卫**（虽然侧路径缺失了，但主路径架构设计是对的）

---

## 2. 扩展方向

基于 130+ 份分析的交叉验证，我提炼出 **5 个高价值架构扩展方向**，按对系统长期健康的影响排序：

### 方向一：后台任务治理与生命周期管理（P0 · 运营根基）

**为什么需要**：
23+ 个常驻后台任务（bus listener、bot、AI worker、sweeper、GC loop）在无声崩溃时全无恢复能力。这是**整个系统最脆弱的一个面**——崩溃不会立即导致服务不可用，而是静默退化（用户收不到消息、bot 不回复、推送丢失），直到 operator 在日志中偶然发现。

**核心挑战**：
- 现有 `TaskTracker` 只追踪任务是否完成，不提供 `restart`/`degrade`/`fail` 语义
- 每个 `run_*` 函数签名不统一：有的接受 `CancellationToken`，有的没有
- `/health/ready` 只探测 PG/Redis/NATS/blob 连通性，不覆盖任何业务层面的就绪状态

**预期架构变更**：
```
当前：
  tracker.spawn(async move { if let Err(e) = task.run(s).await { error!(e) } })

目标：
  registry.register("agent_bot", task.run(s), SupervisorConfig {
    on_exit: OnExit::Restart(max_retries=3, backoff=exponential(1s, 30s)),
    readiness: ReadinessProbe::Heartbeat(interval=5s),
    depends_on: ["pg", "nats", "ai_service"],
  });
```

**对现有系统的影响**：**侵入性低。** 所有后台任务可以原地添加 `BackgroundTaskHealth` 注册 + 心跳。`/health/ready` 只需追加一行 gauge 检查。Supervisor 重构是增量叠加——不影响现有任务的执行逻辑。

### 方向二：事件全链路交付台账（P1 · 核心 UX 与扩展基础）

**为什么需要**：
这是 **ROADMAP 方向三**（per-user delivery cursor）的深化。当前 `?since=message_id` backfill 按房扫描 `list_since`，无 per-user 投递游标。导致：①长离线追平成本 O(N×rooms)；②多设备各自全量重放；③故障后全员冲击 DB。

**核心挑战**：
- `per-subject seq`（`bus/seq.rs`）是房间级别的，但投递确认是用户级别的——需要引入 `delivery_cursor(participant_id, room_id, last_acked_seq)` 表
- 多设备并发 ack 的收敛：必须用 `WHERE last_seq < $1 AND participant = $2 AND room = $3` 做原子推进（乐观锁）
- 与法务保全/留存清扫的交互：被保全消息的 seq 必须被游标锚定，不能因软删而被跳过

**预期架构变更**：
- 新 SQL 迁移：`delivery_cursors` 表，UK `(participant_id, room_id)`
- `backfill_since` 改为两步：先查游标位 → 再 `message_seq > cursor`（indexed scan，O(log N)）
- 新 WS frame：`backfill_delta`（含 gap 内总数）+ `ack_to_seq(seq)`（客户端确认）
- 多设备共享游标：第一台设备 ack 后，第二台设备只收 diff

**对现有系统的影响**：**侵入中等。** 不改变现有消息发送路径，不影响既有 `list_since` API（可作为 fallback）。主要影响 ws 协议 + storage + hub 层。

### 方向三：统一出站信任边界与 SSRF 防护（P0 · 安全底线）

**为什么需要**：
扫描确认两条**无防护的出站 HTTP 路径**：① `unfurl_bot` 的 `ReqwestUnfurler::fetch`（用户发 URL 即触发）② webhook 的 `ReqwestSender::deliver`（用户配置 URL 即触发）。两者均无 private IP 检查、无 DNS rebinding 防护、无重定向 URL 再验证。**这是 P0 级 SSRF 漏洞**——攻击者仅需在公共频道发一个 URL，即可通过重定向链击打云元数据端点（`169.254.169.254`）。

**核心挑战**：
- 不允许访问阿里云/腾讯云 metadata endpoint（CIDR 块 `100.64.0.0/10` 也要覆盖）
- DNS rebinding：域名首次解析到公网 IP，重定向后同域名解析到内网——需要二次验证
- URL redirect chain 的最终目标 IP 检查：reqwest 默认 follow 10 次，须在每跳后检查
- 与 webhook 对公网可达性需求（合理 webhook URL 可能是公网）的平衡

**预期架构变更**：
```
// 新增 OutboundHttpClient wrapper
pub struct OutboundHttpClient {
    inner: reqwest::Client,
    ip_filter: IpFilter,       // 禁止 RFC 1918 + cloud metadata
    redirect_validator: Arc<dyn Fn(&Url) -> bool>,  // 每跳检查
    dns_rebind_check: bool,    // 二次解析校验
}

// 替代两个独立 Client
- struct ReqwestUnfurler { client: OutboundHttpClient }
- struct ReqwestSender { client: OutboundHttpClient }
```

**对现有系统的影响**：**侵入低。** 只需替换两个 `reqwest::Client` 的构造点，所有出站 HTTP 调用集中受控。`AERO_CLAMAV_HOST`（病毒扫描）和 AI provider URL 也可复用该 wrapper。

### 方向四：跨消费者安全守卫一致性（P1 · 合规）

**为什么需要**：
消息路径有 `assert_room_access` 的完整守卫链（成员资格/停用/2FA/隔离墙），但 push/thread notification/webhook/bot dispatch 四条消费通路绕过。**合规审计会发现，一个被管理员停用的用户仍在通过 push token 接收交互通知。** 这在 SOC2/ISO 27001 场景下是明确的 non-conformance。

**核心挑战**：
- 守卫校验（`is_active` / `is_info_barred` / `is_muted`）是 **read-path query**，每条 push 通知一次查询——对推送达量大的场景有性能影响
- 法务保全与推送的交互：被保全消息的预览不应该出现在 push 通知内容中
- 逆向场景：停用用户停用前发出的消息触发回复，回复是否需要通知停用用户？

**预期架构变更**：
```
// 统一守卫 trait
trait NotificationGuard: Send + Sync {
    fn can_notify(&self, recipient: &ParticipantId, context: &NotifyContext) -> Result<bool>;
}

// 实现组合链
struct ChainGuard {
    guards: Vec<Box<dyn NotificationGuard>>,
    // 默认 fail-open，可配置 fail-closed
}
```

**对现有系统的影响**：**侵入中等。** 需要修改 `push_bot`、`thread_subs`、`webhook/delivery`、`bot_dispatch` 的投递逻辑，在前置校验中增加守卫调用。守护链本身是新增模块（`im-core/service/guards.rs`），不改变现有模块内部结构。

### 方向五：可观测性基础设施聚合（P1 · 运维可诊断）

**为什么需要**：
当前可观测性存在三个断层：①**后台任务无健康 gauge**（`aero_background_task_up{task="agent_bot"}` 不存在）；②**日志纯文本非 JSON**（无法按 `trace_id` 聚合）；③**邮件/推送/关键路径无告警**（SMTP 挂了无告警、token 存储失败无计数器）。跨进程追踪虽然有 `traceparent` stamp/extract（已在 `events.rs` 和 `bus.rs`），但 `EventBus` trait 不传递 headers，导致 trace 上下文在进程边界断裂。

**核心挑战**：
- `EventBus::publish` 的签名是 `fn publish(&self, subject, payload) -> Result`——没有 `headers` 参数。加 headers 需要 trait 签名变更，影响所有实现
- JSON 日志在 Rust 中的性能影响（`tracing-subscriber` 的 JSON 格式序列化开销）
- 告警规则和仪表盘是 ops 配置，不属于代码——但缺少 `docker-compose` 或 `helm` 中的 prometheus-rule 示例

**预期架构变更**：
- `EventBus::publish` 加 `headers: Option<HashMap<String, String>>`（向后兼容，默认 None）
- 消费端从 header 提取 `traceparent` 做 `set_parent`（替代现有 payload 字段法）
- 日志输出 `AERO_LOG_FORMAT=json` 切换（默认 text，json 可选）
- `aero_server_background_tasks_total` gauge 注册 + `/health/ready` 聚合

**对现有系统的影响**：**侵入低-中。** EventBus trait 签名变更是单点变更（影响 `MockBus` + `JetStreamBus` + 所有测试 mock）。日志格式是配置切换。告警规则是运维配置，不影响代码。

---

## 3. 接口设计建议

### 3.1 EventBus 接口演进

当前 `EventBus` trait 的无 headers 签名是 trace 断裂的根因：

```rust
// 当前
pub trait EventBus: Send + Sync {
    async fn publish(&self, subject: &str, payload: Vec<u8>) -> Result<()>;
}

// 建议
pub trait EventBus: Send + Sync {
    async fn publish(&self, subject: &str, payload: Vec<u8>) -> Result<()>;
    async fn publish_with_headers(
        &self, subject: &str, payload: Vec<u8>,
        headers: Option<HashMap<String, String>>,
    ) -> Result<()>;
}
```

**设计原则**：
- **默认方法**：`publish` 调用 `publish_with_headers(..., None)` 保持向后兼容
- 所有既有调用方不需要改签名
- 新调用方（AI 请求/webhook 出站）传递 `traceparent` header

### 3.2 配置治理

配置的「双下划线 vs 单下划线」混乱必须终结：

```rust
// 当前：40+ 个散落的 env::var()
// 建议：集中 AppConfig + 启动验证
pub struct AppConfig {
    pub retention: RetentionConfig,
    pub push: PushConfig,
    pub cors: CorsConfig,
    // 所有配置通过 figment 注入，启动时全量验证
}

impl AppConfig {
    /// 启动时调用，报告所有 required config 的状态
    pub fn validate_startup(&self) -> Vec<ConfigIssue>;
}
```

**设计原则**：
- `config.example.toml` 是唯一真实来源——env 覆盖只是运行时 override
- `GET /debug/config` endpoint（admin bearer 门控）dump 生效配置树
- 启动时 `--validate-config` 模式（docker entrypoint 调用）

### 3.3 后台任务治理

```rust
pub enum OnExit {
    Ignore,           // 当前行为：只日志，不复生
    Restart {         // 自动恢复
        max_retries: u32,
        backoff: Duration,
    },
    Degrade {         // 标记降级，/health 反映
        message: &'static str,
    },
}

pub struct BackgroundTaskHandle {
    pub name: &'static str,
    pub health: Arc<AtomicU64>,  // last heartbeat timestamp
    pub on_exit: OnExit,
}
```

**设计原则**：
- `tracker.spawn` 保持兼容——新 `tracker.spawn_supervised` 加治理语义
- `/health/ready` 增加 `"bg_tasks": { "total": 23, "healthy": 21, "degraded": [] }`
- 心跳在 `run_*` 主循环中最低成本更新（每 5s 一个 `AtomicU64::store`）

### 3.4 WS 协议向后兼容

当前 WS 帧无版本字段，`ServerFrame` 的 serde 策略需审计：

```rust
// 建议：所有 ServerFrame variant 使用
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerFrame {
    // ...
}

// 关键规则：
// 1. 所有 variant 不加 #[serde(deny_unknown_fields)]
// 2. 新字段必须 Option<T> 或提供 default
// 3. 每个新增 variant 对应前端 ws.on('msg:...') handler
// 4. ServerFrame 增加 schema_version: u32 可选字段
```

---

## 4. 技术选型

### 4.1 不需要引入的新栈

以下方向可以在现有生态内解决，**不需要新依赖**：

| 方向 | 可复用现有 | 说明 |
|------|-----------|------|
| 后台任务健康 | `AtomicU64` + `DashMap` | 无需 actor 框架 |
| 配置治理 | `figment` 已引入 | 只需将所有 `env::var()` 迁移到 `AppConfig` |
| 多头限流 | Redis `INCR` + `EXPIRE`（复用 `ws_rate.rs` 模式） | 不引入 leaky bucket 库 |
| JSON 日志 | `tracing-subscriber` 已支持 | 只需 `AERO_LOG_FORMAT=json` 环境变量 |
| 前端测试 | `vitest`（零配置 ESM） | 不需 Jest，不需 `jsdom` 以外的 DOM 库 |

### 4.2 需要谨慎评估的引入

| 候选 | 用途 | 评估 | 替代方案 |
|------|------|------|---------|
| **Cron 调度库**（`tokio-cron-scheduler`） | 替代 interval 定时器 | ⚠️ 当前 `MissedTickBehavior::Skip` + interval 模式已够用。Cron 表达式增加运维复杂度 | 保留 interval，加可配置的 `cron` 字符串 parser 做可选扩展 |
| **GraphQL**（`async-graphql`） | 替代 REST 轮询 | ❌ 不推荐。WS 推送未读状态（方向五）直接解决需求，GraphQL 引入大量复杂度和版本化难题 | WS 帧扩展 |
| **WAL-G / pgBackRest** | PG PITR 备份 | ✅ 推荐。现有 DR 真空（方向五）需要生产级备份工具 | 自写 `pg_dump` 脚本 |
| **Playwright** | 前端 E2E 测试 | ✅ 推荐。vitest + jsdom 不覆盖跨窗口/跨 tab 场景 | 无 |

### 4.3 自建 vs 采购决策矩阵

| 需求 | 自建成本 | 采购选项 | 推荐 |
|------|---------|---------|------|
| DR 备份脚本 | 低（`scripts/db-backup.sh` 一天完成） | 无 | **自建** |
| 邮件基础设施（SendGrid/Resend） | 中等（作业队列 + 模板 + 退订） | SendGrid ≈ $20/100K | **先自建队列**（复用 `ai_usage_ledger_drain` 模式），后期可切换外部 provider |
| SSRF 防护 | 低（`OutboundHttpClient` wrapper） | 无（WAF 不覆盖 outbound） | **自建** |
| 前端测试 | 中等（从纯函数开始渐进覆盖） | 无 | **自建** |

---

## 5. 实施路线图

### 优先级策略

```
P0 = 数据丢失 / 安全漏洞 / 运维盲飞
P1 = 合规缺口 / 用户体验 / 扩展基础
P2 = 运维效率 / 产品边缘
```

### 阶段划分

#### 阶段一：止血（2 周 · P0 级）

| 任务 | 估时 | 影响 |
|------|------|------|
| ① SSRF 防护 wrapper（替换 `ReqwestUnfurler` + `ReqwestSender`） | 2 天 | 堵 P0 安全漏洞 |
| ② 后台任务健康注册表（`BackgroundTaskHealth` + `/health/ready` 增强） | 3 天 | 消除无声崩溃盲区 |
| ③ 启动/关闭时序修复（bus listener 延迟启动 + 提前关闭） | 2 天 | 消除启动窗口期事件丢失 |
| ④ 所有 `env::var()` 迁移到 `AppConfig` + 启动验证 | 3 天 | 配置治理基石 |

**交付验证**：
- `curl /health/ready` 返回 `{"bg_tasks": {"total": 23, "healthy": 23}}`
- `unfurl_bot` 请求 `http://169.254.169.254/` 返回 403 而非响应体
- 启动窗口 event loss → 100% 消除（新模拟测试验证）

**风险点**：`EventBus` trait 签名变更可能影响多个 mock 实现。需先在分支上重构，确保 `cargo test --workspace --lib` 全绿。

#### 阶段二：合规加固（3 周 · P1 级）

| 任务 | 估时 | 影响 |
|------|------|------|
| ⑤ 侧路径守卫统一（push/thread/webhook/bot 加 `can_notify`） | 5 天 | 停用用户不再收到推送/通知 |
| ⑥ 软删孤儿数据清扫策略 | 3 天 | reactions/receipts/pins 不再无限膨胀 |
| ⑦ 密码重置 UX（前端表单 + 重置页面） | 1 天 | ROI 极高，一上午完成 |
| ⑧ 事务性邮件基础设施（偏好门控 + 作业队列） | 5 天 | 为所有邮件场景铺路 |

**交付验证**：
- 停用用户推送到 `rejected` token → `unregister`（不静默重试）
- `EXPLAIN SELECT COUNT(*) FROM reactions WHERE message_id IN (SELECT id FROM messages WHERE soft_deleted)` → 0 行
- 登录页出现「忘记密码」链接，流程可走通

**风险点**：守卫链的性能影响（每条 push 通知一次读查询）。可以批量校验（一次 `SELECT is_active FROM participants WHERE id = ANY($1)`）。

#### 阶段三：交付与可观测（3 周 · P1 级）

| 任务 | 估时 | 影响 |
|------|------|------|
| ⑨ `delivery_cursor` 持久投递台账（迁移 + repo + WS 帧） | 5 天 | 长离线追平 O(log N)，多设备共享游标 |
| ⑩ 分布式限流升级（HTTP + login 迁移 Redis） | 3 天 | N 节点不再降级限流阈值 |
| ⑪ 跨进程 trace 贯通（`EventBus::publish_with_headers` + JSON 日志） | 3 天 | 故障诊断从 30 分钟缩短到 2 分钟 |
| ⑫ WS 推送未读状态变化 | 3 天 | 消除 6.5s 轮询盲区 |

**交付验证**：
- `?since=` backfill 走 `delivery_cursors` 索引扫描而非全表
- 启动 5 个节点 + 配置 `AERO_AUTH_RATE_LIMIT_PER_SEC=20` → 总入站 auth 不超过 20 rps
- JSON 日志行包含 `trace_id`、`span_id`、`workspace_id`

**风险点**：`delivery_cursor` 引入新状态（游标推进）的一致性边界。必须做 at-least-once 幂等 + 乐观锁（`WHERE last_seq < $1`）。

#### 阶段四：质量基础（4 周 · 持续）

| 任务 | 估时 | 影响 |
|------|------|------|
| ⑬ 前端纯函数测试（`render.js` 685 行 + `context.js` 208 行） | 3 天 | 最高 ROI 的测试覆盖 |
| ⑭ API 版本化基础设施（`VersioningLayer` 中间件 + 合约测试） | 3 天 | 企业签约的前提条件 |
| ⑮ DR 文档 + 备份脚本 | 2 天 | 合规必须 |
| ⑯ API 兼容性合约测试套件 | 3 天 | 阻止静默 breaking change |

**交付验证**：
- `npx vitest run --coverage` → `render.js` 覆盖率 > 60%
- `/api/v1/` 前缀路由存在并返回 200
- `docs/runbooks/disaster-recovery.md` 存在且步骤可 verifiable

### 整体时间线

```
Week 1-2:   阶段一（止血）—— SSRF + 后台健康 + 启动时序 + 配置治理
Week 3-5:   阶段二（合规）—— 守卫统一 + 孤儿清扫 + 密码重置 + 邮件基建
Week 6-8:   阶段三（交付）—— 投递台账 + 分布式限流 + trace + 未读推送
Week 9-12:  阶段四（质量）—— 前端测试 + API 版本 + DR + 合约测试
            ───────────────────────────────────────
总工期：约 12 周（3 个月），2-3 人并行
```

### 关键风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| 后台任务治理重构导致现有 bot 回归 | 中 | 高 | 每个 bot 独立重构 + 回归测试 |
| `delivery_cursor` 与既有 backfill 不兼容 | 中 | 高 | 灰度切换：先写双写（新旧并存），切流后移除旧路径 |
| SSRF 防护阻断合法 webhook | 低 | 高 | `AERO_SSRF_ALLOWED_CIDRS` 白名单 + 启动验证 |
| 前端测试引入导致 CI 时间翻倍 | 高 | 中 | vitest 比 Jest 快 3 倍；CI 分层：L1 纯函数（30s） / L4 E2E（5min 单独 job） |
| 分布式限流在 Redis 故障时 fail-closed 锁死全系统 | 中 | 高 | **必须 fail-open**：Redis 不可用降级到 in-process L1，记录降级告警 |

### 不做的方向（战略放弃）

以下方向虽然在高频分析文档中被提到，但**在当前阶段不应进入路线图**：

- **Canvas CRDT**：需要完整 CRDT 库 + 新 block 类型 + 前端实时编辑器。在产品验证明确需求之前（当前零代码），不应投入。
- **消息表自动分区**（ROADMAP 方向四）：`migration 0148` 是 shadow 状态。分区在 10K+ 用户前不紧迫，且可 defer 到 Postgres 17 表继承语法成熟。
- **多区域/灾备**：当前单区域尚未稳定，多区域是基础设施级投资，应等 P0/P1 修复后再规划。
- **移动端原生 SDK**：Web 优先策略正确。在 Web 全链路验证稳定之前，不应分散投入。

---

**总结**：Aero IM 的事件驱动架构在功能层面非常扎实（消息/搜索/AI/直播/通话均经过仔细设计），但运营基础设施存在系统性欠债。上述路线图的四个阶段正是从「功能富 demo」到「生产就绪 SaaS」的路径——先止血（P0 安全与可靠性）、再合规（P1 守卫与数据完整）、再交付（P1 投递与可观测）、最后质量（持续测试与版本化）。建议优先启动阶段一，其余阶段对齐 Q3 目标。
