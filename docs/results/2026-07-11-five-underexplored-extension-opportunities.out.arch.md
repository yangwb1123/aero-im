以下是对《Aero IM — 全量代码扫描后：5 个尚未被系统性覆盖的扩展机会》的架构师视角分析。

---

# 架构师分析报告：Aero IM 五个未覆盖扩展机会的深入评估

> **基于**: `docs/requirements/2026-07-11-five-underexplored-extension-opportunities.md`  
> **交叉验证**: 138 份既有分析 + 设计 Spec (`docs/specs/2026-05-22-aero-im-design.md`) + AGENTS.md 约束  
> **方法**: 从架构评估、扩展方向、接口设计、技术选型、实施路线图五个维度逐一展开

---

## 一、架构评估

### 1.1 当前架构的优势

文档识别的 5 个方向，恰好折射出 Aero IM 架构的两个重大**设计选择**及其副作用：

**优势 1: 极致的服务端完备性**
- ~200+ API 端点、37 个 crate、35+ 常驻后台任务，覆盖完整的 IM + 直播 + 企业协作能力域
- 所有企业级功能（SSO、SCIM、2FA、Legal Hold、Webhook、Info Barrier）在服务端均已落地
- `AGENTS.md` 中 "feature-first 单位是 crate" 的治理原则确保了模块边界清晰、可独立演进

**优势 2: 事件驱动骨架的可靠性基础**
- NATS JetStream + per-subject seq + durable consumer 提供了 at-least-once 投递和跨实例扇出
- Hub 的 bounded mpsc 扇出防止了进程内连锁崩溃
- 「跨实例事实源 = NATS，集群状态 = Redis，进程内缓存 = 一致性问题拥有者」的三层模型清晰

**优势 3: 务实的设计决策**
- 「MLS E2E 仅 scaffold，不做客户端密码学」—— 避免了陷入客户端安全性的无底洞
- 「不做 Federation」—— 专注于单集群性能
- 直播媒体 seam 标记为「已建+已测，但未接线」—— 诚实的技术债务管理

### 1.2 架构债务与设计缺陷

文档识别的 5 个方向揭示了三个层级的架构债务：

| 层级 | 债务类型 | 方向 | 根因 |
|------|---------|------|------|
| **L1 可靠性** | 有意的数据不安全设计 | 方向一 | `claim_due` 先标记后发送的设计哲学以永久丢失为代价避免无限重试 |
| **L1 安全** | 安全边界随部署规模退化 | 方向三 | 限流状态进程本地化，水平扩缩导致安全边界线性衰减 |
| **L2 产品** | 服务器端完备但用户不可触达 | 方向二 | 设计初期以「API-first」为优先，Web SPA 定位为 debug client |
| **L2 UX** | 客户端无状态化 | 方向四 | 过度依赖服务端回填，客户端仅作为薄渲染层 |
| **L3 合规** | 数据生命周期管理不完整 | 方向五 | 删除路径没有覆盖所有内容痕迹表，GDPR 合规不完整 |

**关键观察**: 这 5 个方向不存在"架构错误"，而是**在设计优先级上的折中**: Aero IM 优先保证了服务端功能完备性和事件驱动可靠性，而牺牲了:
1. 定时消息的数据安全性（宁可丢失也不要无限重试）
2. 分布式限流的精确性（本地 DashMap 简单但不安全）
3. 前端覆盖率和状态持久化（Web SPA 仅用于联调）
4. 合规删除的完整性（保留 Message history 用于审计/恢复）

### 1.3 需要进行的设计决策重新评估

以下设计决策值得重新审视：

| 当前决策 | 当初合理 | 现在需重新评估的理由 | 建议调整 |
|---------|---------|-------------------|---------|
| `claim_due` 先标记后发送 | ✅ 避免无限重试 | 水平扩缩下丢失概率成倍增加；有更好的两阶段方案 | 改为两阶段 claim → send → confirm |
| 限流器进程本地 DashMap | ✅ 单实例时足够 | 多实例部署时安全边界退化 | 迁移主限流到 Redis，本地作为降级 |
| Web SPA 为 debug client | ✅ MVP 阶段合理 | 35+ 企业功能零 UI 覆盖，严重影响 GA | 升级为产品级 SPA，分层覆盖 |
| 客户端状态纯内存 | ✅ 简化前端复杂度 | 刷新丢失全部上下文，UX 差 | 渐进式引入 sessionStorage/localStorage |
| 删除只覆盖 messages 表 | ✅ 最小实现 | 合规要求需覆盖全部内容痕迹表 | 扩展 cascade_content_erase 管线 |

