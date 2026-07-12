# 架构师分析：Aero IM 前端覆盖缺口与系统演进方向

## 1. 架构评估

### 1.1 当前架构的核心优势

Aero IM 选择了**后端优先、API 驱动的架构**，这在项目早期阶段是正确的决策。后端 20+ 管理模块全部有 REST endpoint 和完整的鉴权链路，这意味着：

- **API 契约已固化**。前端可以逐模块并行开发，不存在后端阻塞前端的依赖倒挂。
- **多租户安全边界已夯实**。每个管理端点都有 `assert_room_access` 或 `member_role` 守卫，前端的缺失不会导致安全漏洞——只是功能不可见。
- **迁移风险极低**。前端集成不需要修改后端，只需消费既有 API。

这种「后端先于前端」的策略，在快速原型期有效降低了单次迭代的耦合面。但**当项目进入用户可触达阶段**，这种不平衡就变成了显性的架构债务。

### 1.2 架构债务

我将当前架构的债务分为三个层级：

**L1 — 架构层面（需要重构解决的问题）**

| 债务 | 表现 | 严重程度 |
|------|------|----------|
| **前端无路由系统** | `index.html` 单页内所有功能通过 JS 条件显示/隐藏实现，无 URL 路由、无历史管理、无懒加载 | 高 |
| **UI 与 API 的映射无治理** | 20+ 后端模块存在 `routes()`，但前端只有零散的 `fetch()` 调用，无统一的数据获取层或缓存策略 | 中 |
| **缺乏组件复用机制** | 模态框、表单、表格、分页等常见 UI 模式无抽象层，每个新功能的实现成本 = 从头构建 DOM | 高 |

**L2 — 实现层面（可以通过渐进改进解决的问题）**

| 债务 | 表现 | 严重程度 |
|------|------|----------|
| **ES2020 零依赖策略已到上限** | 零依赖在原型期降低复杂度，但构建管理控制台（表格分页、多级表单、搜索高亮）时，手写 DOM 的维护成本呈超线性增长 | 中 |
| **WebSocket 帧处理无类型化** | `handleIncomingMessage` 中 switch-case 遍布所有事件类型，没有按事件域（房间事件/流事件/通话事件）做分发隔离 | 中 |
| **缺少前端状态管理** | 用户身份、当前频道、线程上下文等全局状态散落在 DOM 属性和全局变量中 | 低 |

**L3 — 可观测性层面**

| 债务 | 表现 | 严重程度 |
|------|------|----------|
| **前端无错误边界** | API 调用失败（如 403/429）没有统一的 toast 或 retry 逻辑 | 中 |
| **无前端性能指标** | 没有 `web-vitals` 或自定义指标的采集 | 低 |

### 1.3 关键设计决策的合理性评估

**决策一：「单页 index.html + app.js」**
- **合理之处**：零构建工具链，无 bundle 步骤，`<script type="module">` 即可开发。对 5-10 个交互式页面的 SPA 场景，这是可行的。
- **已到边界**：当页面数量超过 15（管理控制台 = 15+ 子页面），单文件维护成本爆炸。URL 路由缺失使得用户无法深层链接到某个设置页。
- **结论**：对于管理控制台这个方向，**必须引入前端路由**。对于线程面板和搜索体验，可以在现有单页模式内完成。

**决策二：「设置页面通过 JS 动态隐藏/显示」**
- **合理之处**：避免服务器端渲染或客户端路由库。
- **已到边界**：功能隔离靠 `display:none`，意味着所有 20+ 模块的 JS 代码在页面加载时全部下载、全部注册事件监听。这在 20 个模块下尚可接受，但当扩展至 50+ 时，首屏 JS 体积会膨胀到不可控。
- **结论**：管理控制台应使用**懒加载模式（按需挂载事件）**。

**决策三：「零外部依赖」**
- **合理之处**：无运行时依赖意味着零供应链风险、零版本冲突、零 bundle 配置。
- **边界条件**：当需要实现表格排序/分页、表单验证、富文本编辑、搜索高亮等「车轮」问题时，零依赖要么导致实现质量低下，要么开发时间翻倍。
- **结论**：这是一个**值得在管理控制台方向重新评估的决策**。不一定要引入框架，但应该允许引入**小型、targeted 的库**（如 marked.js 做渲染、DOMPurify 做 XSS 防护）。

---

## 2. 扩展方向

