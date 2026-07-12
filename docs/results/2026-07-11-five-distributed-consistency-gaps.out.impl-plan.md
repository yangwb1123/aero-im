现在我已经掌握了所有需要的上下文。以下是全面的技术负责分析。

---

# Tech Lead 分析：5 个分布式一致性差距

**分析日期**: 2026-07-12
**范围**: 5 个功能方向，涵盖 ~46K Rust / ~5.3K JS / 16 个 crate / 157 次迁移
**代码验证**: 根据 `2026-07-11-five-distributed-consistency-gaps.out.md` 中的实际代码检查进行校正（85-98% 精确度）

---

## 1. 任务分解

### TASK 清单

#### 方向一（P0，已评级 85%）——事件投递顺序

| ID | 标题 | 文件 | 前置依赖 | 预估工时 | 验收标准 |
|----|------|------|---------|---------|---------|
| **TASK-001** | 客户端 `handleEdited`：幽灵消息守卫 | `web/app.js:295-312` | — | 1.5h | 编辑发生在原始消息之前时，`replaceNodeForMsg` 不会 `appendChild`；幽灵节点为零回归 |
| **TASK-002** | `SeqGate` 的范围从去重扩展到排序 | `web/ws.js:18-39` | — | 3h | `SeqGate` 提供 `accept(scope, seq)`，在应用之前按 seq 对事件进行排序；编辑/删除总是应用于 ≥ 原始消息 seq |
| **TASK-003** | 审核→交付屏障（两阶段交付路径） | `crates/aero-server/src/ws/ws_impl/bus.rs`，`crates/aero-server/src/moderation_bot.rs` | — | 4h | 在 `handle_room_event_sub` 中，软删除的消息在 ack 到 WS 扇出之前要对照 DB `deleted_at` 进行双重检查 |
| **TASK-004** | 投递的慢消费者顺序保护 | `crates/aero-server/src/ws/ws_impl/bus.rs` | TASK-003 | 3h | WS 扇出在 120 秒落后时跳过陈旧的 (seq < 高水位线) 事件；客户端 SeqGate 忽略陈旧的事件 |
| **TASK-005** | Webhook dispatcher 屏障（待消息扇出完成后再触发 Webhook） | `crates/aero-server/src/webhooks.rs` | TASK-003 | 2.5h | Webhook 投递延迟 ≤ WS 扇出 + 500ms 或直到收到 seq 确认 |

#### 方向一（校正）——额外客户端修复

| **TASK-006** | Reconnect change-replay 竞态条件修复 | `web/app.js:323-328` | TASK-001 | 2h | `applyChange` 检查 `state.messagesByRoom` 以及 DOM；在正在进行的回填期间，断线重连不会丢失编辑内容 |

#### 方向二（P1，已评级 95%）——搜索索引一致性

| ID | 标题 | 文件 | 前置依赖 | 预估工时 | 验收标准 |
|----|------|------|---------|---------|---------|
| **TASK-007** | 小消息的内联 re-embedding | `aero-im-core/src/service/messages.rs` | — | 4h | 对于 ≤4 个块/≤2KB 的消息，`edit_message` 同步计算并更新 `embedding`（嵌入内容）；更大的消息退回到 `ai_jobs` |
| **TASK-008** | `embedding_version` 列 + 搜索降级逻辑 | `migrations/`，`aero-storage/src/message/search.rs` | TASK-007 | 4h | 新的 `messages.embedding_version` 列（默认为 0）；搜索返回 `embedding_version < messages.version` 的消息时仅使用 FTS；向量查询会合并遗漏值 |
| **TASK-009** | 删除后清理 embedding | `aero-im-core/src/service/messages.rs`，`aero-storage/src/message/crud.rs` | TASK-008 | 2h | `delete_message` 同步设置 `embedding = NULL`；搜索排除 `embedding_version = 0` 的空向量 |
| **TASK-010** | 图片标题更改的 image re-embedding | `aero-im-core/src/service/messages.rs` | TASK-007 | 1.5h | `edit_message` 为搜索重建纯文本块时，将 `searchable_text` 为非空的图片消息入队到 `Embed`；否则为图片标题编辑跳过（非空守卫） |

