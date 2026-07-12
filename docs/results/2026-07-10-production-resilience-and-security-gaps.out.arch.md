# 架构师分析：Aero IM 交叉验证报告

以下分析基于交叉验证报告的 5 个深水区方向，结合 AGENTS.md 中描述的系统骨架与约束，从架构层面给出评估、方向建议和路线图。

---

## 一、架构评估

### 1.1 当前架构的结构性优势

源码验证确认了 AGENTS.md 所描述的核心架构承诺基本兑现，以下几点值得肯定：

| 维度 | 评估 |
|---|---|
| **分层清洁** | crate 依赖自下而上无环，`aero-common` 作为纯叶子层，`aero-server` 作为纯组合层——这是 Rust 工作区架构的典范布局。方向五确认 NATS 生命周期管理是显式的 `create_consumer()`，非隐式依赖。 |
| **事件 DAG 清晰** | 双命名空间（`im.room.*` durable + `live.stream.*` ephemeral）设计合理：IM 消息需要可靠投递（durable cursor），直播弹幕可容忍丢失（ephemeral）。这个区分在源码级完全成立。 |
| **CI 门控覆盖面广** | `truth-check.sh` + `file-size-check.sh` + `web-check.sh` + clippy 全 workspace — 在多 agent 并行集成的场景下，这些门控是质量底线。迁移编译期嵌入 + `FOR UPDATE SKIP LOCKED` 等并发策略也经过验证存在。 |

### 1.2 结构性缺陷（架构债务）

交叉验证报告揭示的五个方向，按**严重程度**排序：

#### P0 级：WS 投递质量盲区（方向四）

这是最严重的架构缺陷。**当前系统的实时投递层完全没有可观测性**：

```
┌─────────────────────────────────────────────────────┐
│                  Hub::fan_out_raw                    │
│                                                     │
│  RoomEvent ──→ bounded mpsc ──→ WebSocket 帧        │
│                     │                                │
│                [drop if full]                        │
│                     │                                │
│               tracing::warn!                         │
│               (只有 log，无指标)                       │
└─────────────────────────────────────────────────────┘
```

**问题本质**：一个号称"AI-Native IM + 互动直播平台"的系统，核心实时数据传输路径（WebSocket fan-out）缺乏以下基本可观测性：

- 发送帧计数（`ws_frames_sent_total`）
- 丢帧计数（`ws_frames_dropped_total`）
- 连接断开原因分布（`ws_disconnects_total{reason="..."}`）
- 消息投递延迟（`ws_message_latency_seconds`）
- 回填截断计数（`ws_backfill_truncated_total`）

AGENTS.md §2 明确指出 `run_bus_listener` 中 `RoomEvent::Message` 是"**唯一非重复计数收口**"（`MESSAGES_SENT_TOTAL`），但这条计数只到 NATS 发布，不进 WS 投递。这意味着：

> **你无法回答生产中最基本的问题："用户的消息到底投递到客户端没有？"**

**架构根因**：`Hub` 的 `fan_out_raw` 是纯扇出路径，所有状态管理都在订阅端（`WsSubscriber` 的有界 channel + drop-on-full），但没有任何累计指标来区分"正常投递"、"channel 满丢帧"和"连接断开"。

#### P0 级：错误预算不存在（方向三）

系统有三个层面的错误预算缺失：

1. **AI 层**：`AiWorker` 有预算控制（per-ws + 全局），但这是**成本预算**，不是**错误预算**。当 AI 服务不可用（API key 过期、超时、5xx），当前行为是 fail-open（log-skip 或道歉消息），但**没有累计错误率指标、没有断路器、没有 503 降级端点**。

2. **NATS consumer 层**：`health.rs` 只探连通性（`ping`），不探 backlog 深度。如果某个 durable consumer 积压了 10 万条消息，health endpoint 仍返回 200。

3. **内存层**：`Hub` 的 `CancellationToken` 只来自 `WsConfig`，没有基于 `rss` 或 `heap` 触发的优雅关闭。在 Rust 的 `alloc` 默认配置下，OOM 直接 `abort`。

