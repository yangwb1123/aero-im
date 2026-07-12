Now I have a comprehensive view of the codebase. Here is the full Tech Lead analysis.

---

# Tech Lead 分析报告：客户端侧产品化缺口

## 1. 任务分解

将 5 个方向 + P0 Token Refresh 修复拆解为可执行的工程任务。

### 1.1 Token Refresh (方向外, P0)

| 任务 ID | 标题 | 文件 | 前置 | 工时 | 验收标准 |
|---------|------|------|------|------|---------|
| TASK-001 | 实现 `POST /api/auth/refresh` 后端端点 | `aero-server/src/auth.rs`, `aero-auth/src/` | 无 | 2h | `curl -X POST /api/auth/refresh -H "Authorization: Bearer <refresh_token>"` 返回新 `{access_token, refresh_token}`；旧 refresh token 失效（轮换） |
| TASK-002 | 客户端 `api.refreshToken()` 方法 | `web/api.js` 新增方法 | TASK-001 | 0.5h | `api.refreshToken()` 用存储的 refresh token 调用端点，返回新会话 |
| TASK-003 | 客户端 `request()` 拦截 401 自动 refresh | `web/api.js` 修改 `request()` | TASK-002 | 2h | 401 后：自动调用 refresh → 重试原请求（最多一次）→ refresh 失败则 `forceReauth()` |
| TASK-004 | WebSocket 断线重连时刷新 token | `web/ws.js` 修改 `_scheduleReconnect()` | TASK-002 | 1h | WS 连接失败若因 token 过期，重连前先 refresh token；refresh 失败不再重连 |

### 1.2 亮色/暗色双主题系统 (方向二修正, P1)

| 任务 ID | 标题 | 文件 | 前置 | 工时 | 验收标准 |
|---------|------|------|------|------|---------|
| TASK-005 | 提取全部硬编码颜色为 CSS 变量 | `web/style.css` | 无 | 3h | `grep '#[0-9a-f]\{3,8\}' style.css` 返回 0 非变量颜色（`transparent`/`currentColor`/`none` 除外） |
| TASK-006 | 编写亮色主题 CSS 变量集 | `web/style.css` 新增 `[data-theme="light"] :root` | TASK-005 | 2h | 所有变量在亮色下 WCAG 1.4.3 对比度 ≥ 4.5:1（文本）和 ≥ 3:1（大文本） |
| TASK-007 | 主题切换按钮 + localStorage 持久化 | `web/theme.js` 新文件；`web/index.html` 加 toggle | TASK-006 | 1.5h | 点击切换，所有 UI 即时重绘，刷新后记住选择 |
| TASK-008 | 跟随系统主题（可选） | `web/theme.js` 添加 `prefers-color-scheme` 监听 | TASK-007 | 1h | 系统主题变更时自动切换（用户手动选择覆盖系统） |

### 1.3 i18n 基础设施 (方向三, P1)

| 任务 ID | 标题 | 文件 | 前置 | 工时 | 验收标准 |
|---------|------|------|------|------|---------|
| TASK-009 | 设计 `t()` 函数 + locale 文件结构 | `web/i18n.js` 新文件 + `web/locales/zh-CN.json`, `en.json` | 无 | 2h | `t('login.title')` 返回当前 locale 的翻译；fallback 链 `en→zh-CN→key` |
| TASK-010 | 替换 JS 中全部 hardcoded 字符串 | `web/app.js`, `web/render.js`, `web/media.js`, `web/search.js`, `web/notifications.js`, `web/calls.js`, `web/mentions.js`, `web/modals.js`, `web/polls.js` | TASK-009 | 3h | 所有 `textContent` 赋值中用户可见文本通过 `t()` 调用；不变的是纯符号/图标 |
| TASK-011 | 替换 HTML 模板中静态文本 | `web/index.html` | TASK-009 | 1h | HTML 中所有非图标文本节点替换为 `data-i18n` 属性，由初始化脚本批量翻译 |
| TASK-012 | locale 切换 UI | `web/theme.js` 或 `web/i18n.js` 添加语言选择器 | TASK-009 | 1h | 选择语言后即时重绘所有 UI（刷新页面持久化） |
| TASK-013 | 英文 locale 文件全量填充 | `web/locales/en.json` | TASK-010 | 2h | 覆盖全部 `t()` key；英文为 native 级质量（需 native speaker review） |