#### 方向三（P1，已评级 80%，顺序更正）——通话生命周期一致性

| ID | 标题 | 文件 | 前置依赖 | 预估工时 | 验收标准 |
|----|------|------|---------|---------|---------|
| **TASK-011** | 为 `join_group_call` 实现 Saga 补偿 | `aero-im-call/src/lib.rs:356-440` | — | 5h | `add_peer` 成功后如果 `register_and_decide` 失败，调用 `remove_peer` 作为补偿；每个步骤记录其自己的本地状态以进行补偿 |
| **TASK-012** | 定期 SFU ↔ Redis 路由对账 | `aero-im-call/src/lib.rs`，`aero-server/src/call_bridge_supervisor.rs` | TASK-011 | 4h | 后台循环与 `ensure_bridges` 心跳一起运行；比较 `SfuRouter::participants(call)` 与 `CallRouteStore::nodes_for_call(call)`；清除孤立的 Redis 条目并删除孤立的 SFU peer |
| **TASK-013** | 修复 `end_call` / `join_group_call` 的竞争条件 | `aero-im-call/src/lib.rs:end_call` | TASK-011 | 3h | `end_call` 首先读取 Redis（事实源），然后在 DB 清理之前检查并发连接；乐观锁方案（`version` 列在 `call_sessions` 中） |
| **TASK-014** | 为通话生命周期操作添加幂等 ID | `aero-im-call/src/lib.rs`，`aero-storage/` | TASK-011 | 3h | 每个 `join`/`leave`/`end` 都有一个全局 UUID `operation_id`；跨存储操作以此为键是幂等的；通过 `ON CONFLICT DO NOTHING` 实现至少一次安全 |

#### 方向四（从 P2 升级到 P1，已评级 98%）——多节点缓存一致性

| ID | 标题 | 文件 | 前置依赖 | 预估工时 | 验收标准 |
|----|------|------|---------|---------|---------|
| **TASK-015** | 在 `fan_out_raw` 之前添加成员重新校验 | `crates/aero-server/src/ws/ws_impl/bus.rs:119` | — | 5h | `handle_room_event_sub` 在调用 `fan_out_raw` 之前，对每个成员执行轻量级 Redis `SISMEMBER room:{id} {pid}`；60 秒过扇出窗口缩小到 ≤2 秒（一次网络往返） |
| **TASK-016** | Redis Pub/Sub 缓存失效广播 | `crates/aero-server/src/room_member_cache.rs`，`crates/aero-server/src/participant_cache.rs` | TASK-015 | 4h | 节点 A 上的成员变更发布到 Redis `__keyspace@0__:room:{id}` 或专用 Pub/Sub 频道；所有节点的热监听器使本地缓存条目失效；缓存未命中延迟 < 重取的时间 |
| **TASK-017** | 删除 `room_member_cache` 中具有误导性的注释 | `crates/aero-server/src/room_member_cache.rs:16-18` | — | 0.5h | 注释更改为：*“没有下游 auth gate。在 `fan_out_raw` 之前，TASK-015 添加了 Redis 重新校验，以缩小过扇出窗口。”* |
| **TASK-018** | 修复 `online_list` 以聚合所有节点 | `crates/aero-server/src/online.rs`，`aero-storage/src/presence.rs` | TASK-016 | 3h | `GET /api/rooms/:id/online` 查询 Redis presence 以获取集群范围的在线视图（不仅仅是 `state.hub.room_members_online`） |

#### 方向五（P2，已评级 95%）——推送通知与 WS 的协调

