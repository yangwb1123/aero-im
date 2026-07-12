现在我已充分了解了代码库的结构。以下是我的综合 Tech Lead 评估。

---

# Tech Lead 执行评估：Aero IM 架构改进

## 报告概要

基于审查文档《架构扫描》及直接代码验证（rate_limit.rs、config.rs、state.rs、bus.rs、metrics_tasks.rs、messages.rs、frame.rs），我将五个方向分解为**16 个任务**，跨四个阶段，预计投入 **2–3 名开发人员 4–6 周**。优先级基于影响/工作量比：方向一/方向二/方向五可合并到第一阶段（第 1–2 周）；方向三规划在第二阶段；方向四作为第三阶段的连续改进独立推进。

---

## 1. 任务分解

### 方向一：PG 池保护（P0 — 高价值，低风险）

| 任务 ID | 标题 | 涉及文件 | 前置依赖 | 工时 | 验收标准 |
|---------|------|---------|----------|------|---------|
| **TASK-001** | `metrics_tasks` 中的 PG 池利用率 gauge：将采样率从 15s → 5s，并添加滚动 `in_use_avg`（1 分钟滑动窗口） | `crates/aero-server/src/bin/boot/metrics_tasks.rs` | 无 | 2h | Gauge 每 5s 报告 `in_use_avg`；保留 `in_use` 瞬时值；Prometheus 指标可见 |
| **TASK-002** | 连接池压力传感器：在 `state.rs` 中新增 `PoolPressureSensor`，基于 `in_use_avg / max` 比率（阈值 70%/85%/95%）计算压力等级 [0.0–1.0] | `crates/aero-server/src/state.rs` 新增 `pool_pressure: PoolPressureSensor`；`metrics_tasks.rs` 添加 gauge | TASK-001 | 3h | 压力输出在 [0.0, 1.0] 区间；压力 >0.7 时记录 `warn!`；暴露 `DB_POOL_PRESSURE` gauge |
| **TASK-003** | AI worker 分流到 `pg_read`：将 `AiWorker` 的所有 DB 只读查询（`list_dead`、`claim`、`count_dead`）从 `pg` 迁移到 `pg_read` | `crates/aero-storage/src/ai_job.rs`（注入第二个池或状态机持有 `pg_read` 引用）；`crates/aero-server/src/bin/boot/services.rs`（传递 `pg_read`） | 无 | 4h | AI worker 查询使用 `pg_read`；读副本可用时其连接池利用率下降；`test_ai_worker_reads_replica` 集成测试通过 |
| **TASK-004** | 房间成员缓存命中率仪表化：为 `room_member_cache` 添加 `hit_count`/`miss_count` 计数器，作为 Prometheus gauge 暴露 | `crates/aero-server/src/room_member_cache.rs`；`crates/aero-server/src/metrics.rs`（注册计数器） | 无 | 2h | `/metrics` 暴露 `ROOM_MEMBER_CACHE_HITS` 和 `ROOM_MEMBER_CACHE_MISSES` |
| **TASK-005** | 房间访问检查缓存：在 `room_member_cache` 之上或 `ImService` 中添加 `RoomAccessCache`，缓存 `assert_room_access` 结果（TTL 30s，失效率 0.05） | `crates/aero-server/src/room_member_cache.rs`（或新增 `room_access_cache.rs`）；`crates/aero-im-core/src/service/mod.rs`（注入） | TASK-004 | 4h | WS 重连风暴场景下，`assert_room_access` 的 PG 查询减少 >80%（通过集成测试中注入的计数器验证） |

### 方向二：消息投递可靠性（P0 — 高价值，中等风险）

