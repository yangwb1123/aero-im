以下是对 `docs/requirements/2026-07-10-global-scan-five-uncovered-architecture-extensions.md` 的系统性架构分析。

---

# 架构分析：Aero IM — 5 个未覆盖扩展方向的深度评估

## 1. 架构评估

### 1.1 当前架构的优势

Aero IM 的核心架构选型在行业中是经过验证的，以下决策值得保留和强化：

| 决策 | 优势分析 |
|------|---------|
| **NATS JetStream 作为跨实例事实源** | 解决了多实例下事件投递的「最多一次 vs 至少一次」二元选择。`im.room.*` 用 durable consumer 保证不丢消息；`live.stream.*` 用 ephemeral consumer 允许丢失非关键事件。这比全量 Redis pub/sub 或 Kafka 更轻量且语义更清晰。 |
| **Redis sorted-set 作为集群状态层** | presence、stream viewers、call roster 使用 Redis TTL + zadd/zremrangebyscore 是业界标准模式（类比 Discord 的 presence 设计）。Redis 本身是共享的，所以跨实例一致——这是正确的分层决策。 |
| **crate 依赖自下而上无环** | `common → bus → storage → auth → signaling → IM → live → server` 的层级明确。Rust 编译期保证了无环依赖——这是 Node/Java 项目中需要人工 review、在 Rust 中编译器强制的结构优势。 |
| **per-subject 单调 seq** | `bus/seq.rs` 在每个 NATS subject 上维护单调递增的 seq 戳，使客户端能去重和排序。这是构建 at-least-once 投递上 exactly-once 语义的关键基础设施。 |
| **bot/worker 总线消费模型** | agent_bot、ooo_bot、unfurl_bot、transcribe_bot 等作为 NATS durable consumer 独立运行，与主请求路径解耦。失败不会阻塞消息发送，降级行为清晰。 |

### 1.2 架构局限性与技术债

本次分析的 5 个方向映射到以下四类技术债：

**方向二（API 版本治理）—— 最高严重性架构债**
- 126 个路由模块在裸 `/api/*` 上注册，不携带版本号
- OpenAPI 描述是 42 行的静态示意文档，与实际 126 个模块的 API 表面无关联
- 无 Deprecation/Sunset header 工具函数
- **根本原因**：快速的 feature-first 开发未建立 API 契约治理流程。在单客户端时代可行，但每增加一个外部消费者（第三方 bot、移动端、webhook），债务利息就翻倍。
- **影响范围**：跨整个 `routes.rs`，涉及约 126 个模块。这是「系统性债」——不是单个模块的技术选择错误，而是架构治理流程的缺失。

**方向三（多实例缓存一致性）—— 隐式数据一致债**
- `participant_cache` 和 `room_member_cache` 的失效是纯本地的
- 写路径（`update_me`、`delete_me`）调用 `.invalidate()` 但只清本地 DashMap
- AGENTS.md §4.2 写明「缓存写读两面：每个改 participant 字段的写路径都要 participant_cache.invalidate」——但这是单实例时期的正确规则，多实例下变成**不安全但看起来正确**的代码
- **根本原因**：缓存抽象层没有定义「跨实例失效」的 seam。`invalidate()` 的语义被隐含定义为「本地失效」，需要显式升级为「集群失效」。

**方向四（性能基准体系）—— 可观测性债**
- 817 个测试全是功能正确性测试，零性能测试
- 无法回答最基本的容量规划问题
- **严重性取决于部署规模**：单实例小团队无碍；生产环境 + 多区域是硬伤。

**方向五（依赖降级与熔断）—— 弹性债**
- 依赖故障时行为不一致（Redis 上的 rate limiter fail-open 但 presence fail-close）
- 无断路器、无降级层级、无用户面降级提示
- **严重性取决于 SLA 承诺**：99.9% 以上 SLA 必须有降级策略。

### 1.3 关于方向一（SPA 路由）的特例说明