| ID | 标题 | 文件 | 前置依赖 | 预估工时 | 验收标准 |
|----|------|------|---------|---------|---------|
| **TASK-019** | Presence 感知推送跳过 | `crates/aero-server/src/push_bot.rs` | — | 3h | `push_bot` 在发送前检查 Redis 中参与者的在线状态：用户在桌面端（或任何设备）上 30 秒内活跃 → 跳过推送 |
| **TASK-020** | push_bot 的 DND/snooze 门控 | `crates/aero-server/src/push_bot.rs`，`aero-storage/src/notification_prefs.rs` | TASK-019 | 2h | `push_bot` 在发送推送前检查参与者的 `notif_prefs`（DND 窗口、snooze 截止时间）；与 WS 端门控逻辑对齐 |
| **TASK-021** | Redis 跨设备通知状态 | `aero-storage/src/push_token.rs`，`crates/aero-server/src/push_bot.rs` | TASK-019，TASK-020 | 4h | `notif:{user_id}:{notif_id} → {status}` 在 Redis 中；push_bot 和 WS 共享此状态以避免重复投递；设备在渲染通知时将其标记为“已消费” |
| **TASK-022** | 推送聚合（批处理） | `crates/aero-server/src/push_bot.rs` | TASK-021 | 3h | 将来自同一会话的多个通知聚合为一条 FCM 数据/通知消息；使用 FCM `collapse_key` + 自定义聚合逻辑（每会话 1 条推送/5 秒） |

---

## 2. 执行顺序

```
graph TD
    subgraph "Phase-0: Quick Wins (Week 1)"
        T001[TASK-001: handleEdited ghost guard]
        T017[TASK-017: Fix misleading cache comments]
        T006[TASK-006: Reconnect change-replay fix]
        T009[TASK-009: Clean embedding on delete]
        T010[TASK-010: Image caption re-embed fix]
    end

    subgraph "Phase-1: Core Consistency (Week 2-3)"
        T015[TASK-015: Member re-check before fan_out_raw]
        T016[TASK-016: Redis Pub/Sub cache invalidation]
        T003[TASK-003: Moderation→delivery barrier]
        T002[TASK-002: SeqGate ordering buffer]
    end

    subgraph "Phase-2: Call & Search (Week 3-4)"
        T011[TASK-011: Saga for join_group_call]
        T014[TASK-014: Idempotent call operations]
        T007[TASK-007: Inline re-embed for small edits]
        T008[TASK-008: embedding_version + search degrade]
    end

    subgraph "Phase-3: Remaining Gaps (Week 4-5)"
        T012[TASK-012: SFU↔Redis reconciliation]
        T013[TASK-013: end_call / join race fix]
        T004[TASK-004: Slow-consumer seq protection]
        T005[TASK-005: Webhook delivery barrier]
        T018[TASK-018: Cross-node online aggregation]
    end

    subgraph "Phase-4: Push & Polish (Week 5-6)"
        T019[TASK-019: Presence-aware push skip]
        T020[TASK-020: DND/snooze gating in push_bot]
        T021[TASK-021: Cross-device notification state]
        T022[TASK-022: Push batching]
    end

    %% Dependencies
    T015 --> T016
    T003 --> T004
    T003 --> T005
    T002 --> T006
    T007 --> T008
    T011 --> T012
    T011 --> T013
    T011 --> T014
    T019 --> T020
    T019 --> T021
    T021 --> T022

    %% Phase boundaries
    T001 -.-> |"Phase-0 done"| T015
    T016 -.-> |"Phase-1 done"| T011
    T015 -.-> |"Phase-1 done"| T019
    T016 -.-> |"Phase-1 done"| T018

    style T001 fill:#90EE90
    style T015 fill:#90EE90
    style T016 fill:#90EE90
    style T011 fill:#90EE90
    style T019 fill:#90EE90
```

**关键可并行化的组：**

| 并行组 | 任务 | 理由 |
|----------|------|---------|
| **客户端修复** | TASK-001，TASK-002，TASK-006 | 纯 JS，无服务器依赖；可由前端人员并行处理 |
| **缓存修复** | TASK-015，TASK-016，TASK-017，TASK-018 | 密集共享的 Redis 和 Hub 基础设施；最好由同一人按顺序处理 |
| **通话 Saga** | TASK-011，TASK-014 | 核心通话编排逻辑；与搜索并行（TASK-007，TASK-008） |
| **搜索索引** | TASK-007，TASK-008，TASK-009 | 与通话任务无共享依赖；单独的 crate |
| **推送通知** | TASK-019，TASK-020，TASK-021，TASK-022 | 除 TASK-015/016（Redis 存在性）外无共享依赖；可稍后启动 |

