现在我已经对代码库进行了全面审查。以下是架构分析。

---

# 架构分析：Aero IM

## 1. 架构评估

### 1.1 当前架构的优势

**分层正确，解耦性强。** 16 个 crate 的依赖关系严格遵守自下而上的方向——从 `aero-common`（叶子类型/ID/Error/Config）开始，经过 `aero-storage` 和 `aero-auth` 等基础设施层，再到 `aero-server` 中的组合根。没有循环依赖。这一约束（通过 `scripts/crate-dependencies.sh` 强制执行）是大型 Rust 代码库中最有价值的架构决策之一。

**事件驱动架构选型合理。** NATS JetStream 作为跨实例事实来源，Hub 作为进程内扇出，这是正确的双管道模式：

- `im.room.*` durable consumer → `run_bus_listener` → `Hub::fan_out_raw` → WebSocket 帧
- `live.stream.*` 临时 consumer → `run_live_bus_listener` → `hub.stream_watchers`

将跨实例持久化（durable NATS consumer）与进程内扇出（bounded mpsc）分离，使得无需分布式发布/订阅基础设施即可实现水平扩展。每个进程拥有自己的 Hub；NATS 将事件传递给所有进程。

**CQRS 架构——非正式但一致。** REST API 处理查询和命令（历史记录、搜索、权限变更），WebSocket 处理实时扇出。这并非严格的 CQRS（REST 也会写入），但实时路径没有冲突的读取，这是正确的。

**功能演进基于 Waves。** 从 `routes.rs` 中可以明显看出，功能是成组交付的（Wave 10–24、ROADMAP 5–11），每个功能都有一个稳定的模块边界（`crate::foo::routes()`）。这种方法将交付节奏与架构内聚性解耦——新 Wave 可以添加 `.merge(crate::new_thing::routes())`，而无需更改核心循环。

### 1.2 当前架构的局限性

**单点故障：网关层没有冗余。** 每个进程运行一个 Axum 网关，该网关托管 HTTP、WS、RTMP 和所有 bot。网关崩溃意味着所有功能同时丢失。虽然 NATS 连接和 Hub 可以优雅地恢复，但网关恢复时间取决于 tokio 重启和 bus listener 重连——一个故障的网关会完全消失，直到被流程管理器重新启动。

**2854 行的 routes.rs 是一个架构债务里程碑。** 它不仅仅是一个路由注册表——它导入模块、内联中间件、定义上下文提取器和帮助函数，并声明整个 API 表面。虽然 `routes.rs` 的文件大小上限是 3000 行（按 `file-size-check.sh`），但接近这个上限意味着它已接近内爆。建议将每个功能域的 `.merge(crate::*)` 调用提取到按 Wave/域分组的子模块中。

**SPA 是单片调试客户端，不是产品。** `web/` 目录的 README 内容缺失，但在 `docs/specs/2026-05-22-aero-im-design.md` 中，它被定性为「联调用 HTML/JS 客户端」（调试客户端）。它在一个 1009 行的 `app.js` 中处理消息、通话、直播、AI、搜索、通知和表情符号，通过 init-time 导入连接 13 个叶子模块。零组件架构。零路由。零用户治理 UI。这是代码库中最大的缺口——后端提供约 120 个 API 端点，但 SPA 消耗的不到一半。

**157 次迁移全部是单向的。** 没有 `down.sql`。虽然这在活跃开发阶段是合理的（重建开发数据库比重放回滚更快），但这是一个生产部署风险。在 v1 之前，成熟的迁移策略需要回滚路径。

**零备份/DR 基础设施。** `scripts/` 中有 50 多个脚本，但没有 `backup.sh`、没有 `restore.sh`、没有 `failover.sh`。Postgres 17 需要 `pg_dump`/`pg_restore` cron 和 WAL 归档。NATS JetStream 流需要镜像。Redis 需要 RDB 快照。这些缺失并不阻碍开发——但根据 AGENTS.md 的标准（「无备份、无回滚、无 DR——这在生产级系统中是站不住脚的」），它们是 P0。

**暂存 seam 已构建但未接线——这是特性，不是债务。** AGENTS.md 明确将 `sfu_media` 的 `run` 循环、`call_bridge_supervisor` 的 `ensure_egress` 以及各种 `with_*` builder 标记为「已构建+已测试，但生产 socket 循环未接线」。这是有意的架构预留，不是死代码。然而，这种状态对项目的新人来说有信息差——零调用 builder 在编译器死代码检测中被标记为未使用，但它们在设计上是未使用的。

