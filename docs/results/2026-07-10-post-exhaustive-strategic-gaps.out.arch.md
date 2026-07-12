# 架构分析报告：第 33 轮全局扫描 — 5 个未覆盖方向

## 1. 架构评估

### 1.1 当前架构的核心优势

**事件驱动骨架的正确性已被验证**。Aero IM 的事件 DAG（业务层 → NATS → Hub → WebSocket）是架构中最稳固的部分，其优势在于：

- **进程内扇出与跨实例投递的解耦**：`Hub::fan_out_raw` 的 bounded mpsc 模式保证了单进程内不会因扇出过载而阻塞业务路径，而 NATS JetStream 提供了跨实例的持久化投递。这一分层在方向一（嵌入式客服）中被复用评估为「准确」，说明设计具有前瞻性。
- **per-subject 单调 seq 的设计**（`bus/seq.rs`）为客户端去重/排序提供了可靠基础，即使 at-least-once 重投也能保持顺序。
- **crate 分层严格遵循依赖方向**（基础层 → IM 层 → 直播层 → 组合层），无成环风险，这在新方向（如方向四的迁移工具）中可以作为独立 crate 插入而不破坏现有依赖。

### 1.2 暴露的架构局限性

| 局限性 | 表现 | 严重程度 |
|--------|------|----------|
| **认证模型的刚性** | 所有入口假设已认证用户（`AuthUser` extractor），无法直接嵌入访客场景 | **高**——方向一的根本障碍 |
| **编译时迁移嵌入成为约束** | `sqlx::migrate!("../../migrations")` 不支持 down.sql 的干净分离，回滚能力被架构锁死 | **高**——方向四的根源 |
| **seq 分配与消息生命周期的耦合** | `publish_room_event` 在确认交付前就分配 seq，与 Undo Send 冲突 | **中**——方向二的深层问题 |
| **异步审核管线的非投递门属性** | `moderation_bot` 是有预算约束的异步后处理，不能用作访客消息的实时过滤 | **中**——方向一的内容安全缺口 |
| **Redis vs 进程内存的状态分歧** | 跨节点状态（presence/roster）走 Redis，但 undo 窗口在内存中，崩溃恢复无保证 | **中**——方向二 |

### 1.3 关键设计决策的合理性审视

**最有争议的决策**：`sqlx::migrate!("../../migrations")` 编译时嵌入。

这是一个典型的「便利性 vs 灵活性」trade-off。它在项目早期（迁移 < 30 个）时是完全正确的选择——零部署步骤、零运行时依赖、编译即验证。但在 157 次迁移后，它正在成为架构瓶颈：

- **无法支持回滚**：down.sql 无法以干净的方式与 forward migrations 共存于同一目录
- **无法动态选择迁移版本**：二进制文件编译时固定了迁移集，无法做金丝雀部署中的 schema 版本协商
- **迁移工具与运行时代码耦合**：`aero-cli migrate` 本质是执行编译进二进制的前向迁移，没有独立的数据迁移工具

**建议的架构转向时机**：当迁移次数超过 100 次或团队 > 3 人时，应考虑独立迁移工具。当前 157 次迁移已经过了这个阈值。

## 2. 扩展方向

### 2.1 方向一：可插拔认证架构（Authentication Seam）

**为什么需要**：
- 方向一（嵌入式客服）需要一个完全独立的认证流，但当前 `AuthUser` extractor + JWT 中间件栈是单体式的
- 未来还可能出现：API key 认证（已有 PAT 但不同）、OAuth 设备授权流、Webhook HMAC 签名验证
- 当前每个新认证模式都需要修改中间件核心逻辑，违背开闭原则

**核心挑战**：
- 如何在 route 级别选择认证策略，而不影响全局中间件栈
- 访客 token 必须有严格的作用域隔离（只能访问指定 conversation，不能访问任何 room）
- 与现有 `AuthUser` 的共存：同一个请求路径下可能同时需要两种认证模式