---

## 二、扩展方向（高价值架构扩展）

基于文档识别的 5 个方向，我进一步提炼出 **5 个高价值架构扩展方向**，每个包含详细的技术分析。

### 方向 A（从方向一升级）：定时任务调度引擎重构——从「乐观丢失」到「可追溯的两阶段交付」

**为什么需要（已从 P1 升级为 P0）**: 当前的 `claim_due` + `list_due` 模式存在双重问题——一次性消息永久丢失、周期性消息无锁定可能重复发送。对于一个 IM 协作平台，定时消息的可靠性直接影响用户信任。如果用户安排了「明天 9:00 发送重要提案」，结果因 sender 被移出房间而无声丢失——这是不可接受的。

**核心挑战**:

| 挑战 | 难度 | 方案选项 |
|------|------|---------|
| 两阶段提交 vs. 事务边界 | 🔴 高 | 选项 A: 单独状态列 `claimed/pending/delivered/failed`，异步超时回滚；选项 B: 用 PG 可序列化隔离 + 发送在事务内；选项 C: 发送为独立 Saga，包含补偿事务 |
| 重试策略与幂等性 | 🟡 中 | 重试间隔指数退避（30s/2min/5min/30min），`MAX_RETRIES`=5，之后进死信队列 |
| 死信队列的管理界面 | 🟢 低 | 复用既有 `ai_jobs.dead` 模式，新增 `scheduled_dead_letters` 表 |
| 发送者通知机制 | 🟡 中 | 复用 `system` bot 发送 DM 的既有路径，附失败原因 |
| 周期性消息的原子 claim | 🟡 中 | 引入 `FOR UPDATE SKIP LOCKED` 到 `list_due`，与 `reschedule` 在同一事务中 |

**预期的架构变更**:

```
当前:      claim_due(标记delivered_at) → send_message → (失败则静默丢弃)
          list_due(只读) → send_message → reschedule(无锁)

未来:      claim(标记claimed) → send_message → confirm(标记delivered_at)
                                                  ↓ (失败)
                                            mark_failed + 回滚或死信
```

具体变更文件:
- `aero-storage/src/scheduled.rs`: `claim_due` 改为 `claim`（状态列），新增 `confirm_delivery` / `mark_failed` / `timeout_reclaim`
- `aero-storage/migrations/`: 新增 `status` 列（`pending/claimed/delivered/failed`），新增 `scheduled_dead_letters` 表，新增 `attempts` 列，新增 `last_error` 列
- `aero-server/src/scheduled.rs`: `run_scheduled_dispatcher` 改为两阶段发送 + 超时回捡 loop
- `aero-storage/src/recurring.rs`: `list_due` 加 `FOR UPDATE SKIP LOCKED`，`reschedule` 与其同一事务

**对现有系统的影响**:
- `claim_due` 的调用者只有 `run_scheduled_dispatcher`，影响面小
- 新增的定时回捡 loop 需要在 `bin/boot/` 注册（复用既有 background task 模式）
- `recurring` 的变更可能影响性能（`FOR UPDATE SKIP LOCKED` 行锁），但 `list_due` 本来就在 PG 上操作，延迟可接受

### 方向 B（从方向二升级）：UI 分层覆盖策略——从 "API-first" 到 "GA-ready" 的前端治理

**为什么需要（P1 产品缺口）**: 这不是「重建前端」的问题，而是**产品成熟度**的问题。Aero IM 有完整的企业 IM 能力集，但 90% 的功能仅可通过 curl/Postman 使用。企业客户购买的是产品，不是 API 集合。

**核心挑战**:

| 挑战 | 难度 | 方案选项 |
|------|------|---------|
| 30+ 功能域的 UI 实现优先级排序 | 🟡 中 | 按用户影响分 P0-P4（详见路线图 §5） |
| Web SPA 架构从 debug client 升级为产品级 | 🔴 高 | 选项 A: 渐进式增强现有 SPA；选项 B: 使用 React/Vue 重写；选项 C: 保持零依赖 ES2020，但引入组件化模式 |
| 后端 API 与前端交互的契约化 | 🟡 中 | 引入 OpenAPI 规范（当前是手写示意性文档） |
| 多语言 i18n 的需求 | 🟢 低（延迟） | 方向二中列出的 30+ 功能域可先只做中文/英文 |