### 1.4 a11y 基础 (方向一, P2)

| 任务 ID | 标题 | 文件 | 前置 | 工时 | 验收标准 |
|---------|------|------|------|------|---------|
| TASK-014 | 改造 `el()` 支持 `aria-*` 属性传递 | `web/render.js`、`web/polls.js` 修改 `el()` 函数 | 无 | 1h | `el('button', { attrs: { 'aria-label': 'close' } })` 正确设置 |
| TASK-015 | 为交互元素添加 ARIA label + role | `web/render.js` 中按钮/链接/消息列表；`web/chrome.js` 中所有 .room-item, .tab | TASK-014 | 2h | Lighthouse a11y audit 通过 (除 color-contrast 外)；NVDA 可朗读所有交互元素 |
| TASK-016 | 实现全键盘导航 (Tab 顺序 + 焦点环) | `web/style.css` 加 `:focus-visible` 样式；`web/chrome.js` 处理 Tab 顺序 | TASK-007 (主题感知焦点环) | 2h | 纯键盘可完成：切换房间→输入消息→发送→切换房间→选择文件上传→发送 |
| TASK-017 | 消息列表 aria-live 区域优化 | `web/index.html`、`web/app.js` | TASK-015 | 1h | 新消息被 screen reader 朗读；消息列表用 `aria-live="polite"` 区域 |
| TASK-018 | 模态/抽屉焦点陷阱 | `web/chrome.js` 修改 | TASK-015 | 1.5h | 模态打开后焦点 trap 在模态内；Tab/Shift+Tab 循环；Esc 关闭；关闭后焦点回到触发元素 |

### 1.5 上传进度条 (方向五子项, P1)

| 任务 ID | 标题 | 文件 | 前置 | 工时 | 验收标准 |
|---------|------|------|------|------|---------|
| TASK-019 | 改造 `request()` 支持 `onProgress` 回调 | `web/api.js` 修改；将 `uploadBlob` 改为 XHR 或用 `fetch` + `ReadableStream` 计算上传进度 | 无 | 2h | `uploadBlob(file, (pct) => console.log(pct))` 回调 0-100 |
| TASK-020 | 粘贴上传支持 | `web/media.js` 添加 `paste` 事件处理器 | TASK-019 | 1h | Ctrl+V 图片可上传 |
| TASK-021 | 进度 UI：消息内进度条 + 取消按钮 | `web/media.js`、`web/render.js` 修改 | TASK-019 | 1.5h | 上传中消息气泡底部显示带 % 的进度条；点击取消中止上传；完成自动刷新为正式消息 |
| TASK-022 | 多文件上传队列 | `web/media.js` 修改 | TASK-021 | 1h | 批量选择/拖放多文件时串行上传不丢不重排序 |

### 1.6 图片灯箱 (方向四, P2)

| 任务 ID | 标题 | 文件 | 前置 | 工时 | 验收标准 |
|---------|------|------|------|------|---------|
| TASK-023 | 灯箱组件：全屏遮罩 + 缩放/平移 | `web/lightbox.js` 新文件 | 无 | 2h | 点击消息中的图片 → 全屏灯箱打开 → 点击遮罩/Esc/关闭按钮关闭 → 支持滚轮缩放 → 触摸板平移 |
| TASK-024 | 灯箱导航：上一张/下一张 | `web/lightbox.js` 修改 | TASK-023 | 1h | 消息中有多张图片时：左右箭头切换；键盘 ← → 导航 |
| TASK-025 | 图片点击 → 灯箱事件接线 | `web/render.js` 修改 `attachment-img` 渲染 | TASK-023 | 0.5h | 点击 `img.attachment-img` 打开灯箱 |

