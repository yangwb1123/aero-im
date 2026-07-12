# 架构分析：Aero IM Web 前端

## 1. 架构评估

### 1.1 当前架构的优势

- **零运行时框架、零依赖核心**：纯 ES2020 模块，无 React/Vue 等重型框架。直接 DOM 操作为调试客户端提供了无中间抽象层的路径，极端轻量（`render.js` 685 行完成全部视图渲染）。
- **模块边界清晰**：按领域切分 `calls.js` / `live.js` / `polls.js` / `search.js` 等，每个模块公开最小接口（函数导出），符合 feature-first 原则。
- **WebSocket 层工程质量高**：`SeqGate` 去重基于 per-subject 单调 seq 对抗 at-least-once 重投，`WsClient` 带指数退避重连 + `?since=` 游标回溯，backfill truncated 的 REST 兜底机制完备。这层放在生产级产品中也不需大改。
- **DOM 安全性优秀**：所有用户输入都经由 `textContent` 注入，纯 `innerHTML` 仅用于零插值的静态骨架模板，XSS 攻击面极小。
- **启发式能力（Server-side Markdown parsing / Block Kit）**：消息块的 `spans` 服务端解析使客户端免于实现富文本解析器，`Block` 类型系统（text/code/mention/file/voice/card/button/select）是一个轻量级 UI 组件协议。

### 1.2 核心架构限制与技术债

| 层级 | 问题 | 影响 | 债务等级 |
|---|---|---|---|
| **状态管理** | `state` 是单例全局对象，`state.messagesByRoom` 持有所有房间全部消息（不做 Watermark/Page eviction），随使用增长线性膨胀 | 内存泄漏 + 首次 `switchRoom` 重新渲染全量 DOM | **P0 - 性能** |
| **渲染** | 无虚拟滚动，`renderMessage` 14-40+ DOM 节点/消息。5000 条 → 125K-200K DOM 节点 | 布局抖动、OOM、大房间（万人群）不可用 | **P0 - 性能** |
| **CSS 管理** | 单文件 1236 行，无 BEM/CSS Modules/Shadow DOM，所有选择器全局作用域 | 随着模块增多，命名冲突风险递增，可维护性下降 | **P2 - 可维护** |
| **测试覆盖** | 前端零测试（grep 无 `*.test.js` / `cypress` / `playwright` 引用），验证仅靠 lint + 后端 smoke | 回归脆弱，重构风险高 | **P2 - 质量** |
| **可访问性** | `render.js` 中零 `aria-*` 属性引用；`style.css` 中存在 `:focus { outline: none }` 风险 | 键盘导航 + 屏幕阅读器支持缺失 | **P1 - 合规** |
| **PWA / 离线** | 无 Service Worker，`manifest.json` 已配 `display: standalone` + 图标但无 SW 注册 | 无离线能力，无 cache-first 资源策略 | **P1 - 体验** |
| **构建/缓存** | 零构建步骤，资源直接从 CDN 加载（hls.js），无版本指纹 | 浏览器缓存失效率低，发布后用户可能持旧代码 | **P1 - 部署** |
| **错误边界** | 无 React-like error boundary；仅 `unhandledrejection` 全局兜底 | 单一渲染异常可能崩溃整个视图 | **P1 - 韧性** |
| **国际化** | UI 硬编码中文（"发送"、"搜索"、"消息已删除" 等） | 多语言产品需重写所有用户面向文案 | **P3 - 扩展** |

### 1.3 关键设计决策评估