---

## 3. 技术风险

### 3.1 高风险项目

| # | 风险 | 方向 | 影响 | 缓解措施 |
|---|------|------|------|---------|
| R1 | **客户端 SeqGate 排序引入延迟**：如果在排序缓冲区中等待的时间过长，消息会明显延迟到达 UI | D1 | 用户感知到的消息延迟从 ~100ms → 3-5 秒 | 可配置的排序超时（默认 500ms）；后台 RAG/搜索使用 seq 栅栏而非实时 WS；设置每房间最大缓冲区大小 |
| R2 | **Redis `SISMEMBER` 热键**：在高流量频道中，每个事件在每个节点上都调用 `SISMEMBER room:{id} {pid}` | D4 | Redis 延迟增加，可能达到每秒数百次调用 | 批量：收集房间的所有成员，在一个 `SMEMBERS room:{id}` 调用中获取，在客户端进行过滤；在无争议的频道中使用本地缓存作为第一道防线 |
| R3 | **通话 Saga 补偿导致级联失败**：补偿动作本身可能失败（网络分区） | D3 | 僵尸 SFU peer 或 Redis 路由泄漏 | 在补偿中实现最后手段的本地清理（尽力而为）并记录；由 TASK-012 中的对账循环拾取 |
| R4 | **嵌入回填风暴**：TASK-007 为小型编辑添加了同步嵌入计算，在批量编辑操作期间可能会使 AiWorker 过载 | D2 | AiWorker 预算耗尽，延迟所有嵌入作业 | 同步嵌入限制为 ≤4 个块/≤2KB；用进程内信号量限制并发同步嵌入计算（默认为 4）；更大的消息走异步队列 |
| R5 | **Redis Pub/Sub 消息丢失**：如果节点在处理失效广播之前崩溃，该节点将使用过时的缓存持续长达 60 秒 | D4 | 时效性较差的过扇出窗口（最多 60 秒 vs 修复后的 2 秒） | 失效是可选的优化，不是正确性要求；如果没有收到失效，缓存最终会过期（回退到 60 秒 TTL）；与 TASK-015 的重新校验相结合，在收到失效之前该窗口最多为 2 秒 |

### 3.2 中等风险项

| # | 风险 | 方向 | 缓解措施 |
|---|------|------|---------|
| R6 | `embedding_version` 迁移很耗时（扫描 200 万行进行回填） | D2 | 分批回填（`LIMIT 1000` 循环），在部署期间不阻塞读取；列默认为 `0`（视为“陈旧”） |
| R7 | FCM/APNs 速率限制：批量推送可能被服务提供商限制 | D5 | `push_bot` 中的指数退避，随机抖动（±30%）；TASK-022 聚合减少了总体推送数量 |
| R8 | 通话 `end_call` 乐观锁定增加了 `call_sessions` 表的写入争用 | D3 | PostgreSQL 乐观锁定适用于低冲突工作负载；如果冲突率超出预期，则回退到基于 Redis 的租约 |

### 3.3 测试难点

| # | 挑战 | 方向 | 策略 |
|---|------|------|---------|
| R9 | 跨消费者 NATS 顺序违规的测试在 CI 中不可重现 | D1 | 不要等待 NATS 来搞乱顺序——编写单元测试，以反向顺序或无序交付模拟 WS 帧和 Bot 事件；`SeqGate` 单元测试覆盖所有排序场景 |
| R10 | 跨节点通话测试需要两个真实实例 | D3 | 使用 mock SFU 和 mock Redis 路由对 Saga 补偿进行单元测试；在隔离测试网络中编写集成测试（两个本地服务器进程 + 一个 NATS 实例） |
| R11 | Redis Pub/Sub 集成测试需要运行中的 Redis 实例 | D4 | 使用 `fred` 的测试辅助工具（`RedisServer` 对于编译时特性是可选的）；如果 Redis 不可用，则在 CI 中跳过 |

---

## 4. 资源评估

### 4.1 团队构成

