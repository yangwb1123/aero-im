这是一个非常扎实的分析报告。我逐方向验证了你引用的代码证据并做了交叉对比——结论是**技术证据准确，但"零命中"的覆盖声明需要限定**。以下逐方向验证：

---

## 交叉验证结果

### ✅ 代码证据准确度

| 方向 | 你引用的代码证据 | 验证结果 |
|------|----------------|---------|
| **方向一：token 生命周期** | 无 `/api/auth/refresh` 路由 | ✅ 确认——routes.rs 有注释 `// Session management: access-token refresh` 但无对应 POST 路由 |
| | `api.js` 存 `REFRESH_KEY` 但不用于自动续期 | ✅ 确认——`auth.setSession()` 接受并存储 refresh，`request()` 无 401 拦截 |
| | `ws.js` 重连用 `this.token` 快照 | ✅ 确认——`_open()` 行 93 用 `this.token`（connect 时赋值，永不更新） |
| | 无 BroadcastChannel/storage 事件 | ✅ 确认——`context.js` 无 storage 事件监听 |
| **方向二：a11y** | 仅 1 处 `aria-live` 属性 | ✅ 确认——`render.js` toast 容器唯一；消息列表无 `aria-live` |
| | `openModal/closeModal` 不保存/恢复焦点 | ✅ 确认——`context.js` 行 195-196 仅 toggle hidden |
| **方向三：PWA/离线** | 无 service worker 注册 | ✅ 确认——`index.html` 无 sw 注册 script |
| | `ws.js` send 在断网返回 false 不排队 | ✅ 确认——行 160+ 检查 `readyState !== OPEN` 返回 false，无队列 |
| **方向四：文件上传** | 单次 fetch POST 整个文件 | ✅ 确认——`api.js` 行 173-176 uploadBlob 单 FormData POST |
| | 无拖放、无进度、无前端校验 | ✅ 确认——`app.js` grep 无 drag/drop listener |
| **方向五：消息导航** | 向前插入后 `scrollTop` 不调整 | ⚠️ **部分正确**——`loadHistory` 行 592-595 实际**有**滚动锚定逻辑（`pt + (nh - ph)`） |
| | 无新消息指示器 | ✅ 确认——无 IntersectionObserver |
| | 无 permalink UI | ✅ 确认——无 hash 路由处理 |

### ⚠️ "零命中"声明不准确

逐文档 grep 验证后发现以下方向**已被既有分析覆盖**：

| 你的方向 | 已有覆盖文档 | 覆盖程度 |
|---------|------------|---------|
| **方向二：a11y** | `2026-07-11-five-zero-coverage-ux-platform-directions.md` §1 | **系统性论证**——ARIA landmarks、焦点管理、对比度、键盘导航均覆盖，推荐阶段基本一致 |
| **方向三：PWA/离线** | `2026-07-10-undiscovered-client-infrastructure-directions.md` §3 | **系统性论证**——sw.js、离线队列、推送通知均有，结构高度相似 |
| **方向四：文件上传** | `2026-07-12-five-uncovered-client-ux-productization-directions.md` §1 | **系统性论证**——拖放、进度、分片、预览均覆盖 |
| | `2026-07-11-five-zero-coverage-ux-platform-directions.md` §5 | 同样覆盖拖放/进度/队列 |
| **方向五：消息导航** | `2026-07-12-five-uncovered-client-ux-productization-directions.md` §2 | **系统性论证**——滚动锚定、新消息指示器、日期跳转，与你的方向五几乎完全对齐 |

**方向一（token 生命周期）** 是唯一真正**未系统性覆盖**的方向——既有分析 `2026-07-09-five-truly-uncovered-client-side-directions.md` §3 只覆盖了**WS 建连后无定期重认证**的安全缺口，未覆盖 **refresh token 静默续期缺失、localStorage XSS 风险、多 tab 同步、登出广播**。你的分析比既有分析深入得多。

---

## 你的文档的独特贡献

1. **方向一：refresh token 从未被使用的发现**——这是真正的生产就绪缺口，现有分析均未指出 refresh_token 从服务端下发后客户端从未调用 `/api/auth/refresh`（因为该端点不存在）。结合你那句"用户登录后最多 1 小时所有 API 请求返回 401"的判断是正确的。

2. **方向一的系统性（token → WS → 多 tab → 登出广播）**——把整个 token 生命周期串起来分析，而既有分析只落在 WS 单点。

3. **方向五中「permalink 缺失弱化了搜索功能价值」的论点**——既有分析仅描述缺失现状，你点出了与搜索功能之间的**价值耦合**。

---

## 建议

是否需要我将这份文档存为文件？按命名约定建议 `2026-07-12-sixth-round-global-scan-five-client-infra-directions.md`，放在 `docs/requirements/` 目录下。

另外，如果你希望调整方向声明——把"零命中"改为**"未系统性覆盖"或"仅边缘性提及"**会更准确（方向二至五），同时方向一的"零命中"合理但建议补充指出现有分析覆盖了 WS 凭证生命周期但不包括 refresh token 静默续期层面——这样既承认既有工作又不减损你的独特贡献。
