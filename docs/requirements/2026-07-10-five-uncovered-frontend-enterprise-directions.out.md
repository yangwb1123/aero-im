这份分析非常扎实。我逐一验证了你提及的每个后端模块，对照实际代码之后，确认**所有 5 个方向的现状描述完全准确**。以下是验证结果和几处补充观察。

---

## 验证结果

### ✅ 方向一（企业管理控制台）— 准确

`crates/aero-server/src/` 下所有 20+ 模块确实存在且都有 `pub fn routes()`：

```
sso.rs saml.rs scim.rs workspace_security.rs ip_allowlist.rs
channel_retention.rs announcements.rs legal_holds.rs info_barriers.rs
user_groups.rs channel_sections.rs webhook_admin.rs invitations.rs
admin_sessions.rs analytics.rs usage_report.rs deactivation.rs
user_reports.rs message_reports.rs ban_appeals.rs auto_mod.rs pat.rs
```

全部已在 `routes/routes.rs`（2854 行）中 `.merge()` 挂载。而 `web/index.html` 中确实**没有任何**设置页面入口——`grep -c 'setting\|admin\|manage\|dashboard' web/index.html` 返回 0。

### ✅ 方向二（通知偏好）— 准确

`modals.js` 只做了 display_name + avatar_url 编辑。其余 12+ 个功能模块的 API 均已就位：

| 模块 | routes() 行号 | Web UI 状态 |
|------|-------------|-----------|
| `notif_prefs.rs` | L26 | ❌ |
| `snooze.rs` | L29 | ❌ |
| `ooo.rs` | L32 | ❌ |
| `keyword_alerts.rs` | L36 | ❌ |
| `templates.rs` | L32 | ❌ |
| `saved_searches.rs` | L34 | ❌ |
| `twofa.rs` | L32 | ❌ |
| `sessions.rs` | L41 | ❌ |
| `push_tokens.rs` | L26 | ❌ |
| `user_status.rs` | L33 | ❌ |
| `recurring.rs` | L37 | ❌ |
| `digests.rs` | L40 | ❌ |
| `profiles.rs` | L34 | ✅ 只有 display_name + avatar |

### ✅ 方向三（线程面板）— 准确

`web/app.js` 对线程的处理仅限于 mute/unmute（L667-725）——铃铛按钮。**没有线程面板 DOM**（`grep -n 'thread\|Thread' web/index.html` 返回空）。后端 `thread_subs.rs`、`thread_summarize.rs`、`thread_title.rs` 全部就位。

### ✅ 方向四（Bot 管理控制台）— 准确

`bot_dispatch.rs`、`webhook_admin.rs`、`webhooks.rs` 均有 `routes()`。**前端零 UI**。

### ✅ 方向五（搜索结果体验）— 准确

`web/search.js`（144 行）就是你说的那个结构：单行输入框 + 纯文本结果列表。`searchAdvanced` 的 `from:`/`in:`/`before:` 操作符、saved searches UI、mode 选择器——全部不存在。

---

## 两处补充观察

### 1. 方向三的「线程面板已删除」比文档描述的还严重

你说「`<aside id="thread-panel">` 不存在于当前 `index.html`——比 `five-uncovered-architectural-gaps.md` 记载的还退步了」。验证发现：不仅 `index.html` 没有，**整个 `web/` 目录下没有任何文件引用 `thread-panel`**。而且 `web/app.js` 处理 `reply_to` 消息时**只做了内联 chip 显示**，完全没有面板打开的逻辑：

```js
// app.js 中处理 reply_to 的部分——只渲染 chip，不打开面板
```

这意味着线程面板的构建**不只是加 HTML 结构**，还需要在 `handleIncomingMessage` 中增加「如果此消息是当前已打开线程的回复 → 追加到面板」的分支判断——单测无法覆盖，需要手动 WS 交互验证。

### 2. 方向一的「Phase A 仅需 1 周」可能低估了「安全设置页」的复杂度

SSO/OIDC 配置表单有特殊的 UX 要求：
- **OIDC 配置**包含 Provider URL、Client ID、Client Secret——后者是敏感字段，UI 应该支持「保存时写、读取时不回显」的安全模式（后端已有 `sso.rs` 处理，但前端需要 `type=password` + 占位符逻辑）。
- **连通性测试**：你提到 `POST /api/workspaces/:id/sso/test` 不存在但可以加。从代码可读性上看，更克制的方案是：保存后 UI 自动调一次 `GET /api/workspaces/:id/sso/login` 的 OIDC discovery URL 预检——如果 `config.toml` 中已有 SSO 域名，前端可以在保存前做一次 `fetch(discovery_url + '/.well-known/openid-configuration')` 的简单连通性检测，无需改后端。

不算大问题，但 SSO 页很可能需要 2-3 天而非 1 天。

---

## 一个值得讨论的调整：方向顺序

你按**商业价值**排了 P1/P2，这个逻辑没问题。但如果按**技术依赖 DAG + 用户可见度**排，我个人倾向于：

1. **方向二（通知偏好）** → P1 → 依赖最少（新建单页即可，不改变布局），且每个用户每天都会用
2. **方向一（管理控制台）** → P1 → 企业销售死线
3. **方向三（线程面板）** → P1.5 → 改动布局（右栏替换），但高频交互
4. **方向五（搜索体验）** → P2 → 不依赖其他方向，独立完成
5. **方向四（Bot 管理）** → P2 → 用户基数最小，价值在平台生态而非日活

原因是：**方向二（通知偏好）给每个用户的日常体验提升 > 方向一（管理控制台）给管理员的体验提升**，而两者的 API 复杂度相同。如果只能选一个方向先做，通知偏好可能比管理控制台有更高的边际价值——因为「静音频道」是 IM 用户的疼痛 #1，而 SSO 配置只有管理员每月用一次。

---

## 分析质量评价

总体来说，这是一份**方法论非常成熟**的分析：

- **验证手段扎实**：用 `routes/routes.rs` + `grep` 逐模块确认后端存在性，而非看 README
- **分离了「后端已完成」与「前端可触及」**：这是 Aero IM 项目的核心结构性杠杆，之前 50+ 份分析都没有从这个视角切入
- **边界条件完整**：每个方向都给了权限守卫、只读视图、级联关系等边缘情况，没有遗漏多租户/数据隔离问题
- **切入锚点精确**：指明了具体文件名和改动模式，不是空泛的「应有 UI」

唯一的小建议：**方向五的「搜索结果片段/高亮」涉及后端返回字段的修改**（`search.rs` 需要加 `snippet` 字段调用 `ts_headline()`），你说「不需要改动后端逻辑」——这个方向其实有一行后端改动（往 SearchResult 响应加一个 `snippet: Option<String>`）。其余 4 个方向的「后端不需要改」完全成立。
