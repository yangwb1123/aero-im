# 架构分析：Aero IM 系统

> 基于 AGENTS.md、文档反馈、以及代码结构推断，从架构视角进行系统性评估。

---

## 一、架构评估

### 1.1 现有架构的优势

**领域驱动的前期决策基本正确。**

| 决策 | 合理性 |
|---|---|
| 按功能垂直切分 crate（`aero-im-core` / `aero-live-webrtc` / `aero-bus`） | 边界清晰，无循环依赖，编译并行度高 |
| 事件总线（NATS JetStream）作为跨实例意见一致性的唯一收口 | 比直接 Redis pub/sub 更鲁棒（持久化+回溯），比 Kafka 更轻量 |
| WS 扇出由 in-process `Hub` 完成 | 避免了每条消息穿 NATS 两次，延迟可预测 |
| 集群状态统一归 Redis sorted-set | zadd + zremrangebyscore 模型天然适合带 TTL 的在线名册 |

**值得肯定的模式**：`bus/seq.rs` 的 per-subject 单调 seq 是防止 at-least-once 重投导致乱序的关键设计——这比依赖 NATS 自带 timestamp 更可控，也比全局 seq 更水平友好。

### 1.2 五个可观察的架构债务

#### 债务 1：`aero-server` 单体膨胀

`routes/routes.rs` 约 3000 行，挂载 100+ 子模块路由。随着时间推移将产生以下症状：

- **编译时**：改动`aero-im-core`一个类型签名 → `aero-server`全量重编（这是 Rust 单体 crate 的固有代价，但可以通过 `routes` 拆 crate 缓解）
- **启动时**：boot 程序串行装配所有模块，即使某些模块（如 SRT 摄入）在部署中未启用
- **运行时**：所有 bot/drainer/timer 在同一 `tokio::spawn` 下，无法按优先级/资源池分组

**根本原因**：框架层没有「微内核 + 插件」的概念——所有功能编译期硬链接进 server，无法运行时插拔。

**建议方向**：定义 `Plugin` trait：

```rust
#[async_trait]
pub trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;
    fn enabled(&self, config: &Config) -> bool;
    fn routes(&self) -> Vec<RouteSpec>;      // 路由贡献
    fn background_tasks(&self) -> Vec<TaskSpec>;  // 后台任务
    fn on_shutdown(&self) -> BoxFuture<'_, ()>;   // 优雅关停
}
```

这一步与方向五（第三方应用平台）共用同一扩展机制——**自定义 plugins 的 API 就是内部 plugin 的 API**。

#### 债务 2：鉴权层缺乏统一资源模型

当前鉴权路径有三种并行的模式：

| 模式 | 使用场景 | 问题 |
|---|---|---|
| `AuthUser` extractor + `assert_room_access(participant, room)` | 房间级数据 | 新资源易漏（CI authz_lint 兜底，但只能扫名不能扫逻辑） |
| `member_role` + `can_administer` | 工作区管理操作 | 与 `assert_room_access` 不同函数签名，混用易错 |
| `ip_allowlist::enforce_layer` 中间件 | IP 白名单 | 全局中间件，与上层鉴权正交 |

**缺失的东西**：一个统一的 `AuthorizationContext`，能回答「用户 X 对资源 Y 可有操作 Z？」。当前是「用户 X 对房间 Y 可有访问？」的 ad-hoc 检查，但「资源」不止房间——还有 webhook、bot、stream、workspace、PAT 本身。

**后果**：
- 每条新路由的鉴权是「copy-paste + 改一下」→ 遗漏概率随路由数量线性增长
- PAT scope 无法复用房间鉴权——`assert_room_access` 是 `(participant, room)`，PAT scope 是 `(token, action)`，两者不做正交组合

#### 债务 3：一致性模型未显式契约化

系统涉及四个存储子系统：

```
Postgres (强一致, ACID)  ← 业务核心状态
Redis (最终一致, 带 TTL) ← 集群视图
NATS (at-least-once)    ← 事件总线
In-process Hub (mpsc)   ← WS 扇出
```

**问题**：这些子系统之间的一致性边界没有显式文档或代码断言。例如：

