现在我已充分了解代码库——涵盖 17 个前端的 JS 模块、后端的 2854 行 routes.rs、Hub 扇出引擎、NATS 总线监听器，以及自定义指标注册表。以下是架构分析。

---

# 架构分析：Aero IM — 五个系统性被忽视的高价值扩展方向

## 1. 架构评估

### 优势

| 方面 | 评价 |
|---|---|
| **Crate 架构** | 15 个 crate 对应严格的自下而上依赖顺序（`aero-common` → `aero-server`），环扣最小化。每个 crate 都是一个可独立测试的领域边界。 |
| **事件驱动骨架** | NATS JetStream 用于跨实例持久化，Hub 用于进程内扇出：这种复用模式在整个系统中保持一致——IM 房间事件、直播流事件、通话信令均遵循相同的数据流。 |
| **分布式追踪基础设施** | W3C `traceparent` 已在总线信封中传播。跨度从生产者（`publish_room_event`）→ 消费者（`bus.rs` 中的 `bus_consume_span`）形成链接。这为端到端延迟分析提供了 80% 的基础。 |
| **有状态 Agent/Bot 系统** | 7 个持久的 NATS consumer bot、有预算限制的 AiWorker、5 个定时器——全部拥有明确定义的不变量（幂等守卫、fail-open、死信）。这些都是架构文档化做得好的标志。 |
| **前端动态可恢复性** | `WsClient` 已实现指数退避重连、`since` 游标回溯填充、`SeqGate` 去重以及 `resync` 帧协议。连接丢失 *不会* 导致静默数据丢失。 |
| **内部指标注册表** | 手写 Prometheus 注册表（`aero-common/src/metrics.rs`，~973 行）是完整且可测试的——避免了 opentelemetry crate 的版本兼容性噩梦。 |

### 局限性（按严重程度排序）

| # | 局限性 | 影响 |
|---|---|---|
| **L1** | **前端无构建管道**。17 个独立 ESM 模块 → 17 次 HTTP 往返。无 tree-shaking、无代码分割、无压缩、无 HMR。 | 移动端页面加载时间预计 2-4 秒，即使在 HTTP/2 下（每个模块一个连接流）。性能在 3G 网络上会非常糟糕。 |
| **L2** | **离线能力几乎为零**。`send()` 在未连接时返回 false（静默丢失！）。无 ServiceWorker、无 IndexedDB 消息队列、无 sessionStorage 状态持久化。 | 页面刷新丢失：草稿、已输入的文字、滚动位置、未发送的乐观消息。Web 应用恢复到“刚登录”状态。 |
| **L3** | **零缓存交付**。`blob_download` 无 `Cache-Control`、无 `ETag`、无 `Range`/`206` 支持。每次 blob 请求都将完整字节读入内存（即使是大文件）。 | 图片和附件即使在重复查看时也需要重新下载。没有渐进式加载（JPEG progressive 或 HTTP Range）。移动端带宽浪费。 |
| **L4** | **端到端延迟盲区**。总线监听器（`handle_room_event_sub`）和 `fan_out_raw` 完全没有计时。NATS 积压监控仅覆盖 2/9 个 consumer。存在时间跨度基础设施，但未被指标利用。 | 操作人员无法回答“消息传递延迟是多少？”或“瓶颈在哪里？”（解码？成员展开？扇出？） |
| **L5** | **前端状态架构无持久化策略**。`context.js` 在内存中保存 `state`——在页面会话内遵循单例模式，但忽略了 `beforeunload`、`sessionStorage` 恢复和草稿保存。 | 每次页面刷新都有初始 API 调用风暴（房间列表、每个人的 presence、未读计数）。更大的工作区可能需要 5-10 个串行 REST 调用来重建状态。 |

### 关键架构决策（合理性检查）

