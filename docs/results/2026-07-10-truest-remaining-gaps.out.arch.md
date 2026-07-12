# 架构分析：Aero IM — 基于第七轮代码验证的评估与扩展方向

## 1. 架构评估

### 1.1 当前架构的优势

**事件驱动骨架——稳健且可演化**
Aero IM 最坚实的架构决策是以 NATS JetStream 作为跨实例事实源（source of truth）总线。`Hub` 进程内扇出 + NATS 跨节点投递的二元结构，实现了一个清晰且可水平扩展的实时数据管道。具体优势：

- **解耦彻底**：业务层（`ImService`、直播 handler）只关心 `publish_room_event`/`publish_stream_event`，不感知扇出拓扑。这是高阶消息总线的正确使用方式。
- **持久性分层合理**：durable consumer（`aero-server`）保证房间事件不丢；ephemeral consumer（`live.stream.*`）容忍丢失——这个决策反映了对两类事件**可用性需求差异**的深刻理解，值得称赞。
- **seq 编号**跨实例去重/排序的基础设施就位，这对 at-least-once 语义下的客户端状态机至关重要。

**crate 边界与领域一致**
从叶子 crate（`aero-common`）到组合 crate（`aero-server`）的依赖方向清晰。媒体 seam（whip/webrtc/sfu）隔离在独立 crate 中并通过 `Cargo.toml` 精确控制 str0m 依赖范围，是优秀的依赖管理实践。

**智能体（bot/worker/timer）架构成熟**
`agent_bot`、`ooo_bot`、`unfurl_bot`、`transcribe_bot`、`push_bot`、`moderation_bot` 各负责一个明确的能力域。它们的 fail-open、幂等守卫（dedup key / `ON CONFLICT DO NOTHING` / loop-guard）体现了对**分布式系统边界条件的深度认知**。特别是 `moderation_bot` 的**双成本预算**（per-ws + 全局）结合 defer 而非 drop 的背压策略，是 `AiWorker` 设计中值得作为模板反复使用的模式。

**AuthN/AuthZ 架构坚实**
`AuthUser` extractor + `assert_room_access(participant, room)` + CI lint（`authz_lint`）构成了一个可审计、可测试的授权架构。`ip_allowlist` 的 fail-open（空名单放行）和豁免管理路由防自锁的设计是典型的防御性工程。

### 1.2 关键架构局限性

**缓存一致性协议缺失——这是最大的架构债务**

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│  Instance A  │     │  Instance B  │     │  Instance C  │
│  ┌─────────┐ │     │  ┌─────────┐ │     │  ┌─────────┐ │
│  │ HashMap │ │     │  │ HashMap │ │     │  │ HashMap │ │
│  │ cache   │ │     │  │ cache   │ │     │  │ cache   │ │
│  └─────────┘ │     │  └─────────┘ │     │  └─────────┘ │
│     stale    │     │    stale     │     │    stale     │
└──────┬───────┘     └──────┬───────┘     └──────┬───────┘
       │                    │                    │
       └────────────────────┼────────────────────┘
                            │
                    ┌───────▼───────┐
                    │    Redis      │
                    │  (source of   │
                    │   truth)      │
                    └───────────────┘
