好，这份文档我仔细读完了。以下是我的架构分析。

---

# Aero IM — 架构分析与扩展方向评估

> **角色**：资深架构师  
> **依据**：`docs/requirements/round9-scan-5-extensions-2026-07-10.md` + 代码库已有架构（`AGENTS.md`、15+ crate、157 迁移）  
> **视角**：不是复述文档，而是从架构层面判断每条建议的技术合理性、投入产出比，以及对系统整体质量属性的影响


## 一、架构评估

### 1.1 当前架构的核心优势

**事件总线（NATS JetStream）作为解耦中枢**是这整套系统最正确的架构决策。业务模块只生产事件、不关心消费者——`ImService::publish_room_event` 不感知谁在订阅，`live.rs` 不感知 HLS 切片还是 SFU 扇出。这种设计的直接收益：

- **水平扩缩是天然属性**：多实例共享一个 NATS subject，durable consumer 按 `queue-group` 负载均衡，ephemeral 消费者每实例独立接收。不存在单点瓶颈。
- **故障隔离**：总线消费 bot 崩溃不影响业务模块（事件持久化在 JetStream 里，恢复后继续消费）。只有 `Hub` 扇出故障会影响 WebSocket 投递——但这设计为进程内 bounded mpsc，失败域限定在单进程。
- **审计点清晰**：每类事件都有确定的 subject + 消费链，可通过 NATS 监控看到整个系统的数据流。

**Crate 分层**：`aero-common`（叶子类型）→ `aero-bus` / `aero-storage` / `aero-auth` → `aero-im-core` / `aero-live-*` → `aero-server`（组合）。依赖方向严格向上，不会成环。这对 Rust 工作区的编译时间（虽然长，但不会因循环依赖而爆炸）和模块所有权清晰度都有直接帮助。

**预算控制的 AiWorker**：`KeyedCostBudget(60) + CostBudget(300)` 两级预算 + `Semaphore` 限并发 + `SKIP LOCKED` 分片消费——设计上同时考虑了 fairness（per-ws 不饿死）、全局吞吐、和水平扩展兼容。这不是一次性实现能到位的，看得出来经历过迭代。

### 1.2 当前架构的结构性局限

**局限一：所有数据在同一个存储层，没有 tiering 抽象**

当前 `messages`、`message_history`、`audit_events`、`ai_jobs`、`call_transcript` 全部在同一个 PG 实例上。这是 **monolithic storage**——和微服务 vs 单体一样的权衡问题。当 `messages` 表从千万级增长到亿级时，以下问题会同时爆发：

| 问题 | 表现 | 根本原因 |
|------|------|---------|
| 备份窗口膨胀 | pg_dump 从 5 分钟到 1 小时+ | 备份是纯全量，没有增量基础 |
| 查询性能衰减 | FTS 从 50ms 到 500ms+ | 即使有索引，IO 带宽被全量数据争用 |
| 存储成本线性增长 | 冷数据（365 天前的消息）和热数据同价 | 没有存储分层策略 |
| 迁移风险放大 | 157 个迁移操作都在同一份数据上 | 无备份安全网（文档方向二正好点中这个） |

这不是架构错误——MVP 阶段 monolith 是正确的。但产品体量到了要出备份策略、考虑存储成本的阶段，这个局限就开始痛了。

**局限二：AI 行为完全锁在编译时**

当前所有 prompt 是 `const` / 字符串字面量散布在 10+ 模块中。从架构角度看，这是**控制面（control plane）和数据面（data plane）没有分离**。

- AI 能力的**执行**（调用 Anthropic API、预算控制、模型路由）——这是数据面，实现得很好。
- AI 能力的**配置**（prompt 内容、语气、知识边界）——这是控制面，当前不存在。

这意味着产品团队要改 AI 行为→提 PR→部署→等待。对于 AI-native 产品来说，控制面必须是运行时可配置的。这不是「锦上添花」，而是 AI 运营的基本要求。

**局限三：无备份架构是一个合规/运营风险，也是一个迭代速度瓶颈**

当前「无备份运行」在 MVP 阶段可以接受——数据丢了可以重建，迭代快更重要。但现在：

- 157 个迁移 = 157 个可能出错的机会。没有备份意味着每次迁移出问题，「数据受损」就是最终状态。
- SOC 2 / ISO 27001 里「可恢复性」是一票否决项。审计员会在第一天问备份策略。
- 即使不考虑合规，单节点 PG 故障 = 全量数据丢失。这是运营事故，不是 bug。

### 1.3 架构债务与技术债

**硬编码的 Prompt（设计债务）**：上面已经分析过。这是「过早优化不存在的灵活性和正确性」的典型案例——为了类型安全把 prompt 写成 const，导致运行时灵活性的结构性缺失。修复成本不高（新增 `prompt_templates` 表 + 迁移 + 一次查找逻辑替换），但积累越久，迁移的 prompt 越多，债务越重。