**预期的架构变更**:

```
当前: web/ 是扁平的 JS 文件集合，api.js 暴露 ~35 个函数
      routes/routes.rs 无前端端的结构性映射

未来: web/ 按功能域组织:
      web/
      ├── api.js            (HTTP 客户端层，可考虑从 OpenAPI 生成)
      ├── state.js           (集中式状态管理 + 持久化层)
      ├── pages/             (按路由组织的页面模块)
      │   ├── login/
      │   ├── workspace-settings/  (SSO/SCIM/Webhook)
      │   ├── admin/               (Legal Hold/Info Barrier)
      │   └── ...
      ├── components/        (可复用 UI 组件)
      └── worker.js          (Service Worker)
```

**对现有系统的影响**:
- 现有 SPA 的 `api.js` 是零依赖的 ES2020 模块，没有框架锁定。升级为产品级 SPA 时，可以选择保持零依赖（学习曲线低）或引入框架（生态丰富但需重构）
- 关键是不需要一次性重建：可以按 P0→P4 顺序增量交付
- 后端 API 已经存在，前端开发者只需要调用——不需要后端配合

### 方向 C（从方向三升级）：分布式限流基础设施——从 "进程内 DashMap" 到 "Redis 主控 + 本地降级"

**为什么需要（P1 安全缺口）**: 文档的表格已经量化了问题——在 Redis 下线场景下，N 实例 = N × 配额。对于一个承载企业通信的平台，限流失败意味着凭据爆破防护失效、资源耗尽攻击奏效。

**核心挑战**:

| 挑战 | 难度 | 方案选项 |
|------|------|---------|
| Redis 为主限流器的性能开销 | 🟡 中 | Token Bucket 用 Lua EVAL 实现，单次请求增加 0.5-2ms，可启用 pipeline |
| Redis 故障时的降级策略 | 🔴 高 | 选项 A: fail-open（当前行为，降级到本地 DashMap，但发告警）；选项 B: fail-closed（全局 429，更安全但影响可用性）；选项 C: fail-degraded（降低本地配额到 1/N） |
| 已有本地 DashMap 的迁移路径 | 🟢 低 | 在 `RateLimiter` 上加 `RedisBackend` 实现，DashMap 作为后备；`check` 方法先查 Redis，Redis 不可用时回退 DashMap |
| WS 限流与 HTTP 限流的统一 | 🟡 中 | WsRateStore 已用 Redis，可以与新的 HTTP Redis 限流共享键命名空间 |

**预期的架构变更**:

```
当前: DashMap<ClientKey, Bucket> (完全进程本地)
      + check_cluster_rate (60 req/min 固定窗口，fail-open)

未来: 
      HTTP 请求 → rate_limit::check(key)
                   ├── Redis 可用 → EVAL Lua Token Bucket (原子滑动窗口)
                   └── Redis 不可用 → DashMap(本地) + alarm

      WsRateEnforcer → 复用同一 Redis 键命名空间
```

具体变更:
- `aero-server/src/rate_limit.rs`: 新增 `RedisTokenBucket` 实现，使用 `EVAL` 脚本；`RateLimiter` 结构体增加 `redis_backend: Option<RedisTokenBucket>`
- 配置项新增 `AERO_RATE_LIMIT_REDIS_FAIL_CLOSED`（默认 false）
- 配置项新增 `AERO_RATE_LIMIT_REDIS_URL`（默认从既有 Redis 连接读取）
- 如果既有 Redis 连接池（fred）是进程中唯一的 Redis 连接，需要确认无资源竞争——限流操作是轻量的 `INCR/EXPIRE`，不会阻塞其他操作

**选项分析 — Redis 限流的实现策略**:

| 方案 | 精度 | Redis 开销 | Lua 复杂度 | 适用场景 |
|------|------|-----------|-----------|---------|
| INCR + EXPIRE 固定窗口 | 粗（窗口边界可能突变） | 低（2 命令） | 无 | 集群级粗粒度限流 |
| ZADD + ZREMRANGEBYSCORE 精确滑动窗口 | 精确 | 中（O(log N)） | 低 | 需要精确计数的场景 |
| Lua Token Bucket | 精确滑动 | 低（1 EVAL） | 中 | 本文推荐方案，对齐本地行为 |
| Redis Cell (generic-cell-rate-limiter) | 精确 | 最低 | 无（模块命令） | 需要安装 Redis 模块，部署复杂 |