**架构变更**：
```
当前：Request → AuthUser extractor (JWT/header-only) → handler

目标：Request → AuthenticationSelector → 
  ├── JwtStrategy → AuthUser (现有)
  ├── VisitorTokenStrategy → VisitorSession (新建)
  ├── PatStrategy → AuthUser (已有但内联)
  └── WebhookHmacStrategy → WebhookContext (未来)
```

**关键设计决策**：
- **选项 A（trait-based extractor）**：为 `AuthUser` 实现一个 `FromRequestParts` 的变体，内部委托给 `AuthenticationSelector`。优点是对现有 handler 零改动；缺点是 trait dispatch 的灵活性有限。
- **选项 B（axum middleware layer）**：在 `routes::build()` 中按路由组挂载不同的认证中间件。优点是完全解耦；缺点是需要显式区分 `authenticated` 和 `visitor` 路由组。
- **选项 C（提取器策略模式）**：引入 `Authenticated<T>` 泛型提取器，`T` 在路由注册时通过拓展类型指定。优点是最灵活；缺点是学习曲线。

**推荐**：选项 B 作为近期方案（因为方向一需要全新的路由组，自然可以挂载独立中间件），选项 C 作为远期架构目标。

**影响范围**：`server/src/` 中间件层、`aero-auth` crate 新增策略 trait、无 storage 变更。

### 2.2 方向二：两阶段 seq 分配（Two-Phase Seq Minting）

**为什么需要**：
- Undo Send 的核心冲突在于 seq 分配时机：当前 `publish_room_event` 在发送时立即分配 seq，但 undo 窗口内的消息不应被确认
- 除 Undo Send 外，未来还可能支持 Scheduled Send（定时发送）、Draft Send（草稿预览）。这些场景共享同一个矛盾：**消息存在于系统中但尚未获得永久的 seq 位置**

**核心挑战**：
- `bus::SeqMinter` 当前基于 NATS JetStream 的 per-subject 计数器，是一次性操作
- 需要将 seq 分配分解为 `reserve` 和 `commit` 两个阶段，且 `reserve` 必须保证 seq 不空洞（commit 失败时释放）
- `commit` 必须在 undo 窗口过期后、消息最终确认时触发，这要求 `ImService` 中有明确的「确认点」

**架构变更**：
```
当前：publish → assign_seq → publish_room_event(NATS) → fan_out

目标：publish → reserve_seq(pending) → publish_room_event(NATS,seq=pending) → 
      [undo window] → confirm_delivery → commit_seq(pending→confirmed) → fan_out(最终)
      
      cancel_delivery → release_seq(pending→void) → nack NATS message
```

**关键接口**：
```rust
// bus crate 新增
trait SeqMinter {
    async fn reserve(&self, subject: &str) -> Result<ReservedSeq>;
    async fn commit(&self, subject: &str, seq: ReservedSeq) -> Result<CommittedSeq>;
    async fn release(&self, subject: &str, seq: ReservedSeq) -> Result<()>;
    // 崩溃恢复
    async fn recover_pending(&self, subject: &str) -> Result<Vec<PendingSeq>>;
}
```

**Redis 存储结构**：
```
pending_seqs:{subject} → Sorted Set (score=reserve_time)
  member: "{seq}:{message_id}" score: 1712345678
```

恢复时，扫描 `pending_seqs:*`，过期（> undo window）的自动 commit，未过期的重建 in-memory undo 窗口。

**影响范围**：`aero-bus` crate（新增 trait + Redis-backed 实现）、`aero-im-core`（`ImService::send_message` 修改）、`aero-storage`（可选——如果使用 Redis 则无存储变更）。

### 2.3 方向三：租户上下文解析与缓存层（Tenant Resolution Layer）

**为什么需要**：
- 白标品牌定制的核心技术挑战不是在数据库中存储 brand config，而是在每个请求的**路由匹配之前**解析出 tenant context
- 当前所有路由都假设 URL 路径中已经包含 `workspace_id` 或通过 JWT 解析出 participant → workspace，但白标场景下：
  - 自定义域名（`customer.com`）需要 `Host` header → workspace 的查询
  - 品牌 CSS 变量需要在 HTML `<head>` 中注入，这意味着在渲染之前 workspace 必须已解析
  - 同域名多 workspace（`aero.im/workspace-slug`）需要 URL 前缀解析