**DraftRepo 的前端死代码（协作债务）**：后端 100% 实现（2 个迁移 + 完整仓储 + REST 路由），前端 0% 接线。这揭示了一个团队流程问题：前后端没有同步交付。后端工程师完成了存储层和 HTTP 层的抽象，但没有产品需求驱动前端消费它。`truth-check.sh` 如果能检测这种「有路由但无前端调用」的死代码，应该加上。

**单 config 文件 vs 环境感知配置（轻度债务）**：`config.example.toml` 不完整，`.env.example` 有但零散。当前开发人员需要手工组合配置。对于 15 个 crate 的项目，配置管理应该自动化——`aero-cli dev setup` 一键完成配置生成。

**迁移无回滚（架构债务）**：`aero-cli migrate` 只支持向上迁移，不支持回滚。`AGENTS.md` 提到「down.sql 不负责恢复数据」——这合理（down migration 的数据恢复是不现实的），但没有备份的兜底就是没有回滚能力。这是备份和迁移回滚之间的架构耦合：备份到位了，回滚自然就安全了。


## 二、扩展方向分析

文档已识别 5 个方向并从产品/运维角度做了分析。我从架构角度评估每个方向的技术合理性、难点和系统影响。

### 方向一：AI Prompt 管理平台（P1 · 控制面与数据面分离）

**为什么需要（架构视角）**：

AI 模块当前的架构把所有 prompt 编译进二进制，意味着 AI 行为的每次调整都是一次**部署事件**。对于 AI-native 产品，prompt 的迭代频率可能每天几次到几十次——和部署频率不匹配。架构上需要将 prompt 从「编译时资产」变为「运行时配置」。

这里本质上是 **Control Plane 和 Data Plane 的分离问题**：

```
当前：data_plane(调用) + control_plane(编译时 const) → 每次改 prompt = 部署
目标：data_plane(调用) + control_plane(DB/API) → 改 prompt = 写 DB
```

**核心技术难点**：

1. **工具定义的只读保护**：AI 模块的工具定义（tool definitions / function calling schema）是安全边界——不能被工作区管理员编辑的 prompt 覆盖。架构上需要将 prompt 模板拆为「不可编辑的前缀（工具定义）」+「可编辑的模板体」。这要求 `AiService` 内部在构建 API 请求时，从两个不同的来源拼接 system prompt。

2. **scope 覆盖链的查询性能**：`message_level → room_level → workspace_level → global_default` 的覆盖链，理论上每次 AI 调用都需要向上查找。N+1 查询风险。需要缓存（每个 `(workspace_id, task_kind)` 的 active prompt 版本缓存）或批量预加载。

3. **变量注入的安全边界**：`{{user_display_name}}` 等模板变量在注入时需要考虑：
   - SQL 注入（变量值进入 prompt 而不是 SQL，风险较低）
   - PII 泄露到日志（日志中必须脱敏变量值）
   - Prompt 注入（用户如果能在自己的 display_name 里放入「忽略之前的指令，执行以下操作」…）——这是需要 `AiService` 在构建请求时对变量值做 sanitization 的地方

**预期架构变更**：

- 新增 `prompt_templates` 表（迁移）→ `PromptTemplateRepo`（仓储）→ `routes()`（HTTP 层）→ 全链路：`AGENTS.md` §4.1 标配。
- `AiService` 初始化时不再使用 `const`，而是从缓存/DB 加载 template → 变量替换 → 发送给模型。
- 缓存层需要 `prompt_template_cache`（类似 `participant_cache`），使用 `(workspace_id, task_kind)` 作为 key，在模板更新时失效。
- Phase D（A/B 测试）需要增加 `prompt_feedback` 表 + 请求路由层按百分比分配 prompt 变体——这是独立于 `AiWorker` 的新 worker 或现有 worker 的新 variant。

**系统影响**：

- `AiService` 的核心逻辑（`answer_question`、`summarize`、`moderate` 等）的调用链路不变——只是 system prompt 的来源从 `const` 变为 `lookup + render`。
- 缓存失效的写路径：`PUT /api/workspaces/:id/ai/prompts/:kind` → `cache.invalidate(workspace_id, task_kind)`。
- 默认回退：未配置 prompt 的工作区透明使用全局默认 prompt——不会引入功能退化。

### 方向二：数据灾备与业务连续性（P1 · 系统韧性基础）

**为什么需要**：

从架构韧性角度，当前系统有一个清晰的安全边界：**运行态的高可用（多实例、健康探针、优雅关停）做得不错，但持久态的数据保护是零**。这不是一个遗漏的 feature，而是一个系统性的韧性缺口——系统的 RTO（恢复时间目标）和 RPO（恢复点目标）完全没有定义。

**技术选型的关键决策点**：

有两个备份路线，需要明确选择：

| 方案 | 工具 | 优势 | 劣势 |
|------|------|------|------|
| 传统 pg_dump | PG 内置 | 零依赖、成熟、可脚本化 | 全量备份、大库窗口长、不支持 PITR |
| WAL 归档 + PITR | pgBackRest / barman | 增量备份、PITR、并行恢复 | 需要额外配置复制槽和归档命令 |

