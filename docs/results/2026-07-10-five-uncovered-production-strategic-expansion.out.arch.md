以下是我基于输入文档和既有系统架构（`AGENTS.md`、代码库扫描）的架构分析。

---

# 架构分析报告：Aero IM — 5 个高价值扩展方向

> 分析基准：`docs/requirements/2026-07-10-round-n-plus-1-scan-five-underexplored-high-value-extensions.md`  
> 参考上下文：`AGENTS.md` crate 地图 + 事件 DAG + 常驻智能体清单 + 全局约束  
> 视角：系统架构评审 + 技术战略

---

## 1. 架构评估

### 1.1 当前架构的核心优势

**事件总线的纯粹性** 是系统最大的架构资产。`RoomEvent` → NATS JetStream → `Hub::fan_out_raw` → WebSocket 的管线已经经过两代验证：

- 每个 `RoomEvent` 变体都走统一的 serde tag 解码 + `seq` 戳 + durable consumer
- 广播与点对点通知共享同一扇出路径，`NotifyBatch` 展开机制避开了 O(N) NATS publish
- 总线消费者（bot / push / moderation / unfurl / transcribe / ooo）均以第一个类 citizen 挂载在 `im.room.*` subject 上，新 bot 只需加一个新 consumer，对既有消费者零影响

**crate 分层（基础 → IM → 直播 → 组合）** 严格自下而上无成环依赖，这是可扩展性的制度保障。新功能要么落在既有 crate 内（`aero-storage::x` + `aero-server::x` 两条腿），要么开新 crate 挂在新领域。`routes/routes.rs::build()` 的一处装配点降低了路由管理的认知负载。

**NATS + Redis 的分工** 精准：NATS 持跨实例**事件流**（durable + ephemeral），Redis 持跨实例**状态**（presence / viewer / roster sorted-set）。进程内 `Hub` 只做本地扇出，不持跨实例状态——这意味着水平扩展不需要修改 data plane。

### 1.2 架构的局限性（与 5 个新方向直接相关的）

**1.2.1 消费者模型的「订阅者身份」未抽象**

当前所有 bot（`agent_bot`、`unfurl_bot`、`transcribe_bot` 等）是以**第零类参与者**硬编码在 `bin/boot/background.rs`。每个 bot 有自己的 NATS consumer、自己的 module 入口。新 bot = 新 crate 或新 module + `tokio::spawn`。没有统一的 `BotTrait` / `WorkflowStep` 注册表。

这对方向 **5（工作流自动化引擎）** 是结构性障碍：一个用户可配置的工作流需要动态注册 trigger → action 对，不能在 `background.rs` 硬编码。

**1.2.2 推送网关的 `PushGateway` trait 约束不足**

```rust
// push_gateway.rs 的 trait 设计（推测结构）
trait PushGateway {
    async fn send(&self, token: &PushToken, payload: &PushPayload) -> Result<()>;
}
```

当前实现：`FcmGateway`、`ApnsGateway`、`FakeGateway`。direction **1（Web Push）** 要加 `WebPushGateway`——VAPID 签名 + 不同浏览器适配 + `pushsubscriptionchange` 处理，这个 trait 可能已经够用。但 direction **4（紧急广播多渠道兜底）** 需要的 `SmsGateway` / `TtsPhoneGateway` 的接口语义完全不同（异步状态回调、送达回执、按运营商计费）。单向 `send()` 返回 `Result` 的模型不适合短信/电话。

**1.2.3 消息模型缺少「非房间消息」的原语**

系统当前所有实时事件以 `RoomEvent` / `StreamEvent` 为根。**没有独立于房间的「平台事件」**（system-wide notification / urgent broadcast / workflow trigger event）。direction **4（紧急广播）** 的 `im.broadcast.*` 需要一个房外总线 subject，其事件格式不共享 `RoomEvent` 的 `kind` tag 和 `room_id`。这不是修改既有模型能解决的——需要新的 event enum 和新的 hub fan-out 路径。

**1.2.4 路由层无版本化预留**

所有 API 路由在 `/api/` 下无版本前缀。即便 Phase A 只做 `301 redirect /api → /api/v1`，这也意味着：

