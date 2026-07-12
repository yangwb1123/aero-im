# Aero IM — 架构深度分析

*基于代码库直接验证（121 route 模块、157 个迁移、16 个 crate、~6000 行前端 JS）和战略反馈文档的校正数据*

---

## 1. 架构评估

### 1.1 现有架构的核心优势

**分层 crate 架构相当干净**。依赖方向自下而上（`aero-common` → `aero-bus`/`aero-storage` → `aero-im-core` → `aero-server`），没有环，这是 Rust workspace 治理中最难做到的事情之一。每个 crate 有清晰的职责边界：

| crate | 设计质量信号 |
|---|---|
| `aero-common` | 叶子 crate，被全系统引用，无内部依赖。`RoomEvent`/`StreamEvent` tagged-enum + `define_id!` 宏是正确抽象 |
| `aero-bus` | `EventBus` trait 设计优良——`dyn`-compatible，`publish_json` 有 `Self: Sized` 守卫，测试用 `MockBus` 可互换 |
| `aero-storage` | 按功能划分 `XRepo`（`BotRepo`/`RoomRepo`/`MessageRepo`），每个带 `db_tests #[ignore]` 门控——可测试性设计到位 |
| `aero-im-core` | `ImService` 是携带 `EventBus` 的编排层，不直接耦合 NATS——可在单元测试中注入 `MockBus` |

**事件 DAG 设计的正确性**：事件生产 → NATS JetStream 跨实例扇出 → 进程内 `Hub` bounded `mpsc` → WS 帧。这个设计的优点：

- **有界背压**：每连接 `mpsc` channel 有界 + `disconnect_on_full` 策略，防止慢消费者造成内存膨胀
- **DashMap 而非 RwLock<HashMap>**：AGENTS.md 已正确更新。DashMap 分片锁对高频扇出场景（直播弹幕/礼物/hype train）更友好
- **at-least-once 语义**：durable consumer cursor + per-subject seq 戳 + 客户端 `SeqGate` 去重，三件套是正确且完整的

**迁移嵌入编译期**：`sqlx::migrate!("../../migrations")` 在 build 时嵌入全部 157 个迁移。虽然约束了开发流程（必须先 build 再 migrate），但避免了运行期迁移文件丢失这个更危险的类。

### 1.2 架构债务与技术债

#### 前端/后端严重不对称（P0 架构债）

121 个 `pub fn routes()` 模块 vs 约 30 个前端 JS DOM 交互模块。服务端覆盖率约 25-30%，且**不是"缺 UI"这么简单**，而是：

- **无前端模块注册层**：`web/app.js` 的 `import` 链是扁平的，无模块发现机制。新增后端模块需要手动在至少 3 个文件（`routes.rs` + `app.js` import + DOM handler）中添加引用
- **debug client 标签**：`index.html` 标题 `"debug client · 联调专用"` 本身就暗示了前端不被作为一等公民对待
- **无前端错误边界**：`app.js` 中模块初始化的失败会静默吞掉而未通知用户

**建议**：这不是逐个补 UI 能解决的问题。需要先加前端功能发现层，再建立前端覆盖率监控，再按优先级补高价值模块。

#### WebSocket 连接管理缺少移动端生命周期适配（P1 技术债）

当前 `ws.js` 假设永久在线连接。移动端需要在以下场景中管理连接：

| 场景 | 需要的行为 | 当前行为 |
|---|---|---|
| `pagehide` | 优雅关闭 WS，保存最后 cursor | 无处理 |
| `pageshow` / 前台恢复 | 重建 WS + `?since=` 回填 | 由 `_scheduleReconnect` 接管，但可能延迟过高 |
| `visibilitychange` → hidden | 延长心跳间隔到 60s | 心跳固定 25s |
| 后台被系统 kill | 利用 `navigator.sendBeacon()` 发最后心跳 | 无处理 |

如果要短期做 mobile SPA（而非 Tauri 壳），这个比 CSS 响应式更关键——一个在移动端频繁断开/重连的 WS 连接会给 NATS durable consumer 造成不必要的重投压力。

#### bus listener cursor 的跨区域短板（P2 架构债）