### 1.3 架构债务（非预留项目）

1. **`DeliveryCursorRepo` 是准孤儿。** 它在 `aero-storage` 中完整实现（`delivery_cursor.rs`，包括 `advance`、`get`、`cursors_for`、`clear_room`），并通过 `lib.rs` 重新导出。但评估文档中声称它「完全在服务器中接线」的说法是错误的——我验证了在 `crates/aero-server/src/` 中，没有针对 `DeliveryCursor`、`delivery_cursor`、`DeliveryAck` 或 `cursors_for` 的命中。存储库存在，但没有任何消费者。这是真正的死代码（除非有 WS 帧路径通过我未找到的泛型间接调用它）。

2. **`routes/routes.rs` 结构扁平。** 所有 `.merge(crate::*)` 调用都在一个平面文件中，没有按 Wave/域进行组内分组。虽然文件大小在限制范围内（2854/3000），但模块间结构被掩盖了。像 `build()` 函数这样的提取模式，返回一个将 `.merge()` 调用分组的 `Router` 链，将提高可读性。

3. **SPA 没有测试基础设施。** `eslint.config.js` 存在，但 `scripts/web-check.sh` 只进行静态检查（无未定义变量）。没有 Jest、Playwright 或 Cypress。后端有 50 多个 smoke 脚本，但 SPA 是手工测试的。

---

## 2. 扩展方向

### 方向 1：生产就绪基础设施（P0）

**为什么需要：** 目前，系统无法从数据丢失场景中恢复。157 次迁移没有回滚路径，没有备份计划，没有恢复运行手册。对于一个声称「纯 Rust AI-Native IM + 互动直播平台」的项目来说，数据丢失是不可接受的。

**核心挑战：**
- 迁移回滚需要语义理解——简单的撤销 DDL 是不够的（列删除需要重建数据）。
- 备份需要存储管理——你应该轮换多少备份？WAL 归档需要多少存储空间？
- DR 需要编排——跨区域故障转移意味着 DNS、负载均衡器和数据库复制。

**预期架构变更：**
- `scripts/backup.sh` + `restore.sh`：包装 `pg_dump`/`pg_restore`，可选择 S3 上传
- `scripts/migration-down.sh`：基于每个迁移元数据的 `down.sql` 执行器（`_sqlx_migrations` 按应用顺序记录）
- `config.toml`：备份预配置（S3 端点、GPG 密钥、保留策略）
- 注意：Postgres 和 Redis 的 Docker Compose 辅助配置，支持持久卷和 WAL 归档

**影响：** 低。新脚本，与 Rust 代码零耦合。迁移编排器的最小 `aero-cli` 变更。

### 方向 2：SPA 产品化 + 治理 UI（P0.5）

**为什么需要：** 后端已经路由了 100+ 个端点，其中 30+ 个是治理/管理功能（`ban_appeals`、`user_reports`、`message_reports`、`channel_retention`、`legal_holds`、`info_barriers`、`webhook_admin`、`usage_report`、`ai_dlq` 等）。没有一个有对应的 web UI。与评估文档所称的「孤立模块」相反——**它们在后端都有完整的路由**——但 SPA 从未调用它们。对于一个「生产级」系统来说，没有举报按钮、没有申诉表单、没有保留设置面板是一个严重的可用性缺口。

**核心挑战：**
- 单页应用架构：现有 SPA 是 1009 行的 `app.js` 单片模块，所有功能都通过 init-time `import` 拉入。添加管理 UI 需要路由（设置/治理与管理面板分离）和适当的组件分解。
- 角色门控渲染：治理 UI 应根据角色显示/隐藏（普通用户看到「举报」，管理员看到「审核队列」）。
- 向后兼容性：现有用户不应因 JS 错误而失去功能。新模块应通过备用/懒加载注入。

**预期架构变更：**
- `web/` 目录重组：
  ```
  web/
  ├── index.html          # 入口（单页应用骨架）
  ├── spa.js              # 路由 + 视图切换器（替代 app.js 的顺序启动）
  ├── components/         # 可复用 UI 组件
  │   ├── message.js      # 现有 render.js 的消息渲染
  │   ├── governance/     # 新
  │   │   ├── report-button.js
  │   │   ├── report-form.js
  │   │   ├── moderation-queue.js
  │   │   ├── appeal-form.js
  │   │   └── retention-settings.js
  │   └── admin/          # 新
  │       ├── workspace-settings.js
  │       └── usage-dashboard.js
  ├── api.js              # 现有 API 客户端
  └── ws.js               # 现有 WebSocket 客户端
  ```
