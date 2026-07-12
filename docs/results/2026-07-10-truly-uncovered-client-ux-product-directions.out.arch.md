好的，我已经完整阅读了文档。以下从架构师视角进行深入分析。

---

# 架构分析：5 个未被覆盖的客户端/UX 产品化方向

## 1. 架构评估

### 1.1 当前架构的优势

在评估这 5 个方向之前，需要先承认当前架构的几个重要优势，它们是分析的基础：

| 优势 | 表现 |
|------|------|
| **后端优先 (Backend-First)** | 5 个方向涉及的功能（bookmarks, pins, translate, settings APIs, user-groups...）**后端 100% 已实现**。这是正确的工程策略——服务逻辑先于 UI，API contract 先行。其他产品常犯的反向错误（UI 先做然后发现 API 不支持）在 Aero IM 中被避免了 |
| **总线解耦** | NATS JetStream durable/ephemeral consumer 模型使实时扇出与 REST API 独立，新功能（如置顶实时更新）只需加 `RoomEvent::Pin` variant 即可无中断扩展现有扇出管道 |
| **无框架前端保持低负债** | 纯 ES2020 SPA 没有 Webpack/Vite/React 的依赖、构建、类型系统债。当团队评估是否引入框架时，起点是极低的技术债而非"已有 800 个组件需要迁移" |

### 1.2 架构局限性与技术债

**局限一：前端缺少组件抽象层（关键债）**

这是**最根本的架构局限性**。当前 `web/` 下的组织方式是"功能模块 + 全局 DOM 操作"：

```
web/
├── app.js       # ~1200 行，路由 + 状态 + 事件 + 渲染
├── render.js    # ~800 行，消息列表/频道/用户渲染
├── modals.js    # ~600 行，所有弹窗
├── media.js     # ~250 行，上传/拖拽
└── mentions.js  # ~108 行，@提及
```

方向三（实体选择器）揭示了这个问题：当前需要一个可复用的 `<entity-picker>` 组件级抽象，但现有架构**没有支持组件注册、生命周期、状态隔离、DOM 清理的机制**。每个"组件"实质上是向 `document.body` 追加一堆 DOM 节点，然后手动管理移除。

- 当 `app.js` 的 `handlePin` 只执行 `toast()` 而无对应面板渲染时——不是开发者不想做，而是"置顶面板"作为一个独立 UI 单元没有一个干净的挂载点。
- 当 `mentions.js` 无法重用于"添加成员"——不是复用不了，而是 **不存在"以组件形式实例化、配置、挂载"的设计模式**。

**局限二：前端状态管理隐式且碎片化**

当前状态模式是 `state` 全局对象 (`app.js` 的 `let state = {...}`)，被多个模块直接读写：

```js
// modals.js 直接修改 state
state.currentRoomId = roomId;
// render.js 直接读取 state
if (state.searchQuery) ...
// media.js 也读
const roomId = state.currentRoomId;
```

当方向一、二、四、五同时实现时，这个模式会崩溃：

- 文件上传进度应该驱动 UI 更新 → 谁持有进度状态？`media.js` 的局部变量？还是 `state` 上？
- 设置面板修改后需要通知消息列表重新渲染 → 怎么通知？事件总线？还是直接调 `render()`？
- 书签面板打开时，用户又在另一个 tab 收藏了新消息 → 怎么同步？

**当前没有任何发布-订阅模式、也没有单向数据流。这是方向一到五实现过程中的最大架构阻力和风险。**

**局限三：Web SPA 缺少"骨架"概念——面板系统未抽象化**

当前 UI 中确实有"侧面板"的概念（`#drawer-bookmarks` 等被文档提及），但未作为架构元素存在。没有标准化的面板生命周期（open/close/toggle）、层级管理（同时只能打开一个？还是可以叠放？）、转场动画。

方向五的置顶面板、方向四的书签面板、方向二的设置面板——三个面板同时出现时，当前架构没有机制管理它们之间的 z-index、关闭行为、历史记录。

**局限四：HTTP 客户端缺少拦截器/中间件模式**