- Axum 的 `Router::new().nest("/api", ...)` 需要改成 `nest("/api/v1", ...)` + 冗余旧路径
- 现有客户端（web SPA、第三方 bot API 调用者、内部 bot dispatch 的 HTTP 回调）全部断裂，直到 redirect 到位
- 不能渐进地只版本化部分路由——要么全有要么全无

这是方向 **3（开发者平台）** 的先决条件，也是**最大的兼容性负债**。越晚做，迁移成本越高。

**1.2.5 定时器的资源隔离粒度粗**

当前 `interval` 清扫全部在 `bin/boot/` 的 `tokio::spawn` 中运行。方向 **5 的工作流引擎** 需要一个按工作区隔离的定时器调度器（`scheduled_time` 触发），而非一个全局 `tokio::spawn`。如果多个工作区的定时工作流共享一个 tokio 任务，一个工作区的死循环工作流会阻塞其他工作区的定时执行。

### 1.3 架构债务 / 技术债

| 债务项 | 严重度 | 影响方向 |
|--------|--------|---------|
| 路由无版本前缀，迁移窗口收紧 | 🔴 高 | 方向 3（SDK）、方向 5（webhook 扩展） |
| `webhook.rs` 事件过滤器硬编码只送 `Message` | 🟡 中 | 方向 3（webhook 扩展需求） |
| `PushGateway` trait 单向模型不够表达多渠道兜底 | 🟡 中 | 方向 4（SMS/电话兜底） |
| Bot 注册机制无 trait/registry，只能硬编码 | 🟡 中 | 方向 5（工作流动作执行） |
| `common/` 下的 `Config` 缺少动态重载能力 | 🟢 低 | 方向 4（广播冷却期配置） |
| `Hub` 的 `stream_watchers` 和 `call_rosters` 是进程内 HashMap，集群广播强制新 subject | 🟡 中 | 方向 2（事件升级跨节点）、方向 4（跨实例广播） |

---

## 2. 扩展方向深度分析

### 2.1 方向 1：Desktop Web Push（桌面浏览器推送补齐）

**为什么需要** — 这不是功能缺口，这是**交付路径缺口**。移动端推送（FCM/APNs）已经完备，桌面端关闭浏览器后通知归零。对于 IM 核心价值「在关键时间到达关键人员」，桌面端关闭后的遗漏意味着 Aero IM 在桌面场景不如一个浏览器 tab 常驻的 Slack 网页版。

**核心挑战**：

1. **多浏览器密钥格式差异** — Chrome 使用 VAPID（RFC 8292），Firefox 使用不同的应用服务器公钥编码，Safari 16.4+ 使用 APNs 作为推送服务。单靠 `web-push` crate（仅覆盖 VAPID）不足以解决。需要一个 `BrowserVendor` 枚举来决定 push endpoint 适配逻辑。

2. **Subscription 生命周期管理** — `pushsubscriptionchange` 事件的绑定、Token 刷新上报、权限撤销的级联清理。当前 `PushToken` 模型（`push_tokens.rs`）假设 token 是静态字符串，不处理 subscription 的自动轮换。

3. **重复通知消除** — Web Push 送达时，如果用户恰好在活跃 SPA 标签页中，WS 帧已经产生了通知 → Web Push 再送来一个重复。需要 Service Worker 收到 push 事件后先 `postMessage` 询问各标签页是否已在显示该消息。

**对现有系统的架构影响**：

```
┌─────────────────────────────────────────────┐
│ 现有：push_bot ─→ PushGateway ─→ FCM/APNs     │
│               └── PushPlatform::WebPush = None │
│                                               │
│ 修改后：push_bot ─→ PushGateway             │
│                    ├── FcmGateway             │
│                    ├── ApnsGateway             │
│                    └── WebPushGateway (新增)    │
│                                               │
│ web/ 新增：sw.js + manifest.json              │
│ index.html: SW注册 + push subscription 上报    │
└─────────────────────────────────────────────┘
```