| 任务 ID | 标题 | 涉及文件 | 前置依赖 | 工时 | 验收标准 |
|---------|------|---------|----------|------|---------|
| **TASK-006** | 服务端 `MessageAck` 框架 + 客户端处理：新增 `ServerFrame::MessageAck { temp_id, message_id, ts }`；修改 `ws_impl/frame.rs` 中的 `::SendMessage` 和 `::SendMarkdown` 处理函数，在 `send_message` 成功后发出 ack | `crates/aero-server/src/ws/ws_impl/frame.rs`（ack 发送逻辑）；`crates/aero-server/src/ws/ws_impl/frame.rs`（`ServerFrame` 枚举）；`web/app.js` / `web/ws.js`（`pendingByTempId` 回调） | 无 | 6h | 每条已持久化的 `send_message` 都会发出 `MessageAck` 帧；客户端在收到 ack 前不会清除 `pendingByTempId`；现有消息流无变化 |
| **TASK-007** | NATS 投递失败高级日志：当 `publish_room_event` 失败时，在 `warn!` 中添加 `message_id` 和 `room_type` 结构体上下文；若 `ImService` 上配置了可选的 `delivery_failed: mpsc::Sender`，则发出事件 | `crates/aero-im-core/src/service/events.rs`（`publish_room_event` 方法） | 无 | 2h | `publish_room_event` 失败日志包含 `message_id`、`room_id`、`room_type`；可选的事件接收器在 10s 内收到 `DeliveryFailed` 事件 |
| **TASK-008** | WebSocket `send()` 返回 false 检查：在 `ws.js` 的每处 `send()` 调用处添加返回 false 时回调 `onMessageFailed(tempId)`；现有 `app.js` 中的 `sendMessage()` 做非 false 检查 | `web/ws.js`（添加 `send_with_check(data, onFail)`）；`web/app.js`（`sendMessage` 调用 `send_with_check`） | 无 | 3h | `ws.send()` 返回 false 时触发 `onMessageFailed`；用户在 UI 中看到红色感叹号重试指示器 |

### 方向三：优雅降级编排（P1 — 高影响力，高风险）

| 任务 ID | 标题 | 涉及文件 | 前置依赖 | 工时 | 验收标准 |
|---------|------|---------|----------|------|---------|
| **TASK-009** | `HealthAggregator` 模块：新增健康度量收集器（PG 压力、Redis 延迟、NATS 背压、AI 预算水位、hub 扇出队列深度），通过 TASK-002 的压力传感器轮询 | `crates/aero-server/src/health_aggregator.rs`（新增）；`crates/aero-server/src/state.rs`（注入）；`crates/aero-server/src/bin/boot/mod.rs`（引导） | TASK-002 | 6h | 每秒更新一次 `HealthLevel { 0=healthy, 1=strained, 2=degraded, 3=critical }`；暴露 `HEALTH_LEVEL` gauge；`/health/ready` 端点反映 `HealthLevel >= 3` 时的不就绪状态 |
| **TASK-010** | `rate_limit.rs` 注入降级感知：从 `HealthAggregator` 读取降级层级，当 `Level>=2` 时收紧通用速率限制器参数（`rate *= 0.5`） | `crates/aero-server/src/rate_limit.rs`（`RateLimiter` 新增 `set_degradation_factor(factor: f64)`）；`crates/aero-server/src/state.rs`（连接） | TASK-009 | 4h | 设置降级因子后，速率限制器在 1 分钟内收紧至目标速率；`/metrics` 暴露 `RATE_LIMIT_DEGRADED` gauge |
| **TASK-011** | `run_bus_listener` 中 NATS 退避采用指数退避 + 熔断：将 `BUS_RESUBSCRIBE_BACKOFF` 恒定 1s 替换为指数退避（1s→2s→4s→8s→16s 上限），及 5 次失败后的熔断状态（30s 冷却） | `crates/aero-server/src/ws/ws_impl/bus.rs`（`run_bus_listener` 和 `run_live_bus_listener`） | 无 | 4h | NATS 断连时退避呈指数增长，上限 16s；连续 5 次失败后熔断打开 30s；恢复后自动重置 |
| **TASK-012** | 降级层级反馈到 `/ready` 端点：当 `HealthLevel >= 3`（严重）时，使就绪探针返回 HTTP 503 | `crates/aero-server/src/health_aggregator.rs`（注册就绪检查）；`crates/aero-server/src/routes/health.rs`（`ready` 端点） | TASK-009 | 2h | `HealthLevel >= 3` 时 `GET /health/ready` 返回 503 及 body `{"status":"not_ready","reason":"critical"}`；`<3` 时返回 200 |