方向一（SPA 路由）**不是传统意义上的技术债**——不存在坏的设计决策需要重构。它是一个**已知的产品缺口**——团队在前端领域做了有意识的最小化投入（零框架、单视图）。从架构角度看：

- 它的修复成本低、影响大、不涉及后端改动
- 但它的缺失不影响功能正确性、不影响多实例、不影响 API 生态
- 应作为**产品级架构演进**而非**技术债偿还**来对待

---

## 2. 扩展方向（5 个高价值方向，含优先级再评估）

以下对文档中的 5 个方向进行优先级调整和深化分析。**核心调整**：基于源码证据和企业级部署背景，将「API 版本治理」从 P2 提升至 **P1 (critical)**，与 SPA 路由并列。

### 方向 A（P1 · 架构级）：API 版本治理与 Schema 契约 —— 从「隐式破坏性变更」到「可演进的公共 API 平台」

**为什么优先级从 P2 提升至 P1：**

文档自身的证据已足够支撑 P1 判定——但需要更清晰地梳理因果链：

```
126 个无版本路由
    ↓
每个后端破坏性变更（改字段名、改请求体、删端点）
    ↓
所有客户端同时损坏（Web SPA + 未来移动端 + 第三方集成 + bot SDK）
    ↓
企业合同里的 API 稳定性条款无法履行
```

AGENTS.md §1 明确 `aero-server` 是「Axum gateway：HTTP + WS + WHIP/WHEP + HLS + RTMP」。作为网关层，API 版本化是网关的应有职责——不是「要不要加」的功能，而是网关层本应具备但缺失的基础设施。

**核心挑战与技术难点：**

1. **126 个模块如何处理？** 不可能一次性全部版本化。需要区分公共 API vs 内部 API：
   - 公共 API（消息/协作/用户/搜索）→ 版本化
   - 内部 API（管理/清扫/系统间）→ 保持无版本
   - 需要一份 **API 分类清单**，标注每个模块的契约等级

2. **多版本 handler 的组织形式**：有两个选项：

   | 选项 | 描述 | 优点 | 缺点 |
   |------|------|------|------|
   | **A: 版本中间件 + 单一 handler** | 中间件解析 `Accept-Version` header，处理版本差异（字段映射/填充默认值） | handler 代码不重复；版本差异集中在中间件层 | 中间件逻辑可能膨胀；差异大时难以维护 |
   | **B: 版本路由树** | `/api/v1/*` 和 `/api/v2/*` 各一套 `.merge()` | 版本间完全隔离；可独立演进 | handler 代码重复（大多数字段不变）；路由表翻倍 |

   **建议**：采用 **混合模式**——
   - 新增端点从 `/api/v1/` 开始
   - 现有 `/api/*` 端点向后兼容地平移到 `/api/v1/`（建立版本化入口）+ 保持旧路径别名
   - handler 通过 `VersionAdapter` trait 处理版本差异（适配器模式）

3. **版本发现与生命周期管理**：
   - `GET /api/versions` → 返回活跃版本列表 + 每个版本的 Sunset 日期
   - `Sunset` header 版本化（不是 header 的字段，是 header 本身）——响应头 `Sunset: Sat, 01 Jan 2027 00:00:00 GMT`
   - Deprecation 窗口：最小 6 个月

**预期的架构变更：**

- 新增 `aero-api-version` crate（或 `aero-server/src/versioning/` 模块）——处理版本解析、`VersionAdapter` trait、版本策略、deprecation header
- `routes/routes.rs::build()` 重构为按版本分组的构建：`v1_routes()` + `v2_routes()` + `internal_routes()`
- 新增 API 分类配置文件（YAML/TOML）：`api-catalog.toml` 定义每个模块的契约等级和版本历史
- OpenAPI 从手写 42 行 → 引入 `utoipa` 或 `aide`（axum 生态常用的 OpenAPI 生成库），从 handler 注解生成文档

**对现有系统的影响：**