### 方向 A：前端路由与懒加载架构（基础设施层）

**为什么需要**

当前的单页模式在管理控制台的场景下会出现两个根本性问题：
1. **用户无法深层链接**到 `/settings/security/sso`——每次回访都需要导航三次点击。
2. **全局 DOM 一次性挂载**导致事件监听的规模随功能数线性增长，最终产生可感知的交互延迟。

实现管理控制台 15+ 子页面**而不引入前端路由**，技术上可行但会产出不利于维护的架构。路由系统是管理控制台的基础设施，不是可选项。

**核心挑战**

- 零依赖约束下的路由系统需手写 `window.addEventListener('hashchange', ...)` + URL 解析。这本身是成熟的模式，但需要配合**异步模块加载**——即只在进入某个设置页面时才执行其 JS 逻辑。
- 鉴权重定向：未登录用户直接访问 `/settings/security` 需要跳转到登录页。当前登录态检查在 `app.js` 中，需要提取为可跨路由共享的守卫。
- 路由层级的设计：`/workspaces/:id/settings/security/sso`——浅层路由（`/settings` 分出 5 个标签页）已经够用，还是需要支持嵌套路由？

**架构变更建议**

引入一个最小化的路由系统：

```
web/
  router.js          # 简化的 hash 路由，支持参数提取
  guards/
    auth.js          # 登录守卫（复用现有 checkAuth 逻辑）
    workspace.js     # 工作区权限守卫
  pages/
    settings/        # 管理控制台各页面（懒加载）
    threads/         # 线程面板
    search/          # 搜索增强
```

**关键约束**：路由系统不能破坏现有非路由页面的工作方式。现有 `app.js` 的 `handleIncomingMessage`、频道切换等功能应该**无感继续运行**，路由只控制「右侧主内容区渲染什么」。

**对现有系统的影响**

- **低**。仅新增 `router.js` 和页面拆分，不修改现有文件。现有 `index.html` 的 `main-content` 区域继续作为路由内容的挂载点。
- 需要定义一个约定：「路由页面」在挂载时调用 `init()`，卸载时调用 `destroy()`（清理事件监听）。

---

### 方向 B：管理控制台的组件化抽象层（实现层）

**为什么需要**

管理控制台的 15+ 子页面存在大量重复 UI 模式：

```
表单（SSO 配置、频道保留策略、Webhook 配置）    → 输入框 + 保存按钮 + 验证
表格（用户列表、Sessions、Webhook 列表）          → 分页 + 排序 + 行操作
开关（频道归档、TOTP 强制、功能开关）             → 开关 + 描述
空状态 / 错误状态 / 加载状态                      → 统一的占位 UI
```

如果为每个子页面都手写这些模式，15 页 × 3 个模式 = 45 次重复实现，**成本是线性的**。建立 5-8 个组件函数，成本是 O(1) 的初始化投入 + 后续 O(1) 的复用。

**核心挑战**

- 在不引入 JSX/模板引擎的前提下实现组件化。策略是**纯函数模板**：`function Table({ columns, rows, onAction }) { return html`<table>...</table>`; }`，利用 ES6 模板字面量组装 DOM 字符串，再 `innerHTML` 挂载。
- 状态更新：属性变更后如何高效更新 DOM？最简单方案是 **`rerender()` 全量替换子节点**——这适用于管理控制台的交互频率（不涉及毫秒级动画或实时列表，后者走 WebSocket）。
- WebSocket 驱动的列表（如在线会话、推送 token）需要增量更新。现有代码已用 `handleIncomingMessage` 做增量，组件需要暴露 `appendRow(data)` / `removeRow(id)` 方法。

**架构变更建议**

```
web/
  components/
    Table.js         # 通用表格（列定义、行渲染、分页、排序）
    Form.js          # 通用表单（字段定义、验证、提交状态）
    Switch.js        # 开关组件（on/off 状态 + 持久化）
    Toast.js         # 通知弹窗（成功/错误/警告）
    Modal.js         # 通用模态框（标题、内容、操作按钮）
    Tabs.js          # 标签页导航（选项卡切换）
    Badge.js         # 状态标签（启用/禁用/待处理等）
```

**对现有系统的影响**

- **低**。组件是新文件，不修改现有代码。现有 `modals.js` 的 `showModal` 可以保留，`Modal.js` 作为新的统一接口，逐步替代手写模态框。
- `components/` 目录的组件**不感知业务逻辑**——它们接收数据和回调，不直接调用 API 或访问 WebSocket。