**推荐**: Lua Token Bucket。原因：对齐现有本地 Token Bucket 行为，Redis 模块无依赖，单一 `EVAL` 调用的开销可控。

### 方向 D（从方向四升级）：客户端状态持久化——从 "无状态薄客户端" 到 "有状态弹性客户端"

**为什么需要（P2 UX 缺口）**: 文档中「刷新后 watcher 状态丢失→直播停止推送」是一个具体的、可复现的用户体验问题。更广泛地，每次刷新都看到空白聊天界面，直到 REST 调用返回，这在高延迟网络下尤其糟糕。

**核心挑战**:

| 挑战 | 难度 | 方案选项 |
|------|------|---------|
| 缓存一致性——本地缓存 vs 服务端状态 | 🟡 中 | 选项 A: 本地缓存作为「渲染加速」，写操作后无效化；选项 B: 本地缓存作为「事实源」之一，与 WebSocket 事件合并；选项 C: 仅缓存非关键状态（草稿、watcher） |
| 存储容量管理 | 🟢 低 | `sessionStorage` 限额 ~5-10MB，每条消息 ~200 字节，每房间缓存 100 条 ≈ 20KB，30 房间 ≈ 600KB，远低于限额 |
| 敏感数据在 localStorage 中的安全 | 🟡 中 | 不缓存 JWT（已使用 httpOnly cookie 方案更好）；不缓存消息内容中的敏感信息；可加密存储 |
| Service Worker 的注册与更新策略 | 🔴 高 | SW 缓存失效和版本管理是经典难题；可作为 P3 延迟 |

**预期的架构变更**:

```
当前: state.messagesByRoom = new Map()  // 纯内存
      state.watchedStreams = new Set()  // 纯内存

未来: web/
      ├── state.js           # 状态管理 + 持久化中间件
      ├── persistence.js     # 封装 sessionStorage/localStorage/IndexedDB
      └── worker.js          # Service Worker (P3)
```

具体:
- 新增 `web/persistence.js`: 封装存储操作，自动 JSON 序列化/反序列化，LRU 淘汰
- `web/state.js`: 在每个 setter 中调用 `persistence.set(key, value)` + debounce
- `web/app.js`: 监听 `DOMContentLoaded`，从 `persistence` 恢复房间列表、未读计数、watcher、草稿

**方案对比 — 客户端状态存储技术选型**:

| 存储 | 限额 | 同步/异步 | 生命周期 | 适用场景 |
|------|------|----------|---------|---------|
| `sessionStorage` | ~5-10MB | 同步 | 标签页生命周期 | 消息缓存、当前房间状态 |
| `localStorage` | ~5-10MB | 同步 | 持久 | 草稿、watcher、未读计数、偏好设置 |
| `IndexedDB` | 大（磁盘空间） | 异步 | 持久 | 全文搜索索引、离线消息队列（P3） |
| `CacheStorage` | 取决于浏览器 | 异步 | SW 控制 | Service Worker 资源预缓存 |

**推荐**: 先 `sessionStorage`（消息缓存）+ `localStorage`（草稿/watcher/未读），P3 再引入 IndexedDB。

### 方向 E（从方向五升级）：数据生命周期治理——从 "消息级软删" 到 "全表级内容擦除管线"

**为什么需要（P2 合规缺口）**: 文档列举了 `message_history`、`audit_events.detail`、`webhook_delivery_log.payload`、`notification_history` 等未被覆盖的内容痕迹表。对 GDPR/CCPA/FINRA 合规至关重要。

**核心挑战**:

| 挑战 | 难度 | 方案选项 |
|------|------|---------|
| 级联擦除的范围和性能 | 🔴 高 | 选项 A: 同步级联（当前事务中擦除所有关联表）；选项 B: 异步级联（当前事务中写入擦除队列，后台 drain）；选项 C: 软删 + 清扫器模式（与现有 retention sweep 类似） |
| 审计链完整性 vs 内容擦除 | 🔴 高 | 审计事件不能硬删除（维护审计链），但必须清除 PII/内容；需要区分「删除事件」和「匿名化事件」 |
| 法务保全与擦除的互斥 | 🟡 中 | 被保全的消息不能在保全期内被擦除；保全解除后触发延迟擦除 |
| 大型工作区的批量擦除性能 | 🟡 中 | 工作区级擦除可能涉及百万级消息，需要分页 drain |

**预期的架构变更**:

