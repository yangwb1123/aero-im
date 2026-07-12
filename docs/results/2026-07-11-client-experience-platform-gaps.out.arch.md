好的，我已经完整研读了 `2026-07-11-client-experience-platform-gaps.md`。这份文档出自资深代码扫描，发现了 5 个既有分析覆盖极低但影响重大的客户端缺口。以下是我作为架构师的完整分析。

---

# 架构师分析：Aero IM 客户端体验平台鸿沟

## 1. 架构评估

### 1.1 当前架构的优势

文档中提到的 5 个方向其实从侧面印证了当前架构的健康度——问题全部集中在 **web/ 客户端**，而非服务端。这意味着：

- **服务端/客户端边界清晰**：17 个 Rust crate 中没有一个需要为客户端 UX 缺口而重构。`aero-push` 的 `PushBackend` trait 设计足够抽象，可以无侵入地插入 `WebPushBackend`。
- **后端能力前置**：`search.rs` 的 FTS/向量/混合/高级操作符、`commands.rs` 的 `/poll` `/remind` `/giphy`、`saved_search_monitor.rs` 的实时监控——这些**服务器端已经实现的功能只是没有被客户端消费**。这不是架构问题，是交付时序问题。
- **Blob API 的通用性**：`/api/blobs/:id` 返回原始字节，不绑定内容类型——这意味着添加缩略图端点（`?thumb=256`）不需要破坏现有 API。

### 1.2 架构债务和技术债

| 债务类型 | 具体表现 | 严重程度 | 修复成本 |
|---------|---------|---------|---------|
| **缺少客户端抽象层** | 全 SPA 是用原生 ES2020 模块手写的，零框架、零状态管理、零数据层抽象。`context.js` 中的 `state` 是全局可变 Map，没有不可变更新、没有选择器、没有变化订阅 | **高** | 高（等于重写） |
| **状态存储位置单一** | 所有数据在内存 `state` 对象中，页面刷新全部丢失。`localStorage` 只在两个地方使用（JWT token + AI 历史） | **中** | 理论上通过 IndexedDB 逐步迁移 |
| **单一渲染路径** | `render.js` 的 `appendBlock` 没有内容类型的多态分派——所有文件走同一条 `<a>` 下载链接路径。这种架构使得未来添加新 block 类型不够优雅 | **中** | 可以通过引入 `BlockRenderer` 注册表模式解决 |
| **无离线优先设计** | `ws.js` 重连机制只考虑连接恢复，不考虑状态恢复。重连期间所有 `ws.send()` 返回 false 后不做 `pendingQueue` 缓存 | **高** | 需引入 `OfflineFirstWsClient` 封装 |
| **无测试分层** | 文档没提但 `web/` 中无单元测试或集成测试——这是所有 5 个方向的底层风险 | **高** | 构建支持 + 测试框架引入 |

### 1.3 关键设计决策评估

| 决策 | 是否合理 | 分析 |
|------|---------|------|
| **纯原生 ES2020 无框架** | ✅ **合理但双刃** | 零依赖、零构建步骤、零 Tree-shaking 复杂度。但状态管理和 DOM diffing 完全手动，在 5 个方向之后可能达到复杂度拐点。**当前阶段是合理的**——功能总量 ~6K JS，还没到 `useState` 管理的债 |
| **`state` 全局可变 Map** | ⚠️ **可接受但有上限** | 对于当前规模（~6K JS）是可管理的。但引入 IndexedDB 离线缓存 + 搜索 UI + Command Palette + 快捷键路由后，状态层需要正式的订阅/通知模式 |
| **REST API 优先 + WS 实时补差** | ✅ **正确** | 所有资源操作走 REST，实时事件通过 WS 扇出。这为 PWA 离线缓存提供了清晰的分界：**方法调用走 REST 需要网络，但已获取的资源可以读缓存** |
| **`PushBackend` trait 抽象** | ✅ **正确** | `aero-push` 的设计允许 FCM、APNs、Web Push 共享同一 `push_to_participant` 路由——这是有远见的设计 |

---

## 2. 扩展方向（5 个高价值架构方向）

### 方向 A：客户端数据层抽象（Offline-First State Layer）

> 这不是文档中列出的 5 个方向之一，而是**支撑方向一（PWA 壳）和方向三（搜索 UI）的底层架构变更**。

