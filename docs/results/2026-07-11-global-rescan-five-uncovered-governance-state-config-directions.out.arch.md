以下是我基于 `2026-07-09-five-verified-architectural-gaps.md` 的完整架构分析。注意：此前 `docs/results/2026-07-09-five-verified-architectural-gaps.out.arch.md` 因连接错误仅 9 行报错信息，实际分析未完成。本回复是对该文档的完整架构分析。

---

# Aero IM — 五个已验证架构缺口的架构分析

> **分析范围**: 基于 `2026-07-09-five-verified-architectural-gaps.md` 的五个方向  
> **视角**: 资深架构师，关注架构债务、扩展性、运维可靠性、实施策略  
> **输出约定**: 仅架构分析与策略，不写具体代码

---

## 1. 架构评估

### 1.1 当前架构的优势

**事件驱动骨架的质量**是系统最突出的架构优势。NATS JetStream 作为跨实例事实源 + 进程内 `Hub` 扇出的双层模式，在以下方面表现出色：

- **水平扩展透明性**：`run_bus_listener`（durable consumer）保证每事件至少被消费一次，多节点仅增加扇出能力，不产生消息丢失或重复。
- **关注点分离**：每个 crate 有清晰的依赖方向（自下而上），无循环依赖。IM 核心逻辑、直播、AI、存储、信令之间通过事件总线解耦。
- **at-least-once 状态机的纪律执行**：`FOR UPDATE SKIP LOCKED` 在 AI worker 中的使用、幂等键在 ooo_bot 中的使用、dead-letter 队列在各处的应用——这是生产级系统的标志。
- **迁移编译期嵌入**：`sqlx::migrate!("../../migrations")` 将迁移在编译时烤入二进制，避免了运行时迁移文件缺失的问题——一个被许多项目忽略但极其重要的安全实践。

### 1.2 系统性的架构债务

五大缺口揭示了一个**共性模式**：系统在「横向扩展」（新增功能、新增 crate）上投入巨大且执行到位，但在以下**四个横切面维度**上系统性地欠债：

#### 维度 A：策略/规则的组合协调层缺失

方向一（治理仲裁器）是其中最典型的代表。系统在**同一调用路径上叠加了多个独立的策略评估点**（关键词、PII、Spam、AutoMod、AI 审核、信息隔离墙、法务保全、留存策略），但缺少一个将它们**组合为一个可审计的决策树**的机制。

这不是「少了一个类」的问题——这是一个**架构模式选择**：当前的选择是「管线内嵌式策略评估」（每个策略在 `send_message` 中顺序 if-let），而需要的选择是「仲裁器聚合式策略评估」。这个模式在多个上下文中重复出现：

| 上下文 | 当前模式 | 债务表现 |
|--------|---------|---------|
| 消息治理 | 内嵌 if-let 链 | 新策略必须改核心管线 |
| 直播治理 | 独立 StreamModRepo | 与 IM 治理策略不共享 |
| 编辑审核 | 无策略评估 | 绕过路径存在 |
| 通话合规 | 无任何评估 | 完全缺失 |

#### 维度 B：状态恢复的分布式认知缺失

方向二（WS 非消息事件）和方向四（跨节点状态协调）的共同根源是：**系统设计时假定了进程内状态的稳定性**，但生产环境中进程崩溃、网络抖动是常态。

具体而言：
- **客户端 side 的状态恢复假设**：`_lastSeen` 只追踪 `message.id`——这是一个客户端架构决策，假设非消息事件「不重要」或「可以通过全量拉取恢复」。实际上大多数非消息事件（编辑、删除、反应、置顶、成员变更）是**状态变更事件**，丢失后无法通过任何方式自然恢复。
- **服务端 side 的状态分布假设**：`Hub.room_online`、`SfuRouter`、`WhipRegistry` 的纯进程内存设计——假设进程永远不崩溃。

这两个假设在单节点部署时成立，但在多节点 + 故障常态的分布式环境中破裂。系统目前在功能上支持多节点（NATS fan-out），但在**状态管理上仍是单节点架构**。

#### 维度 C：横切面审计覆盖不完整

方向三（非消息数据生命周期）和方向五（配置生命周期）的共同根源是：**横切面关注点（数据治理、配置管理）的审计覆盖是零散的**。

配置的 6 种来源、表清理的 15 张覆盖 + 11 张遗漏、环境变量的单/双下划线分歧——所有这些都指向同一个事实：横切面关注点在功能开发浪潮中被逐层累积，没有被系统性管理和审计。

这是「早期创业节奏」的痕迹：先实现功能，再补齐治理。在 55+ 方向覆盖、87+ 功能模块的体量下，补齐横切面治理已从「nice to have」变为「must have」。

### 1.3 关键设计决策是否合理