| 决策 | 合理吗？ | 注释 |
|---|---|---|
| 无前端构建工具 | ❌ 不再合理 | 对于具有 17 个模块的 P0 SPA，零构建方法是一种资产。对于现在拥有 87 个功能点的平台来说，这是一个负债。`esbuild` 是一个低风险、最小侵入性的添加。 |
| 手写 Prometheus 指标而非 OpenTelemetry SDK | ✅ 合理 | 给定的 crate 版本兼容性问题（AGENTS.md 明确提到了这一点），手写注册表是一种务实的权衡。代价：无原生 OTLP 导出，无自动 HTTP 跨度创建。 |
| NATS 用于事件总线 | ✅ 合理 | JetStream 耐久性 + at-least-once 语义对于 IM 来说是正确的选择。用于直播流的临时 consumer 也是正确的（丢失弹幕是可以接受的）。 |
| Hub 作为进程内扇出（而非每个实例一个 NATS consumer） | ✅ 合理 | NATS 投递 *到* 每个实例一次，然后 Hub 在进程内扇出。这是标准竞争消费者模式。 |
| 无全量 OpenAPI 规范 | ⚠️ 可接受 | 对于 100+ 路由的面向服务的 API，无单源事实会增加前端-后端摩擦。当前“手写文档”方法无法扩展。 |

---

## 2. 扩展方向

### 方向 A：前端交付管道 + PWA 基线（高业务价值，低成本）

**为什么需要：** 当前的 17 次 HTTP 往返架构带来明显的加载延迟。没有 ServiceWorker，该应用在移动网络上无法使用，且无法被添加到主屏幕（无离线支持）。所有竞争对手（Slack、Teams、Discord）都有成熟的 PWA 或原生客户端。

**核心挑战：**
- 在零外部运行时依赖（无 React、无 Vue）的同时，保持无构建的调试体验有效
- ServiceWorker 生命周期极其复杂——更新、作用域、缓存失效
- 在操作 API 调用层的 IndexedDB 消息队列时，避免竞争条件

**预期的架构变更：**

```
当前状态：
  17 × <script type="module"> → 17 次 HTTP GET 请求 → ESM 模块图解析

目标状态：
  esbuild bundle → 1 个 app.bundle.js + 1 个 style.css (+ 代码分割的 *.chunk.js)
  ServiceWorker (sw.js) → 预缓存 shell + 运行时缓存 blob + 离线消息队列
  manifest.json (现有骨架) → 填充 + 图标
```

**对现有系统的影响：**
- *极低。*不更改后端代码。不更改模块内部逻辑。
- `web/` 需要：
  - `esbuild`（一个新的 devDependency）
  - `build.js` 脚本（在 `package.json` 中约 20 行）
  - `sw.js` 文件（每个事件处理程序约 10 行）
  - `manifest.json` 校正（填充 `icons`、`start_url`、`display`）
- 将 `web/README.md` 中的 `python3 -m http.server` 指令替换为 `npm run build && npx serve dist/`

**风险：** 无构建工作流失去了*零配置*方面的优势。通过使 `esbuild` 成为可选并在 `dev` 模式下保留原始的 `<script type="module">` 路径来缓解（前端按 env 切换加载方式）。

---

### 方向 B：端到端延迟可观测性（高运维价值，极低成本）

**为什么需要：** 每个实时消息系统都需要 p50/p95/p99 延迟仪表板。目前，Ops 无法回答“消息现在是否滞后？”。现有追踪基础设施（W3C traceparent + 跨度传播）已就位，但未被利用。

**核心挑战：**
- 在热点路径上使用 `Instant::now()` 没有显著的性能开销，但测量 *每个* 消息会增加计数器更新的原子操作
- Histogram 分桶需要仔细校准（<10ms 用于 LAN，<100ms 用于 WAN，<500ms 用于慢客户端）

**预期的架构变更：**

将 5 个 `Instant::now()` 测量点注入一次：
```
bus.rs: handle_room_event_sub:
  t0 = Instant::now()               // 进入 handler
  t1 = Instant::now()               // 解码后
  t2 = Instant::now()               // 成员展开后
  t3 = Instant::now()               // fan_out_raw 后（所有 try_send）
  t4 = Instant::now()               // sub.ack() 后
```
将 4 个持续时间指标导出为直方图：
- `aero_bus_decode_duration_seconds`（t1-t0）
- `aero_bus_member_expand_duration_seconds`（t2-t1）
- `aero_bus_fanout_duration_seconds`（t3-t2）
- `aero_bus_ack_duration_seconds`（t4-t3）