### 1.7 快捷键系统 (方向一子项, P2)

| 任务 ID | 标题 | 文件 | 前置 | 工时 | 验收标准 |
|---------|------|------|------|------|---------|
| TASK-026 | 定义快捷键命令注册表 | `web/keys.js` 新文件 | 无 | 1h | `registerShortcut('Ctrl+K', cmdSearch)` → 焦点进入搜索框 |
| TASK-027 | 快捷键绑定 UI | `web/keys.js` + `web/chrome.js` | TASK-026 | 2h | 实现：`Ctrl+K` 搜索、`Ctrl+U` 上传、`Ctrl+N` 新房间、`Escape` 关闭面板、`Ctrl+Shift+M` 静音/取消静音 |

### 1.8 视频/音频增强 (方向四子项, P3)

| 任务 ID | 标题 | 文件 | 前置 | 工时 | 验收标准 |
|---------|------|------|------|------|---------|
| TASK-028 | 内联视频播放器 | `web/render.js` 修改 file `kind=video` case | 无 | 1.5h | 视频附件在消息中内联显示 `<video controls>`（不跳新页面） |
| TASK-029 | 视频/音频时长 + 缩略图预览 | 后端 `BlobStore` 返回元数据 | TASK-028 | 2h | 消息中文件附件显示时长和视频首帧缩略图 |

---

## 2. 执行顺序与依赖图

```mermaid
graph TD
    subgraph "Phase 1: Infrastructure (Week 1)"
        T001[TASK-001: Refresh token 后端]
        T002[TASK-002: Refresh token 客户端方法]
        T005[TASK-005: 提取硬编码颜色→CSS变量]
        T009[TASK-009: t() 函数 + locale 文件结构]
        T014[TASK-014: el() ARIA 属性支持改造]
        T019[TASK-019: request() onProgress 回调]
        T026[TASK-026: 快捷键注册表]
    end

    subgraph "Phase 2: Core Features (Week 2-3)"
        T001 --> T003[TASK-003: request() 401 自动 refresh]
        T005 --> T006[TASK-006: 亮色主题 CSS 变量集]
        T009 --> T010[TASK-010: JS textContent i18n 替换]
        T009 --> T011[TASK-011: HTML 静态文本 i18n]
        T019 --> T021[TASK-021: 进度 UI]
        T014 --> T015[TASK-015: ARIA label + role]
        T003 --> T004[TASK-004: WS 重连 token refresh]
        T021 --> T020[TASK-020: 粘贴上传]
        T021 --> T022[TASK-022: 多文件上传队列]
    end

    subgraph "Phase 3: UX Polish (Week 3-4)"
        T006 --> T007[TASK-007: 主题切换 UI + 持久化]
        T010 --> T012[TASK-012: locale 切换 UI]
        T010 --> T013[TASK-013: 英文 locale 全量填充]
        T015 --> T016[TASK-016: 全键盘导航 + 焦点环]
        T015 --> T017[TASK-017: aria-live 消息朗读]
        T015 --> T018[TASK-018: 模态焦点陷阱]
        T026 --> T027[TASK-027: 快捷键绑定]
        T007 --> T008[TASK-008: 跟随系统主题]

        T023[TASK-023: 灯箱组件] --> T024[TASK-024: 灯箱导航]
        T023 --> T025[TASK-025: 图片点击→灯箱]
    end

    subgraph "Phase 4: Media Enhancement (Week 4+)"
        T028[TASK-028: 内联视频播放器]
        T029[TASK-029: 视频时长+缩略图]
    end

    %% 并行组标注
    style T001 fill:#f9f,stroke:#333,stroke-width:2px
    style T005 fill:#bbf,stroke:#333,stroke-width:2px
    style T009 fill:#bbf,stroke:#333,stroke-width:2px
    style T014 fill:#bbf,stroke:#333,stroke-width:2px
    style T019 fill:#bbf,stroke:#333,stroke-width:2px
    style T026 fill:#bbf,stroke:#333,stroke-width:2px
```