- 不要重写——逐步将模块从 `app.js` 提取到 `components/` 中，从治理功能开始，因为它们是新的且独立的。

**影响：** 中等。只有 `web/`。后端零变更。迁移后，`app.js` 应缩小到纯粹的 init + 路由编排。

### 方向 3：可观测性 + 成本透明度（P1）

**为什么需要：** 评估文档在识别缺口方面做得很正确——除了 `ai_usage_ledger` 之外，没有其他使用计量功能。对于多工作区系统，管理员无法看到：
- 每个工作区/频道的存储使用量（`blobs` 有 `byte_size` 但没有聚合路径）
- 每个工作区的 API 调用次数
- 每个工作区的带宽（主要在 WebSocket 上，但部分在 blob 下载上）
- AI 预算消耗细粒度视图（`ai_usage_ledger` 有精细数据，但没有聚合 API）

**核心挑战：**
- 可观测性需要精确聚合——近似值对诊断有用，但对计费可接受性不够。
- 存储计量需要扫描——`SELECT SUM(byte_size) WHERE room_id = ANY(...)` 在一个大表上很昂贵。
- 带宽计量需要中间件——每个响应/帧都有吞吐量数据，但这些数据尚未被采集。

**预期架构变更：**
- 新表或物化视图：`workspace_daily_usage`（由 caddy/tick 或夜间物化视图刷新填充）
- 新路由在 `usage_report.rs` 中展开：
  - `GET /api/workspaces/:id/usage`（现有端点，但当前范围有限）
  - `GET /api/workspaces/:id/usage/messages`、`/storage`、`/bandwidth`、`/ai`
- 在 WS 帧层采集带宽（`Hub` 可以按房间累计出站字节）

**影响：** 低到中。新存储查询，新 API 路由，零架构重构。

### 方向 4：用户激活 + 上手引导（P1，高收入杠杆）

**为什么需要：** 评估文档正确指出——零上手引导，零导入，零预设工作区模板。对于一个类 Slack/Lark 的产品来说，新工作区是一个空的房间列表和登录页面。没有引导式教程，没有来自竞争的导入，没有帮助跳板（「/tutorial」机器人，预设频道）。

**核心挑战：**
- 导入（Slack/Teams/Discord）需要解析器：Slack 导出为 JSON 文件夹，Discord 有 API 获取器。每个都需要一个适配器。
- 预设工作区需要可配置：模板不应硬编码——它们应该是可插入的（默认频道 + 预设机器人 + 欢迎消息）。
- 上手引导机器人重用现有机器人框架（`agent_bot.rs`），但需要持久状态（步骤完成）。

**预期架构变更：**
- 新 crate 或模块：`aero-onboarding`（如果重则用 crate，如果轻则用 `aero-server/src/onboarding.rs`）
- 导入适配器：
  ```
  aero-im-core/src/onboarding/
  ├── mod.rs
  ├── slack_import.rs   # ZIP 解析 + 逐块导入
  ├── discord_import.rs # DiscordChatExporter 兼容
  └── templates.rs      # 预设工作区结构
  ```
- 持久状态：`participant_onboarding` 表（`participant_id`、`workspace_id`、`step`、`completed_at`）
- 欢迎机器人：现有 `golive_bot.rs`/`ooo_bot.rs` 模式的重用——监听 `member_joined` → 发送 DM。

**影响：** 中等。新存储表和轮子，新模块，但后端基础设施（机器人系统、NATS 事件）已经存在。web UI 需要新的上手引导面板和导入向导——这属于方向 2 的重构工作。

### 方向 5：模块间统一（P2）

**为什么需要：** 该系统遵循一致的 CQRS 模式（REST + WS + NATS），但不同功能域之间的内部 API 边界风格不同——有的使用 `ImService` 方法，有的使用直接存储库调用，有的使用 WS 帧处理程序。缺少一个统一的内部 API 契约层，导致模块间耦合：

- `transcribe_bot` 直接调用 `ai.transcribe`
- `moderation_bot` 有单独的预算管理器
- `push_bot` 有自己的通知排序逻辑
- 其他机器人大多独立运行