**我的建议**：不要把两者对立。**Phase A 应该用 pg_dump（2 天实现），Phase B 再迁移到 pgBackRest**。原因：
- pg_dump 的 0 依赖、0 配置风险意味着可以今天实现今天就有备份。
- pgBackRest 的配置和运维复杂度（复制槽管理、归档命令配置、S3 存储桶策略）需要一个专门的迭代周期。

**核心难点**：

1. **备份不影响业务**：`pg_dump` 使用 `repeatable read` 隔离级别，对业务读写的影响主要是 IO 争用。在 pgBackRest 中，并行备份的 CPU/IO 控制需要配置 `process-max` 和 `compress-level`。

2. **恢复验证的自动化**：备份只有「恢复过」才证明可用。`scripts/dr-drill.sh` 需要在隔离环境（新数据库、新实例）全量恢复备份并运行测试。这是备份系统真正的工作量所在——不是备份本身，而是恢复验证的自动化。

3. **加密密钥管理**：备份加密密钥不能和备份放在同一位置。建议：
   - 开发/测试环境：环境变量 `AERO_BACKUP_ENCRYPTION_KEY`
   - 生产环境：KMS（AWS KMS / GCP Cloud KMS / HashiCorp Vault）

4. **Redis 备份 vs 重建**：Presence 和 session 数据可以从 PG 重建（`auth_sessions` 表），所以 Redis 备份的优先级低于 PG。但限流计数器的丢失意味着限流状态重置——在恢复窗口内需要 fallback 到宽松限流模式。

**系统影响**：

- `docker-compose.yml` 加 `pg_backup` 服务（独立容器）。
- `aero-cli` 加 `backup` / `restore` / `pitr` 子命令。
- 新增 `scripts/backup.sh`、`scripts/restore.sh`、`scripts/dr-drill.sh`。
- 无核心 crate 变更，无迁移。

### 方向三：消息草稿持久化（P2 · 前后端同步交付问题）

**架构视角**：

这个方向的架构价值不在于草稿本身——而在于它暴露了 **「后端完整实现但前端零接线」这个流程问题**。DraftRepo 不是孤例——`truth-check.sh` 标记了 6 个 UNWIRED 的 builder 方法（`with_relay`、`with_cost_model` 等），同样是后端写了但未接线。

草稿功能本身的架构是健康的：
- 仓储层独立（`DraftRepo`），不需要 join 其他表
- owner-scoped，无鉴权泄露风险
- RESTful 路由（`GET/PUT/DELETE`），语义清晰
- `blocks: Vec<Block>` 复用已有的消息块类型

**核心设计决策**：

草稿的存储粒度——是按房间单个草稿还是多个草稿？当前架构是 single draft per room（`PUT /api/rooms/:id/draft` 直接覆盖），合理。多草稿会增加复杂度（草稿列表 UI、选择草稿回复、草稿命名），不是 MVP 需要的。

**技术难点**：

1. **竞态条件**：设备 A 发送消息 → 设备 B 的草稿未删除 → 设备 B 恢复草稿时看到已发送的内容。解决方案是在 `saveDraft` 中做**版本比较**：
   - `GET /api/rooms/:id/draft` 返回 `{ blocks, reply_to, updated_at }`
   - 前端在渲染草稿时，检查最新消息的 `created_at` 是否晚于草稿的 `updated_at`
   - 如果草稿内容与最新消息内容相同，自动删除草稿

2. **离线支持**：`navigator.onLine` 检测 + localStorage fallback。用户离线时草稿存 localStorage，上线后同步到服务端。这里的复杂性在于合并冲突——用户离线时在设备 A 和设备 B 都编辑了草稿。简化方案：**「最后保存胜出」**，不做版本合并。

**系统影响**：

- 纯前端改动（`web/app.js` + `web/drafts.js`）。
- 后端已有路由，不需要任何 crate 变更。
- 不需要新迁移。

### 方向四：冷热数据分层（P2 · 存储架构演进）

**架构视角**：

这是 5 个方向中**架构影响最大**的一个——因为它触及了系统的存储模型和查询路由。当前是 **monolithic storage**（所有数据在 PG），要演变成 **tiered storage**（热 PG + 冷 S3 + 查询路由器）。

**核心架构决策**：

冷热分层不是一个 feature，是一次**存储架构升级**。有以下实现路线：

**路线 A — 软标记 + 查询隔离（Phase A 方案）**

在 `messages` 表加 `archived BOOLEAN DEFAULT FALSE`，默认查询 `WHERE archived = FALSE`，需要时单独端点查归档数据。

```
优势：零数据迁移、零风险、可随时回退
劣势：PG 数据物理上仍存在（存储成本未解决）
```

**路线 B — 物理卸载到 S3（Phase B 方案）**

后台 worker 将 `archived = TRUE` 的消息序列化为 Parquet/JSON Lines → 写入 S3 → PG 删除。

```
优势：PG 存储成本可控、PG 查询性能不受历史数据影响
劣势：数据迁移风险、归档查询性能下降、跨存储一致性
```

