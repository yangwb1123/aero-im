# 架构师评估：Aero IM Web SPA 客户端缺口分析

## 1. 架构评估

### 1.1 当前架构的核心设计决策

当前 web SPA 的架构选择——**零依赖、零构建、纯原生 ES2020**——是一种经过深思熟虑的**约束驱动架构**。理解这一点是评估所有扩展方向的前提。

**已做出的关键设计决策：**

| 决策 | 表现 | 架构含义 |
|------|------|---------|
| 无框架 | 全部手写 DOM + WS | 无虚拟 DOM 开销，无框架升级债务；但组件化靠约定而非强制 |
| 无构建步骤 | `.js` 直接 serve | 部署极简（cp 即发布）；但无 tree-shaking、无 TS、无代码分割 |
| `el()` 工厂函数 | `el('div', {class:'x'}, [child])` | 声明式 DSL 雏形——本质是手写 JSX 的穷举版，无 diffing |
| CSS 变量做主题 | `:root { --bg: #0b0d12; }` | 主题化基础设施已就位，仅 20 个变量 — 覆盖率 ~40% |
| WebSocket 直连 | `ws.js` 单例 + JSON 帧 | 实时性优，但无重连退避、无心跳恢复、无 token refresh |
| 模块通过 `<script>` 加载 | 无 ES modules / import maps | 全局命名空间污染风险；依赖加载顺序隐式 |

### 1.2 架构优势

1. **极致精简**：单页 SPA ~300KB（未压缩），首屏加载 = 1 个 HTML + 几个 JS + 1 个 CSS。无框架冷启动开销，移动端 3G 场景友好。
2. **无框架锁定风险**：不绑定 React/Vue/Svelte 生态，未来迁移成本仅为逻辑迁移而非工具链迁移。对 startup 而言，这是巨大的**期权价值**。
3. **可审计性**：每行 DOM 操作可见，无黑盒框架行为。安全审计（XSS 面）只需 grep textContent/innerHTML 即可覆盖。
4. **部署简洁**：`cp -r web/ /var/www/` 即完成部署，与后端解耦。可 embed 进 Rust binary（axum static file serve），零额外基础设施。

### 1.3 架构债务与技术债

**高优先级债务（影响功能交付的）：**

1. **Token 生命周期管理缺失**（方向外但架构层面关键）
   - 现状：`localStorage.getItem('auth_token')` → 设置 `Authorization` 头 → 过期后 401 永不重试
   - 影响：用户每 15 分钟被迫刷新页面
   - 架构根因：没有**统一的 HTTP 客户端层**——REST 调用散布在各模块中（`let res = await fetch(...)` 模式），无法挂载拦截器
   - 修复成本极低，但需要所有模块改用统一 client

2. **`el()` 函数签名不支持 attribute 透传**
   - 现状：`el('button', {class:'btn', onclick: handler}, 'text')`——第二个参数只识别有限的键
   - 影响：添加 `aria-label`、`role`、`tabindex`、`data-*` 需要事后 `.setAttribute()`，导致 a11y 属性的声明式添加和组件化的绑定断裂
   - 架构根因：`el()` 的参数处理采用 allowlist（`class`/`id`/`onclick`/`href`/`src` 等），未开放通用属性通道

3. **无状态管理模式**
   - 现状：状态存储在 DOM 属性（`element.dataset`）和闭包变量中
   - 影响：不同模块间共享状态（如当前用户 ID、当前房间 ID、主题偏好）需通过全局变量或 DOM 爬取
   - 架构根因：从未设计应用级存储抽象

**中优先级债务：**

4. **CSS 硬编码颜色的比例被低估**
   - 文档称「50% 已变量化」，实际 ~40%。约 60% 的颜色值仍为硬编码。
   - 影响：主题迁移（亮色/暗色）需要全量扫描和替换，工作量约 2-3 天纯 CSS 重构。
   - 建议：提取一个迁移清单，按 CSS 文件行号标注。

5. **模块加载顺序依赖隐式**
   - 现状：`index.html` 中 `<script src="...">` 的顺序有隐含依赖（`ws.js` 先于 `app.js`，`render.js` 先于各 feature JS）
   - 影响：重构时容易破坏顺序，且无法通过静态分析验证
   - 改进建议：最轻量方式是每个模块自行检测依赖是否存在（`if (typeof el === 'undefined') throw Error(...)`）

