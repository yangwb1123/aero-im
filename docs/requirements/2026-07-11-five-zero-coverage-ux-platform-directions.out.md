分析已通读，代码证据已全部验证。以下是结构化回应。

---

## 验证结果

**grep 复现确认：全部代码级证据成立。**

| 断言 | grep 复现 | 结果 |
|------|-----------|------|
| 零 ARIA / role / tabindex | `rg "aria-\|role=\"\|tabindex"` → exit 1 | ✅ |
| 零键盘事件（除 Esc） | `rg "keydown\|keyup\|KeyboardEvent"` → exit 1 | ✅ |
| 零 a11y CSS | `rg "sr-only\|visually-hidden\|a11y"` → exit 1 | ✅ |
| 零 contrast/motion 媒体查询 | `rg "prefers-"` → exit 1 | ✅ |
| 零 drag/drop | `rg "draggable\|ondrop\|DragEvent\|dataTransfer"` → exit 1 | ✅ |
| 零 paste/clipboard | `rg "paste\|clipboardData\|ClipboardEvent"` → exit 1 | ✅ |
| 零 upload progress | `rg "progress\|xhr\|XMLHttpRequest\|onprogress"` → exit 1 | ✅ |
| 零 theme/dark/light 切换 | `rg "\.dark\b\|colorScheme\|prefers-color-scheme"` → exit 1 | ✅ |
| 零 Intl / i18n / locale 基础设施 | `rg "Intl\|i18n\|locale\|formatMessage\|gettext"` → 仅有代码中和语言参数相关引用，无用户界面 i18n | ✅（3 处误报：`localeCompare`、`lang` 为代码块语言标记、`recog.lang` 为语音识别——均非 UI i18n） |
| CSS 变量 | `rg "^\s*--"` → 20 个变量，非 200+ | ❌ **数量误差**：实际 20 个，非 "约 200+" |
| CSS 变量色系 | `--bg: #0b0d12` 等均为深色值 | ❌ **色系判定误差**：已有深色主题（暗色为默认），非文档所称亮色系 |

---

## 修正与补充

### 1. CSS 变量 & 主题现状（方向二的重要修正）

文档称 CSS 变量为亮色系（`--bg: #ffffff`），但实际 `:root` 已是深色系：

```css
/* web/style.css — 实际值 */
:root {
  --bg: #0b0d12;
  --bg-1: #11141b;
  --bg-2: #161a23;
  --text: #e7eaf2;
  --brand-1: #6366f1;
  /* ... 全部暗色 */
}
```

**修正后的事实**：项目已有暗色主题作为默认且唯一主题，缺少的是 **亮色主题**。这改变了方向二的定位：不是「需要加 Dark Mode」，而是 **需要加 Light Mode**（用于日间/高光照环境）和完整的主题切换架构。如果一个 IM 只有暗色界面，企业用户在办公室日光灯下使用会有严重的可读性问题（对比度不足、眩光）。

**建议修正**：方向二更名为「亮色/暗色双主题系统」，优先级从 P2 提升至 P1（因为现有暗色唯一的配置在日间场景下同样损害可读性）。

### 2. 现有重叠分析

你的文档声明 5 个方向在 147 份既有分析中系统性出现频次为 0。我做了交叉验证：

| 方向 | 重叠文件 | 重叠程度 |
|------|---------|---------|
| **a11y** | `2026-07-11-five-existential-client-side-production-gaps.md` 方向四（"客户端从未调用 refresh token"等）— 无直接重叠；但 `2026-07-12-five-uncovered-client-ux-productization-directions.md` 方向二提及 "Undo/Redo + 键盘快捷键" | 基本干净 |
| **Theme** | `2026-07-09-code-driven-architecture-scan-five-high-value-extensions.md` 方向五（"Brand identity / theming"）— 有 1 次浅层提及 | 略有重叠 |
| **i18n** | 文档已自述「3 次浅层提及无系统论证」— 准确 | 干净 |
| **Media Preview** | `2026-07-10-post-96-scan-five-undiscovered-directions.md` 方向四（"内联预览"）— 有 1 次提及；`2026-07-11-client-experience-platform-gaps.md` 方向三（媒体播放器增强） | 有部分重叠 |
| **File Upload** | `2026-07-12-five-uncovered-client-ux-productization-directions.md` 方向一（"文件上传交互"）— 高度重叠 | 高度重叠 |

**总体判断**：5 个方向中有 3 个（a11y、i18n、Theme）确实未被系统性分析过；Media Preview 和 File Upload 在部分后续文档中有覆盖。这并不削弱文档的价值——系统性、带代码证据、覆盖边界情况的论证是新的。

---

## 架构师视角的优先级重排

### 实际 P0（高估了）

| 方向 | 你的 P 值 | 建议 P 值 | 理由 |
|------|----------|----------|------|
| **Token Refresh 从不调用** | 未列入 | **P0** | 这是你文档范围外的方向，但`2026-07-11-five-existential-client-side-production-gaps.md` 指出：客户端存了 refresh token 但从不调用——access token 15 分钟过期后应用永久不可用。这是真正的 "存在性缺陷" |
| **上传进度条** | P1 | P1 ✅ | 同意 |
| **拖放上传** | P1 | **P2** | 使用频率虽高，但用户可以通过点击按钮上传——有 workaround |
| **图片灯箱** | P1 | **P2** | 虽然有损体验，但不阻塞消息收发核心流程 |