### 可并行执行的任务组

| 组 | 任务 | 说明 |
|------|------|------|
| **G1** | TASK-001, TASK-005, TASK-009, TASK-014, TASK-019, TASK-026 | 6 个方向的基础设施无任何依赖关系，完全并行 |
| **G2** | TASK-003, TASK-006, TASK-010, TASK-015, TASK-021 | 各自依赖 G1，互不依赖，并行 |
| **G3** | TASK-007, TASK-012, TASK-016, TASK-020, TASK-022, TASK-023, TASK-027 | 依赖 G2，部分互有弱依赖 |
| **G4** | TASK-008, TASK-013, TASK-017, TASK-018, TASK-024, TASK-025 | 依赖 G3，互不相干 |

---

## 3. 技术风险

### 3.1 Token Refresh (TASK-001 ~ TASK-004)

| 风险 | 级别 | 说明 | 缓解策略 |
|------|------|------|---------|
| Refresh token 轮换时的竞态 | **高** | 并发请求同时触发 401 → 多个 refresh 调用 → 只有第一个成功，后续拿到 stale refresh → 401 循环 | 客户端锁：`_refreshing` Promise 去重；`request()` 内 await 单个 refresh 调用 |
| WS token 过期 vs HTTP token 不同步 | **中** | WS 连接使用独立 token 生命周期，可能先于 HTTP 过期 | WS 层同样实现 `_refreshBeforeReconnect()`；500ms 窗口内不重复 refresh |
| 后端 refresh 端点幂等性 | **中** | 轮换策略下客户端网络重试导致两次消费同一个 refresh token | `ON CONFLICT DO NOTHING` + `WHERE used_at IS NULL` 条件更新；已消费返回 401 不崩溃 |

### 3.2 双主题系统 (TASK-005 ~ TASK-008)

| 风险 | 级别 | 说明 | 缓解策略 |
|------|------|------|---------|
| 硬编码颜色遗漏 | **高** | 1236 行 CSS 中约 40% 颜色已变量化，60% 硬编码（约 70-100 处）可能遗漏 | 使用 `rg '#[0-9a-f]{6,8}' style.css | grep -v var` 作 checklist；对比度 CI 检测 |
| 亮色主题对比度不足 | **高** | `--text-mute: #8b93a7` 在浅色背景 `#f5f5f7` 上对比度约 4.0:1，不满足 WCAG AA (4.5:1) | 工具扫描（`accessibility-checker` 或 `axe-core`）；每个变量逐对校验 |
| JS 动态样式污染 | **中** | `el()` 的 `opts.style` 和内联样式 `element.style.xxx` 不响应 CSS 变量 | 审核所有内联 `style` 赋值；用 CSS class 或 `setProperty('--var', val)` 替代 |

### 3.3 i18n (TASK-009 ~ TASK-013)

| 风险 | 级别 | 说明 | 缓解策略 |
|------|------|------|---------|
| 遗漏文本字符串 | **中** | 约 40+ 处 `textContent` 赋值 + HTML 静态文本 + `toast()` 字符串可能遗漏 | 扫描全部 JS/HTML 中中文字符 + `t()` 包围 + CI 检查 `rg '[\x{4e00}-\x{9fff}]' web/*.js` |
| runtime branching 性能 | **低** | `t()` 被高频调用（聊天列表每条消息渲染） | `t()` 实现为 `Map.get()` 查找 + 空缓存编译；不做 gettext .mo 解析 |
| 复数/性别语法 | **低** | 英文 `1 message` vs `5 messages` 需 ICU MessageFormat 或简单复数规则 | 用 `t('n_messages', { count: 5 })` + 简单 `pluralize` 函数，不引入完整 ICU |