| 决策 | 当时合理性 | 现在是否需要调整 |
|------|-----------|----------------|
| 治理策略内嵌于消息管线（`send_message` 中 if-let 链） | ✅ 功能数量少时可行 | ⚠️ 需要仲裁器抽象 |
| `_lastSeen` 仅追踪 message ULID | ✅ 快速实现 | ❌ 需要事件级游标 |
| Hub 状态为进程内 DashMap | ✅ 单节点高性能 | ⚠️ 需要 Redis 镜像 |
| 配置为多来源松散模式 | ✅ 快速迭代 | ❌ 需要统一 schema |
| retention sweep 以函数而非注册表模式 | ✅ 覆盖核心表 | ⚠️ 需要扫描 - 注册模式 |
| str0m SFU 会话为纯进程内存 | ✅ 正确（str0m 状态机不可迁移） | ✅ 依然正确（见下文分析） |

需要特别强调的是：**str0m 纯进程内存不是架构债务，而是正确的设计决策**。DTLS-SRTP 会话状态（密钥协商、加密上下文、序列号空间）是进程绑定的，无法像数据库行那样跨节点迁移。方向四中方案 B 的「SFU 会话接管」应当定位为「快速通知客户端重连」而非「热迁移」，这一点源文档已明确指出。

---

## 2. 扩展方向

### 方向一（高价值）· 横切面策略引擎——从治理仲裁器到通用策略管道

#### 为什么需要

治理仲裁器不应仅限于内容治理。系统的多个子系统都有「注册策略 → 评估 → 聚合裁决 → 审计」的模式：

- 内容治理（当前讨论）
- 限流/配额（`RateLimiter`、`SpamGuard`、`CostBudget`、每租户配额）
- 授权/访问控制（`assert_room_access`、`member_role`、`InfoBarrier`）
- 消息发送前校验（关键词、PII、block 校验）
- 异步执行后审计（AI 审核、webhook）
- 定时清理策略（retention、legal hold）

一个泛化的 `PolicyEngine` 可以将这些散落的策略评估统一为一个可审计、可组合、可观测的管道。

#### 核心挑战

1. **同步 vs 异步策略的时序**：同步策略（关键词）在消息发送前执行，异步策略（AI 审核）在发送后执行。它们之间的关系（异步能否覆盖同步的决定？异步能推翻同步的允许吗？）需要显式的组合规则。
2. **性能与事务边界**：仲裁器应在事务外执行（避免长事务），但 `soft_delete_moderated` 和 `audit_log` 的行级一致性需要事务保护。策略执行 + 审计的原子性是一个棘手的问题。
3. **策略优先级与互斥的声明式描述**：源文档已识别 `overrides` / `conflicts_with` 的需求。设计时需要决定：这些关系是代码级的（trait 方法）还是配置级的（基于策略名称的声明式 DSL）？

#### 预期的架构变更

```
当前：send_message → [keyword_check, pii_check, spam_check, auto_mod_check, ...]
                                                          ↓
目标：send_message → PolicyEngine::evaluate(msg, context)
                       ├─ 同步策略（keyword, pii, spam, automod, infobarrier, costbudget）
                       ├─ 并行执行（所有同步策略可同时运行）
                       ├─ 聚合裁决（Block/Allow/Warn 优先级排序）
                       ├─ 审计日志（统一 audit_event 行）
                       └─ 返回裁决结果
```

#### 对现有系统的影响

- **适度重构**：现有 `send_message` 中的 if-let 链需提取为独立 Policy 实现
- **迁移路径**：可以按策略逐一提取，不要求一次完成
- **向后兼容**：仲裁器输出与当前 `send_message` 返回类型应兼容（`Block(reason)` → 不需调用者改动）

### 方向二（高价值）· 分布式状态镜像层——Hub 状态的轻量级 Redis 回写

#### 为什么需要

方向四（跨节点状态协调）的本质是：**进程本地状态在分布式环境中的可用性问题**。解决方案不必是全自动故障转移（这在媒体会话中不可行），而是**让状态从「进程私有」变为「进程缓存 + Redis 权威」**。

这不仅解决节点故障恢复问题，还解决了：
- 在线状态跨节点一致性（不同节点上的用户看到相同的在线列表）
- 观众计数跨节点准确性（计数从 Redis 聚合，而非从 `Hub.stream_watchers` 的本地快照）
- 运维可见性（运维工具可以直接读 Redis 了解系统状态）

#### 核心挑战

1. **一致性与性能的权衡**：每状态变更都写 Redis 增加延迟。关键是找到哪些状态需要写 Redis、哪些可以仅进程本地。
   - **必须写**：`call_rosters`、`stream_watchers`（计数跨节点）、`room_online`（在线状态跨节点可见）
   - **可本地**：`Hub.conns`（WS 连接是进程绑定的）、`WsSender`（不可跨节点迁移）
