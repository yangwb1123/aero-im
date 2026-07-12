# 架构分析报告：Aero IM 系统性缺口

## 1. 架构评估

### 1.1 当前架构优势

从 AGENTS.md 和验证报告可以提取出一些值得肯定的架构决策：

| 决策 | 价值 |
|------|------|
| **事件驱动 + NATS 扇出** | 跨实例水平扩展，进程内 Hub 本地扇出解耦 |
| **at-least-once 总线 + 幂等键** | webhook/付费等有副作用路径有幂等兜底（§4.2「at-least-once 状态机」规则） |
| **Redis 集群状态（非进程内存）** | presence/roster/观看数避免单进程脑裂 |
| **迁移编译期嵌入** | 部署原子性，无 schema drift |
| **AI 无 key 退化** | 沙箱/开发环境无阻塞路径 |

### 1.2 关键架构缺口（验证报告验证的 4 个方向）

```
┌──────────────────────────────────────────────────┐
│                  4 个系统性问题域                     │
├─────────┬──────────┬──────────┬──────────────────┤
│ 发送可靠性 │ Web Push │ 媒体韧性  │ 一致性治理         │
│          │          │          │                  │
│ 无 nonce │ 无 SW    │ 无看门狗  │ 扇出成员缓存 60s  │
│ 重投失联  │ 浏览器关闭│ 单错即死  │ 踢人仍收消息      │
│          │ 即不可达  │          │                  │
└─────────┴──────────┴──────────┴──────────────────┘
```

这四个问题性质不同但都指向同一个深层模式：**系统的可靠性/一致性保障只覆盖了「核心数据路径」（持久化 + 总线投递），但没有延伸到「端到端用户感知面」——客户端状态、媒体面、推送面、扇出面的成员集。**

### 1.3 架构债务分类

按严重性和修复成本分层：

| 层级 | 债务 | 影响范围 |
|------|------|----------|
| **P0 用户可见 bug** | 消息重投导致 pending 孤悬 | IM 核心体验 |
| **P1 设计缺失** | 无 ICE restart / SFU 无 watchdog | 直播/通话线上可用性 |
| **P1 设计缺失** | Web Push 无 Service Worker | 推送覆盖缺口 |
| **P2 一致性窗口** | 扇出成员缓存 60s → 越权可达 | 安全/合规 |
| **P3 工程治理** | 媒体 seam 有框架无接线（§2 标注） | 代码可理解性 |

---

## 2. 扩展方向

### 方向 A：端到端消息可靠性层（End-to-End Message Reliability）

**为什么需要**：当前 at-least-once 只到 Hub 扇出。客户端收到帧后：
- 不确认 → 服务端不知道是否真的展示
- 重连不补发 → 丢消息不可恢复
- pending 匹配靠内容 + 时间 → 连续两条相同消息必丢一条

这是**用户可见的 bug**，不是理论风险。

**核心挑战**：
1. **幂等键的端到端传播**：`nonce` 需从客户端生成 → 服务端持久化 → 去重写入。当前 `SendMessage` 没有 `nonce` 字段，`StreamGift` 有。需要统一。
2. **重连补发（Gap Recovery）**：客户端需知道自己的 last seen seq，服务端需支持 per-connection seq gap 查询。当前 `seq` 是 per-subject 单调递增，但客户端没有持久化的游标。
3. **ACK 回执的流量放大**：每消息 ACK → N per 房间成员 → O(N²)。需合并/批量。

**架构变更**：

```
当前:  Client ──send_msg──→ Server ──bus──→ Hub ──fan_out──→ Clients
                                                              ↑ 无确认

建议:  Client ──send_msg(nonce)──→ Server(去重持久化)
                                     │
                                     ├──bus──→ Hub ──fan_out──→ Clients
                                     │                          │
                                     └── response(nonce, seq) ←─ ACK
                                              (批合并)
```

- 客户端消息 **必须带 nonce**（UUID v7），服务端 `ON CONFLICT DO NOTHING`（复用 `message_history` 或新 `message_nonces` 表）。
- WS 帧新增 `ack`/`gap_request`/`gap_fill` 类型。
- 客户端连接后发 `gap_request(last_seq)` → 服务端批量回补。
- 非功能要求：ACK 合并窗口（每 200ms 一批）。