**我的建议**：先走路线 A（软标记），运行观察至少一个完整的数据生命周期（如 90 天），再决定是否走路线 B。原因：
- 路线 A 可以今天实现，风险为零。它解决的是**查询隔离**问题——热查询不受冷数据干扰。
- 路线 B 解决的是**存储成本**问题——但存储成本是慢变量，不需要急着解决。而且 PG 的 TOAST 对大文本的压缩比通常超过 S3 + gzip。
- 路线 B 的归档 worker 需要端到端的正确性保障（事务性读取、版本一致性检查、删除后验证），工作量比预期大。

**核心技术难点**：

1. **归档查询的路由**：并行搜索 PG（热）和 S3（冷）需要应用层的合并排序逻辑——类似搜索引擎的 `merge_hits`。当前已有的 `crate::search::merge_hits`（hybrid FTS/vector 搜索）是一个可以参考的模式。

2. **归档期间的编辑一致性**：消息在归档 worker 读取和写入 S3 之间被编辑 → 归档了旧版本。需要在事务内读取消息 + 所有版本，并在写入成功后检查 `updated_at` 版本号。

3. **法务保全跳过**：`legal_holds` 表已有 `is_held` 标记。归档 worker 必须 `WHERE is_held = FALSE`。这需要 `messages` 和 `legal_holds` 的 join，可能影响归档扫描性能。

4. **回滚保留期**：归档流程必须保留至少一个月的 PG 原始数据（`archived = TRUE` 但不删除），以便发现 bug 时回退。这要求归档 worker 分两步走：先标记已归档、一个月后再物理清理。

**系统影响**：

- 需要新迁移（`messages.archived` + 索引）。
- 新增 `archive_worker`（类似 `AiWorker` 的轮询模式，`FOR UPDATE SKIP LOCKED`）。
- 新增 `S3BlobStore` 的归档存储路径（复用已有 blob 存储，但生命周期策略是 S3 Glacier / 冷存储）。
- 查询路由层需要 aware of 两层存储——`search` 端点需添加 `?include_archived=true`。
- `retention_sweep` 定时器的行为需要修改——从「物理删除/软删除」变为「标记归档」。
- **新外部依赖**：Parquet 序列化需要 `apache-avro` / `parquet` crate，或使用 JSON Lines（零依赖）。

### 方向五：本地开发体验（P2 · 工程效率基础设施）

**架构视角**：

这个方向的独特之处在于它不改变产品功能，而是改变**开发者的认知负荷和迭代速度**。对于一个 143K+ 行 Rust、16 crate、300+ 模块、编译 5-15 分钟的项目，开发体验不是一个舒适度问题——是一个**开发速度的乘数**。

**关键权衡**：

| 方案 | 热重载程度 | 实现复杂度 | Rust unsafe 要求 |
|------|-----------|-----------|-----------------|
| `cargo-watch` + 增量编译 | 进程级重启（3-15s） | 低（CRATES-IO 依赖） | ❌ 无 |
| `cargo-hotpatch` | 函数级热替换 | 高（需要 patch 框架） | ✅ 可能需 unsafe 豁免 |
| 模块拆分 + 测试隔离 | 不解决热重载 | 中（crate 拆分工作） | ❌ 无 |

**我的建议**：选 `cargo-watch` + 增量编译。

- `cargo-hotpatch` 在 `unsafe_code = "forbid"` 的 lint 下需要特殊豁免，而且 Rust 的热函数替换在跨 FFI 边界（tokio task 内部的闭包捕获）时限制很多。实现成本（调试不稳定的热重载）远高于收益。
- `cargo-watch` 的 3-15s 重启对于 HTTP/WS 服务来说是可以接受的——WebSocket 连接断开后自动重连即可，HTTP 请求是幂等的。

**种子数据的架构设计**：

种子数据生成工具的几个架构约束：

1. **幂等性**：`aero-cli dev seed` 在非空数据库上运行时应检测已存在的种子数据并跳过。检测方式：`SELECT 1 FROM workspaces WHERE name = 'Demo Workspace' LIMIT 1`。

2. **版本化**：种子数据应随迁移版本变化——migrate N 新增了一个必填字段，seed 脚本也要能生成该字段的数据。建议种子脚本用独立的版本号或与迁移序号对齐。

3. **隐私安全**：种子数据不能包含真实用户信息。生成的消息内容应来自预设的模板列表（技术讨论、打招呼、bug report 等），不调用 AI 生成。

**系统影响**：

- `aero-cli` 加新子命令（`dev setup`、`dev reset`、`dev seed`、`dev logs`）。
- 新增 `scripts/install-hooks.sh`。
- 新增 `/dev/dashboard` 路径（`AERO_DEV_MODE=1` 门控）。
- 持续集成 `utoipa` 标注（量级：为每个路由 handler 加 `#[utoipa::path]`——约 100+ 路由，不是一个小工作，但可以逐步覆盖）。


## 三、接口设计建议

### 3.1 关键模块的接口设计原则

**1. AiService：从 const prompt 到注入式 prompt**

当前 `AiService` 的各方法签名大致是：

```rust
impl AiService {
    async fn answer_question(&self, participant: &AuthUser, room: &Room, question: &str) -> Result<Answer>;
    async fn summarize(&self, participant: &AuthUser, messages: &[Message]) -> Result<String>;
    async fn moderate(&self, content: &str) -> Result<ModerationResult>;
    // ...
}
```