**核心挑战**：
- 这是一个「在路由匹配之前」执行的操作，axum 的中间件层支持但不广泛使用
- 缓存策略：`Host → WorkspaceId` 的查询必须是高性能的（每个请求一次），且需要在 workspace 配置变更时即时失效
- 与现有 `AuthUser` extractor 的顺序关系：先解析 tenant context，再认证用户——因为认证可能需要 tenant 特定的 IdP 配置

**架构变更**：
```
新增中间件优先级（在现有中间件之前）：

Request → TenantResolutionLayer → AuthenticationLayer → RouteMatching → Handler
  
TenantResolutionLayer:
  1. Host header 提取
  2. Host 域名在 TenantCache 中查找（in-memory DashMap + TTL）
  3. 缓存未命中则回表查询 workspace_domains 表
  4. 结果注入 request extensions (axum::extract::Extension)
  5. 若为路径式多租户，从 URL 路径提取 slug → workspace_id

Handler 中通过 Extension<TenantContext> 提取：
  - workspace_id: Uuid
  - brand_config: Arc<RwLock<BrandConfig>> (懒加载)
  - is_custom_domain: bool
  - supported_idps: Vec<IdpConfig> (用于认证页面)
```

**缓存设计**：
```
TenantCache {
  // Host → workspace_id, 30s TTL, 最大 10,000 条目
  domain_to_workspace: DashMap<String, CacheEntry<WorkspaceId>>,
  // workspace_id → BrandConfig, 60s TTL, 按需加载
  brand_configs: DashMap<WorkspaceId, CacheEntry<Arc<BrandConfig>>>,
  // 失效方式 (Phase F)
  version_vector: AtomicU64, // 每次 brand_config 变更时递增
}
```

**影响范围**：`aero-storage` 新增 `workspace_domains` 表、`server/src/middleware/` 新增 `tenant_context.rs`、`aero-common` 新增 `TenantContext` 类型、`routes.rs` 新增中间件注册。

### 2.4 方向四：独立迁移工具与 Schema 版本协约（Independent Migration Tooling）

**为什么需要**：
- 当前 `sqlx::migrate!("../../migrations")` 的编译时嵌入正在成为回滚和 schema 生命周期管理的架构瓶颈
- 157 次迁移后，任何 schema 变更都需要全量迁移验证，无法做增量回滚
- 金丝雀部署、蓝绿部署、多版本并存都需要「应用二进制版本 N 可以安全地使用 schema 版本 N-1」

**核心挑战**：
- 拆分迁移目录：`migrations/forward/` 和 `migrations/rollback/` 需要修改 `build.rs` 或迁移加载器
- 与现有 `sqlx::migrate!` 的兼容：需要新的 `aero-cli` 子命令不使用 `sqlx::migrate!`，而是手动加载迁移
- Schema 版本兼容性声明（L5）需要在每个 migration 中嵌入 `min_app_version` 和 `max_app_version`，并在启动时验证

**架构变更**：
```
当前：
  migrations/  (flat, .sql files)
  sqlx::migrate!("../../migrations") in build.rs

目标：
  migrations/
    forward/   (前向迁移, 编译时嵌入)
      0001__create_users.sql
      0002__add_avatar.sql
    rollback/  (回滚脚本, include_str! 手动加载)
      0001__drop_users.sql  
  aero-cli migrate [--target N]  # 支持回滚到指定版本
  aero-cli check  # 检查 schema 版本与二进制版本兼容性
```

**Schema 版本协约的启动时检查**：
```
Server 启动时：
  1. 查询 _sqlx_migrations 表获取当前 schema 版本
  2. 读取自身嵌入的 min_schema_version / max_schema_version
  3. 如果当前 schema 版本不在范围内：
     panic!("二进制版本 v{} 要求 schema 版本 {}–{}，当前版本 {}。请执行迁移。")
```

**影响范围**：`aero-cli` crate（新增子命令）、`aero-storage/db.rs`（迁移加载方式修改）、`Cargo.toml`（新依赖 `sqlx-cli` 或手写迁移解析器）、无业务代码变更。

