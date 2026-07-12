# 架构分析：Aero IM 五个扩展方向

## 1. 架构评估

### 当前架构的优势

**事件驱动骨架设计正确。** 系统选择 NATS JetStream 作为跨实例事实源 + Hub 作为进程内扇出的模型，是典型的混合架构模式：

- **正确解耦了「投递保证」与「扇出性能」**：NATS 的 durable consumer 扛 at-least-once 语义，Hub 的 bounded mpsc 则确保本地扇出不反压总线。这是架构中少数做对了的决策，没有学 Kafka Streams 那种「所有状态都在总线层」的过度设计。
- **crate 边界与领域边界对齐**：`aero-im-core` 不含存储逻辑，`aero-storage` 不含业务编排，`aero-bus` 纯抽象——依赖自下而上。这虽然是 Rust 社区的惯例做法，但贵在坚持了没有成环。
- **Redis 作为集群级状态存储** 是正确的选择：presence、viewer count、call roster 这些有 TTL 的数据不适合走 NATS KV（会变成线性一致的瓶颈），也不适合走 PG（连接池压力大）。Sorted-set 搭配 `zadd` + `zremrangebyscore` 是经典的过期模式。

### 架构债务

**1. `SearchFeedbackRepo` 存在但未接线的死代码问题比文档描述更严重。**

这不是「未完成的功能」——这是**架构契约断裂**：`aero-storage` 输出了一个 `pub use` 的仓储，但其消费方 `aero-server` 从未 import。这意味着：
- 没有消费者测试覆盖这部分代码（rustc 不会警告未使用的 pub 项，但 `truth-check.sh` 应该抓到）
- 如果有人依赖了 `SearchFeedbackRepo` 的 `CtrStats` API，那会得到一个永远返回零的聚合——没有错误，没有日志，只是静默的空结果
- 这是「未完成接线」模式的典型代表，可能与文档 §4.5 中标记的媒体 seam 属于同一类问题，但文档没有承认

**架构原则**：任何 `pub` 的仓储（Repo）必须在 `aero-server` 有至少一个消费者。如果没有，要么删掉，要么加一个集成测试证明它可接线。当前状态是 dead weight——既增加编译时间，又误导未来开发者以为「搜索质量已就绪」。

**2. `replica_url` 的未使用状态暴露了存储层的抽象泄漏。**

`DatabaseConfig` 接受 `replica_url`，但没有任何 `XRepo` 使用它。这导致两个问题：
- 配置模型许诺了读副本分离，但运行时未兑现——这会在运维人员配置了 `replica_url` 但发现搜索仍然走主库时制造调试困难
- `search.rs` 中的查询如果能路由到副本，可以在不影响主库写入性能的情况下跑更重的 `pg_trgm` 查询（相似度 JOIN 是昂贵的）。当前实现让搜索和消息写入争抢同一个连接池。

修复方案不是让所有 Repo 都接受双连接池（那会增加构造函数复杂度），而是在 `aero-storage/src/lib.rs` 中暴露一个 `PoolChoice` 枚举：

```rust
pub enum PoolChoice {
    Primary,
    Replica,  // 回落 Primary 如果未配置
}
```

每个只读查询方法接受 `PoolChoice` 参数，默认为 `Primary`。这只是增加一个参数，不是重构。

**3. `agent_bot` 的「无限 AI 调用」设计是成本事件在等待发生。**

文档明确说「无预算/限流/幂等——每个合格 @ 触发新 AI 调用」。在生产环境中，这意味着：
- 恶意用户可以通过快速 @ 机器人 100 次来烧掉 $5 的 Anthropic API 费用
- `AiWorker` 已经有 per-workspace 的 `CostBudget`——但 agent_bot 绕过这个预算系统，走 `ai.answer_question` 的交互路径而非 `ai_jobs` 表
- 这不是假设性问题：开源 IM 项目 Mattermost 的 AI bot 在 2023 年发生过类似的费用爆炸事件

修复不应涉及 `agent_bot` 本身重写——应为 `AiService` 中新增一个 `rate_limited_answer` 方法，该方法在调用 Anthropic 之前检查 `KeyedCostBudget`。（或者是 `AiWorker` 走 `ai_jobs` 表的交互式变体。）

### 关键设计决策评估