```
当前: soft_delete_audited(id) → messages.blocks = NULL
      audit_events.detail → 保留
      message_history.blocks → 保留

未来: cascade_content_erase(message_id, scope)
        ├── messages.blocks = NULL (当前)
        ├── message_history.blocks = NULL, searchable_text = NULL
        ├── webhook_delivery_log.payload = NULL (对应 message_id)
        ├── notification_history.message_text = NULL
        └── audit_events.detail → anonymize (清内容字段，保留结构)

      purge_workspace_content(workspace_id)  // 工作区级擦除
        → 遍历消息 → cascade_content_erase 每一条
```

具体:
- `aero-storage/src/content_erasure.rs`: 新增模块，包含 `cascade_content_erase`、`anonymize_audit_event`、`purge_workspace_content`
- `aero-storage/migrations/`: 新增 `CREATE TABLE IF NOT EXISTS content_erasure_queue`（异步模式时使用）
- `aero-core/service/events.rs`: `soft_delete_audited` 扩展为可选择调用 `cascade_content_erase`
- `aero-server/src/legal_holds.rs`: `release_hold` → 触发 `purge_held_content`
- `aero-server/src/workspaces.rs`: `DELETE /api/workspaces/:id/purge`（管理端 API）

---

## 三、接口设计原则

### 3.1 定时任务引擎接口

```rust
// 当前（有问题）:
fn claim_due(until: DateTime, limit: u32) -> Vec<ScheduledMessage>
// ↑ 内部 SET delivered_at = now()，发送失败无法恢复

// 未来（两阶段）:
fn claim(until: DateTime, limit: u32) -> Vec<ScheduledMessage>
// ↑ SET status = 'claimed', claimed_at = now(), attempts = attempts + 1
//   注意：不是 SET delivered_at

fn confirm_delivery(id: ScheduledMessageId) -> Result<()>
// ↑ SET status = 'delivered', delivered_at = now()

fn mark_failed(id: ScheduledMessageId, error: String) -> Result<()>
// ↑ 如果 attempts < MAX_RETRIES: SET status = 'pending' (回滚)
//   如果 attempts >= MAX_RETRIES: INSERT INTO scheduled_dead_letters + SET status = 'failed'

fn timeout_reclaim(older_than: Duration) -> Vec<ScheduledMessage>
// ↑ 回捡超时未提交的 claimed 行: SET status = 'pending' (回滚)
```

**设计原则**: 保持与现有 `ImService::send_message` 接口不变。新增的 claim/confirm 接口只需在 `scheduled::ScheduledRepo` 层修改。上层 `run_scheduled_dispatcher` 的流程从单步改为三步。

### 3.2 分布式限流接口

```rust
// 当前:
pub struct RateLimiter {
    buckets: Arc<DashMap<ClientKey, Bucket>>,
    rate: f64,
    capacity: f64,
}
impl RateLimiter {
    pub fn check(&self, key: &ClientKey) -> Result<()>;
}

// 未来:
pub enum RateLimiterBackend {
    Redis(RedisTokenBucket),
    Local(DashMap<ClientKey, Bucket>),
    Degraded { local: DashMap<ClientKey, Bucket>, divisor: f64 },
}

pub struct RateLimiter {
    primary: RateLimiterBackend,   // Redis (在线时)
    fallback: RateLimiterBackend,  // Local (降级时)
    fail_closed: bool,
}

impl RateLimiter {
    pub fn check(&self, key: &ClientKey) -> Result<()> {
        // 先试 primary (Redis)
        // Redis 故障时: 如果 fail_closed → Err(429), 否则 → fallback
    }
}
```

**设计原则**: `check()` 接口签名不变，内部实现透明替换。调用方不需要知道后端是 Redis 还是本地。

### 3.3 内容擦除管线接口

```rust
/// 级联擦除一条消息的所有内容痕迹
/// - scope: 控制擦除范围（仅消息 / 含审计 / 含通知 / 完全）
/// - 返回被擦除的表列表（用于审计日志）
pub async fn cascade_content_erase(
    conn: &PgPool,
    message_id: MessageId,
    scope: ErasureScope,
) -> Result<Vec<ErasureTarget>>;

pub enum ErasureScope {
    MessageOnly,           // 仅 messages 表
    IncludeHistory,        // + message_history
    IncludeWebhookAndNotification, // + webhook_delivery_log + notification
    Full,                  // + audit_events (匿名化)
}

/// 工作区级完全擦除
pub async fn purge_workspace_content(
    conn: &PgPool,
    workspace_id: WorkspaceId,
    strategy: PurgeStrategy,
) -> Result<PurgeReport>;

pub enum PurgeStrategy {
    Synchronous,                     // 同步执行（工作区数据量小）
    Async { batch_size: u32 },       // 分页 drain（大工作区）
}
```