**低优先级 / 长期：**

6. **无 SPA 路由**：所有页面状态通过显示/隐藏 `div` 切换，URL 不反映应用状态。影响：无法直接链接到特定房间/消息/通话说，也无法使用浏览器后退按钮。

---

## 2. 扩展方向

### 方向 A：统一 API Client 层（Token 生命周期 + 请求/响应拦截器）

**为什么需要：**
- 这是当前最大的单点故障——access token 过期后应用永久不可用
- 所有 REST 调用散布在各模块中，无法挂载拦截器（retry、refresh token、统一错误处理）
- 文档范围外的 P0 缺陷，但架构上直接影响方向四（上传）和方向五（媒体预览）

**核心挑战：**
- 现有代码中 fetch 调用约 30+ 处，分布在 8 个模块中，调用模式不一致（有的 `.json()`，有的 `.blob()`，有的 `.text()`）
- 需要保持向后兼容——新 client 应是一个 drop-in wrapper
- 上传进度需要将 fetch 替换为 XHR 或 `fetch + ReadableStream`——两个不同抽象

**架构变更：**

```
现状：
  app.js: fetch('/api/...', {headers: {Authorization: 'Bearer '+token}})
  calls.js: fetch('/api/...')
  搜索模块: fetch('/api/...')
  → 每处独立处理 401/错误

目标：
  // api-client.js — 新增模块
  const api = createApiClient({
    baseUrl: '/api',
    tokenProvider: () => localStorage.getItem('auth_token'),
    refreshTokenProvider: () => localStorage.getItem('refresh_token'),
    onUnauthorized: async () => { /* refresh flow */ },
  });
  
  // 所有模块改用：
  const data = await api.get('/rooms/' + roomId + '/messages');
  // POST with progress:
  const result = await api.post('/upload', formData, { onProgress: (pct) => ... });
```

**对现有系统的影响：**
- 低影响——`api.get(...)` 与 `fetch(...)` 签名相近，可逐模块替换
- 新 client 模块可独立测试（mock fetch）
- 不为零：需要同时支持 `fetch`（现有）和 XHR/ReadableStream（上传进度）

**可选方案对比：**

| 方案 | 工作量 | 优点 | 缺点 |
|------|--------|------|------|
| A1: 封装 fetch（`api.get/post/put/del`） | ~2 天 | 最小改动，retry/refresh 逻辑集中 | 无法做上传进度（fetch 的 ReadableStream 上传进度需要双倍工作量） |
| A2: 纯 XHR 封装 | ~3 天 | 原生支持上传进度、超时精细控制 | callback 风格，需 Promise 包装 |
| A3: 双通道（fetch + XHR fallback） | ~4 天 | 两全其美 | 接口复杂，需 `{ responseType: 'json'|'blob'|'progress' }` |

**建议**：A3，但 API 设计为 `api.get()`/`api.post()` 默认 fetch，`api.upload()` 专门用 XHR。这样不改变 90% 调用，仅上传路径特殊处理。

---

### 方向 B：轻量组件系统（基于 `el()` 的演化而非替换）

**为什么需要：**
- 当前 `el()` 不支持 attribute 透传——每个需要 `aria-*`/`role`/`tabindex`/`data-*` 的组件都要事后 `.setAttribute()`，破坏了封装
- 无组件化导致一致性差——相同 UI 模式（成员列表、消息气泡、表格行）有多份重复实现
- i18n 和 Theme 都需要在组件渲染层面注入

**核心挑战：**
- 不能引入框架——现有代码已 2300+ 行，重写成本过高
- `el()` 的签名扩展需要向后兼容
- 组件化 ≠ 框架化——需要在不引入 virtual DOM 的前提下实现声明式渲染

**架构变更：**

```
现状 el()：
  el('button', {class:'btn', onclick: handler}, 'Click me')
  → 识别有限 keys，未知 keys 静默忽略

目标 el() 扩展（向后兼容）：
  el('button', 
    {class:'btn', onclick: handler, 'aria-label': 'Send', 'data-id': '42'},
    'Click me'
  )
  → 所有 unknown keys → setAttribute

组件模式（新增，不强制）：
  // 不定义 class/function，而是约定：
  // 1. 文件名 = 组件名（如 message-bubble.js）
  // 2. 导出函数：render(props) → DOM 元素
  // 3. 组件可带 .update(el, props) 方法
```