`run_bus_listener` 使用 **durable consumer** `aero-server`。这在单集群中是正确的——NATS 故障恢复后 cursor 仍在。但在 leafnode 跨区域拓扑中，每个 leaf 有自己独立的 cursor。当前代码无 region 前缀在 subject 中，也无 region 标签在 `EventBus` trait 中。这意味着：

- 区域 A 的消息被区域 B 的 consumer 消费后，区域 B 的 cursor 前移
- 区域 A 的节点可能因 cursor 不同步而错过消息
- 如果加 region 维度，`publish_room_event` 的 subject 模式需要从 `im.room.{id}` 变为 `{region}.im.room.{id}`，消息需要跨 region 复制

这不是当前的功能需求，但在考虑多区域部署时应作为已知约束记录。

#### 限流配置前缀的不一致（P3 小债）

AGENTS.md §4.3 已记录：大部分配置用 `AERO__SECTION__KEY`（双下划线），但限流用 `AERO_RATE_LIMIT_PER_SEC`（单下划线）。这是 figment `Env::prefixed("AERO__").split("__")` 的产物，限流变量为历史原因保留单下划线。这不是高优先级，但在运维文档中需要明确标注。

---

## 2. 扩展方向

### 方向一：Bot/App 开放平台 — P0（反馈调整为最高优先级）

#### 为什么是 P0

这是**反馈文档明确指向的 ROI 最高方向**。原因是代码管线已经成熟到"只差一层 OAuth 包装"：

- `BotRepo::create` 已存在（`aero-storage/src/bot.rs`），bot 作为 `kind='bot'` 的 participant 创建
- `bot_dispatch.rs` 已实现完整的事件匹配引擎（`event_type` + `room_id`/`workspace_id`/`action_id` 过滤器）
- `BotDelivery` 已记录投递结果（`bot_subscription_deliveries` 表）
- `block_interactions.rs` 已处理 `Button`/`Select` 点击，写入 `block_interactions` 表 + 广播 `RoomEvent::Interaction`
- `commands.rs` 的 `COMMANDS` lazy static 已为 slash-command 扩展提供了注册入口

#### 核心挑战

**缺的是一层 OAuth app install 流**，而非基础设施：

1. **Bot manifest/注册 UI**（2 周）：当前 bot 创建只走 API（`POST /api/bots`），缺一个前端注册页 + bot 安装流程
2. **Interactive callback URL 补全**（1 周）：`block_interactions.rs` 广播 `RoomEvent::Interaction` 但不回调 bot URL。需要把 `Interaction` 事件喂入 `bot_dispatch` 管线，加上 webhook POST
3. **Bot token OAuth scope 模型**（1-2 周）：当前 bot token 是全局可用的 bearer credential。需要加 scope 限制（如 `read:message`/`write:message`/`reaction:read`）

#### 架构变更

```
当前: Built-in Bot → bus → dispatch → internal handler
目标: Third-party Bot → OAuth install → BotRepo.subscribe → bus → dispatch → webhook POST
```

变动范围：
- `aero-storage/bot.rs`：加 `scopes` 字段到 `bot_event_subscriptions`
- `aero-server/bot_dispatch.rs`：加 `RoomEvent::Interaction` 路径回调
- `aero-server/`：加 `POST /api/apps/install` OAuth flow
- `web/`：加 bot 注册页 + manifest 编辑器

#### 对现有系统的影响

**极小**。bot dispatch 是纯附加的 bus consumer（`aero-bot-dispatch`），与 `aero-server` consumer 各自独立的 durable cursor。不影响 WS 扇出路径。

---

### 方向二：前端发现层 + 高价值模块补齐 — P0

#### 为什么是 P0

121 个后端模块 vs ~30 个前端模块的覆盖率差距在持续扩大。新模块（canvas、tasks、approvals、legal_holds、scheduled...）不断加后端，但前端无人接。这个缺口正在产生三类问题：

- **用户无法使用的功能**：canvas、tasks、approvals、schedule 等全无 UI，等于不存在
- **维护负担增加**：每次改合约（API request/response），前端需要至少改一处代码但没有人在意
- **演示/联调成本高**：所有功能只能 curl 验证，对产品经理/QA 不透明

#### 核心挑战

不是逐个补 UI（那个无穷尽），而是先建立 **前端功能发现层和模块注册模式**：