2. **Redis 故障退化**：当 Redis 不可用时，系统应从「Redis 镜像」退化到「纯进程本地」模式，不能因为 Redis 故障导致在线状态变空白。
3. **TTL 一致性**：`room_online` 的 Redis set 和 `PresenceStore` 的 zset 共享相同的 TTL 策略，但两者是独立的数据结构——需要确保一个更新时另一个也更新。

#### 预期的架构变更

```
当前：
  Hub.room_online (DashMap) ← 用户加入/离开房间
  Hub.stream_watchers (DashMap) ← 用户 watch/unwatch 流
  CallRosterStore (Redis) ← 独立维护

目标：
  Hub.room_online (DashMap, 缓存层) ──异步写──→ Redis SADD "room:{id}:online" + EXPIRE
                                                ↑
                                        读取前检查 DashMap→Miss→Redis→回填 DashMap
  
  Hub.stream_watchers + Redis SMEMBERS 联合：观众数量从 Redis SCARD 读取
```

#### 对现有系统的影响

- **影响面广但每个改动小**：`join_room`、`leave_room`、`watch_stream`、`unwatch_stream` 各加一条 Redis 写操作
- **无 API 变更**：纯内部实现变化
- **无数据迁移**：Redis set 是临时数据，无需迁移现有数据

### 方向三（高价值）· 事件游标式重连恢复——从 `_lastSeen` 到 `?cursor`

#### 为什么需要

方向二（WS 非消息事件交付保证）的根源不是客户端代码量的不足，而是**架构级的设计假设**：`_lastSeen = message.id` 假设只有消息需要恢复。在系统早期只有消息时这个假设成立，但现在系统有 14+ 种事件类型，这个假设已全面崩溃。

从 `_lastSeen` 到 `?cursor` 的迁移是 IM 系统从「消息中心」到「事件驱动」的架构升级信号——消息只是事件的一种。

#### 核心挑战

1. **快照 + 增量模式**：长时间离线后，回放 10 万 + 条事件在客户端是灾难性的。需要类似 canvas checkpoint 的机制：服务端在某个 seq 处生成房间状态的快照（当前成员、置顶、通话状态、投票状态），客户端先加载快照，再增量应用快照之后的 seq。
2. **SeqGate 与缺口检测**：当前 `SeqGate` 维护已应用 seq 的位图，但没有「预期但未到达」的检测机制。需要 per-scope 心跳 seq（每 30s 发一个空 seq 帧）让客户端感知到有缺口。
3. **与 `?since=` 向后兼容**：旧客户端仍使用 `?since=`（ULID），新客户端使用 `?cursor=`（seq）。服务端需同时支持两个参数，并在 WS 连接建立时通过 capability negotiation 汇报各自能力。

#### 预期的架构变更

```
客户端：
  _lastSeen (message ULID) → _cursor (per-room seq)
  重连：ws://host/ws?cursor={seq}
  
服务端：
  新增 POST /api/rooms/:id/snapshot?after_seq={seq}
  → 返回 { snapshot: { members, pins, calls, polls }, since_seq }
  
WS 协议扩展：
  新增帧类型 "heartbeat_seq" { scope, seq }
  每 30s 由服务端发送，客户端更新本地水位线
```

#### 对现有系统的影响

- **中度变更**：客户端 JS 需要新的状态管理和帧处理逻辑
- **向后兼容**：`?cursor=` 是新参数，旧客户端继续用 `?since=`
- **服务端新增**：快照生成 + 事件回放端点
- **无数据迁移**：纯运行时变更

### 方向四（中价值）· 配置工具链——从杂乱来源到声明式生命周期

#### 为什么需要

方向五（配置生命周期管理）识别了 6 种配置来源、零校验、无热重载、敏感值泄漏等问题。从架构角度看，配置管理系统是**运维契约的基础设施**——没有好的配置管理，就无法可靠地部署、升级、调试系统。

当前系统已有 `figment` 作为配置框架，但问题是使用方式过于松散：部分配置通过 `figment` 管理（有结构、有类型）、部分通过 `env_parse()` 手动解析（无结构、无文档）。这不是 figment 的问题，是用例未统一的问题。

#### 核心挑战

1. **渐进迁移**：一次性将所有配置迁移到统一 schema 不可行。需要让新配置使用统一入口，旧配置逐个迁移。
2. **热重载的安全边界**：不是所有配置都能热重载。需要明确的声明式机制区分哪些配置项可热重载（KeywordModerator、rate limit、日志级别）和哪些需要重启（数据库连接、feature gates）。
3. **敏感值标注**：Rust 的类型系统可以很好地支持 `Sensitive<T>` 包装器（`Display` 输出 `****`），但需要确保所有配置消费者使用这个类型——而不是 `String`。

#### 预期的架构变更