**如何保持轻量：**
- 不需要 class/extends 关键字——只需函数返回 DOM 的约定
- 不需要 shadow DOM——避免 polyfill 和样式隔离开销
- 不需要 virtual DOM——数据变化后直接操作 DOM（由组件自行决定）

**关键判断**：这是**正确的基础设施投资**——它解锁了 a11y（透传 ARIA）、Theme（组件层级覆写）、i18n（组件内文本可替换）三个方向。如果不做，三个方向的改动都会在 `el()` 周围打补丁，产生更多技术债。

---

### 方向 C：应用级存储抽象（AppStore）

**为什么需要：**
- 当前无集中状态管理——房间列表、当前用户信息、设置偏好（主题、语言）散落在各处
- Theme 切换需要广播到所有活跃组件（当前用户点「切换亮色」后所有已渲染元素需更新）
- 语言切换同理——已渲染的文本不会自动更新
- 无持久化设置——刷新页面后主题/语言偏好丢失

**核心挑战：**
- 不能引入 Redux/Zustand 等外部库（零依赖约束）
- `EventEmitter` + 响应式更新需自行实现
- 需要决定「什么进 Store」vs 「什么保持本地」

**架构变更：**

```javascript
// store.js — 极简响应式 Store
const store = createStore({
  // 初始值
  user: null,
  theme: 'dark',     // 从 localStorage 读取
  locale: 'zh-CN',   // 从 localStorage 读取
  activeRoom: null,
});

// 订阅
store.subscribe('theme', (newVal, oldVal) => {
  document.documentElement.setAttribute('data-theme', newVal);
  // 所有感知主题的组件通过观察者更新
});

// 更新
store.set('theme', 'light'); // → 通知所有 theme 订阅者
```

**技术难点：**
- **已渲染元素的更新**是真正的挑战——改变主题色（CSS 变量切换）还好，因为浏览器自动 re-render；但改变语言需要组件自行处理更新
- 一种方案：每个组件在 mount 时注册 `store.subscribe('locale', () => this.reRender())`——但需要 `reRender()` 方法约定

**建议**：Store 本身轻量（~80 行），但需要组件配合。新增的组件系统约定应包括更新生命周期。

---

### 方向 D：i18n 基础设施设计

**为什么需要：**
- 零 i18n 基础 = 产品只能服务中文市场
- 企业 IM 产品需要支持多语言（尤其是中英文双语界面）
- 所有三个高优先级方向（Theme、a11y、File Upload）的文本输出都需要 i18n

**核心挑战：**
- 所有 UI 文本散布在 HTML 模板、`textContent` 赋值、CSS `::before`/`::after` content 中——需全量扫描提取
- 无构建步骤 → 不能使用 `.po`/`.json` 编译流程——需要运行时加载 locale 代码
- 动态内容（用户生成的消息）不应翻译——需区分 UI 文本和内容文本

**架构决策：t() 函数 + 模块级 locale 文件**

```
// i18n.js — 新增模块
// 不引入 ICU/Mozilla L20n，使用简单 key-value 模式

const locales = {};
let currentLocale = 'zh-CN';

export function t(key, params = {}) {
  let str = locales[currentLocale]?.[key] || key;  // fallback = key
  return str.replace(/\{(\w+)\}/g, (_, k) => params[k] ?? `{${k}}`);
}

export function setLocale(locale) {
  currentLocale = locale;
  document.documentElement.lang = locale;
  store.set('locale', locale);  // 通知所有订阅组件
}

// locale/zh-CN.js
export default {
  'chat.placeholder': '输入消息...',
  'search.placeholder': '搜索消息...',
  'upload.button': '上传文件',
  'upload.progress': '上传中 {percent}%',
  'settings.theme': '主题',
  'settings.language': '语言',
};
```

**为什么不使用标准 i18n 库：**
- 所有 JS i18n 库（i18next、globalize、intl-messageformat）都假设构建步骤
- 项目零依赖约束——引入 i18next（~30KB gzip）与当前架构哲学相悖
- `Intl` API 浏览器内置处理数字/日期/货币格式化——不需要库