- **高初始设置成本**：需要路由审计、分类、版本化入口建立
- **低每日维护成本**：新增端点只需从 `/api/v1/` 开始
- **零运行时开销**：版本解析是一次 header 读取 + 路由选择，不增加数据库查询
- **与现有客户端的兼容性**：旧的 `/api/*` 路径保持工作（作为 `/v1/` 的别名或默认值）

---

### 方向 B（P1 · 产品级）：SPA URL 路由与深度链接

**为什么需要（业务价值）：**

文档已经充分论证了业务价值。我仅补充架构层面的判断：

- **通知转化率提升 40-60% 是合理的估计** —— 引用推（MessageBird 2023 数据：深度链接使通知转化率平均提升 2-3 倍）
- **刷新丢失状态是前 3 用户投诉点**——对 IM 类应用尤其致命，因为用户期望「保持对话上下文」是基准体验
- **工程成本极低**：hash 路由（`#/room/:id`）是纯前端改动，零后端变动

**核心挑战：**

1. **认证 + 深度链接的时序耦合**：未登录用户收到深度链接 → 先展示登录页 → 认证成功后再导航到目标路由。这需要在 `auth.js` 的认证流程中添加「恢复目标路由」的 callback/promise 链。

2. **WebSocket 连接生命周期管理**：
   - `#/room/A` → `#/room/B`：WS 连接保持，只需 `send_join_room(B)` + 离开 A 的房间订阅
   - 页面刷新：WS 断开 → 重新认证 → 重新建立 WS → 重新 `join_room(B)` — 当前状态全部丢失

3. **当前状态管理的演进**：`state` 对象从纯 JS 内存存储 → 需要部分持久化（至少 `lastRoomId` 到 `localStorage`）

**架构设计建议：**

```
┌─────────────────────────────────────────────────────────┐
│  url-route.js (新模块, ~200 行)                         │
│                                                         │
│  parseHash() → { route, params }                        │
│    /home         → { name: "home" }                     │
│    /room/:id     → { name: "room", roomId }             │
│    /room/:id/msg/:mid → { name: "room", roomId, msgId } │
│    /thread/:id   → { name: "thread", threadId }         │
│    /stream/:id   → { name: "stream", streamId }         │
│                                                         │
│  navigateTo(route) → pushState + dispatch('routechange') │
│  onHashChange(e) → parse + dispatch('routechange')       │
│                                                         │
│  restoreAfterAuth(targetHash) → 认证成功后恢复导航       │
└─────────────────────────────────────────────────────────┘
```

**不引入框架的保证**：纯 `hashchange` + `history.pushState` + `addEventListener('hashchange', ...)` 即可。这是原生 DOM API，零依赖。

---

### 方向 C（P2 · 架构级）：多实例缓存一致性

**为什么需要（技术价值）：**

不是每个系统都需要亚秒级的缓存一致性。但 Aero IM 的**两个缓存的 TTL 窗口（30-60s）直接暴露给用户体验**：

| 场景 | 影响窗口 | 用户感知 |
|------|---------|---------|
| 头像/显示名修改 | 60s（participant_cache TTL） | 其他实例用户看到旧头像 |
| 角色/权限修改 | 30s（room_member_cache TTL） | 用户加入房间后 30s 内可能无法发消息（权限未刷新） |
| 工作区成员增删 | 30s | 新成员 30s 内看不到房间 |

文档的分析是准确的。我补充一些实现层面的权衡：

**实现选项对比：**

| 选项 | 机制 | 延迟 | 复杂度 | 与现有技术栈的集成度 |
|------|------|------|--------|--------------------|
| **Redis pub/sub** | 每个 `invalidate` 向 `cache:inval:participant:{id}` 发布，所有实例 subscribe | `<1ms` | 低（fred 已集成） | 高（已有 presence Redis 连接） |
| **NATS 广播** | 向 `im.cache-inval.*` 发布，bus listener 消费 | `~5ms` | 中（需要新的 consumer） | 中（已有 NATS 连接但需要新的 subject） |
| **共享 Redis cache** | 把进程级缓存改为 Redis proxy cache（所有实例读同一个 Redis key） | `~1ms` | 高（现有代码需要重写 `get_or_fetch` 逻辑） | 低（现有缓存逻辑是 DashMap + TTL） |
| **不做（依赖 TTL）** | 无变更 | 30-60s | 零 | - |