**核心挑战：**
- 为超过 30 个功能模块定义内部 API 边界是一项元工作，在模块数量较少之前没有回报。
- 统一意味着接口定义——Rust trait 或消息契约——这增加了样板代码。
- 风险：过度工程化。当前的务实解耦（每个模块一个文件 + `routes()` 函数）对 30 个功能单元来说效果很好。

**预期架构变更：**
- 不要立即使用——监控耦合衰减信号：如果 `ImService` 超过 20 个方法或 `routes.rs` 超过 3000 行，则提取内部 API。
- 如果出现，潜在的内部 API 边界：
  - `EventBus` trait（存在但仅供内部使用——NATS 实现是硬编码的）
  - `NotificationDispatch` trait（`push_bot`/`ooo_bot`/`golive_bot` 都消费事件但彼此不了解）
  - `AiGateway` trait（存在，因为 `HashEmbedder` 是回退——但通过 `aero-ai` crate 边界保持模块化）

**影响：** 低（仅供监控）。当前架构没有耦合问题，因此主动统一没有紧急价值。

---

## 3. 接口设计建议

### 3.1 内部模块契约（现有实践）

系统已经有一个被所有功能 CRUD 模块一致遵循的稳定模式：

**Rust 模块模板：**
```rust
// 1. 声明
pub mod foo;                     // lib.rs
pub fn routes() -> Router<AppState>  // foo.rs

// 2. 接线
.merge(crate::foo::routes())     // routes.rs
```

**存储库模板：**
```rust
// 存储 crate 中的独立文件
pub struct FooRepo(PgPool);
impl FooRepo {
    pub fn new(pool: PgPool) -> Self;
    pub async fn get(...) -> Result<Foo>;
    pub async fn insert(...) -> Result<Foo>;
    // ...
}
```

这个模式应该正式记录并强制执行。它有几个优点：

- **零开销抽象**——独立模块，在编译时组合，没有动态调度
- **可测试性**——每个存储库都可以使用 `#[ignore]` + `DATABASE_URL` 门控独立测试
- **可发现性**——`routes.rs` 中的 `.merge()` 列表提供了完整的 API 表面清单

### 3.2 需要引入新的抽象层

**存储库工厂 trait——不是现在。** 新模块当前通过 `let repo = FooRepo::new(s.pg.clone())` 内联构造存储库。这适用于当前规模（约 30 个模块）。我不会引入工厂 trait 或依赖注入容器，除非：
- 模块数量超过 50
- 存储库构造需要每个模块的配置参数（不仅仅是 `PgPool`）
- 我们需要测试替身模拟（`MockFooRepo`）

**WebSocket 帧路由——不是现在。** WS 帧处理程序（在 `ws/ws_impl/frame.rs` 中）当前通过 `match msg.type { ... }` 路由帧。这适用于约 20 种帧类型（`send_message`、`edit_message`、`join_room`、`call_invite` 等）。我不会引入帧路由 trait，除非：
- 帧类型超过 40 种
- 核心 WS 循环需要热重载帧处理程序
- 第三方 bot 需要注册自定义帧类型

**应引入的是什么：一个 Web 组件注册表。** 最大的架构缺口不是 Rust 端，而是 JS 端。当前的 SPA 在 app.js 中通过 init-time import 连接模块。治理 UI 需要一个组件注册表——一个声明式系统，其中模块在 init 时自身向中心路由器注册，而不是手动导入：

```javascript
// 设想的组件注册表 API
// web/spa.js
export class ComponentRegistry {
  register(name, { view, routes, eventHandlers }) { ... }
}

// web/components/governance/report-button.js
registry.register('report-button', {
  routes: [],  // 不需要视图路由——挂接到现有消息渲染中
  eventHandlers: { 'msg:message': injectReportButton },
});
```

这种简单模式避免了：
- 在 `app.js` 导入中忘记新模块（当前每个模块都需要手动添加导入）
- 如果 JS 加载失败则破坏现有渲染（注册表是附加的，不会覆盖）
- 大型单片 `app.js` 文件（模块可以懒加载）

### 3.3 向后兼容性

**REST API 已经是版本化的。** 所有路由都以 `/api/` 为前缀，但没有显式版本号（`/v1/`）。对于当前阶段（预生产），这没问题。在 v1 发布之前，应该建立版本化策略：