估计影响范围：
- `aero-push`: 加 `WebPushGateway` 实现（~300 行），修改 `PushToken` 模型支持 `p256dh`/`auth`/`endpoint`
- `aero-storage`: 可能不需要改——`push_tokens` 表的 `token` 字段可存入 subscription JSON
- `aero-server`: `push_bot.rs` 加 `WebPush` branch——payload 截断 ≤ 4096 bytes
- `web/`: 新文件 `sw.js` + `manifest.json` + 修改 `index.html` + `app.js`

**风险与 mitigate**：

| 风险 | 概率 | mitigate |
|------|------|----------|
| 不同浏览器推送端点 URL 格式不一致 | 高 | 路由到 `WebPushGateway` 时按 URL domain 分发到浏览器特定实现 |
| SW 注册失败（HTTP-only 站点限制） | 中 | HTTPS 要求已在部署文档强调；开发环境允许 localhost |
| 用户撤销权限后 token 未删除 | 中 | Web Push gateway 收到 `410 Gone` 时触发 token 清理 |
| Phase B 的 `urgency`/`topic` 头浏览器支持不一 | 低 | 降级忽略，不影响基本推送 |

### 2.2 方向 2：应急响应与值班管理

**为什么需要** — 这是从「IM 平台」到「关键任务平台」的跨越。现有基础设施（NATS 扇出、Presence、通话、AI 复盘）已经为事件协作铺好了管道，上层缺少的是**轮值定义**、**升级链**、**事件记录**。客户粘性极高——一旦一个组织的应急流程围绕 Aero IM 建立，替换成本不可逆。

**核心挑战**：

1. **值班轮换模型** — 需要覆盖：每周/每日固定轮换、节假日例外、两个人的 round-robin、按角色（先找 SRE 组，组内按权重找可用者）。这比 cron 表达式复杂——需要一个至少覆盖 `(轮换周期，参与人列表，首选人，备选人，起始日期)` 的 `OnCallSchedule` 模型。

2. **升级链的超时语义** — 升级不是简单「第 1 级 N 分钟后通知第 2 级」。而是：**发高优推送 → 等待 ack → N 分钟内未 ack → 再发 + 通知下一级**。ack 需要持久化（`incident_ack` 表），超时检测需要精确的 `tokio::time::sleep`（非 `tokio::spawn` interval 扫描）——每个 active incident 一个 cancellation-safe 的定时器。

3. **确认送达的可靠性缺口** — 当前 FCM/APNs/Web Push 全部是 best-effort。应急场景需要「发后确认送达，否则升级」。这需要对现有 `push_bot` 的 send 流程增加**送达回执等待**。但 FCM/APNs 不保证送达回执——所以升级链必须假设推送是 best-effort，以**未 ack 作为升级触发器**，而非以「未送达」。

**对现有系统的架构影响**：

```
┌─────────────────────────────────────────────────┐
│ 新增 crate (aero-incidents 或放在 aero-server 内) │
│ ├── incidents/ (repo + service + routes)        │
│ ├── on_call_schedules/ (轮换逻辑)                │
│ ├── escalation_chain/ (升级链 + 超时定时器)       │
│ └── incident_channel/ (自动创建专用频道)          │
│                                                 │
│ 修改：                                          │
│ ├── aero-server/commands.rs → 加 !ack / !call   │
│ ├── aero-storage/org_chart.rs → 扩展汇报链查询  │
│ └── aero-push → 加 high_priority 标志            │
└─────────────────────────────────────────────────┘
```

**Phase B 的 OnCallProvider trait 设计建议**：

```rust
trait OnCallProvider: Send + Sync {
    /// 将内部 incident 同步到外部系统
    async fn sync_incident(&self, incident: &Incident) -> Result<()>;
    /// 将外部 incident（PagerDuty webhook 回调）映射为内部事件
    async fn ingest_event(&self, raw: &[u8]) -> Result<Option<ExternalEvent>>;
}
```

这保持了 External Provider 作为 plugin 的接入点，主流程不依赖外部系统。

### 2.3 方向 3：开发者平台 + 公开 API SDK

**为什么需要** — 这是从「好用的 IM 产品」到「平台」的质变。Slack 的 2400+ 应用生态不是自发产生的，而是因为 Slack 提供了 `@slack/bolt` SDK + Events API + Socket Mode + OAuth。Aero IM 当前的 Bot 系统是企业内部开发者的玩具——要求 Rust 代码 + 注册 participant。对于企业 IT 团队（擅长 Node.js/Python），这门槛太高。