架构变更后，每个方法需要隐式或显式地获取 prompt 模板：

**选项 A — 隐式注入（推荐）**：`AiService` 内部持有 `PromptTemplateRegistry`，在方法执行时自动按 `(workspace_id, task_kind)` 查找并解析模板。调用方感知不到变化。

```
优势：调用方零改动，与现有代码兼容
劣势：每个 AI 调用多一次缓存查询（~0.1ms，可接受）
```

**选项 B — 显式注入**：调用方传入 `PromptTemplate(workspace_id, task_kind)` 参数。

```
优势：调用方显式控制使用的 prompt
劣势：所有调用点都需要改签名，侵入性大
```

**我的建议**：选 A。AI 模块有 10+ 个调用点，选项 B 的变更量太大了，而且调用方不需要关心 prompt 管理——这是 AiService 的内部职责。

**2. DraftRepo：保持 owner-scoped，无需新抽象**

草稿路由当前已经设计正确：

- `PUT /api/rooms/:id/draft` — upsert（已存在草稿则覆盖）
- `GET /api/rooms/:id/draft` — find
- `DELETE /api/rooms/:id/draft` — delete

不需要新增接口。需要做的只是前端消费这些接口。但需要补充一个批量接口：

- `GET /api/drafts` — 返回用户所有房间的草稿概览（用于侧边栏「有草稿的房间」标记）

后端 `DraftRepo::list` 已经实现。但在前端的 `switchRoom()` 被调用保存当前草稿后，侧边栏也需要更新。所以需要前端加一个 `updateDraftIndicators()` 函数，在每次草稿保存/删除时刷新侧边栏的草稿标记。

**3. 归档查询的路由器接口**

这是需要谨慎设计的新抽象。归档查询的核心问题是：**搜索结果是来自热存储还是冷存储，对调用方应该是透明的**。

**不建议的做法**：让调用方自己查热存储 + 冷存储然后合并。这会把存储细节泄漏到每个使用搜索的地方。

**建议的做法**：`SearchService` 内部增加一个路由层：

```rust
// 内部实现（对调用方透明）
impl SearchService {
    async fn search(&self, query: &SearchQuery) -> SearchResult {
        // 1. 如果 query.include_archived == false → 只查 PG
        // 2. 如果 true → 并行查 PG + S3，合并排序
        // 3. 支持按时间范围自动触发归档数据预取
    }
}
```

这个路由层可以复用 `/server/src/search.rs` 已有的 `merge_hits` 逻辑——当前 hybrid 搜索已经是 parallel merge，归档搜索是类似的模式，只是数据源不同（PG × S3 而不是 FTS × vector）。

### 3.2 是否需要新的抽象层

**需要：AiService 的 PromptTemplateRegistry**

这是一个新的抽象层，职责是：

1. 启动时从 PG 加载所有 active prompt 模板到内存缓存
2. 提供 `get_prompt(workspace_id, task_kind) -> Prompt` 方法，按 scope 覆盖链解析
3. 在 `PUT` 更新 prompt 时自动失效对应的缓存项
4. 提供 `render(prompt, variables) -> String` 模板渲染（变量替换 + sanitization）

这个抽象层应该独立于 `AiService` 还是内嵌其中？**建议独立**。职责不同：
- `PromptTemplateRegistry`：管理 prompt 的存储、加载、缓存、渲染
- `AiService`：管理 AI 调用、预算控制、模型路由

独立后，`AiService` 可以在测试时注入 mock `PromptTemplateRegistry`，不需要真的去查 PG。

**不需要：新的「存储抽象层」**

当前没有统一的存储抽象（各功能通过各自的 `XRepo` 直接操作 PG），归档功能**不需要**先建一个统一的存储抽象再在之上做归档。添加通用抽象层会增加一个中间的适配器层，对于归档功能来说过早了。

归档 worker 可以直接使用 `DraftRepo` / `MessageRepo` 的现有方法 + `S3BlobStore` 的归档路径。不需要统一的 `StorageTier` trait。

### 3.3 向后兼容性

**方向一 — Prompt 管理**：
- 新工作区透明使用全局默认 prompt → 功能退化零
- 已配置 prompt 的工作区在读取时获取自定义行为 → 非破坏性变更
- 现存 AI 调用路径不变（`AiService` 方法签名不变）→ 二进制兼容

**方向二 — 数据灾备**：
- 备份脚本是额外操作，不影响现有服务
- `docker-compose.yml` 加 `pg_backup` 服务是追加，不修改现有服务配置
- `aero-cli backup` 是新命令，不破坏现有 CLI 行为

**方向三 — 草稿持久化**：
- 纯前端改动，后端接口不变
- 未接入草稿功能的旧前端版本不受影响

**方向四 — 冷热分层**：
- 默认查询不返回归档数据（`WHERE archived = FALSE`）→ 现有 API 行为不变
- `?include_archived=true` 是新增查询参数，不破坏现有查询
- 归档 worker 是新增后台进程，不影响现有进程