### 2.5 方向五：可观测性审计管道（Observability-Backed Audit Pipeline）

**为什么需要**：
- 管理操作审计日志（方向五）是正确但不够——独立的审计表会在强合规场景下扩展为多个审计表（数据访问审计、系统配置变更审计、权限变更审计）
- 需要一个统一的审计管道模式，而不是逐功能手动添加 `emit_admin_audit` 调用
- PCI-DSS、SOC2、HIPAA 等合规框架要求审计日志不可篡改、不可绕过

**核心挑战**：
- **审计的不可绕过性**：如果审计是应用层显式调用 `emit_admin_audit`，开发者在写新功能时可能忘记调用。需要一个更底层的审计钩子。
- **审计存储的不可篡改性**：应用层 `no DELETE/UPDATE` 不够——需要 DB 级保护（RLS 或触发器）。
- **审计事件的关联性**：单个管理操作可能触发多个资源变更（如「删除 workspace」级联删除所有房间和消息），需要事务级关联 ID。

**架构变更**：
```
// 统一审计管道（在事件驱动骨架中自然对齐）
trait AuditSink: Send + Sync {
    async fn record(&self, event: AuditEvent) -> Result<()>;
}

// 事件定义（不是在每个 handler 中自由拼凑）
struct AuditEvent {
    id: Uuid,
    correlation_id: Uuid,      // 关联同一操作链的事件
    timestamp: DateTime<Utc>,
    actor_id: ParticipantId,
    actor_type: ActorType,     // Admin | System | ApiKey | Webhook
    action: AuditAction,       // 枚举，非自由字符串
    resource_type: ResourceType,
    resource_id: String,
    before: Option<JsonValue>, // 变更前快照
    after: Option<JsonValue>,  // 变更后快照
    ip_address: Option<IpAddr>,
    user_agent: Option<String>,
}

// 存储层
enum AuditStorage {
    Database(AuditDbSink),     // 当前方案，适合大多数场景
    Elasticsearch(EsSink),     // 高查询量场景
    S3(S3Sink),                // 冷存储归档
    Composite(Vec<Box<dyn AuditSink>>), // 同时写入多个目的
}
```

**DB 级保护**（在 migration 中声明）：
```sql
-- 审计表默认 RLS
ALTER TABLE audit_admin_actions ENABLE ROW LEVEL SECURITY;
-- 应用角色只有 INSERT 权限
REVOKE UPDATE, DELETE ON audit_admin_actions FROM aero_app;
-- 审计管理员有 SELECT
GRANT SELECT ON audit_admin_actions TO aero_auditor;
```

**影响范围**：`aero-storage` 新增 `audit` 模块（trait + DB impl）、`aero-common` 新增 `AuditEvent` 类型、现有管理 handler 修改为通过 `AuditSink` 记录（替换直接 SQL insert）、`aero-server` 中间件层可选的自动审计钩子。

## 3. 接口设计建议

### 3.1 核心原则

1. **Trait 优先于具体类型**：所有跨 crate 边界的能力（认证、审计、迁移、seq 管理）都应定义为 trait，让实现层在各自的 crate 中完成。这符合当前架构的 crate 隔离原则。

2. **事件驱动，不变应万变**：审计、品牌配置变更、租户变更都应该走 NATS event（`admin.audit.*`、`tenant.brand.*`），而不是 RPC 调用。这样跨实例同步自然得到保障（方向三的缓存失效、方向五的跨节点审计关联）。

3. **新增接口不影响现有绑定**：所有新接口都应该是 `additive` 的——不修改现有 trait 的已有方法签名，不要求现有实现提供新方法（使用 default impl 或新 trait）。

### 3.2 需要引入的抽象层