### 方向四：自适应限流（P2 — 中等价值，低风险）

| 任务 ID | 标题 | 涉及文件 | 前置依赖 | 工时 | 验收标准 |
|---------|------|---------|----------|------|---------|
| **TASK-013** | 速率限制器衰减因子：为 `RateLimiter` 添加动态 `capacity_multiplier: AtomicF64`，允许运行时收紧/放松突发容量而不丢失现有 bucket | `crates/aero-server/src/rate_limit.rs`（`RateLimiter` 新增 `set_capacity_multiplier`、`capacity()` 方法使用乘数） | 无 | 3h | 设置 `capacity_multiplier = 0.5` 后，新 bucket 和现有 bucket 在 2 个 refill 窗口内收敛到一半容量；`/metrics` 暴露 `RATE_LIMIT_CAPACITY_MULTIPLIER` |
| **TASK-014** | 全局速率限制器 + per-ws 健康聚合器：将 `DbPool::size()` vs `num_idle()` 的非原子读取替换为针对 `PoolPressureSensor` 的原子 `try_acquire` 样式探测 | `crates/aero-server/src/state.rs`（`PoolPressureSensor` 添加 `acquire_permit()` 方法）；`metrics_tasks.rs`（使用新方法） | TASK-001, TASK-002 | 4h | 压力传感器提供 `acquire_permit(timeout)` 返回 `Result<PoolPermit>` 或超时；gauge 采样使用 `try_acquire` 而非 `size - idle` |

### 方向五：WebSocket 帧尺寸校验（P0 — 极低风险，高价值）

| 任务 ID | 标题 | 涉及文件 | 前置依赖 | 工时 | 验收标准 |
|---------|------|---------|----------|------|---------|
| **TASK-015** | WS 配置中新增 `max_frame_bytes`：在 `WsConfig` 中添加 `max_frame_bytes: usize`（默认 `4 * 1024 * 1024`，4 MiB 以容纳大 base64 语音消息）；`from_env()` 解析 `AERO_WS_MAX_FRAME_BYTES` | `crates/aero-server/src/config.rs`（`WsConfig` 添加 `max_frame_bytes`） | 无 | 1h | `WsConfig::default().max_frame_bytes == 4_194_304`；`AERO_WS_MAX_FRAME_BYTES=2097152` 正确解析 |
| **TASK-016** | WS 帧输入校验中间件：在 `ws_impl/frame.rs` 的 `handle_text` 顶部添加校验，对大于 `max_frame_bytes` 的帧返回 `ServerFrame::Error { code: "frame_too_large" }` | `crates/aero-server/src/ws/ws_impl/frame.rs`（`handle_text` 入口处校验）；`crates/aero-server/src/ws/ws_impl/mod.rs`（传递配置） | TASK-015 | 2h | 超过限制的帧收到 `{"type":"error","code":"frame_too_large"}`；小于限制的帧正常处理；`test_ws_frame_too_large` 通过 |

---

## 2. 执行顺序

### 依赖图