- **选项 A：显式版本化（`/api/v1/rooms`）** ——清晰，但路由需要重复/重写。适用于公共 API。
- **选项 B：通过 Accept 头部进行内容协商** ——适用于不兼容的模式变更（V2 返回与 V1 不同的 JSON）。更干净，但中间件更复杂。
- **选项 C：按域版本化（`/api/rooms/v2/...` 同时保留 `/api/rooms/v1/...`）** ——适用于缓慢演变的 API。更多路由。

**建议**：选项 B，用于 v1。实施时影响最小（Accept 头部检查附带在中间件中），如果从未需要，可以移除。

**WS 帧已经是前向兼容的。** `SeqGate` 悄悄丢弃未识别的帧类型。可以添加新帧类型，而不会破坏现有客户端。保持这样。

**Web 客户端应该使用功能检测，而不是版本匹配。** SPA 不应检查 `"feature_X" in ws`。相反，它应该尝试调用 API 并优雅地处理 404/405 错误。这避免了 SPA 与服务器之间的绑定耦合。

---

## 4. 技术选型

### 4.1 当前栈评估

| 层 | 当前 | 评估 |
|---|---|---|
| HTTP/WS | axum 0.7 | ✅ 正确选择。安全、可扩展、tokio 原生 |
| 数据库 | Postgres 17 + pgvector | ✅ 未来可预见的正确选择 |
| 缓存 | Redis 7 | ✅ Presence/roster/速率限制——语义上属于缓存 |
| 事件总线 | NATS JetStream | ✅ 这是 ace。持久队列、扇出、ephemeral consumer |
| WebRTC | str0m 0.19 | ✅ 纯 Rust DTLS-SRTP，降低编译依赖 |
| AI | Anthropic + Voyage（带 HashEmbedder 回退） | ✅ 无 AI 密钥的退化路径是务实的设计 |
| 推送 | FCM + APNs（内置） | ✅ 网关模式正确 |
| SPA | 零依赖 ES2020 | ✅ 低耦合，但缺少框架=缺少组件路由 |

### 4.2 建议引入的新栈

**对于 SPA 重构：lit-html + 最小路由器（在引入框架之前）**。评估文档正确地指出了 SPA 缺口，但一个成熟的框架（React/Vue/Svelte）引入了一个新的构建步骤、节点运行时和版本冲突。lit-html 是：
- 使用 JavaScript 标记模板字面量——零构建步骤
- 组件模型（LitElement），但轻量级（约 5KB gzip）
- 与现有代码库兼容——lit-html 模块可以增量包裹现有的裸 DOM 代码

**对于迁移回滚：sqx CLI 已经支持 `down`**。迁移声明已经包含 `-- reversible` 标志；添加 `down.sql` 脚本是可行的。投入工作量低——为每个迁移编写反向 DDL。

**对于导入：自建，不购买。** Slack 导出是 JSON 文件夹；Discord 导出是 CSV/JSON。没有第三方库可以「导入 Slack 到 Rust IM」——适配器必须针对系统自己的数据模型进行编写。但是，用于解包 Slack ZIP 文件的库（Python 的 `zipfile`，Rust 的 `zip`）是标准库。

**对于 WebRTC/SFU：坚持使用 str0m。** 评估文档没有就此提出质疑，但这是一个风险点：str0m 很小众。替代方案是 `webrtc-rs`（功能更齐全，但编译时间更长）或直接使用 GStreamer（运行时依赖重）。str0m 的纯 Rust 特性对于 CI/CD 和编译时保证来说，是一个足够好的权衡。

### 4.3 自建与采购指南

| 能力 | 自建 | 采购/集成 |
|---|---|---|
| 备份/恢复 | ✅ 需要；简单包装 `pg_dump`/`pg_dumpall` | ❌ 过度设计 |
| SCIM 入站供给 | ✅ 已完成 | N/A |
| SCIM 出站同步 | ❌ 不要构建——直到有客户要求 | ✅ 上游 IdP 处理 |
| 联邦（Matrix/ActivityPub） | ❌ 明确不在范围内 | N/A |
| 视频转码（录制） | ✅ 已经有 HLS writer | ❌ 采购 FFmpeg 集成 |
| 推送通知 | ✅ 已完成 | N/A |
| WebRTC TURN | ❌ 不要自建 | ✅ 运行 coturn 或 cloudflare Calls |
| 客户端 SDK | ❌ 不要构建 | ✅ Web 优先，移动端后续 |

---

## 5. 实施路线图

### 5.1 优先级排序