**架构根因**：系统假设了"基础设施永远健康"。NATS 连通 = 系统可用。这在 demo/PoC 阶段成立，在生产中不成立。

#### P1 级：Web SPA 供应链安全（方向一）

三条防线全敞：
- CDN 加载 hls.js 无 `integrity` → 如果 CDN 被攻陷，所有用户的浏览器执行攻击者代码
- JWT 明文存 `localStorage` → XSS 一次即完全账户接管
- CSP 可选非默认 → 没有纵深防御

**AGENTS.md 没有提到任何安全架构原则**，这是一个架构文档层面的缺口。

#### P1 级：迁移系统缺乏生产保障（方向二）

纯 forward 迁移、无 down、无超时、无 advisory lock——这在以下场景有风险：

- **回滚场景**：如果新版本有 bug 需要 revert 数据库，没有 down 迁移意味着必须手写逆向 SQL
- **并发迁移**：多实例同时启动，`sqlx::migrate!()` 没有 advisory lock，可能两个实例同时跑迁移
- **长迁移**：无 `lock_timeout`/`statement_timeout`，对大表的 `ALTER TABLE ADD COLUMN` 可能锁死生产

**但这是 trade-off**：Rust 编译期嵌入迁移 + 不可变部署的事实意味着回滚通常是"部署旧版本"而不是"跑 down 迁移"。当前方案在部署流程正确的前提下是可接受的——但缺乏 advisory lock 是个真正风险。

#### P2 级：NATS 生命周期管理（方向五）

Consumer 健康心跳指标缺失，且无 `drain`/`maintenance` 端点。但考虑到 NATS 自身有 max_deliver=16 的 poison 处理 + JetStream 的持久化，这个缺口的影响在早期生产可容忍。

### 1.3 架构债务总结

| 债务类型 | 位置 | 影响 | 修复成本估算 |
|---|---|---|---|
| 可观测性缺失 | WS/`Hub` 扇出路径 | 无法诊断投递问题 | ~1 周（加指标 + 暴露） |
| 弹性设计缺口 | AI 服务/NATS 消费 | 降级行为不可观测 | ~2 周（健康检查扩展） |
| 安全架构缺失 | web/ 前端 | XSS→ATO 风险 | ~1 周（SRI + CSP 默认开） |
| 迁移保障不足 | `db.rs` | 并发迁移风险 | ~3 天（advisory lock） |
| 运维能力缺口 | NATS consumer | 排障效率低 | ~1 周（指标 + 端点） |

---

## 二、扩展方向

基于当前架构的缺口和 AGENTS.md 描述的愿景，我建议以下高价值架构扩展方向：

### 方向 A：可观测性基础设施（P0）

**为什么需要**：没有可观测性，试运行阶段之外无法运营。方向四和方向五的核心问题都指向同一个缺失层。

**核心挑战**：
1. **指标定义标准**：当前 `metrics.rs` 散落各种 Prometheus 计数器——但没有命名公约。需要约定 `{domain}_{operation}_{result}_total` 模式。
2. **WS 连接生命周期追踪**：每个 `WsSubscriber` 需要关联 metadata（room_id、user_id、user_agent、connected_at），并在断开时 emit 带有 reason tag 的 counter。
3. **结构化日志与指标的一致性**：`tracing::warn!` 位置应该同步 emit 指标，但目前不是——这导致日志和指标脱节。

**预期的架构变更**：

```
┌─────────────────────────────────────────────────┐
│                 Observable Hub                   │
│                                                   │
│  fan_out_raw(user, payload) →                     │
│    ws_frames_sent_total{user,room}+=1             │
│    ws_message_latency_seconds{room}.observe()     │
│                                                   │
│  on_disconnect(user, reason) →                    │
│    ws_disconnects_total{reason}+=1                │
│    WS_CONNECTIONS.dec()                          │
└─────────────────────────────────────────────────┘
```

**对现有系统的影响**：低侵入。`Hub` 已经持有 `Metrics` 引用（从 `AppState` 传入），只需要在 `fan_out_raw` 和连接生命周期钩子中加指标调用。不需要新类型或新 trait。

**选项对比**：