**对现有系统影响**：
- 侵入性中等：`SendMessage` 帧加 `nonce` 是 break change，需前后端同步上线。
- `aero-im-core` 写路径加去重检查（性能损耗 < 0.1ms）。
- 客户端 `app.js` 需 pending 跟踪从「内容匹配」改为「nonce 索引」。

**选项 A1 vs A2**：

| | A1: 轻量 nonce 去重 | A2: 完整 Gap Recovery |
|--|---------------------|----------------------|
| 实现量 | ~2 天（前后端） | ~2 周（含 ACK 机制、重连补发、游标持久化） |
| 解决 | pending 孤悬 bug | + 断线不丢消息 |
| 风险 | nonce 唯一性冲突（UUID v7 可忽略） | 游标持久化 → 本地存储方案（IndexedDB/Web Storage） |
| 建议 | **P0 立即上** | P1 后续迭代 |

---

### 方向 B：Web Push 架构升级——Service Worker 离线推送

**为什么需要**：当前 `new Notification()` 只活在页面打开期间。移动端/后台场景推送完全不可达。尽管有 FCM/APNs 原生推送（`push_bot`），但 Web 端（桌面浏览器后台）存在缺口。

**核心挑战**：
1. **VAPID + Service Worker 注册生命周期**：`PushManager.subscribe()` 需用户手势触发，推送 token 需上传并关联用户。浏览器可能吊销 permission。
2. **SW 消息处理 vs 页面唤醒**：SW 收到 push 后 `self.registration.showNotification()` → 点击时 `clients.openWindow()` 携带 deep-link。需要协调 SPA 路由。
3. **双通道冲突**：原生 FCM/APNs + Web Push 可能重复推送。需要 dedup（按 `event_id`）。

**架构变更**：
```
      push_bot ──→ FcmGateway/ApnsGateway ──→ 原生推送
                │
                └──→ WebPushGateway ──→ VAPID Push Service ──→ Service Worker
                                                                  │
                                                           showNotification()
                                                                  │
                                                          click → openWindow(url)
```

- `aero-push` 新增 `WebPushGateway` 实现 `PushGateway` trait（现有 seam 直接插拔）。
- 需增加 `web_push` crate 依赖（RFC 8030）。
- 数据库 `push_tokens` 表增加 `type` 字段：`fcm`/`apns`/`web_push`。
- 客户端注册：页面 load → `if 'serviceWorker' in navigator` → 注册 SW → `PushManager.subscribe` → POST token。

**对现有系统影响**：
- 低侵入：`PushGateway` trait 已存在，加实现即可。
- 客户端改动中等（新增 sw.js + 注册逻辑）。
- `push_bot` 需处理 Web Push 的 `expirationTime`（过期 token 回收已在总线的 Rejected 路径）。

---

### 方向 C：媒体面韧性层（SFU Watchdog + ICE Restart + HLS 段校验）

**为什么需要**：当前媒体组件有三个相互独立的可用性问题：
1. **SFU 单点故障**：`SfuMediaSession::run` 任何 error → `return`（`sfu_media.rs:129`），会话彻底终止
2. **无 ICE restart**：网络切换（WiFi→4G）导致媒体中断不可恢复
3. **HLS 残缺段**：无 TS 完整性校验 → 播放器卡在最后一段

这三个问题在直播/通话场景中是 **线上 P1**。

**核心挑战**：
1. **ICE restart 触发时机**：str0m 支持 ICE restart 但需要信令面配合——重新交换 SDP。WS 需新增 `ice_restart` 帧，`SfuMediaSession` 需绑定 cancel-safe restart 逻辑。
2. **Watchdog 状态机**：`no media for N seconds → trigger restart → no media for M seconds → downgrade → hangup`。需要合理参数（N=10s, M=30s？）避免 WiFi 短暂抖动触发过度 reaction。
3. **HLS 段校验的格式依赖性**：TS 校验需理解 MPEG-TS 结构（sync byte 0x47、PID map、PTS 连续性）。`FlvToTsConverter` 输出端加校验比接收端加校验更可控。

**架构变更**：

