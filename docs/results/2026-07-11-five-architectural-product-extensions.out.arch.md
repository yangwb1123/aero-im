好，下面我从架构师视角，基于验证评估的代码证据做深层分析。

---

# 架构分析报告

## 1. 架构评估

### 优势：事件驱动骨架正确，但表面质量不均匀

先看优势。`AGENTS.md` 定义的事件 DAG——NATS → 进程内 Hub → WS 扇出——已通过 `run_bus_listener`（durable consumer）和 `run_live_bus_listener`（ephemeral consumer）验证。这是正确的分层：跨实例事实源与进程内扇出分离，且 durable/ephemeral 的选择理由充分（房间事件从丢失中恢复是语义需求，弹幕丢失是可容忍的）。骨架没有根本性问题。

但验证评估暴露了一个系统性问题：**后端基础设施质量远高于前端，形成架构的「头重脚轻」状态**。后端的 at-least-once 语义、幂等键、预算控制器、优雅重连、seq gate——全部有设计、有测试、有运行时保障。前端却在裸 WebSocket 上乐观渲染，对 send() 返回值从不检查，DOM 永不回收。这不是「前端还没做完」，而是两个子系统用不同的可靠性哲学建造。

从系统完整性看，**WebSocket 链路出现了断裂**：后端已经实现了 at-least-once delivery（durable consumer + seq + ack 前扇出），但在前端 send() 失败时，消息既不入队重试也不报警。这意味着最高层的可靠性（at-least-once）在边界处被降级为「可能丢失」——架构投入与用户体验的回报不成正比。

### 关键设计决策评估

**好的决策**：

1. **NATS JetStream 作为事实源**：选型合理。比 Kafka 运维轻，比 Redis Stream 更可靠（R1/N-1 集群），原生 Push Consumer 简化了扇出模式。一个架构性风险：依赖 `async-nats 0.36`（预 1.0），JetStream API 的稳定性取决于社区成熟度。

2. **crate 分割按领域不按层**：`aero-common` → `aero-im-core` / `aero-live-*` 的依赖关系清晰，无成环。这使 AI 检索/通话编排/直播摄入等不同生命周期的模块可以独立演进。`aero-common` 维护得不错——`define_id!`、`RoomEvent` tagged enum、共享类型——是 crates 之间契约的良好收口。

3. **Redis 只做集群级状态**：presence、stream viewer counts、call roster 全部在 Redis sorted-sets 中，不放在进程内存。这个决策保护了水平扩展时的正确性：增加实例自动扩展开销，不会出现传统 IM 的「每进程在线名册不一致」问题。

4. **预算控制器（AiWorker 的 KeyedCostBudget + global CostBudget）**：极好的防御性设计。用 per-ws 预算防止单个工作区饿死全局，用 defer（非 fail）处理超限避免激进的退避循环，`FOR UPDATE SKIP LOCKED` 支持 postgres-based 水平扩展。这是稀缺资源（AI 推理）的制度保障。

5. **事件总线的 subject 设计**：`im.room.{id}` 和 `live.stream.{id}` 两个命名空间，各自 per-subject 单调 seq，避免全局 seq 成为瓶颈。JetStream 的 subject-based 消费模型在这里是最佳选择。

**值得商榷的决策**：

1. **AiWorker 的 PostgreSQL 轮询 vs NATS JetStream 队列**：轮询 `ai_jobs` 表 + `FOR UPDATE SKIP LOCKED` 是可靠的，但和系统的「事件驱动」骨架不一致。如果所有 worker（包括 bot）都走 NATS consumer，AI jobs 同样可以用 `ai_jobs.{kind}` subject + queue-group consumer（每 kind 一个队列组）。现在 AiWorker 持有一个独立的 PG poll 循环和 separate budget controller，增加了系统复杂度（boot 时多一个 tokio::spawn，但代码却没有统一到 BusConsumer trait）。这不是功能性缺陷，是架构一致性问题。迁移成本不高，但优先级不高。