```mermaid
graph TD
    subgraph "Phase 1: Foundation (Week 1-2)"
        T001[TASK-001: PG gauge 5s sampling]
        T004[TASK-004: Room member cache metrics]
        T006[TASK-006: MessageAck frame]
        T007[TASK-007: NATS fail logging]
        T008[TASK-008: WS send() false check]
        T015[TASK-015: max_frame_bytes config]
    end

    subgraph "Phase 2: Core (Week 2-3)"
        T002[TASK-002: PoolPressureSensor] --> T001
        T005[TASK-005: Room access cache] --> T004
        T016[TASK-016: Frame size validation] --> T015
        T011[TASK-011: NATS exponential backoff]
    end

    subgraph "Phase 3: Integration (Week 3-5)"
        T003[TASK-003: AI worker → pg_read] --> T002
        T009[TASK-009: HealthAggregator] --> T002
        T010[TASK-010: Rate limiter degradation] --> T009
        T012[TASK-012: /ready degradation] --> T009
        T013[TASK-013: Rate limiter decay factor]
        T014[TASK-014: Atomic pool pressure probe] --> T002
    end

    T009 --> T010
    T009 --> T012
```

### 并行执行组

| 组 | 任务 | 并行理由 |
|----|------|----------|
| **A**（基础设施） | TASK-001, TASK-004, TASK-006, TASK-007, TASK-008, TASK-015 | 无文件冲突；三个独立代码区域（metrics、WS 协议、observability） |
| **B**（压力 + 缓存） | TASK-002 → TASK-003, TASK-005 | TASK-002 需在 TASK-003 之前完成以验证分流效果；TASK-005 在 TASK-004 之后 |
| **C**（校验） | TASK-016 | 仅在 TASK-015 之后 |
| **D**（降级） | TASK-009 → TASK-010, TASK-011, TASK-012 | 健康聚合器必须在消费之前完成 |
| **E**（自适应） | TASK-013, TASK-014 | 可独立于 D 组并行执行；TASK-014 需 TASK-002 |

---

## 3. 技术风险

### 3.1 高概率/高影响

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|----------|
| **TASK-003 中的 `pg_read` 事务隔离**：AI worker 当前查询使用 `SELECT … FOR UPDATE SKIP LOCKED`（更新语义）。路由到只读副本会破坏声明逻辑。 | 高 | 高 | `pg_read` 仅用于纯只读、非锁定查询（`count_dead`、历史记录）。`claim`/`SKIP LOCKED` 必须在主库上运行。在任务边界处显式文档化。 |
| **TASK-006 中的 MessageAck 顺序**：如果 ack 与常规消息帧乱序到达（NATS 交付延迟），客户端可能会对 ID 解析感到困惑。 | 中 | 中 | Ack 帧携带可靠的消息 `ts` 和 `temp_id`；客户端仅按 `temp_id` 匹配，不假设顺序。 |
| **TASK-009 中的健康聚合器振荡**：在重负载下，压力传感器在 L2↔L3 之间快速抖动，导致 `/ready` 探针频繁翻转（负载均衡器不稳定）。 | 中 | 高 | 对降级决策应用 5 秒滞后（低于阈值后等待 5s 才升级；高于阈值后等待 10s 才降级）。添加 `health_aggregator::Config { upgrade_delay, downgrade_delay }`。 |
| **TASK-011 中的 NATS 熔断**：错误的熔断实现可能导致健康 NATS 被错误熔断，对整个系统造成复合故障。 | 中 | 高 | 熔断仅影响订阅循环；publishing 使用不同的连接/重试逻辑。熔断状态通过 `HealthAggregator` 可见。熔断定时器是单调的；不会过早重置。 |

### 3.2 低概率/高影响

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|----------|
| **TASK-002 中的 `PoolPressureSensor` 线程安全**：`metrics_tasks` 在 tokio 任务中运行，但未来的读取器（速率限制器、健康状态）可能从不同运行时上下文读取并发 unsafe 计数。 | 低 | 高 | 使用 `Arc<AtomicU32>` 作为 `in_use_avg`；`Ordering::Relaxed` 用于 gauge 语义（近似值）。 |
| **TASK-014 中的 `acquire_permit` 语义**：向压力传感器添加许可方案可能会意外阻塞 web 处理程序。 | 低 | 高 | `acquire_permit` 使用 `try_acquire`（非阻塞）加 `tokio::time::timeout`。从不 `await` 池借用在锁之外。 |

### 3.3 外部依赖