### 3.4 上传进度 (TASK-019 ~ TASK-022)

| 风险 | 级别 | 说明 | 缓解策略 |
|------|------|------|---------|
| `fetch` 不支持上传进度 | **中** | `fetch()` 的 `Request.body` 不可见上传进度；`XMLHttpRequest.upload.onprogress` 是浏览器 API 中唯一方案 | 对 `uploadBlob` 单独用 XHR；其他普通请求继续用 `fetch` |
| AbortController 兼容性 | **低** | 取消上传需要 AbortController | 已有 `AbortController` 使用（`REQUEST_TIMEOUT`）；复用即可 |
| 大文件内存占用 | **低** | blob 已整个在内存中，无需 streaming | 暂不处理；5MB 上限预期在业务层或 Nginx 层 |

### 3.5 a11y (TASK-014 ~ TASK-018)

| 风险 | 级别 | 说明 | 缓解策略 |
|------|------|------|---------|
| SPA 动态内容的 focus management | **高** | 路由切换（房间/频道）后焦点不自动移动到内容区 | 路由切换后 `msgList.focus()`；设置 `tabindex="-1"` 使其可编程聚焦 |
| 第三方 ESM 模块无打包器 | **中** | 原生 ESM 无法 tree-shake；a11y 辅助依赖只能通过 ES module import | 自行实现：a11y 核心需求（ARIA label + focus）不依赖第三方库 |
| VoiceOver/NVDA 兼容性 | **低** | 缺乏无障碍测试设备 | 使用 `axe-core` 自动化扫描 + 手动 NVDA 验证关键流程（登录→发消息→切换房间） |

### 3.6 灯箱 (TASK-023 ~ TASK-025)

| 风险 | 级别 | 说明 | 缓解策略 |
|------|------|------|---------|
| 触摸/手势支持 | **中** | 灯箱需支持移动端手势缩放 + 滑动 | 使用 `touch` 事件 + `gesturechange` 实现基础 pinch-zoom；`swipe` 导航 |
| 大图加载时间 | **中** | 未经缩放的原始图片可能很大 | 预加载当前图片 + 下一张，使用 `<img decoding="async">` |
| z-index 冲突 | **低** | 灯箱遮罩与模态/抽屉 z-index 层叠 | 灯箱 z-index 设在 `--z-lightbox: 10000`，高于所有模态 |

---

## 4. 资源评估

### 人力需求

| 角色 | 数量 | 技能要求 | 负责方向 |
|------|------|---------|---------|
| **Senior Rust 后端工程师** | 1 人 | Rust + sqlx + Axum；熟悉 JWT/令牌轮换 | Token refresh 后端 (TASK-001) |
| **Senior 前端工程师** | 1 人 | 原生 JS (ES2020) + CSS 变量 + 无障碍 WCAG | Theme (TASK-005~008)、a11y (TASK-014~018)、灯箱 (TASK-023~025) |
| **全栈工程师** | 1 人 | JS + 客户端框架设计 + API 对接 | i18n (TASK-009~013)、上传 (TASK-019~022)、快捷键 (TASK-026~027)、媒体增强 (TASK-028~029) |
| **前端 QA 工程师** (兼职) | 0.5 人 | Jest/Puppeteer + axe-core + 屏幕阅读器 | CI 集成测试 + 无障碍手动测试 |

### 关键里程碑