**为什么需要**：
- 目前各方向独立推进会导致各自引入自己的存储：方向一写 IndexedDB 消息缓存、方向三写搜索缓存、方向五写 SW 推送数据。缺一个统一的客户端数据层，会导致三个独立的存储路径，容易不一致。
- 当前 `state.messagesByRoom` 是内存 `Map`，如果直接改成每次从 IndexedDB 读再写回，而没有 **内存→存储** 的双层架构，页面切换会频繁触发异步存储读取，引入肉眼可见的延迟。

**核心挑战**：
- 如何在不引入 React/Vue 的情况下，设计一个 **响应式数据层**（类似 `zustand` + `idb-keyval` 的组合）
- 如何设计 **内存缓存 → IndexedDB 持久化 → REST API fallback** 的三层读取策略，且保证一致性
- 如何在不改变现有 `state.*` 引用模式的情况下引入（向后兼容）

**预期架构变更**：
```
web/
├── db/                    ← 新目录：数据层
│   ├── schema.js          -- IndexedDB 表定义 + 版本迁移
│   ├── store.js           -- 通用的 get/set/query 封装（对 idb 的包装）
│   ├── messages.js        -- 消息的 CRUD + 分页读取
│   ├── rooms.js           -- 房间列表 + 元数据缓存
│   └── pending.js         -- 离线待发送队列
├── state/
│   ├── Store.js           -- 响应式 Store 基类（发布-订阅模式）
│   ├── RoomStore.js       -- 房间列表 Store
│   ├── MessageStore.js    -- 消息 Store（自动同步 db/）
│   └── ParticipantStore.js-- 参与者缓存
├── context.js             -- 改用 Store 实例
└── app.js                 -- 通过 Store.subscribe 更新视图
```

**对现有系统的影响**：中。需要新建文件 + 逐步迁移 `context.js` 中的访问模式，但不需要重写现有渲染逻辑。

---

### 方向 B：Block 渲染器注册表（Plugin-Based Renderer Registry）

> 支撑方向二（富媒体预览）和后期的 Block 类型扩展（互式消息、投票结果卡片、AI 卡片等）。

**为什么需要**：
- 目前 `appendBlock` 是一个巨型 `switch`（`'text' | 'file' | 'voice' | ...`），每个新增 Block 类型都需要修改这个函数——违背开闭原则。
- 方向二要区分 `image`、`video`、`document`、`audio`——如果继续在 `appendBlock` 里加 `if`/`switch`，函数会迅速膨胀到不可维护。
- 未来还可能加入 `poll_result`、`ai_card`、`meeting_schedule`、`location` 等 Block 类型，需要一个可扩展的注册机制。

**核心挑战**：
- 纯 ES2020 没有接口/抽象类，如何实现 **注册表模式** 而不引入 TypeScript
- 渲染器之间如何协作（例如：`image` 渲染器可以选择使用 `Gallery`（Lightbox）渲染器来包装）
- 如何让渲染器可以自定义样式而不会 CSS 冲突

**预期架构变更**：
```javascript
// web/renderer/registry.js
const registry = new Map();

export function registerBlockRenderer(blockKind, renderer) {
  registry.set(blockKind, renderer);
}

export function renderBlock(block, parent, opts) {
  const renderer = registry.get(block.kind) || registry.get('fallback');
  return renderer.render(block, parent, opts);
}

// web/renderer/image.js
registerBlockRenderer('image', {
  render(block, parent) {
    const img = el('img', { attrs: { src: block.thumbnail_url } });
    img.addEventListener('click', () => gallery.open(block.blob_id));
    return img;
  }
});
```

**对现有系统的影响**：低。初期可以保留 `appendBlock` 作为 fallback，逐步迁移已注册的 Block 类型。

---

### 方向 C：全局快捷键/指令路由总线

> 支撑方向四（键盘快捷键 & Command Palette）。

**为什么需要**：
- 目前键盘事件分散在 `chrome.js`（Escape）、`app.js`（Enter）、`search.js`（Enter）、`composer.js`（所有输入）等多个文件中——没有统一的焦点系统，容易冲突（比如输入框内打 `/` 时不应该触发 Command Palette）。
- Command Palette 本身需要知道当前可用命令列表——这些命令目前分散在各个模块中。
- 快捷键配置**不应硬编码**——用户期望能自定义（虽然不是 P0）。