`api.js` 的 `request()` 是典型的"早期抽象"——一个 `fetch` 封装，支持 `GET/POST/PATCH/DELETE`、JSON 序列化、错误处理（401 跳登录）。但它缺少：

- **进度回调**：方向一需要 `upload.onprogress`，但 `fetch` 不支持原生上传进度，必须降级到 `XMLHttpRequest`
- **请求/响应拦截器**：不能全局注入请求 ID、token 刷新、请求耗时埋点
- **取消令牌组合**：每个调用方自己管理 `AbortController`，无统一取消机制

这意味着方向一（上传进度）不仅需要改 `api.js` 的底层实现，还要保持向后兼容所有现有调用方。

---

## 2. 扩展方向 — 高价值架构扩展

基于上述局限性和 5 个方向，我给出以下架构扩展建议。这些不是具体功能实现，而是**为 5 个方向提供支撑的架构层**。

---

### 方向 A（P0 · 架构基础设施）：前端组件系统（Component System）

**为什么需要**

方向三（实体选择器）是直接触发点。但本质问题是：**没有组件抽象，就无法在 10+ 场景复用同一交互模式，且无法安全地管理 DOM 生命周期**。

方向一的上传队列（每个文件一个进度条）→ 至少需要 `<upload-item>` 组件。
方向四的书签面板 → 需要 `<bookmark-list>` 组件。
方向五的置顶面板 → 需要 `<pinned-item>` 组件。

三个方向加起来约 15+ 独立 UI 单元。如果没有组件系统，15 个单元将被缝进 `modals.js` 和 `render.js`，使它们从"已过大"变成"无可救药"。

**核心挑战**

1. **零外部依赖约束**：项目约定纯 ES2020 零框架，所以不能引入 Lit/Svelte/Preact。需要自建极简组件系统。
2. **DOM 清理（Cleanup）**：手动管理 DOM 的最大问题是内存泄漏——组件移除时未清理的事件监听器、定时器、子组件。必须设计 `disconnectedCallback` 等价物。
3. **样式隔离**：当前所有 CSS 是全局的，组件样式容易被其他模块覆盖。

**预期的架构变更**

```
web/
├── components/           # 新增
│   ├── Component.js      # 基类：mount/unmount/update 生命周期
│   ├── EntityPicker.js   # 方向三
│   ├── ProgressBar.js    # 方向一
│   ├── UploadQueue.js    # 方向一
│   ├── BookmarkList.js   # 方向四
│   └── PinnedList.js     # 方向五
├── app.js                # 保持路由/顶层状态
├── render.js             # 缩小：只保留消息渲染逻辑
├── modals.js             # 缩小：弹窗改为 Component 子类
└── media.js              # 缩小：只保留上传调度逻辑
```

**对现有系统的影响**

- **中风险**：需要逐步迁移。不能一次性把所有模块改成组件系统。建议：
  1. 先做 `Component.js` 基类（< 100 行）
  2. 用 `EntityPicker` 作为验证组件（方向三 L1）
  3. 验证成功后，逐组件迁移
- 迁移期间新旧模式并存（但并存周期应限制在 2 周内）

**方案选择**：

| 选项 | 权衡 |
|------|------|
| **A: CustomElement (Web Components)** | 原生浏览器的 `customElements.define` + Shadow DOM 样式隔离。优点：零依赖、样式隔离。缺点：Shadow DOM 与现有全局 CSS 交互复杂；`<slot>` 投影需要学习 |
| **B: 极简类组件（推荐）** | `class Component { mount(container); unmount(); update(props); }`，内部用 `innerHTML` 或 `DocumentFragment` 渲染。优点：极简（~80 行基类）、与现有 DOM 操作无缝衔接、无 Shadow DOM 复杂性。缺点：无样式隔离（但可通过 BEM 命名 + CSS 变量缓解） |
| **C: 引入 Lit** | Google 的 5KB Web Components 库。优点：声明式模板、响应式更新。缺点：引入 npm 依赖、需要构建步骤、违反项目当前"零依赖"约束 |

**我的建议：选 B**。在当前的工程约束下（零框架、纯 ES2020），极简类组件是最低侵入、最高 ROI 的方案。未来如果组件数量 > 30，再评估是否迁移到 Lit。