```
当前：
  env::var("AERO_BLOCKED_WORDS") → String
  env_parse("AERO_HTTP_TIMEOUT_SECS") → Option<u64>
  config.toml → figment → AppConfig
  from_env() → WsConfig

目标：
  ConfigRegistry (启动时注册)
  ├─ register("aero.blocked_words", Type::String, Some(""), Sensitive(false))
  ├─ register("aero.http.timeout_secs", Type::U64, Some(30), Sensitive(false))
  ├─ register("aero.internal.bridge_secret", Type::String, None, Sensitive(true))
  │
  ├─ resolve() → ConfigSnapshot (Arc<RwLock<...>>)
  │
  ├─ 启动校验：所有注册项 → parse → 失败 warn + 默认值
  ├─ 热重载：POST /-/reload → 仅 reload 标记为 hot_reloadable 的项
  ├─ 自省：GET /-/config-schema → 所有注册项+当前值（敏感值脱敏）
  └─ 废弃处理：register 时标记 deprecated → 命中时 warn
```

#### 对现有系统的影响

- **低破坏性**：不改变现有配置读取方式，只需在每个读取点加注册
- **逐步迁移**：可从最关键的配置项开始（敏感值、限流参数、热重载目标）
- **零停机**：配置 schema 自省和热重载是增量功能

### 方向五（中价值）· 数据生命周期统一注册表

#### 为什么需要

方向三（非消息数据生命周期管理）识别了 11 张遗漏表。但根本问题不是遗漏了多少张表，而是**没有系统性的机制确保「新表加入时不会遗忘生命周期管理」**。

当前 `retention.rs` 中每个 sweep 函数是手动编写的 —— 新加一张表时，开发人员必须记住去 `retention.rs` 加一个新的 `sweep_*` 函数并在 `run_sweep` 中调度它。这个模式在 15 张表时已经吃力，在 30 张表时必然出遗漏。

需要一个**声明式的数据生命周期注册表**，让开发人员在定义迁移时就能同时声明清理策略。

#### 核心挑战

1. **声明式 vs 过程式**：清理策略有时无法用纯 SQL 表达（如 canvas ops 需要先检查是否有活跃编辑、法务保全需要 JOIN），需要 hooks/回调。
2. **批量操作的限流**：一次性 `DELETE` 百万行会锁表、膨胀 WAL、拖累复制延迟。所有 sweep 操作应分批执行（`LIMIT 10000` + 循环）。
3. **法务保全的交叉过滤**：多张表（messages, call_transcripts, canvas_ops）都需要跳法务保全标记的行。这个过滤不应在每个 sweep 函数中重复——应作为 sweep 框架的通用层。

#### 预期的架构变更

```
当前：
  retention.rs 中手动编写每个 sweep_* 函数
  run_sweep() 显式调用每个函数

目标：
  RetentionRegistry
  ├─ register(RetentionPolicy {
  │     table: "canvas_ops",
  │     condition: "id < (SELECT max(id) FROM canvas_version WHERE ...)",
  │     batch_size: 10000,
  │     skip_legal_hold: true,  ← 自动 JOIN legal_holds
  │     gdpr_relevant: true,    ← 自动加入删除列表
  │     ttl: None,              ← SQL 表达式驱动而非 TTL
  │   })
  │
  └─ sweep_all() 遍历所有注册项
```

#### 对现有系统的影响

- **纯新增**：不改变现有 sweep 函数
- **新表强制执行**：迁移模板中加入 `retention.register()` 步骤
- **旧表可逐步迁移**：现有 sweep 函数可以逐个包装为 `RetentionPolicy`

---

## 3. 接口设计建议

### 3.1 策略评估接口的原则

治理仲裁器和通用策略引擎的核心接口需满足以下原则：

#### 原则 1：输入 = 事件 + 上下文，输出 = 裁决向量

```
trait Policy: Send + Sync {
    fn name(&self) -> &'static str;
    fn phase(&self) -> Phase;               // PreSend | PostSend | ReadTime
    fn evaluate(&self, event: &Event, ctx: &Context) -> Result<Vec<Verdict>>;
}

enum Verdict {
    Allow { reason: Option<String> },
    Block { reason: String, severity: Severity },
    Warn { message: String },
    Skip,                                    // 策略不适用
}
```

`Verdict` 是一个向量而不是单一值——一条消息可能同时被关键词屏蔽（Block）和 PII 检测（Warn）。仲裁器聚合多个策略的裁决后，根据优先级确定最终行为。

#### 原则 2：策略依赖显式，不在评估内发起 IO

```
trait PolicyDependencies {
    fn required_repos(&self) -> Vec<&'static str>;  // 声明需要的仓储
}
// 每个策略所需的仓储在启动时注入，不在 evaluate() 内获取
```

这确保 `evaluate()` 是纯同步操作（或仅有已知的有限异步），不因 IO 抖动导致消息发送阻塞。需要异步的重 IO 策略（AI 审核）注册为 `PostSend` 阶段。

