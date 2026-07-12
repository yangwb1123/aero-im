已完整通读你的战略分析文档，同时检查了代码库关键节点。以下是有数据支撑的反馈。

---

## 总体评价

这份文档对代码库穿透力很强。121 个 route 模块、126 条 `.merge()`、157 个迁移、5939 行前端 JS——你文档里引用的数字都与源码完全对齐。方向筛选也合理：避开了 AGENTS.md §4.4 禁区（联邦/MLS 客户端），识别到了真正服务端冗余而前端缺失的结构性问题。

下面按方向给出验证数据和修正建议。

---

## 方向一：移动/桌面客户端 — 数据验证 + 修正

**文档准的**：
- `aero-push` 确实有 FCM/APNs seam，无 Web Push（grep 零匹配）
- `web/index.html` 确实无 service worker 注册（grep `sw\.\|serviceWorker\|install` 零匹配）
- Push token 注册路由 ✅

**需要微调的两个点**：

1. **`web/index.html` 并非零响应式**——已经有一组基础的 responsive 代码：
   ```
   <meta name="viewport" content="width=device-width, initial-scale=1" />
   @media (max-width: 900px) { ... }
   @media (max-width: 640px) { ... }
   ```
   所以不是零覆盖，是**只有折叠侧栏级别的基本响应式，缺失 mobile-first navigation、touch 事件、底部 tab bar**。建议你把描述从"无 responsive"改为"仅桌面级 reactive layout，缺 mobile-first 重新架构"。

2. **Web Push 接入的实际成本被低估**——`aero-push` 目前的 `PushPayload` 是 provider-neutral 的，但它的 token 模型（`kind` 字段不在当前 schema 中）和服务端 push 触发机制（当前由 `push_bot.rs` 在 `im.room.*` bus 上触发）都需要适配：
   - Web Push 需要 VAPID key 对（`aero-push/certs/` 或 env 配置）
   - 需要新 `PushGateway` impl 调用 `web_push` crate（目前 Cargo.toml 无此依赖）
   - Service Worker 需要注册 `push` 事件 + `notificationclick` 事件
   - 浏览器侧需要 `Notification.requestPermission()` + `pushManager.subscribe()`
   
   总体 2-4 周仍然合理，但建议分解为 **Phase 1（SW 注册 + 权限流 + 回调 URL）** 和 **Phase 2（VAPID 签发 + 服务端投递）**。

**还缺的一个维度**：**WebSocket 在移动端的连接管理**。当前 `ws.js` 假设永久在线连接。移动端需要在 `pagehide` 时优雅关闭、`pageshow` 时重建、`visibilitychange` 时调节心跳间隔。如果要做 Mobile SPA（短期路径），这个比 CSS 响应式更关键。

---

## 方向二：Bot/App 平台 — 我最高优先级建议

**文档评估完全准确**——这是 ROI 最高的方向。补充几个你文档里没提的现有资产：

1. **`commands.rs` 已非纯硬编码**——它有 `COMMANDS` lazy static，且每个 command 实现 `async fn` handler。新 command 的注册入口已在，只是缺 OAuth/app manifest 层。

2. **`bot_dispatch.rs` + `BotEventSubscription` + `BotDelivery`** 这条链已为多租户设计——`bot_id` 字段存在，subscription 按 `event_type` 过滤，delivery 有 `status` 和 `attempts`。第三方 bot 只需注册 subscription 即可复用这条管线。**核心工作不是写新代码，是把 `BotRepo::create` 从管理 API 暴露为 OAuth app install 流**。

3. **`block_interactions.rs`** 已经处理 `Button`/`Select` 点击并写入 `block_interactions` 表 + 广播 `RoomEvent::Interaction`——但确实缺回调到 bot URL。补这环大概是最快的 2 周 win。

**建议把优先级从"6-8 周 MVP"提升到"4-6 周 MVP"**，原因是：
- 这块的代码管线（BotRepo → dispatch → delivery）已经是最成熟的扩展点
- 不需要外部依赖
- 对产品叙事的杠杆最大（从"内置 bot" → "开放平台"）

---

## 方向三：多区域/边缘部署 — 数据校正

**几个需要调整的事实判断**：

1. **NATS 单集群 ≠ 不支持跨区域**。`aero-bus` 底层是 async-nats，它原生支持 LeafNode 拓扑（`nats://leafnode:` 配置即可）。实际约束不是 NATS 能力，而是**当前 `EventBus` trait 没有 region 参数**——`publish_room_event` 硬编码 subject `im.room.{id}`，没有 region 前缀。所以工作不是换 NATS 拓扑，是加 `region` 标签层。

2. **Postgres 并非单实例**——`AERO__DATABASE__URL` 是一个连接串，但 sqlx 连接池可以用 `PgPoolOptions` 配多个 host。不过确实没有写路由层，你的判断整体成立。

3. **"Redis 单集群"** 准确。Presence 的 key 模式是 `presence:workspace:{ws_id}`，不支持 geo-shard。

4. **最关键的一点你文档没提**：`run_bus_listener` 是 **durable consumer** `aero-server`——这意味着 NATS cluster 挂了再恢复时，consumer cursor 仍在。但如果是区域间复制场景，durable consumer 在 leafnode 上行为不同（每个 leaf 有自己的 cursor）。**这需要比"4-8 周"更长的设计和测试周期**。建议把方向三标注为"10-16 周"而非"8-12 周"。

---

## 方向四：前端完成度 — 最有数据支撑的论点