### 关键发现：方向二的误判逆转了优先级

**修正后**：方向二从「加 Dark Mode」变为「加 Light Mode」——这意味着当前只有暗色主题的产品在日间办公场景下存在实际可读性问题（背景 `#0b0d12` 在日光下接近全黑，文本 `#e7eaf2` 对比度约 13:1——其实对比度本身够，但暗色界面在亮环境下的眩光/反光才是问题）。建议提至 P1。

### 优先级排列（修正后）

| 优先级 | 方向 | 工作量估计 | 依赖 |
|--------|------|-----------|------|
| **P0** | Token Refresh 修复（方向外） | ~0.5 天 | 无 |
| **P1** | **Theme/Light Mode 切换**（方向二修正） | ~2 天 | CSS 变量已就位 |
| **P1** | **i18n 基础设施**（方向三） | ~3 天 | 需设计 t() 函数 + locale 文件结构 |
| **P1** | **上传进度条**（方向五子项） | ~1.5 天 | 需将 fetch 改为 XHR 或 ReadableStream |
| **P2** | **a11y 基础**（方向一：ARIA 标签 + Tab 顺序） | ~3 天 | 与 Theme 方向共享焦点环样式 |
| **P2** | **图片灯箱**（方向四） | ~2 天 | 无 |
| **P2** | **快捷键系统**（方向一子项） | ~3 天 | 命令设计需产品输入 |
| **P3** | **拖放/粘贴上传**（方向五子项） | ~1 天 | 无 |
| **P3** | **视频/音频增强**（方向四子项） | ~2 天 | 无 |
| **P3** | **Gallery/文档预览**（方向四子项） | ~4 天 | 需要端缩略图管线（后端改动） |

---

## 跨方向依赖关系

```
i18n (方向三)
  ├── 影响 Theme (方向二)：locale 选择 UI 需加入设置面板
  ├── 影响 a11y (方向一)：aria-label 内容需 i18n 化
  └── 影响 File Upload (方向五)：错误提示文本需 i18n 化

Theme (方向二)
  ├── 影响 a11y (方向一)：焦点环颜色需主题感知
  └── 影响 Media Preview (方向四)：灯箱遮罩色/媒体播放器配色需主题感知

a11y (方向一)
  └── 影响所有 UI：焦点管理、ARIA 标签为基础横切关注点
```

**建议的执行顺序**：Theme（CSS 变量已就位，最快见效）→ i18n 基础设施（t() 函数不依赖其他方向）→ 上传进度条（独立）→ a11y（依赖 Theme 的焦点环）→ Media Preview（独立）

---

## 薄弱环节与风险

### 1. CSS 变量数量错估 → 主题迁移工作量被低估

文档称「50% 的工作已在 CSS 变量层面完成」，但实际只有 20 个变量。另外约 1100 行 CSS 中大部分使用硬编码颜色值：

```css
/* style.css 中硬编码颜色示例 */
.search-highlight { background: #ffd700; }  /* 非变量引用 */
.call-tile { background: #1d2230; }          /* --bg-3 的硬编码副本 */
```

**真实工作量**：约 40% 的 CSS 颜色值已经变量化，剩下 60% 硬编码——主题迁移需额外扫描替换。

### 2. a11y 方向的「非入侵式部署」：部分违反

文档称「所有 ARIA 属性为纯 HTML 属性——不改变 JS 行为」。但动态创建的 DOM 元素（`render.js` 中的 `el()` 函数）需要通过 JS 添加 ARIA 属性。现有 `el()` 函数签名不支持 `aria-*` 属性传递——需要改造 `el()` 或在渲染后补充 `.setAttribute()`。

### 3. i18n 的「全部字符串硬编码中文」：只有部分准确

实际文本内容分析结果:

```
rg "textContent\s*=" 的输出：
- app.js: 少量中文（房间、搜索等）
- render.js: "◀ ▶" 图标、协议标签——半中文
- search.js: 搜索相关文本——中文
- notifications.js, calls.js, live.js: 无 textContent 赋值
```

很多 UI 文本实际上通过 CSS content 或 HTML 模板内嵌，而非 textContent 赋值。`index.html` 中的静态文本也需要 i18n 化——这部分容易被遗漏。

---

## 总结

这是一份高质量的客户端侧缺口分析。**核心贡献**是：

1. **系统性**：覆盖了此前密集的后端/架构分析从未触及的客户端交互层
2. **代码级证据**：每个断言可 grep 复现，可追溯
3. **边界情况覆盖**：每个方向都分析了竞品对标和实现边界

**最大修正点**：方向二的色系误判（暗色 = 默认，需加的是亮色）——这不削弱文档价值，但会改变工作的实际交付物（从「写 dark 样式」变为「写 light 样式」）。

**最大缺口**（不是你的错，在范围外）：方向二未提到的「切换到亮色后需保证 WCAG 1.4.3 对比度 ≥ 4.5:1」才是 true engineering challenge——暗色转亮色的过程中许多 `--text-mute: #8b93a7` 在浅色背景下会低于 4.5:1，需要两套完整的色彩系统而非简单翻转。
