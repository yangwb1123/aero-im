已完整阅读并做了代码级验证。以下是反馈：

---

## 代码验证结果

我抽检了文档中的几个关键锚点，**全部吻合**：

| 锚点 | 验证结果 | 详情 |
|---|---|---|
| `push_tokens.rs` 只支持 FCM/APNs | ✅ 确认 | `PushPlatform` enum 只有 `Fcm`/`Apns`，无 `WebPush` variant |
| Web Push (VAPID) 零代码 | ✅ 确认 | 全库 grep `WebPush`/`VAPID`/`webpush` 无结果 |
| `commands.rs` 硬编码 built-in | ✅ 确认 | `match name { "me" \| "shrug" \| "giphy" \| "remind" \| other => Error }` |
| 交互组件无 webhook 回调 | ✅ 确认 | `interactions.rs` 只写 `block_interactions` 表 + 广播 `RoomEvent::Interaction`，无 `webhook::build_delivery` 调用 |
| Web SPA 行数 | ~5939（含 HTML/CSS） | JS 文件 ~4318 行，文档写的 ~7400 可能含 `web/` 下的其他资源配置 |

---

## 对文档本身的评价

这是目前我看到的最完整的战略分析版本。相比已存在的 `2026-07-11-strategic-expansion-directions.md`（聚焦多区域网格 + 边缘架构），你的版本**视角更产品化**，把 5 个方向排了优先级矩阵，且有明确的实现路径和工期估算，可操作性强。

### 几个值得探讨的点

**1. 方向二的 Bot 平台建议路径可以更渐进**

你说"新 crate `aero-app-platform`"——但按 AGENTS.md §4.2 的规则（`feature-first 单位是 crate`），这确实合适。不过 MVP 阶段可以**先不碰 OAuth**，直接复用 `bot_dispatch.rs` 已有的 `BotEventSubscription` + `BotDelivery` 管道：

- 先加 `POST /api/workspaces/:id/apps`（注册 webhook_url + commands[]）
- 把 `commands.rs` 的 `render_command` 扩充一个 `CommandRegistry` trait，bot 注册的命令存 DB，启动时 `load_registered_commands()`
- OAuth 授权流可以放到 V2

这样 2-3 周就能出一个可 demo 的第三方 bot 注册+slash command 扩展，而非 6 周后才见到第一个端点。

**2. 方向五：WebRTC 字幕 + 翻译已就绪但被低估**

你的缺口分析列了 6 项，但**AI 实时字幕→自动章节的时间线可以更激进**：`call_recap.rs` + `CallTranscriptRepo` 已经做 AI 复盘（文本级），`vod_chapters.rs` 有章节数据结构。缺的只是**将 transcript 按 NLP topic shift 切分喂给 `vod_chapters` 写入**的编排——这可以在 Event 实体模型还没做完时先拆出来做，作为纯 AI 管线增量，不依赖任何活动生命周期。

**3. 方向四：前端完成度可以做 "invisible" 分层重构**

你提到 `web/context.js` ~208 行是全局状态杂货铺。一个建议：先不做框架迁移（Lit/Preact 引入是架构决策），而是先做 **module boundary 合规化**——`web/polls.js` ~169 行已经是一个很好的独立模块范式（自包含 DOM 渲染 + API 调用）。可以先用 `scripts/web-check.sh` 加一条规则：新 UI 模块禁止 `import` `context.js` 的全局状态，必须通过 `ws.js` 的 `subscribe('msg:...')` 接收事件。这样渐进地收缩 `context.js` 的 scope。

---

## 你需要我做什么？

是这个文档需要：
1. **进一步代码验证**（比如查某个 gap 的具体代码位置）？
2. **生成一个可执行 roadmap**（把 5 个方向的建议路径拆成 GitHub Issues / 迁移计划）？
3. **开始实现某个方向的 MVP**（比如方向二的 slash command 扩展机制，或方向一的 Web Push 网关）？
4. **或单纯存档**——文档已保存到 `2026-07-11-strategic-expansion-analysis-mobile-bot-event-platforms.md`。

给你指示吧。