```

当前三个缓存（`participant_cache`、`room_member_cache`、AI answer cache）全部是 **进程本地 `HashMap`，无跨节点失效机制**。这会引发一系列问题：

| 场景 | 失效时机 | 问题 |
|------|---------|------|
| 用户更新个人资料 (Instance A) | `participant_cache.invalidate` 仅在 A | B/C 返回旧资料直到 TTL 过期或下一 NATS 事件触发 |
| 管理员修改房间角色 (Instance B) | `room_member_cache.invalidate` 仅在 B | A/C 继续使用旧角色授权 |
| AI 摘要生成 (Instance C) | `cache_answer_invalidate_room` 使用 `SMEMBERS+DEL` | 非原子；B 可能在 `SMEMBERS` 和 `DEL` 之间读并写回旧值 |

**严重性评估**：
- participant 缓存不一致**不破坏安全**（最终授权在 DB），但产生**不一致的用户体验**（修改头像后旧实例看到旧头像）
- room_member 缓存的 stale read 若被传递给 `assert_room_access`... 等一下，`assert_room_access` 每次查 DB 吗？如果它查的是缓存，则角色变更会有**安全窗口**。这是需要紧急核查的架构边界——确认 `assert_room_access` 的调用链中是否有使用缓存。

**校验建议**：追查 `assert_room_access` 的实现——它应该每次都查 DB（或至少从 Redis 读权威数据），不能信任进程内缓存的 room_member 数据来做授权决策。如果它用了缓存，那么这是一个 P0 安全债务。

**无 BFF 模式——Web SPA 的可维护性代价**

验证确认了无 BFF。这意味着：

- Web SPA 直接消费后端 domain type（`RoomEvent`、`ServerFrame`）
- 前端无法进行字段裁剪（over-fetching）
- 后端合约变更导致前端部署紧耦合

对于当前规模（单 SPA 约 100+ 路由能力），这不是立即的危机。但随着前端复杂度增长（特别是 poll UI、AI 流式渲染、直播互动），「后端 API 即前端 API」会变成**演进摩擦**。

**迁移计数——工程债的信号**

`AGENTS.md` 明确声明「迁移计数勿在文档硬编码」，因为迁移通过 `sqlx::migrate!("../../migrations")` 编译期嵌入。这一设计有三个隐含问题：

1. **迁移和二进制强耦合**：任何迁移变更强制全量 rebuild → 部署包更新 → 回滚困难。在 CI/CD 流程中，这意味着迁移的回滚必须与二进制回滚严格同步。
2. **无迁移版本号可查**：运营人员无法在运行时回答「当前数据库在哪个迁移版本」——因为 `_sqlx_migrations` 表的记录号与文件序号绑定，但文件序号不在运行时暴露。
3. **迁移即部署**：加一个 typo 修复的 SQL 文件，就产生一次新部署。对于频繁迁移的环境，这会产生不必要的部署压力。

> 这不是紧急问题，但是一种「迁移即部署」的耦合债务。一个更灵活的模式是运行时可配置的迁移路径（如 `sqlx migrate run --source`），但代价是增加部署复杂度。

### 1.3 关键设计决策评估

| 决策 | 评价 | 说明 |
|------|------|------|
| 进程内缓存 + Redis 做源 | ✅ 合理 | 对 participant 这种读多写少的场景，本地缓存大幅降低 Redis 负载。但需要失效机制。 |
| `NATS JetStream durable consumer` 作为广播总线 | ✅ 优秀 | 比 Redis pub/sub 更可靠（持久化、重放），比 Kafka 更轻量。 |
| `sqlx::migrate!` 编译期嵌入 | ⚠️ 可接受 | 见上分析。对单服务部署有利，多服务/蓝绿部署需额外流程管理。 |
| `tag="kind"` 加 serde rename 规避字段冲突 | ✅ 合理 | Rust 的 serde 限制导致这个变通。但此模式要求所有 enum 变体都记得用 rename，靠 review 防止遗漏。 |
| `authz_lint` 作为 CI 门禁 | ✅ 优秀 | 在编译时捕获 IDOR 是 Rust 元编程的出色应用。 |


## 2. 扩展方向

基于当前代码验证和架构分析，我识别出 5 个高价值架构扩展方向。**注意**：这些方向独立于文档中已有的 5 个方向（已被验证为有效）——我聚焦于验证中新暴露的架构级问题。

### 方向 A：分布式缓存失效总线（P1）

**为什么需要**

当前每个实例的 `HashMap` 缓存只能被本地写操作失效。在多实例部署中（如蓝绿部署、水平扩展），更改发生在实例 A 但请求打到实例 B 时，缓存返回 stale 数据。虽然核心授权路径不应该（也不能）信任本地缓存，但 participant 显示信息、房间元数据等非关键路径的 stale 窗口产生了**不一致的用户体验**。

此外，AI 答案缓存的 `SMEMBERS+DEL` 非原子操作在所有实例共享一个 Redis 键集合时存在写入-删除-写入的竞态窗口：

```
时间线：
Instance A: SMEMBERS key → 得到 [old_val]
Instance B:                               SMEMBERS key → 得到 [old_val]
Instance A: DEL key
Instance A: SADD key new_val_A
Instance B:                                   SADD key new_val_B  ← 覆盖了 A 的写入
```

虽然 AI 缓存失效不是安全关键，但缓存中毒（返回过时或错误答案）对用户体验的损害不可忽视。

**核心挑战**

1. **无新增依赖**：不应引入 Redis pub/sub 或 NATS 以外的新技术。NATS 已经就位，是天然的失效广播通道。
2. **选择性失效**：不是所有变更都需要广播。只有多实例共享的、写操作发生在单实例上的数据需要失效广播。只读操作和本就单实例的数据（如 live stream watchers）无需参与。
3. **扇出范围**：如果我们在 NATS 上用 `cache.invalidate.{cache_name}` subject 广播，所有实例都会收到。30 个实例的 participant 变更触发 29 次无意义的本地 map remove——这是可以接受的吗？对于 participant 和 room_member 的变更频率（相对低频），可以接受。

**预期的架构变更**

```
┌─────────────────────────────────────────────────────┐
│                 Current Architecture                 │
│                                                     │
│  write_path() → invalidate_local_cache()            │
│               → (no broadcast)                      │
│                                                     │
│  read_path()  → cache.get() → miss → Redis/DB       │
│                  stale until TTL or local invalidate │
└─────────────────────────────────────────────────────┘
                        ↓