| 选项 | 方案 | 优点 | 缺点 |
|---|---|---|---|
| A1 | 直接在 `hub.rs` 中调用 metrics | 改动最小，快速出效果 | 指标分散在各处，无统一 gateway |
| A2 | 引入 `MetricsCollector` trait + 若干实现 | 可测试，可切换后端 | 过度抽象，对于~10 个指标不值得 |
| ✅ 推荐 A1 | 直接埋点 + 后期按需提取 | 6 个月内够用 | — |

### 方向 B：弹性模式层（P0-P1）

**为什么需要**：方向三揭示的缺口本质是**系统没有对依赖故障的结构化应对策略**。当前只有两态：可用 / log-skip（fail-open）。生产需要三态：正常 / 降级 / 熔断。

**核心挑战**：
1. **依赖健康抽象**：需要统一 `HealthCheck` trait 来聚合所有依赖的状态（NATS backlog、AI 错误率、Redis 延迟），而不仅是 ping。
2. **断路器定位**：断路器应该放在 `ImService` 还是 `aero-ai`？放在 `aero-ai` 更靠近故障源，但 `ImService` 需要感知降级状态来选择响应策略。
3. **降级策略定义**：AI 不可用时，是返回错误还是返回启发式结果？AGENTS.md 说"AI 无 key 退化，逻辑路径不变"——但退化也需客户端感知。

**预期的架构变更**：

```
                    ┌────────────────────┐
                    │   HealthRegistry   │  ← 新模块
                    │   (crate: health)  │
                    └────────┬───────────┘
                             │
          ┌──────────────────┼──────────────────┐
          ▼                  ▼                  ▼
   NatsBacklogHealth   AiErrorRateHealth   RedisLatencyHealth
          │                  │                  │
          └──────────────────┼──────────────────┘
                             ▼
                    ┌────────────────────┐
                    │   /health/depth    │  ← 新端点
                    └────────────────────┘
```

**对现有系统的影响**：中等。需要在 `boot/` 装配中加一个 `HealthRegistry` 实例，传入相关组件。`health.rs` 需要扩展但不需要重写。

**选项对比 —— 断路器位置**：

| 位置 | 优点 | 缺点 |
|---|---|---|
| `aero-ai` 内部（`AiService`） | 靠近故障源，减少网络开销 | `ImService` 不知情，可能返回矛盾结果 |
| `ImService` 调用侧 | 业务层知情，可调整响应 | 需要在多处加检查 |
| ✅ 推荐：中间件层 | 统一 gateway，所有 AI 路由过同一检查 | 需要封装当前散落的 AI 路由 |

### 方向 C：安全架构加固（P1）

**为什么需要**：方向一的三条防线缺口需要系统性解决，**而不是逐个打补丁**。

**核心挑战**：
1. **localStorage JWT 的替代方案**：HttpOnly cookie 需要后端支持 + CSRF 保护。当前 WS 的 token 认证是基于 `Bearer` 头或 `access_token` 参数——改为 cookie 需要修改 WS 握手路径。
2. **SRI 的维护成本**：CDN 版本升级需要手动更新 `integrity` hash——需要自动化（`scripts/update-sri.sh`）。
3. **CSP 的生产兼容**：如果 `hls.js` 使用 `eval()` 或 `new Function()`，严格的 CSP 可能 break。需要评估。

**对现有系统的影响**：中等。cookie 方案涉及后端改动（WS 握手 + CORS 配置）。

**选项对比 —— JWT 存储**：

| 选项 | 方案 | 优势 | 劣势 |
|---|---|---|---|
| C1 | 继续 localStorage + CSP 默认开 + XSS 缓解 | 改动最小 | localStorage 在 XSS 面前无效 |
| C2 | HttpOnly cookie + CSRF token | 行业标准，XSS 不泄露 token | 需要后端改 WS 握手 + CSRF 端点 |
| ✅ 推荐 C2 | 增量迁移：先 CSP + SRI，后 cookie | 阶段性降低风险 | — |

### 方向 D：迁移安全加固（P1）