#### 原则 3：审计记录由框架生成，不由策略产生

```
// 仲裁器在聚合裁决后自动生成审计记录
struct AuditRecord {
    event_id: Uuid,
    policies_evaluated: Vec<PolicyResult>,
    final_verdict: FinalVerdict,
    arbiter_version: u32,  // 策略组合的版本号
}

// 策略不直接写审计日志
```

#### 原则 4：仲裁器不应成为瓶颈——策略评估应并发

```
// Arbiter::evaluate 内部：
// 1. 收集所有 PreSend 策略
// 2. 用 tokio::join_all 并行执行（各策略 &self 只读）
// 3. 聚合裁决
// 4. 写审计（非阻塞）
```

### 3.2 事件游标接口的兼容性设计

从 `?since=`（消息 ULID）迁移到 `?cursor=`（per-scope seq）需要前后兼容的接口设计。建议：

```
// 服务端 WS 升级请求处理：
enum ReconnectMode {
    Legacy(since_ulid: Ulid),    // ?since=01ARZ3NDEKTSV4RRFFQ69G5FAV
    Cursor {
        cursor: u64,             // ?cursor=1042
        scopes: Vec<Scope>,      // ?scope=room:abc123&scope=room:def456
    },
}

// 服务端响应 WS 帧新增：
{
   "type": "reconnect_snapshot",
   "cursor": 1042,
   "snapshot": {
       "pinned_messages": [...],
       "active_calls": [...],
       "poll_states": {...},
       "member_count": 12,
   },
   "since_seq": 1043,          // 从此 seq 开始增量回放
}
```

### 3.3 配置系统接口——隔离关注点

```
// 核心配置 trait：读取 + 校验 + 热重载感知
pub trait ConfigItem: Send + Sync {
    fn key(&self) -> &'static str;
    fn value_type(&self) -> ValueType;
    fn default_value(&self) -> Option<&'static str>;
    fn description(&self) -> &'static str;
    fn is_sensitive(&self) -> bool;
    fn is_hot_reloadable(&self) -> bool;
    fn validate(&self, raw: &str) -> Result<(), ConfigError>;
    fn parse(&self, raw: &str) -> Result<ConfigValue, ConfigError>;
}

// ConfigStore 管理生命周期
pub struct ConfigStore {
    items: HashMap<&'static str, Box<dyn ConfigItem>>,
    values: Arc<RwLock<HashMap<&'static str, ConfigValue>>>,
}

impl ConfigStore {
    pub fn register(&mut self, item: Box<dyn ConfigItem>);
    pub fn load(&self) -> Result<(), Vec<ConfigError>>; // 启动加载
    pub fn hot_reload(&self) -> HotReloadResult;         // 只 reload 标记项
    pub fn schema(&self) -> Vec<ConfigItemSchema>;       // GET /-/config-schema
}
```

### 3.4 保持向后兼容的策略

所有新增接口都应遵循「三个版本窗口」模式：

1. **V1（当前）**：一切照旧，无变化
2. **V2（过渡）**：新增接口 + 旧接口仍可用，但旧接口用法会触发 `warn!()` 日志
3. **V3（清理）**：移除旧接口（或至少不再提供文档说明）

具体到此分析的五个方向：

| 方向 | V1 → V2 过渡策略 | V2 → V3 清理时机 |
|------|-----------------|-----------------|
| 治理仲裁器 | 仲裁器新增 + 旧 if-let 链保留，新策略通过仲裁器注册，旧策略维持原状 | 所有策略迁移完成后的下一个 major 版本 |
| WS 事件恢复 | `?cursor=` 新增 + `?since=` 保留；服务端并行支持 | `?_since=` 标记为 deprecated，半年后移除 |
| Hub 状态镜像 | Redis 镜像新增 + DashMap 主路径保留；读请求先检查 DashMap → Miss → Redis | DashMap 降级为纯缓存层 |
| 配置工具链 | ConfigRegistry 新增 + `env_parse` 继续可用，但旧用法触发 warn | 配置项逐个迁移，声明式为主后移除手动解析 |
| 数据生命周期 | RetentionRegistry 新增 + 旧 sweep_* 函数继续；新表强制注册 | 旧函数封装为 RetentionPolicy 实例 |

---

## 4. 技术选型

### 4.1 不需要引入新的技术栈

上述五个方向中，**没有任何一个需要引入新的运行时基础组件**：

| 方向 | 所需技术 | 是否已在栈中 |
|------|---------|-------------|
| 治理仲裁器 | Rust trait + 审计表（Postgres） | ✅ |
| WS 事件恢复 | SeqGate 扩展 + REST 端点 | ✅ SeqGate 已实现，REST 端点可增量添加 |
| Hub 状态镜像 | Redis SET/SCARD | ✅ Redis 已在栈中（fred） |
| 配置工具链 | figment 扩展 + 配置表（Postgres 可选） | ✅ figment 已在栈中 |
| 数据生命周期 | SQL + Postgres 分区/清理 | ✅ |