┌─────────────────────────────────────────────────────┐
│                 Proposed Architecture                │
│                                                     │
│  write_path() → invalidate_local_cache()             │
│               → nats.publish("cache.inv.{name}",{id})│
│                                                     │
│  run_cache_listener() ← nats.subscribe("cache.inv.*")│
│               → self.map.remove(id)                  │
│                                                     │
│  add cache listener to boot/background.rs            │
│  as a short-lived consumer (ephemeral, same as live) │
└─────────────────────────────────────────────────────┘
```

具体设计要点：

- **Subject 命名**：`cache.invalidate.participant.{pid}`、`cache.invalidate.room.{rid}`、`cache.invalidate.ai_answer.{rid}`。使用通配符订阅 `cache.invalidate.*`，按 subject 尾部解析。
- **Consumer 类型**：ephemeral（加入集群的实例应尽快收到失效，重启丢几条没关系）
- **Payload**：仅包含 ID（`{pid}`、`{rid}`），map remove 不需要值内容
- **序列化**：简单 string payload，避免引入 protobuf 等新依赖
- **上线策略**：破坏性变更—新代码发布的实例开始广播失效，旧实例不接收。这不会导致不正确，只是旧实例继续 stale 直到重启。属于安全可回滚。

**对现有系统的影响**

- 新增 `aero-bus` 或 `aero-server` 中的一个小 NATS consumer（~60 行代码）
- `participant_cache.invalidate` 和 `room_member_cache.invalidate` 扩展为：1) 本地 remove 2) NATS publish
- boot 流程新增一个 `tokio::spawn`（参考 `run_live_bus_listener` 模式）
- 影响范围：局部，不影响现有任何路由或其他智能体

**备选方案：Redis Keyspace Notification**

不使用 NATS 而使用 Redis 的 `__keyspace@*__` 通知，当 Redis 键被 SET/DEL 时得到通知。但 Redis keyspace notification 是 fire-and-forget（不持久化），且无法精确控制通知到「需要失效的实例」——所有订阅的客户端都收到。优势是无需额外 NATS subject 管理。权衡后，使用 NATS 更一致（不引入新机制），且可以利用 JetStream 的持久化保证（如果选择 durable consumer）。

### 方向 B：AI 缓存原子化与 TTL 自动驱逐（P2）

**为什么需要**

当前 AI 缓存的实现是 `SMEMBERS` + `DEL` + `SADD`，非原子操作，见方向 A 的竞态分析。此外，当前没有 TTL 驱逐机制（`EXPIRE` 是对整个 key 设过期，但对 `SADD` 后的 set 成员不单独设过期）。这意味着：

1. 缓存项会无限增长（占 Redis 内存）——除非每次写都覆盖全量
2. 过时答案永久停留

**核心挑战**

- 替换为 `Lua script`（`EVAL` 或 `SCRIPT LOAD`）实现原子 `DELETE + SADD`（或 `DEL + SET` 改用 Set 为单个字段）
- 或者使用 Redis 6.2+ 的 `SDEL`... 不，没有这个命令。可以使用 `MULTI/EXEC` 事务包裹，但不保证原子性（Redis 事务在执行期间可以穿插其他客户端命令）。使用 Lua 脚本是正确方案。
- TTL 策略：每次访问刷新 TTL（`EXPIRE`）或使用固定 TTL（写入时设置）
- fred（Rust Redis 客户端）支持 Lua 脚本注册和调用

**预期变更**

将 `cache_answer_invalidate_room` 和对应的读路径改为调用一个 `script load` 加载的 Lua 脚本：

```lua
-- redis.compute_cache_invalidate
local key = KEYS[1]
local member = ARGV[1]
redis.call('DEL', key)
redis.call('SADD', key, member)
redis.call('EXPIRE', key, 3600)
return 1
```

**影响**：局部变更，限于 `aero-storage` 或 `aero-ai` 存储相关模块。本质上是一个 bugfix level 的改动，但需要增加 fred 的 Lua 调用测试。

### 方向 C：BFF 层——条件收敛（P3）

**为什么需要**

当前无 BFF 的架构虽在早期运作良好，但随着 Web SPA 承载越来越多能力（直播互动、AI 流式聊天、通话字幕、polls、threads），前端对后端数据合约的耦合正逐步演化成风险：

- **字段重命名**：后端 `kind` → 前端 `kind`，但 `CallEvent` 用了 `call_kind`（因为 serde 字段冲突），前端需要 `event.call_kind || event.kind` 的 fallback 逻辑——这是合约泄漏的表现
- **over-fetching**：`RoomEvent` 的消息体包含完整 `Block` 向量，但前端可能只需要 `text` 和 `sender.name`
- **多端逻辑重复**：搜索结果的 `merge_hits` 融合逻辑如果又在前端复制一遍，维护成本翻倍

然而，**现在引入 BFF 的时机过早**。直接的 BFF（一个专用于前端的中间层服务）将：

- 增加部署拓扑复杂性
- 引入序列化/反序列化开销
- 增加延迟（哪怕只是轻微增加）
- 需要维护一个独立的 crate

**一个更克制的方案：条件收敛（Conditional Convergence）**

不增加新服务，而是在现有 Axum 路由层引入**响应适配器**：

```
当前：
  POST /api/rooms/:id/search
  → handler 调用 repo 返回 Vec<SearchHit>
  → serde_json::to_string_pretty 序列化
  → Content-Type: application/json