**为什么需要**：虽然当前方案在不可变部署流程下可接受，但多实例并发启动是真实场景（k8s rolling update 会同时启动多个 pod）。

**核心挑战**：
1. advisory lock 需要 Postgres 版本支持（PG 9.1+ 支持 `pg_advisory_lock`）
2. 迁移超时：需要 `SET lock_timeout = '2s'` 防止迁移阻塞写流量
3. 迁移进度输出：长迁移需要向 stdout 输出进度

**对现有系统的影响**：小。只需要包裹 `sqlx::migrate!()` 调用。

```
db.rs 改造建议：

pub async fn migrate(pool: &PgPool) -> Result<()> {
    // 1. 获取 advisory lock (key=20260522 基于设计文档日期)
    sqlx::query("SELECT pg_advisory_lock(20260522)").execute(pool).await?;
    
    // 2. 设置 lock_timeout
    sqlx::query("SET LOCAL lock_timeout = '2s'").execute(pool).await?;
    
    // 3. 执行迁移
    sqlx::migrate!("../../migrations").run(pool).await?;
    
    // 4. 释放锁
    sqlx::query("SELECT pg_advisory_unlock(20260522)").execute(pool).await?;
}
```

### 方向 E：WebSocket 投递可靠性改进（P1-P2）

**为什么需要**：方向四不仅是指标缺口——`Hub` 的 drop-on-full 策略（`bounded mpsc` channel 满时丢帧）在没有指标的情况下不可审计。需要策略决策：丢帧是否可接受？还是应该背压到 NATS consumer？

**核心挑战**：
1. **背压传播**：如果 `fan_out_raw` 阻塞，会背压到 `run_bus_listener` 的 NATS ack——这意味着 1 个慢客户端可能阻塞整个房间的事件投递。
2. **替代策略**：per-client channel → 慢客户端单独排队 → 超时断连。这是更复杂但更正确的方案。
3. **回填策略**：重连后的消息回填需要 seq 比较，当前已有 `RESYNC_FRAME` 机制，但没有回填截断的指标。

**对现有系统的影响**：大。涉及 `Hub` 内部架构调整。

**选项对比 —— 慢客户端策略**：

| 选项 | 方案 | 优势 | 劣势 |
|---|---|---|---|
| E1 | 保持 drop-on-full + 加指标 | 简单，不改变行为 | 仍丢帧 |
| E2 | per-client unbounded channel + OOM guard | 不丢帧 | 内存风险 |
| ✅ 推荐 E3 | per-client bounded channel + 超时断连 | 不阻塞 NATS，慢客户端被踢 | 更复杂，需要断连逻辑 |

---

## 三、接口设计建议

### 3.1 健康检查接口统一化

当前 `health.rs` 的接口风格是**逐依赖手写**（`check_pg`、`check_redis`、`check_nats`、`check_blob`）。建议抽象为：

```rust
// 建议的 trait，非代码
trait HealthCheck {
    fn name(&self) -> &'static str;
    fn check(&self) -> impl Future<Output = Result<HealthStatus>>;
}

enum HealthStatus {
    Healthy,
    Degraded { message: String, latency_ms: u64 },
    Unhealthy { error: String },
}
```

**收益**：
- 新依赖（S3、AI proxy、WebSocket backlog）接入 health 只需实现 trait
- `/health/depth` 端点可以返回所有依赖的结构化状态
- 可以根据 Degraded 数量决定整体 readiness 状态

### 3.2 指标收集接口

当前 `metrics.rs` 是手写 Prometheus counters。不建议引入新框架（metrics crate / opentelemetry-rust 会增加依赖复杂度），但**建议固定命名模式**：

```
{组件}_{操作}_{结果}_{单位?}
ws_frames_sent_total
ws_frames_dropped_total
ws_disconnects_total{reason="timeout|error|client_close"}
nats_consumer_lag{consumer="aero-server"}
ai_request_duration_seconds{provider="anthropic|voyage"}
```

**向后兼容**：新增指标不破坏现有面板，旧指标命名保持不变。建议在 `metrics.rs` 顶部加注释解释命名约定。

### 3.3 弹性策略接口