---

### 方向 B（P0 · 架构基础设施）：前端事件总线（Event Bus）

**为什么需要**

当前的功能级事件模式是 "DOM event + 模块直接调函数"：

```js
// app.js
ws.on('msg:pin', (data) => {
  // 直接调 render.js 的渲染函数
  handlePin(data);
  // 直接调 toast
  toast('一条消息被置顶');
});
```

当方向四（书签）和方向五（置顶）同时运行时：

- 用户收藏一条消息 → `bookmarks.js` 需要通知 `render.js` 更新该消息的图标状态
- 用户置顶一条消息 → `pins.js` 需要通知 `render.js` 更新消息角标 + 置顶面板刷新 + 频道标题更新计数
- 文件上传完成 → `media.js` 需要通知消息列表插入新消息 + 上传队列移除已完成项

**没有事件总线，这些跨模块通信将退化为"全局函数调全局函数"——这是技术债的放大器。**

**核心挑战**

1. **事件命名空间**：避免冲突。`pin` 事件可以被用于 PIN code、置顶、图钉图标——需要 `msg:pinned`、`room:pin-count-changed` 的约定
2. **内存泄漏**：组件 unmount 时必须解除所有订阅
3. **异步安全性**：事件处理函数如果抛异常，不应该影响总线中的后续订阅者

**预期的架构变更**

在 `web/` 下新增 `bus.js`（~50 行）：

```js
class EventBus {
  constructor() { this._handlers = new Map(); }
  on(event, handler, context) { /* 注册 + 返回取消函数 */ }
  off(event, handler) { /* 精确移除 */ }
  emit(event, payload) { /* 同步广播，try/catch 每个 handler */ }
  once(event) { /* 仅触发一次 */ }
}
```

`app.js` 中的 `ws` 事件监听器改为"接收 WS 帧 → 转换为业务事件 → 向 bus 派发"，消费方通过 `bus.on('msg:pinned')` 订阅。

**对现有系统的影响**

- **低风险**：事件总线是增量引入的，不影响现有直接调用的模式。可以先在方向四/五的模块中使用，逐步替换现有直接跨模块调用。
- 现有 `app.js` 中的事件处理（~20 个 WS 事件处理函数）可以逐步迁移到 `bus.emit` 模式，不需要一次性改造。

---

### 方向 C（P1 · 前端架构增强）：API 客户端层重构——引入拦截器 + 进度支持 + 去重缓存

**为什么需要**

方向一的"上传进度"直接撞击当前 `api.js` 的架构限制：`fetch` 不支持 `upload.onprogress`。

但更深层的问题是：当前 `api.js` 将 HTTP 请求视为"发起 → 等待 → 返回"，缺少现代客户端库（如 axios、ky）的拦截器链概念。

需要的具体能力：

| 需求 | 当前限制 |
|------|---------|
| 上传进度回调 | `fetch` 不支持 xhr.upload.onprogress；需使用 `XMLHttpRequest` |
| 请求重试 | 弱网下上传到 80% 掉线 → 整个失败，没有自动重试 |
| 去重缓存 | 方向一建议上传前 SHA-256 去重，当前无客户端缓存 |
| 请求级取消传播 | 方向一"取消上传" → 需要 `AbortController` 在 API 层统一管理 |
| 401 自动刷新 token | 当前在 `request()` 中硬编码 401 跳登录，没有刷新尝试 |

**核心挑战**

1. **向后兼容**：当前 `api.js` 的 `request()` 有 50+ 调用方，不能一次性全改
2. **XMLHttpRequest vs fetch**：上传进度要求使用 XHR，但现有代码基于 `fetch`。需要适配层，使两种方式对调用方透明
3. **上传重试的幂等性**：重试上传已部分写入 S3 的 blob → 后端 blob dedup（方向一建议）是前提

**预期的架构变更**