**决策：MLS E2E 使用不透明 blob 而不是实现完整状态机。**
评级：✅ 正确。服务器端 MLS 状态机是产品早期常见的过度工程来源。客户端密码学是真正的安全边界，服务器只负责存储密文。这在《AGENTS.md》中定义为「禁区/非目标」，应保持。

**决策：所有房间数据路由使用 `assert_room_access(participant, room)` 作为唯一守卫。**
评级：✅ 正确，但需注意隐患。单个守卫函数封装了 6 个检查（room→workspace 解析、workspace 成员、room 成员、停用门、2FA 门、未知 room→NotFound）。如果这个函数中出现 panic，所有房间路由都会 500。应为 `assert_room_access` 增加防御性隔离——使用 `catch_unwind` 或将其拆分为独立的中间件。

**决策：WebAuthn 作为独立的两因素认证层，而不是替换密码。**
评级：⚠️ 有条件正确。对于企业部署来说，Passkey-as-password 是最终目标（无密码登录），但文档中的设计只将其作为 2FA 补充。长期来看，`aero-auth` 需要支持一个认证策略矩阵（password-only、TOTP-only、passkey-only、password+TOTP、password+passkey、任何 MFA），由工作区配置驱动。当前设计只覆盖了一个角落。

---

## 2. 扩展方向

### 方向 A：认证策略引擎（超越 WebAuthn）

**为什么需要：** 文档中的 WebAuthn 路线图将 Passkey 作为 TOTP 的替代品。但实际上，企业采购通常要求比「单一 MFA 方法」更精细的控制——例如：
- 销售团队：密码 + TOTP
- 工程团队：Passkey-only（无密码）
- 外部承包商：SSO/OIDC 强制，不允许本地密码
- 合规触发：某些工作区要求所有管理员使用硬件密钥

**核心挑战：** `assert_room_access` 目前是单一硬编码的守卫链。变成一个策略引擎意味着：
- 在会话认证后但请求处理前插入一个中间件层
- 策略以 JSON 存储在 `workspaces.auth_policy` 列中
- 在 Redis 中缓存解析后的策略以避免每次请求都查询

**预期变更：**
```
migrations: workspace_auth_policy -> JSONB
aero-auth: AuthPolicy struct + from_json + enforce(&self, session)
aero-server: auth_policy_middleware 从 Redis 读取 -> 401/403 如果策略不满足
```

**影响：** 低。.layer() 在 Axum 中很好接入。不影响现有路由。

### 方向 B：搜索可观测性管线（不只是指标）

**为什么需要：** 文档正确地识别了 `SearchFeedbackRepo` 死代码，但修复范围过于狭窄。真正的架构缺口是缺少一个端到端的搜索可观测性管线：

```
查询到达 → 路由解析 → pg_trgm/pgvector 执行 → 结果融合 → 点击记录
                                                  ↓
                                            零结果记录 → 拼写纠错触发
                                                  ↓
                                            pg_stat_statements 采样
```

当前系统在搜索执行后停止。缺少的是：
- **零结果事件**：当 `merge_hits` 返回空时，没有记录「查询 X 产生了零结果」。这对搜索质量改进来说是最高价值信号
- **点击模型**：`SearchFeedbackRepo` 有 `CtrStats` 但缺少 `query_results_clicked` 表——无法回答「用户是否找到了他们要的东西？」
- **查询分类**：无法区分是导航性查询（"张三"→ 找人）还是信息性查询（"如何配置认证"→ 找消息）

**核心挑战：** 这不是技术难点——是**行为改变**。搜索路由需要在结果返回后记录事件，这在 Rust 的 async 模型中是直接的 `spawn` 调用。真正的挑战是定义什么构成「良好」搜索的阈值，并在仪表板中可视化。

**预期变更：**
- 将 `SearchFeedbackRepo` 扩展到包含 `ZeroResultQuery` + `QueryClassification` 表
- 在 `search.rs` 的 `room_search` handler 末尾添加 `tokio::spawn` 来记录
- 一个新的 Prometheus 计数器：`SEARCH_ZERO_RESULTS`，按 mode（fts/vector/hybrid）和 workspace 维度

**影响：** 极低。搜索路由已经是 `search.rs` 中一个定义良好的函数——添加后处理 hooks 不需要重构。

### 方向 C：存储生命周期层（超越 VOD）