不建议在现阶段引入断路器库。对于 AI 降级，一个简单的**策略枚举**就够了：

```rust
// 概念性接口设计
enum AiFallbackStrategy {
    /// 返回错误给客户端
    Error,
    /// 返回启发式结果（HashEmbedder / 模板回答）
    Fallback,
    /// 排队异步处理（不阻塞用户请求）
    Defer,
}
```

在 `AiService` 构造时注入策略，而不是硬编码。这样不同部署（沙箱 vs 生产）可以选择不同策略。

---

## 四、技术选型

### 4.1 不需要引入的技术

| 候选技术 | 结论 | 理由 |
|---|---|---|
| OpenTelemetry Rust SDK | ❌ 不引入 | 当前 Prometheus 客户端族已经够用。OpenTelemetry 的 Rust 实现还在快速演进，且引入 gRPC 依赖链。当前只需扩展已有 `metrics.rs` |
| 断路器库（rsbreaker/safecircuit） | ❌ 不引入 | 对于当前系统，AI 降级的场景可以用简单状态机实现。引入库带来的序列化/反序列化开销不值当 |
| `metrics` crate ecosystem | ❌ 不引入 | 当前 `prometheus` crate 直接使用是最小依赖路径。迁移到 `metrics` 族需要改大量 `register_*` 调用 |

### 4.2 可以考虑引入的技术

| 候选技术 | 场景 | 建议 |
|---|---|---|
| `console-subscriber`（tokio-rs/console） | 调试 WS 扇出的 task 行为（哪个 subscriber channel 满、哪个 task 堆积） | ✅ **有条件推荐**：只用于开发/调试环境。生产不部署。当前"帧从哪里丢"完全看不见，tokio-console 可以回答这个问题 |
| `sentry` / `sentry-tracing` | 错误事件追踪（panic、ws disconnect 原因、NATS poison message） | ✅ **评估中**：比 Prometheus 更适合事件级排障。但需要评估对 Rust panic hook 的影响 |

### 4.3 第三方依赖评估标准

当前系统已经有一个清晰的约束："feature-first 单位是 crate"。对于新依赖，建议用以下矩阵评估：

| 维度 | 硬性红线 | 弹性筛选 |
|---|---|---|
| 许可证兼容 | MIT/Apache 2.0 双许可是基线 | MPL-2.0 可评估；GPL 不可 |
| MSRV | 不晚于当前 workspace MSRV（1.80） | — |
| 审计 | 安全相关的依赖需要 `cargo audit` 无已知漏洞 | — |
| API 稳定性 | 非 `0.x` 版本 | 如果 `0.x`，评估作者和社区活跃度 |
| 依赖链 | 不引入 >5 个传递依赖（含自身的） | 基础架构类（如 TLS）可放宽 |

---

## 五、实施路线图

### 优先级评定

| 方向 | 优先级 | 理由 |
|---|---|---|
| 方向四：WS 投递指标 | **P0** | 没有这个，无法回答"系统在工作吗" |
| 方向三：健康检查扩展 | **P0** | 没有这个，降级/故障不可观测 |
| 方向一：CSP + SRI | **P1** | 安全基线，但风险概率较低（CDN 被攻陷） |
| 方向一：JWT cookie 迁移 | **P1** | 更大的安全改进，但影响后端架构 |
| 方向二：迁移 advisory lock | **P1** | 并发迁移风险虽低但后果严重 |
| 方向五：NATS consumer 指标 | **P2** | 运维便利性，非阻塞 |
| 方向四：WS 背压/慢客户端 | **P2** | 需要 P0 指标数据才能决定方案 |

### 阶段划分

#### Phase 0 — 紧急止血（1 周）

目标：在与 AGENTS.md §4.3 "提交前必过" 相同的 CI 门控级别，把可观测性基线建立起来。

```
Week 1:
├── ws_frames_sent_total         // hub.rs fan_out_raw
├── ws_frames_dropped_total      // hub.rs drop path
├── ws_disconnects_total         // WsSubscriber drop
├── ai_error_total {provider}    // aero-ai 调用路径
├── CSP 默认启用                  // serve.rs 配置
└── SRI hash 加入 hls.js 标签    // web/index.html
```