| 抽象层 | 位置 | 职责 | 现有替代 |
|--------|------|------|----------|
| `AuthenticationStrategy` | `aero-auth` | 认证策略的 trait + 注册表 | `AuthUser` extractor（耦合于 JWT） |
| `TenantResolver` | `aero-server/middleware` | Host → workspace_id 解析 + 缓存 | 无（路径式解析隐式存在于路由中） |
| `AuditSink` | `aero-storage/audit` | 审计事件写入的 trait | 无（方向五是新增） |
| `SeqMinterV2` | `aero-bus` | 两阶段 seq 分配 | `SeqMinter`（单阶段） |
| `MigrationEngine` | `aero-cli` | 独立迁移执行器 | `sqlx::migrate!`（编译时嵌入） |

### 3.3 向后兼容性策略

1. **选项弃用（deprecation window）**：
   - 新的 `SeqMinterV2` trait 与旧的 `SeqMinter` 共存两个 minor 版本
   - 旧的 `sqlx::migrate!` 路径保留，但在 `aero-cli` 中标记 `deprecated`
   - `AuthUser` extractor 继续有效，新的认证策略作为可选项添加

2. **配置驱动的行为选择**：
   ```toml
   [features]
   # 默认使用旧认证模式
   visitor_auth = false  
   
   [audit]
   # 默认无审计，打开后新审计管道生效
   enabled = false
   mode = "database" | "elasticsearch" | "both"
   ```

3. **特征门控（compile-time feature flags）**：
   - 新方向的功能通过 Cargo features 控制（如 `feature = "visitor-auth"`、`feature = "audit"`）
   - 不影响现有用户的编译时间和二进制体积
   - 新 crate 默认 optional（如 `aero-audit` 默认不引入）

## 4. 技术选型

### 4.1 新依赖评估

| 候选依赖 | 用途 | 评估 |
|----------|------|------|
| `acme-lib` / `rustls-acme` | Let's Encrypt 自动证书 (方向三 Phase D) | **推迟引入**。XXL 体量意味着应作为独立项目，不引入 root workspace。如果 Phase D 开启，选择 `rustls-acme`（纯 Rust，与 `unsafe_code = forbid` 兼容，无 C 绑定） |
| `sqlx-cli` | 独立迁移执行 (方向四) | **需要的**。当前的 `sqlx::migrate!` 无法支持回滚。建议使用 `sqlx-cli` 作为基础，在其上构建自定义迁移引擎，而不是引入新的迁移框架 |
| `tower-http` 的 `SetRequestIdLayer` | 租户上下文注入 (方向三) | **已有依赖**（当前 server 层使用 `tower-http` 做压缩），无需新依赖。直接复用 |
| `elasticsearch` / `opensearch` | 审计日志高级查询 (方向五远期) | **P3 评估**。初期使用 PG JSONB 即可，ES 增加运维复杂度。只在审计查询成为性能瓶颈时考虑 |
| `opentelemetry` 用于审计 | 审计事件的 metrics 导出 | **已有依赖**（当前 observability 使用 OTLP）。直接为审计事件添加 `Histogram`（写入延迟）和 `Counter`（事件总量/丢弃量），不需要新框架 |

### 4.2 自建 vs 采购决策矩阵

| 能力 | 自建理由 | 外部采购可行性 | 决策 |
|------|----------|---------------|------|
| 嵌入式客服 | 需要深度集成到现有 auth/room/hub 中，外部客服工具（Intercom/Zendesk）无法复用现有基础设施 | 可采购但需要维护两条并行的实时通道 | **自建**——这是架构红利的方向 |
| Let's Encrypt 自动化 | 核心能力 = ACME client + DNS-01 验证 + 证书生命周期管理 | `cert-manager`（Kubernetes）、`acme.sh`（脚本）可做外部组件 | **外部组件**——将 Let's Encrypt 委托给 nginx proxy / cert-manager，server 只关心 `certificate_path` 配置 |
| 审计日志存储 | 简单 JSONB 表即可满足初期需求 | `Amazon CloudTrail`、`Elasticsearch` 等但增加了外部依赖 | **自建初期 + 可扩展接口**——通过 `AuditSink` trait 保持未来切换到 ES 的可能 |
| 迁移回滚 | 需要深度控制迁移执行器的行为（版本协约、回滚、CI 集成） | `sqlx-cli` 是基础工具但不是完整方案 | **自建迁移引擎**——基于 `sqlx-cli` 或手写 |