---

### 方向 C：WebSocket 消息路由与状态管理层（基础设施层）

**为什么需要**

当前 `handleIncomingMessage` 是一个约 400-600 行的 `switch-case` 块（估算），处理所有事件类型。当线程面板加入后，需要新增「收到回复 → 追加到线程面板」的分支。当管理控制台加入实时数据（如用户在线状态变更）后，又需要新的分支。

**这个函数的规模会随功能数量线性增长，最终达到不可维护的阈值。**

将消息路由从 `app.js` 中解耦为独立的**消息总线**，允许各模块独立订阅感兴趣的事件类型，是控制复杂度的关键架构决策。

**核心挑战**

- 订阅者的生命周期管理：当用户切换到其他页面时，某个模块的订阅者需要被**自动销毁**，否则会出现「设置页面的响应函数尝试更新已被移除的 DOM」的错误。
- 事件类型的安全保障：当前 `message.data` 的 `type` 字段是字符串枚举，无编译期检查。在 JS 中无法获得 Rust 的 exhaustiveness checking，但可以通过**事件注册表**来集中定义所有支持的事件类型，避免拼写错误。
- 性能：每个 WebSocket 消息需要遍历所有订阅者。如果订阅者数量 < 50（每个事件类型 3-5 个监听器），这不是问题。但如果每个模块都订阅 `*`（所有事件），则 O(N) 遍历会影响高频弹幕场景。

**架构变更建议**

```
web/
  bus/
    ws-bus.js        # WebSocket 事件总线（订阅/取消/发布）
    events.js        # 事件类型常量（与后端 ServerFrame 的 kind 对齐）
  guards/
    lifecycle.js     # 页面离开时自动取消订阅的机制
```

`ws-bus.js` 的接口设计：

```
wsBus.subscribe('message', handler)     → returns subscriptionId
wsBus.subscribe('stream_chat', handler) → returns subscriptionId
wsBus.unsubscribe(subscriptionId)       → 按 ID 取消
wsBus.subscribeAll(handler)             → 用于日志/监控
```

**对现有系统的影响**

- **中**。需要将 `app.js` 中的 `socket.onmessage` 处理函数从直接逻辑改为调用 `wsBus.dispatch(event)`。这是对核心路径的有侵入重构，**应该与线程面板的改动一起做**（线程面板是第一个需要在 `handleIncomingMessage` 之外订阅事件的模块），避免两次修改同一代码段。
- 过渡策略：先 `wsBus` 只包装当前消息分发逻辑，逐步将 switch-case 分支迁移到各自的独立订阅者。

---

### 方向 D：API 客户端层与后端响应增强（数据层）

**为什么需要**

当前前端调用 API 的方式分散在各个 `.js` 文件中，每个调用点有自己的错误处理逻辑（或没有）。引入统一 API 客户端的好处：

1. **统一错误处理**：403 → 跳登录；429 → 显示重试倒计时；500 → toast 错误。
2. **请求/响应拦截**：自动注入 `Authorization` header、记录 API 耗时到指标。
3. **缓存与去重**：对 `GET /api/workspaces/:id/members` 这类跨页面复用的数据，避免重复请求。

同时，搜索结果体验的改进**需要后端配合增加一个字段**——`snippet`。这是已验证的需求（来自原分析文档的验证结果）。后端改动虽然只有一行，但需要涉及 `SearchResult` 的序列化、`ts_headline()` 的 SQL 调用、以及前端展示层。

**核心挑战**

- 缓存失效的时机：什么情况下需要重新拉取成员列表？答案是：WebSocket 通知 `workspace_member_joined/left` 时。这又回到了方向 C——需要事件总线来驱动数据层的缓存失效。
- 搜索结果的高亮渲染：「搜索结果片段」需要对后端返回的 `snippet` 进行 HTML 安全渲染——`ts_headline()` 返回的 `...<b>关键词</b>...` 片段包含 HTML 标签，前端需要 `DOMPurify` 或白名单过滤后渲染。

**架构变更建议**

```
web/
  api/
    client.js        # 统一的 fetch 封装（错误处理、重试、拦截器）
    workspaces.js    # 工作区 API 方法
    rooms.js         # 频道 API 方法  
    search.js        # 搜索 API 调用（增强现有 search.js）
    users.js         # 用户 API 方法
```