**对现有系统的影响：**
- 中等——需要全量文本替换（`el.textContent = '搜索'` → `el.textContent = t('search.label')`）
- 建议分模块进行，不要求一次完成
- CSS `::before`/`::after` 中的图标文本（如「▶」「◀」）不需要翻译——只翻译自然语言

---

### 方向 E：构建/资产管线（最简约版本）

**为什么需要：**
- 当前无构建步骤意味着：无代码分割、无 tree-shaking、无 TS 检查、无 CSS post-processing
- i18n locale 文件需要打包（多语言文件合并为一个 bundle）
- 主题变体需要按需加载（暗色主题 CSS + 亮色主题 CSS 分开）

**核心挑战：**
- 不能引入 Webpack/Vite 这种重量级工具链——与当前「cp 即部署」哲学冲突
- 需要保持部署简单

**建议方案：渐进式引入，三步走：**

**阶段 1**（立即可用，不改变现有流程）：
- 新增 `scripts/build.sh`，只做 3 件事：
  1. CSS 压缩（`csso`/`lightningcss` CLI）
  2. JS 压缩（`esbuild --minify` 或 `terser`）
  3. locale 文件合并（`cat locale/*.js > dist/locale.bundle.js`）
- 仍然可 `cp web/ /var/www/` 直接部署（未压缩版）

**阶段 2**（当需要主题/语言动态加载时）：
- 将 CSS 按主题拆分：`style.css`（共享）+ `theme-dark.css` + `theme-light.css`
- JS 按需加载：`import('./locale/en-US.js')` 动态加载 locale

**阶段 3**（如果需要更多）：
- 引入 TypeScript — `tsc --noEmit` 做类型检查，不改变输出
- `esbuild` 做 bundle + code split

**为什么不是 Vite 或 Webpack：**
- 项目只有一个 SPA，无复杂依赖图——问题空间不需要这些工具
- 构建工具本身有维护成本（升级 config、插件兼容性）
- 当前团队规模下，增加构建步骤带来的认知负载 > 收益

---

## 3. 接口设计建议

### 3.1 核心接口原则（六条）

1. **渐进增强**：新接口不能破坏现有调用模式。`el('div', {class:'x'})` 永远工作——新能力（attribute 透传）是附加的。
2. **函数式优于类式**：保持 JS 的函数风格。`render(props)` 优于 `new Component(props).render()`。
3. **约定优于配置**：不创建复杂的配置系统。模块发现通过文件命名约定，而非注册表。
4. **绑定延迟**：不初始化不用的子系统。`<script src="i18n.js">` 仅加载代码，调用 `t()` 时才会初始化。
5. **跨模块通信走事件，不走直接引用**：`store.subscribe()` / `window.dispatchEvent(new CustomEvent(...))`，避免模块间的 `import` 循环。
6. **默认失败优雅**：`t('missing.key')` → 返回 `'missing.key'`，不抛异常。`store.get('unknown')` → `undefined`，不 crash。

### 3.2 关键抽象层建议

**需要新增的抽象（5 个新模块）：**

| 模块 | 职责 | 依赖关系 |
|------|------|---------|
| `api-client.js` | 统一 HTTP 请求 + token refresh + retry | 无 |
| `store.js` | 应用级响应式存储 | 无 |
| `i18n.js` | `t()` 函数 + locale 管理 + 数字/日期格式化 | `store.js`（写入 locale 偏好） |
| `component.js` | 组件基类/约定 + `el()` 扩展 | 无 |
| `theme.js` | 主题切换 + CSS 变量管理 + `prefers-color-scheme` 检测 | `store.js` |

**需要改造的现有抽象：**

| 现有模块 | 改造内容 | 向后兼容要求 |
|---------|---------|-------------|
| `render.js` — `el()` | 支持 attribute 透传（unknown keys → setAttribute） | ✅ 完全向后兼容，现有调用继续工作 |
| `ws.js` | 增加重连退避（`exponential backoff`）+ 心跳检测 | 需要保证现有 `onmessage` 回调无影响 |
| `app.js` | 初始化时读取 `localStorage` 恢复主题/语言设置 | 无影响 |

### 3.3 组件接口设计（提案）

