Now I have a thorough understanding of the project. Let me compose the architecture analysis.

---

# Aero IM — 架构分析与扩展建议

> **分析视角**：资深架构师 | **依据来源**：`AGENTS.md`、全量 crate 结构、Web SPA 源码、`docs/specs/2026-05-22-aero-im-design.md`、`docs/requirements/` 中最新交叉验证分析文档 | **日期**：2026-07-12

---

## 一、架构评估

### 1.1 当前架构的优势

**事件驱动骨架高度成熟**。从 `AGENTS.md §1` 的 DAG 描述可以看出，系统已经走过了「把东西串起来」的阶段，进入了「每个 seam 都定义了边界」的阶段：

| 维度 | 当前状态 | 评价 |
|------|---------|------|
| **事件总线** | NATS JetStream durable/ephemeral consumer 双模式、per-subject 单调 seq、at-least-once 语义 | ✅ 生产级设计 |
| **进程内扇出** | `Hub` bounded mpsc + lossy/disconnect_on_full 策略 | ✅ 反压机制清晰 |
| **跨节点媒体** | `CallBridge` + `bridge_frame` 编帧、UDP relay | ✅ 已建+已测 |
| **存储层** | 16 个 crate 每个职责明确、sqlx + Redis + NATS 三层数据源分工清楚 | ✅ 整洁 |
| **迁移治理** | 157 个迁移全部幂等、编译期嵌入 | ✅ 工程纪律好 |
| **常驻智能体** | 7 个 bot 各自模块化、每个有明确的不变量和 fail-open 策略 | ✅ 状态机清晰 |

**核心决策经得起推敲**：

- **Rust + tokio + axum**：对于 IM/直播这种 I/O 密集型 + 有状态长连接场景，这是正确的技术栈。特别是 Hub 的 `Arc<RwLock<HashMap>>` 连接管理在 GC 语言中会面临 STW 压力。
- **NATS JetStream 做跨实例事实源**：比 Kafka 轻量、比 Redis Stream 可靠、at-least-once 语义 + 可回放，适合每个 subject 独立 seq 的场景。
- **存储层 per-crate XRepo 模式**：比 ORM 灵活、比手写 SQL 可维护、每个 repo 的方法边界清晰。
- **IM + 直播 复用 WebSocket 连接**：减少连接数（每条 WS 连接都携带 room subscriptions + stream watchers），符合「一个进程管理所有实时状态」的架构哲学。

### 1.2 架构债务与技术债

> 以下按严重程度排列。标记为 **P0** 的会影响生产可靠性，**P1** 影响扩展性/可维护性，**P2** 影响产品化进度。

#### P0 — WebSocket 重连 TOCTOU 窗口（方向三）
- `delivery_cursors` 表和 repo 已建（migration 0153），但 WS 协议层只传 `since` 不传 `cursors`
- `ws.js` 的 `_lastSeen` 在页面刷新后丢失，断网重连 + 多设备交替使用场景下有**消息丢失**窗口
- 这是数据可靠性缺陷，不是功能缺失

**技术债根源**：`?since=` 是 MVP 时期的快速方案，`delivery_cursors` 表是后来加的，但协议层和客户端没有同步跟进。属于「数据层超前于协议层」。

#### P0 — @everyone/@channel 通知风暴无上游防护（方向四）
- `dispatch_notifications` 对 `@everyone` 无任何频率/总量限制
- 慢消费者 + @everyone 风暴在 10 万人房间可能导致 Hub 连接管理器震荡
- 移动端推送费用在滥用场景下不可控

**技术债根源**：`dispatch_notifications` 在 `orig.rs` 中写于限流/预算基础设施之前，没有和 `KeyedCostBudget` 或 `RateLimiter` 整合。

#### P1 — Web SPA 处于「调试客户端」阶段（方向二）
- ~5,900 行 vanilla JS 无类型安全、无 bundler、无 i18n、无 offline 支持
- 11 个独立的 JS 文件，无 module boundary
- 服务端的 100+ 路由和 30+ WS 帧类型中，约 1/3 客户端没有对应的处理逻辑（如 `Interaction`、`MessageSeen`）