1. **功能导航/目录层**（1-2 周）：当前 UI 入口分散在 tab bar 和 channel header，用户无从发现 polls/calls/live/search。需要类似 Slack "应用"或"更多"菜单的聚合入口
2. **前端模块注册模式**（1 周）：定义每个前端模块应导出的接口（`init(els, state, ws)`、`teardown()`、`routes()`），用扫描而非手动 import 注册
3. **覆盖率监控**（1 周）：扩展 `scripts/web-check.sh` 为 grep `pub fn routes()` → 提取模块名 → 查 `web/` 中同名 `.js` 引用 → 输出缺失率

#### 高价值模块优先级

| 模块 | 服务端复杂度 | 前端 UI 复杂度 | 优先级 |
|---|---|---|---|
| `canvas.rs` | 低（CRUD + 协作锁） | 中（富文本/协同编辑） | P0 |
| `tasks.rs` | 低（CRUD + assign） | 低（checkbox + 列表） | P0 |
| `scheduled.rs` | 低（定时消息） | 低（picker） | P1 |
| `approvals.rs` | 低（单审批流） | 低（approve/deny 按钮） | P1 |
| `keyword_alerts.rs` | 低（用户 CRUD） | 低（设置界面） | P1 |
| `legal_holds.rs` | 低（toggle） | 低（管理面板） | P1 |

#### 对现有系统的影响

**中等**。前端的模块注册模式改变会影响所有现有模块的初始化方式，需要一次迁移。但迁移可以增量（新模块用新模式，旧模块逐步适配）。

---

### 方向三：Web Push （方向一的前置依赖）— P1

#### 为什么需要

当前 `aero-push` 只支持 FCM/APNs。Web Push 是桌面和 PWA 场景的基础能力，且是方向一（Tauri 壳）的前置条件——即使用 Tauri，浏览器内 Web Push 仍是首屏体验的决定性因素。

#### 核心挑战

反馈文档已详细列出：

1. **VAPID key 管理层**：`aero-push/certs/` 或 env 配置，需要新增 `WebPushGateway` impl
2. **Service Worker 注册**：`web/sw.js` 需要处理 `push` + `notificationclick` 事件
3. **浏览器侧权限流**：`Notification.requestPermission()` + `pushManager.subscribe()`
4. **Token 模型适配**：当前 `push_tokens` 表可能没有区分 Web Push endpoint 格式的字段（验证：检查迁移 0132）

#### 架构变更

```
aero-push:
  trait PushGateway
    ├── FcmGateway
    ├── ApnsGateway
    └── WebPushGateway (new) — web_push crate

web/:
  sw.js — push event + notificationclick
  app.js — permission request + subscription registration
```

**建议分两阶段**：
- **Phase 1（2 周）**：SW 注册 + 权限流 + `POST /api/me/push-token` 回调
- **Phase 2（2 周）**：VAPID 签发 + `WebPushGateway` + 服务端投递

#### 对现有系统的影响

**低**。`PushGateway` trait 已定义，新增 impl 不影响现有 FCM/APNs 路径。`push_bot.rs` 的 dispatch 逻辑不需要改。

---

### 方向四：多区域/边缘部署 — P2（反馈调整为 10-16 周）

#### 为什么推迟

不推荐在 DAU 跨区域前投入。原因是技术深度被低估——不是简单的 NATS 拓扑调整：

| 障碍 | 工作量 | 原因 |
|---|---|---|
| EventBus subject region 化 | 1-2 周 | `im.room.{id}` → `{region}.im.room.{id}`，需改 ImService 所有 publish 调用 |
| durable consumer 在 leafnode 的行为差异 | 2-4 周设计 + 4-6 周实现 | 每个 leaf 有自己的 cursor，需要跨区域 seq 协调 |
| Postgres 跨区域复制 | 2-4 周 | PgBouncer + logical replication 配置，服务端需要连接池路由层 |
| Redis geo-shard | 2-3 周 | Presence key `presence:workspace:{ws_id}` 需要 region 前缀 + 跨区域合并 |
| 跨区域 call-bridge | 4-6 周 | 当前 `call_bridge_supervisor` 只假设单节点 UDP relay，跨区域需要 TURN/relay 节点 |

**总计约 10-16 周**，且大部分是设计/验证工作而非编码。