**NATS consumer 积压：** 在 `CONSUMERS` 数组中添加 7 个缺失的 consumer（`aero-bot`、`aero-ooo`、`aero-unfurl`、`aero-transcribe`、`aero-push`、`aero-moderation`、`aero-golive`）。

**对现有系统的影响：** 无。仅添加指标。无依赖。无架构变更。

**风险：** 直方图分桶选择错误导致语义缺失信号。缓解措施：在 Prometheus 侧使用 `le` 分桶（从 1ms 到 5s），允许事后重新分桶。

---

### 方向 C：Blob 交付优化——缓存 + 范围请求 + 缩略图（高用户体验价值，中等成本）

**为什么需要：** `blob_download` 是未优化的本地读取路径中的热路径。每次附件查看都涉及：速率限制检查 → 元数据查询 → 访问控制查询 → 存储获取（完整文件） → 内存响应。对于图片，这意味着每次重新加载页面时都会重新下载。

**核心挑战：**
- 缓存设置需要知道哪些 blob 是可变的（头像 vs 消息附件 vs 自定义表情）
- 缩略图生成需要 `image` crate 或外部库——增加了 crate 依赖
- `Range`/`206` 支持需要修改 `BlobStore` trait（当前只有 `get(id) → Bytes`）
- 条件请求（`If-None-Match`、`If-Modified-Since`）在 blob 生命周期内是安全的（附着后不可变），但头像会变

**预期的架构变更：**

```
当前:
  blob_download(id) → is_accessible_by() → blob_store.get(id) → Bytes → Response

目标:
  blob_download(id) → is_accessible_by() → ETag lookup (sha256) → 
    if-Match → 304/206 或 blob_store.get_range(offset, length) → 
    Cache-Control: public, max-age=31536000 (不可变 blob)
                    private, max-age=300      (头像)
```

**后端合约变更：**
- `BlobStore` trait 需要新的 `get_range(id, offset, length) -> Result<Bytes>` 方法
- `Blob` 行需要 `etag` 字段（已存在 `sha256` 作为隐含 ETag）
- 用于 HEAD 请求的 `blob_meta` 路由（无条件下载前检查）

**对现有系统的影响：**
- *中等。*BlobStore trait 变化影响本地 fs 路径和 S3 后端
- S3 后端可以委托给 `GetObjectRanges`（免费）；本地 fs 需要 `tokio::fs::File::read_at`
- 缩略图生成是一个增量步骤。`image` crate（~350 个传递依赖）是沉重的——考虑推迟到 P2

**风险：** ETag 与 `sha256` 耦合意味着 ETag 在 blob 创建之前是未知的（上传 → 哈希 → 持久化）。对于流式上传来说没问题，但意味着 HEAD 请求必须读取元数据（而非计算值）。可接受。

---

### 方向 D：前端状态持久化——会话恢复 + 草稿保存（中等用户体验价值，中等成本）

**为什么需要：** 页面刷新是无状态的。`state` 对象（`me`、`rooms`、`unreadByRoom`、`pendingByTempId`、`typing`、`receiptsByRoom`、`call` 状态）全部消失。这导致：
1. 刷新后的 API 调用风暴（5-10 次初始 REST 查询）
2. 丢失的消息草稿（Shift+Enter 多行消息——沮丧）
3. 丢失的滚动位置（需要滚动回复、搜遍历史记录）

**核心挑战：**
- sessionStorage 有同步 API，但 5-10MB 的限制意味着缓存必须是有选择性的
- IndexedDB 有异步 API（在上下文初始化的热路径上增加了复杂性）
- 草稿必须按房间+线程范围存储，并在重新连接到 WS 时恢复
- 定时消息需要本地 IndexedDB 队列，配合 WS 连接的守护

**预期的架构变更：**

```
context.js:
  + import { hydrateState, persistState, saveDraft, loadDraft } from './persist.js';
  
  + window.addEventListener('beforeunload', () => {
  +   persistState(state);  // 将关键状态写入 sessionStorage
  +   saveDraft(state.currentRoomId, composeEl.value);  // 草稿到 IndexedDB
  + });

  // 创建时恢复：
  + hydrateState(state);  // 从 sessionStorage 读取

ws.js:
  + 添加 offline_queue：当 send() 由于 ws===null 而返回 false 时，推送到 IndexedDB
  + 重新连接时：从 IndexedDB 清空，添加服务器分配的 ID 映射到乐观 ID
```