**核心挑战**：

1. **API 版本化的业务连续性** — 从无版本化到 `/api/v1/` 的迁移必须在同一大版本内完成且不断既有流量。考虑到 Axum 的 `Router` 不支持路由前缀的运行时切换，需要：

   ```
   方案 A（推荐）：Router::new()
       .nest("/api/v1", v1_routes)
       .fallback(|req: Request| async move {
           // 如果 path 是 /api/xxx 且不属于 /api/v1 前缀，301 redirect
           Redirect::to(&format!("/api/v1{}", req.uri().path().strip_prefix("/api").unwrap()))
       })
   ```

   这要求所有既有路由代码中的 `"/api/..."` 硬编码路径必须同时保留对 `/api/v1/...` 的响应。

2. **Webhook 事件风暴防冲击** — 扩展 webhook 支持 `Edited`/`Deleted`/`Reaction`/`Membership` 后，一个大工作区（1000+ 成员）可能每秒产生数百次 `Membership` 事件（加入、离开、角色变更）。当前 `webhook_delivery.rs` 是单任务循环，无背压——事件风暴会撑爆内存队列。

3. **OAuth token 广播吊销** — 当用户通过 `/api/me/sessions` 全局登出时，OAuth refresh token + access token 必须全部失效。这需要一个跨实例的 token revocation broadcast 机制（走 NATS subject `session.revoke.{user_id}`）。

**Phase A 推荐路径**：

```
第 1 步（2 周）：路由版本化 + 301 redirect
第 2 步（2 周）：Webhook 扩展事件类型 + subscriber-side 事件过滤器
第 3 步（2 周）：JS SDK 发布（@aero-im/sdk，先覆盖 Message + Reaction + Auth）
第 4 步（并行 2 周）：OpenAPI 文档自动化生成（从 axum router 提取 schema）
```

**不推荐在 Phase A 做**：OAuth 授权码流程。它引入的状态机复杂度（authorization code → access token → refresh token → token rotation）与现有 PAT/JWT 认证模型的共存会引发安全边界的复杂问题。PAT 作为开发者入口已经够用，OAuth 留到 Phase B。

### 2.4 方向 4：紧急广播与大规模通知系统

**为什么需要** — 这是方向 2（应急响应）的前置依赖和补充。方向 2 面向值班人员的事件协作；方向 4 面向**全员**的紧急通知。两者共享 `ack` 确认机制和升级链但没有重叠的表结构。竞争产品（Slack `@channel`/Teams Emergency Calling）对这一场景覆盖很浅——这是 Aero IM 的**差异化窗口**。

**核心架构决策**：独立 subject vs 复用房间 subject

| 方案 | 优势 | 劣势 |
|------|------|------|
| **A（推荐）：独立 `im.broadcast.{workspace_id}` subject** | 消息不混入房间历史；广播 consumer 可以从实例启动；广播消息的 room_id = null 不会破坏既有 RoomEvent 模型 | 需要新的 consumer 管理逻辑；Hub 需新增 `broadcast_watchers` 扇出路径 |
| B：复用 `im.room.{broadcast_channel_id}` + 建一个隐藏频道 | 零新的 consumer；复用全部 RoomEvent 管线 | 广播消息混入历史搜索；频道名册管理复杂；难以实现「确认收悉统计」 |

**推荐方案 A** 的理由：紧急广播消息有一组 RoomEvent 不具备的属性——`severity`、`target_scope`、`confirmations`、`cool_down`。强行塞入 RoomEvent 会让既有代码的所有 match arm 多一个无法处理的变体，污染架构。

**Phase A 表结构建议**：

```sql
CREATE TABLE broadcast_messages (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id UUID NOT NULL REFERENCES workspaces(id),
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    severity TEXT NOT NULL CHECK (severity IN ('urgent', 'important', 'info')),
    target_scope JSONB NOT NULL DEFAULT '{}',  -- { "org_unit": [...], "floor": [...], "role": [...] }
    sender_id UUID NOT NULL REFERENCES participants(id),
    requires_ack BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE broadcast_confirmations (
    broadcast_id UUID NOT NULL REFERENCES broadcast_messages(id),
    participant_id UUID NOT NULL REFERENCES participants(id),
    read_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    acked_at TIMESTAMPTZ,  -- 可选确认回执
    source TEXT NOT NULL DEFAULT 'ws',  -- ws / push / sms / phone
    PRIMARY KEY (broadcast_id, participant_id)
);
```