| 里程碑 | 时间 | 可交付物 | 依赖 |
|--------|------|---------|------|
| **M1** | Day 2 | Token refresh 全链路（后端 + 客户端自动 refresh）合入主干 | TASK-001~004 |
| **M2** | Day 5 | CSS 变量化完成；亮色主题 UI 可切换 | TASK-005~008 |
| **M3** | Day 7 | i18n t() 函数可用；中文→英文一次切换验证 | TASK-009~013 |
| **M4** | Day 10 | 无障碍基础：ARIA label + 键盘导航 + focus trap 验收 | TASK-014~018 |
| **M5** | Day 12 | 文件上传进度条 + 图片灯箱 | TASK-019~025 |
| **M6** | Day 14 | 快捷键 + 媒体增强；全量 e2e 验收 | TASK-026~029 |

### 阻塞点与解决策略

| 阻塞点 | 影响 | 策略 |
|--------|------|------|
| **CI 中无可访问性自动化** | M4 验收无标准 | 集成 `@axe-core/puppeteer` 到 CI；添加 `npx axe --exit` 脚本 |
| **无 i18n 第三方库** | M3 需自己造轮子 | 实现最小 `t()`：`{ let fn = keys[key] || key; return fn.replace(/{(\w+)}/g, (_,k) => params[k]) }`——不引入 polyglot/ICU |
| **无测试基础设施** | 全局 | 见 §5 质量保证 |

---

## 5. 质量保证

### 5.1 单元测试覆盖要求

| 域 | 文件 | 测试框架 | 最低覆盖率 | 关键测试点 |
|----|------|---------|-----------|-----------|
| i18n | `web/i18n.js` | 原生 JS（在 Node 用 `node --test`） | 90%+ | `t()` fallback 链；参数替换；缺失 key 不 panic |
| Theme | `web/theme.js` | Puppeteer + Node | 85%+ | 切换 class 正确设置；localStorage 持久化；系统主题监听 |
| Lightbox | `web/lightbox.js` | Puppeteer | 80%+ | 打开/关闭；← → 导航；Esc 关闭；缩放不溢出 |
| Upload | `web/api.js` (uploadBlob 分支) | Puppeteer (mock XHR) | 80%+ | 进度回调 0→100%；取消中止；错误回退 |
| Token Refresh | `web/api.js` | Puppeteer (mock fetch) | 90%+ | 401→refresh→retry；refresh 失败→forceReauth；并发去重 |
| a11y | — | `axe-core` (e2e) | 无覆盖数字 | 通过 axe-core 全部 rules（`#toast-stack` 已知 false positive 可 `exclude`） |

### 5.2 集成测试策略

| 测试类型 | 工具 | 范围 | 环境 |
|---------|------|------|------|
| **Lighthouse CI** | `@lhci/cli` | 所有 HTML 页面：a11y 分数 ≥ 90 | PR preview / staging |
| **Axe-core 扫描** | `@axe-core/puppeteer` | 所有路由（auth → chat → call → live → polls） | CI (headless Chromium) |
| **键盘导航 E2E** | Puppeteer | Tab 遍历关键路径：登录 → 选房间 → 发消息 → 切换房间 | CI |
| **主题渲染回归** | Puppeteer screenshot diff | 暗色/亮色下所有 UI 截图对比基准 | CI |
| **Token refresh E2E** | Puppeteer + mock 后端 | access token 过期后自动刷新不弹登录 | CI（mock 15s TTL） |

### 5.3 代码审查要点

| 审查要点 | 关注方向 | 具体检查项 |
|---------|---------|-----------|
| **CSS 变量完整性** | Theme | 新增颜色值是否使用 `var(--xxx)`；亮色/暗色集是否成对出现 |
| **i18n 全覆盖** | i18n | diff 中新增中文字符串是否被 `t()` 包围；`rg '[\x{4e00}-\x{9fff}]'` 为 0 |
| **ARIA 属性正确性** | a11y | `role` 是否匹配语义；`aria-label` 是否本地化（需在 TASK-015 后补）；`aria-live` 区域行为是否合理 |
| **不退化原则** | All | 打开灯箱时不应改变消息列表 z-index；主题切换时不应有闪烁的 `transition` |
| **竞态条件** | Token Refresh | refresh 请求是否去重；多 tab 是否冲突（`window.addEventListener('storage')` 同步） |