| 决策 | 评价 | 建议 |
|---|---|---|
| 无框架直接 DOM | 对于 685 行 `render.js` 正确——复杂度和规模匹配。但当视图量级超 10K DOM 节点时，无框架的 diff/patch 成本快速上升 | 保留直接 DOM 操作思路，但引入虚拟 scroller 做 DOM 回收 |
| 全局 mutable `state` 对象 | 简单直接，但副作用不可追踪。异步 handler （`handleIncomingMessage` → `replaceNodeForMsg`）直接触 DOM 变化而不经过 diff | 中型规模下可接受，不建议上 Redux/Zustand 等重型状态管理；可考虑 `Proxy` 做变更记录给调试 |
| 服务端渲染 Markdown → `Block[]` | 优秀设计——客户端不需维护 parser，服务端产出格式统一 | 保留，但考虑加客户端 fallback parser 做离线场景 |
| WebSocket 作为实时唯一通道 | 与后端事务边界对齐。`SeqGate` 去重是亮点 | 保留，加强 slow-consumer 端的背压策略 |
| `state.messagesByRoom` 全量缓存 | 当前最大架构缺陷。每切换房间就全替换 `msgList.replaceChildren()`，LLM 对话场景消息量极易膨胀 | **必须改为 LRU + 虚拟滚动** |

---

## 2. 扩展方向

### 方向 A：虚拟滚动与 DOM 回收（P0）

**为什么需要**：Aero IM 的 endgame 场景（万人群 / 直播弹幕 / AI 会话历史回溯）需要在前端管理 10K-100K 消息，当前全量 DOM 策略不可扩展。Node 对每个元素约有 300-800 字节主体开销，100K 条消息 1.5M-8M DOM 节点不会在现代浏览器稳定运行（大部分浏览器对 >500K 节点就开始出性能问题）。

**核心挑战**：
- 每个 message DOM 节点高度可变（reply chip / blocks 递归 / reactions / read strip），`IntersectionObserver` 驱动回收时需精确保持 `scrollTop` 锚
- 从 `state.messagesByRoom` 切到 Segment/Chunk 存储（例如每 500 条一分段），虚拟 scroller 只持有视口内及缓冲区的 ~60-100 条
- 编辑/删除/反应等 mutation 需要能从 id 快速定位到 DOM 节点——如果该 segment 被回收，需"重新水合"
- `loadHistory` 的上拉 pagination 需要和虚拟滚动协作，避免滚动位置跳变

**预期架构变更**：
```
state.messagesByRoom: Map<RoomId, Message[]>
    ↓
state.messageStore: Map<RoomId, {
  segments: Segment[],         // 每个 segment 是 500 条 frozen 快照
  visibleRange: [start, end],  // 视口条索引范围
  virtualLength: number,       // 总条数
}>
```

新增 `VirtualScroller` 类（~400 行）：
- 持有容器元素、行高估计、缓冲策略
- 监听 `onscroll` → 计算 `visibleRange` → 对不再可见的 segment 做 `replaceChildren()` 释放
- 通过 `CachedRow`（离屏 DOM）加速新 segment 展现

**对现有系统影响**：
- `renderMsgWithReactions` 等保留，接收的参数从全消息列表改为单条
- `handleIncomingMessage` 需增量插入：若在可见范围 → append / prepend；若在回收区 → 只更新 store
- `switchRoom` 不再 `els.msgList.replaceChildren()`，而是创建新 scroller
- `loadHistory` 兼容即可（从 segment store 更底层拿数据）

**估算工作量**：~3 天核心 + 1 天 mutation 兼容 + 1 天测试

---

### 方向 B：Service Worker 与离线策略（P1）

**为什么需要**：`manifest.json` 已配 PWA 元数据（`display: standalone` + 双图标 + `categories`），但无 SW 注册，用户"添加到主屏幕"后仍无离线能力——消息客户端作为高频使用场景，网络中断时的读缓存意义重大。

**核心挑战**：
- 离线设计面复杂：IM 是双向实时应用，简单 cache-first 策略会导致 stale 数据
- 需考虑四层缓存：① Shell（HTML/CSS/JS）→ Cache-First；② API 响应（历史消息）→ Network-First with cache fallback；③ 已渲染的消息 DOM→不可 cache，离线时仅展示缓存的 JSON
- WebSocket 在线切换：SW 可以在离线后保持通知弹窗（类似 WhatsApp Web 的“已断开连接”）