- `send_message` 写入 PG → publish NATS → Hub 扇出。如果客户端在 Hub 扇出前断开，重连后读 `list_since` 可恢复。但如果 NATS 消费失败 + PG 已写入，客户端通过 REST 可以读到消息但收不到事件——*这不是 bug，但没在任何地方说明这个窗口期*。
- `participant_cache.invalidate` 是写路径手动触发的，但如果某条写路径忘记调它，数据一致性问题可能几小时后才被发现。

**建议**：为每个关键操作定义一致性契约，在 `docs/specs/` 下以表格呈现：

```markdown
| 操作 | 持久性 | 一致性 | 恢复行为 |
|---|---|---|---|
| send_message | PG commit + NATS publish | 写后读同连接保证 | 重连后 list_since 补偿 |
| update_me | PG write + cache invalidate | 最终一致 ≤ 1 RTT | cache TTL 自动过期 |
| go_live | Redis zadd + NATS Status | Redis 先于 NATS | 跨节点桥心跳修复 |
```

#### 债务 4：媒体管道可靠性缺口

> AGENTS.md §4.5 明确指出 media seam 已建但未接线。这不是死代码，是测试覆盖不到的逻辑路径。

从架构层面，风险点在于：

- **SfuMediaSession.run()** 没有生产实例，意味着 str0m 的 poll/sink 循环从未在真实网络条件下跑过。RTP 抖动、丢包、乱序的回退行为无法验证。
- **call_bridge_supervisor**（跨节点 RTP 中继）的单测在 localhost 跑，没有模拟丢包/延迟/节点故障。`ensure_egress` 返回 `None` 时全休眠——但如果是在节点运行中网络分区呢？
- **RTMP → HLS 管线** 没有传输层熔断：如果 HLS writer 阻塞（磁盘满或 NFS 延迟），RTMP ingestion 不会感知，导致推流端不断积压 TCP 缓冲区。

**建议方向**：为每个媒体管道定义 **SLI (Service Level Indicator)**：

```
RTMP pipeline: 输入帧 → HLS 分片延迟 ≤ 2秒 P99
SFU pipeline:  输入 RTP → 订阅者输出 ≤ 50ms P99
Call bridge:   跨节点 RTP 延迟 ≤ 150ms P99
```

没有这些指标，媒体管道的退化是无感无声的——直到用户投诉。

#### 债务 5：WebSocket 协议的演进能力

当前 WS 协议是隐式版本协商——`ClientFrame`/`ServerFrame` enum 的变体就是协议。AGENTS.md §4.1 提到「WS 帧格式变更直接导致断连」，但现网没有版本号。

**核心矛盾**：WS 连接是长连接（可能存活数小时），但 server 版本可能在连接期间升级。如果 `ServerFrame` 新增字段（比如 `Card.metadata.thumbnail_url`），旧 SPA 收到后会忽略——但如果删除字段或改字段类型，旧 SPA 可能 crash（serde 反序列化失败）。

**修复方案**（与文档反馈一致）：

1. 连接时 `?v=N` 协商版本
2. 版本 1（当前）维持现有帧格式
3. 引入 `ServerFrame::V2 { .. }` 变体
4. 跨版本兼容性在 `Hub::fan_out_raw` 做分叉

---

## 二、高价值架构扩展方向

### 方向 A：多租户资源隔离体系

**业务价值**：从「共享集群」走向「隔离的工作区租户」，是定价/计费的前提。

**技术难点**（不只是配额）：

| 层级 | 当前状态 | 目标状态 |
|---|---|---|
| **路由隔离** | 所有工作区共享路由路径 | `/{workspace_id}/api/...` 命名空间 |
| **存储隔离** | PG 共享表，`workspace_id` 列区分 | 可选 schema-per-workspace 或 RLS |
| **计算隔离** | 所有 bot/worker 共享 tokio 线程 | spawning 分组（per-workspace semaphore） |
| **速率隔离** | Redis 窗口 + 单档限流 | 按工作区定价计划映射到多档速率表 |

**架构变更**：

- `QuotaStore` 新仓储，管理 `workspace_quotas` 表（消息/月、存储/工作区、并发连接/工作区）
- `QuotaMiddleware` 在 `routes` 层注入，作用于所有 mutating 路由
- 配额检查与 `assert_room_access` 正交——前者是资源预算，后者是权限

**现有系统影响**：

- `create_room_in_workspace` 需要先检查房间配额（新耦合点）
- `put_blob` 需要检查存储配额（当前 `blob_gc_drain` 是后置清理，不是前置门）
- 需要 new 迁移来加 quota 表
- CI `authz_lint` 需要扩展以覆盖配额检查