**建议不使用额外的外部依赖**。以下依赖是应避免的：

- **配置中心（etcd / Consul / Spring Cloud Config）**：对于单进程部署模式，这些是过度工程。热重载通过 SIGHUP 或 HTTP endpoint 即可实现。
- **分布式协调（etcd / ZK）**：Redis zset 足够实现轻量级的节点心跳和存活检测（方向四方案 A）。引入 etcd 需要额外的运维栈。
- **消息队列缓冲区（Kafka / Pulsar）**：当前 NATS JetStream 已满足所有消息持久化需求。非消息事件的生命周期管理不需要额外的消息中间件。

### 4.2 唯一可考虑的第三方扩展：Postgres 分区管理

方向三（数据生命周期）建议中涉及大量 `DELETE` 操作，对 `canvas_ops`、`stream_gifts` 等高频写入的表，使用 `DELETE` 进行清理在大表上不可行——会产生大量 WAL 和死元组，导致 `VACUUM` 跟不上。

对于这些表，建议从「DELETE 按条件清理」逐步迁移到「分区 + DROP 分区」模式：

```
CREATE TABLE canvas_ops (...)
PARTITION BY RANGE (created_at);

-- 每月一个分区
CREATE TABLE canvas_ops_2026_07 PARTITION OF canvas_ops
  FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');

-- 清理：DROP TABLE canvas_ops_2026_01 CASCADE;
-- 而非 DELETE FROM canvas_ops WHERE created_at < '2026-02-01'
```

这不需要新的第三方依赖——Postgres 原生分区在 PG17 中已非常成熟且性能良好。需要的是 `pg_partman` 扩展或一个轻量级的定时任务（每月的第一个 cron 创建下月分区 + DROP 过期分区）。

### 4.3 自建 vs 采购的决策依据

所有五个方向都是**自建**场景：

| 方向 | 为什么不采购/用现成 | 自建复杂度 |
|------|-------------------|-----------|
| 治理仲裁器 | 策略引擎在 IM/协作领域高度定制（关键词+AI+法务保全的混合是特定领域的） | M（~2 周核心实现） |
| WS 事件恢复 | 无标准化的「IM 状态恢复协议」可用 | M（~1.5 周） |
| Hub 状态镜像 | Redis 操作 + DashMap 缓存的组合很轻量 | S（~1 周） |
| 配置工具链 | figment 已有，只差注册+校验+热重载封装 | S-M（~1 周核心） |
| 数据生命周期 | 纯 SQL + 定时任务，无现成库比 SQL 更好 | S（~3-5 天） |

### 4.4 测试策略建议

由于大部分变更在行为边界（策略组合、状态恢复、配置校验），建议优先使用**集成测试**而非单元测试：

```
# 策略仲裁器集成测试
#[sqlx::test]
async fn arbiter_keyword_wins_over_pii_when_both_block() {
    // 安排：注册 KeywordPolicy (priority=100) 和 PiiPolicy (priority=50)
    // 断言：Block 裁决来自 KeywordPolicy
}

# WS 游标恢复集成测试
#[sqlx::test]
async fn reconnect_with_cursor_restores_deleted_event() {
    // 安排：消息 A 已发送 → 消息 A 被删除（seq 1, 2）
    // 连接断开 → 重新连接 with cursor=1
    // 断言：收到 deleted 事件（seq 2）
}

# 配置校验集成测试
#[test]
fn config_with_invalid_type_logs_warning() {
    // 安排：AERO__SERVER__RETENTION_SWEEP_SECS=thirty
    // 断言：warn!() 日志输出，默认值被使用
}
```

---

## 5. 实施路线图

### 优先级排序和阶段划分

| 阶段 | 方向 | 预估成本 | 依赖条件 | 关键风险 |
|------|------|---------|---------|---------|
| **P0** — 尽快启动 | 方向一（治理仲裁器） | 2 周核心 + 1 周迁移 | 无 | 策略互斥的语义设计需谨慎 |
| | 方向二（WS 状态恢复方案 A） | 3-5 天 | 无 | 客户端 JS 更新需要发版 |
| **P1** — 本季度 | 方向三（数据生命周期） | 3-5 天 A 期 + 2 周 B 期 | 方向一仲裁器完成（可选） | canvas checkpoint 可能现有函数不兼容 |
| | 方向五（配置工具链） | 3-5 天 A 期 | 无 | 需决定配置表是否需 PG 持久化 |
| **P2** — 下季度 | 方向四（跨节点状态协调） | 1 周 A 期 + 2-3 周 B/C 期 | 依赖 P0/P1 优先 | SFU 会话不可迁移需管理用户预期 |
| | 方向二（WS 状态恢复方案 B/C） | 2 周（完整） | 依赖方向一？ | 快照生成对大房间的性能挑战 |