**核心挑战**：
- 焦点上下文管理：在 compose 框内打字时，`j/k` 不应导航消息、`/` 不应弹出指令面板
- 作用域/优先级：避免多个处理器竞争同一个按键
- 可发现性：用户如何知道有哪些可用的快捷键（Cmd+/ 帮助面板）

**预期架构变更**：
```javascript
// web/keyboard/bus.js
const bus = new Map(); // scope → Map(key → handler)

export function register(scope, keyCombo, handler, opts) {
  // scope: 'global' | 'composer' | 'search' | 'modal'
  // keyCombo: 'Cmd+K' | 'Escape' | 'j'
}

// 焦点变化时切换 scope
export function activateScope(scope) { currentScope = scope; }

// 全局 keydown 只在这个 bus 中处理
document.addEventListener('keydown', (e) => {
  const combo = normalizeKeyEvent(e);
  const handler = findHandler(currentScope, combo)
               || findHandler('global', combo);
  if (handler) { e.preventDefault(); handler(e); }
});
```

**对现有系统的影响**：低。可以逐步替换 `chrome.js` 和分散的 `keydown` 监听器。

---

### 方向 D：统一的 Push 路由网关（Unified Push Router）

> 支撑方向五（Web Push），同时优化现有的 FCM/APNs 实现。

**为什么需要**：
- 当前 `push_bot.rs` 对每个注册设备逐条发送 FCM/APNs 请求——如果用户有 3 个设备（桌面 Web Push + 手机 FCM + 平板 FCM），同一条消息会被发送 3 次。不是性能问题，但缺乏批量优化。
- Web Push 的 VAPID 密钥管理（轮换、撤销）与 FCM 的 Server Key 管理不同，需要统一的配置接口。
- 不同 Push backends 的投递确认/失败处理各有差异——FCM 返回 canonical ID（token 刷新）而 Web Push 返回 410 Gone（subscription 过期），需要统一的 `handleDeliveryStatus` 逻辑。

**核心挑战**：
- VAPID 密钥对的生命周期管理——是持久化到数据库还是配置文件
- Web Push subscription 的存储：需要新表 `web_push_subscriptions(participant_id, endpoint, p256dh_key, auth_key, user_agent)` 还是扩展现有 `push_tokens` 表
- 浏览器 push subscription 的过期检测（410 Gone）与自动清理

**预期架构变更**：
```
// 服务端新增
crates/aero-push/src/
├── web_push.rs              -- WebPushBackend: PushBackend trait 实现
├── vault.rs                 -- VAPID 密钥管理（生成/缓存/轮换）
└── router.rs                -- 统一路由：遍历所有 enabled backends

// 数据库迁移
migrations/NNNN_web_push_subscriptions.sql
// 或：扩展 push_tokens 表加一个 type 列 ('fcm' | 'apns' | 'web_push')

// 客户端新增
web/
├── sw.js                    -- Service Worker（含 push + notificationclick）
└── push-subscription.js     -- pushManager.subscribe + 向服务端注册
```

**对现有系统的影响**：中低。`PushBackend` trait 已存在，新增实现类即可。需要一次数据库迁移。

---

### 方向 E：跨房间搜索边界代理（Cross-Room Search Boundary Proxy）

> 虽然文档方向三提到搜索 UI，但架构层面一个更重要的问题是搜索边界如何在客户端统一处理。

**为什么需要**：
- 当前 `search.rs` 中每个搜索受 `assert_room_access` 约束，但跨房间搜索需要：
  - 客户端明确知道用户可以访问哪些房间（有 `state.rooms`）
  - 搜索结果中的每条消息需要带上房间来源，以便跳转
  - 搜索结果需要做 **房间名 + 消息预览** 的混合展示
- 如果跨房间搜索不加边界代理，会出现用户在搜索中看到不该看到的房间片段（虽然服务端会过滤，但 UI 展示路径不能依赖服务端过滤作为唯一防线——**defense in depth**）