### 5.4 性能测试需求

| 测试 | 场景 | 阈值 |
|------|------|------|
| i18n 初始化时间 | 首次加载 `t()` 函数 + locale JSON | < 50ms |
| 主题切换重绘 | 亮色→暗色 | < 16ms (60fps) |
| 灯箱打开延迟 | 点击→灯光箱显示 | < 200ms (图片加载不计) |
| 上传进度回调频率 | 1MB 文件 | 回调间隔 ≤ 200ms（通过 XHR `onprogress` 保证） |
| Keyboard a11y | 按 Tab 循环 10 个焦点元素 | < 100ms（无 JS 阻塞） |
| CSS 变量查询性能 | `var(--xxx)` 被 200+ 选择器引用 | 无显著重绘延迟（Chrome DevTools Performance 检查） |

---

## 6. 实施计划

### 阶段 1: 基础设施搭建（Day 1–2）

```
Day 1    Day 1.5  Day 2
┌────────┬────────┬────────┐
│ T001   │ T002   │ T003   │  ← Token Refresh (并行: 1 人)
│ Refresh│ Client │ 401    │
│ 后端    │ method │ auto   │
├────────┼────────┼────────┤
│ T005   │ T006   │        │  ← Theme (并行: 1 人)
│ CSS var│ Light  │        │
│ 提取    │ set    │        │
├────────┼────────┼────────┤
│ T009   │ T010   │ T011   │  ← i18n (并行: 1 人)
│ t()    │ JS     │ HTML   │
│ 设计    │ 替换   │ 替换   │
├────────┼────────┼────────┤
│ T014   │        │        │  ← a11y (同一人, Day 1)
│ el()   │        │        │
│ ARIA   │        │        │
├────────┼────────┼────────┤
│ T019   │        │        │  ← Upload (同一人, Day 1-2)
│ onProg │        │        │
│ -ress  │        │        │
└────────┴────────┴────────┘
  G1                     G2 (partial)
```

**Day 1 交付**：6 个并行基础设施任务同时启动。1 人 Rust 后端 (T001)；2 人前端 (T005, T009, T014, T019)。

**Day 2 交付**：T002(refresh client method) → T003(401 auto refresh) 完成。T006(亮色变量) 完成。T010(JS i18n) 50% 完成。

### 阶段 2: 核心功能实现（Day 3–5）

```
Day 3    Day 4    Day 5
┌────────┬────────┬────────┐
│ T004   │        │        │  ← Token Refresh 收尾
│ WS     │        │        │
│ refresh│        │        │
├────────┼────────┼────────┤
│        │ T007   │ T008   │  ← Theme 收尾
│        │ Toggle │ System │
│        │ UI     │ pref   │
├────────┼────────┼────────┤
│ T012   │ T013   │        │  ← i18n 收尾
│ locale │ EN     │        │
│ switch │ 填充    │        │
├────────┼────────┼────────┤
│ T015   │ T016   │ T017   │  ← a11y
│ ARIA   │ 键盘    │ aria-  │
│ label  │ nav    │ live   │
├────────┼────────┼────────┤
│ T021   │ T020   │ T022   │  ← Upload
│ 进度UI │ 粘贴   │ 多文件 │
├────────┼────────┼────────┤
│ T023   │ T024   │ T025   │  ← Lightbox
│ 组件   │ 导航    │ wiring │
├────────┼────────┼────────┤
│ T026   │ T027   │        │  ← Shortcuts
│ 注册表  │ 绑定   │        │
└────────┴────────┴────────┘
```

**Day 5 验收点 (M2 M3)**：亮色主题可切换 + i18n 英中切换 + 灯箱可用。

### 阶段 3: 集成测试和优化（Day 6–8）