### 阶段 1（P0，1-2 周）：高安全优先级 — 治理 + 状态感知

#### 目标

消除「配置错误」和「治理绕过」引起的运行时安全事故。

#### 交付物

1. **GovernanceArbiter 核心**：
   - `Policy` trait 定义（pre_send / post_send 阶段）
   - 仲裁器实现（并发策略评估 + 优先级排序 + 统一审计）
   - `KeywordPolicy`、`PiiPolicy`、`SpamPolicy`、`AutoModPolicy` 四个同步策略提取
   - `InfoBarrierPolicy` 新增（补齐信息隔离墙在发送路径的缺口）
   - `EditPolicy` 新增（编辑路径过同步策略评估，跳过 AI 审核）

2. **WS 轻量恢复（方案 A）**：
   - `refreshRoomState(roomId)` 端点实现（置顶/反应/通话/投票）
   - 重连时客户端自动调用

#### 里程碑

- **Day 3**：`Policy` trait + 仲裁器核心实现，单策略 e2e 测试通过
- **Day 7**：5+ 策略迁移到仲裁器，编辑路径修复
- **Day 10**：客户端 `refreshRoomState` 实现 + 重连调用
- **Day 14**：冒烟测试 + 回归测试 + 文档

#### 风险与缓解

| 风险 | 可能性 | 影响 | 缓解 |
|------|-------|------|------|
| 策略互斥定义不完整导致线上误判 | 中 | 高 | 初始阶段所有策略独立评估，不启用互斥逻辑；互斥在 V2 引入 |
| `refreshRoomState` 在重连时造成请求风暴 | 中 | 中 | 加 200ms 随机延迟（jitter）+ 限流 |
| 编辑路径过策略误拦合规编辑 | 低 | 高 | 编辑路径跳过 auto_mod 和 spam 检查（仅过关键词+PII） |

### 阶段 2（P1，2-3 周）：横切面数据治理

#### 目标

消除表增长失控和 GDPR 合规残留。

#### 交付物

1. **数据生命周期 A 期**：
   - `RetentionRegistry` 框架（注册 + 分批执行 + 法务保全自动 JOIN）
   - 11 张遗漏表的 sweep 函数注册
   - GDPR `delete_participant_data` 扩展覆盖 `block_interactions` 等表

2. **配置工具链 A 期**：
   - `ConfigRegistry` 框架（注册 + 启动校验 + 敏感值脱敏）
   - 最关键的 20+ 配置项注册（敏感值优先、限流参数、feature gates）
   - `GET /-/config-schema` 端点
   - config 废弃检测（`AERO_WS_SEND_QUEUE_CAP` → 新名）

#### 里程碑

- **Day 3**：`RetentionRegistry` 框架 + 首批 4 张表（canvas_ops, message_history, block_interactions, stream_gifts）
- **Day 7**：剩余 7 张表 + GDPR 扩展
- **Day 10**：`ConfigRegistry` 框架 + `/-/config-schema`
- **Day 14**：配置废弃检测 + 20+ 项注册
- **Day 17**：集成测试 + 文档

#### 风险与缓解

| 风险 | 可能性 | 影响 | 缓解 |
|------|-------|------|------|
| `DELETE` 百万行造成 WAL 爆炸 | 高 | 高 | 所有 sweep 分批执行（`LIMIT 5000` + 循环，每次 COMMIT） |
| 配置废弃 warn 被运维团队忽视 | 中 | 中 | 废弃配置同时输出到响应头（`X-Deprecated-Config: AERO_RETENTION_SWEEP_SECS`） |
| canvas checkpoint 与活跃编辑的 race | 中 | 中 | checkpoint 前检查 `last_active_at` < now() - 60s |

### 阶段 3（P2，3-5 周）：分布式韧性 + 完整恢复

#### 目标

消除多节点部署的「单节点架构幻觉」。

#### 交付物

1. **跨节点状态协调**：
   - 节点心跳（`cluster:live` zset）
   - `room_online` Redis 镜像（仅在 `PresenceStore` 基础上增量写回）
   - `stream_watchers` Redis 镜像
   - `call_rosters` 来自 Redis（已有 `CallRosterStore`，补回写到 `Hub.call_rosters`）

2. **WS 事件游标（方案 B/C）**：
   - `GET /api/rooms/:id/snapshot?after_seq={seq}` 端点
   - SeqGate 心跳 seq（每 30s 空 seq 帧）
   - 客户端 `_cursor` 替换 `_lastSeen`
   - 通话状态独立恢复（方案 C）

3. **配置热重载 B 期**：
   - `POST /-/reload` 端点
   - 前 5 个热重载项（KeywordModerator, rate_limit, telemetry, log level, CORS）