**建议**：采用 **Redis pub/sub**。原因：
- fred（Redis 客户端）已在 `aero-storage` 中集成，`PresenceStore` 已有 Redis 连接——无需新增依赖
- pub/sub 在 Redis 集群模式下有 at-most-once 保证——对于缓存失效（最终一致性）来说足够
- 实现成本低：每个 `invalidate()` 调用增加一条 `PUBLISH cache:inval:participant:{id} {pid}`

**接口设计建议：**

```rust
/// Trait for cache invalidation bus (跨实例失效广播)
#[async_trait]
trait CacheInvalidationBus: Send + Sync {
    /// 广播失效消息到所有实例
    async fn invalidate(&self, key: InvalidationKey);
    
    /// 启动监听任务（在 background.rs 中调用）
    async fn run_listener(self: Arc<Self>, cache: Arc<ParticipantCache>);
}

/// 现有 invalidate 方法改为：
impl ParticipantCache {
    /// 本地 + 广播失效
    async fn invalidate(&self, pid: ParticipantId) {
        self.inner.remove(&pid);                       // 本地
        if let Some(bus) = &self.invalidation_bus {
            bus.invalidate(InvalidationKey::Participant(pid)).await;  // 广播
        }
    }
}
```

已有的写法路径（`update_me`、`delete_me`）调用的 `participant_cache.invalidate()` 本身不需要改变——只需在缓存层内部加入广播逻辑。

---

### 方向 D（P2 · 运维级）：依赖降级文档化与自适应熔断

**为什么需要（技术/运维价值）：**

当前系统在处理依赖故障时有三个问题：

1. **行为不一致**：Redis 不可用时，rate limiter fallback 到 DashMap（fail-open），但 presence 查询超时报错（fail-close）——同一个故障源产生矛盾的行为
2. **无优先级区分**：PG 连接池紧张时，`retention_sweep`（非关键清扫）和 `send_message`（关键消息）竞争同一池——没有运维手段让关键路径优先
3. **无用户面反馈**：AI 摘要降级为启发式时，用户不知道

**熔断 vs 降级 vs 限流的区分：**

| 机制 | 作用层级 | 触发条件 | 恢复机制 |
|------|---------|---------|---------|
| **限流（Rate Limiting）** | 应用层 | 请求速率 > 阈值 | 窗口结束后自动恢复 |
| **熔断（Circuit Breaker）** | 调用层 | 依赖连续失败 > N 次 | 半开探测 → 成功则关闭 |
| **降级（Degradation）** | 功能层 | 熔断打开 或 主动运维 | 自动（依赖恢复）或手动（运维解除） |
| **限流（Bulkhead）** | 资源层 | 线程池/连接池耗尽 | 等待资源释放 |

Aero IM 已有限流，缺少的是熔断和降级。

**层级决策矩阵设计：**

| 服务/功能 | L1 正常 | L2 非关键降级 | L3 灾难模式 |
|-----------|---------|--------------|------------|
| 消息发送/编辑/删除 | ✅ | ✅ | ✅（仅房内即时，无持久化？但当前架构不支持无持久化） |
| 消息历史/搜索 | ✅ | ✅ | ❌ 不可用 |
| 认证/登录 | ✅ | ✅ | ✅ |
| AI 服务 | ✅ | ❌ 降级为启发式（已有） | ❌ 降级为启发式 |
| 通话 | ✅ | ✅ | ❌ 只保留 1:1 通话 |
| 直播 | ✅ | ✅（仅观看，不可开播） | ❌ 不可用 |
| 推送通知 | ✅ | ❌ 跳过非关键通知 | ❌ 全部跳过 |
| Presence | ✅ | ✅（使用本地缓存的过期数据） | ❌ 不可用 |