**技术债根源**：战略决策——早期集中火力做 Rust 后端是正确的（后端是核心竞争力），前端做「够用就行」。但现在后端已经 P0-P11 全部就位，前端的调试态成为产品化的**唯一瓶颈**。

#### P1 — Interaction 与 MessageSeen 被静默丢弃（方向一）
- 这是**最小的修复产出比最高**的问题：服务端完整发送了 `Interaction` 和 `MessageSeen` 帧，但客户端没有处理器
- `Interaction` 扇出给全房间而非仅发帖 bot —— 带宽浪费 + CPU 浪费
- 修复只需要：客户端加 2 个 `ws.on` handler + 服务端 `explicit_recipients` 加 1 个分支

**技术债根源**：`ServerFrame` 枚举在 `ws/ws_impl/mod.rs` 中定义，新 variant 由后端开发者添加，但没有同步更新客户端的 `hookWs()`。缺失跨 crate（Rust 端 ↔ JS 端）的事件注册清单。

#### P1 — Bot 生态「有管道无面市」（方向五）
- Bot CRUD API 完整、事件订阅系统完整、7 个内置 bot 运行中
- 但 Bot webhook 投递**没有重试**（`bot_delivery_log` 是一次性的）、签名用空 secret、无管理 UI
- 交互式 Block（Button/Select）在 render.js 中完整渲染，但 `Interaction` 帧无人消费导致 bot 无法收到点击反馈

**技术债根源**：Bot 基础设施是逐步添加的（migration 0141/0142/0147），bot_dispatch 的 retry 逻辑被推迟到「以后」，但一直没有 backlog 排期。

#### P2 — SFU 复用群通话（方向三）
- 直播侧有生产级 SFU（`aero-live-webrtc`），群通话走浏览器原生 P2P mesh
- 8 人以上群通话不可用，服务端无法录制、无法控制焦点、无法跨节点级联
- 但这不是 P0/P1，因为通话的 1:1 场景当前够用，群通话的 mesh 限制不是崩溃性缺陷

**技术债根源**：架构上的 crate 边界（`aero-live-webrtc` 是直播 crate，`aero-im-call` 是通话 crate）阻止了 SFU 复用。需要 boot 层的 `AppState` 暴露 SFU 实例。

#### P2 — 审计/合规管理「最后一公里」UI（方向四）
- 审计日志 CRUD、法务保全 CRUD、信息隔离墙 CRUD 全部已实现
- 但没有任何管理 UI——对企业合规团队来说等于「没有」
- 无 SOC2/ISO27001 合规审计所需的日志浏览器

**技术债根源**：功能驱动开发——后端团队先实现了 API（因为这是「技术上有挑战的」），UI 被划为「前端工作」而一直未排期。

---

## 二、扩展方向

基于量分析，我给出 **5 个高价值扩展方向**，重点选择那些**投入产出比最高**且**被既有 100+ 份分析系统性低估**的：

### 方向一（P0）：WebSocket 重连交付可靠性 ← 本文建议升为 P0

> 原有方向三的「重连 TOCTOU」问题。

**为什么需要**：

`delivery_cursors` 表已在 migration 0153 创建，`DeliveryCursorRepo::advance` 已实现，但整个链路断在：
1. WS `connect()` 只传 `since` 不传 `cursors`
2. 服务端 `parse_resume_cursor` 只解析 `since`，`cursors` 参数被忽略
3. 客户端 `_lastSeen` 在页面刷新后丢失

这是一个**数据层、协议层、客户端层三层都部分实现但没串通**的典型架构 seam。

**核心挑战**：

| 挑战 | 方案选型 |
|------|---------|
| `cursors` 参数格式 | JSON map `{room_id → seq}` vs 压缩二进制（推荐 JSON，与现有 `?since=` 风格一致） |
| 多设备游标合并 | `advance` 的 seq 单调合并安全但不完备——设备 A 在房间 X 看到 seq 100，设备 B 在房间 Y 看到 seq 200，合并后的 max=200 但房间 X 的进度不能为 200。需要 per-room 独立游标 |
| 回填风暴 | 首次 `cursors` 全部为 0 时，相当于「回填全部历史」。对 10 万条消息的房间需要 `LIMIT` + 分页回填 |