| 优先级 | 方向 | 为什么 |
|---|---|---|
| **P0** | 生产就绪基础设施 | 数据丢失风险。157 次迁移不可逆。零备份。 |
| **P0.5** | SPA 产品化 + 治理 UI | 后端有 30+ 个治理 API，没有一个有前端。产品缺口。 |
| **P1** | 可观测性 + 成本透明度 | 工作区管理员看不到使用情况。流失风险。 |
| **P1** | 用户激活 + 上手引导 | 高收入杠杆（Slack/Teams 导入直接驱动采用） |
| **P2** | 模块间统一 | 关注——仅在耦合变得棘手时采取行动 |

### 5.2 阶段划分

**阶段 1（当前 → 2 周）：生产基础**
- `scripts/backup.sh` + `restore.sh`：包装 `pg_dump`、`pg_dumpall`、S3 上传
- `scripts/migration-down.sh`：通过 sqlx CLI 执行向下迁移（为每个迁移添加 `down.sql`——从新迁移开始，不能追溯）
- `docker-compose.yml`：添加持久卷 + WAL 归档配置 + Redis RDB 快照
- 文档：`docs/runbooks/disaster-recovery.md`

**阶段 2（2 → 4 周）：治理 UI**
- 组件注册表模式（`web/spa.js`）
- 按优先级排列的治理 UI 组件：
  1. 消息举报按钮（`POST /api/rooms/:id/messages/:mid/report`）
  2. 申诉表单（`POST /api/streams/:id/appeals`）
  3. 审核队列仪表板（`GET /api/workspaces/:id/admin/moderation-queue`）
  4. 频道保留设置（`PATCH /api/rooms/:id/retention`）
  5. 使用仪表板（`GET /api/workspaces/:id/admin/usage`）
- 目标：所有现有治理 API 的 UI 覆盖率，通过 eslint 强制

**阶段 3（4 → 6 周）：成本 + 可观测性**
- 用于使用聚合的物化视图（`workspace_daily_usage`）
- `GET /api/workspaces/:id/usage` 扩展为包括存储/带宽/AI
- 在 `Hub` 中采集 WebSocket 扇出吞吐量

**阶段 4（6 → 8 周）：用户激活**
- 导入适配器：Slack JSON 导出 → Aero 房间/消息/成员
- 预设工作区模板：配置默认频道模型（`#general`，`#random`，`#announcements`）
- 欢迎机器人：在加入时发送 DM，引导 `/help` 命令
- 上手引导教程：逐步任务列表（创建频道、发送消息、邀请成员）

### 5.3 风险与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| 迁移太多，无法追溯添加 `down.sql` | 高 | 中 | 只锁定新迁移；为旧迁移留下工具，但标记为「不保证回滚」 |
| SPA 重构破坏现有 UI | 中 | 高 | 增量替换——lit-html 组件包裹现有裸 DOM 代码；每个组件都进行冒烟测试 |
| 导入适配器需要维护 | 中 | 低 | Slack 导出格式是稳定的（JSON 文件夹，自 2020 年以来没有变化） |
| 可观测性增加运营成本 | 低 | 低 | 聚合可以重用现有查询模式，无需新的中间件管道 |
| 组件注册表模式因规模而失效 | 低 | 低 | 与成熟的路由器一起作为框架迁移的垫脚石——如果增长令人痛苦，则切换到 svelte/lit |

---

## 结论

Aero IM 代码库是一个设计良好的 Rust 系统，具有一致的架构模式（事件驱动、分层 crate、CQRS）。它在 157 次迁移、16 个 crate 和约 120 个 API 端点方面展示了巨大的交付动力。最大的架构缺口不是代码库本身，而是**SPA 与后端的差距**——治理端点已实现并部署，但前端从未消耗它们。生产就绪基础设施（备份、向下迁移）是另一个 P0 项目，不是因为在技术上具有挑战性，而是因为数据丢失是不可逆的。

关键的架构建议是：

1. **在添加业务逻辑之前修复生产基础**——备份和迁移回滚不性感，但它们决定了系统是否能从故障中恢复。
2. **在添加 Rust 代码之前修复 SPA**——后端治理 API 没问题；缺口在于前端消耗。
3. **不要过度抽象 Rust 模块接口**——当前的 flat-file 模式可以扩展到约 50 个模块。让它先破裂，然后再统一。
4. **lit-html 作为桥梁，不是跳板**——用 lit-html 组件包裹现有的裸 DOM 代码，不要重写。如果需要成熟的 SPA 框架，可以稍后切换。