**对现有系统的影响：**
- *低到中等。*不更改后端。前端新增一个 `persist.js` 文件（~150 行）。
- 乐观消息的离线队列需要 `ws.js` 中的一个状态字段更改：`send()` 返回一个承诺或可取消对象，而不仅仅是 false。
- 现有的乐观消息替换协议（按 `sender + text + 15s 窗口` 匹配）仍然可以工作。

**风险：** sessionStorage 在隐身/私有浏览模式下不可访问。缓解措施：捕获 `QuotaExceededError` 和 `SecurityError`，静默降级。

---

### 方向 E：API 契约层——为前端提供类型化 OpenAPI 客户端（中等运维价值，中等成本）

**为什么需要：** 前端调用 50+ 个 REST 端点，全部使用原始 fetch 调用和手写 JSON 处理。没有请求/响应类型检查。没有模式验证。没有端点目录。这导致：
- 重构后出现生产 500 错误（字段被重命名，前端仍在发送旧名称）
- 后端开发人员不知道哪些字段被前端使用
- 没有可以共享给第三方 API 消费者的自动生成 API 文档

**核心挑战：**
- Rust + Axum 目前没有 OpenAPI 运行时生成器（如 `utoipa`）的惯用支持
- `utoipa` 需要宏标注每个 handler、每个请求类型、每个响应——对现有 2854 行的 routes.rs 增加大量噪音
- 保持 OAS 规范与实现同步是一个需要纪律的持续过程

**预期的架构变更：**

**选项 A（推荐）：** 输出优先——手写 OpenAPI 3.1 `openapi.yaml` 作为事实来源。使用 `oazapfts` 或 `openapi-typescript` 从规范生成 TypeScript 类型定义。在 CI 中使用 `redocly lint` 验证。

```
openapi/
  openapi.yaml          # 事实来源（400 行顶层 + 每个端点路径项）
  scripts/generate.sh   # npm run generate → web/api-types.d.ts
  redocly.yaml          # CI linting 配置
```

**选项 B（激进）：** 运行时生成——添加 `utoipa` 并标注每条路由。将生成的 OpenAPI 提供给 `/api/openapi.json`。

| | 选项 A（手写 + CI） | 选项 B（utoipa 运行时） |
|---|---|---|
| 准确性 | 手动保持，可能过时 | 始终为最新 |
| 前端 DX | 良好——类型生成有效 | 最佳——始终精确 |
| 维护成本 | 低——规范就是 API 文档 | 中等——大量的宏标注 |
| CI 集成 | 需要 linting 检查 | 内置于测试中 |

**对现有系统的影响：**
- *中等。* 新文件 + CI 步骤，无需运行时更改。
- 目前，`api.js` 中的 `request()` 只关心 HTTP 状态和 JSON 主体。添加模式化响应意味着引入 `api-types.d.ts`，但 JS 不会原生使用它（ESLint + JSDoc 可以，但 `// @ts-check` 可以获得部分效果）。

**风险：** 如果无人值守，手写 YAML 几乎会过时。缓解措施：添加 CI 步骤，当对 routes.rs 的更改与 OpenAPI 规范不匹配时失败（`oasdiff`），或者转向完全生成的选项 B。

---

## 3. 接口设计建议

### 3.1 Hub 扇出合约（当前已足够——不要抽象化）

`Hub::fan_out_raw(&self, recipients: &[ParticipantId], text: &str)` 是一个干净的接口。它有 1 个同步非阻塞语义（try_send，从不 `.await`），这是正确的。**不要**向它添加计时。而是由 *caller*（`handle_room_event_sub`）打点，使用 `Instant::now()` 和直方图。这种关注点分离保持了 Hub 的简洁。

### 3.2 BlobStore 接口演进