```
SfuMediaSession::run
  │
  ├── media_loop (select!)
  │   ├── ← SfuPeer::poll → Media/MediaAbort
  │   ├── ← cancel (CancellationToken)
  │   └── ← watchdog_tick (每 2s)
  │         ├── last_rtp_packet < threshold → ice_restart()
  │         └── no_media_for > max_downgrade → call_end()
  │
  └── on_error → 非 fatal 错误（str0m 协议错误）重入，
                  fatal（cancel token 触发）优雅 shutdown
```

- `stream_watchdog` 模块（新文件，`aero-live-webrtc/src/watchdog.rs`）封装状态机。
- ICE restart 信令走 WS `call_ice_restart` 帧 → `CallOrchestrator.signal_ice_restart` → 交换新 SDP。
- HLS 端在 `FlvToTsConverter::write_segment` 后调用 `validate_ts_segment(path) → Result`，失败则 flush 缓冲区的上一个完好段 + 记异常日志。

**对现有系统影响**：
- 侵入性中等偏高。`SfuMediaSession` 当前唯一调用方是 test，生产未接线（§2 seam）。这意味着**当前没有生产媒体面**——加韧性层前需先完成 §2 标注的接线。
- ICE restart 需要 WS 协议新帧 + `CallOrchestrator` 新方法。
- HLS 段校验纯新增，零侵入。

**实施建议**：先接 SFU 生产回路（§2 seam），再加 watchdog 和 ICE restart，最后 HLS 校验。三个阶段不可倒置。

---

### 方向 D：扇出一致性窗口消除（Stale Member Cache Gap）

**为什么需要**：验证报告指出 `bus.rs:84,129` 使用 `room_member_cache`（TTL 60s）做扇出决策。这意味着：
- 踢人 → 被踢者仍收消息 60s
- 权限变更 → 旧角色继续生效 60s
- 加入房间 → 60s 后才收到实时消息

这是安全和体验双缺口。

**核心挑战**：
1. **缓存失效的原子性**：成员变更操作（`remove_member`/`change_role`）需要同步失效 bus listener 端的缓存。当前各进程独立缓存，无跨进程广播失效机制。
2. **实时 vs 性能权衡**：每消息查 DB 做扇出决策 → O(N*Q) 不可接受。需要比 60s TTL 更细粒度的失效策略。

**解决方案选项**：

| 方案 | 机制 | 优势 | 代价 |
|------|------|------|------|
| D1: 消息级 DB 查询 | 每消息扇出前查 `room_members` + `participants` | 零窗口 | 每消息 N 次 DB 查询（大房间数千成员 → 不可接受） |
| D2: TTL 缩短 | 10s 代替 60s | 改动极小 | 窗口缩短但仍在；查询压力 6x |
| D3: 变更总线广播 | 成员变更时总线发 `MemberChanged` → 每进程失效本地缓存 | 窗口 ≈ 总线延迟（ms 级） | NATS subject 增加一类；bus listener 需处理新类型 |
| D4: 最终一致性接受 | 文档声明 60s 窗口为预期行为 | 零改动 | 不合规 |

**建议**：D3（推荐）+ D1 fallback（仅在被踢者有活跃 WS 连接时做额外校验，避免大房间全量查）。

**架构变更**：
```
成员变更操作（HTTP/WS）
  │
  ├── 持久化变更（DB）
  ├── invalidate 本地 participant_cache
  └── publish(im.room.{id}, MemberChanged{user_id, action: Removed})
       │
       bus listener（每进程）
         ├── room_member_cache.invalidate(user_id)  # 立即失效
         └── 继续现有 fan_out 逻辑（下次自动回源）
```

- 新增 `RoomEvent::MemberChanged` variant（注意 §4.1 的 `kind` 标签规则）。
- `bus.rs` 增加 match arm：收到 `MemberChanged` → 更新 `room_member_cache` 或直接清除。
- `room_member_cache` 增加 `invalidate(user_id)` 方法。

**对现有系统影响**：
- 中等侵入：新增 `RoomEvent` variant（影响序列化、WS 帧、web 端兜底处理）。
- 不影响已有消息流（纯新增逻辑分支）。
- 缓存结构无变化，仅增加失效机制。

---

### 方向 E：客户端状态机治理（Client State Machine Layer）