### 4.3 技术风险评估

| 技术决策 | 风险 | 缓解 |
|----------|------|------|
| 两阶段 seq 分配 | 引入 `reserve`/`commit` 状态后，seq 空洞可能导致客户端逻辑错误 | 严格的测试覆盖：无消息丢失、空洞应在可预期时间内填补、`release` 必须明确记录 |
| 租户解析中间件 | `Host → workspace_id` 缓存 30s TTL 可能导致切换域名后的短暂不一致 | 触发事件驱动失效（方向三 Phase F），即使缓存过时也只是显示旧的 brand config（不影响安全） |
| 独立迁移引擎 | 迁移顺序错误可能导致数据丢失 | 与 `sqlx::migrate!` 相同的排序机制（前缀数字），加 dry-run 模式和 CI 验证 |
| AuditSink bounded channel | channel 满时的事件丢失 | 同步 fallback 写（阻塞） + Prometheus alert on overflow + 高水位标记 |

## 5. 实施路线图

### 5.1 优先级重排序（基于 ROI + 依赖分析）

```
P0（生产安全，立即执行）:
  ├── 方向四 L5: Schema 版本兼容性声明 (~1天)
  ├── 方向五: Admin Audit 表 + AuditSink trait (~1周)
  └── 方向四 L1-L2: 迁移目录拆分 + 回滚执行器 (~1周)

P1（销售就绪度，本季度）:
  ├── 方向三 Phase A-C: 品牌配置表 + CSS 变量 + 邮件模板 (~1周)
  ├── 方向二: Undo Send (不含崩溃恢复，第1版用内存) (~2周)
  └── 方向四 L3-L4: CI 护栏 + dry-run 模式 (~3天)

P2（架构现代化，本季度）:
  ├── 方向一 Phase 0: 可插拔认证架构（AuthenticationSelector trait）(~2周)
  ├── 方向二: Undo Send 崩溃恢复（Redis-backed pending seq）(~1周)
  └── 方向三 Phase D: TenantResolver 中间件 (~1周)

P3（产品扩展，下季度）:
  ├── 方向一 Phase 1-N: 嵌入式客服全功能 (2-3月)
  ├── 方向三 Phase D: 自定义域名（委托给外部 ACME 工具）(~2周)
  └── 方向五 Phase 2: Elasticsearch/S3 审计存储后端 (2-3周)
```

### 5.2 阶段划分

**Phase 0：基础设施准备（1-2 天）**

与现有系统零冲突，可以在现有分支上逐步实施。

- 方向四 L5：`_sqlx_migrations` 表新增 `min_app_version` / `max_app_version` 列 + 启动时检查
- 方向五：`audit_admin_actions` 表 migration + `AuditSink` trait 定义 + `DbAuditSink` 实现
- 方向四 L1：迁移目录拆分 `migrations/forward/` + `migrations/rollback/`

**里程碑**：`cargo check --workspace` 干净 + 启动时 `panic on schema mismatch` + 审计表可写入

**Phase 1：核心价值交付（2-3 周）**

可以独立交付，每个方向有明确的业务价值。

- 方向五：所有现有管理 route 接入 `AuditSink`（先覆盖 user/workspace/room CRUD + API key 操作）
- 方向三 Phase A：`workspace_branding` 表 + GET/PUT API + `BrandConfig` 类型
- 方向三 Phase B：CSS 变量注入（在 HTML template 中输出 `:root { --brand-primary: #{brand.primary_color} }`）
- 方向四：`aero-cli migrate down` 实现 + CI 中 `verify-migration-rollback` job

**里程碑**：可演示的管理审计历史 + 可配置的 workspace brand color/logo

**Phase 2：架构现代化（3-4 周）**

需要修改核心 crate，需要更严格的测试覆盖。

- 方向一 Phase 0：`AuthenticationSelector` + `VisitorTokenStrategy`（短生命周期 HMAC token）
- 方向二：`SeqMinterV2` + `ImService::send_message_with_undo`
- 方向三：`TenantContext` + `TenantResolutionLayer`