**预期架构变更**：
```
web/
  sw.js              # 新文件，~150 行
  register-sw.js     # app.js 最前面 import，~20 行
```

SW 生命周期：
- `install` → prefetch shell (index.html + *.js + style.css)
- `activate` → 清理旧 cache
- `fetch` 拦截：static assets → Cache-First；API → Network-First；WS → 不拦截但发 `offline` 事件

**对现有系统影响**：
- `index.html` 加 `<script>` 注册 SW
- `app.js` / `ws.js` 加 online/offline event listener，离线时显示 banner
- 无其他结构性变化

**估算工作量**：~1 天核心 + 0.5 天离线 UI + 0.5 天测试

---

### 方向 C：构建管线与模块 Bundling（P1）

**为什么需要**：当前 `index.html` 直接 `<script type="module" src="app.js">` 依赖浏览器 ESM 支持，无 tree-shaking、无代码分割、无资源指纹。`web/` 目录下已有 17 个 `.js` 源文件但全部透传。对于生产部署：
- 浏览器耗时为 17 次独立 HTTP 请求（无 HTTP/2 时显著）
- 无 minification（当前 `render.js` 30KB 未压缩）
- 无 dead code elimination

**核心挑战**：
- 需要零运行时开销的构建工具（不引入 React compile/babel plugin 等）
- `esbuild` 最优：快、ESM 原生输出、JSX 无关
- 需不改动源码模块（import/export 语法完全符合 ES2020）

**预期架构变更**：
```
web/                         web/dist/ (esbuild 输出)
  app.js        esbuild →      app-[hash].js
  render.js       →            render-[hash].js (chunk)
  ws.js           →            ws-[hash].js (chunk)
  ...
  index.html    esbuild →      index.html (hash 替换)
```

构建流程：
```json
{
  "scripts": {
    "build": "esbuild app.js --bundle --splitting --outdir=dist --format=esm --minify",
    "dev": "esbuild app.js --bundle --outdir=dist --format=esm --servdir=."
  }
}
```

**对现有系统影响**：极小——只需改 `index.html` 加载路径。不改变运行时。

**估算工作量**：~4 小时

---

### 方向 D：Accessibility 可访问性改造（P1）

**为什么需要**：`render.js` 当前零 `aria-*`，`style.css` 的 `:focus { outline: none }`（如确认存在）使键盘用户不可见焦点。无 `role` 标注、无 `tabIndex`、无快捷键。法律合规（如 EU Accessibility Act / ADA）和企业采购前置条件。

**核心挑战**：
- 消息列表是最复杂的交互区域——每条消息是 `role="article"`, 带 actions (`role="toolbar"`), reactions (`role="group"`)
- 虚拟滚动（方向 A）后还需保持焦点管理：滚动后焦点不能丢失
- 需要维护者持续注意——不是一次性改造

**预期架构变更**：
在 `render.js` 的各渲染函数加 `attrs`：
- `renderMessage`: `wrap.setAttribute('role', 'article')` + `aria-posinset` / `aria-setsize`
- `wireMsgActions`: actions container → `role="toolbar" aria-label="消息操作"`
- `msg-reactions`: → `role="group" aria-label="反应"`
- Composer: `textarea` → `aria-label="消息输入框"`
- 消息列表容器: `role="log" aria-live="polite"`（或 `role="feed"`）

`style.css` 修改：
- `:focus { outline: none }` → `:focus-visible { outline: 2px solid var(--brand-1); outline-offset:2px; }`

**对现有系统影响**：纯 additive——不改功能逻辑，只加属性。不影响现有测试。

**估算工作量**：~2 天

---

### 方向 E：消息储存层 LRU 化与内存治理（P0-P1）