**方向五 — 开发体验**：
- `/dev/*` 路径仅在 `AERO_DEV_MODE=1` 时可用 → 生产不可达
- `aero-cli dev *` 是新子命令 → 不影响现有 CLI 行为


## 四、技术选型

### 4.1 需要引入的新技术栈或框架

每个方向的依赖评估：

| 方向 | 需要的技术/工具 | 是否新依赖 | 评估 |
|------|---------------|-----------|------|
| 一·Prompt 管理 | 无 | ❌ | 基于 PG 存储 + 已有缓存模式 |
| 二·数据灾备 | pg_dump/pg_restore（内置）、pgBackRest（Phase B） | ⚠️ pgBackRest 可选 | pg_dump 零依赖；pgBackRest 是外部工具不是 Rust crate |
| 三·草稿持久化 | 无（纯 JS） | ❌ | 无新依赖 |
| 四·冷热分层 | Parquet 编码 / JSON Lines、S3 SDK（已有） | ⚠️ Parquet crate 可选 | JSON Lines 零依赖（直接写字符串到 S3）；Parquet 需要 `apache-avro` / `parquet-rs` |
| 五·开发体验 | `cargo-watch`、`utoipa`（可选）、Swagger UI（CDN） | ⚠️ `cargo-watch` 是开发工具，非运行时依赖；`utoipa` 是可选编译时依赖 | `cargo-watch`：`cargo install cargo-watch`，不影响编译；`utoipa`：如果选型它，需要为 ~100+ 路由 handler 添加标注 |

**关键决策：Parquet vs JSON Lines 作为归档存储格式**

| 维度 | Parquet | JSON Lines |
|------|---------|------------|
| 压缩比 | ~4-8x（列式 + zstd） | ~2-3x（行式 + gzip） |
| Schema 演进 | 支持（`schema_evolution`） | 无（每行独立解析） |
| 读取效率 | 列裁剪（只查某字段只读该列） | 全行读取 |
| 工具生态 | Spark/Presto/Redshift Spectrum | jq/`grep`/Python |
| Rust 支持 | `parquet-rs`（Apache 维护，成熟） | 无（直接 `serde_json`） |
| 实现工作量 | 需要定义 schema + Parquet writer | 直接 `serde_json::to_string` + `\n` 拼接 |

**我的建议**：Phase A 和 Phase B 初期用 JSON Lines。原因：
- 零新依赖——`serde_json` 已经在依赖树中。
- 恢复时可以直接 `grep` / `jq` / Python 处理——运维友好。
- 未来要迁移到 Parquet，可以写一个一次性的转换工具（从 JSON Lines 读 → 转 Parquet 写到新路径）。

### 4.2 第三方依赖的评估标准

当前项目依赖管理策略可以从 `Cargo.toml` 看出：

- `unsafe_code = "forbid"` — 排除需要 unsafe 的 crate
- 核心基础设施：`tokio` / `axum` / `sqlx` / `fred`(Redis) / `async-nats` — 都是 Rust 生态的成熟选择
- 直播：`str0m`(WebRTC) / `rml_rtmp` — 特定领域的小众但功能匹配的 crate
- AI：`anthropic` / `voyage` — AI API client

**对新依赖的评估标准**：

提出一个问题清单，给每个候选依赖打分：

1. **是否必需？** 能否用现有依赖 + 少量代码替代？→ 替代不了才加
2. **是否在 forbid list 上？** `unsafe_code` 是否使用？→ 用了就排除
3. **维护状态？** crates.io 最近更新？issue 响应？→ 超过 1 年不维护的存疑
4. **编译影响？** 增量编译时间增加？transitive 依赖数？→ 需要评估
5. **是否替代方案？** 能否用 `cargo` feature flag 做成可选？→ 避免强制依赖

**具体到本文方向**：

- Parquet（方向四）：`parquet-rs` v52+，Apache 维护，`unsafe` 在列式编码热点路径中使用。**需要 `#![allow(unsafe_code)]` 局部豁免**，在 `Cargo.toml` 中用 `[workspace.lint]` override，而非全局 `forbid`。
- `utoipa`（方向五）：`utoipa` v4+，`unsafe_code = "forbid"` 合规，编译时间影响可控（处理宏标注）。**推荐使用**。
- `pgBackRest`（方向二）：外部工具，非 Rust crate。不受项目 lint 约束，但需要在 `docker-compose.yml` 中配置。**推荐使用（Phase B）**。

### 4.3 自建 vs 采购

对于这 5 个方向，基本不需要采购外部服务：

| 方向 | 自建 vs 采购 | 理由 |
|------|------------|------|
| Prompt 管理 | ✅ 自建 | 核心产品差异化——不应依赖第三方 prompt 管理平台 |
| 数据灾备 | ✅ 自建（PG 内置 + 脚本） | PG 备份是成熟领域，不需要采购；但云环境可考虑 RDS 自动备份（托管服务） |
| 草稿持久化 | ✅ 自建（后端已有） | 零成本——只需前端接线 |
| 冷热分层 | ✅ 自建（S3 已有） | 复用现有 `S3BlobStore`，存储层已就绪 |
| 开发体验 | ✅ 自建（CLI + 脚本） | 工具类基础设施，自建的定制度更高 |