方案：
  POST /api/rooms/:id/search    ← Accept: application/json    现有行为
  POST /api/rooms/:id/search    ← Accept: application/vnd.aero.v1+json  适配版
  
  handler 检查 Accept 头：
    JSON → 现有行为
    vnd.aero.v1 → 调用 .to_frontend_response() 适配层
```

**核心挑战**：在不引入新 crate 的情况下定义一个可测试的适配层。不能无限膨胀路由 handler。

**建议的妥协**：

- 不为整个 API 套适配，只在高频 API（搜索结果、room 列表、message 列表）提供
- 适配以 `.into_frontend()` method 的形式放在 domain type 旁边，不单独建适配层 crate
- 前端先发 `Accept: application/json`，逐步迁移到 `vnd.aero.v1` 后稳定合约
- 这本质上是**版本化的 API 表示层**，但不是完整的 BFF

**当需要真正的 BFF 时**：当出现以下任一情况时，应考虑独立 BFF crate：
- 移动端（Android/iOS）加入，与 Web 端对数据形状的需求显著不一致
- 第三方开发者开始使用 API（需要严格契约、版本化、SDK 生成）
- 前端团队和后端团队分离运作（需要独立部署节奏）

此时 BFF 的额外复杂度才是合理的投资回报。

### 方向 D：文本输入进化——Markdown 撰写层（P2）

**为什么需要**

代码验证发现 composer 是 `<textarea>` 而非 `<div contenteditable>`。这是一个**纯收益**的安全决策，但为此付出的代价是：

1. 无内联格式（**粗体**、*斜体*、~~删除线~~）
2. 无 @提及的视觉提示
3. 无预览/分屏模式
4. 草稿（`Phase D`）的后端就位但前端缺少 sessionStorage 暂存

注意：不是要求富文本编辑器（那会引入巨大的 XSS 面和安全事件），而是要求在 textarea 之上叠加一个 **Markdown 撰写助手层**——类似于 Slack 和 Discord 的模式。

**核心挑战**

1. **选择 Markdown 撰写库**：需要零依赖且安全。现有方案：
   - **SimpleMDE**（已弃用）
   - **EasyMDE**（fork 持续维护？）
   - **自定义实现**：仅支持 `**bold**`、`*italic*`、`~~strikethrough~~`、`` `code` ``、`> blockquote` 几个基本 pattern，用 `input` 事件的实时渲染 + 工具栏按钮插入 Markdown 标记。
   - **marked.js**（CDN，零依赖，~10KB gzipped）用于预览渲染

   推荐：**自定义实现**（如果只需要基本格式）+ `marked.js` 用于预览。因为：
   - 自定义实现直接使用 DOM API，无安全依赖
   - 工具栏按钮 = 在光标位置插入 `**text**` 模式，不改内容的安全模型
   - 预览使用 `marked.js` 并设置 `sanitize: true`

2. **@提及交互**：需要解决「触发 @ → 下拉列表 → 选择 → 插入成员 ID」的交互流程。后端接口（`GET /api/rooms/:id/members`）已存在。

3. **草稿暂存**：frontend `sessionStorage` + 现有后端 draft API。每次 `input` 事件 debounce 存入 `sessionStorage`；用户按 Enter 发送后清空；后端 drafts 用于跨设备同步。

**对现有系统的影响**

- 仅影响 `web/` 目录：`composer.js`（或内联在 HTML 中的 script）
- 后端的 draft API 已就位（`draft.rs` + `drafts.rs`），零后端变更
- 安全风险降低（富文本 vs textarea + Markdown 预览）

### 方向 E：迁移体面性工程化（P3）

**为什么需要**

当前迁移管理存在两个运营痛点：

1. **编译期嵌入导致回滚困难**：如果 `migrations/0042_fix_bad_index.sql` 被后续三个迁移覆盖，那么部署旧版二进制时 `sqlx::migrate!("migrations/")` 运行所有迁移（包括新的），可能导致表结构不兼容。实际部署中，这意味着迁移必须是**向前兼容的**（可回滚的），但在编译期嵌入的模式下，你**无法选择只运行部分迁移**。

2. **无法运行时查询迁移状态**：`_sqlx_migrations` 表只有 apps 在启动时通过 `migrate!` 微框架检查。运营人员没有 `GET /health/migrations` 端点来查询当前数据库的迁移版本。

**建议方案：迁移体面性层**

- **方案 A（推荐）**：在 `aero-server` 的 health 端点添加迁移状态查询：

  ```
  GET /health/migrations → {
    "current": "0042_fix_bad_index",
    "pending": ["0043_add_index", "0044_new_table"],
    "applied_count": 42,
    "status": "up_to_date" | "behind" | "ahead"
  }
  ```

  这只需要在 boot 时调用 `migrator.applied_migrations()` 和 `migrator.pending_migrations()`（sqlx 提供），然后注入 `AppState`。成本：~50 行代码。

- **方案 B（未来）**：将迁移从编译期嵌入改为运行时可配置：

  ```toml
  [server]
  migration_source = "file://migrations"  # or "embed://" for current behavior
  ```

  但这是**基础设施级的重构**，需要修改 `aero-storage/db.rs` 的 `migrate()` 签名。不值得在现阶段做——除非出现无法用向前兼容迁移解决的问题。

- **方案 C（最小化）**：在 README 或 ops runbook 中明确声明**迁移回滚策略**：每次部署前先 `aero-cli migrate down`（如果支持），或约定「迁移必须向前兼容 2 个版本」。对于当前项目规模，这可能是最实际的做法。

**对现有系统的影响**

方案 A 的影响：1 个新路由 + `AppState` 新字段（`migration_info`）——很小。

## 3. 接口设计建议

### 3.1 缓存失效总线接口

如果方向 A 被采纳，建议的接口设计：

```rust
// aero-bus 或 aero-server 内部
trait CacheInvalidationBus: Send + Sync {
    /// 广播一个缓存键失效。调用方应先执行本地失效。
    async fn broadcast_invalidation(&self, cache_group: CacheGroup, key: &str);
}