**广播冷却期的实现位置**：放在 `aero-storage` 的 service 层，调用 `create_broadcast` 前检查 `broadcast_messages` 最近 N 分钟内是否有同 workspace 同 severity 的记录。

### 2.5 方向 5：工作流自动化引擎

**为什么需要** — 这是五个方向中**架构影响面最广、长期价值最高**的一个。它撕开了从 Bot 系统（硬编码 Rust）向「用户可配置自动化」的转型。Slack 的 Workflow Builder 上线后月活用户增速是平台侧 3x。对于 Aero IM，这是客户自服务化的终局。

**核心架构挑战**：

**2.5.1 循环检测** — 最危险的风险。工作流 A `message_posted` → `send_message` → 触发工作流 B → `send_message` → 又触发工作流 A。

建议方案（非 DAG 静态分析，而是运行时保护）：

```
运行时保护层级：
L1. 每个工作流实例携带 workflow_run.depth 计数器 + 最大深度（默认 5）
L2. 每 60s 内同一来源工作流最多触发 N 次（可配置）
L3. `workflow_runs.trigger_event_id` 唯一索引——同一事件不会触发同一工作流两次
L4. `send_message` 动作产生的消息带 `x-aero-workflow-id` header——新事件解析时跳过自身来源
```

静态 DAG 分析（检测编译时循环）对动态配置的工作流不现实——触发条件包含正则匹配、频道过滤等运行时因素。

**2.5.2 执行身份** — `send_message` 动作应该以什么身份发送？

| 方案 | 优势 | 劣势 |
|------|------|------|
| **A（推荐）：system bot 身份（类似 Slack `app`）** | 不占用 participant 身份；可配置头像/名称；Bot 撤回权限统一管理 | 需要在新 `BotRepo` 中注册「工作流引擎」作为内置 bot participant |
| B：创建工作流的管理员身份 | 消息显示为管理员发送，权限自然继承 | 管理员离职后工作流 still runs 但发消息显示已离职用户；消息删除权限随管理员身份消失 |
| C：Bot PAT 身份 | 不需要新的 participant | 创建者需要管理 PAT；PAT 过期后工作流断裂 |

**推荐方案 A**：在 Bot 注册表中新增一个不可删除的 `SystemBot::WorkflowEngine` variant。所有工作流 `send_message` 动作都发送为此 bot 身份。bot 的头像/名称可由工作空间管理员配置。

**2.5.3 触发频率保护与背压** — `message_posted` 触发在高流量频道是危险场景。每个 `workflow` 必须强制配置 `rate_limit`（默认 60/min）。当触发速率超过阈值时，`workflow_dispatcher` 跳过该次触发但要记录到 `workflow_runs.warnings` 供管理员审计。

**架构影响范围**（Phase A 定义）：

```
┌────────────────────────────────────────────┐
│ 新增：                                     │
│ aero-storage/workflow_repo.rs              │
│ ├── workflow_triggers 表操作              │
│ ├── workflow_actions 表操作               │
│ └── workflow_runs 表操作                   │
│                                            │
│ aero-server/workflow_engine/               │
│ ├── mod.rs — dispatcher 入口               │
│ ├── trigger/ — trigger 解析 + 匹配         │
│ ├── action/ — send_message / webhook / ..  │
│ ├── guard/ — 限流 + 循环保护 + 幂等         │
│ └── scheduler/ — 定时工作流调度器           │
│                                            │
│ 修改：                                     │
│ routes/routes.rs — .merge(workflow_routes) │
│ bin/boot/background.rs — spawn 工作流消费者│
│ aero-server/bot_repo.rs — 加 WorkflowEngine│
│ NATS: im.workflow.{workspace_id} subject   │
└────────────────────────────────────────────┘
```

---

## 3. 接口设计建议

### 3.1 新增抽象层的时机