2. **REST + WS 双层 API**：消息编辑同时有 REST `PATCH /api/messages/:id` 和 WS `editMessage`，二者最终结果相同（持久化 + 扇出 RoomEvent）。这增加了维护负担——文档、测试、权限校验需要同步。但考虑到历史原因（REST 是 CRUD 接口的天然选择，WS 提供实时反馈路径），这种双重覆盖在 IM 系统中是常见实践。不是债务，但需要警惕两端歧义（例如 REST 路径可能漏了某些 WS 路径已有的校验）。

3. **call-bridge 寻址依赖 Redis heartbeat 而非服务发现**：`call_route`/`stream_route` 心跳（30s）在 Redis 中维护本节点可寻址性。这是 IM 领域常见实践，但心跳间隔 30s 意味着节点故障最坏延迟 30s 才能从路由表中移除——在此期间桥接请求会发向死节点。对于通话，30s 中断可能产生可感知的呼叫延迟。如果未来需要 <5s 的故障转移，需要改为基于 membership 的 gossip 或 dedicated failure detector。

### 架构债务

基于验证评估，我识别出以下技术债：

| 债务 | 严重程度 | 描述 |
|------|---------|------|
| **前端 DOM 无上限增长** | **高** | 单次会话 DOM 节点数随消息线性增长，无回收/虚拟滚动。长时间运行会话（1 天+）会 OOM。不是「明天崩溃」，但确定性崩溃。 |
| **WS 上行无确认** | **高** | 12+ 个 `ws.send*()` 调用点全部忽略返回值。后端有 at-least-once，但边界丢消息。乐观渲染让问题更严重：`send()` 失败后乐观条目永久残留。 |
| **merge_hits 使用 max-score 而非 RRF** | **中** | `routes/helpers.rs` 的 `merge_hits` 用 max-score 合并 FTS 和向量搜索结果，返回结果集质量较低。不过已有现成 `fuse_rankings`（RRF）在 `aero-ai::rerank` 中可用。 |
| **CI 被注释阻断** | **中** | `.github/workflows/ci.yml` 全部 job 已定义但被注释/注意块阻断。剩下 6 个 job？实际上阻止了 PR 质量门禁。 |
| **前端`calls.js:74-76`硬编码 Google STUN** | **中** | 只回退到公共 STUN，无 TURN relay。在对称 NAT 或防火墙场景下，P2P ICE 会失败，通话完全不可用。 |
| **头重脚轻可靠性哲学** | **结构性** | 后端有幂等键、预算、at-least-once 语义；前端无确认、无重试、无队列。系统整体可靠性取决于最弱环节。 |

---

## 2. 扩展方向

### 方向一：前端消息可靠性层（P0）

**为什么需要**：验证评估已展示，12+ `ws.send*()` 调用点全部忽略返回值。在一次网络抖动中，消息、编辑、反应静默丢失，用户不可知。这是 IM 产品的基本可用性缺陷——核心体验（发消息）无可靠性保障。

**核心挑战**：
- 出站帧需要在 `send()` 返回 `false` 后排队，WebSocket 重连后重放
- 需要区分「已发送未确认」和「已确认」两个状态，在 UI 层给出视觉指示（单钩 vs 双钩）
- 重连后重放出站帧的顺序性和幂等性（后端已基于 seq 实现了幂等，但前端不能依赖这个）
- 乐观渲染需要和确认回执配合——确认前显示「发送中」指示，确认后转为「已发送」

**预期的架构变更**：

```
当前：  用户输入 → send() (无视返回值) → 乐观追加到 DOM
目标：  用户输入 → 入出站队列 → send() 
            ├── 成功 → 等待 ack → 确认渲染（可选双钩）
            └── 失败 → 保持队列 → 触发重连 → 重放 → 等待 ack
```

在前端 `WsClient` 中引入显式出站帧队列 + Ack 表：
- `_outbox: Frame[]` — 未发送帧（send() 返回 false 时入队）
- `_waitingAck: Map<seq, Frame>` — 已发送等待确认
- 每次 `_open` 后 drain `_outbox`
- `onMessage` 收到 ack 后从 `_waitingAck` 移除，通知 UI 更新状态