**设计原则**: 保持 `soft_delete_audited` 现有行为不变（向后兼容），新增 `cascade_content_erase` 作为可选扩展。现有调用点选择性升级。

### 3.4 不需要引入的新抽象层

| 被认为可能需要的抽象 | 评估结论 | 原因 |
|-------------------|---------|------|
| 通用调度引擎抽象 | ❌ 不需要 | Aero IM 已有 `bin/boot/` 的统一 task 注册模式 + 定时器既有 `MissedTickBehavior::Skip` 治理，不需要 Quartz/Celery 式的通用调度器 |
| 通用缓存抽象层 | ❌ 不需要 | 客户端状态持久化直接使用浏览器原生 API，不需要类 Redux 框架；服务端已经有 `participant_cache` 模式，不需要通用 cache abstraction |
| 通用合规引擎 | ❌ 不需要 | 内容擦除是数据操作层的事，不需要独立的合规规则引擎 |
| API 规范化层 | ✅ 建议引入 | `web/api.js` → 考虑生成式 API 客户端（从现有 handler 签名生成 TypeScript 类型） |

---

## 四、技术选型

### 4.1 需要引入的新技术依赖

| 方向 | 建议引入 | 理由 | 风险评估 |
|------|---------|------|---------|
| B（前端治理） | **TypeScript**（可选渐进式，从 `.js` 到 `.ts` 逐步迁移） | 35+ 功能域的 UI 代码复杂度需要类型系统控制 | 🔴 需要重构构建流程；当前零依赖 SPA 没有 webpack/babel，引入 TS 需要构建链 |
| D（客户端持久化） | 无新依赖（使用浏览器原生 `sessionStorage`/`localStorage`/`IndexedDB`） | Aero IM 的策略是零依赖 SPA，这个方向不需要新框架 | 🟢 无 |
| C（分布式限流） | 无新依赖（复用既有 `fred` Redis 客户端） | Lua `EVAL` 不需要额外 crate | 🟢 `fred` 已有 eval 支持 |
| A（定时任务） | 无新依赖（纯 SQL + Rust 实现） | PG 的 `SKIP LOCKED` + 事务已足够 | 🟢 无 |
| E（内容擦除） | 无新依赖（纯 SQL + Rust 实现） | 级联更新不需要外部工具 | 🟢 无 |

**关键结论**: 这 5 个方向中，**4 个不需要引入新依赖**。唯一可能需要 TypeScript 的方向 B，也可通过渐进式 JSDoc 注释 + `// @ts-check` 的方式在不引入构建链的情况下获得类型安全。

### 4.2 安全性评估—Redis 限流的 Lua 脚本

Redis 限流需要引入 `EVAL` 调用。需要评估的性能影响：

```lua
-- Token Bucket 限流 (Lua)
local key = KEYS[1]
local rate = tonumber(ARGV[1])     -- tokens/second
local capacity = tonumber(ARGV[2]) -- burst capacity
local now = tonumber(ARGV[3])
local cost = tonumber(ARGV[4])     -- 通常是 1

local bucket = redis.call('HMGET', key, 'tokens', 'last_refill')
local tokens = tonumber(bucket[1] or capacity)
local last_refill = tonumber(bucket[2] or now)

-- 计算补充
local elapsed = math.max(0, now - last_refill)
local refill = elapsed * rate / 1000  -- rate per ms
tokens = math.min(capacity, tokens + refill)

if tokens >= cost then
    tokens = tokens - cost
    redis.call('HMSET', key, 'tokens', tokens, 'last_refill', now)
    redis.call('EXPIRE', key, math.ceil(capacity / rate) + 1)  -- TTL 自动清理
    return 1  -- 允许
else
    redis.call('HMSET', key, 'tokens', tokens, 'last_refill', now)
    return 0  -- 拒绝
end
```

性能特征：每个限流检查 = 1 `EVAL` 调用（不是 2 个往返），~0.5-2ms Redis 开销。对于 10K QPS 的请求量，Redis 单实例可承受 50K+ EVAL/s，不是瓶颈。

### 4.3 自建 vs. 采购的决策