**为什么需要**：`state.messagesByRoom` 是 `Map<RoomId, Message[]>`，无容量上限。用户打开 50 个房间，每个 1000 条消息，每条消息平均 2KB JSON → 50 × 1000 × 2KB = 100MB。这是个真实泄漏路径。

**核心挑战**：
- 需要与虚拟滚动（方向 A）协同：不可见的房间消息可以序列化到 `sessionStorage` 或 LRU Map，可见时反序列化
- 跨会话持久化？`sessionStorage` 可保 tab 切换，但离开 tab 即释放
- 需要明确的 eviction 策略：保留最近 5 个房间的全量，其余只保留元数据（房间名 + 最后一条）

**预期架构变更**：
```
// 在 state 上加代理
state.messagesByRoom: RoomMessageStore
  - maxRooms: 10
  - get(roomId): Message[] | null  // LRU get, bump
  - set(roomId, arr): void
  - evict(roomId): void            // 序列化到 sessionStorage
```

`sessionStorage` 序列化策略：
- 非活跃房间（切出 >5 分钟）→ 压缩 JSON + `sessionStorage.setItem`（5MB 限额，粗略每条消息 0.5KB 压缩，约 10K 条/域）
- 超出限额→ 仅保留最后 200 条

**对现有系统影响**：
- 所有 `state.messagesByRoom.get/set` 调用点自动享受 LRU（接口不变更）
- `switchRoom` 触发时如果房间已 evict → 从 sessionStorage 反序列化 + REST backfill
- 需处理 `sessionStorage` 写异常（限额满时 `try/catch`）

**估算工作量**：~1.5 天核心 + 0.5 天集成测试

---

## 3. 接口设计建议

### 3.1 关键模块接口现状与改进

当前架构中没有显式的接口定义（TypeScript / JSDoc），全部通过函数签名约定。建议加 JSDoc `@typedef` 标注关键类型，使编辑器推导可用。

**消息体类型**（当前靠注释 + 阅读 Rust `Block` enum 推断）：

```typescript
// 建议加 JSDoc typedef
/**
 * @typedef {{ type: 'text', content: string, spans?: Span[] }} TextBlock
 * @typedef {{ type: 'code', content: string, lang?: string }} CodeBlock
 * @typedef {{ type: 'mention', participant: string }} MentionBlock
 * @typedef {{ type: 'file', blob_id: string, name?: string, kind: string, size?: number }} FileBlock
 * @typedef {{ type: 'voice', blob_id: string, transcript?: string }} VoiceBlock
 * @typedef {{ type: 'card', schema?: string, payload?: object }} CardBlock
 * @typedef {{ type: 'button', label: string, action_id?: string, url?: string, style?: string }} ButtonBlock
 * @typedef {{ type: 'select', action_id: string, options: Array<{label:string,value:string}>, placeholder?: string }} SelectBlock
 * @typedef {TextBlock|CodeBlock|MentionBlock|FileBlock|VoiceBlock|CardBlock|ButtonBlock|SelectBlock} Block
 */
```

### 3.2 是否需要新抽象层

**建议引入 "视图数据层" ViewModel Layer**：

当前 `handleIncomingMessage` 同时做：
1. 消息去重 → 存储层
2. DOM 追加 → 视图层
3. 未读计数更新 → 状态层
4. 桌面通知 → 外部系统

单一函数有 4 个职责。建议加中间层 `RoomViewModel`：

```
WebSocket frame
  → handleIncomingMessage → 消息去重 + 存储层写入
  → RoomViewModel.append(message)
    → 检查当前房间是否 active
    → 检查虚拟滚动可见范围
    → 触发 render 或只写 store
    → 更新未读
    → 触发热通知（仅在 tab hidden 时）
```

**设计原则**：保持 `render.js` 纯渲染（input: data → output: DOM），所有副作用（网络、通知、存储）从视图层分离。

### 3.3 向后兼容性

所有建议的架构变更**对后端无影响**。WebSocket 协议不变，REST API 不变。变更局限在前端架构：