| 依赖 | 用途 | 风险 |
|------|------|------|
| **Prometheus 客户端库** | `common_metrics::set_gauge` — 已存在 | 无（已在代码库中使用） |
| **Redis** | 集群速率限制窗口中的 `WsRateStore::incr_raw` | 低（失败时降级为仅本地限制） |
| **NATS JetStream** | durable consumer cursor 管理 | 中（熔断实现必须处理 JetStream 特定的 `consumer_pending` 语义） |

---

## 4. 资源评估

### 人员配置

| 角色 | 技能要求 | 数量 | 分配 |
|------|----------|------|------|
| **高级后端工程师** | Rust、tokio、sqlx、NATS 经验；异步系统设计 | 1 | FT，第 1–6 周 |
| **后端工程师** | Rust 熟练度；WebSocket 协议经验 | 1 | FT，第 1–4 周 |
| **前端工程师** | JavaScript/ES2020、WebSocket 客户端 | 0.5 | 第 1–2 周（TASK-006、TASK-008） |

**总计**：2–2.5 FTE，持续 4–6 周。

### 里程碑

| 里程碑 | 交付物 | 预计日期 | 依赖 |
|---------|----------|-------------|----------|
| **M1：基础设施完成** | TASK-001、TASK-004、TASK-015 完成，通过 review+test | D5 | 无 |
| **M2：核心可靠性** | TASK-006、TASK-008、TASK-016 完成（MessageAck 框架、帧校验、WS send 检查） | D10 | M1 |
| **M3：池保护** | TASK-002、TASK-003、TASK-005 完成（压力传感器、AI 分流、访问缓存） | D15 | M1 |
| **M4：降级编排** | TASK-009、TASK-010、TASK-011、TASK-012 完成（HealthAggregator、退避、就绪反馈） | D22 | M3 |
| **M5：自适应限流** | TASK-013、TASK-014 完成（衰减因子、原子探测） | D26 | M3 |
| **M6：集成上线** | End-to-end smoke test 通过；性能基线测量；文档更新 | D30 | M4、M5 |

### 阻塞点与解决策略

| 阻塞点 | 描述 | 解决策略 |
|--------|------|----------|
| **B1：pg_read 副本可用性** | TASK-003 假设程序集部署中存在 Postgres 副本；单节点部署应使用主库克隆 | 在生产中，`persistence.rs` 已在 `database.replica_url` 未设置时将 `pg_read` 回退为 `pg.clone()`。TASK-003 保持此行为。 |
| **B2：MessageAck 的 web 客户端兼容性** | 遗留 web 客户端（旧版本浏览器）可能无法处理未知帧类型 | 新帧在不更新 UI 的情况下静默忽略。旧客户端忽略 ack，回退到基于超时的消息发送置信度。 |
| **B3：熔断参数调优** | TASK-011 中的 NATS 熔断阈值需要生产流量下的经验校准 | 从保守阈值开始（5 次尝试，30s 冷却），作为可配置的 `BusConfig` 值暴露，并进行生产监控。 |

---

## 5. 质量保证

### 5.1 单元测试覆盖

| 构件 | 最低覆盖率 | 关键测试场景 |
|------|------------|--------------|
| `rate_limit.rs` | 90% | `set_capacity_multiplier` 后收敛；降级因子对 `check_status` 输出的影响；并发 bucket 访问 |
| `health_aggregator.rs` | 85% | 四个压力阈值之间的状态转换；滞后延迟；就绪状态计算 |
| `bus.rs`（指数退避） | 80% | 退避序列正确性；熔断打开/关闭转换；重置 |
| `frame.rs`（帧尺寸校验） | 95% | 刚好低于/高于边界；空帧；无效 UTF-8（边界校验应基于字节长度） |
| `room_member_cache.rs`（访问缓存 + 仪表化） | 80% | TTL 过期；失效率逻辑；命中/未命中计数器 |
| `messages.rs`（ack 发送） | 90% | ack 在 `send_message` 成功后发出；`publish_room_event` 失败后不发送 ack |