| 能力 | 自建 | 采购 | 推荐 |
|------|------|------|------|
| 定时调度引擎 | ✅ 已有骨架，只需加固 | ❌ 没有现成的 Rust 定时消息库 | **自建**（2-3 周） |
| Web SPA 升级 | ✅ 后端已完备，前端无框架锁定 | ❌ 没有现成的企业 IM 前端（即使用框架也要大量定制） | **自建**（分阶段 3-6 月） |
| 分布式限流 | ✅ REdis + Lua 方案 | ❌ 没有成熟的 Rust Redis 限流库 | **自建**（1 周） |
| 客户端持久化 | ✅ 浏览器原生 API | ❌ 没有适用的 Service Worker 框架 | **自建**（2 周） |
| 内容擦除合规 | ✅ SQL 级联更新 | ❌ 领域特定，无法采购 | **自建**（1-2 周） |

**结论**: 所有 5 个方向都应自建。Aero IM 的既有骨架（PG + Redis + NATS + 浏览器原生 API）已经提供了足够的基础设施。

---

## 五、实施路线图

### 5.1 优先级排序

```
P0 (立即 — 下一 Sprint)    ← 方向 A (从 P1 升级) + 方向 C (P1 安全)
   |- 定时调度两阶段提交 + 死信队列 [方向A]
   |- Redis 主控分布式限流 [方向C]

P1 (GA 前必须)              ← 方向 E (P2 合规) + 方向 B 的 P0 子集
   |- 级联内容擦除管线 [方向E]
   |- 密码重置 UI + 2FA 绑定 UI [方向B - P0子集]
   |- 工作区设置管理 UI (SSO/SCIM/Webhook) [方向B - P1子集]

P2 (GA 后 Q1)              ← 方向 D (P2) + 方向 B 剩余
   |- sessionStorage 消息缓存 [方向D]
   |- localStorage 草稿 + watcher 持久化 [方向D]
   |- 用户自助 UI (会话/PAT/通知偏好/关键词提醒) [方向B - P2子集]

P3 (GA 后 Q2)              ← 方向 B 的深水区 + 方向 D 增强
   |- 协作增强 UI (Canvas/任务/审批/组织架构) [方向B - P3子集]
   |- IndexedDB 离线消息队列 + Service Worker 预缓存 [方向D - P3]

P4 (远期)                   ← 方向 B 的直播管理
   |- 直播管理 UI (分类/订阅/切片/数据面板) [方向B - P4子集]
```

### 5.2 阶段划分和里程碑

#### 阶段 1：可靠性加固（2-3 周）
- **目标**: 消除 P1 数据丢失风险 + P1 安全边界退化
- **交付物**:
  - `scheduled_messages` 的两阶段提交流程
  - `scheduled_dead_letters` 死信队列
  - `recurring` 的 `FOR UPDATE SKIP LOCKED`
  - Redis 为主限流器 + fail-closed 配置项
  - WsRateStore 与 HTTP 限流共享键命名空间
- **里程碑**: 定时消息 100% 可追溯，多实例限流安全边界不退化

#### 阶段 2：内容合规 + 关键 UI（2-3 周）
- **目标**: 满足 GDPR 合规 + 用户登录/2FA 自助操作
- **交付物**:
  - `cascade_content_erase` 管线
  - 审计事件 PII 匿名化
  - 工作区级内容擦除
  - 密码重置 UI 页面
  - 2FA 绑定/恢复码 UI
  - 工作区设置管理 UI（SSO/OIDC/SAML/SCIM/Webhook 配置）
- **里程碑**: 可通过 UI 完成首次登录、2FA 绑定、工作区配置

#### 阶段 3：客户端体验提升（2-3 周）
- **目标**: 消除「刷新即空白」的用户体验问题
- **交付物**:
  - `sessionStorage` 消息缓存（最近 100 条/房间）
  - `localStorage` 草稿持久化（发送后清除）
  - `localStorage` watcher 状态持久化（刷新后自动恢复）
  - `localStorage` 未读计数还原
- **里程碑**: 页面刷新后 3 秒内恢复完整聊天视图

#### 阶段 4：产品化冲刺（4-6 周）
- **目标**: 覆盖企业用户的日常自助操作
- **交付物**:
  - 用户自助 UI（会话管理、PAT、通知偏好、关键词提醒、消息模板、我的导出）
  - 协作增强 UI（Canvas 视图、任务面板、审批列表、组织架构图）
  - 搜索高级操作 UI（saved_searches、search_advanced）