**建议方案**：

```
WS upgrade: ?cursors={"room_X":50,"room_Y":120}
服务端: 
  对每个 room_id，用 delivery_cursor 取 stored_seq，对比 client_seq
  回填 > max(stored_seq, client_seq) 的消息（每条 seq+1000 的消息）
  如果差异超过 100 条，逐步回填（先最近 100 条，再 `?since=` 渐进）
WS onopen 后:
  服务端发送 ServerFrame::CursorsRestored { room_count, message_count }
```

**对现有系统的影响**：
- `ws.js`: 修改 `connect()` 签名，持久化 `_roomCursors` 到 `sessionStorage`（不是 localStorage——页面关闭后清除）
- `ws/ws_impl/mod.rs`：`parse_resume_cursor` 增加 `cursors` 参数解析分支
- `delivery_cursor.rs`：新增 `batch_get` 方法（按 participant_id + room_ids 列表批量查）
- 无需新增迁移、无需新增表

### 方向二（P0）：@everyone/@channel 广播治理

**为什么需要**：

10 万人的工作区中，一次 `@everyone` 产生：
- 100k 条通知行（INSERT 峰值 ~50MB 数据）
- 100k 个 `NotifyBatch` 收件人（NATS 消息 ~6MB JSON）
- 100k 次 `Hub::fan_out_raw` 遍历（CPU 峰值）
- 如果 push_bot 开启 → 100k 次 FCM 推送 → 移动端通知风暴

**技术难点不在于实现，在于设计正确的节流语义**：

| 维度 | 选项 A（基于角色） | 选项 B（基于冷却） | 选项 C（基于预算） |
|------|------------------|------------------|------------------|
| 谁可以 @everyone | 仅 workspace Owner/Admin | 任何成员，但每 N 秒一次 | 任何成员，但有每日配额 |
| 上限 | 按角色 | 按时间窗口 | 按配额池 |
| 复杂度 | 低 | 中 | 高（需持久化配额） |
| 灵活性 | 低 | 中 | 高 |
| 推荐 | 结合 B | 作为默认 | 可选高级配置 |

**建议方案**（结合 A+B）：

1. **新增 `last_broadcast_at` 字段** 到 `rooms` 表（或新建 `room_broadcast_cooldown` 表）：`@everyone` 后设置冷却期 60 秒
2. **广播角色限制**：`room_roles` 增加 `can_mention_everyone` 权限位（默认仅 Owner/Admin/Moderator）；`dispatch_notifications` 校验调用者是否有该权限
3. **广播事件标注**：`NOTIFICATIONS_SENT_TOTAL` 增加 `broadcast=true` label（成本为 0 的 Prometheus 标签），使运营可监控广播频率
4. **@here 降级告警**：当 Redis 故障导致 `here_recipients` 回退到全成员列表时，在响应头 `X-Degraded: @here-fallback` 返回，并记录 metrics 告警

**对现有系统的影响**：
- `migrations/`: 新增 1 个迁移（`room_broadcast_cooldown` 表或 `rooms.last_broadcast_at` 列+索引）
- `aero-im-core/service/orig.rs`：`dispatch_notifications` 新增冷却检查 + 角色检查
- `aero-storage/`: 新增 `RoomBroadcastRepo`（或扩展 `RoomRepo`）
- `common/src/model/block.rs`：`Mention::kind` 不需要改，检查点在通知路径不在消息解析路径
- 无前端改动

### 方向三（P1）：Web SPA 渐进式生产化

**不是重写，是渐进迁移**。

核心原则：**保持 `web/` 作为零构建工具 SPA 的定位不变**，但弥补最致命的缺口。

**阶段化方案**：