```javascript
// 组件是一个普通的 async 函数，返回 HTMLElement
// 约定：

/**
 * @param {Object} props - 组件的输入数据
 * @param {boolean} [props.isNew] - 可选，是否是新消息（触发动画）
 * @returns {HTMLElement} - 渲染的 DOM 节点
 */
async function MessageBubble(props) {
  const el = document.createElement('div');
  el.className = 'message-bubble';
  
  // 通过 el() 创建子元素（现有函数扩展后支持 aria-*）
  const avatar = el('img', {
    class: 'avatar',
    src: props.avatarUrl,
    'aria-hidden': 'true',     // ← attribute 透传
  });
  
  const text = el('span', {
    class: 'text',
    'aria-label': t('chat.message_from', { user: props.username }),
  }, props.text);
  
  el.append(avatar, text);
  return el;
}

// 组件自行管理更新：
MessageBubble.update = function(el, newProps) {
  // 可选定义，用于 store 订阅后增量更新
  el.querySelector('.text').textContent = newProps.text;
};

// 注册组件（可选，仅用于自动更新）：
// 组件名由文件路径推断：components/message-bubble.js → 'message-bubble'
```

**关键设计决策**：不引入生命周期钩子（`onMount`/`onDestroy`）。组件 = 渲染函数 + 可选的 `update` 静态方法。如果不需要自动更新，就不需要注册。

---

## 4. 技术选型

### 4.1 需要引入的技术

经过评估，**建议引入的技术栈变动最小化**：

| 技术 | 建议 | 理由 |
|------|------|------|
| 构建工具 | `esbuild`（仅 CLI 模式，非 bundler） | 零配置、极速。只需 `--minify` 和 `--outdir`，无 config 文件 |
| CSS 压缩 | `lightningcss` CLI | 比 csso 更快，支持 CSS 变量分析 |
| 类型检查 | 不考虑 TS（阶段 3 可选项） | 当前架构下，TS 的类型收益 < 它引入的思维负担 |
| 测试 | 不考虑（但建议开始人工测试清单） | JS 无模块系统意味着测试需要 mock 全局环境，成本高 |

### 4.2 不需要引入的技术

| 技术 | 为什么不引入 | 替代方案 |
|------|-------------|---------|
| React/Vue/Svelte | 2300+ 行重写成本过高；当前无框架方案性能足够 | 组件约定 + `el()` 扩展 |
| Webpack/Vite | 过度杀伤；项目无复杂依赖图 | `esbuild` CLI |
| Tailwind CSS | 与当前 CSS 变量主题系统冲突；增加 2+ MB utility class | 复用现有 CSS + 新变量 |
| TypeScript | 无法增量迁移（当前 JS 无模块系统）；需搭建完整 TS 编译链 | JSDoc 注释 + VSCode IntelliSense |
| i18next | 30KB 库 > 当前所有 JS 总大小 | 自建 2KB `t()` 函数 |
| Redux/Zustand | 与零依赖哲学冲突 | 80 行 `createStore()` |
| Service Worker | PWA 离线缓存非当前需求 | — |
| Shadow DOM | 无需要样式隔离的第三方组件 | CSS BEM 风格命名 |

### 4.3 构建管线选型决策树

```
需要自动构建吗？
├── 否 → 保持现状（cp web/ /var/www/ 直接可用）
│   └── 风险：locale 文件需要手动合并
│
└── 是 → 需要多少？
    ├── 仅压缩 → esbuild --minify（~10 行 shell 脚本）
    ├── + 多主题 CSS → 拆分为 theme-*.css，按需加载
    ├── + 多语言 → 运行时 import() 动态加载 locale
    └── + 代码分割 → esbuild --splitting（阶段 3）
```

**建议**：立即采用「仅压缩」阶段（5 分钟配置），不改变开发流程。等主题和 i18n 完成后再考虑 CSS/locale 拆分。

---

## 5. 实施路线图

### 5.1 优先级重新评估

基于架构分析，修正后的优先级：