| 角色 | 所需技能 | 人数 | 主要负责 |
|------|---------|------|---------|
| **高级后端工程师（通话）** | Rust、Async、SFU/WebRTC、分布式系统设计模式 | 1 人 | 方向三（TASK-011 到 TASK-014） |
| **高级后端工程师（搜索/IM）** | Rust、PostgreSQL、pgvector、搜索相关性 | 1 人 | 方向二（TASK-007 到 TASK-010） |
| **中级后端工程师（总线/缓存）** | Rust、NATS、Redis、缓存策略 | 1 人 | 方向一（TASK-003 到 TASK-005）、方向四（TASK-015 到 TASK-018） |
| **前端工程师** | JavaScript（ES2020）、WebSocket、DOM 操作 | 1 人 | 方向一客户端（TASK-001、TASK-002、TASK-006） |
| **中级后端工程师（推送）** | Rust、FCM/APNs、Redis | 0.5 人（兼职） | 方向五（TASK-019 到 TASK-022） |

**总计**: 4.5 FTE，为期 6 周

### 4.2 关键里程碑

| 里程碑 | 周次 | 目标 | 验收标准 |
|---------|------|------|---------|
| **M0：快速修复合并** | 第 1 周末 | 没有“幽灵消息”，没有误导性注释，删除后正确清理嵌入 | `cargo check --workspace` + `cargo test --workspace --lib` 通过；幽灵消息回归测试通过 |
| **M1：核心过扇出窗口消除** | 第 2 周末 | 方向四的 P1 安全漏洞已关闭；成员在踢出后 ≤2 秒内停止接收消息 | Redis SISMEMBER 在 fan_out 路径中生效；最大过扇出延迟为 2 个网络 RTT |
| **M2：搜索一致性** | 第 3 周末 | 编辑/删除后，向量搜索会在 ≤2 秒（小）或 ≤30 秒（大）内反映变更 | 搜索功能测试：编辑消息 → 在 5 秒内搜索返回更新内容 |
| **M3：通话弹性** | 第 4 周末 | `join_group_call` 在 Redis 故障时正确恢复；`end_call` 不会与并发连接冲突 | 故障注入测试：Redis 在加入期间超时 → peer 未停留在 SFU 中 |
| **M4：推送协调** | 第 5 周末 | 桌面端活跃用户不再收到移动推送；DND 在推送和 WS 之间一致 | 推送行为测试：桌面端连接处于活跃状态 → 没有 FCM 发送 |
| **M5：全面集成** | 第 6 周末 | 所有 5 个方向都已完整实现并通过测试 | `scripts/truth-check.sh` 通过；无 Clippy 新警告；所有集成测试通过 |

### 4.3 阻塞点及解决方案

| 阻塞点 | 涉及方向 | 问题 | 解决方案 |
|--------|---------|------|---------|
| **SFU 媒体面未接线** | D3 | 正如 AGENTS.md §2 所指出的，`SfuMediaSession::run` 在生产中未被调用（仅 `#[cfg(test)]`）。在没有真实媒体的情况下，端到端通话一致性测试无法验证 | 不要阻塞。编写仅使用 `SfuRouter`（已接线）和 `CallRouteStore`（已接线）的 Saga 测试。在受控的第二阶段验证真实媒体路径（当前范围之外） |
| **NATS 集群在 CI 中不可用** | D1、D3 | 没有 NATS 服务器的集成测试无法验证跨消费者订购故障 | 编写模拟 `EventBus` trait 的单元测试，按特定顺序投递事件。使用 `testcontainers` 进行可选的 NATS 集成测试 |
| **Redis 不存在于 `cargo test` 默认值中** | D4、D5 | 依赖 Redis 的测试需要 `IGNORE` + `DATABASE_URL` 门控 | 使用 `fred::types::RedisServer` 进行嵌入式测试（如果编译了 `test-utils` 特性）；否则，跳过具有清晰日志的测试 |

---

## 5. 质量保证

### 5.1 单元测试覆盖要求

