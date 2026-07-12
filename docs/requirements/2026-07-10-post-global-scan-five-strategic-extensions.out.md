`livecards.js`（第 10–39 行）也有 `submitBlockInteraction` 函数，`api.js` 有 `interactBlock()` 方法。

**影响**：方向 ③（Bot/集成平台）的缺口清单里「交互式消息组件 UI」这一项应该标 ✅ 已有，而不是 ❌ 缺失。这是个低级扫描遗漏——`grep` 模式可能只搜了 HTML 没搜 JS。

---

## 需要修正的描述（准确但偏颇）

### 问题 1：Bot SDK 入口的描述

> *"Bot SDK 文档/注册引导 — 无 `/api/bot-sdk` 路由"*

虽无 `/api/bot-sdk` 命名的路由，但 `/api/bots` **有完整的 CRUD**：

```
POST /api/bots                  → bot_create
GET  /api/bots                  → bot_list
POST /api/bots/:id/token        → bot_rotate_token
GET  /api/bots/:id/subscriptions → bot_list_subscriptions
POST /api/bots/:id/subscriptions → bot_create_subscription
DELETE /api/bots/:id/subscriptions/:sub_id → bot_delete_subscription
GET  /api/bots/:id/deliveries   → bot_list_deliveries
```

建议改为：**「Bot SDK 文档/注册引导缺少 SDK 封装层和开发者文档」**——API 存在，包装（客户端 SDK + 引导文档）没有。

### 问题 2：Slash command 的表态

> *"无 /api/commands 自定义注册路由"*

`/api/commands` 存在但只列出内置命令。这个描述本身没错，但可以和 bot 平台合并考虑——自定义 slash command 本质上是 bot subscription + command handler 的组合，不需要独立的 `/api/commands` 注册端点（Slack 也没有，slash command 属于 app manifest）。

---

## 对方向本身的补充观察

### 方向一（邮件产品化）—— 自洽且诚实

mailer.rs 125 行的状态准确，HTML 模板引擎依赖确为零，DKIM 为零。P0 → P2 的优先级划分合理。**但有个没说出来的问题**：Reply-by-Email（P2 最大项）需要邮件网关 `POST /api/email/inbound` 端点暴露在公网，当前项目无任何公网 Webhook receiver 的实现模式——这是新架构模式，不是简单加一个路由。

### 方向二（前端工程化）—— 最强的分析

你正确地识别了这不是「缺某些 UI」而是「整个工程层缺失」。CSS 变量系统、i18n、虚拟列表这几个点抓得很准。**但有一个策略问题没说清楚**：当前是「零依赖 ES2020」，方向二第一步就要引入 Vite（强制 `package.json`、`node_modules`、构建步骤）。这会破坏一个重要的设计约束。建议在文档里明确写上「经评估，零依赖约束在这个阶段是产品的瓶颈，决定牺牲它以获得工程化收益」。

### 方向四（Webhook 出站签名）—— 完全正确

`rg "sign\|hmac\|sha256" crates/aero-storage/src/webhook*` 返回零。这是最容易被忽略的安全缺口——文档建议 2-3 天的体量也合理。**值得考虑**在这个文档中把它从 P2 提到 P1，因为要做 bot 平台（方向三），签名是开发者信任的先决条件。

### 方向五（多区域）—— 诚实但保守

你说「当前阶段投入产出比最低」——我同意。但有两个前提没说清楚：

1. **NATS JetStream 不支持跨区域**这个约束很重要——你提到了「区域独立 NATS 集群 + 桥接」，但没说这个桥的线性一致性能否保证房间内消息顺序。如果 write 在区域 A，read 在区域 B，seq 单调性会碎。
2. **Call-bridge** 在 AGENTS.md 里标注了 `TODO(real-transport)`——一个连单区域跨节点媒体都没跑通的系统，跨区域通话在可预见未来是不可行的。应该在文档里明确说「多区域阶段 1 不包含实时媒体」。

---

## 总结：修改建议清单

| 修改项 | 位置 | 建议 |
|--------|------|------|
| ❌ incoming webhook 不存在 | 方向③/方向四缺口表 | 删除此条目；改为「已有 incoming webhook，缺 OAuth2 授权流和 UI」 |
| ❌ SPA 未渲染交互式 Block | 方向③交互式消息组件条目 | 改为 ✅ 已有，render.js 220-260 已覆盖 Button/Select |
| ⚠️ bot SDK 说成无路由 | 方向③ Bot SDK 条目 | 改为「API 存在，缺 SDK 封装和开发者文档」 |
| ⚠️ 未讨论零依赖牺牲 | 方向二 | 加一段关于破坏零依赖约束的设计决策 |
| 补充：NATS 跨区域约束 | 方向五 | 加注 NATS 桥的线性一致性风险 |
| 补充：call-bridge 状态 | 方向五 | 明确跨区域实时媒体不可行 |

总的来说，**这是一份很好的分析**。事实错误集中在你已熟悉的领域之外（webhook、交互式 Block）——我怀疑你的 grep 扫描路径或关键词漏了这些文件。方向选择和优先级判断的质量很高，修正上述几点后可以直接作为团队 roadmap 文档使用。