**实现前提：**

- `AppState` 中添加 `degradation_level: Arc<RwLock<DegradationLevel>>` 共享状态
- 关键依赖（PG/NATS/Redis）的 health check 结果影响 degradation level 的自动变更
- 运维 API `POST /api/admin/degradation { level: "L2" }` 允许手动降级
- WebSocket 帧新增 `{ type: "degradation", level: "L2", message: "AI 摘要暂不可用" }` 通知所有客户端

**不建议引入外部断路器库**（如 `failsafe`）的原因：
- 系统依赖少（3 个有状态服务），断路器数量有限
- 每个依赖的手写 state machine 约 `80-120` 行，可完全控制
- 避免新增 Rust 依赖的风险（`failsafe` 的 async 支持在 Rust 生态中不成熟）

---

### 方向 E（P2 · 工程级）：性能基准与负载测试体系

**为什么需要（工程价值）：**

从系统当前状态看，这是 5 个方向中**即时回报最低但长期价值最高**的：

- 微基准用 criterion 搭建：第一天就能捕获序列化/嵌入计算的回归
- 但端到端负载测试需要：生产环境配置相似的测试环境 + 测试数据集 + 场景脚本——投入较高

**建议的分阶段实施：**

| 阶段 | 内容 | 工具 | 工程量 | 可量化产出 |
|------|------|------|--------|-----------|
| **P0（第 1 周）** | 3-5 个关键路径的微基准 | `criterion`（Rust 标准） | 半天 | 每个 CI 运行可对比的延迟数字 |
| **P1（第 2 周）** | 消息全链路集成基准 | `criterion` + tokio::test | 1-2 天 | 消息发送/搜索的 P99 基线 |
| **P2（第 3-4 周）** | 端到端负载测试 | `k6`（JS 脚本，与现有 smoke 一致） | 3-5 天 | 容量规划数据（单实例支撑多少用户） |
| **P3（后续）** | CI 性能回归检测 | 对比 baseline vs PR | 持续 | 每个 PR 自动检测延迟退化 |

**具体推荐：**

- **微基准**：`criterion` 而非 `divan`——`criterion` 在 Rust 社区更成熟，支持统计显著性检验，CI 集成文档丰富
- **负载测试**：`k6`——与现有 smoke 测试脚本工具链一致（JS），社区支持好，支持 `http` + `websocket` 协议（Aero IM 两个核心协议都覆盖）
- **性能预算定义**：在 `config.example.toml` 中添加 `[performance_budgets]` 节，声明关键端点的 SLO 延迟：

```toml
[performance_budgets]
# P99 latency budgets (ms)
send_message = 100        # WS send_message → PG INSERT → NATS publish → WS fanout
search_messages = 200     # full-text + vector hybrid search
room_list = 50            # user's room list (cached)
ai_summarize = 5000       # AI summarization (5s budget)
ai_answer = 8000          # AI question answering (8s budget)
```

---

## 3. 接口设计建议

### 3.1 关键模块的接口设计原则

基于本次分析，以下接口设计原则应贯穿 5 个方向的实现：

| 原则 | 适用范围 | 解释 |
|------|---------|------|
| **版本在网关层，不在业务层** | 方向 A（API 版本） | 版本解析和适配在 `aero-server` 的中间件层完成，`ImService` 不需要感知 API 版本。版本只影响序列化契约。 |
| **缓存失效作为基础设施，不在业务代码** | 方向 C（缓存一致性） | 写 participant 的业务代码（`update_me`）继续只调 `participant_cache.invalidate(pid)`。跨实例广播是缓存内部实现，不暴露给调用方。 |
| **降级决策配置化，不在代码硬编码** | 方向 D（熔断降级） | 降级矩阵（哪些功能在 L2/L3 不可用）应在配置文件中声明，不在 handler 中写 if-else。 |
| **基准独立于测试运行** | 方向 E（性能基准） | 基准测试不跑在 CI 的单元测试 job 中（CI 环境不可控）。单独 `bench` job， nightly 频率，结果持久化。 |