**为什么需要**：4 个方向的问题在根上有一个共同原因——**客户端没有结构化状态机**。当前 `app.js` 是事件驱动的扁平 handler 集合，没有：
- 连接状态（connected/reconnecting/disconnected）
- 消息发送状态（pending/sent/delivered/failed）
- 媒体会话状态（negotiating/connected/reconnecting/terminated）
- 本地缓存状态（fresh/stale/loading）

导致每个缺口都需要独立的临时补丁。

**核心挑战**：
1. **JS 无类型检查**：当前 web SPA 是零依赖 ES2020，没有 TypeScript/Flow。引入状态机框架（XState/robot）需要加构建工具。
2. **状态与 UI 分离**：当前状态隐式在 DOM 和闭包中。需引入显式 store（可简单 `class StateStore extends EventTarget` 不依赖第三方）。
3. **迁移成本**：重写 `app.js` 20KB+ 已经是成熟产品。增量迁移比重写可行。

**架构变更**：

```
当前： ws.onmessage → handleEvent(message/edited/deleted/reaction/typing/…)
                        └── 直接更新 DOM

建议： ws.onmessage → StateStore.dispatch(event)
                        │
                        ├── StateMachine.transition(event)
                        │     └── 更新 local cache（消息/成员/游标）
                        │
                        └── UI 层订阅 diff → 批量 DOM 更新
                              └── (requestAnimationFrame 合并)
```

- 新增 `state_store.js`（ES module，~500 行）暴露 `dispatch()` 和 `subscribe()`。
- 现有 `handleEvent` 逐步迁移：先迁移消息相关（最大收益），再迁移媒体/通话。
- 本地持久化（IndexedDB）：消息缓存、游标、nonce 列表——支持离线状态恢复。

**对现有系统影响**：
- 纯新增，不破坏现有 handler（并行存在，逐步迁移）。
- 后端零改动。
- 构建工具可选：纯 ES module 方式不需要 bundler（import map 即可）。

---

## 3. 接口设计建议

### 3.1 关键模块接口原则

**原则 1：每个新能力入口必须在现有 seam 上扩展，不另起炉灶**
- Web Push 走 `PushGateway` trait（已有），不加第二套推送抽象。
- ICE restart 走 WS `CallEvent` enum（已有），不加独立信令通道。
- nonce 去重走 `SendMessage` 加字段（已有），不加独立 idempotency API。

**原则 2：客户端接口变更必须前后端同构**
- `nonce` 必须在 `ClientFrame::SendMessage` 和服务端 `frame.rs` 同时出现，发布时前后端一起上线。
- `ice_restart` 帧必须在 `ClientFrame` 和 `ServerFrame` 都有对应 variant，即使服务端回的是简单 `ack`。

**原则 3：一致性窗口治理不改变外部接口**
- `MemberChanged` 总线事件是内部架构变更，不暴露为 REST/WS API。Web 端无需感知。
- bus listener 的缓存失效是进程内行为，不穿透到 `Hub::fan_out_raw`。

### 3.2 是否需要新抽象层

| 新抽象 | 必要性 | 说明 |
|--------|--------|------|
| **客户端 StateStore** | **推荐** | 现有 JS 无状态管理，5 个独立修复中的 3 个需要它（pending 跟踪、重连补发、本地游标）。如不做，每个修复的复杂度都更高 |
| **SFU Watchdog** | 必要 | 当前 `SfuMediaSession::run` 无重试/恢复逻辑。需要封装成独立结构体而非内联在 run loop 里 |
| **Web Push Gateway** | **推荐** | `PushGateway` trait 已有，加实现即可。不需要新接口抽象 |
| **消息去重层** | 不必要 | 去重逻辑可内联在 `ImService::send_message` 中（加 `SELECT ... WHERE nonce = $1 LIMIT 1` + `ON CONFLICT DO NOTHING`），不需要独立组件 |

### 3.3 向后兼容性

**必须保持兼容的**：
- WebSocket 帧格式：`nonce` 加 `Option<String>`（默认 None），旧客户端不发送也不理解。服务端旧代码也兼容新帧（忽略未知字段）。
- `RoomEvent` JSON：`MemberChanged` 是新 variant，web 端 `ws.on('msg:..')` 无 handler 时静默丢弃（已在 `event.call_kind || event.kind` 兜底逻辑覆盖）。

**允许 break change 的**：
- `SendMessage` 请求体加 nonce（前后端同步上线，窗口期 < 1 分钟）。
- `CallEvent` 加 `IceRestart` variant（同样同步上线）。