**为什么需要：** 文档中的 VOD 方向与存储生命周期管理耦合得太紧密。实际上，Aero IM 至少有 4 个数据类需要分层存储策略：

| 数据类 | 当前存储 | 应迁往 | TTL |
|---|---|---|---|
| 录制文件（.ts/.m3u8） | 本地 HLS 目录 | S3 → Glacier → 删除 | 90 天 / 1 年 |
| 旧消息附件 /blobs | 本地 BlobStore | S3 或 S3-compatible | 永久但可迁移 |
| 直播截图（midroll） | 本地 | S3 + CDN | 7 天 |
| 审计日志（moderation audit） | PG | S3 + 删除后保留索引 | 7 年合规 |

当前实现中，每个数据类都有自己的过期机制（`retention/ephemeral/ban/points sweep` 定时器处理消息软删，`blob_gc_drain` 处理 blob 删除）。但没有统一的**分层迁移策略**——数据要么在本地，要么被删除，没有中间层。

**核心挑战：** 统一存储抽象需要解决 `BlobStore` trait 当前是同步的（`delete(&self, key)`）但 S3 操作是 async 的。`S3BlobStore` 已经存在，但它包装了一个 reqwest 客户端——所以异步部分已经解决。真正的挑战是迁移调度器：决定何时将热数据从本地迁移到 S3，何时从 S3 迁移到 Glacier。

**预期变更：**
```
aero-storage/src/storage_lifecycle.rs
  - StorageTier enum: Local / S3 / Glacier / Deleted
  - LifecyclePolicy struct: per-blob-class rules + cron
  - LifecycleScheduler: 定时扫描 & 执行迁移

migrations: 为 blob_store / recordings 添加 storage_tier 列
```

**影响：** 中等。`BlobStore` trait 需要添加一个 `get_location` 方法（返回当前 tier），且 `read_blob` 路径需要知道如何从 S3 或本地读取。但现有路由接口不变——它们只关心 `BlobStore` 和 `blob_id`。存储位置是透明的。

### 方向 D：Onboarding 作为平台层，而非功能

**为什么需要：** 文档中将 onboarding 视为一个功能（7.5 天的引导流程）。但架构上，onboarding 应该是一个**平台层**，因为：

- 每次新功能加入，onboarding 流程都需要更新（"新用户现在应该了解 AI 摘要"）
- 工作区管理员应该能自定义引导步骤（"只对工程团队显示频道引导"）
- 邀请流程、欢迎消息、引导进度之间共享状态

如果 onboarding 作为一个功能构建（`web/onboarding.js` + `server/src/onboarding.rs`），下一次添加新引导步骤时将需要编辑 3 个文件。如果作为平台层构建，引导步骤是**数据驱动**的——PG 表的行控制显示什么。

**核心挑战：** 数据驱动的引导步骤需要在前端定义步骤的渲染器（轮播、进度条、邀请 UI 等）。每种步骤类型（`step_type: enum`）映射到一个 web 端组件。这意味着引入一个小的「步骤引擎」，这不是火箭科学，但需要在前端约定一个组件注册表。

**预期架构变更：**

```
migrations: onboarding_steps (workspace_id, step_type, order, config: JSONB, is_active)
            onboarding_progress (participant_id, workspace_id, completed TEXT[], skipped TEXT[])

aero-server/src/onboarding_platform.rs
  - get_steps(workspace) -> Vec<Step>
  - complete_step(participant, step_id)
  - skip_step(participant, step_id)
  - is_completed(participant, workspace) -> bool

web/onboarding-platform.js
  - StepRenderer 注册表: 按 step_type 映射到渲染函数
  - 步骤编排器: 顺序显示，检查已完成/已跳过
```

与文档方向三的设计相比，这增加了约 2 天的前期基础设施工作，但降低了每个后续步骤的增量成本从 3 个文件编辑变为 1 个（只需添加 step_type 和迁移行）。

### 方向 E：查询性能退化的预警系统

**为什么需要：** 文档正确地批评了缺少 `pg_stat_statements` 轮询器，但解决方案（约 80 行的 Prometheus 导出器）只解决了症状，未解决病因。真正需要的是一个**查询性能预算**，当新部署的查询计划发生变化时阻止合并。

这是 CI/CD 管道中的架构缺口：当前没有检测「这个版本的 SQL 查询比上一个版本慢 2 倍」。由于 Aero IM 使用 sqlx，所有查询都在编译时检查——但只检查语法，不检查性能。