**后端改动（单点）**：

`server/src/search.rs`（或其他搜索响应结构的定义文件）中，`SearchResult` 结构体需要增加一个 `snippet: Option<String>` 字段，SQL 查询中调用 `ts_headline(body, query, 'StartSel=<mark>, StopSel=</mark>, MaxWords=30, MinWords=15')` 生成高亮片段。

**对现有系统的影响**

- **中低**。API 客户端的新建不影响现有代码。增量替换：新模块（管理控制台、线程面板）使用新客户端，旧文件逐步迁移。
- 后端的 `snippet` 字段是**向后兼容的**：旧客户端不读取该字段，新客户端使用。前端代码中现有搜索结果渲染的部分（`search.js` 的纯文本显示）可以原地增强，不需要重构。

---

### 方向 E：前端测试与 CI 集成（质量层）

**为什么需要**

原分析文档提到「单测无法覆盖，需要手动 WS 交互验证」。当前 CI 只检查 Rust 编译、clippy、Rust 测试——**前端没有任何测试**。这在一个有 WebSocket 实时交互、多页面路由、复杂表单逻辑的应用中，是不可持续的。

具体场景：
- 线程面板：在收到 `reply_to` 消息时追加到面板的逻辑，无法通过单元测试验证
- 管理控制台表单：提交 SSO 配置 → 验证 OIDC URL 格式 → 保存 → 显示成功 toast 的全链路
- 搜索高亮：`ts_headline()` 返回的 HTML 片段在 DOM 中渲染是否正确

**核心挑战**

- **纯前端集成测试**在没有后端的情况下很难做。策略是：单元测试用 mock WebSocket、mock fetch。端到端用 Playwright 连本地 server。
- **WS 模拟的复杂性**：WebSocket 是双向的，模拟需要维护一个虚拟的消息队列，既能注入事件也能捕获发送的事件。但复杂度可控——模拟 WebSocket 是前端测试的成熟领域。
- 测试维护成本：如果 UI 频繁变动，测试会成为负担。建议**只覆盖核心交互路径**（搜索、线程面板打开/关闭、管理控制台表单提交），不追求 100% 覆盖。

**架构变更建议**

```
web/tests/
  unit/
    components/     # 组件函数的单元测试
    bus/            # ws-bus 的发布/订阅测试
    router/         # 路由解析测试
  integration/
    search.test.js  # 搜索交互测试（mock WebSocket + mock API）
    thread.test.js  # 线程面板测试
    settings.test.js# 管理控制台表单测试
  e2e/              # 可选，Playwright 测试
    smoke.test.js
```

测试框架选型：Node.js + **node:test**（Node 20+ 内置的 test runner，零依赖）或 Vitest（Compatible with ES modules, zero-config for browser-like tests）。我倾向于 `node:test` 保持零依赖策略——可以用 `globalThis.fetch` mock + 手写 WebSocket mock，不需要 jsdom。

**对现有系统的影响**

- **低**。测试文件独立于源码目录，不修改现有逻辑。CI 中增加 `node web/tests/*.test.js` 的执行步骤。
- 关键点：测试的引入**不应该**强迫源码改为 CommonJS 或添加 export——测试可以直接 `import` ES module 源码。

---

## 3. 接口设计建议

### 3.1 核心接口设计原则

基于五个方向的评估，我建议引入以下接口抽象层：

**原则一：表现层与数据层分离**

每个页面/组件应该只有一个数据源（single source of truth），并且这个数据源不来自 DOM。

```
❌ 当前做法：从 DOM 读取状态
const currentRoom = document.querySelector('#room-name').textContent;

✅ 建议做法：从状态层读取
const currentRoom = store.get('currentRoom');
```

**原则二：组件接口一致性**

每个组件应该暴露标准化的生命周期接口：

```
ComponentLifecycle {
  init(container: HTMLElement, props: Object): void
  destroy(): void  // 清理事件监听，取消 WS 订阅
  update(props: Object): void  // 属性变更时重新渲染
}
```

这不是框架，只是一个 3 方法的约定。所有管理控制台组件都应该遵循。

**原则三：API 调用的声明式错误映射**

``` 
// 当前做法 — 在调用点处理错误
fetch(url).then(r => { if (!r.ok) showError(r.status); })

// 建议做法 — 声明式配置
api.get(url, {
  onError: {
    403: () => redirectToLogin(),
    429: () => showCooldown(),
    500: () => showToast('Server error'),
  }
})
```