| 任务 | 最低覆盖率（新代码行数） | 关键场景 |
|------|--------------------------|---------|
| TASK-001 | 100% JS 分支 | 原始消息在编辑之前到达 / 之后到达 / 从未到达 / 被删除然后编辑 |
| TASK-002 | 100% JS 分支 | 有序投递、乱序投递、重复 seq、seq 间隙（可接受）、seq 回绕 |
| TASK-003 | 95% Rust 分支 | 消息被审核者在扇出之前删除 / 未被删除 / DB 查询失败 |
| TASK-011 | 90% Rust 分支 | `add_peer` 成功/`register` 失败 → 正确补偿；两者都失败；两者都成功 |
| TASK-015 | 95% Rust 分支 | Redis `SISMEMBER` 找到/未找到/超时；大型接收者列表的批量 |
| TASK-016 | 90% Rust 分支 | Pub/Sub 到达 → 缓存未命中；Pub/Sub 丢失 → TTL 过期；双重失效（幂等） |
| TASK-019 | 95% Rust 分支 | 桌面端活跃 → 跳过推送；仅移动端活跃 → 推送；presence 数据陈旧 → 推送 |

### 5.2 集成测试策略

| 场景 | 涉及的任务 | 方法 | 环境 |
|------|-----------|------|------|
| **端到端消息排序** | TASK-001、TASK-002、TASK-003 | 模拟 NATS 以反向顺序投递消息 + Edited 事件；验证客户端仅按 seq 顺序渲染 | 单元测试（模拟总线） |
| **搜索一致性** | TASK-007、TASK-008、TASK-009 | 编辑消息，向量搜索，验证 ≤2 秒的更新延迟 | 集成（PG + 模拟 AI） |
| **通话 Saga 补偿** | TASK-011、TASK-012、TASK-014 | 注入 Redis 故障；验证 SFU peer 已清理 | 单元测试（模拟 SFU + Redis） |
| **过扇出防御** | TASK-015、TASK-016 | 踢出成员后发送消息；验证被踢成员的 WS 未收到消息 | 集成（PG + Redis + WS） |
| **推送抑制** | TASK-019、TASK-020 | 将参与者标记为在桌面端活跃；发送通知；断言 FCM 未被调用 | 单元测试（模拟 FCM） |
| **并发通话生命周期** | TASK-013 | 两个节点同时加入/结束同一通话；验证最终状态一致 | 集成（2×模拟节点 + Redis） |

### 5.3 代码审查要点

| 关注点 | 要检查的内容 |
|--------|-------------|
| **安全性** | TASK-015 是否真的阻止了过扇出，还是仅仅是对缓存的另一种包装？接收方验证是强制性的 |
| **幂等性** | TASK-014 操作 ID 在 DB/SFU/Redis 失败时是否仍正确补偿？ |
| **免受 DoS 攻击** | TASK-015 中每个事件的 Redis 调用，→ 批量获取每房间一次 |
| **竞态条件** | TASK-013 乐观锁定：`version` 检查是否在写入前捕捉到所有并发？ |
| **客户端内存** | TASK-002 SeqGate 的 `recent` Set 受 256 个上限的限制；没有无限增长的 Map |
| **降级路径** | 所有任务：当 DB/Redis/SFU/AI 不可用时，系统是否正常运行而不是崩溃？ |

### 5.4 性能测试需求

| 方向 | 场景 | 指标 | 阈值 |
|------|------|------|---------|
| D1 | 高吞吐量房间（100 msg/s）中的消息延迟 | p50/p99 客户端渲染延迟 vs 基线 | p50 + 200ms，p99 + 500ms（SeqGate 排序） |
| D2 | 批量编辑后的搜索延迟 | 对旧内容的搜索命中次数 vs 新内容 | 编辑后 ≤5 秒内零命中（旧），≤30 秒内 100% 命中（新） |
| D3 | 具有 100 个通话的大型通话并发加入/离开 | 端到端加入时间；僵尸 peer 计数 | 加入时间 ≤500ms；零僵尸 peer |
| D4 | 10 个节点，每个节点 1000 个用户，活跃房间 | fan_out_raw 中的 Redis 负载；缓存命中率 | `SISMEMBER` < 5k/s/节点；缓存命中率 > 95% |
| D5 | 每分钟 1000 条通知 | FCM 每秒发送量 vs 聚合率 | 每分钟外发 FCM 调用次数减少 ≥40% |