**核心挑战**：
- 搜索结果的授权边界应在**服务端**执行（不可绕过）、但在**客户端**展示（不能泄露存在但无权访问的房间的计数/片段）。
- 分页连续性：跨房间搜索的分页 cursor 如何跨多个房间游走
- 搜索结果渲染：每个 hit 需要带 room 信息以便跳转——但 room 信息可能已从 `state.rooms` 中消失（用户关闭了该房间）

**预期架构变更**：
- 服务端 `search_advanced.rs` 扩展返回：每个 hit 带 `room_id` + `room_name`（服务端填充，因为客户端不一定有所需房间元数据）
- 客户端 `SearchResultStore`：维护搜索结果的内存索引，支持按房间分组显示
- 在 `search.js` 中引入搜索边界展示组件：类似 Slack 的 `in: #general` 过滤 chip

**对现有系统的影响**：中。需扩展服务端搜索响应格式 + 客户端搜索组件重写。

---

## 3. 接口设计建议

### 3.1 关键模块接口设计原则

| 原则 | 适用模块 | 说明 |
|------|---------|------|
| **分层隔离** | 数据层（db/） | IndexedDB 操作只通过 `db/messages.js` 等的 `export` 函数暴露，外部不直接接触 `IDBRequest`。实现可替换（如未来从 `idb` 库迁移到 `Dexie.js`） |
| **注册优先，继承靠后** | Block 渲染器、快捷键 | 使用 `registerX(name, handler)` 而非 `class X extends Y`。纯 ES2020 中组合优于继承 |
| **以 trait 为 seam** | 服务端 Push backend | `PushBackend` trait 已经是正确设计。Web Push 实现只需补一个新的 `impl PushBackend for WebPushBackend` |
| **单数据源（SSOT）** | 状态层 | 每条消息在 IndexedDB 中只有一份，内存 `MessageStore` 只是 IndexedDB 的 LRU 缓存。写操作统一经过 `store.update()` 方法，先写 DB 再更新内存 |
| **可取消/可超时** | 搜索 API 调用 | 用户在搜索过程中输入新关键词时，应自动取消前一个 pending 请求。接口应返回 `AbortController` 信号 |

### 3.2 是否需要新的抽象层

| 抽象层 | 必要性 | 引入时机 | 形式 |
|--------|-------|---------|------|
| **客户端数据层（Store）** | **高** | P0——方向一实施前 | 轻量发布-订阅模式，不引入框架。约 200 行 vanilla JS |
| **Block 渲染器注册表** | **中** | 方向二实施前 | `Map<string, {render, thumbnail?, priority?}>`，逐步迁移 |
| **快捷键总线** | **中** | 方向四实施前 | `Map<scope, Map<combo, handler>>` + 焦点管理 |
| **Push 路由网关** | **低** | 方向五实施时 | 服务端已有 `PushBackend` trait，只需新增实现 |

### 3.3 向后兼容策略

- **数据层迁移兼容**：现有 `state.rooms.get(id)` 调用在新架构下依然工作——`RoomStore` 实现 `get(id)` 作为同步方法（从内存缓存读取）。`RoomStore` 内部在初始化时异步从 IndexedDB 填充。所有现有代码不需要改引用路径。
- **Block 渲染器迁移兼容**：`appendBlock` 先检查注册表中是否有对应 `block.kind` 的渲染器，没有则走原有 `switch` 逻辑。逐步注册新渲染器，不一次性迁移全部。
- **Push subscription 注册**：现有 FCM/APNs token 存储不变。新 `web_push_subscriptions` 表独立存在，`push_bot.rs` 的路由逻辑改为 **遍历所有注册设备的推送目标**（FCM token + APNs token + Web Push subscription）——现有移动设备用户不受影响。
- **搜索响应格式扩展**：在现有 JSON 响应中新增 `room_name` 字段。旧客户端忽略未知字段。

---

## 4. 技术选型

### 4.1 是否需要引入新技术栈