### 3.2 是否需要新的抽象层

- **路由抽象层**：需要。15+ 管理控制台子页面需要 `hashchange` 路由。
- **组件抽象层**：需要。但形式是**工具函数集合（如 React Hooks 的思路，无 JSX）**，不是框架。
- **状态管理抽象层**：暂不需要。全局状态当前只有 3-5 项（currentUser、currentWorkspace、currentRoom、threadContext）。一个简单的 `Store` 类（基于 `Proxy` 或 `EventTarget` 实现响应式）即可满足。
- **WebSocket 总线抽象层**：需要。这是解耦 `handleIncomingMessage` 巨函数的唯一途径。

### 3.3 向后兼容性

所有新抽象层应该满足：

1. **可选采用**：现有 `app.js`、`search.js`、`modals.js` 可以继续按原样工作。新模块（管理控制台、线程面板）使用新抽象。
2. **不修改现有文件**：新抽象层全部是新文件。唯一的侵入点是 `index.html` 的 `<script>` 加载顺序——在 `app.js` 之前加载 `router.js` 和 `ws-bus.js`。
3. **WebSocket 总线兼容**：现有的 `socket.onmessage` 函数接收事件后，应该先通过 `wsBus.dispatch(event)` 分发给新订阅者，再执行原有的 switch-case 逻辑。这样新老模块并存。

---

## 4. 技术选型

### 4.1 是否需要引入新的技术栈

**不建议引入 React/Vue/Svelte 等完整框架。** 理由如下：

| 框架方案 | 收益 | 成本 | 结论 |
|---------|------|------|------|
| React | 成熟的组件生态，JSX 开发体验好 | 需引入 bundler（Vite/Webpack），与现有 ES2020 模块冲突，增加 ~20KB gzip 运行时 | ❌ 成本 > 收益 |
| Vue 3 | 渐进式，可与现有 HTML 共存 | 同样需要 bundler，模板编译步骤 | ❌ 成本 > 收益 |
| Svelte | 编译时框架，无运行时，零依赖 | 需要 bundler + 编译步骤，与现有构建流程完全不同的范式 | ❌ 收益最高，但破坏性最大 |
| **纯 ES2020 组件** | 无新增依赖，与现有架构一致 | 需要手写模板，缺乏生态组件 | ✅ 最适合当前阶段 |
| **极小依赖（marked + DOMPurify）** | 解决两个痛点：Markdown 渲染和 XSS 防护 | 两个小库，合计 ~10KB gzip | ✅ 可接受 |

**推荐方案**：纯 ES2020 + 路由系统 + 组件函数 + 可选引入 2-3 个小库（marked、DOMPurify）。

**为什么不是 HTMX 或 Alpine.js？** HTMX 适合 SSR 应用增强，不适合 WebSocket 密集的实时 SPA。Alpine.js 虽然轻量但在组件复用和生命周期管理上不如手写函数灵活。

### 4.2 第三方依赖的评估标准

如果决定引入外部库（哪怕是 marked、DOMPurify 这种小库），建议遵循以下评估 checklist：

1. **无构建时依赖**：纯 ESM，import 即可用。不需要 bundler、不需要 CSS-in-JS 编译步骤。
2. **体积上限**：gzip 后 ≤ 15KB。一个库应该解决一个问题，而不是全家桶。
3. **无 DOM 污染**：不修改全局原型，不对 `Element.prototype` 做 monkey-patch。
4. **许可证兼容**：MIT / Apache 2.0 / BSD。
5. **类型定义友好**：虽然不是强制，但 TypeScript 用户友好的库通常 API 设计也清晰。

**对 marked 和 DOMPurify 的评估**：

| 库 | 体积 (gzip) | ESM | 无 DOM 污染 | 许可证 |
|----|------------|-----|-------------|--------|
| marked | ~10KB | ✅ | ✅ | MIT |
| DOMPurify | ~6KB | ✅ | ✅ | Apache 2.0 |

两个都通过评估标准。**建议引入的时机**：在开发搜索高亮（需要安全渲染 `ts_headline` 输出的 `<mark>` 片段）或管理控制台的 Markdown 预览（如 Webhook description 字段）时引入。

### 4.3 自建 vs 采购