我逐个 grep 验证了你列的无 UI 功能：

| 服务端模块 | 前端对应 |
|---|---|
| `canvas.rs` + `collab.rs` | ❌ `web/` 中无 `canvas`/`collab` 引用 |
| `tasks.rs` | ❌ |
| `approvals.rs` | ❌ |
| `channel_bookmarks.rs` | ❌ |
| `channel_sections.rs` | ❌ |
| `keyword_alerts.rs` | ❌ |
| `legal_holds.rs` | ❌ |
| `scheduled.rs` | ❌ |
| `recurring.rs` | ❌ |
| `templates.rs` | ❌ |
| `sessions.rs` | ❌ |
| `analytics.rs` | ❌ |
| `directory.rs` | ❌ |
| `org_chart.rs` | ❌ |
| `stream_discovery.rs` | ❌ |
| `subscription_tiers.rs` | ❌ |
| `stream_analytics.rs` | ❌ |
| `vod_chapters.rs` | ❌ |
| `message_reports.rs` / `auto_mod.rs` | ❌ |
| `ip_allowlist.rs` | ❌ |

粗略统计：121 个 route 模块，web 侧有 DOM 交互的约 30-35 个（WS 帧 handler + polls.js + calls.js + search.js + livecards.js + mentions.js 等）。**服务端功能覆盖率确实约 25-30%**，比文档说的"~40%"更保守。

**提一个战略建议**：不要逐个无声模块补 UI。应该：
1. 先给 `web/` 加一个功能目录/discoverability 层——当前 `web/index.html` 标题写着 "debug client · 联调专用"，UI 入口分散在 tab bar 和 channel header 里，用户无从发现已有功能（polls / calls / live / search 都是"藏在代码里，无导航入口"）。
2. **功能覆盖率监控**——你已有 `scripts/web-check.sh`，可以扩展为：grep `crates/aero-server/src/` 的 `pub fn routes()` → 提取模块名 → 查 `web/` 中是否有同名 `.js` 引用。输出缺失率。这会是一个自动化的 actionable 指标。

---

## 方向五：虚拟活动/网络研讨会 — 最大胆但风险最高

**现有资产验证**完全准确——直播+通话+协作+日程+投票+字幕，独立看都是生产级。但组合成"活动"产品线需要：

**一个你没提的硬依赖**：**`multistream` 支持**。当前 `LiveIngest` 是单主播→单流。虚拟活动需要：
- 多嘉宾轮流上台（切换 feed 到 HLS）
- 画中画（嘉宾同时可见）
- 屏幕共享 + 主播摄像头双流（Simulcast 已有但 UI 侧无切换控制）

`SfuMediaSession` 支持 Simulcast（`SfuForwarder` 有 `on_rtp` 重映射），但 `HlsWriter` 是单输入 TS 片段。**多流合一的 HLS 管线不存在**。

这不是说方向五不可行，而是建议在文档中标注**"Phase 1：单主播 webinar（Q&A + 注册 + 录制）vs Phase 2：多嘉宾合流"**。前者 10-14 周合理，后者需要额外 8-12 周做多源合流管线。

---

## 优先级矩阵的调整建议

| 方向 | 你的评分 | 建议调整 |
|---|---|---|
| 方向一：移动/桌面 | 🔥🔥🔥🔥🔥 | ✅ 不变，但拆分 Web Push（4周）作为前置 |
| 方向二：Bot 平台 | 🔥🔥🔥🔥 | ⬆️ **🔥🔥🔥🔥🔥** 4-6 周可达 MVP，ROI 最高 |
| 方向三：多区域 | 🔥🔥🔥🔥 | ⬇️ 🔥🔥🔥 10-16 周，技术深度高，后置到 DAU 跨区 |
| 方向四：前端补齐 | 🔥🔥🔥 | ⬆️ 🔥🔥🔥🔥 先做发现层 + 高优先级模块（画布/任务） |
| 方向五：虚拟活动 | 🔥🔥🔥🔥🔥 | ⬇️ 🔥🔥🔥 仍需多流合流管线等基础设施 |

**建议的执行顺序**：

```
Q3 (6-9月):  方向二 MVP → 方向四 发现层 + canvas/tasks UI → 方向一 Web Push
Q4 (9-12月): 方向四 持续补全 → 方向五 Phase 1 → 方向一 Tauri 壳
2027 H1:     方向五 Phase 2 → 方向三 → 方向一 原生客户端
```

---

## 文档改进建议

1. **Add: 数据佐证引用**——每个方向开头的"重要性"段如果能引用一个具体代码行数（如 `rg 'pub fn routes\(' | wc -l` = 121）会更有说服力。
2. **Add: 竞品对标**——方向二对标 Slack App（~2500 应用），方向五对标 Zoom Webinar（~300M DAU 峰值），方向一对标 Teams/Discord 移动端。产品叙事需要"XXX 有 YYY，Aero IM 可做更强因为 ZZZ"。
3. **Add: 估算依据**——"4-6 周"的依据写一句（如"BotRepo 已就绪，主要工作是 OAuth flow + interactive callback，约 3 人周"）会更可信。
4. **Minor: Hub.stream_watchers**——AGENTS.md 说 `RwLock<HashMap>`，实际代码是 `DashMap<Ulid, Vec<ParticipantId>>`。DashMap 是分片锁，对高频扇出声景更优。两个都与文档"非 Redis"一致，只是内部表示不同。

有什么想深入讨论的方向吗？比如 Bot 平台的技术方案拆解，或者前端架构评估的具体标准？