| 阶段 | 改动 | 交付物 | 工期估算 |
|------|------|--------|---------|
| **P1a** | i18n 抽取 | `i18n.js` + `zh.json` + 80 处 `t('key')` 替换 | 2-3 天 |
| **P1b** | 客户端缺失事件补齐 | `msg:interaction` + `msg:message_seen` 处理器 | 0.5 天 |
| **P1c** | Service Worker + IndexedDB | `sw.js` + `CacheStorage` 策略 + 消息缓存 | 2 天 |
| **P1d** | 引入 esbuild 做 bundler | 11 个 JS 文件 → 2-3 个 bundle + 代码拆分 | 1 天 |
| **P2a** | 通话 SFU 模式 | `calls.js` 新增 `SfuCallSession` 类 | 3-5 天 |
| **P2b** | Bot 管理面板 | `/bots` 独立页面 + API 对接 | 3 天 |
| **P2c** | 合规 Dashboard | `/admin/compliance` 页面 | 2 天 |

> **关键决策**：不要等「前端重写为 React/Vue」后再推进——当前 vanilla JS 架构在后续 6 个月内仍然是最快迭代路径。引入 TypeScript 和框架的时机应在产品化和 PMF 验证之后。

**为什么不是 React**：

当前 `web/` 的 ~5,900 行 vanilla JS 中：
- 没有复杂的状态管理需求（状态是 `context.js` 中简单的全局对象）
- 没有组件树（渲染是 `render.js` 中的函数式 DOM 操作）
- 没有路由（`app.js` 手动管理视图切换）
- 没有表单状态管理（表单很少且简单）

引入 React 会增加 ~200KB 的 JS 体积、引入 JSX 编译步骤、增加构建复杂度，而收益（虚拟 DOM diff、组件复用）在当前复杂度下不成立。**优先确保功能完整，其次才是架构优雅**。

### 方向四（P1）：Bot 开放平台 — 投递可靠性 + 管理 UI

Bot 基础设施是 Aero IM 相对于 Slack/Discord 的核心差异化优势（AI-Native），当前有完整的管道但：
- Bot webhook 投递**没有重试**（one-shot，失败即丢）
- Bot webhook 签名用**空 secret**（接收方无法验证）
- 无 Bot 管理 UI（只能 curl）

**技术方案**：

1. **Bot 投递重试**：迁移 `bot_delivery_log` 表增加 `retry_count INTEGER DEFAULT 0`、`next_retry_at TIMESTAMPTZ` 列。复用 `webhooks.rs` 的 `mark_failed_with_backoff` 模式（已实现），添加后台 `sweep_bot_deliveries` 定时器。这是 1 个迁移 + 1 个定时器的改动量。
2. **Bot Webhook HMAC 签名**：`bot_event_subscriptions` 表增加 `hmac_secret TEXT` 列。Bot 创建时自动用 `generate_token()` 生成。`bot_dispatch.rs` 用真实 secret 做 HMAC-SHA256 签名。接收方可用 `aero-secret` header 验证。
3. **Bot 管理 UI**：在 `web/` 中新增 `bot-admin.js` + `bot-admin.html` 片段（内嵌到 `index.html` 的 `/bots` 视图切换）。CRUD 全部调用已有 API。

**影响面**：
- 涉及 `aero-storage`（迁移 + repo 扩展）、`aero-server`（bot_dispatch 重试逻辑 + 定时器）、`web/`（新增 Bot 管理页面）
- 不涉及事件总线协议变更
- Bot 管理 UI 可复用 `render.js` 中的现有表单/列表渲染模式

### 方向五（P2）：SFU 复用群通话

这是**投入最高、产出最高**的扩展方向。

**为什么当前不是 P0/P1**：通话的 1:1 场景完全可用（P2P mesh），群通话场景的 8+ 人不可用确实是限制，但影响范围小于方向一/二的可靠性问题和方向四的生态缺失。

**技术方案**：

核心改动点：