```rust
// 当前：
trait BlobStore {
    async fn get(&self, id: BlobId) -> Result<Bytes>;
    async fn put(&self, id: BlobId, data: Bytes) -> Result<()>;
    async fn delete(&self, id: BlobId) -> Result<()>;
}

// 提议——添加范围支持：
trait BlobStore {
    async fn get(&self, id: BlobId) -> Result<Bytes>;   // 仍然有效，完整文件
    async fn get_range(&self, id: BlobId, offset: u64, length: u64) -> Result<Bytes>;
    async fn head(&self, id: BlobId) -> Result<BlobMeta>; // 存储为独立调用
    async fn put(&self, id: BlobId, data: Bytes) -> Result<()>;
    async fn delete(&self, id: BlobId) -> Result<()>;
}

struct BlobMeta {
    size: u64,
    sha256: [u8; 32],
    // 未来：content_type，etag
}
```

**向后兼容性：** `get()` 保留其签名和语义。默认实现 `get_range()` → `get()` 的完整读取 + 切片，因此 S3 后端可以在基类默认值之上乐观地覆盖。

### 3.3 持久化层（前端）

```typescript
// persist.ts 接口
interface StateSnapshot {
  version: 1;
  me: Participant | null;
  currentRoomId: string | null;
  unreadByRoom: Record<string, number>;
  lastEditAt: Record<string, number>;   // message_id -> epoch ms
  scrollPositions: Record<string, number>; // room_id -> scrollTop
}

interface DraftStore {
  save(roomId: string, threadId: string | null, text: string): Promise<void>;
  load(roomId: string, threadId: string | null): Promise<string | null>;
  delete(roomId: string, threadId: string | null): Promise<void>;
}

interface OfflineQueue {
  enqueue(message: OutgoingMessage): Promise<string>; // 返回乐观 ID
  dequeue(connection: WsClient): Promise<void>;       // ws 连接时的清空循环
  getPending(): Promise<OutgoingMessage[]>;
}
```

**设计原则：** 存储是一个叶模块。它从 `context.js` 导入 `state`，但 `context.js` 绝不导入存储。这保持了初始化的线性性：`context.js` ⊥ `persist.js` → `hydration` 是显式的（`hydrateState(state)`）。

### 3.4 从不向 `context.js` 添加业务逻辑

`context.js` 目前干净地作为“无业务逻辑、无前向依赖”的叶子模块。这是正确的，必须保持。新的持久化层是一个单独的模块，它*读取*状态但*不改变*它。业务逻辑属于 `app.js` 和领域模块（`calls.js`、`search.js`、`livecards.js`）。

---

## 4. 技术选型

### 4.1 需要引入的新技术

| 技术 | 用途 | 理由 | 替代方案 |
|---|---|---|---|
| **esbuild** | 前端打包 | 最快的 JS 打包器（<50ms 冷启动）。零配置用于简单 bundle。用 Go 编写，一个二进制文件。 | Vite（需要 Node 18+，更重的配置）；Rollup（较慢的构建，更好的插件生态）；Webpack（过于复杂） |
| **ServiceWorker API** | 离线 + 预缓存 | 浏览器原生 API。无第三方依赖。PWA 需求。 | Workbox（Google 的 SW 库——增加 50KB 到 bundle，但对于更简单的 SW 来说是大材小用） |
| **IndexedDB API** | 离线持久化 | 浏览器原生。无依赖。结构化数据 >5MB。 | localStorage（5MB 限制，同步，无索引——不适合消息队列） |

### 4.2 不需要引入的技术

| 技术 | 曾经被考虑 | 为什么拒绝 |
|---|---|---|
| React / Vue / Svelte | 部分前端框架 | 零成本维护评估：当前的 ESM 原生方法对于 17 个模块来说已经足够。添加一个框架会增加 ~50-200KB、运行时开销，以及框架升级的维护成本。 |
| Tailwind CSS | 样式框架 | `style.css` 是 1236 行，很简洁，并且遵循设计系统（CSS 自定义属性）。除非正在进行完整的 UI 重写，否则不需要。 |
| OpenTelemetry SDK | 指标/追踪 | AGENTS.md 明确避免使用 opentelemetry crate 版本兼容性噩梦。当前的手写 Prometheus 注册表用于计数器/仪表/直方图已经足够。如果需要，OTLP 可以稍后通过 `opentelemetry_otlp` 功能标志添加。 |
| image crate | 缩略图 | ~350 个传递依赖。可以推迟到 P2，届时缩略图的需要得到以下信息的支持：可以测量移动带宽节省。在此期间，客户端可以缩放全尺寸图像（已经有效）。 |