对于 5 个方向涉及的组件类型：

| 组件 | 建议 | 理由 |
|------|------|------|
| **路由系统** | 自建 | 需求简单（hash 路由 + 参数提取），50 行函数即可实现，不需要引入外部路由库 |
| **表格排序/分页** | 自建 | 管理控制台的数据量级很小（工作区成员 ≤ 1000，不会达到虚拟滚动需求），手写简单排序和分页即可 |
| **Markdown 渲染** | 引入 marked | Markdown 解析是算法密集型，自建不可靠。marked 是 battle-tested 的 10KB 库 |
| **XSS 防护** | 引入 DOMPurify | 安全领域自建是危险的。DOMPurify 团队专门研究 HTML 净化，不能替代 |
| **搜索高亮** | 自建 | 简单文本替换/正则高亮，不需要库 |
| **表单验证** | 自建 | 管理控制台表单验证规则较简单（必填、邮箱格式、URL 格式），手写最灵活 |
| **WebSocket mock** | 自建 | 用于测试，20-30 行的 `EventTarget` 子类即可 mock |

**结论**：除了 marked 和 DOMPurify，其余全部自建。这符合项目的「最小外部依赖」原则。

---

## 5. 实施路线图

### 5.1 优先级排序

我支持原分析文档的排序调整建议，但做更深的分层：

```
P0（阻塞性基础设施，必须先完成）
  路由系统 ├── router.js、页面目录结构
  API 客户端 ├── client.js、错误处理
  WS 总线 ├── ws-bus.js、事件注册表

P1（高频用户触点，商业价值高）
  通知偏好 ├── 设置页面（单页、不改变布局）
  管理控制台 ├── 15+ 子页面，分期交付
  线程面板 ├── 右栏替换、高频交互

P2（独立可完成，优先级低于 P1）
  搜索体验 ├── 搜索高亮、操作符 UI、保存的搜索
  Bot 管理 ├── 用户基数小，平台生态属性
  测试体系 ├── 不阻塞上线，但保证质量

P3（非紧急，基础设施加固）
  状态管理 ├── Store 抽象
  前端监控 ├── 性能指标收集
```

### 5.2 阶段划分和里程碑

#### 阶段 0 — 基础设施准备（2 周）

目标：为所有后续方向建立可复用的抽象层。

| 周 | 交付物 | 验收标准 |
|----|--------|----------|
| W1 | `router.js` + 页面目录结构 | 支持 `#settings/security/sso` 等路由解析；页面切换时销毁/初始化生命周期 |
| W1 | `api/client.js` + 错误处理拦截 | 所有管理控制台页面的 API 调用使用新客户端；统一错误 map |
| W2 | `bus/ws-bus.js` + 事件注册表 | `app.js` 中的 `socket.onmessage` 将事件分发给 ws-bus 后再执行 switch-case；线程面板可独立订阅 `reply_to` 事件 |
| W2 | `components/` 前 4 个组件 | `Table.js`、`Form.js`、`Switch.js`、`Toast.js` 通过 demo 页面验证可用性 |

**门控条件**：阶段 0 完成前，不得开始 P1 方向的开发。

#### 阶段 1 — 通知偏好 + 管理控制台 Phase A（3 周）

目标：交付 P1 方向中用户感知最强、商业价值最高的功能。

| 周 | 交付物 | 具体内容 |
|----|--------|----------|
| W3 | 通知偏好设置页 | 5 个 tabs：频道静音、关键词提醒、DND 定时、推送 token 管理、电子邮件频率。单页不改变布局 |
| W3 | 管理控制台 Phase A（前 5 页） | 安全设置（SSO/OIDC/SAML）、IP 白名单、频道保留策略、用户群组、Webhook 管理 |
| W4 | 管理控制台 Phase A 剩余 5 页 | 频道分区、2FA 强制、Session 管理、邀请管理、公告管理 |
| W5 | 管理控制台 Phase A 最后 5 页 | 使用报告、Analytics、用户停用、用户举报、消息举报 |
| W5 | 验收 & Bug 修复 | 全 15 页走通 CRUD 流程；权限守卫验证（普通用户看不到管理控制台入口） |

**门控条件**：管理控制台的 15 页都只是「可读取」不算完成——必须**可写入**。每个表单提交后，用 `GET` API 验证持久化生效。

#### 阶段 2 — 线程面板 + 搜索体验（2 周）