**后端配合**：所有 mutating WS 帧的响应需要在 ServerFrame 中包含 `ack_seq`（消息 ID 回显也可），这样前端可以将确认帧关联到出站帧。当前 `Message` 事件的响应中已有 `id`，但编辑/删除/反应的确认可能需要显式 ack。

**对现有系统的影响**：
- 前端 `WsClient` 需要中等重构（100-200 行）
- 后端 WS ServerFrame 需要为所有 mutating 操作增加 ack 字段（可选，不做也 OK 但确认不完整）
- 乐观渲染逻辑需要改为「先入队等待确认后转换状态」
- 不需要修改后端事件总线或持久化层
- 需要 UI 改动（消息状态指示器）

**工作量估计**：3-5 天（前端重构 + 后端 Ack 字段扩展 + UI 适配 + 网络抖动测试）

---

### 方向二：消息 DOM 虚拟化/回收（P0 — 补救；P1 — 完整）

**为什么需要**：验证评估已证明，`appendMessageEl` 无上限追加 DOM，长时间运行确定性 OOM。这不是优化，是止损。

**核心挑战**：
- 纯前端 DOM 列表的虚拟滚动需要正确测量行高（可变高度消息内容、媒体、线程指示）
- 「滚动到最旧消息」和「自动滚动到底部」之间的交互——虚拟滚动通常切断上端，但用户可能向上翻阅历史
- 媒体消息的占位高度 vs 加载后高度的重计算
- 消息搜索定位（搜索命中后需要 scrollIntoView）

**分阶段架构**：

阶段 1（快速修复，< 1 天）：在 `appendMessageEl` 中加 DOM 数量守卫（如验证评估建议的 `while > 500` 移除 `firstChild`）。这会丢失消息，但在达到极限前防止 OOM。附加一个可选的「加载更早消息」按钮让用户恢复旧消息。

阶段 2（完整虚拟列表，2-3 天）：基于 IntersectionObserver 的有限可见窗口，每次渲染 ~60 条消息。

**架构变更**：
```
当前：  msgList(div, flex column) ──[append]-> newMessage
                                    ──[replaceChildren]-> rerender

目标：  VirtualList 组件
          ├─ sentinelTop (IntersectionObserver ← trigger loadOlder)
          ├─ visibleWindow (30-60 条，动态渲染)
          ├─ sentinelBottom (IntersectionObserver ← trigger loadNewer/autoScroll)
          └─ scrollRestoration 缓存 (切换房间时保留 scrollTop + window range)
```

**对现有系统的影响**：
- 主要影响前端代码，后端无变化
- `rerenderCurrentRoom()` 需适配虚拟列表（不再 `replaceChildren`，而是重置窗口位置）
- 搜索/线程跳转需要额外 scroll-to-virtual-position 逻辑
- 阶段 2 可能和出站帧队列冲突（确认后消息需要从列表移除或变更状态）——建议先完成可靠性层，再做虚拟滚动

---

### 方向三：全量 CI 激活 + 质量门禁（P0）

**为什么需要**：验证评估发现 `.github/workflows/ci.yml` 被注释阻断。目前 7 个 job（check/test/size/truth/web/dependency/security）全就绪但未启用。这意味着：
- 单人开发时 `cargo check/cipppy/test` 全靠自觉
- 合并冲突、回退、pedantic 警告无法自动捕获
- GPT/agent 生成的代码可能引入 compile 错误（已发生）
- 没有 PR 自动验证 = 没有工程纪律

**架构变更**：
- 移除 CI yml 顶部的注释头和「注意」块（对 GitHub Actions 而言，注释头是纯注释）
- 对于需要 PG/Redis/NATS 的 `integration-test` job，用 GitHub Actions services 关键字启动容器，无需自建 runner
- 在 `ci.yml` 中设置 `AERO_RATE_LIMIT_PER_SEC=99999` 等 env vars 避免 429
- CI 通过后可在 README 加 CI badge