1. **Boot 层 SFU 共享**：当前 `SfuRouter` 的实例化在 `aero-live-webrtc` crate 内部，对 `aero-im-call` 不可见。需要在 `aero-server` 的 `AppState` 中暴露 `Arc<SfuRouter>`，使通话逻辑可以创建 SFU 会话。

2. **WS 信令扩展**：新增 `ClientFrame::CallJoinSfu { call_id, sdp }` 和 `ServerFrame::CallSfuOffer { call_id, sdp }` 帧。服务端在收到 `CallJoinSfu` 后，通过 `SfuRouter` 创建 SFU peer，生成 SDP answer 返回给客户端。

3. **客户端 `calls.js` 扩展**：新增 `SfuCallSession` 类。`startGroupCall()` 根据参会人数做决策：
   - ≤4 人 → mesh（当前逻辑，无改动）
   - ≥5 人 → SFU 模式（新逻辑：发流到服务端 SFU，订阅远端流）

4. **媒体面透传**：SFU 模式下，`SfuPeer` 的视频/音频流转发走既有 `SfuForwarder::on_rtp` 通道，不需要新增媒体处理逻辑。

**风险点**：
- SFU 模式下服务端带宽成本：每个参会者上传 1 路流，SFU 分发 N-1 路。对于 10 人通话，服务端出口带宽 = 10 × (N-1) 路。需要带宽预估和限流。
- `calls.js` 的 mesh ↔ SFU 模式切换逻辑需要精心设计（通话进行中不可切换模式，只能在新加入时选择模式）。

---

## 三、接口设计建议

### 3.1 当前接口设计的强项

- **REST API 风格一致**：`POST /api/resources`（创建）→ 返回 201 + Location header；`PATCH /api/resources/:id`（部分更新）；`DELETE` 返回 204。符合 REST 最佳实践。
- **WS 帧类型枚举清晰**：`ClientFrame` / `ServerFrame` 使用 tagged enum（`kind` tag），前端按 `msg:*` 分发。
- **XRepo 模式**：每个 repo 的方法签名符合仓储模式（`PgPool → impl Trait`），容易测试和 mock。

### 3.2 亟需改进的接口设计问题

#### 问题 1：`ws.js` 的 `connect()` 签名设计缺陷

```javascript
// 当前：只传 since
connect(roomId, since = null)

// 应该：接受一个上下文对象
connect({ roomId, cursors, lastSeen })
```

当前的设计把「连接参数」与「房间 ID」耦合了——`connect()` 只接受一个房间 ID，但 WS 连接实际上管理多个房间的订阅。随着 `delivery_cursors` 的接入，参数会膨胀。

**建议**：改为 options 对象模式。

#### 问题 2：没有 WS 帧的客户端注册表

当前的 `hookWs()` 是硬编码的显式注册：

```javascript
ws.on('msg:message', (f) => handleIncomingMessage(f.message));
ws.on('msg:edited', (f) => handleEdited(f.message));
// ...
```

一旦服务端新增了 `ServerFrame` variant，客户端不更新就等于静默丢帧。**应该有一个自动化的检查机制**。

**建议**：
- 在 `ws/frame.rs` 中维护一个 `ALL_SERVER_FRAME_KINDS` 常量列表（`&[&str]`）
- 在 CI 中新增一个 `web-check-frames.sh` 脚本，读取 `ws.js` 中注册的 `ws.on('msg:*')` 列表，与 `ALL_SERVER_FRAME_KINDS` 交叉比对
- 缺失的帧类型 → CI 红

这不解决「客户端未实现处理逻辑」的问题，但解决「客户端不知道有新增帧」的问题。

#### 问题 3：`notifications.js` 和 `push_bot.rs` 没有共享的「推送优先级」模型

当前 `dispatch_notifications` 生成的 `NotifyBatch` 收件人列表是全量的，所有通知（@mention、reply、thread、普通消息）都被同等对待。`push_bot` 在发送 FCM/APNs 时也无法区分「高优先级通知（@mention）」和「低优先级通知（普通消息）」。