| 技术 | 推荐 | 理由 | 风险 |
|------|------|------|------|
| **`idb` 库**（npm: `idb`） | ✅ **引入** | IndexedDB API 原生是 callback 风格的、极其冗长。`idb` 是一个 ~1KB 的 Promise wrapper，让 IndexedDB 操作从 15 行变成 3 行。无依赖 | 低。纯 wrapper，没有运行时复杂度 |
| **TypeScript** | ❌ **不引入** | 当前 ~6K JS 的代码库引入 TS 需要构建步骤（tsc/esbuild），破坏"零构建步骤"的设计原则。在添加新文件时可以逐步用 JSDoc 注解提供类型提示 | 中。引入 TS 会触发工具链重构——偏离文档中"不改变服务端架构"的约束 |
| **Workbox** | ⚠️ **可选** | Google 的 Service Worker 库，提供 runtime caching 策略。如果方向一的目标是"离线时缓存所有静态资源"，Workbox 只需要 3 行配置。但如果只需基本的 SW（缓存 manifest + 静态资源 + push 接收），手写 SW 更可控 | 低。Workbox 是配置式的，不会引入运行时复杂度 |
| **`web-push` Rust crate** | ✅ **引入** | Mozilla 维护的 Rust Web Push 库，实现 VAPID + RFC 8030。取代手写 HTTP 请求 + HMAC 签名 | 低。成熟库，MIT 许可 |
| **`cmdk` 风格的 Command Palette** | ❌ **不引入** | 键盘快捷键模块完全手写定制——SPA 的 Command Palette 需要集成到现有 `context.js` 的状态中，第三方库很难适配。参考 Spotlight 的设计模式但手写 | 无外部依赖风险 |

### 4.2 第三方依赖评估标准

对于 `aero-push` 新增依赖的评估（`web-push` crate）：

| 标准 | 评估 |
|------|------|
| **许可证兼容性** | MIT/Apache 2.0（与项目现有一致） |
| **维护状态** | 最后更新 2024 Q3（稳定，不需要频繁更新） |
| **依赖树大小** | 轻量（仅需要 http + base64 + 一些 crypto primitives） |
| **安全审计历史** | 无已知 CVE |
| **替代方案** | 自实现 VAPID（~400 行 Rust）——如果 `web-push` 有过多的传递依赖或维护停止，可以自实现 |

**决策**：初期使用 `web-push` crate，但将其封装在 `WebPushBackend` 之后（见 `aero-push/src/lib.rs` 模式）。未来如果 `web-push` 出现问题，只需重写 `WebPushBackend` 内部而不影响 `push_bot.rs`。

### 4.3 自建 vs 采购决策

这 5 个方向中，没有任何一个适合采购/外购：

| 方向 | 为什么不采购 |
|------|------------|
| PWA 壳 + 离线缓存 | 这是 SPA 自身的架构改进，外部服务无法干涉客户端架构 |
| 富媒体预览 | Lightbox/gallery 组件可以 npm install，但块渲染器注册表的设计必须与现有的 `appendBlock` 和 `Blob API` 集成——定制成本高于通用的花架子 |
| 搜索 UI | 搜索必须对接 `aero-server/search.rs` 的专有 API 响应格式。第三方的 Algolia/SearchKit 只对接自己的 SaaS——不适用 |
| 键盘快捷键 | 纯自定义逻辑 |
| Web Push | 需要与现有的 `push_bot.rs` + `PushBackend` trait 集成，外部服务无法提供 service worker + subscription 管理 |

**结论**：全部自建。

---

## 5. 实施路线图

### 5.1 优先级排序

```
P0 ───────────────────────────────────────────────────────────── P2
(依赖阻塞)                          (独立高价值)                  (锦上添花)

方向一（PWA 壳）  方向五（Web Push）  方向二（富媒体预览）  方向三（搜索 UI）  方向四（快捷键）
    ↓                                     ↓                    ↓
    └────── 阻塞 ──────┘                  └────── 互不依赖 ──────┘
```

| 优先级 | 方向 | 依赖 | 风险 | 建议启动时机 |
|--------|------|------|------|------------|
| **P0** | **方向-A：数据层抽象** | 无 | 低—中（新代码，不影响现有） | **立即**——它是所有后续方向的架构基础 |
| **P0** | **方向一：PWA 壳（SW + IndexedDB 消息缓存）** | 方向 A | 低（成熟模式） | 紧跟方向 A |
| **P1** | **方向五：Web Push 推送** | 方向一（SW 安装） | 低（RFC 成熟 + 有 Rust 库） | 方向一完成后 |
| **P1** | **方向二：富媒体内联预览** | 方向 B（Block 渲染器） | 低（纯 CSS/DOM） | 可与 P0 并行 |
| **P2** | **方向三：全功能搜索 UI** | 方向 A（数据层） | 中（跨房间边界） | P0 完成后 |
| **P2** | **方向四：键盘快捷键** | 方向 C（快捷键总线） | 低 | 随时可开始 |