五个方向中，**方向 3 和方向 5 需要新的抽象层**，方向 1、2、4 在现有抽象内扩展即可。

**方向 5 需要的新抽象**：

```rust
// 工作流触发器的统一接口
#[async_trait]
trait WorkflowTrigger: Send + Sync {
    fn trigger_type(&self) -> TriggerType;  // MessagePosted / MemberJoined / StreamStatus / Scheduled
    async fn evaluate(&self, ctx: &EventContext, config: &serde_json::Value) -> Result<bool>;
}

// 工作流动件的统一接口
#[async_trait]
trait WorkflowAction: Send + Sync {
    fn action_type(&self) -> ActionType;
    async fn execute(&self, ctx: &ExecutionContext, config: &serde_json::Value) -> Result<ActionResult>;
}
```

这样工作流的 trigger 和 action 可以独立注册，无需修改 `workflow_engine/` 核心代码即可扩展新类型。Phase A 只要 2-3 种 trigger + 2-3 种 action，trait 设计预留扩展点。

### 3.2 关键模块的向后兼容原则

**Webhook 事件扩展** — 不能破坏既有 webhook 消费者。现有 webhook 的 event type 过滤器（只送 `Message`）保持不变；新增事件类型的消费者通过新的配置字段选择加入：

```json
// 新增 webhook 的配置
{
  "url": "https://...",
  "events": ["message.created", "message.edited", "member.joined", "broadcast.sent"]
  // 老配置 { "url": "..." } 等价于 events = ["message.created"]，向后兼容
}
```

**API 版本化** — Axum 的 `nest` 层级不能出现 `"/api/v1"` 前缀和 `"/api"` 前缀的**路由键冲突**。旧 web url 应从 `/api/xxx` 重定向到 `/api/v1/xxx`，而非两套都服务。

### 3.3 跨方向共享的接口契约

五个方向之间有一些接口交叉，应在设计时对齐：

| 共享接口 | 涉及方向 | 设计原则 |
|---------|---------|---------|
| `PushGateway` 扩展 | 1, 2, 4 | `send` 增加 `priority` 参数；`send` 返回新增 `delivery_status`（wip / delivered / failed） |
| `ack` 确认模型 | 2, 4 | 方向 4 的 `broadcast_confirmations` 和方向 2 的 `incident_ack` 可以共享同一个 `Acknowledgement` trait（但不共享表——领域不同） |
| 通知频率限制 | 1, 4, 5 | 方向 5 的 `rate_limit` 逻辑可直接用于方向 4 的广播冷却期（需要泛化当前的 `KeyedCostBudget` 模型） |
| Bot 身份注册 | 3, 5 | 方向 5 的 `SystemBot::WorkflowEngine` 和方向 3 的 OAuth App identity 应该共享 Bot 注册表 |

---

## 4. 技术选型

### 4.1 新依赖引入的评估

| 方向 | 可能的第三方依赖 | 评估 |
|------|----------------|------|
| 1（Web Push） | `web-push` crate | 轻量、纯 Rust、支持 VAPID 和 RFC 8292。但是 Firefox/Apple 的端点差异需要自己处理，crate 只处理 Chrome。推荐引入，外加 `BrowserVendor` 适配层 |
| 1（SW） | 无（Service Worker 是浏览器 API） | 不需要新 dep |
| 2（值班） | 无（核心逻辑自建） | PagerDuty/Opsgenie SDK 是 Phase B，不提前引入 |
| 3（SDK） | `utoipa`（OpenAPI 自动生成） | 推荐。已有 `openapi.rs` 是手写的，维护成本高。`utoipa` 用 proc macro 标注现有 route handler，自动生成 OpenAPI 3.0. 但不是零成本——每个 handler 需要加 `#[utoipa::path]`。如果是渐进式引入，可以先只对新加的 `/api/v1/` 路由标注，旧路由沿用手写 doc |
| 3（JS SDK） | `ts-sdk` 无 Rust dep | 纯 JS/TS 项目，发布到 npm。不引入 Rust 依赖 |
| 4（广播） | 无（逻辑自建） | 短信网关 SDK（Twilio/阿里云 SMS）是 Phase B |
| 5（工作流） | `rhai` 或 `rhai-dylib` 作为用户脚本引擎？ | **不推荐引入**。Phase A 的触发条件是声明式配置（`{type: "message_posted", keyword_match: "..."}`），不需要脚本引擎。Phase C 的可视化编排建议用 `serde_json::Value` 表示 DAG，不引入 DSL 或脚本。 |