enum CacheGroup {
    Participant,
    Room,
    AiAnswer,
}

// 消费者端：
async fn run_cache_invalidation_listener(
    bus: Arc<dyn CacheInvalidationBus>,
    participant_cache: Arc<ParticipantCache>,
    room_cache: Arc<RoomMemberCache>,
    ai_cache: Arc<AiAnswerCache>,
    shutdown: CancellationToken,
) {
    // 订阅 cache.invalidate.*
    // 匹配 subject，调用对应 cache 的 invalidate
}
```

这个 trait 的设计原则：
- **窄接口**：只有广播方法，不暴露订阅实现（内部处理）
- **泛化代价低**：新增一个 cache group 只需要加一个 enum 变体 + match 臂

### 3.2 缓存抽象层的取舍

问题：是否应引入统一的 `Cache<K, V>` trait 来统管三个缓存？

**论证：不引入。**

三个缓存的差异大于共性：

| 维度 | participant_cache | room_member_cache | ai_answer_cache |
|------|------------------|-------------------|-----------------|
| 值类型 | `Option<Participant>` | `Arc<RwLock<HashSet<Uuid>>>` | `Redis Set` |
| 后端 | `HashMap` | `HashMap` | Redis |
| 失效粒度 | 单个 pid | 整个 room | 整个 room |
| 写频率 | 低（用户资料更新） | 低（角色变更） | 中（AI 生成） |
| 读取频率 | 高 | 中 | 低 |

强行抽象为统一 trait 会损失类型安全和各自优化空间。当前的「各自独立、模式相似」是合适的状态。唯一应共享的是**失效广播机制**（方向 A），但这应该是单独的总线组件，不是缓存 trait。

### 3.3 Web 前端 API 契约的稳定化建议

当前 web SPA 直接消费 `ServerFrame` 这个 domain enum。随着能力增长，建议：

1. **为每个 frame 类型建立 TypeScript 类型定义**（`types.d.ts`）而不是靠 JSDoc
2. **添加 validation layer**（对 `event.kind` 和 `event.call_kind` 做运行时校验，而不用 `||` 做 fallback）
3. **考虑 API Extract**（提取一个 `web/frames.md` 文档，人工维护，作为后端和前端共同的参考点——类似 OpenAPI 但更轻量）

这些是组织级约定，不是代码变更，但能显著减少跨团队沟通成本。

## 4. 技术选型

### 4.1 不需要引入的新技术

基于验证结果和架构分析，以下**不需要**引入：

| 技术 | 原因 |
|------|------|
| Redis pub/sub | 已经可以通过 NATS 实现缓存失效广播，保持技术栈收敛 |
| Memcached/其他缓存层 | 已有进程内缓存 + Redis 的组合，不要引入第三层 |
| protobuf/FlatBuffers | 现有的 JSON + serde 对当前规模足够，引入序列化框架增加 deploy 复杂度 |
| API Gateway | 现有 Axum 足够；没有微服务化的理由 |
| GraphQL | 解决的是「多端数据需求不同」的问题。在无 BFF + 单 SPA 的情况下，引入 GraphQL 的 schema 管理成本 > 收益 |
| Kafka | NATS JetStream 足够；除非需要严格的分区顺序消费和长期日志保留，否则不要换 |
| WebSocket 子协议 | 现有 JSON 帧加 `kind` tag 已够；引入 sub-protocol 增加跨语言互操作成本 |

### 4.2 可能需要的第三方依赖评估

| 依赖 | 用途 | 评估 |
|------|------|------|
| `fred` Lua script | AI 缓存原子操作 | 已有 fred 依赖；增加的 Lua 调用方式是 fred 的原生能力。无额外依赖引入。 |
| `marked.js` | Markdown 预览渲染（前端） | 已用 CDN 模式。`sanitize: true` 是必须配置的。零 webpack/vite 依赖。 |
| `highlight.js` (可选) | 代码块语法高亮 | 仅当 Markdown 预览包含代码块时。不强制，延迟加载。 |
| `rxdb` / `durable objects` | 离线优先 | 当前项目是联机优先架构，不需要离线优先的本地数据库。不要引入。 |

### 4.3 自建 vs 采购

| 能力 | 决策 | 理由 |
|------|------|------|
| 缓存失效广播 | **自建**（~60 行 NATS 集成） | 业务逻辑极为简单，不值得采购。现有 NATS 基础设施可用。 |
| Markdown 编辑器 | **自建**（textarea + 工具栏插件） | 不需要完整的富文本编辑器。自定义实现约 200 行 JS，避免引入 CVE。 |
| 迁移管理 | **自建**（health 端点） | sqlx 已提供迁移 API，只需要加一个 HTTP 端点暴露。 |
| AI 内容审核 | **采购**（Anthropic） | 已有 `AiService` 集成 Anthropic，审核功能应复用此通路。不引入新的审核供应商。 |
| 推送网关 | **自建**（`aero-push`） | 已存在。FCM/APNs 调用是典型的服务端出站 HTTP 调用，不复杂。 |

## 5. 实施路线图

### 优先级矩阵

| 方向 | 业务价值 | 技术必要性 | 改动规模 | 优先级 |
|------|---------|-----------|---------|--------|
| A: 缓存失效总线 | 中（多实例一致性） | 高（架构债务） | S（~60 行 + boot 接线） | **P1** |
| B: AI 缓存原子化 | 低（非安全关键） | 中（竞态） | XS（Lua 脚本） | **P2** |
| D: Markdown 撰写层 | 高（用户体验提升最直接） | 低（纯前端） | M（~300 行 JS + HTML） | **P2** |
| E: 迁移体面性 | 中（运营效率） | 低 | S（~50 行 + 路由） | **P3** |
| C: BFF 条件收敛 | 中（长期可维护性） | 低 | L（API 适配层） | **P3** |

### 阶段划分

**Phase 1（~1 周）：方向 A + E（最小化改动）**

```
Week 1:
├── Day 1-2: 缓存失效总线设计 + 实现
│   ├── 定义 CacheInvalidationBus trait
│   ├── 实现 NATS 广播端（invalidate 扩展）
│   ├── 实现 NATS 消费者（run_cache_invalidation_listener）
│   └── boot 接线
├── Day 3:  单实例测试（功能不变）+ 多实例验证
│   └── 验证：Instance A 修改 participant → 30s 内 Instance B 看到新值
├── Day 4:  迁移体面性端点
│   ├── AppState 新增 migration_info
│   ├── GET /health/migrations 路由
│   └── 文档更新
└── Day 5:  压力测试 + CI 测试
    └── 缓存失效 NATS consumer 的 ephemeral 重连行为测试