**对现有系统的影响**：零——纯运维层改动。所有 job 已定义，只是被注释导致未运行。

---

### 方向四：TURN relay 基础设施（P1）

**为什么需要**：`calls.js` 回退到公共 STUN，但在企业 NAT/对称 NAT/移动网络下，P2P 媒体路径不可信。没有 TURN relay 时，在这些网络环境下的通话确定性失败。

**核心挑战**：
- 需要自建 coturn 或第三方 TURN 服务（Twilio、metered.ca、Cloudflare Calls）
- TURN 凭证需要短期（3600s？）HMAC 签名，不能硬编码静态密码
- 带宽成本——relay 流量会经过 TURN 服务器，需要计费规划
- TURN 服务器需要公开 IP 和端口（3478/5349），增加了攻击面

**架构变更**：
```
当前：  rtcConfig.ice_servers = [stun:stun.l.google.com:19302]
目标：  rtcConfig.ice_servers = [
          { urls: "stun:stun.l.google.com:19302" },
          { urls: "turn:turn.aero-im.example.com:3478", username: "xxx", credential: "yyy" }
        ]
```

正如验证评估指出的，`aero-signaling` 的 `RtcConfig`/`IceServer` 类型已经包含 `username`/`credential` 字段，API schema 不需要改。需要改的：

1. `docker-compose.yml` 添加 coturn 容器（或 `provision-tools` + 外部 TURN 提供商）
2. `aero-server` 的 boot 阶段填充 `rtc-config` 端点返回值中的 TURN URLs + HMAC 凭证
3. 前端代码不需要改（已从 `state.rtcConfig.ice_servers` 动态读取）
4. 生产环境需要暴露 3478/5349 端口 + TLS 证书（TURNS）

**对现有系统的影响**：
- 后端 API schema 无变化（已验证 `IceServer` 已支持 creds）
- 前端无代码改动
- 运维复杂度增加（需要维护 coturn 或管理第三方 TURN 帐号）
- 如果走自建 coturn，`docker-compose.yml` 增加一个容器，总容器数从 4→5

---

### 方向五：搜索质量的混合排名统一（P1）

**为什么需要**：验证评估展示了 `merge_hits`（max-score）被搜索路由用于 hybrid 模式，而真正的 RRF 实现 `fuse_rankings` 已在 `aero-ai::rerank` 中但未被搜索使用。这虽是 3 行代码的修复，但深入看暴露了一个架构问题：搜索和 AI 管道各自维护了独立的排名逻辑。

**架构变更**：
- `routes/helpers.rs` 中 `merge_hits` 保持作为 `"hybrid_max"` mode（明确命名的退路），添加 `"hybrid_rrf"` mode 调用 `fuse_rankings`
- 将 `fuse_rankings` 从 `aero-ai::rerank` 移动到 `aero-common`（或者路由可访问的公共位置），因为搜索路由不应该依赖 AI crate
- 可选：将 `merge_hits` 标记为 deprecated，默认 hybrid mode 指向 RRF

**工作量**：1 天（代码移动 + 路由适配 + 测试 + 注释更新）

---

## 3. 接口设计建议

### 关键原则

**基于验证评估的发现，接口设计需要强调三个原则**：

**原则一：所有出站路径必须返回给调用方可观察的状态（P0 缺失）**

当前 `ws.send*()` 返回 `boolean` 但调用方忽略。这违反了最小惊讶原则。建议：

```typescript
// 当前
ws.sendMessage(roomId, blocks, replyTo) // → boolean (无视)

// 建议
ws.sendMessage(roomId, blocks, replyTo) // → Promise<SentState> 
// SentState = { pending | sent(seq) | failed(attempts) | acked(serverId) }
```

返回一个 Promise 使得：
- 调用方可以选择 `.then()` 或 `await`（逐步引入，向后兼容）
- 乐观渲染可以挂载到 `sent(seq)` 状态
- 超时后自动转为 `failed`，UI 显示感叹号 + 重试按钮