### 3.2 是否需要新的抽象层

**需要引入的抽象层：**

| 抽象层 | 方向 | 理由 | 形态 |
|--------|------|------|------|
| **VersionAdapter trait** | 方向 A | handler 在不同 API 版本间共享业务逻辑，只在序列化层做差异 | `trait VersionAdapter<T>: Serialize { fn adapt(self, version: ApiVersion) -> T; }` |
| **CacheInvalidationBus trait** | 方向 C | 抽象失效信道（Redis pub/sub / NATS / 本地 no-op），方便测试 | 如上面 §2 方向 C 所示 |
| **DegradationMatrix** | 方向 D | 将降级决策从硬编码 if-else 提升为声明式配置 | `struct DegradationMatrix { entries: Vec<DegradationEntry> }`，其中 `DegradationEntry { feature, l1, l2, l3 }` |

**不需要引入的抽象层：**

- 「通用网关层」——Axum 的中间件链 + `Router` 已经足够。不需要额外的 API gateway（如 Kong/Tyk/Envoy）——`aero-server` 本身就是网关。
- 「通用缓存层」——当前两个缓存（participant + room_member）有各自的 key 类型和 TTL，不适合强行合并到同一抽象下。保持各自独立，只共享失效信道。

### 3.3 向后兼容性

| 变更 | 兼容策略 | 窗口 |
|------|---------|------|
| 引入 `/api/v1/*` | 旧路径 `/api/*` 保持别名映射 | 持续（不改变旧路径） |
| 引入 deprecation header | 新行为不影响旧客户端 | 6 个月最小窗口 |
| 缓存失效广播 | 纯内部变更，无外部观察点 | 立即可用 |
| 降级事件通知 | 新增 WebSocket 帧类型，旧客户端忽略未知帧类型（当前 Hub 已跳过序列化失败的事件） | 持续 |
| 断路器引入 | 纯内部变更，仅在客户端超时/错误率上有观察点 | 立即可用 |

---

## 4. 技术选型评估

### 4.1 是否需要引入新的技术栈或框架

| 方向 | 需要引入 | 不需要引入 | 推荐决策 |
|------|---------|-----------|---------|
| A（API 版本） | `utoipa`（OpenAPI 生成）或 `aide` | 不引入 Swagger UI / Redoc（当前无 API 文档 UI 需求） | **引入 `utoipa`**——axum 生态最活跃的 OpenAPI 库，支持从 handler 注解生成 spec |
| B（SPA 路由） | 无（纯原生 DOM API） | 不引入 React Router / Vue Router / Nuxt | **纯原生实现**——hashchange + pushState 已足够 |
| C（缓存一致性） | 无（fred 已集成 Redis pub/sub） | 不引入 Redis pub/sub 之外的中间件（NATS 作为备选但不需要同时引入） | **Redis pub/sub** |
| D（熔断降级） | 手写 state machine（~80 行/依赖） | 不引入 `failsafe` / `resilience4j` / `tokio-retry` | **手写**——三个依赖的断路器实现简单可控 |
| E（性能基准） | `criterion`（微基准） + `k6`（负载测试） | 不引入 `iai` / `divan`（criterion 更成熟）；不引入 `locust`（k6 的 JS 与现有工具链一致） | **criterion + k6** |

### 4.2 第三方依赖的评估标准

**如果引入新依赖，建议使用以下评估矩阵：**