### 5.2 集成测试策略

| 测试场景 | 工具 | 通过标准 |
|----------|------|----------|
| **WS 重连风暴** | 自定义 `WSTestClient` 模拟 50 个并发重连 → 每个加入 5 个房间 | 主库 PG 连接峰值 < `max_connections * 0.3`；`room_member_cache` 命中率 >0.9 |
| **MessageAck 顺序** | 20 个并发 `send_message` 操作；NATS 发布延迟模拟（`tokio::time::sleep`） | 所有 20 条消息都收到 ack；`temp_id → message_id` 映射 1:1 |
| **健康降级** | 注入 `PoolPressureSensor` 高水位模拟 | 5 秒滞后内触发 `HealthLevel::Critical`；`/health/ready` 在 5 秒内返回 503 |
| **帧尺寸限制** | 发送 5 MiB 帧（>限制）→ 发送 3 MiB 帧（<限制） | 大帧 → `frame_too_large` 错误；小帧 → 正常处理 |

### 5.3 代码审查要点

| 热点 | 审查重点 |
|------|----------|
| **TASK-003：pg_read 路由** | AI worker 中没有 `SELECT … FOR UPDATE` 泄漏到只读副本；`claim` 保留在主库上 |
| **TASK-006：MessageAck 帧协议** | 向后兼容性：旧客户端忽略未知帧类型；ack 不改变现有 `msg:message` 处理 |
| **TASK-009：健康聚合器并行性** | 跨多个 `tokio::spawn` 任务共享 `HealthLevel` 的 `Arc<AtomicU8>` 时无数据竞争；正确的 memory ordering |
| **TASK-011：熔断器自动重置** | 熔断器不会过早重置（在冷却期间为打开状态）；重置时成功订阅会清除错误计数器 |
| **TASK-013：`AtomicF64` 替代方案** | Rust 没有 `AtomicF64`；使用 `AtomicU64` 进行 `f64` 位表示转换，或使用 `RwLock<f64>`（低争用）。首选 `RwLock` 以求简单。 |

### 5.4 性能测试需求

| 场景 | 指标 | 基线（当前） | 目标 |
|------|------|-------------|------|
| 高负载下的 PG 池利用率 | `in_use / max` 比率 | 发送 500 msg/s 时 >95% | <80%（AI 分流后） |
| WS 消息 ack 延迟 | p50/p99 从 `send_message` 到 `MessageAck` 送达 | 无 | p50 <50ms，p99 <200ms |
| 重连风暴恢复 | 到 `Presence` 帧送达的时间 | N/A（当前无节流） | 所有房间 <2s |
| 健康聚合器开销 | `HealthAggregator::tick` 的 CPU 时间 | 无 | p99 <1ms，每 tick 0 次分配 |

---

## 6. 实施计划

### 阶段 1：基础设施（第 1–5 天，周 1）

```
Week 1
Day 1  Day 2  Day 3  Day 4  Day 5
├──────┼──────┼──────┼──────┼──────┤
T001   T001   T004   T004   T006
│              │              │
T015          T001          T006
(done)       (merged)      (WIP)
│
T006 (cont.)
```

**产出**：
- DB 池 gauge 每 5s 采样，附带 1 分钟滑动平均值
- `max_frame_bytes` 配置 + WS 帧尺寸校验已部署
- 房间成员缓存命中/未命中计数器已暴露
- `MessageAck` 框架已搭建（服务端 + 客户端骨架）

**风险把关**：TASK-006 的 MessageAck 框架是阶段 1 中风险最高的构件。在 Day 3 进行早期设计审查，确保帧格式在投入生产之前与前/后端团队达成一致。

### 阶段 2：核心可靠性（第 6–14 天，周 2–3）