**兼容策略**：所有 break change 用 feature flag gate。服务端 `AERO_FEATURE_NONCE`（测试阶段），稳定后移除 flag 强制。

---

## 4. 技术选型

### 4.1 是否需要新技术栈

| 能力 | 推荐 | 替代方案 | 选择理由 |
|------|------|----------|----------|
| **Web Push** | `web-push` crate + `Service Worker` | 无第三方，手写 HTTP + VAPID | `web-push` 是 Rust 社区标准 RFC 8030 实现，约 20KB 依赖，测试覆盖好 |
| **客户端状态管理** | 零依赖 `EventTarget` | XState/Redux/Zustand | 当前 web 是零构建 ES2020，加 bundler/babel 是破坏性变更。`EventTarget` 是浏览器原生 API |
| **IndexedDB 客户端缓存** | `idb` 库（~5KB）或手写 | localStorage（同步，有 5MB 限制） | IndexedDB 异步 + 无大小限制；idb 是 0-dep 的 Promise wrapper |
| **ICE restart 信令** | 走现有 WS `CallEvent` | 独立 HTTP endpoint | 复用现有信令通道，避免候选收集二阶段问题 |

### 4.2 第三方依赖评估标准

每个新依赖的硬性评估 checklist（参考 AGENTS.md §4.2 已有约束）：

```
□ 是否在 MSRV 1.80 上编译通过
□ 是否无 unsafe（不引入 unsafe_code = "forbid" 违规）
□ 是否 Apache-2.0 / MIT 兼容（deny.toml 已配置的 allowlist）
□ 是否 > 1000 GitHub stars 或审计过
□ 是否在 tokio 生态内（async 依赖需是 tokio-native）
□ 是否最小侵入（优先 trait-based 依赖如 tower，优先 small feature flag）
```

### 4.3 自建 vs 采购

| 场景 | 决策 | 理由 |
|------|------|------|
| Web Push | 自建 `WebPushGateway` | `PushGateway` trait 已有，添加实现约 1 天。且 VAPID push 不涉及复杂状态管理 |
| ICE restart 信令 | 自建 | 信令协议已在 WS 层有 `CallEvent`；ICE restart 本质是重新 SDP 交换，复用 `aero-signaling` crate 的 `RtcConfig`/`IceServer` 校验逻辑即可 |
| Web 端重连/状态机 | 自建 | 无成熟 OSS 产品适用于现有零构建 SPA。XState 需要构建工具链迁移 |

---

## 5. 实施路线图

### 5.1 优先级排序

```
P0 ─── 用户可见 bug（直接影响留存）
  ├── nonce 去重 + pending 修复
  └── 扇出窗口 60s → MemberChanged 总线失效

P1 ─── 可用性缺口（影响线上置信度）
  ├── SFU 生产回路接线（§2 seam）
  ├── SFU Watchdog + ICE restart
  └── Web Push Service Worker

P2 ─── 体验优化（中期迭代）
  ├── 客户端 StateStore 增量迁移
  ├── IndexedDB 本地缓存 + 离线恢复
  └── HLS 段校验

P3 ─── 工程治理
  ├── 媒体 seam 接线文档更新（从「未接线」标记改为「已接+已知边界」）
  └── 一致性窗口 SLA 文档声明
```

### 5.2 阶段划分

**阶段 1（2 周）：P0 修复——用户可见 bug 清零**

```
Week 1                Week 2
───────                ───────
nonce 字段定义 (1d)    ─── MemberChanged 总线
     │                         │
后端去重逻辑 (2d)         bus listener 失效
     │                         │
前端 pending 重构 (2d)    Web 端静默处理
     │                         │
集成测试 CI (1d)         ─── 端到端验证 (1d)
     │
冒烟 + 线上部署 (1d)
```

里程碑 M1：连续两条相同消息不再导致 pending 孤悬。踢人后 5s 内停发消息（从 60s 降为总线延迟）。

**阶段 2（3 周）：P1 媒体面韧性**

```
Week 3                  Week 4-5
───────                  ───────
SFU production wiring   SFU Watchdog 状态机
  (sfu_media.rs 接线)       │
     │                    ICE restart 信令
     │                       │
SDP-answer 集成测试      str0m ICE restart 验证
     │                       │
冒烟验证               ─── 生产灰度 (1w)
```