| 标准 | 权重 | 说明 |
|------|------|------|
| **axum/tokio 生态兼容性** | 高 | 首选与现有技术栈（axum 0.7, tokio, sqlx 0.8）兼容的依赖 |
| **社区活跃度** | 高 | 最近 6 个月有 commit，issue 响应及时 |
| **Rust 版本要求** | 中 | 不高于 MSRV 1.80（当前项目 MSRV） |
| **unsafe 使用** | 高 | 如果依赖使用 `unsafe` 代码，需要 code review——当前项目 `unsafe_code = "forbid"`（虽然不传播到依赖，但应优先选择纯 safe 的依赖） |
| **测试覆盖率** | 中 | 依赖自身的测试覆盖率 ≥ 80% |
| **许可证兼容性** | 低 | MIT/Apache 2.0 优先，GPL 需要额外审查 |

### 4.3 自建 vs 采购的决策依据

| 能力 | 自建 | 采购/集成 | 理由 |
|------|------|-----------|------|
| API 版本化 | ✅ 自建 | — | 核心架构基础设施，不依赖外部服务 |
| 路由中间件 | ✅ 自建 | — | ~200 行 JavaScript，不需要框架 |
| 缓存失效广播 | ✅ 自建 | — | 基于已有 Redis 连接，不需要额外服务 |
| 断路器 | ✅ 自建 | — | 3 个依赖，state machine 简单 |
| 性能基准 | — | ✅ 集成 criterion/k6 | 使用社区标准工具，不自建 benchmark 框架 |
| API 文档生成 | — | ✅ 集成 utoipa | OpenAPI 生成是已解决的问题，不自建 |

---

## 5. 实施路线图

### 5.1 优先级总排序

```
P1 (必须 — 立即启动)        P2 (重要 — 下一阶段)         P3 (值得做 — 长期)
─────────────────────────────────────────────────────────────
・API 版本治理             ・多实例缓存一致性            ・性能基准持续化
・SPA 路由与深度链接       ・依赖降级与熔断              ・CI 性能回归检测
```

### 5.2 阶段划分与里程碑

**Phase 1 — 「API 契约 + 页面导航」（第 1-6 周）**

| 周 | 方向 | 里程碑 |
|----|------|--------|
| W1 | A · 审计 | 路由审计：126 个模块按「公共 API / 内部 API」分类，输出 API 分类清单 |
| W2 | A · 基础设施 | 引入 `utoipa`；构建版本解析中间件（Accept-Version header）；设计 `VersionAdapter` trait |
| W3 | A · 迁移 | 将核心公共 API（消息/协作/用户）迁移到 `/api/v1/`；旧路径保留为别名 |
| W4 | A · 文档 | 自动生成 OpenAPI spec；`GET /api/versions` 端点；deprecation header |
| W5 | B · 路由 | 实现 `url-route.js`（hash 路由 + pushState）；与 `context.js` 的 state 同步 |
| W6 | B · 深度链接 | 认证流程 + 深度链接恢复；房间/消息/直播流的可分享 URL |

**Phase 2 — 「多实例强化」（第 7-12 周）**

| 周 | 方向 | 里程碑 |
|----|------|--------|
| W7 | C · 基础设施 | `CacheInvalidationBus` trait + Redis pub/sub 实现；boot 时注册 listener |
| W8 | C · 集成 | 将 `participant_cache.invalidate()` 升级为广播失效；`room_member_cache` 同理 |
| W9 | C · 测试 | 多实例集成测试（两个 server 进程，验证缓存失效传播） |
| W10 | D · 基础设施 | 手写 `CircuitBreaker` state machine；`DegradationMatrix` 配置 |
| W11 | D · 集成 | 在 PG/NATS/Redis/AI 调用点接入断路器；`AppState` 中共享降级等级 |
| W12 | D · 用户面 | WebSocket 降级通知帧；`/api/admin/degradation` 手动降级端点；用户面 toast 指示 |

**Phase 3 — 「度量与自动化」（第 13-16 周，与 Phase 2 部分并行）**