**风险**：配额检查本身是新的 DoS 面——如果配额存储是 hitless 读（Redis），风险可控；如果是同步 PG 查询，则每条写入路径增加 1 RTT 延迟。**建议配额值缓存到 Redis，TTL 60 秒，允许短窗口超量**。

---

### 方向 B：事件驱动架构的形式化

**业务价值**：AGENTS.md 的事件 DAG 是目前最精确的架构描述。但这份 DAG 是**手写文档**，不是代码——无法验证 node/edge 的完整性，也无法阻拦错误的新增 edge。

**核心技术方案**：引入事件类型系统（Event Registry）

当前每个事件变体在枚举里定义，但事件间的关系（cause→effect）是散落在 bot 注册和 handler 中的逻辑。一个可生长的事件 registry 可以：

```rust
// 事件谱系声明
registry.declare::<MessagePosted>()
    .causes::<NotificationFannedOut>()     // 扇出通知
    .causes::<ModerationCheckTriggered>()   // 审核
    .may_cause::<AIBotResponseTriggered>()  // 有条件触发
    .may_cause::<OOOReplyTriggered>();      // 有条件触发
```

**核心挑战**：

- 声明式谱系 ≠ 执行语义——有条件触发无法通过纯声明捕获
- 增加 boilerplate（每个事件变体需额外注册行）
- 没有成熟的 Rust 生态支持事件谱系声明

**建议落地路径**：

1. **不加框架，只加文档校验**：在 `docs/specs/` 下维护 YAML 事件谱系
2. **CI 加 `cargo_manifest_event_check.sh`**：确保 `RoomEvent`/`StreamEvent` 的每个变体在谱系 YAML 中有声明
3. **当变体超过 50 个时**再考虑代码级 registry

**优先级**：P2（当前事件变体约 20+ 个，尚未失控）

---

### 方向 C：观察性作为一等基础设施

**业务价值**：当前 AGENTS.md 提到 `observability_gauge_samplers` 三个 timer，但这些都是**手动 gauge 设值**，不是 tracing 原生。

**技术缺口**：

| 维度 | 当前 | 需要 |
|---|---|---|
| **请求瀑布** | `inject_request_id` middleware，无 span | `tracing::Span` 自动传播到 sub-calls |
| **DB 查询追踪** | sqlx 无 `tracing` feature 启用 | `sqlx::postgres::PgPoolOptions::with_tracing()` |
| **NATS 消费延迟** | 无 | per-subject consumer lag gauge |
| **Redis 延迟** | 无 | `fred` 的 `on_error`/`on_connect` callback → 带 latency 的 histogram |
| **WS 消息延迟** | 无 | `Hub::fan_out_raw` 的 P50/P99 分发延迟 |

**核心挑战**：

- `tracing` 的开销在 WS 热路径（每秒千帧扇出）上不可忽略——`Hub::fan_out_raw` 是 per-frame 的 critical path，加 span 可能增加 5-10% CPU
- 需要区分 **hot path**（WS 扇出、RTP 转发）和 **cold path**（REST API、bot 处理），hot path 低采样率

**架构变更**：

```
ws/fan_out.rs 的 fan_out_raw:
  当前: for each subscriber { tx.send(frame).await }
  建议: with tracing instrumentation but 1:N sample rate (默认 0.01)

  并在 Hub struct 里加 Histogram:
    hub.metrics.distribution_latency[room_id % BUCKETS]  // 预分桶避免高维
```

**对现有系统影响**：零（additive changes，不侵入现有逻辑）

---

### 方向 D：多节点部署拓扑的原生化

**业务价值**：单节点（port 3030）生产级可用，但向多节点扩展时，当前架构有若干隐性约束。

**必须解决的三件事**：

#### 1. WS 连接亲缘性

当前 `Hub` 是进程内的 mpsc 分发。多节点时 `RoomEvent` 通过 NATS 分发给所有节点，每个节点独立扇出给自辖的 WS 连接。这正确，但：

- 客户端重连可能落到不同节点 → 需要 `hub.subscriber_generation` 或者 reconnect token 携带 last-known 节点 ID
- `presence`（Redis 读取）多节点正确，但 `online_room_members`（`participant_cache.get_or_fetch`）如果 cache miss 会穿透到 DB——多节点时穿透率随节点数线性增长