#### 现有架构中的正确设计

虽然不支持多区域，但已有的设计决策为未来做了准备：
- `EventBus` trait — region-aware impl 可切换
- `Hub` 进程内扇出 — 每区域独立 Hub 实例，不竞争
- `call_bridge_supervisor` — `(call, peer_url)` keyed，天然支持跨节点

---

### 方向五：虚拟活动/网络研讨会 — P2（分阶段）

#### 为什么是 P2 而非 P5

反馈文档的调整正确——现有的直播/通话/协作/日程/投票/字幕拼接确实覆盖了虚拟活动的大部分基座。但缺**多流合一的 HLS 管线**，这个不能绕过。

#### 分阶段方案

| 阶段 | 能力 | 工作量 | 前置依赖 |
|---|---|---|---|
| Phase 1 | 单主播 webinar（注册 + Q&A + 录制） | 10-14 周 | 现有直播 + 字幕 + 投票 + 点播 |
| Phase 2 | 多嘉宾合流（画中画 + 屏幕共享切到 HLS） | 8-12 周 | 新 `CompositeHlsWriter` + `MultistreamIngest` |

**Phase 1 的 key gap**不是技术难点而是组合工作：
- 活动注册流程（`scheduled_streams.rs` + 新前端的注册表单）
- Q&A 队列（复用现有的消息 threading，加一个"问问题"按钮）
- 录制自动发布（现有 `vod.rs` + `clips.rs`，加钩子）

**Phase 2 才是真正的架构工作**：当前 `HlsWriter` 读单路 TS 片段。多路合流需要：
```
SfuForwarder.on_rtp → 多个 RTP 流 → 合流 Mixer → CompositeHlsWriter
```
这与 `SfuMediaSession` 的 Simulcast 支持可以复用，但 `HlsWriter` 需要重构。

---

## 3. 接口设计建议

### 3.1 现有接口评估

**`EventBus` trait**（`aero-bus/src/traits.rs`）— **设计优秀**：

```rust
#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish(&self, subject: &str, payload: Bytes) -> BusResult<()>;
    async fn subscribe(&self, subject: &str, durable: Option<&str>)
        -> BusResult<BoxStream<'static, Box<dyn Subscription + Send>>>;
}
```

- `dyn`-compatible ✅ — 全系统传递 `Arc<dyn EventBus>`
- 测试友好 ✅ — `MockBus` 可在单元测试中注入
- 但缺 `region` 参数 ❌ — 可能需要在 `publish` 加 `region: Option<&str>`

**`Hub` 接口**（`aero-server/src/hub.rs`）— **设计合理**：

```rust
pub fn fan_out_raw(&self, recipients: &[ParticipantId], frame: &str);
pub fn join_room(&self, room_id: RoomId, pid: ParticipantId);
pub fn rooms_of(&self, pid: ParticipantId) -> Vec<RoomId>;
```

- `fan_out_raw` 接受 JSON 字符串而非 typed frame——这是正确的设计决策，因为 caller（`bus.rs`）已经完成了 typed → JSON 的序列化，避免重复 serde
- 反向索引（`participant → {rooms, streams, calls}`）——断线清理 O(1) ✅

**WS 帧协议**（`ClientFrame`/`ServerFrame`）— **扩展性良好**：

- `#[serde(tag = "type")]` 标记的 tagged-enum——新 frame 类型只需加 variant，无需改 match exhaustive 检查（serde 会根据 `type` 字段 dispatch）
- `kind` 字段撞名已用 `#[serde(rename = "...")]` 规避 ✅

### 3.2 需要新增的抽象层

#### 前端模块注册层（建议新增）

当前 `app.js` 的模块初始化是线性的：
```javascript
import { initSearchAi } from './search.js';
import { initNotifications } from './notifications.js';
import { initLive } from './live.js';
```

建议演进为一个注册表模式：
```javascript
// registry.js — 模块自注册
const modules = new Map();

export function registerModule(name, { init, routes, onWsMessage, teardown }) {
  modules.set(name, { init, routes, onWsMessage, teardown });
}

// 在 app.js 中统一初始化
for (const [name, mod] of modules) {
  try { mod.init(els, state, ws); }
  catch (e) { console.error(`[${name}] init failed`, e); /* 不阻塞其他模块 */ }
}
```