| 周 | 方向 | 里程碑 |
|----|------|--------|
| W13 | E · 微基准 | `criterion` 集成；消息序列化/反序列化/嵌入计算的 3-5 个微基准 |
| W14 | E · 集成基准 | 消息发送全链路基准（WS → PG → NATS → WS） |
| W15 | E · 负载测试 | `k6` 脚本覆盖典型用户会话（登录 → 切换 → 发消息 → 搜索） |
| W16 | E · CI 集成 | nightly `bench` job，结果持久化；`[performance_budgets]` 声明；超预算 CI 告警 |

### 5.3 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| **API 版本化被抵制（"126 个模块改动太大"）** | 中 | 高 | 从仅 3-5 个核心公共模块开始，提供「渐进式版本化」方案，不追求一次性覆盖所有 |
| **SPA 路由引入导致现有页面跳转逻辑退化** | 中 | 中 | 分步发布：先只改 `showChat()` 内部（添加 hash 但不破坏现有 hidden 切换），再逐步替换视图切换 |
| **Redis pub/sub 在集群模式下丢消息** | 低 | 中 | 熔断/降级设计不依赖缓存失效的可靠性——30-60s TTL 保持为最终后备。丢消息最多回到当前状态 |
| **性能基准在 CI 中不可重复** | 高 | 中 | CI 中只跑相对基准（对比 baseline vs PR 的同一环境）；绝对值基准在专用性能环境跑 |
| **断路器误触发（探测间隔内恰巧超时）** | 中 | 中 | 连续失败 N 次才触发（建议 N=5，可配置）；半开探测间隔不短于 10s；提供手动重置端点 |

### 5.4 不建议放入同一阶段的组合

- **API 版本化 + SPA 路由不应该放在同一周**——一个后端重组、一个前端新模块。W1-W4 专注后端，W5-W6 专注前端。并行开发但不同人/不同 sprint。
- **缓存一致性 + 降级熔断可以并行**——两个都是后端架构改动，但触及不同的代码路径（缓存层 vs 外部依赖调用层）。可以分配给不同开发者。
- **性能基准在整个过程中持续做**——不集中在一个 sprint。Phase 1 就可以开始搭 criterion，Phase 1 结束时已经有 3 个微基准在跑。

---

## 总结性判断

从整体架构视角，5 个方向的优先级和依赖关系可以用一张图概括：

```
当前状态                             目标状态
─────────                            ─────────
                                     ┌─────────────────┐
126 裸路由模块          Phase 1      │ 版本化 API 契约   │ P1
无 OpenAPI 生成         ──────►      │ OpenAPI 自动生成  │
                                     │ Deprecation 协议   │
                                     └─────────────────┘
                                     ┌─────────────────┐
单视图 SPA 无路由       Phase 1      │ hash 路由 + 导航   │ P1
无深度链接              ──────►      │ 可分享 URL         │
刷新丢状态                            │ 刷新恢复上下文      │
                                     └─────────────────┘
                                     ┌─────────────────┐
进程内缓存各自为政       Phase 2      │ Redis pub/sub     │ P2
无跨实例失效            ──────►      │ sub-second 一致   │
30-60s 不一致窗口                     │ TTL 作为后备       │
                                     └─────────────────┘
                                     ┌─────────────────┐
依赖故障直扑用户         Phase 2      │ 熔断 + 降级矩阵    │ P2
无降级策略              ──────►      │ 用户面降级提示     │
行为不一致                            │ L1/L2/L3 层级      │
                                     └─────────────────┘
                                     ┌─────────────────┐
零性能基准               Phase 3      │ criterion + k6    │ P2
无容量规划数据           ──────►      │ 性能预算声明       │
无回归检测                            │ CI 回归告警        │
                                     └─────────────────┘
```

**核心结论**：这 5 个方向不是「锦上添花」的特性和增强，而是系统从「功能完整的原型」演进到「企业级生产平台」的**必经路径上的结构性缺口**。5 个方向的共同特征是在功能快速迭代期被优先级压下、现在需要系统性偿还的架构债。

其中 **API 版本化** 是缺口中的缺口——它是企业合同履行、第三方生态、移动端发布节奏的前置条件。如果整个 roadmap 只能做一件事，做这个。