---

## 6. 实施计划

### 第一阶段：基础设施搭建（第 1-2 天）

```
Day 1   Day 2
├───────┼───────┤
│ T017  │ T001  │    ← 快速修复（注释 + 客户端 ghost guard）
│ T006  │ T009  │    ← 客户端重新连接修复 + 删除嵌入内容清理
│ T010  │       │    ← 图片标题嵌入修复
│       │       │
│ 验收：cargo check --workspace  clean
│       cargo clippy --workspace --all-targets  no new warnings
│       `handleEdited` 不再对缺失消息执行 appendChild
```

**并行性**：T001+T006（前端人员）|| T017+T009+T010（后端人员）

### 第二阶段：核心一致性修复（第 3-8 天）

```
Day 3   Day 4   Day 5   Day 6   Day 7   Day 8
├───────┼───────┼───────┼───────┼───────┼───────┤
│ T015  │ T015  │ T016  │ T016  │ T003  │ T003  │
│       │       │       │       │       │       │
│  Redis │  batching │  Pub/Sub│  集成  │  DB双重│  集成  │
│  SISMEMBER│ + mock  │  广播  │  测试  │  检查  │  测试  │
│       │       │       │       │       │       │
│ T004  │ T004  │ T018  │ T018  │ T002  │ T002  │
│       │       │       │       │       │       │
│  seq高  │  集成  │  节点间  │  测试  │  SeqGate│  前端  │
│  水位线│  测试  │  online │       │  排序  │  测试  │
│       │       │       │       │       │       │
│ 验收：过扇出窗口 ≤2s（从 60s）   │  验收：SeqGate 排序缓冲区工作 +
│      成员重新校验路径已启用      │      审核→交付屏障工作
```

**关键依赖**：T015 → T016 → T018（顺序，同一工程师）
**并行**：T003+T004（服务器）|| T002（前端）

### 第三阶段：通话弹性 + 搜索一致性（第 9-16 天）

```
Day 9   Day 10  Day 11  Day 12  Day 13  Day 14  Day 15  Day 16
├───────┼───────┼───────┼───────┼───────┼───────┼───────┼───────┤
│ T011  │ T011  │ T011  │ T014  │ T014  │ T012  │ T012  │ T013  │
│ Saga  │ 补偿  │ 测试  │ 幂等性 │ 测试  │ 对账  │ 测试  │ 竞争  │
│ 设计  │       │       │       │       │ 循环  │       │ 修复  │
│       │       │       │       │       │       │       │       │
│ T007  │ T007  │ T008  │ T008  │ T008  │ T005  │ T005  │       │
│ 内联  │ 测试  │ 迁移  │ 搜索  │ 测试  │ Web钩子│ 测试  │       │
│ re-embed│     │ +列   │ 降级  │       │ 屏障  │       │       │
│       │       │       │       │       │       │       │       │
│ 验收：Saga 在 Redis 故障时正确恢复  │  验收：向量搜索在 ≤2s/≤30s 内反映编辑
│      end_call 不会与并发加入冲突    │      Webhook 不会在消息之前触发
```

**并行性**：T011→T014→T012→T013（通话工程师）|| T007→T008（搜索工程师）|| T005（总线工程师，在其他任务之后）

### 第四阶段：推送协调 + 在线网络聚合（第 17-24 天）

```
Day 17  Day 18  Day 19  Day 20  Day 21  Day 22  Day 23  Day 24
├───────┼───────┼───────┼───────┼───────┼───────┼───────┼───────┤
│ T019  │ T019  │ T020  │ T020  │ T021  │ T021  │ T022  │ T022  │
│ 存在性 │ 测试  │ DND门 │ 测试  │ Redis │ 测试  │ 批处理 │ 测试  │
│ 感知  │       │ 控    │       │ 通知  │       │       │       │
│       │       │       │       │ 状态  │       │       │       │
│       │       │       │       │       │       │       │       │
│ 验收：桌面端活跃用户跳过推送       │  验收：通知聚合减少 ≥40% 的外发推送
│      DND 在 WS 和推送之间一致     │      跨设备已读状态共享
```