| 优先级 | 方向 | 工作量 | 价值类型 | 前置依赖 |
|--------|------|--------|---------|---------|
| **P0** | **Token Refresh（统一 API Client）** | ~2 天 | **存在性缺陷修复** | 无 |
| **P1** | **`el()` 扩展（attribute 透传）** | ~0.5 天 | **基础设施** | 无 |
| **P1** | **AppStore + 设置持久化** | ~1 天 | **基础设施** | 无 |
| **P1** | **Light Theme 添加**（方向二修正） | ~2 天 | **产品体验** | CSS 变量枚举 → 需先完成 CSS 硬编码替换 |
| **P1** | **i18n 基础：`t()` + locale 文件结构** | ~2 天 | **产品体验** | 无（可并行于 Theme） |
| **P1** | **上传进度条**（方向五子项） | ~1.5 天 | **产品体验** | API Client（用于统一 client + progress） |
| **P2** | **a11y 基础（ARIA + Tab 顺序）** | ~3 天 | **合规/包容性** | `el()` 扩展（attribute 透传） |
| **P2** | **图片灯箱**（方向四） | ~1.5 天 | **产品体验** | 无 |
| **P2** | **快捷键系统**（方向一子项） | ~2 天 | **体验提升** | 无 |
| **P3** | **拖放/粘贴上传**（方向五子项） | ~1 天 | **体验提升** | API Client（可选） |
| **P3** | **视频/音频增强**（方向四子项） | ~2 天 | **体验提升** | 灯箱（可复用 overlay 逻辑） |
| **P3** | **Gallery/文档预览**（方向四子项） | ~4 天 | **体验提升** | 需要后端缩略图 |

### 5.2 阶段划分

```
阶段 0：「止血」（1-2 天）
  ├── Token Refresh 修复（统一 API Client 基础版）
  ├── el() attribute 透传（~50 行代码变更）
  └── AppStore 基础版（仅持久化 theme + locale 设置）

阶段 1：「双主题 + i18n 基础」（1 周）
  ├── CSS 变量全量替换（硬编码颜色 → var()）
  ├── Light Theme CSS（新建 theme-light.css）
  ├── Theme 切换 UI（设置面板开关）
  ├── i18n: t() 函数 + locale/zh-CN.js + locale/en-US.js
  └── 文本替换（~50% UI 文本）

阶段 2：「上传体验 + 组件系统」（1 周）
  ├── API Client 增强（upload with progress）
  ├── 上传进度条 UI（方向五）
  ├── 组件系统约定文档 + 示例
  └── 1-2 个核心组件重构为组件模式（消息气泡、成员列表）

阶段 3：「a11y 基础」（3-4 天）
  ├── 全局 Tab 顺序管理
  ├── 焦点环样式（主题感知）
  ├── 关键交互元素 ARIA 标签（发送按钮、输入框、列表项）
  └── 屏幕阅读器消息通知（aria-live region）

阶段 4：「媒体预览」（1 周）
  ├── 图片灯箱
  ├── Gallery 视图
  └── 视频/音频增强控制

阶段 5：「体验打磨」（持续）
  ├── 快捷键系统
  ├── 拖放上传
  ├── 剩余的 i18n 文本替换
  ├── 剩余的 CSS 变量替换
  └── 剩余 a11y 改进
```

### 5.3 风险点与缓解策略

| 风险 | 可能性 | 影响 | 缓解策略 |
|------|--------|------|---------|
| **Token Refresh 实现后在旧 token 和 refresh token 都过期时仍然需要重新登录** | 高 | 中 | 明确设计 refresh 失败时的回退：清除 token → 跳转登录页 → 保留当前输入框内容不丢（localStorage 暂存） |
| **Light Theme 的 WCAG 对比度不达标（4.5:1）** | 中 | 高 | 每个颜色值变量化后，用 `contrast-checker` CLI 验证所有 `--text*` on `--bg*` 组合；组件层如有额外背景色需额外验证 |
| **i18n 文本替换遗漏** | 高 | 低（渐进） | 不影响功能，只是有些文本仍显示中文。建议在 `t()` 中实现 `console.warn('missing key')` dev 模式，上线前 grep 逐个模块验收 |
| **组件系统增加 mental overhead** | 中 | 中 | 不强求所有组件迁移。先在 1-2 个新写组件上验证，已有组件可以保持原样。如果组件系统未带来明显收益（一致性提升），可退化到仅保留 `el()` 扩展 |
| **`el()` 扩展误将已有属性作为 setAttribute** | 低 | 中 | 对 `el('a', {href: '...'})`，如果 `setAttribute('href', ...)` 行为与 `element.href = ...` 不一致？测试验证：对于 `class`/`id`/`style`/`href`/`src`/`onclick` 等已知 keys 保持旧行为，仅 unknown keys 走 setAttribute |
| **Light Theme 开发中破坏 Dark Theme** | 中 | 高 | 主题切换通过 `data-theme` attribute 门控：`html[data-theme="dark"]` / `html[data-theme="light"]`，默认仅加载 Dark Theme。Light Theme 是附加的 CSS，不修改现有 Dark 样式 |