```js
// web/api.js 重构
class ApiClient {
  constructor(baseUrl) {
    this._interceptors = { request: [], response: [] };
    this._adapter = 'fetch'; // 默认：fetch；大文件上传自动降级到 xhr
  }
  
  upload(path, file, { onProgress, signal }) {
    return this._adapter === 'xhr' 
      ? this._xhrUpload(path, file, { onProgress, signal })
      : this._fetchUpload(path, file, { onProgress, signal });
  }
  
  request(method, path, opts) {
    // 统一拦截器链
    const req = this._applyRequestInterceptors({ method, path, opts });
    return this._fetch(req);
  }
}
```

保持 `api.uploadBlob` / `api.get` / `api.post` 等现有封装函数的签名不变，内部改用新 `ApiClient`。

**对现有系统的影响**

- **中风险**：`api.js` 的变动会影响所有 HTTP 调用。关键约束是**保持所有现有调用方签名不变**（只改内部实现，不改外部接口）。
- 分两步走：
  1. 先加 `_xhrUpload` 私有方法，`uploadBlob` 内部判断文件大小选择适配器
  2. 再重构 `request()` 加入拦截器链——且旧签名保持不变

---

### 方向 D（P1 · 架构基础设施）：面板管理系统（Panel/Sheet Manager）

**为什么需要**

方向二（设置面板）、方向四（书签面板）、方向五（置顶面板）加起来至少 3 个独立面板。加上已有的：用户列表面板、频道信息面板、搜索面板——**6+ 个侧面板**。

当前不存在的架构机制：

- **面板栈管理**：打开置顶面板 → 再打开书签面板 → 后打开的面板覆盖前者 → 关闭后者时应该恢复前者还是关闭全部？
- **一次性关闭所有面板**：`Esc` 键关闭最上层面板，还是关闭全部？
- **面板间的联动**：在书签面板中点击书签 → 应该关闭书签面板 + 跳转到消息所在房间。但当前面板系统不支持"打开 A → 触发 B → 同时关闭 A"的工作流。
- **移动端适配**：面板在桌面端是"侧边展开"，但在移动端应该是"全屏从底部滑入"。当前无响应式机制。

**核心挑战**

1. **层级管理**：面板可以嵌套吗？不能（否则 z-index 管理失控）。应该限制同时只能有一个面板展开。
2. **动画与性能**：面板打开/关闭应该有 CSS transition。但当前 DOM 是"移除添加"而非"显示隐藏"——切换面板时会触发整个 DOM 树的 reflow。
3. **"无面板时"的优雅降级**：用户不想用面板时，现有的"消息列表全宽"应该保持可用。

**预期的架构变更**

```
web/
├── panels/
│   ├── PanelManager.js    # 面板注册 + 层级 + 转场
│   ├── SettingsPanel.js   # 方向二
│   ├── BookmarksPanel.js  # 方向四
│   └── PinnedPanel.js     # 方向五
```

`PanelManager` 的核心接口：

```js
class PanelManager {
  register(name, panelComponent)  // 注册面板
  open(name, props)               // 打开（关闭当前）
  close()                         // 关闭当前
  toggle(name, props)             // 切换
  get current()                   // 当前面板
}
```

**对现有系统的影响**

- **低风险**：PanelManager 是一个新的架构层，不影响现有代码。现有的面板（用户列表、频道信息）可以逐步迁移。
- 建议在面板数量达到 4 个之前开始做——正好是现在。

---

### 方向 E（P2 · 架构优化）：前端数据缓存层（Client-Side Data Cache）

**为什么需要**

方向三（实体选择器）中 @mention 需要频繁搜索成员。当前实现是**每次击键都调用 `api.searchMembers()`**。在 300 人工区中问题不大，但在 100,000 人工区中——每次搜索后端的 `pg_trgm` 索引消耗 + 网络往返 RTT 就会成为瓶颈。

更深层的原因是：**前端没有任何已知数据的缓存**。

当前缓存状况：

| 数据类型 | 当前 | 应该 |
|---------|------|------|
| 当前用户 Profile | 每次刷新页面从 `/api/me/profile` 重新获取 | localStorage + 版本戳 |
| 频道列表 | 每次 `refreshRoomList()` 从后端获取 | 内存缓存 + WS 事件增量更新 |
| 成员列表 | 从不缓存（每次关 | 持久化到 IndexedDB |
| 最近 @提及的人 | 完全不维护 | localStorage LRU |