### 4.3 自建 vs 采购

| 能力 | 建议 | 理由 |
|---|---|---|
| 前端打包器 | **自建**（esbuild + 10 行脚本） | esbuild 是免费的、快速的，并且只需要一个配置对象。一个只有 CLI 标志的 `package.json` 脚本可以处理 bundle。 |
| ServiceWorker 缓存策略 | **自建** | 缓存逻辑足够简单（预缓存 shell + 运行时缓存 blob + 离线队列），不值得使用 Workbox（缓存优先 + 网络优先路由约 30 行）。 |
| 缩略图 | **采购**（`image` crate 或外部服务） | 在 Rust 中实现正确的图像缩放（考虑 EXIF 方向、色度子采样、格式支持）是复杂的。`image` crate 是 de facto 标准。 |
| API 文档 | **采购**（手写 OpenAPI + CI lint） | OpenAPI 是 de facto 标准。无数工具可以消费 OAS 3.1 规范。生成 TypeScript 类型是 100 美元/月工具链中 5 行的 shell 脚本。 |

---

## 5. 实施路线图

### 优先级矩阵

| 方向 | 用户影响 | 运营影响 | 实施成本 | 风险 | 优先级 |
|---|---|---|---|---|---|
| **B：E2E 延迟可观测性** | 低 | 高 | ~50 行 | 极低 | **P0** |
| **A：前端 bundle + PWA** | 高 | 中 | 低（~2 天） | 低 | **P0** |
| **D：状态持久化** | 高 | 低 | 中（~3 天） | 中 | **P1** |
| **C：Blob 缓存 + 范围** | 高 | 中 | 中（~1 周） | 中 | **P1** |
| **E：API 类型契约** | 中 | 中 | 中（~1 周） | 低 | **P2** |

### 阶段划分

```
Sprint N（P0 — 为期 4 天）
├── A: esbuild bundle
│   ├── npm install --save-dev esbuild
│   ├── web/build.js (20 行)：入口点、outfile、banner、minify
│   ├── package.json 脚本："build": "node build.js"
│   ├── index.html：<script> 替换为单个 bundle.js
│   └── 更新 web/README.md
├── A: PWA 脚手架
│   ├── 填充 manifest.json（icons、display、start_url）
│   ├── sw.js："install" 预缓存 shell + "fetch" 网络优先用于 /api/
│   │   + 缓存后备用于 blob / 头像
│   └── index.html：<link rel="manifest"> + <script> 注册 SW
├── B: E2E 延迟直方图
│   ├── bus.rs：5 个 Instant::now() 测量点，4 个直方图记录
│   ├── metrics.rs：4 个新的直方图名称常量
│   ├── metrics_tasks.rs：在 CONSUMERS 中添加 7 个 consumer
│   └── 可选：fan_out_raw 中每个 try_send 内的时间跨度（但这不是必需的）
└── 测试
    ├── cargo test && cargo clippy
    ├── scripts/web-check.sh
    └── 在 localhost 手动冒烟测试 WS 延迟

Sprint N+1（P1 — 为期 5 天）
├── D: 前端持久化
│   ├── web/persist.js：sessionStorage 快照（state→可 JSON 序列化→sessionStorage）
│   ├── web/persist.js：IndexedDB 草稿（DraftStore 接口）
│   ├── ws.js：OfflineQueue 类（IndexedDB 后备 + 重连清空）
│   ├── context.js：beforeunload 上的 hydrateState()/persistState()
│   └── app.js：在房间切换/导航时恢复草稿
├── C: Blob 交付优化（第一阶段）
│   ├── BlobStore trait：get_range(id, offset, length) 带默认 fallback
│   ├── LocalFsBlobStore：实现 get_range（tokio::fs::File::read_at）
│   ├── S3BlobStore：委托给 GetObjectRanges
│   ├── blob_download：ETag（=SHA256）+ Cache-Control 标头
│   ├── blob_download：范围请求检测（Range 标头 → 206）
│   └── 测试：字节范围正确性、ETag 匹配、304 短路
└── 测试 + 文档

Sprint N+2（P1 剩余 + P2 — 为期 5 天）
├── C: Blob 缩略图（如果带宽数据支持）
│   ├── Cargo.toml：添加 image crate（可选功能）
│   ├── blob_download：?thumbnail=200x200 查询参数支持
│   ├── thumbnail_cache：磁盘或 Redis（小图像为 Redis，大图像为磁盘）
│   └── 在消息渲染期间向 WS 帧添加缩略图 URL
├── E: API 类型（第一阶段——轻量级）
│   ├── docs/openapi/openapi.yaml（顶层 + 最常见的 20 个端点）
│   ├── npm install --save-dev @redocly/cli
│   ├── scripts/generate-types.sh（oazapfts → web/api-types.d.ts）
│   └── CI 步骤：oasdiff 路由签名或 redocly lint
└── 完整集成测试 + 文档
```