**并行性**：T019→T020→T021→T022（单个后端工程师，推送专业知识）
**注意**：如果推送工程师仅在部分时间可用，这一个阶段可以拉长（兼职安排是 0.5 FTE ≈ 4 周一阶段）

### 第五阶段：集成测试 + 性能优化（第 25-30 天）

```
Day 25  Day 26  Day 27  Day 28  Day 29  Day 30
├───────┼───────┼───────┼───────┼───────┼───────┤
│ 端到端  │ 性能  │ 性能  │ 修复  │ 代码  │ 发布  │
│ 测试  │ 基线  │ 优化  │ 回归  │ 审查  │ 候选  │
│       │       │       │       │       │       │
│ 全5个  │ 负载  │ 瓶颈  │ 回归  │ 所有  │ 构建  │
│ 方向  │ 测试  │ 修复  │ 修复  │ 审查  │ 标签  │
│       │       │       │       │       │       │
│ 验收：所有集成测试通过         │  验收：cargo test --workspace --lib  all green
│      p99 延迟不受影响          │      scripts/truth-check.sh  0 violations
│      Redis 负载 <5k/s/节点    │      cargo clippy  no new warnings
```

### 摘要：各阶段时间轴

```
Week 1    | Week 2    | Week 3    | Week 4    | Week 5    | Week 6
──────────┼───────────┼───────────┼───────────┼───────────┼───────────
Phase 0   | Phase 1   | Phase 2   | Phase 2   | Phase 3   | Phase 4
Quick Fix  | Core Cache | Call Saga | Call+Search| Push      | Integration
          | + Barrier  |           | Finalize  |           |
─────────────────────────────────────────────────────────────────────
T001  ██  | T015  ████ | T011  ██████ | T012  ████ | T019  ████ | E2E ██████
T017  █   | T016  ████ | T014  ████   | T013  ████ | T020  ████ | Perf ██████
T006  ██  | T003  ████ | T007  ████   | T005  ████ | T021  ████ | Review ███
T009  ██  | T004  ████ | T008  ██████ | T018  ████ | T022  ████ | Release ██
T010  ██  | T002  ████ |              |             |            |
```

**总计**: 30 个工作日 ≈ 6 个日历周，4.5 FTE

---

## 附录 A：任务优先级重新排序

根据代码验证的更正，将原始文档中的优先级重新映射：

| 方向 | 原始优先级 | 建议的优先级 | 理由 |
|---------|-------------|---------------|---------|
| D1（事件顺序） | P0 | **P0** | 不变。幽灵消息（客户端 `appendChild` 错误）是 P0 数据完整性错误 |
| D4（缓存一致性） | P2 | **P1** | 验证显示 fan_out_raw 路径中没有下游 auth gate。被踢成员 60 秒过扇出窗口是真实的安全越权 |
| D3（通话一致性） | P1 | **P1** | 不变。实际操作顺序（SFU→Redis）纠正了风险向量，但并未消除它 |
| D2（搜索一致性） | P1 | **P1** | 不变。搜索是 AI-Native IM 的核心差异化因素 |
| D5（推送协调） | P2 | **P2** | 不变。推送是 UX/留存缺陷，不是安全或数据完整性缺陷 |

## 附录 B：错误预算（用于 P0/P1 的安全预算）

对于 P0 + P1 方向（D1、D2、D3、D4），我建议一旦合并了缓解措施，就为生产部署建立**错误预算**：

| 指标 | 目标 | 错误预算（每月） | 窗口 |
|------|------|-------------------|---------|
| 被踢成员收到消息 | 0 | 0（不允许） | 持续 |
| 用户看到幽灵消息 | 0 | 0（不允许） | 持续 |
| 向量搜索提供过时结果 | < 0.1% 的查询 | 搜索查询的 0.5% | 1 小时 |
| 通话僵尸 peer | < 5/天/集群 | 50/月 | 15 分钟 |
| 不必要的推送 | < 5%/用户/天 | 10%/月 | 1 小时 |