- **里程碑**: 企业用户的 90% 日常操作可在 UI 完成

#### 阶段 5：直播 + 离线（远期）
- **目标**: 覆盖直播管理 + 离线能力
- **交付物**:
  - 直播管理 UI（分类、订阅、切片、数据面板）
  - Service Worker 预缓存 + IndexedDB 离线消息队列
- **里程碑**: 直播创作者可在 UI 管理全部直播流程；弱网/离线可加载应用壳

### 5.3 风险点和缓解策略

| 风险 | 影响范围 | 概率 | 缓解策略 |
|------|---------|------|---------|
| 定时调度两阶段提交引入新的故障模式（长期 claimed 僵死行） | 方向 A | 🟡 中 | 必须有超时回捡 loop + Prometheus 告警（`claimed` 行数 > 阈值）；设置 `claimed_at` + 超时回滚窗口 |
| Redis 限流增加请求延迟 | 方向 C | 🟢 低 | Lua EVAL 在 Redis 内完成全部操作，1 次往返，~0.5-2ms；启用 Redis pipeline |
| Redis 限流 fail-closed 导致系统全面不可用 | 方向 C | 🟢 低（Redis 高可用） | 默认 fail-open（发告警）；fail-closed 作为配置项，仅在安全敏感场景启用 |
| 前端代码复杂度失控（方向 B 的 30+ 功能域 UI） | 方向 B | 🔴 高 | 必须分阶段、不允许并行推进多个 UI 域；每个 UI 域需要有独立的上线 check-list；重用现有 JS 模式避免框架锁定 |
| `localStorage` 消息缓存与 WebSocket 实时更新的竞态 | 方向 D | 🟡 中 | 缓存作为「渲染加速」，WebSocket 事件总是覆盖缓存值；加载时先展示缓存再替换为实时数据 |
| 内容擦除影响法务保全 | 方向 E | 🟡 中 | `cascade_content_erase` 必须检查 `legal_holds` 表，跳过被保全的消息；保全解除后触发延期擦除 |
| 阶段 4 的企业 UI 导致前端的 JS 体积膨胀 | 方向 B | 🟡 中 | 延迟加载 + 按路由分 chunk；当前零依赖 SPA 没有 bundle，直接「页面加载时 import」即可 |

---

## 六、总结：架构师最终建议

### 6.1 核心原则

1. **不要重建系统**。这 5 个方向不需要改变 Aero IM 的事件驱动架构骨架，而是在现有架构上做增量加固。
2. **可靠性优先于功能**。方向 A（定时消息丢失）和方向 C（限流失效）是 P1 安全+可靠性问题，应优先处理。
3. **分层交付，避免大爆炸式改造**。方向 B 的 30+ 功能域 UI 如果尝试一次性重建，必然失败。按 P0→P4 分 5 层覆盖。
4. **向后兼容是硬约束**。所有接口变更（`claim_due`→`claim`/`confirm`、`RateLimiter::check`）必须保持签名不变，内部实现透明替换。

### 6.2 一周内可开始的动作

1. **方向 A**: `scheduled.rs` 的 `claim_due` 改为 `claim`（新增 `status` 列），`run_scheduled_dispatcher` 改为两阶段发送 + 超时回捡。这是纯代码变更，不需要迁移。
2. **方向 C**: `rate_limit.rs` 新增 `RedisTokenBucket` 实现，复用现有 fred 连接。配置项新增 `AERO_RATE_LIMIT_REDIS_FAIL_CLOSED`。
3. **方向 E**: 在 `soft_delete_audited` 旁边新增 `cascade_content_erase`，扩展但不破坏现有行为。

这三个方向可以在 1 周内完成实现和测试，消除最严重的 P1 风险。

### 6.3 三个月内的战略方向

将 Web SPA 从「debug client」升级为「产品级 SPA」是 Aero IM GA 前最大的剩余工作。这不是技术挑战（后端已完备），而是产品管理的挑战——需要精确的优先级排序和分阶段交付规划。

推荐的做法是：**不要一次性重建 SPA，而是将方向 B 的 35 个功能域作为 35 个独立的「UI 覆盖单元」**，每个单元有独立的验收标准、测试用例和上线 check-list。每个 Sprint 覆盖 2-3 个单元，按用户影响排序。这样 3-4 个月后，覆盖率达到 80%+，GA 可期。

---

*本分析 2026-07-12 由架构师角度生成，建议每轮 Sprint Planning 时参考。*