如果 Promise 对现有代码改动太大，至少提供一个 Observable/EventEmitter：

```typescript
// 最小改动
ws.on('outbox-status', (seq, state) => { ... });
// ws.send() 内部 emit 状态变更，调用方可选择监听
```

**原则二：前端和后端之间的契约必须有一个显式的「确认层」**

当前，后端在每个 `RoomEvent` 中嵌入 seq（`ImService::publish_room_event`），前端用 seq 去重/排序。但 seq 只在事件扇出中存在，不在全双工控制帧中存在。

建议引入 `AckFrame`（WS ServerFrame variant）：

```typescript
// 前端发送 mutation 时携带临时 seq（tempId）
ws.send({ type: 'message', tempId: 't1', room: 'r1', content: [...] })
// 后端处理完成后：
ws.send({ type: 'ack', tempId: 't1', serverId: 'msg-uuid', seq: 12345 })
```

这样前端可以：
1. 将 `tempId` 与乐观条目关联
2. 收到 ack 后转为确认状态  
3. 在超时（如 5s）后显示重试按钮

**原则三：搜索排名逻辑应通用化，不绑定到单一 crate**

`fuse_rankings` 是通用 RRF 实现，不应藏在 `aero-ai` crate 内。建议将通用排名/融合工具函数收到 `aero-common` 的 `src/search/` 下（如果 `aero-common` 的依赖不引入额外工具链）或独立 `aero-search` crate。

当前状态导致：
- 搜索路由（`aero-server` 的一部分）需要 fix `aero-ai` crate 来获得 RRF 功能
- 任何新的排名算法复用需要依赖 AI crate 或复制代码
- 搜索和 AI 的测试覆盖率不同步

### 是否需要新的抽象层

**需要：前端 WS 连接管理器（抽象出可靠传输层）**

当前 `WsClient`（`ws.js`）已经包含重连、seq gate、通知路由等基础设施。但它暴露的是裸 `send()` 方法，调用方面临的是「不可靠传输」。

建议在 `WsClient` 之上增加一层 `OutboundReliableLayer`：

```typescript
interface IOutboundChannel {
  // 所有出站帧从这里发送
  send(frame: ClientFrame): Promise<SendReceipt>;
  // 调用方等待确认
  waitForAck(tempId: string, timeout?: number): Promise<AckFrame>;
  // 出站队列的可见状态（用于 UI）
  getOutboxStatus(): SentMessageStatus[];
}
```

这层封装将重连、重试、确认匹配、超时全部收口。`WsClient` 保持底层职责（字节流、连接生命周期、seq 验证），`OutboundReliableLayer` 处理消息可靠性。

**不需要：后端的「通用 Worker trait」**

当前 bot（agent_bot/ooo_bot/unfurl_bot/transcribe_bot）各自消费 NATS subject、各自处理幂等/错误。虽然存在模式重复（解 JSON → 校验 → 执行 → 成品扇出 → ack），但 bot 之间的逻辑差异大到泛化收益有限。强行引入 `trait Bot` 会增加抽象泄漏风险。现有模式是好的——函数式、每个 bot 一个模块、明确的不变量写在注释中（`AGENTS.md §2`）。

### 向后兼容性

对于方向一（前端可靠性层）：

| 阶段 | 后向兼容 |
|------|---------|
| `send()` 改为返回 Promise（不做则保持 boolean） | **破坏**如果已有代码依赖 `send()` 返回 boolean。但验证评估显示所有调用方忽略返回值，所以无影响。 |
| 新增 `ack` ServerFrame variant | **兼容**旧前端忽略未知 variant（后端 serde 已经有 `deny_unknown_fields`？需要检查 `ServerFrame` 的 serde 标记） |
| 出站队列 | **兼容**纯前端改动 |

如果担心破坏性，可以在 `WsClient` 上新增可选的 `.sendAndWaitAck()` 方法，同时保留现有 `send()`。逐步迁移调用点。