**唯一的采购门槛**：如果备份需要合规的加密密钥管理，可以考虑 KMS（AWS KMS 或等效）。但这是基础设施层决策，不是产品层采购。


## 五、实施路线图

### 5.1 最终优先级矩阵

综合**业务价值**、**技术风险**、**实施体量**、**依赖关系**后的评分：

| 方向 | 优先级 | 业务价值 | 技术风险 | 实施体量 | 依赖前置 | 排期窗口 |
|------|--------|---------|---------|---------|---------|---------|
| **一·AI Prompt 管理** Phase A | **P1** | 高（AI 产品化瓶颈） | 低 | S（1 表 + 1 缓存 + 1 替换） | 无 | 立即 |
| **二·数据灾备** Phase A | **P1** | 高（合规一票否决） | 低 | S（2 脚本 + 1 容器） | 无 | 立即 |
| **三·草稿持久化** Phase A | **P2** | 中（每日 UX 摩擦） | 低 | S（纯 JS） | 无 | 立即 |
| **五·开发体验** Phase A | **P2** | 中（工程效率） | 低 | S（CLI 增强） | 无 | 立即 |
| **一·AI Prompt 管理** Phase B-D | P1.5 | 高 | 中 | M-L | Phase A | 4-8 周 |
| **二·数据灾备** Phase B-D | P1.5 | 高 | 中 | M-L | Phase A | 4-12 周 |
| **四·冷热分层** Phase A | P2 | 中（查询隔离） | 低 | M | 无 | 4-8 周 |
| **四·冷热分层** Phase B-C | P3 | 中（存储成本） | 高（复杂） | L | Phase A | 12-24 周 |
| **五·开发体验** Phase B-E | P2 | 中（效率提升） | 中 | M | Phase A | 4-8 周 |

> P1 = 必做（不做的后果不可接受）  
> P1.5 = 短期必做但可容忍短时间不做  
> P2 = 应做（有明确业务/技术价值，但不是硬门槛）  
> P3 = 可做（有价值但执行窗口可延后）

### 5.2 阶段划分与里程碑

**Phase 0 — 立即可做（1-2 周）**：4 个 S 体量任务并行

| 任务 | 工作量 | 执行人 | 交付物 |
|------|-------|-------|--------|
| 方向三·草稿前端接线 | S（~2d） | 前端 | `web/drafts.js` + `app.js` 集成 |
| 方向五·Dev CLI `setup` + `seed` | S（~2d） | 后端 | `aero-cli dev setup`/`seed` 命令 |
| 方向二·备份基线 | S（~2d） | 运维/后端 | `scripts/backup.sh` + `docker-compose` 备份服务 |
| 方向一·Prompt Template Registry | S（~3d） | 后端 | 迁移 + `PromptTemplateRepo` + `AiService` 注入 |

**Phase 1 — 短期（2-4 周）**：

| 任务 | 工作量 | 交付物 |
|------|-------|--------|
| 方向五·Pre-commit 钩子 | S（~1d） | `scripts/install-hooks.sh` + Makefile target |
| 方向二·备份加密 + S3 上传 | S（~2d） | `backup.sh` 加密 + 上传至 S3 |
| 方向一·Prompt 变量注入 | M（~5d） | 模板变量解析器（`{{workspace_name}}` 等） |
| 方向三·草稿列表 UI | M（~4d） | 侧边栏草稿标记 + hover 预览 |
| 方向四·归档标记迁移 | M（~3d） | `messages.archived` 列 + 测试 |

**Phase 2 — 中期（4-8 周）**：

| 任务 | 工作量 | 交付物 |
|------|-------|--------|
| 方向五·API Playground（utoipa） | M（~5d） | `/api/docs` Swagger UI + 核心路由标注 |
| 方向一·Prompt 管理 REST API | M（~5d） | CRUD + 回滚 + 权限控制 |
| 方向二·PITR（pgBackRest） | M（~5d） | WAL 归档 + `aero-cli pitr` |
| 方向四·归档查询扩展 | M（~5d） | `search` 的 `include_archived` 参数 |

**Phase 3 — 长期（8-24 周）**：

| 任务 | 工作量 | 交付物 |
|------|-------|--------|
| 方向一·Prompt A/B 测试 | L（~10d） | `prompt_feedback` 表 + 变体分配 + 数据驱动面板 |
| 方向二·恢复演练自动化 | L（~10d） | `dr-drill.sh` + CI cron + RTO 告警 |
| 方向二·跨区域灾备 | L（~15d） | 异步复制 + patroni 集群 + 故障切换 |
| 方向四·归档管道到 S3 | L（~15d） | `archive_worker` + Parquet/JSON Lines + S3 Glacier |
| 方向五·开发仪表盘 | M（~5d） | `/dev/dashboard` + SSE 日志流 |

### 5.3 风险点和缓解策略

**风险 1：方向一 Prompt 注入安全（中概率 · 中影响）**