里程碑 M2：单 SFU 会话在 10s 静默后自动触发 ICE restart，30s 后优雅降级。HLS 段不卡在残缺段。

**阶段 3（2 周）：P1 Web Push**

```
Week 6                  Week 7
───────                  ───────
WebPushGateway 实现      Service Worker 注册
     │                         │
push_tokens.type 迁移    点击通知 → deep-link
     │                         │
VAPID key 配置           双通道 dedup
     │                         │
集成测试                 ─── 生产灰度
```

里程碑 M3：Chrome/Firefox/Safari 后台收到推送通知，点击后打开正确房间/消息。

**阶段 4（持续 2 个月）：P2 客户端架构升级**

增量迁移，不设硬截止日期。按「每个功能修复时顺带迁移对应状态」的节奏。

```
Week 8-12+
──────────
StateStore 框架 (3d)
     │
消息状态迁移 (1w)        ← 配合阶段 1 nonce 改动
     │
IndexedDB 缓存 (1w)     ← 重连补发的前提
     │
媒体状态迁移 (1w)       ← 配合阶段 2 ICE restart
     │
通话状态迁移 (5d)
     │
存量 handler 清理 (ongoing)
```

### 5.3 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| **nonce 发布前后端不同步** | 中 | 旧客户端发消息无法去重 | WS 协议版本号 + 灰度窗口（先 rollout 后端，再 rollout 静态资源） |
| **ICE restart 与 str0m 版本兼容** | 中-高 | ICE restart 导致连接中断 | 先做 `#[cfg(test)]` 模拟验证，阅读 str0m 0.19 ICE restart 文档；备选：不开 ICE restart，只做 watchdog→hangup 快速失败 |
| **Service Worker 注册被浏览器拦截** | 低 | Web Push 永远不可达 | 优雅降级：SW 注册失败不影响现有页面内通知行为 |
| **扇出缓存失效风暴** | 低-中 | 大量成员变更时总线饱和 | MemberChanged 合并窗口（相同房间多个变更合并为一次批量失效） |
| **IndexedDB 存储配额** | 低 | 缓存撑满 | LRU 淘汰 + 限制缓存消息数（5000 条）+ storage estimate API 告警 |
| **AGENTS.md §4.2 工程规则违规** | 中 | 新依赖引入 unsafe_code | 每一新 crate 加入前 `cargo deny check` + CI 门禁。`web-push` crate 需要 audit（已有 4000+ stars，Apache-2.0） |

### 5.4 渐进式验证策略

```
每阶段验证 checklist：

阶段 1:
  □ cargo test --workspace --lib + --ignored (PG 门控) 全部绿
  □ 手动冒烟：发两条 "收到"，第二条不出 pending
  □ CI: 踢人后 5s 内被踢者 WS 不再收到消息帧

阶段 2:
  □ 单元测试：watchdog 状态机覆盖 6 种时序
  □ 手动冒烟：kill SFU 进程内网络 (iptables drop)，10s 后自动 ICE restart
  □ testground: 双节点 call-bridge + SfuMediaSession 端到端验证

阶段 3:
  □ 单元测试：WebPushGateway.push() 成功/失败/过期
  □ 手动冒烟：Chrome DevTools Application > Service Workers > Push 模拟

阶段 4:
  □ 无回归：现有 handleEvent 路径全部走通
  □ 性能：StateStore dispatch < 1ms per event (1000 event/s 压力)
```

---

## 总结

验证报告揭示的 4 个方向不是独立的 bug，而是共同指向一个根因：**系统的可靠性治理停在了「服务端投递完成」，没有延伸到「客户端确认达成」**。

核心建议：

1. **立即（P0）**：`nonce` + 扇出缓存失效——成本低、收益明确、用户可见 bug 清零。
2. **近期（P1）**：媒体面韧性 + Web Push——补全系统在离线/弱网场景的能力缺口。
3. **中期（P2）**：客户端 StateStore——使得后续所有端侧能力（重连补发、离线缓存、本地搜索）有统一的架构基座。

这 4 个方向独立可交付、互不阻塞，按优先级逐步消化即可。不需要大规模架构重构——每个问题都在现有 seam 上扩展解决。