**修复**：`participant_cache` 加 `local_cache`（`moka`/`quick_cache`）兜底 Redis 未命中，且本地 TTL 短（500ms）。

#### 2. 共享 blob 存储

当前 `blob_store_from_env` 默认 LocalFs。多节点时需要 `S3BlobStore` + 每个 blob 的分布式锁定（或者 S3 的 read-after-write 一致性充足）。

`AERO_S3_BUCKET` 已配但 LocalFs fallback 不是退化——它不能提供跨节点一致性。**启动时如果检测到多节点配置 → LocalFs 阻止启动**。

#### 3. Call bridge 生产接线

当前 `call_bridge_supervisor` 和 `SfuMediaSession` 已建但不在 `run()` 循环中。多节点通话桥（`bridge_frame`）是这个方向的关键基础设施。

**风险点**：
- `bridge_frame` 的编解码没有熔断——如果远端节点 unresponsive，本地节点会持续 buffer RTP 帧
- 跨节点 RTP 的 NAT 穿透没有处理（当前假设 UDP 直达）

**建议**：

```
路径拆分:
  Phase 1 (4 周): WS 多节点 + NATS 集群化 + S3 切换
  Phase 2 (6 周): Call bridge 接线 + 单节点 → 双节点 E2E 测试
  Phase 3 (4 周): 负载均衡 + 优雅缩扩容 + 蓝绿部署
```

---

### 方向 E：平台身份统一（User → Actor）

**业务价值**：当前身份模型假设「一个请求 = 一个人类用户」。但平台经济需要：

- Bot 以 bot identity 而非模拟用户发送消息
- 第三方 app 以 app identity + scope 行动
- 匿名 SSO 用户以临时身份参与

**核心挑战**：`AuthUser` extractor 当前只认识 `(user_id, workspace_id)`。要统一成 `Actor` trait：

```rust
#[derive(Clone)]
pub enum Actor {
    User { id: UserId, workspace_id: WorkspaceId },
    Bot { id: BotId, workspace_id: WorkspaceId, managed_by: UserId },
    App { id: AppId, workspace_id: WorkspaceId, scopes: HashSet<Scope> },
    Anonymous { session_id: SessionId, workspace_id: WorkspaceId },
}
```

每个 `Actor` 可被 `assert_room_access` 校验，但逻辑不同：

- User → 查 membership + 2FA + deactivation
- Bot → 查 bot_registry + user that created it still active
- App → 查 PAT scope + room access
- Anonymous → 只允许公开房间

**架构变更**：

- `AuthUser` → `Actor`（兼容层维持旧 extractor）
- `assert_room_access` 重载为 `accepts Actor`
- 所有 REST handler 逐一迁移（约 100+ 个，工作量大）

**这个方向与方向 B（鉴权层统一）共享大部分工作。**

---

## 三、接口设计建议

### 3.1 REST API 风格标准化

当前不一致模式（索引自 AGENTS.md）：

```rust
// 不一致 A：返回类型
Json<Vec<Message>>              // 直接序列化
ApiResult<Json<Vec<Message>>>   // 带信封
Json<Value>                      // 手拼 JSON

// 不一致 B：错误结构
{
  "error": "not found"          // 字符串
}
{
  "ok": false,
  "error": {"code": 404, "message": "..."}  // 对象
}
```

**建议**：

1. **统一信封**（与方向二文档反馈相同）：

```rust
// 成功
{ "ok": true, "data": ..., "meta": { "total": 100, "page": 1 } }
// 失败
{ "ok": false, "error": { "code": "NOT_FOUND", "message": "..." } }
```

2. **`ApiResult<T>` 作为全局 Response 类型**——所有 handler 返回 `ApiResult<Json<T>>`，错误由 `IntoResponse` 统一栈转换

**影响**：大规模修改约 100+ handler，但可以通过宏或 `Box<dyn IntoResponse>` 渐进式过渡。

### 3.2 WS 协议演进机制

建议在 `ws/mod.rs` 的 `connect` handler 中加入：

```rust
// 协商
let version: u8 = query_params.get("v").and_then(|s| s.parse().ok()).unwrap_or(1);
if version < MIN_SUPPORTED || version > MAX_SUPPORTED {
    return ws.reject(StatusCode::UPGRADE_REQUIRED);
}
```

帧定义增加版本感知：