收益：
- 新模块只需在自己的 `.js` 中调用 `registerModule`，不触及 `app.js`
- 初始化错误不级联
- 可枚举已注册模块用于覆盖率监控

#### EventBus region 适配层（未来所需）

当前 `EventBus` trait 需要加 region 感知但不破坏向后兼容：
```rust
#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish(&self, subject: &str, payload: Bytes) -> BusResult<()>;
    async fn publish_with_region(
        &self, subject: &str, region: &str, payload: Bytes
    ) -> BusResult<()> {
        // 默认实现退化为无 region——兼容
        self.publish(subject, payload).await
    }
    // ...
}
```

### 3.3 向后兼容策略

| 变更类型 | 策略 |
|---|---|
| WS 帧加新字段 | `#[serde(default)]` 或 `#[serde(skip_serializing_if = "Option::is_none")]` |
| WS 帧加新 variant | serde tagged-enum 天然兼容（不认识就跳） |
| REST API 加新字段 | JSON 响应加 `#[serde(default, skip_serializing_if = "Option::is_none")]` |
| REST API 加新路由 | 新 `.merge()` 不影响旧路由 |
| 迁移加表/列 | `CREATE TABLE IF NOT EXISTS` + `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` |
| 改 bus subject 模式 | 旧 subject 保留一段时间，双投递 |

**需要特别小心的**：`RoomEvent`/`StreamEvent` 跨节点通过 NATS 传递，新 variant 加入后旧节点收到会 serde 报错。建议：
- 任何时候加新 variant，先确保所有节点升级
- 或者为 `RoomEvent` 加一个 `#[serde(deny_unknown_fields)]` 来明确控制未知 variant 的行为

---

## 4. 技术选型

### 4.1 当前技术栈评估

| 组件 | 选型 | 评估 |
|---|---|---|
| **Postgres 17 + pgvector + pg_trgm** | ✅ 正确 | 向量 + 全文搜索 + 事务，三者合一减少运维复杂度 |
| **Redis 7 (fred)** | ✅ 正确 | sorted-set 用于 presence/roster/leaderboard 天然合适。fred 是 Rust 最成熟的 Redis 驱动 |
| **NATS JetStream (async-nats)** | ✅ 正确 | 轻量级消息队列，比 Kafka 更易运维。JetStream durable consumer 足够稳定 |
| **Axum 0.7** | ✅ 正确 | Tower 生态 + 类型安全的 extractor，Rust Web 框架的事实标准 |
| **str0m** | ✅ 正确 | 纯 Rust WebRTC stack，避免需要系统 libwebrtc 的构建痛苦 |
| **sqlx 0.8** | ✅ 正确 | 编译期 SQL 检查 + migrate 嵌入，比 diesel 更适合此项目（不用 ORM） |
| **ES2020 零依赖前端** | ⚠️ 有代价 | 零构建步骤有利于快速原型，但缺少组件系统对大型 SPA 有长期成本 |

### 4.2 是否引入新技术

| 候选技术 | 推荐 | 原因 |
|---|---|---|
| **Lit / web components** | ⚠️ **观察，不急** | 零依赖 ES2020 在 ~3000 行 JS 时是可维护的，但每新模块都从 DOM 操开始写（`createElement`/`addEventListener`）的样板代码积累会慢下来。建议在模块数 > 40 或 JS > 5000 行时评估 Lit（4.5KB gzip，无构建步骤） |
| **Tauri** | ✅ **推荐（方向一）** | 相比 Electron 的臃肿（~150MB），Tauri 壳 + Rust 后端 + 系统 WebView ≈ 5MB。已有 `aero-push` 的 `PushGateway` trait，桌面通知集成容易 |
| **web-push crate** | ✅ **必须（方向一 Web Push）** | 目前 Cargo.toml 无此依赖。需要加 |
| **Workers（JavaScript runtime for bot sandbox）** | ❌ **不推荐** | Bot dispatch 的设计是 webhook POST（远程调用），不是本地沙箱。引入 JS runtime 带来的安全性/运维复杂度过高 |

### 4.3 自建 vs 采购