---

## 4. 技术选型

### 是否需要引入新技术栈

**不需要（短期）**。验证评估中发现的 5 个方向全部可以在现有栈上修复：

| 方向 | 需要新依赖？ | 理由 |
|------|------------|------|
| 前端可靠性层 | 不 | 纯 vanilla JS 实现出站队列 + Promise。无需 rxjs/immer 等。 |
| DOM 虚拟化 | 不 | 核心栈已定义（vanilla ES2020 SPA），虚拟滚动可基于 `IntersectionObserver` 手写。不引入 Web Component 框架。 |
| CI 激活 | 不 | GitHub Actions + ubuntu-latest，无需自建 runner。需要 PG/Redis/NATS 时用 `services` 关键字启动容器。 |
| TURN relay | 部分 | 自建 coturn（docker image）+ 现有 API。不引入外部 SDK。如果需要第三方 TURN，考虑 Twilio Network Traversal Service（API 轻，按量计费）。 |
| 搜索排名统一 | 不 | 纯 Rust 重构，现有 `fuse_rankings` 移动到 `aero-common`。 |

**中期可能需要的技术栈**：

1. **性能监控**：当前有 Prometheus + OTLP gauges（`observability_gauge_samplers`），但缺少前端性能监控（FP/FCP/LCP，WebSocket 延迟分布）。如果前端可靠性层部署后需要验证 QoE，考虑引入 **web-vitals**（~1KB、零依赖）或自建简单打点上报。

2. **Web Socket 压力测试**：当前测试覆盖后端事件驱动和 REST 路径，但 WebSocket 端到端的压力测试缺失（方向二暴露的问题与此相关）。现有工具链（`cargo test` 的 WS client）可能不够做长时间运行测试。可以考虑 **autobahn-testsuite**（WebSocket 合规性测试）或基于 tokio 的 WS fuzzer。

3. **通话质量监控**：如果方向四（TURN）部署后通话成为核心功能，需要为 WebRTC stats（`getStats()` 返回的 roundTripTime/packetsLost/jitter）建立上报管道。这部分纯前端获取 + 后端收纳到 Prometheus 或单独的 TSDB。不需要新的大型框架，但需要定义 `RTCStatsReport` 的采集/聚合 schema。

### 第三方依赖评估标准

基于项目当前的治理水平（`deny.toml`、`Cargo.lock`、MSRV 1.80），评估新依赖的标准应明确为：

| 维度 | 检查项 | 最低接受 |
|------|--------|---------|
| 安全 | 审计历史、维护者活跃度、依赖链长度 | cargo deny 通过、无 unsound issue、Mozilla Observatory A |
| 许可 | license 兼容性 | MIT/Apache-2.0/BSD-2-Clause/Unlicense；排除 GPL/AGPL/LGPL（除非作为独立二进制动态链接） |
| 稳定性 | semver 兼容性、预 1.0 库的风险 | 稳定 > 1.0 × 6 个月，或预 1.0 但有明确 roadmap |
| 运行时足迹 | 二进制尺寸、内存、编译时间 | `aero-server` 编译增量 < 30s、静态二进制 < 100MB |
| 替代性 | 手写维护成本 vs 引入依赖 | 手写 ≤ 2 周 / 依赖 ≤ 500 LOC / 依赖已有 > 2 年维护经历的优选 |

当前评估：

- 对 `async-nats 0.36`（预 1.0）的依赖是合理的——NATS 没有满足同类需求的 Rust 1.0 替代品。风险可以通过锁定 `Cargo.lock` + CI 定期更新 + 问题追踪来管理。
- 对 `str0m 0.19`（预 1.0，纯 Rust WebRTC）的依赖同理——项目活跃、API 设计质量高、被 FFmpeg/webrtc-rs 等替代品需要 C++ 绑定。但 `str0m` 的 API 仍在频繁变更（每个 minor 版本有 breaking change），这是已知的维护成本。

---

## 5. 实施路线图

