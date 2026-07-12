Now let me compile my full code-verified assessment.

---

## 代码验证报告：第七轮缺口扫描文档

我对 `2026-07-09-truly-uncovered-gaps.md` 进行了逐项代码验证。以下是详细结果。

---

### 方向一：WebSocket 帧缺口

**✅ 核心论断正确** — `ServerFrame::MessageSeen`（`frame.rs:56`）和 `ServerFrame::Interaction`（`frame.rs:59`）在后端活跃发送，`msg:message_seen` 与 `msg:interaction` handler 在 `app.js:87-124` 的 `hookWs()` 中不存在。`msg:poll` handler 注册在 `polls.js:47` 而非 `app.js`，存在模块加载失败时的静默丢失风险。

**⚠️ 有误细节**：Web SPA 共注册的 handler 为 16 个（文档说 16 种 ServerFrame variant），但 `hookWs()` 实际注册 16 行 `ws.on(...)` 调用（`app.js:105-124`），其中 `msg:pong` 有一行空 handler `() => {}`。方向一的核心论点完全成立。

---

### 方向二：a11y 可访问性

**⚠️ 核心论点方向正确，但有三处事实错误**：

1. **"零 ARIA 属性"** 不准确。`index.html` 已有：
   - `role="tablist"`（行 23）
   - `role="tab"`（行 24-25）
   - `aria-live="polite"` 在 `#toast-stack`（行 12） — 文档自己说「零 `aria-live`」与此矛盾。

2. **"Muted color #888 第 42 行"** 有误：
   - 实际定义：`--text-mute: #8b93a7`（`style.css:10`）
   - 实际使用：`.muted { color: var(--text-mute); }`（`style.css:40`）
   - 颜色值 `#8b93a7`，非 `#888`

3. **a11y 缺失程度描述准确**：焦点管理、键盘导航、`aria-label` 确实为零。整体论断（ADA/Section 508 合规为空白）正确。

---

### 方向三：i18n 骨架

**⚠️ 一处事实错误**：

1. **"`<html>` 标签无 `lang='zh-CN'`"** 不成立。`index.html:2` 明确声明 `<html lang="zh-CN">`。

2. **其余论断正确**：
   - 硬编码中文字符串：已验证 `app.js`、`polls.js`、`notifications.js`、`search.js` 等确实全部硬编码
   - 零 `Intl.*` API 使用：`grep` 确认仅 `calls.js:298` 使用了 `navigator.language`（用于语音识别语言选择），`Intl` 在业务代码中零出现
   - 零 RTL 支持：`dir="rtl"` 不存在
   - 手动 `formatHM`（`render.js:102`）和 `formatTime`（`context.js:172`）未使用 `Intl.DateTimeFormat`

---

### 方向四：错误状态处理

**✅ 高度准确**。catch 模式分析匹配源代码：

| 文件 | catch 数 | 静默吞异常数 | 文档正确 |
|------|---------|-------------|---------|
| `app.js` | 15 | 5 | ✅ |
| `polls.js` | 4 | 1 | ✅ |
| `notifications.js` | 4 | 1 | ✅ |
| `search.js` | 2 | 0 | ✅ |
| `live.js` | 2 | 0 | ✅ |
| `ws.js` | 3 | 2 | ✅ |

静默 `/* ignore */` 的准确位置：
- `app.js:83` — `catch (e) { /* ignore */ }`
- `app.js:150` — `catch { return; }`（`pullRoomSince`）
- `app.js:337` — `catch { /* a failed replay... */ }`（`replayChanges`）
- `app.js:562` — `.catch(() => {});`（`listReceipts`）
- `app.js:606` — `.catch(() => {});`（`reactionsBatch`）

❌ **`ws.send` 返回值检查**：文档称 `ws.send()` 在 `readyState !== OPEN` 时返回 false 且 `app.js` 不检查。已验证 `ws.js` 的 `send()` 方法 — 发前确检查 `this.ws.readyState === WebSocket.OPEN` 并返回 `false`。`app.js` 中 `ws.sendMessage` 调用方（`submitComposer`、消息反应、typing 等）均未检查返回值。**核心论点成立。**