### 4.2 自建 vs 采购

| 能力 | 自建 | 采购 | 决策依据 |
|------|------|------|---------|
| Web Push 网关 | ✅ 自建（约 300 行） | — | 标准化 RFC 8030，无成熟 Rust sdk 能做到全浏览器；自建可控 |
| 值班轮换引擎 | ✅ 自建（核心约 500 行） | — | PagerDuty 的 `on_call` 模型许可证昂贵且不开放；轮换逻辑不复杂 |
| 短信/TTS 网关 | — | ✅ 采购 Twilio / 阿里云 SMS | 运营商接入、资质、短信模板审核是运维非技术问题；通过 trait 抽象可切换供应商 |
| Incident 管理与 Auditing trail | ✅ 自建 | — | 数据主权要求；企业客户要求 incident 数据留在自己的数据库 |
| 工作流可视化编排 UI | — | ⚠️ 可选商业组件（如 React Flow） | React Flow 是 MIT 协议（免费）的商业组件，不要自己画 Canvas；低于 5 付费客户用 API 配置就够了 |

## 5. 实施路线图

### 5.1 优先级与阶段划分

基于输入文档的共识 + 我的架构评估：

```
P0（立即，并行）：
├── 方向 1 Desktop Web Push（2-4 周，低风险高影响）
│   └── 依赖：无（独立交付）
│
├── 方向 4 紧急广播 Phase A（4-6 周，差异化前提）
│   └── 依赖：无（但需要新的 NATS subject 设计）
│   └── 前置：确认 AGENTS.md 的 bus/seq.rs 支持独立 subject seq

P1（下一季，顺序）：
├── 方向 5 工作流自动化引擎 Phase A（8-10 周）
│   └── 前置：方向 4 的广播 subject + Bot 注册表扩展
│
├── 方向 2 应急响应 Phase A（6-8 周）
│   └── 前置：方向 4 的 ack 确认模型可复用
│   └── 注意：不要与方向 5 的 trigger 逻辑重叠（方向 2 用内置 schema，方向 5 用可配置 trigger）

P2（长期）：
├── 方向 3 开发者平台 Phase A（6-8 周，API 版本化 + JS SDK）
│   └── 前置：方向 5 的 Bot 注册表
│   └── 并行：方向 3 Phase B（OAuth + Playground，4-6 周）
└── 方向 5 Phase B-C（迭代增强）
```

**为什么方向 5 先于方向 3**：方向 5 的 Phase A（触发-动作管线）与方向 3 的 API 版本化无明显依赖关系，且方向 5 的 Bot 注册表扩展是方向 3 OAuth 的先决条件。方向 3 的 Phase A 可以滞后，但**不能晚于方向 5 的 Phase B**——因为方向 5 的 webhook action 需要方向 3 的 webhook 扩展。

### 5.2 阶段交付物

| 阶段 | 交付物 | 验收条件 |
|------|--------|---------|
| **Phase 0**（2 周） | 方向 1 Web Push 上线 | Chrome/Firefox/Safari 关闭 Aero IM 标签页后，收到 @mention/未接来电/开播推送通知 |
| **Phase 1**（4 周） | 方向 4 Phase A 紧急广播 | admin 可发紧急广播全公司；广播消息通过独立 subject 扇出；广播冷却期生效 |
| **Phase 2**（6 周） | 方向 5 Phase A 工作流引擎 | 用户可配置 `message_posted` → `send_message` 规则；不超过 N 个并行工作流；循环检测在深度 > 5 时中止 |
| **Phase 3**（6 周） | 方向 2 Phase A 值班 + 升级 | 可创建值班轮换（周/日）；事件频道自动创建；`!ack` 支持；N 分钟未 ack 自动@上级 |
| **Phase 4**（6 周，并行） | 方向 3 Phase A API 版本化 + JS SDK | `/api/v1/*` 路由就绪；旧路由 301；JS SDK 可收发消息；OpenAPI 文档自动生成 |
| **Phase 5**（长期迭代） | 方向 5 B-C + 方向 3 B + 方向 2 B | 可视化编排 UI；OAuth 授权码流；PagerDuty 双向同步；公开状态页 |