**里程碑**：访客 token 可创建 + 消息撤回可工作（含崩溃恢复）+ 自定义域名可解析 workspace

**Phase 3：产品扩展（2-3 月，独立项目）**

- 方向一：嵌入式客服 JS SDK + 完整对话 UI + 座席控制台 + 访客内容安全过滤
- 方向三 Phase D-F：自定义域名 + ACL 权限 + 实时缓存失效
- 方向五 Phase 2：审计事件转发到外部 SIEM

**里程碑**：客服产品可用 + 白标域名上线

### 5.3 风险矩阵

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|----------|
| 方向一认证模式绕过（访客 token 泄漏导致未授权访问） | 中 | 严重 | 访客 token 必须有硬编码的 scope 绑定（只能访问指定 conversation）+ 短 TTL（15min）+ 速率限制 |
| 方向二 Undo Send 崩溃恢复不完整 | 中 | 中 | Redis-backed + 启动时恢复 + 崩溃期间过期的窗口自动确认（而不是撤回，保证消息不丢） |
| 方向三 brand config 缓存过时 | 低 | 低 | 30s TTL 足够短，且 brand config 变更不频繁；Phase F 实现实时失效 |
| 方向四迁移回滚执行顺序错误 | 低 | 严重 | 回滚必须严格按照 reverse order of forward migrations，加 dry-run 模式 + CI 中全量回滚验证 |
| 方向五审计表成为写入热点 | 低 | 中 | bounded channel + sync fallback + 审计写入在单独连接池（不竞争业务连接） |
| 多个方向修改同一核心 crate（如 aero-bus）导致冲突 | 中 | 中 | 每个方向在独立分支开发，通过 RFC 流程预先协商 crate 接口变更 |

### 5.4 跨方向依赖图

```
方向四 L5 (schema version) ── 无依赖，可最先做
    │
方向四 L1-L2 (migration dirs) ── 依赖 L5 的设计决策（version 列）
    │
方向五 (audit) ── 无依赖，可并行于方向四
    │
方向三 Phase A-C (brand config) ── 无依赖，可并行
    │
方向二 V1 (in-memory undo) ── 依赖方向四的 seq 设计决策
    │
方向二 V2 (redis-backed undo) ── 依赖 V1 + 方向三的 Redis 接入模式
    │
方向一 Phase 0 (auth pluggability) ── 独立，但接入  方向三的 TenantResolver
    │
方向一 Phase 1-N (chat widget) ── 依赖 Phase 0 + 方向五的 audit（客服需要审计）
```

**推荐执行顺序**：

1. **第 1 周**：方向四 L5 + 方向五 + 方向四 L1-L2（三个独立线程并行）
2. **第 2 周**：方向三 Phase A-C + 方向二 V1（并行）
3. **第 3-4 周**：方向二 V2 + 方向一 Phase 0 + 方向三 Phase D（顺序执行，因为修改核心 crate）
4. **第 5 周+**：方向一 Phase 1-N（产品线级别的投入）

---

## 总结

这五个方向的全局扫描质量很高，技术分析扎实，边界情况识别到位。主要的架构层面修正集中在：

1. **体量偏差**：方向一（客服）是 XXL 而不是 L——它要求认证模型的根基性变更。方向三 Phase D 同样是 XXL——完整的 ACME 自动化平台。将这两个方向从 P2 移到 P3，避免资源承诺不足。

2. **一个核心架构约束**：`sqlx::migrate!("../../migrations")` 的编译时嵌入与回滚能力存在根本冲突。这是五个方向中唯一需要「先解决架构约束才能推进功能」的情况。方向四 L1-L2 应该先于所有其他方向（甚至在方向五之前）——因为迁移目录拆分会改变所有 migrations 文件的位置，影响其他方向的 migration 添加方式。

3. **最大架构红利**：方向一（嵌入式客服）确实是一个可以从现有事件驱动骨架中生长出来的新产品线。如果成功，它将验证「事件驱动 + 进程内扇出」架构的可扩展性超出了原始设计者的预期——这是架构师最愿意看到的信号。