**建议**：引入 `NotificationPriority { High, Normal, Low }` 枚举，在 `NotifyBatch` 中携带。`push_bot` 根据优先级决定是否立即推送（High→立即、Normal→聚合、Low→静默）。Web 端 `notifications.js` 根据优先级展示不同样式。

#### 问题 4：Bot webhook 签名接口不完整

当前 `bot_dispatch.rs:40` 使用空 secret 签名：`signed with an empty secret`。

**建议**：
- `bot_event_subscriptions` 表加 `hmac_secret` 列，Bot 创建时用 `generate_token()` 填入
- `bot_dispatch` 用真实 secret 做 HMAC-SHA256，header 名为 `X-Aero-Signature-256`
- 接收方（bot 开发者）可以验证签名——这是 Bot 开放平台的准入门槛

### 3.3 是否需要新的抽象层

**不需要新的 crate 级抽象**。当前 16 个 crate 的分工已经清晰：

```
基础层: common → bus → storage → auth → signaling
IM 层:  im-core → im-call → ai → push
直播层: live-core → live-rtmp → live-hls → live-whip → live-webrtc → live-srt
组合层: server
```

但需要在**现有 crate 内**增加两个接口抽象：

1. **`aero-server` 中增加 `BroadcastGate` trait**：方向二的 @everyone 治理中心。当前 `dispatch_notifications` 直接展开为全成员列表，应该改为经过 `BroadcastGate::check(room_id, sender_id, mention_kind) -> Result<(), BroadcastDenied>` 门控。

```rust
#[async_trait]
pub trait BroadcastGate: Send + Sync {
    /// 检查是否允许发送 @everyone/@channel 广播
    /// Ok = 允许，Err(BroadcastDenied) = 拒绝（含冷却剩余秒数或角色缺失原因）
    async fn check(
        &self,
        room_id: RoomId,
        sender_id: ParticipantId,
        kind: MentionKind,
    ) -> Result<(), BroadcastDenied>;
    
    /// 标记一次广播已发生（更新冷却计时器）
    async fn record_broadcast(&self, room_id: RoomId) -> Result<(), StorageError>;
}
```

2. **`web/` 中增加 `EventRegistry` 类**：自动化 WS 帧注册的检查点。

```javascript
// 所有已注册的事件处理器
export const REGISTERED_EVENTS = new Set([
    'msg:message', 'msg:edited', 'msg:deleted', 'msg:reaction',
    'msg:read', 'msg:typing', 'msg:notify', 'msg:pin',
    'msg:presence', 'msg:membership', 'msg:call', 'msg:stream_event',
    'msg:poll',
    // 待添加：
    // 'msg:interaction',
    // 'msg:message_seen',
]);
```

---

## 四、技术选型

### 4.1 当前技术栈评估

| 技术 | 版本 | 用途 | 评估 |
|------|------|------|------|
| Rust | 2021 / MSRV 1.80 | 全量后端 | ✅ 正确选择 |
| tokio | — | 异步运行时 | ✅ 行业标准 |
| axum | 0.7 | HTTP 框架 | ✅ 0.8 已出但 0.7 稳定够用 |
| sqlx | 0.8 | PG 客户端 | ✅ 编译期检查 |
| fred | 9 | Redis 客户端 | ⚠️ 社区活跃度一般，但功能完整 |
| async-nats | 0.36 | NATS 客户端 | ⚠️ 非官方客户端，但质量好 |
| str0m | 0.19 | WebRTC DTLS-SRTP | ✅ 纯 Rust，唯一选择 |
| Postgres | 17 | 主数据库 | ✅ pgvector + pg_trgm 正确选择 |

**没有需要立即替换的技术**。所有选型都合理。

### 4.2 可选的技术补充

以下不是「必须引入」，而是「当前可以考虑引入」的技术：