### 5.3 关键风险与缓解策略

| 风险 | 影响方向 | 概率 | 缓解策略 |
|------|---------|------|---------|
| 方向 1 的 Web Push HTTPS 强依赖导致开发环境困难 | 1 | 🟢 低 | 明确 `docs/development.md` 中 localhost 豁免；CI 用 HTTPS-proxy 容器 |
| 方向 4 广播 subject 与现有 RoomEvent consumer 模型冲突 | 4 | 🟡 中 | 在 `common/src/event.rs` 新增 `BroadcastEvent` enum（独立 tag `kind="broadcast"`），`Hub` 新增 `broadcast_watchers: DashMap<...>` |
| 方向 5 循环检测在 Phase A 不可证明地完备 | 5 | 🔴 高 | 接受「运行时保护 + 最大深度」的实用方案，不做静态 DAG 分析。Phase A 的循环检测文档定义为「尽力检测 + 熔断」而非「证明无环」 |
| 方向 3 API 版本化的 301 重定向在 nginx 反代后失效 | 3 | 🟡 中 | 文档明确提示 nginx 配置需 `proxy_set_header Host $host` 保证 Axum 的 redirect 正确生成 Location |
| 五个方向同时进行导致 crate 间耦合失控 | 全部 | 🟡 中 | 每个方向在其独立的 crate 或 server 子模块中开发；`routes/routes.rs` 一处装配修改；每周 `cargo check --workspace` 干净 |

### 5.4 不做 / 延迟做的事物

- **方向 5 Phase C 的可视化编排 UI** 推迟到 5+ 付费客户出现。Phase A 用 JSON 配置已经足够——Slack Workflow Builder 也是从 API-only 起步。
- **方向 3 的 Socket Mode**（无需公开端口的 bot 运行模式）推迟——当前 `AERO_BOT_API_URL` 要求 bot 有公网回调地址的设计够用。
- **方向 4 的短信/TTS 网关** 推迟到 Phase B——应急广播的 MVP 可以通过 Web Push + FCM/APNs 覆盖大多数用户，短信兜底是锦上添花。
- **方向 2 的公开状态页** 推迟到方向 4 的广播系统稳定后——状态页可以视为「公开只读版」的广播消息列表。

---

## 总结

五个方向中最有架构复利效应的是**方向 5（工作流自动化引擎）**——它建立的新抽象（`WorkflowTrigger` / `WorkflowAction` / Bot 注册表）将成为方向 2（值班升级链）和方向 3（SDK webhook 扩展）的基础设施。建议执行顺序如下，既避免过载又最大化架构正交性：

```
Phase 0 [2周]  方向1（Web Push）—— 低风险快赢,补齐桌面推送缺口
Phase 1 [4周]  方向4（紧急广播）—— 独立subject + ack确认,为方向2铺路
Phase 2 [6周]  方向5 Phase A（工作流引擎）—— 构建自动化抽象层
                └── 方向2（值班）可复用ack模型 + 方向5的Bot注册表
Phase 3 [6周]  方向2（应急响应）—— 站在方向4和方向5的肩膀上
Phase 4 [6周]  方向3（开发者平台）—— API版本化 + SDK
                └── 方向5的Bot注册表是OAuth的前置条件
```

**一个重要的架构建议**：方向 4 的 `BroadcastEvent` 和方向 5 的 `WorkflowTriggerEvent` 在 `common/src/event.rs` 中应作为**独立的 enum（非 RoomEvent 变体）**。RoomEvent 的 serde tag `kind` 机制是房间事件的强约定——广播和工作流事件的字段集合不同，强行合并会让 `ImService::publish_room_event` 的 match 臂膨胀到处理不属于房间的变体，这是架构腐蚀的开始。独立 enum 意味着独立的 NATS subject tree、独立的 Hub 扇出路径、独立的 consumer 管理——这正是目前架构的分层优势所在。