### 关键风险 & 缓解策略

| 风险 | 可能性 | 影响 | 缓解策略 |
|---|---|---|---|
| esbuild bundle 破坏 ESM 模块图 | 低 | 高 | 在 bundle 之前通过 eslint.config.js 运行。保留 `index.dev.html` 以进行回退（原始的 <script type="module"> 路径）。 |
| SW 缓存使陈旧资产永久化 | 中 | 中 | 使用基于哈希的缓存键（`bundle-xxxxx.js`）进行预缓存，并在 `activate` 上清理旧缓存。对于指纹生成，esbuild 的 `--asset-names` 不够——切换到 `entryNames` + contenthash。 |
| IndexedDB 配额在移动端被超限 | 低 | 中 | 在 `put` 上存储 `navigator.storage.estimate()` 并触发 LRU 驱逐。将每条消息的大小限制为 100KB。 |
| 范围请求暴露访问控制绕过 | 低 | 高 | ETag 和范围检查在 `is_accessible_by()` 守卫*之后*进行。范围请求从不绕过身份验证——它们只是改变响应的形状。 |
| OpenAPI 规范随时间漂移 | 中 | 低 | `oasdiff` CI 步骤（`oasdiff -breaking-only`）捕获向后不兼容的更改。未能通过会增加反射成本，但不会离线破坏测试。 |

### 里程碑

| 里程碑 | 时间表 | 可交付成果 |
|---|---|---|
| **M1：可测量的延迟** | Sprint N，第 2 天 | 在 prometheus.yml 的 Grafana 仪表板上，有 p50/p95/p99 延迟直方图用于消息总线路径 |
| **M2：Bundle 交付** | Sprint N，第 3 天 | 1 个 HTTP 请求用于 JS（而非 17 个）。Lighthouse 性能得分 +40。 |
| **M3：PWA 就绪** | Sprint N，第 4 天 | 可添加到主屏幕。离线 shell 加载。所有可缓存 blob 的“网络优先 + SW”。 |
| **M4：刷新后持久化** | Sprint N+1，第 3 天 | 页面刷新恢复了未读计数、草稿和最后查看的房间。没有 API 调用风暴。 |
| **M5：快速附件** | Sprint N+1，第 5 天 | 浏览器缓存 blob 以重新加载。大文件上的 HTTP Range 请求。 |
| **M6：作为契约的 API** | Sprint N+2，第 5 天 | 前端类型生成来自 openapi.yaml。CI 不一致检测。 |

---

## 结论

已验证的分析正确识别了 5 个盲点。其中，**方向 B（可观测性）** 是最具影响力的与成本比——它仅用 ~50 行代码就将最大的生产盲区转化为可监控的信号。**方向 A（前端 bundle + PWA）** 通过缩小后减少 16/17 个 HTTP 请求，对用户感知的延迟产生最直接的影响。

值得称赞的是，该代码库在这些领域具有异常良好的基础：NATS 层上的 W3C 追踪基础设施、用于离线重连的 `SeqGate` 去重、以及清晰的 crate 边界。5 个方向中没有一个是需要 3 个月重写的“架构纠正”——它们都是为期 2-5 天的增量冲刺，能很好地解决实际存在的用户和运维痛点。

**下一步建议：** 立即进行方向 B（4 个直方图 + 7 个 consumer 缺失——今天即可交付）。与方向 A 并行（bundle——明天即可交付）。在 Sprint N+1 中重新评估方向 C（缩略图），届时延迟数据可以量化移动带宽节省情况。