- 方向 A（虚拟滚动）：`renderMessage` 函数签名不变，新增 `VirtualScroller` 类
- 方向 B（SW）：完全 additive
- 方向 C（构建）：输出产物变化但入口不变
- 方向 D（A11y）：纯加属性，不影响功能
- 方向 E（LRU）：替换 `state.messagesByRoom` 但暴露相同 `.get/.set` API

---

## 4. 技术选型

### 4.1 引入技术栈的评估

| 技术 | 是否推荐 | 理由 |
|---|---|---|
| **esbuild**（构建） | ✅ 推荐 | 零配置、极速（比 Webpack 快 20-100x）、原生 ESM 输出、JSX 无关、tree-shaking 内置 |
| **eslint + prettier** | ✅ 推荐 | 当前已有 `eslint.config.js` 基本配置，推荐补充 prettier + 提交前 hook |
| **Playwright**（E2E 测试） | ✅ 推荐 | 比 Cypress 轻、Typed API、支持 WebSocket 拦截测试的关键场景 |
| **Lit / Web Components** | ❌ 不推荐 | 抽象代价高于收益；`render.js` 的函数式 DOM 构建在 685 行内足以表达当前 Block Kit 复杂度 |
| **React / Preact** | ❌ 强烈不推荐 | 虚拟 DOM diff + reconciliation 代价在当前规模下比直接 DOM 高，而且 SPA 框架带重量的心智模型 |
| **TypeScript** | ❌ 当前不推荐 | 编译集成复杂（需 tsc 双通道），建议先保持 JSDoc 类型标注过渡。3-6 月后如模块超 30 个再考虑 |
| **Zustand / Jotai** 状态管理 | ❌ 不推荐 | 全局 `state` 对象加 JSDoc 足够清晰，虚拟滚动和 LRU 不需要原子订阅 |
| **Tauri**（原生壳） | ⏳ 未来可考虑 | 如果 Aero IM 需桌面版，Tauri 可直接复用现前端代码 |

### 4.2 自建 vs 采购决策

| 需求 | 建议 | 原因 |
|---|---|---|
| 虚拟滚动 | **自建** | 消息行高度可变（reply chip / blocks / reactions），市面虚拟滚动库（TanStack Virtual / react-virtual）对接 React；与纯 DOM 架构不兼容 |
| PWA/离线 | **自建 SW（~150 行）** | SW 规范稳定，逻辑可用简单 Cache-First/Network-First 模式，不需 Workbox |
| 端到端测试 | **Playwright 自建** | 消息客户端有独特的 WebSocket 测试需求；Playwright 的 `page.routeWebSocket()` 使直接注入 `send_message`/接收 `msg:message` 帧可行 |
| 可访问性审计 | **axe-core（集成测试）** | npm `@axe-core/playwright` 可在 E2E 测试中自动扫描 A11y 违规 |

### 4.3 第三方依赖评估标准

当前 `web/package.json` 仅列 eslint 相关 devDependencies + hls.js CDN 引用。建议维持**零运行时依赖**策略，引入 dev 工具时评估：

1. **文件体积**：生产是否引入新的 HTTP 请求？（hls.js 已是外部 CDN，不影响 JS 包）
2. **许可兼容**：MIT / Apache-2.0 / BSD（AGPL 不可）
3. **维护活跃度**：GitHub stars > 1K + last commit < 6 月
4. **可替换性**：能否用 <200 行自建替代？

---

## 5. 实施路线图

### 优先级矩阵

| 方向 | 优先级 | 理由 |
|---|---|---|
| A: 虚拟滚动 + DOM 回收 | **P0** | 当前瓶颈，影响核心功能可用性 |
| E: LRU 化消息存储 | **P0** | 内存泄漏风险，影响稳定运行 |
| B: Service Worker | P1 | 产品化门槛，PWA 完整性 |
| C: 构建管线 | P1 | 生产部署前必做 |
| D: Accessibility | P1 | 合规门槛 + 企业采购先决条件 |
| 前端测试（Playwright） | P1 | 后续重构的保险 |
| 国际化基础设施 | P3 | 当前无多语言需求 |