工作区管理员可能在 prompt 模板中插入恶意指令，特别是在 `{{变量}}` 注入时逃逸 system prompt。

缓解：
- 工具定义段标记为只读前缀（不可编辑部分）
- 变量值 sanitization：移除可能的控制字符和异常 Unicode
- CR 评审：Prompt 模板编辑应需要 workspace admin 角色（已有 `member_role` 守卫）
- 审计：编辑 prompt 写入 `audit_events`

**风险 2：方向四归档一致性（低概率 · 高影响）**

归档 worker 在读取消息和写入 S3 之间，消息被编辑/删除，导致归档了不一致的历史数据。

缓解：
- 在事务内读取消息 + 所有版本（`message_history`）
- 写入完成后检查 `updated_at` 版本号——如果变化则丢弃 S3 中的归档，重新读取
- 两步删除：先标记 `archived = TRUE`，30 天后再物理删除

**风险 3：方向四归档查询性能（高概率 · 中影响）**

从 S3 查询归档数据的延迟（50-200ms）高于 PG 查询（1-10ms），且并行搜索 + 合并排序的行为可能导致资源消耗峰值。

缓解：
- 归档搜索结果设置 RT 上限（如 5s 超时）
- 分页归档搜索结果时使用游标（不适用 `OFFSET`）
- 按月份预取：用户浏览特定月份的历史时，后台触发该月份的归档数据预加载
- 在 UI 上区分热数据搜索结果和冷数据搜索结果（分别显示计数和延迟）

**风险 4：方向五种子数据与迁移版本脱节（中概率 · 中影响）**

迁移 N 新增了一个 `NOT NULL` 字段，种子数据生成脚本不知道，在插入时抛出错误。

缓解：
- 种子数据脚本版本化（与迁移序号对齐或独立版本号）
- `aero-cli dev seed` 在运行前检查种子版本是否匹配当前迁移版本，不匹配则报错提示
- CI 中集成种子数据验证——在迁移后的数据库上运行 seed 并检查是否成功

**风险 5：多方向并行执行时的 merge 冲突（高概率 · 低影响）**

每个方向都可能涉及 `routes::build()` 的 `.merge()` 链和 `server/src/lib.rs` 的 `pub mod` 申明。多 agent 并行时，这些共享文件是冲突点。

缓解：
- `AGENTS.md` §4.1 的方法：各自 crate 内实现 + 集成时手接共享文件
- 冲突时保持串行集成（即使开发并行，merge 的冲突解决必须串行）
- Git worktree 隔离 + 最终 `cargo check --workspace` 验证

**风险 6：方向四迁移到 Parquet 的可行性（低概率 · 中影响）**

如果 Phase C 决定走 Parquet 路线，`parquet-rs` 的 `unsafe` 代码需要在 `Cargo.toml` 中配置局部豁免。

缓解：
- 工作区 Cargo.toml 中 `[workspace.lint.rust.unsafe_code]` 设置为 `"deny"`，但通过 crate 级别的 `[lints.rust]` 局部 override
- 封装 Parquet 读写操作在一个独立的 crate 中（如 `aero-archival`），隔离 unsafe 边界
- 或选择 JSON Lines（零 unsafe 风险）作为长期方案

**风险 7：方向二跨区域灾备的复杂度（高概率 · 高影响）**

这是 5 个方向中架构影响最大的任务——涉及跨区域网络延迟、数据一致性模型、故障切换的自动化。

缓解：
- **不要在 Phase A/B 中考虑跨区域**。先完成单区域的备份 + PITR + 恢复演练。
- 跨区域灾备是 Phase D（12-24 周后），时间充裕。
- 评估方案：不一定要 patroni（复杂）。考虑 **RDS Multi-AZ / Cloud SQL 高可用**（如果部署在云上）或 **logical replication 的异步备库**（自建托管）。


## 总结

这 5 个方向的识别质量很高——它们不是随机拼凑的「改进建议」，而是覆盖了系统在 **产品运营面（AI Prompt）、合规面（备份）、UX 面（草稿）、成本面（冷热分层）、工程效率面（Dev 体验）** 的代表性缺口。从架构角度看：

- **方向一和方向二应该立即启动**。前者是产品差异化的控制面缺口，后者是运营韧性的底线缺口。两者都不需要大的架构变更，都是低风险、高价值的 S/M 体量任务。
- **方向三是我最建议「立即做」的**。不是因为它的架构价值最高，而是因为后端已实现但前端零接线——这是投入产出比最高的选择（2 天 JS 工作就能交付一个完整的用户体验改进）。
- **方向四的 Phase A（归档标记）应该比文档建议的更早做**。迁移 `messages.archived` 列是一个零风险的架构准备，做了之后热表查询就不受历史数据干扰了。归档管道的 Phase B/C 可以等。 
- **方向五的 Dev CLI 增强是每个人都想要但没有优先级推动的**——`aero-cli dev setup` + 种子数据对简化新贡献者入职和日常开发循环有直接提升。建议作为「周五任务」这种低压力、低门槛的方式推进，不需要排入 Sprint。
