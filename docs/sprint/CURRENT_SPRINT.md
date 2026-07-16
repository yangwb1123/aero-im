# CURRENT_SPRINT.md — 当前 Sprint 目标与任务看板

> 这是 Agent 的任务看板。
> 每次 Agent 启动时读此文件决定下一步做什么。
> 完成一个任务后，将 `[ ]` 改为 `[x]` 并更新下方进度。

## Sprint 元信息

- **Sprint 编号**: S1（2026-06-15 ~ 持续）
- **状态**: 🟢 功能开发阶段（Phase 1 REFACTOR 已完成 ✅）

---

## 🟢 Phase 1: REFACTOR — 全部完成

```
📊 重构进度:
   14/14 已完成
   ██████████████ 100%

所有 HARD 违规文件已拆分。最后一步：routes/health.rs 路由提取完成。
```

## 🟢 Phase 2: 功能开发

### P0: 通知聚合摘要 — ✅ 全部完成

| 状态 | 任务 |
|------|------|
| [x] | 新增 notification_bundles 表（迁移 0143） |
| [x] | 通知插入点加延迟聚合逻辑 |
| [x] | push_bot 注入 collapse_key / apns-collapse-id |
| [x] | WS 重连回放压缩 (?summarize=true) |
| [x] | web/app.js JS 模块拆分（998 行，降到了 <1000） |

### P0: 索引瘦身 — ✅ 全部完成

| 状态 | 任务 |
|------|------|
| [x] | GIN partial index (migration 0136) |
| [x] | HNSW partial index (migration 0136) |
| [x] | 索引膨胀 Prometheus gauge |

### P1: 富文本编辑 — ✅ 全部完成

| 状态 | 任务 |
|------|------|
| [x] | 服务端 Markdown 解析器 (markdown.rs, 已接入 WS send_message 管线) |
| [x] | Span 合法性校验（嵌套深度）|
| [x] | Web 前端 span 渲染 (render.js appendTextWithSpans) |
| [x] | 搜索高亮 ts_headline (headline 已加入 search/search_advanced 响应) |

### P1: 多级缓存 — ✅ 全部完成

| 状态 | 任务 |
|------|------|
| [x] | Participant profile 本地缓存 (participant_cache.rs, DashMap + TTL) |
| [x] | Room membership 批量预取 (room_member_cache.rs, get_or_fetch) |
| [x] | 缓存命中率指标 (metrics.rs, PARTICIPANT_CACHE_LOOKUPS_TOTAL) |

### P0: 邮件通讯渠道 — ✅ 全部完成

| 状态 | 任务 |
|------|------|
| [x] | mailer.rs: SMTP 发送基础设施 (lettre) |
| [x] | 密码重置邮件 (sessions.rs:360) |
| [x] | 邀请邮件 (invitations.rs:222) |
| [x] | EmailConfig + build_mailer 装配 (state_builder.rs + main.rs) |

### P1: 安全响应头 — ✅ 全部完成

| 状态 | 任务 |
|------|------|
| [x] | X-Frame-Options: DENY |
| [x] | X-Content-Type-Options: nosniff |
| [x] | Strict-Transport-Security |
| [x] | Referrer-Policy |
| [x] | Permissions-Policy |
| [x] | CSP (opt-in via AERO_CSP_POLICY) |

### P1: 搜索高亮 & 路由拆分 — ✅ 全部完成

| 状态 | 任务 |
|------|------|
| [x] | headline 字段加入 /api/search 和 /api/search/advanced 响应 |
| [x] | health 路由提取到 routes/health.rs（routes.rs 3002→2848 行） |

---

## Phase 3: 能力扩展（下一阶段）

> Phase 2 在功能层面已基本完成。以下候选方向供下一 sprint 选取。

### P2: 开放平台（Bot + App SDK）— ✅ 已完成（本 sprint 复核发现早前已建好，未在此文档同步）

| 状态 | 任务 | 备注 |
|------|------|------|
| [x] | Bot 注册与 token 管理 | `bots`/`bot_event_subscriptions` 表（mig 0141/0142），`aero-storage/src/bot.rs::BotRepo`，`POST /api/bots` + `POST /api/bots/:id/token` |
| [x] | Bot API 端点 | routes.rs `bot_create`/`bot_list`/`bot_rotate_token`/`bot_list_subscriptions`/`bot_create_subscription`/`bot_delete_subscription`/`bot_list_deliveries`，均 `ensure_bot_owner` 所有权校验 |
| [x] | 事件订阅细化 | `subscribe`/`list_subscriptions`/`delete_subscription`/`subscriptions_for_event`，JSONB `filters`（room/workspace/action_id），`bot_dispatch.rs` 按订阅分发，投递日志见 mig 0147 |

复核时发现并修复一个真实 SSRF 漏洞：`bot_create_subscription` 此前对用户提供的 `webhook_url` **零校验**，而 `bot_dispatch.rs` 会对其发起服务端 HTTP 请求——任意已认证用户都可以注册 bot 并订阅一个指向内网/云元数据地址的 webhook。已接入与房间级 outgoing webhook 相同的 `assert_webhook_url_safe` 防护并现场验证（loopback/元数据地址 400，公网 URL / 无 URL 均成功）。

### P2: 分片扩展

| 状态 | 任务 | 预估 |
|------|------|------|
| [x] | Redis 热键分片（presence/viewers 256 分片，live_presence.rs + presence.rs） | L |
| [x]（准备 + cutover 脚本均已验证，执行本身留给维护窗口） | 消息表自动分区 — shadow 表 + 回填/维护函数（mig 0148）+ 已验证的 cutover 脚本（`docs/runbooks/messages-cutover.sql`） | XL |