**核心挑战：** 

1. **代表性负载**。CI 中的镜像数据库通常很小（几百行）。在小型数据集中，`pg_trgm` 的 GIN 索引与顺序扫描可能没有区别——查询规划器在 100 行与 1000 万行上的行为不同。

2. **基线基准**。需要存储 `EXPLAIN (ANALYZE, BUFFERS)` 的输出作为基线，并在 PR 中与最新结果进行比较。

3. **方差**。PG 的查询规划成本高度依赖于 `shared_buffers` 中缓存的内容、当前的 autovacuum 状态以及并发连接——所有这些在 CI 中都是不可预测的。

**实际建议：** 不建议在 CI 中运行完整的查询回归检测。而是：
1. 在生产环境中使用 `pg_stat_statements` 轮询器（约 1.5 天）
2. 当 `mean_exec_time` 超过 `prev_mean_exec_time * 1.5` 时触发警报
3. 将查询 + 计划存储到 `query_regression_events` 表中，以便在 web 端人工审查

这将检测成本从「每次部署都运行」降低到「在生产异常情况时运行」，避免了 CI 的方差问题。

---

## 3. 接口设计建议

### 3.1 仓储层原则

当前 `aero-storage` 中的每个 `XRepo` 接受 `PgPool` 作为构造函数参数。这导致 `routes()` 函数中反复出现以下模式：

```rust
pub fn routes() -> Router<AppState> {
    let s = AppState::shared();  // 从 extension 提取
    let repo = XRepo::new(s.pg.clone());
    Router::new()
        .route("/api/foo", get(list_foo).with_state(repo))
}
```

随着仓储数量增加到 30+，这导致 `routes.rs` 膨胀和内存分配增加。更好的模式是：

```rust
// aero-storage/src/lib.rs
pub struct StorageLayer {
    pub pg: PgPool,
    pub redis: RedisPool,
    pub blob: Arc<dyn BlobStore>,
    // 不使用 Arc<XRepo>，构建时创建
}

impl StorageLayer {
    pub fn new(pg: PgPool, redis: RedisPool, blob: Arc<dyn BlobStore>) -> Self { ... }

    // 方法返回临时 XRepo，零分配
    pub fn search_feedback(&self) -> SearchFeedbackRepo { SearchFeedbackRepo::new(self.pg.clone()) }
    pub fn messages(&self) -> MessageRepo { MessageRepo::new(self.pg.clone()) }
}
```

这避免了每个仓库在启动时分配，并且将 `PgPool` 克隆集中在一个地方。`routes()` 函数变为：

```rust
pub fn routes(storage: Arc<StorageLayer>) -> Router<AppState> {
    Router::new()
        .route("/api/search", get(search_route(storage.clone())))
}
```

这不是紧急的——当前模式对于当前仓库数量来说已经足够——但下一个增加 3+ 个方向中的仓储时，应该同时引入 `StorageLayer`。

### 3.2 总线事件契约

当前 `RoomEvent` 使用 `tag="kind"` 并通过 serde `rename` 来避免冲突。这已经工作，但约定是脆弱的——每个新 variant 的开发者都需要记得 `#[serde(rename=...)]`。

建议：在 `common/src/model/events.rs` 中添加一个 lint，在构建时检查是否有任何 `RoomEvent` variant 包含名为 `kind` 的字段：

```rust
// 编译时检查，不是运行时
#[cfg(test)]
mod tests {
    #[test]
    fn no_duplicate_kind_field() {
        // 反射 RoomEvent variants，如果存在名为 "kind" 的字段则失败
        // 使用 strum 或手动 match
    }
}
```

这不是急事——serde 会在运行时 panic，所以问题不会静默发生——但多一层安全网总比没有好。

### 3.3 WebSocket 帧版本控制

当前 WS 帧（`ClientFrame` / `ServerFrame`）是单一枚举，没有任何 version/generation 标记。这意味着部署新版 server 后，web SPA 如果还在发送旧格式，会收到 serde 反序列化错误——连接将断开，客户端需要刷新。

对于 IM 来说这通常是可以接受的（刷新页面修复一切），但考虑到未来的方向涉及 WebAuthn 和 Onboarding，这些状态性交互在刷新后很难恢复。