| 技术 | 用途 | 建议时机 | 替代方案 |
|------|------|---------|---------|
| **esbuild** | Web SPA bundler | 立即（P1b） | Vite（更重但 HMR 更好）、webpack（过重） |
| **Service Worker API** | PWA 离线缓存 | 方向三 P1c | 无（浏览器标准 API） |
| **Mermaid / D3.js** | 合规 Dashboard 可视化 | 方向四 P2c | ECharts（更重） |
| **OpenTelemetry** | 跨服务追踪（已有 Jaeger） | 已接入 | 无需新引入 |
| **Caddy / nginx** | TLS termination + 静态文件服务 | 生产部署前 | 无（必有） |

### 4.3 不需要引入的技术（延期决策）

| 被建议的技术 | 为什么不引入 | 何时重新评估 |
|------------|-------------|-------------|
| **React / Vue / Svelte** | 当前 ~5,900 行 vanilla JS 的复杂度不足以 justify 框架成本。框架解决的是「大型 SPA 的状态管理和组件复用」，而当前页面数量 < 10 个视图 | 当 `web/` JS 总行数超过 15,000 或视图超过 20 个时 |
| **TypeScript** | 渐进式迁移比全量 TS 迁移更现实。当前先做 `JSDoc` 类型注释，再逐步 `.d.ts` | 当引入 esbuild bundler 后，TS 编译成本趋近于零时 |
| **Kafka** | NATS JetStream 完全满足当前 at-least-once + 回放需求。Kafka 的优势（重分区、Exactly-Once、Kafka Connect）当前不需要 | 当日志事件量 > 10GB/天或需要 Kafka Connect 生态时 |
| **TURN/STUN 服务** | 当前使用浏览器原生 ICE，无需额外搭建。生产环境需要 coturn 但归运维不在架构层 | 生产部署前配置 coturn 即可 |

### 4.4 自建 vs 采购的决策参考

| 场景 | 自建 | 采购/第三方 | 当前选择 | 评估 |
|------|------|-----------|---------|------|
| AI 推理 API | ❌（无 GPU 集群） | ✅ Anthropic + Voyage | 采购 | ✅ 正确。AI 推理是核心竞争力但不需要自建 GPU 集群 |
| 推送网关 | ⚠️（已有 FCM/APNs 封装） | ⚠️（firebase/fcm-http） | 自建 aero-push | ✅ 正确。推送是薄封装，自建可控 |
| HLS 分发 | ❌（边缘节点成本） | ✅ CDN（CloudFront/Cloudflare） | 即将选择 | 正确方向 |
| Bot 市场 | ✅（平台差异化） | ❌（没有合适的第三方） | 自建 | 待实现（方向四） |
| 审计合规平台 | ✅（深度集成） | ❌（Splunk 等太重） | 自建 | 方向四，API 已实现 |

---

## 五、实施路线图

### 5.1 优先级排序（P0 → P1 → P2）

```
P0（立即 — 本周）
├── 方向一：delivery_cursors 全线接入（消除重连丢消息）
├── 方向二：@everyone/@channel 广播治理（消除通知风暴）

P1（本月）
├── 方向三 Web SPA 生产化
│   ├── P1a: i18n 抽取（2-3 天）
│   ├── P1b: Interaction + MessageSeen 客户端补齐（0.5 天）
│   ├── P1c: Service Worker + IndexedDB 缓存（2 天）
│   └── P1d: esbuild bundler（1 天）
├── 方向四 Bot 开放平台
│   ├── Bot 投递重试（1 迁移 + 1 定时器，1 天）
│   ├── Bot webhook HMAC 签名（1 迁移 + bot_dispatch 改动，0.5 天）
│   └── Bot 管理 UI（3 天）

P2（本季度）
├── 方向五 SFU 复用群通话
│   ├── Boot 层 SFU 共享（1 天）
│   ├── WS 信令扩展 CallJoinSfu / CallSfuOffer（2 天）
│   └── calls.js SfuCallSession（3-5 天）
├── 方向四延伸：合规 Dashboard（2 天）
└── CI 增强：WS 帧注册表交叉比对脚本（0.5 天）
```

### 5.2 阶段划分和里程碑