```
Day 6    Day 7    Day 8
┌────────┬────────┬────────┐
│ T018   │        │        │  ← a11y 收尾
│ Focus  │        │        │
│ trap   │        │        │
├────────┼────────┼────────┤
│ T028   │ T029   │        │  ← Media Enhancement
│ 内联   │ 缩略   │        │
│ video  │ 图     │        │
├────────┼────────┼────────┤
│        │        │        │
│ E2E    │ Axe    │ Perf   │  ← 测试阶段
│ Suite  │ scan   │ audit  │
│ Puppet │ CI 集成│        │
├────────┼────────┼────────┤
│ CR     │ CR     │ CR     │  ← Code Review 全员
│ Pass 1 │ Pass 2 │ Pass 3 │
│ i18n   │ Theme  │ a11y   │
└────────┴────────┴────────┘
```

**Day 7 验收点 (M4)**：无障碍基础通过 axe-core CI 扫描（除 color-contrast——因亮色对比度修复在 M2）。

### 阶段 4: 发布准备（Day 9–10）

```
Day 9    Day 9.5   Day 10
┌────────┬────────┬────────┐
│ 回归   │ Bugfix │ 发版   │  ← 全团队
│ 测试   │ Sprint │ v2.1.0 │
│ 全部   │ 收尾   │        │
├────────┼────────┼────────┤
│ 本地化 │ QA     │        │
│ Review │ Sign-  │        │
│ (EN)   │ off    │        │
└────────┴────────┴────────┘
```

**Day 10 验收点 (M5 M6)**：全量功能可用 + CI 全绿 + 团队 sign-off。

---

## 总结工作量

| 阶段 | 时间 | 总工时 | 并行人数 | 预估人员 |
|------|------|--------|---------|---------|
| Phase 1 | Day 1–2 | 24h | 3 人（1 后端 + 2 前端） | 24 person-hours |
| Phase 2 | Day 3–5 | 56h | 3 人 | 56 person-hours |
| Phase 3 | Day 6–8 | 24h | 3 人（含测试） | 24 person-hours |
| Phase 4 | Day 9–10 | 16h | 3 人（含测试 + QA） | 16 person-hours |
| **总计** | **10 天** | **120h** | **3 人** | **120 person-hours** |

### 风险缓冲

| 风险区域 | 缓冲天数 | 说明 |
|---------|---------|------|
| 对比度修复 | +1 天 | 亮色主题下需逐个变量调色，可能反复 |
| a11y 手动验收 | +1 天 | NVDA/ VoiceOver 手动测试不可跳过 |
| token refresh 多 tab 同步 | +0.5 天 | `window.addEventListener('storage')` 跨 tab 同步 token |
| **总缓冲** | **+2.5 天** | **12.5 日历天，若仅有 1 人则增至 25 天** |

### "一人全包" 压缩方案（如果只有 1 人）

| 阶段 | 时间 | 说明 |
|------|------|------|
| Day 1–2 | TASK-001~004 (token refresh) | 最短路径消除 P0 缺陷 |
| Day 3–5 | TASK-005~008 (theme) | CSS 变量化 + 亮色主题 |
| Day 6–8 | TASK-009~013 (i18n) | t() + 中英切换 |
| Day 9–11 | TASK-023~025 (lightbox) + TASK-019~022 (upload progress) | 两个独立方向 |
| Day 12–14 | TASK-014~018 (a11y) + TASK-026~027 (shortcuts) | 键盘 + a11y |
| Day 15–16 | TASK-028~029 (media) + 集成测试 + 修复 | 
| **总计** | **16 天** | **单人全栈，依次顺序执行** |

若仅 1 人，**必须裁剪范围**：建议砍掉 TASK-028/029（视频增强，P3）、TASK-026/027（快捷键，P2 低）、TASK-020/022（粘贴上传和多文件，P3），保留 P0–P1 的 token refresh + theme + i18n + progress bar + lightbox + a11y 基础（ARIA label + keyboard nav）。