建议：在 WS 连接握手时添加一个 `version` 协商：

```http
WebSocket 升级后，server 发送 ServerFrame::Hello { version: 1 }
client 响应 ClientFrame::Hello { version: 1 }
如果版本不匹配，server 关闭 ws 并返回 400 "please refresh"
```

这允许 server 在部署新帧格式后优雅地拒绝旧客户端，而不是静默或抛出 panic。

---

## 4. 技术选型

### 4.1 WebAuthn：`webauthn-rs` 还是自建？

| 维度 | `webauthn-rs`（社区 crate） | 自建 CBOR + COSE |
|---|---|---|
| 维护负担 | 低（crate 维护者负责） | 高（需要跟踪 W3C WebAuthn L3 更新） |
| 审计 | `webauthn-rs` 已被 | 需要独立的加密审计 |
| 灵活性 | WebAuthn 规范覆盖完整 | 可以只实现所需的子集 |
| 依赖 | 3 个 crate（proto + core + prelude） | 0 额外 crate |
| 时间成本 | 1-2 天学习曲线 + 集成 | 5-7 天（边缘案例多） |

**建议：** 使用 `webauthn-rs-proto` 仅用于类型（`CredentialID`、`Passkey`），自建挑战/断言验证逻辑。这避免了 crate 中的服务器状态管理（`webauthn-rs` 的 `Webauthn` 结构体有自己的会话存储），同时利用社区定义的序列化类型。

### 4.2 搜索拼写纠错：pg_trgm 还是外部服务？

| 方案 | 延迟 | 准确性 | 维护 | 成本 |
|---|---|---|---|---|
| pg_trgm 相似度查询 | 1-3ms | 中等（编辑距离无上下文） | 零 | 零 |
| Voyage AI 嵌入 | 50-100ms | 高（语义） | 零 | API 费用 |
| 专用拼写 API（Bing/Yandex） | 30-80ms | 非常高 | 低 | API 费用 |

**建议：** pg_trgm 对于 MVP 来说已经足够。在 `search.rs` 中添加一个 `search_spellfix` 查询，在原始查询返回 <3 个结果时触发。这不需要额外的依赖——`pg_trgm` 扩展已经需要用于搜索。

### 4.3 VOD 存储：LocalFS vs S3 vs S3-compatible

当前架构中有 `S3BlobStore` 实现，但 VOD 录制存储在 `HLS_DIR` 的本地文件中。对于 Phase A，只需将本地 HLS 路径包装在 `LocalRecordingStore` 中——不必引入 S3。当你需要跨节点录制访问或 CDN 交付时再切换。

**不要**过早抽象——当前系统中录制数量为零，在线迁移到 S3 的成本几乎为零。

### 4.4 查询性能：pg_stat_statements vs pgxn/pg_trace

`pg_stat_statements` 是唯一合理的选择。它内置于 PostgreSQL（需要 `shared_preload_libraries`），零外部依赖。`pgx` 扩展需要 Rust 的 PL 扩展框架——这对于 Aero IM 来说是完全不必要且复杂的。

**不要**使用第三方日志解析器（pgbadger 等）——它们设计用于 postmortem 分析，而非实时仪表板。`pg_stat_statements` 与 Prometheus 结合可以给出实时聚合。

---

## 5. 实施路线图

### Phase A（2 周）——立即影响

| 周 | 方向 | 关键交付物 | 风险 |
|---|---|---|---|
| 1 | P0: 搜索可观测性 | 连接 `SearchFeedbackRepo`（1 天） + 零结果表迁移 + 路由中的 `record_click` | 低。布线是纯加法 |
| 1 | P0: WebAuthn | 迁移 + 仓储 + `aero-auth` 挑战 + `routes.rs` 挂载 | 中等。web 端 UX 细节（移动端浏览器兼容性） |
| 2 | P1: `pg_stat_statements` 轮询器 | `metrics_tasks.rs` 中的定时器 + Prometheus gauge + Grafana 面板 | 低。已知模式 |
| 2 | P1: 配置级读副本 | `StorageLayer` 引入 + `PoolChoice` + 搜索路由切换 | 低。只是配置变化 |

**Phase A 风险：**
- WebAuthn 的 web 端浏览器兼容性（移动端 Safari 需要特定的 user gesture 触发）
- `pg_stat_statements` 在生产中需要 `ALTER SYSTEM SET shared_preload_libraries = 'pg_stat_statements'` 并重启 PG——这不是零成本的操作。需要在发行说明中说明。