#### 里程碑

- **Day 3**：节点心跳 + `stream_watchers` Redis 镜像
- **Day 8**：WS 快照端点 + 客户端 `_cursor` 实现
- **Day 15**：通话状态独立恢复 + SeqGate 心跳 seq
- **Day 22**：配置热重载
- **Day 25**：完整 e2e 测试（多节点）

#### 风险与缓解

| 风险 | 可能性 | 影响 | 缓解 |
|------|-------|------|------|
| 事件快照在大型房间（10 万+ 成员）的性能 | 中 | 高 | 快照只包括状态（成员列表用计数代替），完整列表通过 REST 拉取 |
| 多节点脑裂 | 中 | 高 | Redis 节点心跳 + TTL 冗余 + 通知但不自动切换 |
| 热重载影响到启动时 gate 决策 | 中 | 中 | 热重载面板标注哪些项不可重载（gate 项标记 `reload: false`） |
| `_cursor` 迁移中断旧客户端 | 低 | 高 | `?cursor=` 是附加参数，`?since=` 保留 6 个月 |

### 实施顺序的总逻辑

```
P0: 治理仲裁器 ──────→ 补齐最大的合规缺口（编辑绕过、隔离墙缺口）
    │
    ↓
P1: 数据生命周期 ─────→ 防止存储失控和 GDPR 风险
    │
    ├── 配置工具链 ──→ 防止配置错误引起的运行时事故
    │
    ↓
P2: 跨节点状态协调 ───→ 多节点部署的完整容错
    │
    ├── WS 事件游标 ──→ 断线重连的用户体验完整
    │
    └── 配置热重载 ──→ 运维效率和变更安全
```

---

## 总结

### 核心判断

这五个缺口不是「随机发现的 bug」，而是系统在从**功能完整性**到**生产可用性**的演进过程中必然遇到的**架构横切面断裂**。它们在三个层面需要解决：

| 层面 | 所需能力 | 覆盖方向 |
|------|---------|---------|
| **策略组合** | 多策略评估需可审计、可组合、可扩展 | 方向一 |
| **状态恢复** | 客户端 and 服务端的状态都需要在故障后自动恢复 | 方向二、四 |
| **横切面治理** | 数据生命周期和配置生命周期需要统一、可审计的框架 | 方向三、五 |

### 实施建议

1. **先拿方向一（治理仲裁器）打样**：它是五个方向中架构收益最大、且可作为后续所有策略扩展的基础设施。仲裁器的 `Policy` trait 模式可以作为「可插拔横切面」在系统内推广。

2. **方向二和方向四有重叠但不要合并**：方向二是客户端状态恢复（偏向端用户体验），方向四是服务端状态分布（偏向运维韧性）。两者的实施路径不同（客户端 JS 变更 vs 服务端 Redis 扩展），但它们的根源相同——「状态恢复的缺失」。建议在 road map 中标注这个共因，但分开实施。

3. **方向五（配置）的敏感值脱敏是 P0 级的安全修补**：即使配置 schema 和热重载可以推迟，敏感值脱敏不应推迟——因为这是一个安全漏洞（敏感配置明文出现在启动日志中）。建议在两周内的 hotfix 级别的独立工作项中修复。

4. **不要承诺「通话热迁移」**：方向四中 SFU 会话的不可迁移性是一个硬性约束。向管理层汇报时，明确的措辞是：「节点故障时通话会中断 3-5 秒（客户端自动重连），但服务器不会丢失通话元数据或用户状态」。做出这个承诺的清晰性比过度承诺再后补更重要。

### 总体建议优先级排序

| 排序 | 工作项 | 成本 | 收益 | 依赖 |
|------|-------|------|------|------|
| 1 | 敏感值脱敏（配置方向 A 期子项） | 1-2 天 | 安全修复 | 无 |
| 2 | 治理仲裁器核心（方向一） | 2 周 | 合规+架构 | 无 |
| 3 | 数据生命周期 A 期（方向三） | 3-5 天 | 存储安全 | 无 |
| 4 | WS 状态恢复方案 A（方向二） | 3-5 天 | 用户体验 | 无 |
| 5 | 节点心跳 + 状态镜像（方向四方案 A） | 1 周 | 运维韧性 | 方向二（可选） |
| 6 | 配置 schema 自省（方向五 A 期） | 3-5 天 | 运维效率 | 无 |
| 7 | WS 事件游标方案 B（方向二） | 2 周 | 完整体验 | 方向二方案 A |
| 8 | canvas checkpoint（方向三 B 期） | 2 周 | 大表治理 | 方向三 A 期 |
| 9 | 配置热重载（方向五 B 期） | 1-2 周 | 运维效率 | 方向五 A 期 |
| 10 | SFU 媒体会话故障通知（方向四 B 期） | 2-3 周 | 通话韧性 | 方向四 A 期 |