```rust
pub enum ServerFrame {
    #[serde(rename = "msg")]
    V1(ServerFrameV1),
    #[serde(rename = "msg_v2")]
    V2(ServerFrameV2),
}
```

`Hub::fan_out_raw` 按 subscriber 版本分发：

```rust
// 在 Subscriber struct 中
struct Subscriber {
    tx: mpsc::Sender<RawFrame>,
    ws_version: u8,
    // 连接时协商，存活期间不变
}
```

### 3.3 事件/命令的 CQRS 边界

当前系统没有显式区分 command 和 event。`send_message` 是 command（期望返回确认），`RoomEvent::Message` 是 event（下游扇出）。

**建议添加 `CommandResult` 枚举**：

```rust
pub enum CommandResult {
    Accepted { message_id: MessageId, seq: u64 },
    Rejected { reason: RejectionReason },  // 审核/限流/配额
    Deferred { estimated_ms: u64 },         // async moderation
}
```

当前 `send_message` handler 直接 insert DB + publish event + 返回 `Message`。有了 `CommandResult`，handler 可以：

1. DB insert → 获取 message_id
2. 如果 moderation 同步拒绝 → 返回 `Rejected`
3. 如果 moderation 异步 → 返回 `Deferred`
4. 否则 → 返回 `Accepted`

这为异步审核路径提供了**前端反馈**——当前如果 `AERO_AI_MODERATION` 开启，发消息返回 `"ok"`，但几秒后消息被软删，用户看到的是消息消失无提示。

---

## 四、技术选型

### 4.1 当前技术栈的风险评估

| 组件 | 风险等级 | 理由 |
|---|---|---|
| str0m 0.19 | **高** | 纯 Rust DTLS-SRTP。代码质量高但用户少，RFC 更新追赶可能滞后。`SfuMediaSession` 无生产实例——真实握手可能暴露未覆盖的边界。 |
| pgvector HNSW | **中** | 文档已点出 recall 退化。HNSW 是增量更新但非增量索引——量变到质变无预警。 |
| `fred` 9 (Redis) | **低** | 主流 Rust Redis 客户端，tokio 原生，`on_error` 机制成熟。 |
| `async-nats` 0.36 | **中** | JetStream API 在 0.36 仍在演进（非 1.0），`consumer` 创建语义在版本间有小变。锁定版本。 |
| `rml_rtmp` | **中** | 非标准 AMF0/AMF3 实现边缘情况可能触发 panic（检查 `unwrap` 使用）。 |
| `sqlx` 0.8 | **低** | 已稳定，`tracing` feature 可选但未启用（见方向 C）。 |

### 4.2 建议引入的依赖

| 依赖 | 用途 | 优先级 | 替代评估 |
|---|---|---|---|
| `moka` / `quick_cache` | 本地缓存（减少 Redis 穿透，支持多节点） | P1 | `moka` 更成熟，`quick_cache` 更轻。建议 `moka`。<br>替代：手写 `HashMap + RwLock`——太原始，无 TTL 逐出 |
| `tracing-opentelemetry` + `opentelemetry-otlp` | 结构化 tracing 到 OTLP collector | P1 | 自建 span 采集器 → 不必要，OTLP 是标准 |
| `utoipa` 6.x | OpenAPI 生成 | P2 | `aide` 备选，但 `utoipa` 与 Axum 0.7 的 `#[utoipa::path(...)]` 宏集成更直接<br>手写 OpenAPI → 不可维护 |
| `governor` | 速率限制（替换/补充 `ws_rate.rs` 的 Redis 窗口） | P3 | 当前 Redis 窗口工作良好；`governor` 提供更细粒度的 GCRA token bucket<br>**当且仅当**需要 per-user 而非 per-workspace 速率时才引入 |

### 4.3 自建 vs 采购的决策

| 功能 | 建议 | 理由 |
|---|---|---|
| 实时 SFU | 自建（str0m） | 纯 Rust，可控，定制性强。WebRTC gateway 无成熟 Rust 替代 |
| 移动推送 | 自建（`aero-push`） | FCM/APNs 的 HTTP API 简单，不需要第三方推送 SDK |
| 多租户 quota | 自建 | 与现有 Redis + PG 深度集成，商业 quota 平台 overshoot |
| 视频转码 | **不建 / 外包** | 纯 Rust 转码尚不成熟（无 ffmpeg 替代层），直接调 ffmpeg CLI 或用外部 transcode service |
| 文档/知识库 | 采购 | Notion API / Confluence 集成比自建知识库产品快 10x。自建 ≈ 重构一个 Notion |