### 5.4 关键交付物检查清单

每个阶段完成后应验证：

**阶段 0 验收标准：**
- [ ] API client 在所有模块中替换了原始 fetch（可逐步替换，不要求一次性完成）
- [ ] access token 过期后自动 refresh，无用户可见中断
- [ ] refresh token 也过期时，干净跳转至登录页
- [ ] `el('div', {'aria-label': 'test'})` 正确设置 aria-label attribute

**阶段 1 验收标准：**
- [ ] 切换 `data-theme="light"` 后全部 UI 使用亮色配色
- [ ] 切换主题后刷新页面，设置保持不变（localStorage）
- [ ] 所有 `--text` 颜色在亮色背景上 WCAG AA ≥ 4.5:1
- [ ] `t('chat.placeholder')` 返回对应语言的文本
- [ ] 设置面板中有语言切换器
- [ ] 切换语言后，已渲染的 UI 文本更新（通过 store 订阅）

**阶段 2 验收标准：**
- [ ] 文件上传显示实时进度条
- [ ] 进度条在深色/亮色两种主题下可见
- [ ] 上传中按钮禁用（防重复提交）
- [ ] 组件系统文档化（至少 README 中有使用示例）

**阶段 3 验收标准：**
- [ ] Tab 键按逻辑顺序遍历所有可交互元素
- [ ] 焦点环在所有主题下可见（非 `outline: none` 无替代）
- [ ] 发送按钮有 `aria-label`（如语言当前为「发送」）
- [ ] 错误消息推送至 `aria-live` region

---

## 附录：架构决策记录（ADR）

### ADR-001：统一 API Client 模式

**决策**：在 `web/api-client.js` 中创建统一的 HTTP client，封装 fetch 并提供 token 管理。

**上下文**：当前 30+ 处 `fetch()` 调用散布在所有模块中，无法添加 interceptors。

**后果**：
- 正向：token refresh 一次性解决
- 正向：可添加统一错误处理、请求 ID、重试逻辑
- 负向：需要逐个模块迁移（可并行，不阻塞）

### ADR-002：组件系统使用约定而非强制框架

**决策**：不引入框架，不定义 Component 基类。组件 = 函数返回 DOM 元素 + 可选的 `Component.update()` 静态方法。

**上下文**：需要 a11y、i18n、Theme 三个方向的可组合性，但全量重写成本过高。

**后果**：
- 正向：学习成本几乎为零
- 正向：已有代码不需要改造即可使用新特性（仅 `el()` 扩展即受益）
- 负向：没有编译期验证，纯靠代码审查保持一致性

### ADR-003：CSS 变量双主题架构

**决策**：CSS 变量定义在 `:root`（暗色默认），亮色主题通过 `html[data-theme="light"]` 选择器覆写。

**上下文**：文档方向二的修正——当前已是暗色默认，需要的是亮色主题。

**后果**：
- 正向：默认加载无额外 CSS（暗色为 baseline）
- 正向：亮色主题 CSS 可延迟加载（`<link rel="preload" href="theme-light.css">`）
- 负向：需要维护两套色值，但这是双主题的必然代价

### ADR-004：i18n 使用运行时 key-value 而非编译时

**决策**：`t(key, params)` 函数运行时根据当前 locale 查找翻译文本，locale 文件为 JS 模块动态加载。

**上下文**：无构建管线，不能使用 `.po`/`.mo` 或 `.json` + loader。

**后果**：
- 正向：部署无需编译步骤，`cp` 即可
- 正向：locale 文件可独立缓存（locale/en-US.js、locale/zh-CN.js）
- 负向：key 拼写错误静默 fallback（dev 模式可 warn）
- 负向：无工具链提取未使用的 key（手动管理）

--- 

以上架构分析基于对文档证据的验证和交叉检查。核心结论：当前 web SPA 的零依赖架构在约束下是理性的选择，但缺少的几个抽象层（API Client、Store、Component 约定）构成了递进式技术债务。**建议从 token refresh 修复和 `el()` 扩展两个低风险高收益的基础设施开始，再逐步推进主题和 i18n 这两个核心产品化方向。**