| 里程碑 | 时间 | 交付物 | 验证标准 |
|--------|------|--------|---------|
| **M1: 可靠性上线** | 本周 | delivery_cursors 全线接入 + @everyone 冷却 + 广播角色限制 | 多设备重连测试不丢消息；@everyone 每分钟最多 1 次 |
| **M2: 客户端完整性** | 第 2 周 | Interaction + MessageSeen 帧消费；Bot 管理 UI；投递重试 | 交互式按钮有实时反馈；Bot 创建/配置全 UI 化 |
| **M3: Web 基线** | 第 3 周 | i18n 抽取 + esbuild + Service Worker 缓存 | zh.json 语言包；2 个 JS bundle；断网时显示缓存消息 |
| **M4: 通话升级** | 第 5 周 | SFU 群通话（5+ 人自动切换） | 8 人通话 O(N²) → O(N)；服务端可录制 |

### 5.3 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|---------|
| delivery_cursors 回填风暴（首次游标全 0 时） | 中 | 高 | 设 `BACKFILL_LIMIT=500` 每房间，逐步回填；首次 `cursors_restored` 帧通知客户端「还有未回填消息」 |
| @everyone 冷却导致管理员投诉 | 中 | 中 | 冷却 60 秒可配置（`AERO_BROADCAST_COOLDOWN_SECS`）；超级管理员（workspace Owner）忽略冷却 |
| esbuild 引入后 dev workflow 变复杂 | 低 | 低 | 保留 `index-dev.html`（直接加载 JS 文件）的零构建模式，esbuild 仅用于生产构建 |
| Bot 投递重试 + 签名变更导致已有 Bot 行为变化 | 中 | 低 | HMAC secret 向后兼容：无 secret 的行跳过签名（不破坏现有 bot）；重试最小间隔 60 秒，不影响 one-shot 投递的即时性 |
| SFU 群通话带宽预估不足 | 中 | 高 | 初始 SFU 模式限制 ≤16 人通话；视频默认 480p（发流端 `{ width: 640, height: 480 }` 约束）；监控 `sfu_media_bytes_total` metrics 决定带宽扩容 |

### 5.4 依赖关系图

```
方向一（delivery_cursors） ← 无依赖，可独立开工
方向二（@everyone 治理）    ← 无依赖，可独立开工
方向三 P1a（i18n）          ← 无依赖
方向三 P1b（Interaction）   ← 依赖方向一（客户端事件注册）
方向三 P1c（Service Worker）← 依赖方向三 P1a（但可并行）
方向三 P1d（esbuild）       ← 依赖方向三 P1a（但可并行）
方向四（Bot 平台）          ← 无依赖，可独立开工
方向五（SFU 群通话）        ← 依赖方向三 P1a（calls.js 重构基础）
```

> **关键依赖**：方向五（SFU 群通话）依赖于 `calls.js` 的代码可维护性提升（方向三 P1a）。如果直接修改当前 639 行的 `calls.js` 添加 SFU 模式，风险太高。建议先完成 esbuild bundler + 代码拆分（P1d），使 `calls.js` 的 mesh 和 SFU 逻辑可以在独立文件中开发和测试。

---

## 总结

Aero IM 的 Rust 后端已经处于**极高的工程成熟度**——事件驱动骨架、存储层、常驻智能体、WebRTC 媒体管线的设计质量远超大多数同阶段项目。当前的核心矛盾是：

> **后端生产级 ↔ 前端调试级**

五个扩展方向中，**方向一（交付可靠性）和方向二（广播治理）是 P0**，因为它们直接影响生产可用性且修复成本极低。**方向三（Web 渐进式生产化）是 P1**，是产品化的必经之路但不需要一蹴而就——先补 i18n、再补 bundler、再补 offline，每一步独立交付。

**不需要大重构**。当前的架构骨架支持所有这些扩展，不需要新的 crate、不需要换框架、不需要重写现有模块。这本身是对当前架构设计质量的最佳证明。

---

*以上分析基于 `AGENTS.md`、`docs/specs/2026-05-22-aero-im-design.md`、`docs/requirements/` 最新交叉验证分析文档、以及 crate 和 Web SPA 源码的架构级阅读。*