**核心挑战**

1. **数据新鲜度**：缓存数据可能与服务器不一致。需要策略：TTL + WS 驱动无效化
2. **缓存大小控制**：10 万用户的搜索建议缓存 ≈ 8MB（每个用户 ~80 字节）。IndexedDB 够用但需要逐出策略
3. **LRU 实现的正确性**：方向三需要的"最近使用"不复杂，但扩展到"热门频道"时需要 hit-count 排序

**预期的架构变更**

新增 `web/cache.js`——一个简单的内存 + localStorage 缓存层：

```js
class DataCache {
  constructor(name, opts = { ttl: 300_000, maxEntries: 1000 }) {}
  
  get(key)           // 检查 TTL，过期返回 null
  set(key, value)    // 存入 + 更新时间戳
  invalidate(key)    // 定向清除
  invalidateAll()    // 全部清除（如用户登出）
  getRecent(limit)   // LRU 遍历
}
```

**对现有系统的影响**

- **低风险**：完全新增模块，不影响现有逻辑。API 调用方可以选择性使用缓存（走 `cache.wrap('members:'+query, () => api.searchMembers(query))`）。
- 注意：不要尝试自动缓存所有 GET 请求——显式缓存的错误率远低于隐式缓存。

---

## 3. 接口设计建议

### 3.1 前端模块间的接口原则

当前全局状态 `state` 对象是隐式接口——任何模块都可以读写任何字段。重构时应遵循：

**原则一：单向依赖层次**

```
app.js (路由 + WS) → bus.js (事件总线) → components/* (组件) → render.js (纯渲染)
                                ↕
                           api.js (HTTP)
```

- `components/*` 不能直接 import `app.js`（反向依赖）
- 组件通过事件总线派发"用户操作完成"事件，由 `app.js` 监听并决定下一个动作
- `render.js` 缩小为纯渲染函数：`(data, container) → DOM`，不持有状态

**原则二：组件 Props Down, Events Up**

```js
// 组件接收 props，不修改外部状态
class EntityPicker extends Component {
  constructor(props) {
    // props: { mode, multi, recent, exclude, onSelect, onClose, fetch }
  }
}

// 组件通过回调向上通信，不直接调 app.js
picker.on('select', (items) => { bus.emit('entity:selected', items); });
```

**原则三：API 层关注点分离**

```
api.js
  ├── upload(path, file, opts)    # 上传专用（XHR + 进度）
  ├── request(method, path, opts) # 通用请求（fetch + 拦截器）
  └── get, post, patch, delete    # 便捷封装（调 request）
```

不要将业务逻辑塞进 API 层。上传完成后，`media.js` 负责判断是否发送消息，`api.js` 只负责传输。

### 3.2 是否需要新的抽象层

| 抽象层 | 必要 | 理由 |
|--------|------|------|
| **组件系统（Component.js）** | ✅ 必要（P0） | 没有它，10+ 个新 UI 单元将落入 `modals.js` 造成不可维护 |
| **事件总线（bus.js）** | ✅ 必要（P0） | 跨模块通信的最低耦合方式 |
| **面板管理器（PanelManager）** | ✅ 建议（P1） | 6 个面板的层级/转场/生命周期必须统一管理 |
| **数据缓存（DataCache）** | ⚠️ 可选（P2） | 大工作区 (>1000 人) 必须；小团队 (<100 人) 可有可无 |
| **虚拟列表（VirtualList）** | ⚠️ 可选（P2） | 书签/消息列表如果预期 >500 条则必须 |

**不建议**引入的新抽象层：

- **Router**：当前 `app.js` 的 `switch(hash)` + 状态机模式足够应付
- **状态管理库**（Redux 风格）：纯 ES2020 SPA 的状态复杂度不需要受控的 reducer
- **构建工具**：当前 `index.html` 的 `<script type="module">` 无打包方案对零组件项目是可接受的。引入 Webpack 会增加 dev 工程链复杂度

### 3.3 向后兼容性策略

本质问题：**这 5 个方向涉及的前端改造是增量增强，不是重构。任何时候都不应该破坏当前正在工作的 UI。**