复核 `docs/runbooks/messages-partitioning.md` §5a 的 6 月验证记录时发现：迁移 `0157`（本 sprint 新增的 `messages.version` 乐观锁列）晚于 `0148` 建好 shadow 表，而 `LIKE messages INCLUDING DEFAULTS` 是建表那一刻的快照、不会自动跟进后续新列——`messages_partitioned` 和 `backfill_messages_partition` 因此从未携带 `version`。若不修，真正执行 cutover 会让每条曾被编辑过的消息 version 静默重置为默认值 1，导致客户端记住的 `expected_version` 永久失配、编辑一律 409。已修复（`migrations/0158_messages_partition_shadow_version_column.sql` + 更新 `messages-cutover.sql` 的最终同步列表)，并在全新 throwaway DB（158 条迁移链，不是共享 `aero` DB）上端到端重新验证：种入一条 version=5（模拟 4 次真实编辑）的消息 → 回填 → 跑 cutover 脚本 → 确认 cutover 后 version 仍为 5（未被重置），以及 6 月记录的全部检查项（行数对齐、8 条 FK 校验通过、7 张子表 0 孤儿、级联删除、FTS、分区裁剪）在当前 schema 下依然全部通过。**实际执行仍需真实维护窗口 + product/ops 批准 + 已测试的备份**——这是刻意的部署期操作，不是（也不应该是）sandbox 里能单方面"执行"的代码任务；但"脚本本身是否正确"这一层已经完整验证，不再是未验证的假设。

### P2: 直播媒体面生产接线 — 移出本 sprint 范围（环境/设计边界，非遗漏）

复核前先假设"文档写 `[ ]` 就是没做"，结果两次都错了（Bot 平台、分区 cutover 脚本其实都已完整实现）。这次逐项深挖代码后确认：这两项这次是**真的**卡在需要人工设计决策或真实基础设施，不是"文档过期"的第三次重演。逐项证据：

- **`CallBridge::ensure_egress` 跨节点生产接线**：`call_bridge_supervisor.rs` 自身文档写得很清楚——registry、spawn-on-`BridgeTo`、幂等去重、leave 时取消，全部是真实代码且被单元测试覆盖（对 `FakeCallUpstream`/`LoopbackUpstream`）。唯一留白的是 `UpstreamFactory`：为对端节点 URL 建立一条真实的跨节点 RTP 拉取连接（SDP recvonly 交换 + ICE/DTLS/SRTP + UDP）。这一段结构上就需要**另一个真实节点**去连接和验证——单节点 sandbox 里无法有意义地实现或测试它，不是造 WebRTC 造得不够多的问题（`aero-live-webrtc` 本身已有 7500+ 行经测试的编解码器/RTCP/simulcast/BWE 实现）。

- **`SfuMediaSession` bind+run 生产接线**：`bind()`/`accept_offer()`/`run()` 本身是完整实现且有单元测试（`sfu_media.rs`）；但深挖调用方发现 `SfuMediaSession::bind` **在全代码库零调用**——`join_group_call()` 只把参与者登记进 `SfuRouter`（记账），真正建立媒体连接这一步从未发生；`ClientFrame::CallOffer` 至今仍走纯 P2P 中继（`relay_call_event`），也就是说无论 DB 里 `call_mode` 标不标 `Sfu`，**当前运行时的实际路径是全网状 P2P**，服务端从未真正终结过一路媒体。把它接上意味着：(a) offer 要路由到 `SfuMediaSession` 而不是转发给对端——这一半是纯后端接线，可以做；(b) 但 web 前端（`calls.js`，639 行）目前假设的是"直接连其他浏览器的 ICE candidate"，要切到"连服务端 socket 的 ICE candidate"是一次同等量级的前端改造，不是本次顺手能带的小改动；(c) 真实媒体流转（ICE/DTLS/SRTP 握手）按模块自身文档明确说明"要靠真实 WebRTC peer 验证，CI 里验证不了"——这个 sandbox 没有可用的真实浏览器/WebRTC 客户端来端到端跑通。在没有产品侧对"是否要把现有能跑的全网状通话换成服务端终结"做出决策、也没有配套前端改造计划的情况下，单方面改动服务端会在改变一个当前工作正常的通话系统的行为语义，且改完也无法在本环境验证是否真的能建立媒体连接——风险与不可验证性都指向"需要人工决策 + 真实基础设施"，而不是"续接线代码"。

两项均需要：产品/工程侧对目标架构的决策 + 真实基础设施（第二真实节点 / 真实 WebRTC 客户端）+（SfuMediaSession 这项）配套前端改造计划。这是环境与设计边界，留给专门排期的下一阶段，不计入本 sprint 完成范围。

这两项依赖真实媒体服务器基础设施（非本 sandbox 环境可提供），历次 ROADMAP 复核均得出相同结论——不是遗漏，是部署环境缺口。

---

## 当前纪律

1. **禁止**修改以下模块（除非修复 bug）：
   - `aero-live-srt/`（SRT 协议）
   - `aero-live-webrtc/`（SFU 媒体面）
   - `aero-live-whip/`（WHIP/WHEP）

2. **每次修改后必须运行**：`cargo check --workspace -q`

3. **Phase 2 内**，按 P0 优先于 P1 优先于 P2 的顺序执行

## 决策流程（Agent 每次启动）

```
读 CURRENT_SPRINT.md
│
├─ 有 [ ] 任务 → 实现下一个
│
├─ 编译失败 → cargo check 修复
│
└─ 全部通过 → 读 ROADMAP.md 选择下一项
```