**交付检查**：
- `curl http://localhost:3030/metrics | grep ws_frames` 返回非零值
- `web/index.html` 的 `<script>` 标签含 `integrity` 属性
- `make smoke` 通过

#### Phase 1 — 结构加固（2 周）

```
Week 2-3:
├── HealthCheck trait + /health/depth 端点
│   ├── NATS consumer backlog 深度
│   ├── AI error rate (sliding window 5m)
│   └── WS 连接积压 (per-room queue depth)
├── 迁移 advisory lock (pg_advisory_lock)
├── AiService 降级策略枚举 (Error|Fallback|Defer)
└── metrics 命名规范文档 (docs/metrics-conventions.md)
```

**风险点**：`/health/depth` 端点如果查询 NATS backlog 需要 JetStream API 调用——如果 NATS 本身延迟高，健康检查端点可能超时。**缓解**：设 500ms timeout + 缓存 5s。

#### Phase 2 — 安全提升（2 周）

```
Week 4-5:
├── HttpOnly cookie support for JWT
│   ├── WS 握手支持 cookie 认证
│   ├── CSRF token 端点
│   └── 向后兼容：旧客户端继续用 Bearer header
├── CSP strict mode 默认配置
│   ├── 验证 hls.js 兼容性
│   └── strict-dynamic fallback
└── SRI 自动化脚本 (scripts/update-sri.sh)
```

**风险点**：cookie 方案下 WS 的 token 刷新机制需要重新设计——当前 `REFRESH_KEY` 在 `localStorage` 中可以被 JS 刷新，但如果 token 在 HttpOnly cookie 中，刷新需要另外的 REST 端点。**缓解**：保留 `REFRESH_KEY` 在 localStorage（只用于刷新端点，不可读用户 ID） + access_token 在 cookie。

#### Phase 3 — 投递可靠（3 周）

```
Week 6-8:
├── WsSubscriber 慢客户端检测
│   ├── per-client bounded channel 阈值监控
│   ├── 超过阈值 → 发 CloseFrame (policy_violation)
│   └── 指标：ws_client_timeout_total
├── NATS consumer 健康心跳指标
│   ├── consumer_last_acked_timestamp
│   └── consumer_pending_bytes
├── 维护模式端点 /maintenance/drain
│   ├── 拒绝新 WS 连接
│   └── 等待已有连接 drain (grace period + force)
└── /health/readiness 集成 draining 状态 → 503
```

### 整体风险评估

| 风险 | 可能性 | 影响 | 缓解策略 |
|---|---|---|---|
| Phase 0 指标加完后发现 WS 丢帧率很高，需要紧急切换方案 | 中 | 高 | Phase 0 先加指标再行动——让数据驱动决策 |
| Cookie 迁移破坏现有 token 认证 | 低 | 高 | 向后兼容设计：新旧并存 2 个 release cycle |
| 断路器误判（AI 临时 5xx 触发熔断） | 中 | 中 | 健康检查使用滑动窗口 5min + 至少 10% 错误率阈值 |
| 迁移 advisory lock 死锁 | 低 | 高 | 设置 `lock_timeout=2s` + 重试逻辑 |
| SRI hash 更新被遗漏 | 中 | 低 | CI 中加 `scripts/update-sri.sh` check |

---

## 总结

交叉验证报告的 5 个方向全部成立，没有虚报。系统在**核心架构（事件驱动、分层、并发策略）**方面设计扎实，AGENTS.md 中的约束和边界得到了源码验证。主要缺口集中在**可观测性（方向四、五）**和**弹性设计（方向三）**——这些都是生产就绪度问题，不是架构方向问题。

最高价值的下一步不是引入新技术栈或重写模块，而是在现有架构上加一层**可观测性基础设施**。AGENTS.md 说"全系统复用骨架"，这个骨架需要在加入指标和健康检查后才能真正支撑生产运行。

建议按 Phase 0 → Phase 1 → Phase 2 → Phase 3 的顺序执行，Phase 0 可在一周内完成并产生可度量的改善。