### 5.2 阶段划分和里程碑

#### 阶段 0：[0-1 周] 基础设施插入

| 任务 | 产出 | 风险 |
|------|------|------|
| `web/db/schema.js` — IndexedDB schema 定义 | 版本化 schema，支持数据迁移 | 低 |
| `web/db/store.js` — 通用 `idb` wrapper | 统一 get/set/query/delete | 低 |
| `web/state/Store.js` — 响应式 Store 基类 | `subscribe()`, `notify()`, `get()`, `set()`, `persist()` | 低 |
| `web/state/MessageStore.js` — 消息 Store（内存→DB 双层） | 页面刷新后消息不丢失 | 低 |
| `web/state/RoomStore.js` — 房间列表持久化 | 进入 SPA 直接显示房间列表，无需 loading | 低 |

**里程碑**：页面硬刷新后进入 SPA，房间列表和最近 50 条消息立即渲染（读 IndexedDB），然后在后台拉最新数据。

#### 阶段 1：[2-3 周] PWA 壳 + Web Push 推送

| 任务 | 产出 | 风险 |
|------|------|------|
| `web/sw.js` — Service Worker 实现 | 离线时 SPA 不白屏 + 静态资源缓存策略 | 低 |
| `web/manifest.json` — 完整 PWA 元信息 | 可安装的 Web 应用 | 低 |
| `web/index.html` — SW 注册 + 离线 fallback | `navigator.serviceWorker.register` | 低 |
| `web/push-subscription.js` — pushManager.subscribe | 浏览器推送订阅流程 | 低 |
| `aero-push/src/web_push.rs` — WebPushBackend | VAPID 签名 + RFC 8030 HTTP 请求 | 低 |
| `migrations/` — web_push_subscriptions 表 | 持久化浏览器推送 subscription | 低 |

**里程碑**：关闭浏览器标签页后，收到新消息仍有桌面通知弹出。

#### 阶段 2：[2-3 周，与阶段 1 并行] 富媒体预览

| 任务 | 产出 | 风险 |
|------|------|------|
| `web/renderer/registry.js` — Block 渲染器注册表 | 可扩展的渲染器注册机制 | 低 |
| `web/renderer/image.js` — 图片内联渲染 + 缩略图 | `<img>` 内联展示 | 低 |
| `web/renderer/video.js` — 视频内联播放 | `<video controls>` | 低 |
| `web/renderer/audio.js` — 音频独立播放器 | `<audio controls>`（非 voice block） | 低 |
| `web/renderer/document.js` — 文档预览（PDF Office） | `<embed>` / Google Docs Viewer iframe | 中（跨域/安全） |
| `web/gallery.js` — Lightbox 全屏阅览 | 图片点击→全分辨率→左右导航 | 低 |
| `web/style.css` — Gallery + 内联媒体样式 | `max-width: 100%`, `border-radius`, 过渡 | 低 |

**里程碑**：点击图片消息直接看到 `<img>`，而非 🖼 下载链接。

#### 阶段 3：[3-4 周] 搜索 UI + 键盘快捷键