### Phase B（3 周）——体验提升

| 周 | 方向 | 关键交付物 | 风险 |
|---|---|---|---|
| 3 | P1: Onboarding 平台层 | 迁移 + 仓储 + 步骤引擎（Web） + 欢迎消息 | 中等。Web 端步骤引擎需要组件注册表约定 |
| 3-4 | P2: 搜索拼写纠错 | `search_spellfix` 查询 + 零结果触发的集成 | 低。纯 PG 扩展，100 行代码 |
| 4 | P2: 存储生命周期调度器 | `StorageTier` + `LifecyclePolicy` + 定时器 | 中高。需要 S3 IAM 角色 + `S3BlobStore::copy_to(S3, key) → Glacier` |
| 4-5 | P2: Onboarding 工作区管理 | 管理员 UI + 自定义步骤 | 低。数据驱动架构使其易于添加 |

**Phase B 风险：**
- 存储生命周期调度器需要 `S3` 的 `COPY` 操作来改变存储级别——如果你使用的是 S3-compatible 存储（MinIO、Backblaze），Glacier 过渡可能不可用。必须是真实 AWS S3 或兼容 Lifecycle API 的替代品。

### Phase C（3 周）——差异化

| 周 | 方向 | 关键交付物 | 风险 |
|---|---|---|---|
| 5-6 | P3: VOD Phase A（存储 + 生命周期） | `LocalRecordingStore` + `RecordingStore` trait + 生命周期定时器 | 低。把现有的 `vod.rs` 路由胖化为完整的 `routes()` 函数 |
| 6-7 | P3: VOD Phase B（播放器 UI + 进度） | HLS.js 集成 + 进度记忆 + 章节 | 中等。Web 端 HLS.js 缓冲区管理与自动恢复 |
| 7 | P3: 查询回归事件表 | `query_regression_events` + web 端仪表板 | 低。只是记录 + 展示 |

**Phase C 风险：**
- VOD 播放器 UI 对于 web SPA 来说是新肌肉记忆——目前没有其他媒体播放器组件（直播使用 HLS.js 但完全由 `live.js` 控制）。需要一个新的 `vod-player.js` 模块，其中包含对错误恢复的恰当关注。

### 跨阶段关注点

1. **Feature Flags（P1，贯穿所有阶段）**：
   - 每个方向默认被一个 `AERO_FEATURE_X` env gate 保护
   - 在 Phase A 的每个方向中都内联引入，而不是作为一个独立的预先项目
   - 这避免了方向 1（大特性标记项目）的陷阱，同时确保我们没有部署未就绪的功能

2. **测试策略**：
   - Phase A 中的所有方向都应该有集成测试（`tests/` 中），模拟完整的 HTTP 请求
   - 对于搜索，这意味着创建一个房间、索引消息、搜索、验证 `record_click` 被调用
   - 对于 WebAuthn，这意味着模拟 `navigator.credentials.create` 响应（在测试中使用伪造挑战）

3. **部署顺序**：
   - `pg_stat_statements` 查询轮询器需要 DBA 操作（PG 重启）→ 应最先部署，以便其他方向受益于基线监控
   - 搜索可观测性管线只需要应用迁移 → 可随时部署
   - WebAuthn 需要客户端切换（注册 + 登录界面）→ 需要协调的发布

---

## 总结

| 方向 | 我的评估 | 建议 |
|---|---|---|
| WebAuthn | 工作量略高（web 端），方向正确 | P0，Phase A |
| 搜索可观测性 | 工作量低很多（管线已有，死代码） | P0，Phase A，本周开始 |
| 搜索质量（拼写/零结果） | 文档正确 | P1，Phase B |
| Onboarding | 需要建成平台层而非功能层 | P1，Phase B |
| VOD | 过于分散→拆分为 Phase A/B | P3，Phase C |
| 查询性能 | 轮询器容易，回归检测较难 | P1 轮询器（Phase A）+ P2 回归检测（Phase C） |
| Feature Flags | 不要单独立项→内联到每个方向 | 贯穿所有阶段 |

代码库的质量高于预期——大多数问题不是「系统损坏」而是「未接线」。这是架构师喜欢的类型。