目标：交付 P1.5 和 P2 方向。

| 周 | 交付物 | 具体内容 |
|----|--------|----------|
| W6 | 线程面板 | 右栏 `<aside id="thread-panel">`；`handleIncomingMessage` 中追加全局的线程状态追踪；回复消息时自动打开面板；mute/unmute 线程 |
| W6 | 搜索高亮 | 后端 `SearchResult.snippet` 字段 + SQL `ts_headline()`；前端搜索结果渲染 `<mark>` 高亮片段 |
| W7 | 搜索操作符 UI | `from:`/`in:`/`before:` 的输入提示；搜索模式选择器（FTS/Vector/Hybrid）；保存的搜索管理 |
| W7 | 修复 & 打磨 | 线程面板 + 搜索的集成测试 |

#### 阶段 3 — Bot 管理 + 测试体系（2 周）

目标：交付 P2 剩余功能，建立质量保障体系。

| 周 | 交付物 | 具体内容 |
|----|--------|----------|
| W8 | Bot 管理控制台 | Bot 列表、Webhook 日志、Bot 创建/编辑/启用/禁用、Webhook 重试 |
| W8 | 前端单元测试 | 组件函数测试、ws-bus 测试、路由测试 |
| W9 | 前端集成测试 | 核心交互路径（搜索、线程面板、管理控制台表单）的 Playwright 端到端测试 |
| W9 | CI 集成 | `node web/tests/*` 加入 CI；`cargo check` + `clippy` + `test` + `web-test` 全绿门控 |

### 5.3 风险点和缓解策略

#### 风险 1：管理控制台 15 页并行开发的质量不一

- **概率**：高
- **影响**：中
- **缓解**：建立模板化开发流程——每个设置页遵循固定三部曲：① 在 `settings/` 下新建 `modulename.js`，使用 `Form.js`/`Table.js` 等组件 ② 在 `router.js` 注册路由 ③ 在新标签页打开 `/settings#module-name` 手动测试 CRUD。模板可以是一个 `_template.js` 文件 + 说明注释。
- **后备方案**：如果某一页的复杂度超出预期（如 SSO 配置的连通性测试），先交付**只读视图**（用户可以查看当前配置），写操作延期到下一 sprint。

#### 风险 2：线程面板的布局改动破坏现有 UI

- **概率**：中
- **影响**：高
- **缓解**：使用 CSS `display: none` / `display: flex` 控制线程面板显示，不改变现有 DOM 树结构。现有 `main-content` 和 `sidebar` 的 CSS 选择器不受影响。线程面板作为一个独立的 `<aside>`，在 `index.html` 底部插入。
- **测试**：手动测试以下场景：手机端（宽度 < 768px）打开线程面板 → 应全屏覆盖；桌面端打开/关闭 → 主消息列表宽度应自适应；切换频道 → 线程面板应关闭或清空。

#### 风险 3：零依赖策略导致组件开发效率过低

- **概率**：低～中
- **影响**：中
- **缓解**：在阶段 0 的组件抽象层投入充分时间。如果 `Table.js` 的分页/排序逻辑占了 200 行以上，且 Bug 不断——**允许重新评估引入微型框架**。备选方案是 Lit（Google 的 Web Components 库，~5KB gzip，无 bundler 需求）。
- **决策点**：阶段 0 结束时，评估 `components/` 的代码质量和 Bug 率。

#### 风险 4：WebSocket 总线重构导致消息丢失

- **概率**：中
- **影响**：高
- **缓解**：ws-bus 的迁移分两步走。第一步（阶段 0），在现有 `socket.onmessage` 顶部插入 `wsBus.dispatch(event)`，**不删除任何原有逻辑**，确保消息不被吞。第二步（阶段 2），逐步将线程面板等新模块的订阅从 switch-case 移到独立订阅者。**旧模块的 switch-case 分支永远保留**，直到所有模块迁移完毕。
- **回滚方案**：如果 ws-bus 有 Bug，删除 `wsBus.dispatch(event)` 一行即可回退到原有逻辑。

#### 风险 5：管理控制台 API 的鉴权测试遗漏