| 任务 | 产出 | 风险 |
|------|------|------|
| `web/search.js` — 搜索面板重写（模式选择 + 过滤器 chip + 无限滚动） | 可比肩 Slack/Discord 的搜索体验 | 中 |
| `web/saved_searches.js` — 保存搜索管理 UI | 已有后端能力被前端消费 | 低 |
| `web/keyboard/bus.js` — 快捷键总线 | 上下文感知的快捷键路由 | 低 |
| `web/keyboard/shortcuts.js` — 预置快捷键注册 | Cmd+K, /, j/k, Cmd+Shift+[等 | 低 |
| `web/keyboard/palette.js` — Command Palette 组件 | 模态面板 + 键盘导航 + 模糊搜索 | 中 |
| `web/commands.js` — 斜杠指令自动补全 | `/remind` `/poll` `/giphy` 补全 UI | 低 |
| 服务端 `search_advanced.rs` 扩展 | 返回 `room_name` + 分页 cursor 优化 | 低 |

**里程碑**：Cmd+K 弹出切换器 → 输入房间/人名 → Enter 跳转；`/remind` 打出自动补全；搜索结果有高亮和翻页。

### 5.3 关键里程碑总览

| 里程碑 | 时间 | 可演示的用户价值 |
|--------|------|-----------------|
| M0: 数据层就绪 | 第 1 周 | 不能直接看到，但所有后续功能依赖它 |
| M1: 离线不白屏 | 第 3 周 | 断网时 SPA 加载静态页面 + 显示最后缓存的房间列表 |
| M2: Web Push 通知 | 第 3 周 | 关掉标签页也能收到新消息通知 |
| M3: 图片/视频内联 | 第 3 周 | 图片不再显示为 🖼 图标 |
| M4: 全功能搜索 | 第 6 周 | 使用 `from:@user after:2026-01` 搜索历史消息 |
| M5: 键盘生产力 | 第 6 周 | 全键盘使用 Aero IM（Cmd+K 切换、/ 指令、j/k 导航） |

### 5.4 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|---------|
| **IndexedDB 存储配额问题** | 低 | 中——用户消息过多导致浏览器限制存储 | - 设置单房间缓存上限（如最近 500 条）<br>- IndexedDB 的 `navigator.storage.estimate()` 监控配额<br>- 添加 `stale_messages_sweep` 时间戳清理 |
| **Service Worker 更新管理** | 中 | 中——旧 SW 缓存导致用户看到过期内容 | - 使用 `skipWaiting()` + `clients.claim()` 立即激活新 SW<br>- 实现版本化缓存命名（`aero-v2-cache-v2`）<br>- `activate` 事件中清理旧缓存 |
| **Web Push VAPID 密钥泄露** | 低 | 高——第三方可以模拟服务端发推送 | - 密钥不在代码中硬编码<br>- 通过环境变量注入（`AERO__VAPID__PRIVATE_KEY`）<br>- 支持密钥轮换（`/api/admin/push/rotate-vapid`） |
| **搜索边界泄露** | 中 | 高——用户搜索看到不可访问的房间片段 | - **搜索结果中的 `room_name` 由服务端填充**，客户端不做服务端元数据查询<br>- 搜索结果的 `href` 跳转仍需走 `assert_room_access`——不能依赖客户端过滤 |
| **客户端 JS 规模增长** | 高（中期） | 中——从 ~6K 增长到 ~12K 后，纯 vanilla JS 的 organized 挑战 | - 在代码量达到 ~12K 时考虑引入 ESBuild 做最小化 + 代码分割<br>- 关注点分离（data / render / keyboard / api 四个清晰目录）<br>- 不考虑迁移到 TypeScript（会增加构建复杂度） |
| **测试覆盖不足** | 高 | 高——纯前端无测试，回归风险随功能增长 | - 新数据层（Store/db）必须写测试<br>- 引入 `web-test-runner` 或 `node --test` 跑 DOM 无关的逻辑<br>- 渲染器测试用 JSDOM 或 headless Chrome（Playwright） |

---

## 总结看法

这份 `2026-07-11-client-experience-platform-gaps.md` 是我近期看到的**质量最高的客户端缺口分析之一**。它的价值在于：

1. **证据驱动**：每个断言都附带可复现的 grep 命令，没有猜测。
2. **不重复**：明确列出了 14 个「既有分析已覆盖的方向」，文档的 5 个方向是真正的新发现。
3. **切割合理**：P1/P2 分级有说服力——方向一（PWA 壳）是方向五（Web Push）的前置，方向二（富媒体）虽然是 P1 但可以和 P0 并行。
4. **保守务实**：所有方向都是"使用已存在的后端能力"≠"新造基础设施"。

我对分析文档的补充建议只有一个：

> **建议在路线图中考虑引入一个短期的"客户端冒烟测试"阶段**（阶段 0 的一部分，1-2 天），原因是 web/ 目前完全没有自动化测试。这不是指责，而是风险量化。在开始 5 个方向的并行开发前，给每个新增模块（`db/`、`state/`、`renderer/`）配上基础的单元测试，远比 5 个方向全部完成后再补测试要经济得多。

如果您需要我基于此分析制定更具体的实现计划，或者对某个方向做更深度的代码级影响分析，请告诉我。