```
Week 2                           Week 3
Day 6  Day 7  Day 8  Day 9  Day 10  Day 11  Day 12  Day 13  Day 14
├──────┼──────┼──────┼──────┼───────┼───────┼───────┼───────┼───────┤
T007   T008   T011   T011   T011    T002    T002    T005    T005
│              │                    │               │
T006           T008                 T016            T003
(merged)      (done)               │                │
                                    T016            T003
```
**产出**：
- NATS 投递失败携带丰富上下文
- WS `send()` 返回 false 被捕获并通知 UI
- NATS 订阅使用指数退避 + 熔断
- `PoolPressureSensor` 以三个阈值运行
- AI worker 查询路由到 `pg_read`
- 房间访问检查由 TTL 缓存支持

**风险把关**：在 Day 8（TASK-011，NATS 熔断）——审查熔断状态机的实现，确保它不能关闭健康连接。进行混沌测试：杀死 NATS → 验证退避序列 → 等待重新连接 → 验证无事件丢失。

### 阶段 3：降级编排（第 15–26 天，周 4–5）

```
Week 4                           Week 5
Day 15 Day 16 Day 17 Day 18 Day 19 Day 20 Day 21 Day 22 Day 23 Day 24
├──────┼──────┼──────┼──────┼───────┼───────┼───────┼───────┼───────┤
T009   T009   T009   T010   T010   T012    T013    T013    T014    T014
│                    │              │       │               │
T002                 T009           T010    T013            T014
(merged)             (merged)       │       (done)          │
                                     T012                    T014
```

**产出**：
- `HealthAggregator` 在 4 个级别上运行，带滞后功能
- 速率限制器根据健康状况收紧
- `/health/ready` 在严重压力下返回 503
- 速率限制器容量乘数动态
- 池压力传感器原子探测

**风险把关**：在 Day 16 对 `HealthAggregator` 进行架构审查，重点关注滞后决策和与现有就绪探针的交互。在 Day 18 进行模拟生产负载的集成测试：施加压力 → 验证 L0→L1→L2→L3 转换 → 降低负载 → 验证 L3→L2→... 回退。

### 阶段 4：集成与上线（第 27–30 天，周 5–6）

```
Week 5-6
Day 25 Day 26 Day 27 Day 28 Day 29 Day 30
├──────┼──────┼──────┼──────┼───────┼───────┤
集成   集成   性能   性能   上线前  上线
测试   测试   基线   调优   审查
```

**活动**：
- 端到端集成测试（所有 16 个任务，无回归）
- 相对于当前基线的性能基线（消息吞吐量、延迟、池利用率）
- 负载测试结果文档化
- 发布说明 + 配置迁移指南（`AERO_WS_MAX_FRAME_BYTES` 等新环境变量）
- 操作手册更新（降级层级、熔断参数）

---

## 附录：实施优先级矩阵

| 方向 | 价值 | 风险 | 工时 | 优先级 | 理由 |
|---------|-------|------|------|----------|--------|
| **方向一** | 高 | 低 | 15h（5 个任务） | **P0** | 在生产中解决最痛苦的连接池耗尽问题；所有构件独立或低依赖 |
| **方向二** | 高 | 中 | 11h（3 个任务） | **P0** | 消息可靠性丢失是产品 Bug，非功能差距；第二周提供用户可见的改进 |
| **方向三** | 高 | 高 | 16h（4 个任务） | **P1** | 编排框架实现成本高；HealthAggregator 是其他方向（一、四）的基础，但第一阶段可以在没有它的情况下进行 |
| **方向四** | 中 | 低 | 7h（2 个任务） | **P2** | 自适应限流是锦上添花；TASK-013 可在第一阶段后作为持续改进独立实施 |
| **方向五** | 中 | 极低 | 3h（2 个任务） | **P0** | 实现成本极低，立即增强安全态势；无依赖 |

**阶段分配**：第一阶段 = 方向一（P0）+ 方向二（P0）+ 方向五（P0）；第二阶段 = 方向三（P1）；第三阶段 = 方向四（P2）。第一阶段后即可安全上线生产。