| 能力 | 决策 | 原因 |
|---|---|---|
| **通话 SFU** | ✅ **自建（str0m）** | 已实现 `SfuMediaSession` + `SfuForwarder`。纯 Rust 实现，无外部依赖。比采购 LiveKit 等产品更可控且成本更低 |
| **推送网关** | ⚠️ **大部分自建** | FCM/APNs/Web Push 各自需要适配层，但 `PushGateway` trait 是正确抽象。FakeGateway 可用于测试 |
| **AI 模型** | ✅ **自选（Anthropic/Voyage/OpenAI）** | `AiService` + `HashEmbedder`（无 key 退化）的设计正确处理了"有 key 用外部服务，无 key 退化"的场景 |
| **OIDC/SSO** | ✅ **自建** | 已实现 `sso.rs` + `saml.rs`。OIDC 是标准协议，自建比采购 Auth0 等更适合私有化部署 |

### 4.4 第三方依赖的评估标准

如果新增依赖，建议使用这个 checklist：

1. **许可证兼容**（MIT/Apache 2.0/dual——不引入 GPL 传染性许可）
2. **维护活跃度**（最近 6 个月有 commit，非存档/非 solo 项目）
3. **Rust 生态成熟度**（下载量、Stars、是否被大项目依赖）
4. **安全记录**（RustSec 无未修复已知漏洞）
5. **构建成本**（是否引入 C 依赖、编译时间增加是否可接受）
6. **最小引入原则**（`cargo tree` 确认不拉入已存在的 deps 的另一个版本）

---

## 5. 实施路线图

### 5.1 优先级矩阵（修正版）

| 方向 | 优先级 | ROI | 技术风险 | 工作量 | 外部依赖 |
|---|---|---|---|---|---|
| Bot 平台 (方向二) | **P0** | 🔥🔥🔥🔥🔥 | 低 | 4-6 周 | 无 |
| 前端发现层 + Canvas/Tasks (方向四 Phase 0) | **P0** | 🔥🔥🔥🔥 | 中 | 4-6 周 | 无 |
| Web Push (方向一 Phase 1) | **P1** | 🔥🔥🔥 | 低 | 4 周 | web-push crate |
| 虚拟活动 Phase 1 (方向五) | **P1** | 🔥🔥🔥 | 中 | 10-14 周 | 无 |
| Tauri 桌面壳 (方向一 Phase 2) | **P1** | 🔥🔥🔥 | 中 | 6-8 周 | Tauri CLI |
| 前端持续补齐 | **P1** | 🔥🔥🔥 | 低 | 持续 | 无 |
| 移动 Web SPA (连接管理 + tab bar) | **P2** | 🔥🔥 | 中 | 4-6 周 | 无 |
| 虚拟活动 Phase 2 (合流) | **P2** | 🔥🔥 | 高 | 8-12 周 | 无 |
| 多区域部署 (方向三) | **P2** | 🔥 | 高 | 10-16 周 | 多个 |

### 5.2 阶段划分

```
Q3 2026 (7月-9月)
├── Bot Platform MVP ───────────────── 4-6 周
│   ├── Week 1-2: OAuth app install flow + bot manifest UI
│   ├── Week 3: Interactive callback URL (block_interactions → bot_dispatch)
│   └── Week 4-6: Bot token scope model + documentation
│
├── Frontend Phase 0 ───────────────── 4-6 周 (可并行)
│   ├── Week 1-2: Module registry (registerModule pattern) + navigation menu
│   ├── Week 3-4: Canvas UI + Tasks UI (P0 modules)
│   └── Week 5-6: Coverage monitor + remaining P1 modules
│
└── Web Push Phase 1 ───────────────── 2 周
    └── SW registration + permission flow + push-token callback

Q4 2026 (10月-12月)
├── Tauri Desktop Shell ────────────── 6-8 周
│   ├── Week 1-2: Tauri project scaffold + system tray
│   ├── Week 3-4: Native notification integration (PushGateway)
│   └── Week 5-6: Window management + deep-link protocol
│
├── Virtual Events Phase 1 ─────────── 10-14 周 (Q4 跨年)
│   ├── Week 1-2: Event registration flow (scheduled_streams frontend)
│   ├── Week 3-4: Q&A queue (threaded messages + UI)
│   ├── Week 5-6: Recording auto-publish
│   ├── Week 7-8: Ticketing / access control
│   └── Week 9-10: Integration testing + docs
│
└── Frontend Phase 1 (scheduled/approvals/keyword_alerts) ─ 4-6 周

2027 H1
├── Virtual Events Phase 2 (Multi-stream compositing) ─ 8-12 周
│   ├── CompositeHlsWriter + RTP mixer
│   └── Guest-invite flow + screen-share→HLS
│
├── Multi-region ───────────────────── 10-16 周
│   ├── EventBus region parameter
│   ├── Postgres logical replication
│   ├── Redis geo-shard
│   └── Cross-region test infrastructure
│
└── Mobile SPA / Native client ─────── TBD based on Q4 data
```