```

**变更清单**：
- `aero-server/src/cache_bus.rs`（新文件）
- `aero-server/src/bin/boot/background.rs`（加 spawn）
- `aero-storage/src/cache.rs`（扩展 `invalidate` 签名）
- `aero-server/src/health.rs`（加路由）

**风险**：缓存失效是**最终一致**的（at-least-once + ephemeral 消费者），意味着实例崩溃后重启会错过失效。这是故意的——重启后所有缓存为空，自然从 DB 重新加载，所以不会永远 stale。但重启期间发生的失效会丢失。这是一个接受的风险（liveness vs consistency 的权衡）。

**缓解**：考虑给缓存 TTL（现无），设为较短时间（如 5 分钟）作为第二道防线。如果已经在设计中使用 TTL，方向 A 的优先级可以降到 P2。

**Phase 2（~2 周）：方向 D（前端 Markdown）**

```
Week 2-3:
├── Day 1-2:  设计 composer 扩展 API
│   ├── 工具栏按钮位置和样式（不破坏现有布局）
│   ├── @mention 交互设计（dropdown 位置/数据源）
│   └── 草稿暂存的 sessionStorage key 命名
├── Day 3-5:  实现
│   ├── 工具栏按钮（B/I/S/Code/Quote）
│   ├── @mention 组件（输入 @ → 请求 → 下拉 → 选择 → 插入 user id）
│   ├── 草稿暂存（input → debounce → sessionStorage）
│   ├── 预览模式（tab 切换）
│   └── 现有 draft API 前端对接
├── Day 6-7:  测试
│   ├── 浏览器兼容测试（Chrome/Firefox/Safari）
│   ├── 无障碍测试（aria-label, keyboard nav）
│   └── 安全测试（输入 sanitization）
```

**变更清单**：仅 `web/` 目录
- `web/composer.js`（新文件，或合并到现有 HTML script 块）
- `web/index.html`（工具栏 HTML + 样式）
- 无后端变更（draft API 已存在）

**Phase 3（P3 方向，持续 1-2 周，可并行）**

- **方向 B（AI 缓存原子化）**：1-2 天
  - 创建 Lua 脚本 `cache_invalidate.lua`，注册到 fred
  - 替换现有 `SMEMBERS+DEL` 调用
  - 增加 `EXPIRE` 命令
  
- **方向 C（BFF 条件收敛）**：这是一个**持续的策略方向**，不需要一次性完成。建议：
  - 在每个新 API 中加入 `into_frontend()` method（如果前端数据形状与 domain 不一致）
  - 不主动重构现有 API，除非出现合约摩擦

### 关键风险矩阵

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| 缓存失效总线引入 NATS 消息风暴（高频写场景） | 低 | 中 | 参与者变更频率低；增加 debounce（同一 pid 的多次失效合并为一条） |
| Markdown 预览 XSS | 中 | 高 | `marked.js` + `sanitize: true` + DOMPurify 双重 sanitize；禁止 `dangerouslySetInnerHTML` |
| draft API 前端实现导致过期草稿堆积 | 中 | 低 | sessionStorage 天然隔离（浏览器关闭即清除）；后端定期清理未完成的 draft（`updated_at < NOW() - INTERVAL '30 days'`） |
| BFF 适配层变为「两层 serializer」 | 中 | 高（维护负担） | 严控适配范围；仅对跨端高频 API 做适配；拒绝「为了未来」的泛化 |
| 迁移 health 端点暴露迁移版本号（安全） | 低 | 低 | 置于 `/health`（现有健康检查端点），不暴露敏感表结构信息 |

### 不做的声明

以下方向在本次架构分析中明确**不推荐**：

- **GraphQL API**：成本和复杂度远超当前收益
- **离线优先（Offline-first）**：与当前联机优先架构矛盾，需要重写大量前端状态管理
- **WebSocket 子协议切换**：当前 JSON 帧 + `kind` tag 模式对单 SPA 来说足够灵活
- **全链路追踪（OpenTelemetry tracing）**：当前 OTEL 主要做 metrics 和 logging；加入 tracing 需要侵入式修改所有 async fn 签名。保留至性能问题出现时引入。

## 总结

Aero IM 的架构基础扎实——事件驱动骨架、清晰的 crate 边界、成熟的智能体架构、审慎的依赖管理，都反映了高水平的工程判断。本次分析识别的最大架构债务是**缓存一致性**（方向 A），虽然不直接威胁安全，但在多实例部署中产生不一致的用户体验，且在 AI 缓存中存在确定性竞态。

建议的路线图优先级为：**缓存失效总线（P1）→ Markdown 撰写层（P2）+ AI 缓存原子化（P2）→ 迁移体面性（P3）+ BFF 条件收敛（P3）**。

总代码增加量预计在 **400-600 行**（不包括测试），没有引入新的第三方依赖（marked.js CDN 不计入编译依赖），与现有架构无冲突。