| 策略 | 应用范围 |
|------|---------|
| **侧边面板模式**（而非替换模式） | 设置面板、书签面板、置顶面板——新建 `#drawer-*` 容器，不影响现有 DOM 树。用户不打开面板 = 零可见变化 |
| **CSS 变量 + 增量类名** | 新 UI 使用的 CSS 变量（`--panel-bg`, `--upload-progress-color`）不影响现有硬编码颜色 |
| **功能门（Feature Gate）** | 新 UI 组件通过 `state.features.xxx` 门控。万一有问题，关闭门即可回退，不需要 revert 代码 |
| **API 版本不变** | 所有新功能**复用现有 REST API**，不改路径/参数/响应格式。这要求后端已实现的 API 签名与前端需求一致——已经从文档确认一致 |
| **增量加载** | 组件 JS 文件按需加载（`import('./components/EntityPicker.js')`），不增加初始加载体积 |

---

## 4. 技术选型

### 4.1 是否需要引入新技术栈

**核心判断：不需要。**

5 个方向覆盖的是前端产品化缺口，不是底层技术能力的缺口。现有技术栈（纯 ES2020 + CSS 变量 + DOM API）经过增强后完全可以承载。引入 React/Vue/Svelte 的成本（构建工具链 + 学习曲线 + 现有代码重构）远大于收益。

**唯一需要评估的技术引入**：

| 技术 | 评估 | 决策 |
|------|------|------|
| **Web Components (Custom Elements)** | 提供原生组件封装 + Shadow DOM 样式隔离。但 Shadow DOM 与全局 Bootstrap 式 CSS 的交互需要额外适配层，且 polyfill 在老旧浏览器上可能不兼容 | ⚠️ 当前不建议。选择方案 B（极简类组件），未来可考虑迁移到 Lit |
| **IndexedDB** | 方向 E（数据缓存层）需要客户端持久化。localStorage 只能存 5-10MB 字符串；IndexedDB 存结构化数据（成员列表、频道索引）更合适 | ✅ 需要引入。但只在 DataCache 层内部使用，对外暴露简单 `get/set` 接口 |
| **exiftool / ImageMagick** | 方向一 L3 (EXIF 剥离 + 缩略图) 需要服务端图片处理工具 | ✅ 需要引入。建议通过 `std::process::Command` 调用系统工具而非 Rust crate 原生实现（`kamadak-exif` 解析但不剥离；`image` crate 不支持 EXIF 写入）。成熟系统工具更可靠 |
| **Service Worker** | 方向一（上传）可能需要离线上传队列 | ❌ 暂不需要。Service Worker 的复杂度与上传进度需求的 ROI 不匹配 |

### 4.2 第三方依赖评估标准

| 标准 | 阈值 | 解释 |
|------|------|------|
| 许可协议 | MIT / Apache 2.0 / BSD | GPL/AGPL 对商业产品禁止引入 |
| 打包大小 | < 20KB (gzip) | 当前 SPA 整体 ~70KB gzip，每个依赖不应增加 >30% |
| Rust crate 安全性 | `cargo deny` 通过 + 无 unsound issues | 对 `aero-server` 的二进制体积增加 < 5% |
| Web 依赖的前后兼容 | 无需构建工具/打包器 | 使用 ES module CDN 或原生 ESM 导入，不引入 Webpack 构建步骤 |
| API 稳定性 | 非 0.x 版本 | 避免 API 在未来的大版本中不兼容 |

根据此标准：

| 依赖 | 评估 |
|------|------|
| **Lit**（Web Components 库） | 5KB gzip，MIT，稳定 v3。满足标准。但需要 ESM import —— 增加一个构建阶段。暂不引入 |
| **idb**（IndexedDB 封装） | 1.5KB gzip，MIT。极简 wrapper，纯 ESM。✅ 如引入 IndexedDB，推荐 |
| **exiftool**（系统工具） | Perl 库，通过 `subprocess` 调用。 ✅ 已普遍安装在 Linux 发行版中。作为外部依赖文档化即可 |

### 4.3 自建 vs 采购/引入