### 优先级排序（P0/P1/P2）

| ID | 方向 | 优先级 | 理由 |
|----|------|--------|------|
| P0.1 | CI 激活 | P0 | 零风险、零代码改动、立即可做。所有 job 已定义，仅移除注释。质量门禁是一切后续变更的基础。 |
| P0.2 | DOM 快速回收（阶段 1） | P0 | 确定性 OOM 风险，单行修复可立即止损。完整虚拟滚动可以稍后。 |
| P0.3 | WS 上行确认 | P0 | 核心 IM 体验的可靠性缺陷。12 个调用点，每个都可能丢消息。 |
| P1.1 | DOM 完整虚拟化（阶段 2） | P1 | 阶段 1 止损后，需要完整虚拟滚动来满足长期运行场景。但优先级略低于可靠性，因为「丢 DOM」好于「丢消息」。 |
| P1.2 | RRF 修复（merge_hits → fuse_rankings） | P1 | 3 行代码 + 测试，< 1 天。直接提升搜索质量。优先级高但「先发不准确的结果」不是灾难性的。 |
| P1.3 | TURN relay | P1 | 覆盖企业 NAT/防火墙用例，但当前代码还不支持视频通话的端到端（str0m SFU 未在生产接线），所以通话本身还不是生产特性。TURN 可以等通话功能验证后再做。 |
| P2.1 | 搜索排名工具通用化（移动 fuse_rankings） | P2 | 重构优先级低，因为当前 `aero_ai::rerank` 的依赖虽然不合适但不会导致运行时错误。 |
| P2.2 | AiWorker 迁移到 NATS consumer | P2 | 架构一致性改善，但当前基于 PG 轮询的方式功能正确且已在生产稳定运行。迁移带来收益（统一工作模式）但风险（重新验证边 case）。 |

### 阶段划分

**阶段 0（稳定基线）[1-2 天]**

目标：建立自动化质量保障，解决最紧急的生产风险。

| 步骤 | 产出 | 风险 |
|------|------|------|
| 激活 CI yml | 所有 PR 自动跑 check/test/clippy/truth/size/web/dependency/security | 基于 ubuntu-latest，services 容器拉取需 ~2min。PG/Redis/NATS 端口映射正确即可。 |
| DOM 快速回收（`while > 500 removeFirstChild`） | DOM 不再无限增长 | 消息多于 500 条会从顶部消失。需在阶段 1 完整虚拟滚动前接受。可同时添加「加载更早消息」按钮补偿。 |
| CI 通过后添加 CI badge 到 README | 可见性提升 | — |

**阶段 1（前端可靠层）[3-5 天]**

目标：WebSocket 出站消息不再静默丢失，用户获得可靠发送体验。

| 步骤 | 依赖 | 风险 |
|------|------|------|
| `WsClient` 新增 `_outbox` 队列 + `_waitingAck` 表 | 无 | 增加客户端内存使用（队列中的暂态帧）。上限设 100 帧防 DoS。 |
| `send()` 在连接断开时入队 + 触发重连 | `_reconnectTimer` 已存在 | 重连风暴——如果网络反复断连，队列可能堆积。建议给出站队列独立的上限（50 帧）+ 背压丢弃时通过回调通知 UI。 |
| 所有 `ws.send*()` 调用点插入 send() 返回值检查 | ws.send*() 需逐个修改 | 12 个调用点，每个需要改动。容易遗漏——建议用 lint rule 或 grep + 代码 review 逐个确认。 |
| 可选：新增 `status-indicator` UI（消息气泡的「发送中/已发送」图标） | 出站队列 | 额外 UX 工作，但不影响可靠性。可以等虚拟滚动后再做。 |

**阶段 2（搜索 + 通话）[2-3 天]**