### 阶段划分（建议 4-6 周）

```
Phase 1 (Week 1-2): 核心性能修复 [A+E]
  ├── Day 1-2:   VirtualScroller 原型（~300 行）
  ├── Day 3-5:   Segment-based message store + 增量插入
  ├── Day 6-8:   编辑/删除/反应在分片模型下的定位与重水合
  ├── Day 9-10:  LRU 消息存储（sessionStorage 序列化 + 反序列化）
  └── Day 10:    集成测试（Playwright WS mock, 1000 消息压力测试）

Phase 2 (Week 3): 生产就绪 [B+C]
  ├── Day 1-2:   Service Worker + PWA 注册
  ├── Day 3:     esbuild 集成 + hash 指纹 + CDN 路径
  └── Day 4-5:   CI 管线中集成构建 + lint + 测试

Phase 3 (Week 4-5): 质量提升 [D + 测试覆盖]
  ├── Day 1-3:   A11y 改造（aria-* + role + keyboard nav）
  ├── Day 4-5:   Playwright E2E（核心路径：发消息、切换房间、搜索、反应）
  └── Day 6-8:   错误边界 + 监控（前端的 Sentry / 自己的 error handler）

Phase 4 (Week 6, 可选): 可观测性
  ├── Day 1:     Web Vitals（INP/CLS/LCP）埋点
  ├── Day 2:     手动性能报告（`performance.mark` 测量虚拟滚动帧率）
  └── Day 3:     WebSocket 延迟看板
```

### 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| 虚拟滚动改造导致编辑/反应/已读等 mutation 无法定位已回收 DOM | 高 | 严重 | **渐进式改造**：Phase 1 从 `msgList.replaceChildren()` 改为 `mutation.set(msgId, newDom)` + 可回收/不可回收规则。先不加真正 DOM 回收，只加水合/去水合逻辑 |
| LRU + `sessionStorage` 限额满（5MB）后降级行为不明确 | 中 | 中 | **明确阶梯降级**：5MB → 每组仅保留最后 200 条 + `console.warn`；`sessionStorage` 不可用（隐私模式）→ 直接回退现有纯内存 |
| Service Worker 离线策略与 WebSocket 冲突，用户看到 stale 数据 | 中 | 中 | **不缓存 API 消息响应**；SW 只缓存静态 shell，消息始终走 Network-First |
| 团队不熟悉 Playwright WebSocket mock | 低 | 低 | 提供 `test/ws-mock.js` 帮助函数 + 样例测试（`test/message.spec.js`）作模板 |
| A11y 改造对现有 `msg-actions` 点击事件产生意外行为 | 低 | 低 | 纯加属性不改逻辑；`focus-visible` 替代 `focus` 改 `outline` 后通过视觉快照对比验证 |

---

## 总结

Aero IM 前端架构当前最大的**架构债务**是 **消息存储无上限 + 无虚拟滚动**。这会在消息规模 >2000 条/房间时触发严重性能退化。

**第二优先级的风险**是无构建管线 + 无测试覆盖——虽然不影响小规模用户，但阻碍了后续所有架构变更的交付（安全网缺失）。

**建议立即启动 Phase 1**：虚拟滚动 + LRU 消息存储。这是唯一能从根本上消除「OOM / 页面崩溃」风险的变更。两个工作并行进行（LRU 不依赖虚拟滚动，可单独交付）。

**不需要做的**：不引入框架，不迁移 TypeScript，不引入重型状态管理。当前函数式模块化 + 全局 state 的组合在 Aero IM 的规模下足够。等模块 >30 个或团队 >3 人再考虑晋升架构。