| 能力 | 自建 | 采购/引入 | 理由 |
|------|------|----------|------|
| **组件系统** | ✅ 自建 | ❌ | ~80 行基类代码，自建比引入外部库的学习成本和适配成本都低 |
| **事件总线** | ✅ 自建 | ❌ | 需求简单到不需要第三方（发布-订阅 ≈ 50 行） |
| **文件上传管道** | ✅ 自建 | ❌ | 后端已有完整 S3/AV/嗅探管线，前端只加进度/预览——与现有管道紧密耦合，不适合外部替换 |
| **缩略图/EXIF 剥离** | ✅ 自建 | ⚠️ 使用系统工具 | 使用 exiftool + ImageMagick 作为外部进程调用，自己写编排代码。不引入商业 API |
| **IndexedDB 封装** | ✅ 可选自建 | ✅ idb 库 | idb 库仅 1.5KB，比自建更可靠（处理了所有 IndexedDB 边缘情况——版本升级、存储配额超限、隐私模式无权限） |

---

## 5. 实施路线图

### 5.1 优先级排序与阶段划分

```
Phase 0 (Week 1-2) — 架构基础设施
├── Component.js 基类
├── EventBus (bus.js)
├── api.js 重构 (XHR 进度 + 拦截器)
└── PanelManager 雏形

Phase 1 (Week 3-4) — P1 功能并行
├── 方向一 L1：上传进度条 + 拖拽 + 预览 + 前端校验
├── 方向二 L1：设置面板 (Profile + 安全 + 通知 + 会话)
└── 方向三 L1：EntityPicker 组件 + @提及替换 + 添加成员

Phase 2 (Week 5-6) — P2 功能
├── 方向四 L1：书签/翻译/链接/消息动作扩展
├── 方向五 L1：置顶面板 + 引用回复基础 + 消息转发
└── 方向三 L2：转发 + 斜杠命令 + 最近使用排序

Phase 3 (Week 7-8) — 优化与 L2/L3
├── 方向一 L2：上传队列管理 + 存储用量
├── 方向二 L2：扩展设置 (消息模板/状态/个性化/存储)
├── 方向五 L2：置顶自动过期
├── 方向一 L3：缩略图管线 + EXIF 剥离
└── 方向四 L2：文本引用回复 + 从消息创建任务
```

### 5.2 关键里程碑

| 里程碑 | 时间 | 交付物 | 验证标准 |
|--------|------|--------|---------|
| M0: 架构基座 | Week 2 | Component.js + bus.js + PanelManager 可用 + api.js 重构不影响现有调用 | 所有现有 smoke test 通过；新增 3 个单元测试覆盖基类 |
| M1: 核心体验修复 | Week 4 | 上传拖拽有区域 + 进度条 + 预览；设置面板覆盖 4 组功能；@mention 重写为 EntityPicker | 拖拽 5MB 图片显示进度条/预览后可发送；改头像/密码/2FA 走 UI |
| M2: 消息操作增强 | Week 6 | 每条消息可收藏/翻译/复制链接；频道有置顶面板；可转发消息到其他频道 | 收藏 10 条消息后面板可浏览/跳转；日文消息翻译显示完整；转发消息到目标频道可见 |
| M3: 产品化完成 | Week 8 | 上传队列/存储用量/扩展设置/引用回复/任务创建/EXIF 保护 | 全 smoke 通过；queued upload 3 个大文件可取消单个 |