### 5.3 关键里程碑

| 里程碑 | 交付物 | 时间 |
|---|---|---|
| **M1 Bot MVP** | 第三方 bot 可用 OAuth 安装 + 接收事件回调 | Q3 Week 6 |
| **M2 前端基础** | 模块发现导航 + Canvas/Tasks UI + 覆盖率 ≥ 50% | Q3 Week 8 |
| **M3 Web Push 可用** | 桌面浏览器可接收推送通知 | Q3 Week 10 |
| **M4 Tauri 桌面版** | 可安装的桌面应用，native 通知 + 系统托盘 | Q4 Week 8 |
| **M5 虚拟活动可用** | 单主播 webinar：注册 → 直播 → 录制 → 回放 | Q4 Week 14 |
| **M6 多区域验证** | 双区域可部署，消息/call 跨区可用 | 2027 H1 |

### 5.4 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| Bot 平台安全问题（bot token 泄露/滥用） | 中 | 高 | Bot token scope 模型 + 速率限制 + 审核日志。P0 要求 |
| 前端模块注册模式改动破坏现有 UI | 中 | 中 | 增量迁移：新模式和新模块一起上的同时旧模块保持旧模式。同时并存至少 2 周 |
| Tauri 壳的 WebRTC 兼容性 | 中 | 高 | Tauri 使用系统 WebView（macOS WKWebView/Windows WebView2），其 WebRTC API 与浏览器不完全一样。需要提前验证 `calls.js` 中的 `RTCPeerConnection` |
| 多区域 NATS cursor 不一致 | 高 | 高 | 需要在非 prod 环境搭建双区域测试。不 rush，等 DAU 验证需求 |
| 合流 HLS 性能（多路 RTP → TS → mux） | 中 | 中 | 第 1 版用 GPU 编码（ffmpeg 子进程）作为 fallback，后续再纯 Rust 优化 |
| 团队并行集成冲突 | 中 | 中 | AGENTS.md §4.1 规范 + git worktree 隔离。每个方向一个独立 worktree，合入时协调 `routes.rs` `.merge()` 链 |

### 5.5 不推荐的路径

- **Native 移动客户端（Swift/Kotlin）优先于 Tauri**——在 DAU > 100k 或明确的 iOS/Android 需求出现前，Tauri + mobile SPA 是更好的成本效益比。支持依据：当前 Web SPA 已有 ~80% 的 IM 功能，移动端主要是补齐连接管理 + UX 范式而非重写
- **Bot 后台沙箱（Deno/QuickJS）**——增加安全面和构建复杂度。Webhook POST 是对标 Slack App 的标准模式，足够覆盖用例
- **全量 OpenAPI 生成**——当前手写 `openapi.rs` 是示意性的，全量生成需要 `utoipa` 等 crate 加宏标注，收益（客户端生成）在当前无原生客户端时不足以 justify 成本

---

## 总结

Aero IM 的后端架构在 Rust 生态中是同类项目中最好的一档：干净的 crate 隔离、正确的事件 DAG 设计、有界背压、总线抽象。真正的结构性缺陷在前端（覆盖率、架构模块化、移动端适配）和平台化（Bot OAuth、Web Push）——这些是产品从"功能完整 IM"迈向"平台级协作工具"的必经之路。

最高 ROI 的行动顺序很清楚：**先把 Bot 平台的最后一公里走完（4-6 周），同时建立前端模块基础。然后 Web Push 和 Tauri 壳打开桌面场景。多区域和虚拟活动是需要更重投入的中期方向，在 DAU 和数据支撑前不要 rush。**