- **概率**：高
- **影响**：高
- **缓解**：在管理控制台的每个页面中添加**显式的权限状态指示**——如果 API 返回 403，页面应显示「你没有权限访问此设置」而非空白页或无限 loading。每个设置页的验收条件包含「用非管理员 token 请求应看到权限错误提示」。
- **自动化验证**：写一个 quick 脚本，对每个管理控制台 API 路径发 `curl -H "Authorization: Bearer $USER_TOKEN"` 和 `curl -H "Authorization: Bearer $ADMIN_TOKEN"`，验证前者 403 后者 200。纳入 CI。

---

## 6. 最终架构图

以下是五个方向全部实现后的前端架构分层图：

```
web/
├── index.html                   # 骨架 HTML（header + sidebar + main + aside#thread-panel）
├── router.js                    # [新增] hash 路由 + 页面生命周期管理
├── app.js                       # [强化] 精简后的核心逻辑（频道切换、消息列表等）
│
├── bus/
│   ├── ws-bus.js                # [新增] WebSocket 事件总线
│   └── events.js                # [新增] 事件类型常量
│
├── api/
│   ├── client.js                # [新增] 统一 fetch 封装（错误处理 + 拦截器）
│   ├── workspaces.js            # [新增] 工作区 API
│   ├── rooms.js                 # [新增] 频道 API
│   ├── search.js                # [增强] 增强后的搜索 API
│   └── users.js                 # [新增] 用户 API
│
├── components/
│   ├── Table.js                 # [新增] 通用表格
│   ├── Form.js                  # [新增] 通用表单
│   ├── Switch.js                # [新增] 开关
│   ├── Toast.js                 # [新增] 提示弹窗
│   ├── Modal.js                 # [新增] 模态框
│   ├── Tabs.js                  # [新增] 标签页导航
│   └── Badge.js                 # [新增] 状态标签
│
├── pages/
│   ├── thread-panel.js          # [新增] 线程面板模块
│   ├── search.js                # [增强] 搜索页面（操作符 + 高亮 + 保存）
│   └── settings/               # [新增] 管理控制台
│       ├── index.js             # 设置页主入口
│       ├── security.js          # SSO/OIDC/SAML
│       ├── ip-allowlist.js      # IP 白名单
│       ├── channel-retention.js # 频道保留策略
│       ├── user-groups.js       # 用户群组
│       ├── webhooks.js          # Webhook 管理
│       ├── sections.js          # 频道分区
│       ├── twofa.js             # 2FA 强制
│       ├── sessions.js          # Session 管理
│       ├── invitations.js       # 邀请管理
│       ├── announcements.js     # 公告管理
│       ├── usage.js             # 使用报告
│       ├── analytics.js         # Analytics
│       ├── deactivation.js      # 用户停用
│       ├── user-reports.js      # 用户举报
│       ├── message-reports.js   # 消息举报
│       ├── bots.js              # Bot 管理
│       └── notif-prefs.js       # 通知偏好
│
├── lib/
│   ├── marked.js                # [引入] Markdown 渲染
│   └── purify.js                # [引入] DOMPurify
│
├── styles/
│   ├── thread-panel.css         # [新增] 线程面板样式
│   ├── settings.css             # [新增] 管理控制台样式
│   └── search-enhanced.css      # [新增] 搜索增强样式
│
├── tests/                       # [新增]
│   ├── components/              # 组件测试
│   ├── bus/                     # ws-bus 测试
│   ├── router/                  # 路由测试
│   └── integration/             # 集成测试
│
└── ... 现有文件（search.js, modals.js, polls.js 等保持不变）
```

---

## 总结

Aero IM 当前处在「后端完备、前端裸露」的架构不平衡状态。5 个方向的共同特征是**后端 API 就位、前端实现为零或不足**。这是一个工程优先级的问题，不是架构缺陷——团队之前聚焦后端基础设施（迁移、仓储、鉴权、实时总线、流媒体），这些投入的回报是：现在可以以「纯前端工作」的形态交付这些功能，而不需要改动后端。

**最高价值的架构建议**：不要在管理控制台的开发中途才开始考虑组件抽象。阶段 0 的 2 周基础设施投入是杠杆最高的决定——它会让后续 7 周的开发效率提高 2-3 倍，且避免 15 个设置页出现 15 种不同的代码风格。

**最大的架构风险**：线程面板的布局改动（右栏替换）和去中心化的 `handleIncomingMessage` 重构（ws-bus）如果在同一 sprint 进行，可能互相干扰。应该错开——线程面板先以「独立订阅 + 挂载侧边栏」的方式实现，ws-bus 的全面推广放在后续。