### 5.3 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| **Component.js 设计过于通用/抽象** | 中 | 组件滥用导致"组件地狱" | 约束：只有 **有状态/有生命周期** 的 UI 单元才做成 Component。纯渲染函数保持为函数。在组件规范文档中明确触发条件 |
| **api.js 重构导致未发现的回归** | 高 | 上传/登录/搜索等功能性错误 | 方法：API 层保持 100% 签名兼容；重构期间运行完整 smoke 测试；先在 staging 部署 24h 再上线 |
| **XHR vs fetch 适配层增加代码复杂度** | 中 | api.js 内部有两个适配器，维护成本高 | 策略：`fetch` 作为默认适配器；`XMLHttpRequest` 只在上传时使用。上传完成后逻辑与 fetch 路径汇合（统一 JSON 解析/错误处理） |
| **设置面板修改后其他模块状态不一致** | 中 | 用户修改显示名后，消息列表仍显示旧名 | 机制：设置面板保存成功后，`bus.emit('profile:updated', newProfile)`，`render.js` 监听后重新渲染涉及用户名的区域。所有模块通过总线监听，不直接依赖设置面板 |
| **10+ 组件同时引入导致初始加载体积膨胀** | 低 | 初始 JS 从 35KB 增长到 60KB+ | 缓解：组件延迟加载 `import('./Component.js')`。仅 `EntityPicker` 在首页加载（@mention 是高频），书签/置顶面板在用户首次打开时加载 |
| **团队学习曲线** | 低 | 新抽象层（组件/事件总线/面板管理）需要时间适应 | 方法：基类代码 + 1 个参考实现（EntityPicker）作为文档。禁止"先把组件做完再学"——用 EntityPicker 边做边学 |
| **事件总线过度使用导致调试困难** | 中 | 不知道某个事件从哪发出、被谁处理 | 策略：事件名命名约定 `<domain>:<action>[:<detail>]`（如 `msg:pinned:added`、`upload:progress`）；dev 模式下 bus 打印所有 emit 调用栈 |
| **方向三 EntityPicker 设计过于灵活** | 中 | 为"支持所有场景"而过度参数化，不如"2-3 个明确场景" | 原则：YAGNI。EntityPicker v1 只支持 4 种 mode（user/channel/room/group），不支持自定义 `fetch`。覆盖 @mention + 添加成员 + 转发 就够。v2 再加通用化 |

### 5.4 不建议做的

在讨论 5 个方向的实施过程中，有几个 "看似合理但应当避免" 的架构决策：

1. **❌ 用 iframe 隔离设置面板**：设置面板是 SPA 的一部分，不应引入 iframe（跨域通信、样式不一致、SEO 无意义）
2. **❌ 引入 SPA 路由库（vaadin-router / navigo）**：当前 hash-based 路由工作良好。新面板只是"显示/隐藏"不涉及 URL 变更，不需要路由
3. **❌ 自制虚拟滚动库**：如果消息列表性能成为问题（预期 >10,000 条消息不渲染），引入 `Clusterize.js` 或 `TableView` 等轻量库比自建更可靠。但当前阶段（<500 条消息）不需要
4. **❌ 将设置面板做成全屏独立页面**：设置面板作为侧面板而非独立页，保持"不离开当前上下文"的 IM 体验。Slack/Lark 都证明了侧面板优于全屏设置页
5. **❌ 一次实现全部 5 个方向再发布**：增量交付。每个阶段结束都应有"可向用户展示的功能增量"，而不是 8 周后一起发布

---

## 总结：架构师建议优先序列

从架构支持而不是功能优先级的角度，我建议的投入顺序是：

```
Week 1-2:  组件系统 + 事件总线 + API 重构 ──→ 让后续功能有架构土壤
Week 3-4:  上传进度 + 设置面板 + EntityPicker ──→ 每个都是架构验证者
Week 5-6:  消息操作扩展 + 置顶 + 面板管理 ──→ 不再受架构限制
Week 7-8:  所有 L2/L3 优化 ──→ 从"能用"到"好用"
```

**关键决策总结**：

| 决策 | 选择 | 原因 |
|------|------|------|
| 前端框架？ | 不引入，自建 Component 基类 | 当前复杂度不需要框架开销 |
| 上传进度用 fetch 还是 XHR？ | XHR | `fetch` 无原生上传进度支持 |
| 面板管理模式？ | 单一面板（一次只开一个） | 简单可靠，避免 z-index 争斗 |
| 事件总线还是直接调用？ | 事件总线 | 跨模块解耦的必须工具 |
| 缓存做吗？ | P2 优先（缓存先不做） | 除非有明确性能瓶颈测量数据 |

以上分析覆盖了 5 个方向的架构影响、所需的架构基础设施、技术选型决策和分阶段实施路线。关键信息已对齐 AGENTS.md §4 的工程规则（无框架约束、逐步迁移、向后兼容）。