| 步骤 | 依赖 | 风险 |
|------|------|------|
| `merge_hits` 替换为 `fuse_rankings`（或默认 hybrid→RRF） | `aero_ai::rerank` 可用 | 回归——需要确保 RRF 在边界情况（空结果、单结果、同分）下不比 max-score 差。现有单测覆盖重叠取胜，但需补充搜索路由集成测试。 |
| RRF 后保留 `merge_hits` 作为 `hybrid_max` mode | 方向三修复 | 路由参数文档需要更新。不带 `?mode=hybrid_max` 默认为 RRF。 |
| docker-compose 加 coturn | 网络拓扑无冲突 | coturn 默认端口 3478/5349 与现有服务不冲突（现有：5432/6379/4222/9000）。需决定自建 vs 第三方。自建增加一个容器，适合生产独立部署。 |
| `rtc-config` 端点填充 TURN URLs | coturn 部署完成 | TURN 凭证短期 HMAC 签名需要在 aero-server 中实现。如果走第三方 TURN（如 Twilio），需要 API 密钥管理和计费追踪。 |

### 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| **CI 激活后 flakes** | **高**（首次 CI 运行大概率有未预期问题） | **中**（延缓后续开发） | 阶段 0 不应引入大代码改动，只激活 CI + DOM 快速修复。如果 CI 有 flakes（如 `integration-test` 依赖 services 容器启动顺序），给足 2 天缓冲。设置 `continue-on-error: true` 给修复余地。 |
| **出站队列与乐观渲染冲突** | **中** | **中**（用户看到「已发送」图标但消息延迟出现） | 设计规则：乐观渲染立即显示（无论 send() 状态），队列重放时不重复追加。在 `pendingByTempId` 中跟踪重放状态。 |
| **乐观条目残留** | **低**（当前行为——send() 失败后条目永远残留） | **中**（用户迷惑） | 在 send() 返回 false 后，标记条目为 `failed` 状态（样式变化 + 重试按钮），而不是保持 `pending`。超时（30s）后从 DOM 移除并显示通知。 |
| **出站队列重放顺序** | **低**（NATS per-subject seq 确保后端顺序） | **高**（消息乱序） | `_outbox` 按入队顺序发送（FIFO），收到 ack 前不发送下一帧？这会降低吞吐。更实际的做法：发送时携带 `clientOrder`（monotonic counter），后端按此排序。 |
| **coturn 暴露端口被扫描** | **高** | **中** | coturn 应在 `--no-tcp-relay` 模式运行（UDP only），使用 `--fingerprint` 选项。如果自建，禁止 TCP relay（减少攻击面）。端口暴露通过 docker-compose 的 `ports:` 映射即可——不新建网络暴露。 |
| **fuse_rankings 移动到 aero-common 导致 crate 重构冲突** | **低** | **中**（重构难度低但分支管理恶化） | 先不移，直接在路由中 `use aero_ai::rerank::fuse_rankings` 做修复。重构到 `aero-common` 标记为 P2 后续。 |

---

## 总结

这份验证评估文档揭示了一个系统性问题：**Aero IM 的后端基础设施达到生产级可靠性标准，但前端 WebSocket 链路在「最后一公里」断裂**。后端的 at-least-once 语义（NATS 持久性、幂等键、预算控制器、优雅重连）到前端边界退化成了「可能丢失」——因为 `ws.send()` 的返回值被全部忽略。

这不是「前端没做完」，而是两个子系统用不同的可靠性哲学建造。修复路径清晰：

1. **立即止损**（阶段 0）：CI 激活 + DOM 数量守卫，两者都可在 1-2 天内完成且无架构风险。
2. **修复可靠性裂缝**（阶段 1）：出站帧队列 + ack 确认 + UI 状态指示，3-5 天。这是架构真正的「头重脚轻」问题修复。
3. **补齐功能缝**（阶段 2）：RRF 搜索排序（1 天）+ TURN relay（2 天），提升搜索和通话质量。

整体的技术债中，**前端可靠性层的缺失是最严重的**——因为它直接作用于核心 IM 体验（发消息），且后端已有投入无法通过前端弱点被用户感知。其余方向（搜索排名、CI、TURN、DOM 虚拟化）都是增量改进，不是系统性问题。