---

## 五、实施路线图

### 优先级排序

```
P0 (立即，0-4 周)
  └ WS token 日志安全（Sec-WebSocket-Protocol 迁移 阶段 1）
  └ SAML 签名验证（关 fail-open 洞）
  └ PAT scope 模型（M2 工作，阻断后续 PAT 相关 feature）

P1 (短期，4-12 周)
  ├ 统一响应信封（ApiResult 标准化）
  ├ WS 协议版本协商（?v=N + Hub 版本分发）
  ├ 本地缓存层（moka → 多节点就绪）
  ├ 一致性契约文档化（docs/specs/consistency.md）
  └ 三个 media pipeline 的 SLI 仪表化

P2 (中期，12-24 周)
  ├ 鉴权层统一（Actor 模型）
  ├ Plugin trait + 内部插件化
  ├ 多租户配额体系（绑定价需求）
  ├ Call bridge 生产接线 + 熔断器
  └ OpenAPI 自动生成（utoipa）

P3 (远期，24+ 周)
  ├ 多节点部署拓扑（NATS 集群 + S3 + LB 黏性会话）
  ├ 第三方应用平台（workspace install MVP）
  └ API 版本前缀（/api/v1/）
```

### 关键里程碑

| 时间窗口 | 里程碑 | 验证指标 |
|---|---|---|
| T+2 周 | WS token 安全 + SAML 验证 | `nikto` scan 无 query string token 泄露；SAML 断言无签名则 401 |
| T+4 周 | API 信封统一完成 | 所有 handler 返回 `ApiResult<Json<T>>`，web 端错误显示统一 |
| T+6 周 | WS 协议版本协商就绪 | 改变 `ServerFrame::V1` 格式 → 旧 SPA 连接用 `?v=1` 仍工作 |
| T+12 周 | 媒体管道 SLI 可监控 | P50/P99 延迟 + error budget dashboard 上线 |
| T+16 周 | Actor 鉴权模型迁移 50% | `authz_lint` 覆盖率从源码扫描提升到编译期 trait bound |
| T+24 周 | 两节点 E2E 高可用验证 | 节点故障时 WS 重连 + call bridge 重路由 < 5s |

### 风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| **str0m 在真实网络下暴露 bug** | 中 | 高（通话/直播不可用） | 先做 SFU 单节点 E2E 测试（浏览器 OBS→server→HLS），再拆多节点 |
| **PG 迁移锁表导致停机** | 低 | 高 | 所有迁移 `CREATE INDEX CONCURRENTLY` + 非业务低谷跑。加 CI check `migration_lint` 拒绝 `CREATE INDEX` 不带 `CONCURRENTLY` |
| **`async-nats` 0.36→0.37 破坏性变更** | 中 | 中（编译期发现，但修复需时间） | vendor `Cargo.lock` + 升级前在 CI staging 全量 run 集成测试 |
| **多租户配额检查成新 DoS 面** | 中 | 中 | 配额值走 Redis 缓存（TTL ≤ 60s），PG 兜底；配额 middleware 本身限流 |
| **人手不够覆盖 5 个方向** | 高 | 高 | P0/P1 方向限定 1-2 个同时在途；P2/P3 方向写 spec 但等资源 |

---

## 总结

Aero IM 的架构在**领域切割**和**事件驱动**方面达到了较高的成熟度，但**API 契约化**、**鉴权统一**、**媒体管道可靠性**是三块最值得投入的进深领域。

文档提供的 5 个方向分析整体质量很高，我与它的主要分歧在**优先级排序**上——安全（WS token、SAML、PAT scope）和**API 卫生**（信封统一、WS 版本化）应优先于配额和第三方平台，因为：

1. 安全漏洞的修复窗口是**现在**，配额的定价窗口是**未来**
2. 统一信封的成本是**一次性迁移**（约 1-2 周），WS 版本化的成本是**粘合层代码**（约 1 周），两者门槛都很低
3. 第三方平台在外部开发者出现之前是**0 业务价值**，但 API 卫生在第一天就改善开发者体验

建议接下来：**用 4 周集中处理 P0/P1 项**，同时起草 P2 的详细 spec 和迁移计划。