---

### 方向五：客户端状态持久化

**✅ 高度准确**：

- 所有 state 存储在内存 Maps（`context.js` 行 41-59）：`rooms`、`messagesByRoom`、`unreadByRoom`、`reactionsByMsg`、`receiptsByRoom`、`pendingByTempId`、`typing`、`threadMuted` 等
- 唯一持久化：JWT token（`localStorage`，`api.js:10-21`）+ AI 会话历史（`sessionStorage`，`search.js:110-123`）
- 刷新后请求瀑布精确：`api.me()` → `ws.connect()` → `api.listRooms()` → `api.rtcConfig()` → `api.liveGifts()` → `refreshNotifBadge()`
- IndexedDB 使用：零

---

### 核心问题：「既有分析均未覆盖」的论断不成立

逐方向核查既有文档后，**5 个方向中的 4 个已在既有分析中覆盖**：

| 方向 | 既有覆盖 | 证据 |
|------|---------|------|
| **方向一：WS 帧缺口** | ✅ **已覆盖** | `code-verified-gaps.md` §2.4 已列出 MessageSeen/Interaction 为缺失 handler，并建议添加。方向一与此高度重合 |
| **方向二：a11y** | ✅ **已覆盖（本文件自身）** | 本文件 `truly-uncovered-gaps.md` §2 即为该内容。同时 `five-critical-gaps-final-scan.md:22` 列出 a11y 为方向 |
| **方向三：i18n** | ✅ **已覆盖（本文件自身）** | 本文件 `truly-uncovered-gaps.md` §3 即为该内容 |
| **方向四：错误处理** | ⚠️ 部分覆盖 | 多个文档提及但无独立方向；方向四的深度和系统性是新增的 |
| **方向五：状态持久化** | ✅ **已覆盖** | `five-truly-uncovered-client-side-directions.md` §1 已完整覆盖「SPA 状态持久化真空—页面刷新即空」，包括状态清单和刷新瀑布分析 |

文档开头的「既有分析覆盖全景」表中缺少以下条目：
- **WebSocket 帧消费缺口**（`code-verified-gaps.md` §2.4）
- **客户端可访问性 a11y / 国际化 i18n**（方向二/三 — 存在于本文件后续段落中，不应在「均无雏形」的列表里）

---

### 其他事实性修正

| 文档原文 | 实际值 | 影响评级 |
|---------|--------|---------|
| 颜色 `#888` (第 42 行) | `#8b93a7` (CSS 变量行 10, 使用行 40) | 低 |
| "style.css 的 1236 行" | 行数准确 (`wc -l` = 1236) | ✅ |
| "无 `lang` 属性" | `<html lang="zh-CN">` 存在于 `index.html:2` | 中 |
| "零 ARIA 属性" | `role="tablist"`, `role="tab"`, `aria-live="polite"` 存在 | 中 |
| `hookWs` handler 统计 | 16 行准确（含 `msg:pong` 空 handler） | ✅ |
| 刷新后 7 请求瀑布 | 6 个 REST（含 refreshNotifBadge）+ WS 连接 | ⚠️ 部分 via Notif endpoint |

---

### 总结

| 评估维度 | 结果 |
|---------|------|
| **分析质量** | 高 — 代码细节准确（catch 统计、状态清单、帧缺口） |
| **新颖性** | ⚠️ 4/5 方向已在既有文档覆盖。方向四（错误处理系统化）是唯一真正新增的独立方向 |
| **有误事实** | 3 处（lang 属性、ARIA 是否为零、颜色值） |
| **修复体量评估** | 合理 — S/M/L 分级与排期建议务实 |
| **最大价值方向** | 方向四（错误处理）和方向一（WS 帧缺口）——后者虽非「未覆盖」但确为高影响低成本的快速收益项 |
