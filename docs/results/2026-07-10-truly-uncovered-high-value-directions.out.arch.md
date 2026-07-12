以下是从资深架构师角度，对 `2026-07-10-truly-uncovered-high-value-directions.md` 的完整分析。

---

# 架构师分析：5 个高价值扩展方向的深度审视

> **基于文档**: `2026-07-10-truly-uncovered-high-value-directions.md`  
> **交叉验证**: 源码扫描 + `AGENTS.md` + ~160 份既有分析  
> **视角**: 架构评估 → 扩展方向 → 接口设计 → 技术选型 → 实施路线图

---

## 一、架构评估

### 1.1 当前架构的既有优势（确认且值得保护）

在评估扩展方向之前，先确认现有架构中**不应被破坏**的部分：

- **事件 DAG 拓扑正确**：`NATS JetStream（事实源）→ run_bus_listener（两阶段解码）→ Hub::fan_out_raw（单 writer bounded mpsc）→ WebSocket`。这是经过 Slack/Discord 验证的生产级模式。任何扩展都**不应**绕过或篡改这一拓扑的核心单调序保证。
- **crate 依赖图健康**：16 个 crate 形成 DAG（叶子 `aero-common` → 中间层 `aero-bus`/`aero-storage`/`aero-auth` → 业务层 `aero-im-core`/`aero-live-*` → 组合 `aero-server`）。新功能应在现有 crate 内追加模块，而非交叉依赖。
- **fail-open 文化已建立**：AI 无 key 退 `HashEmbedder`、Redis 故障 fail-open（限流）、迁移失败进程退出（fail-fast）——这些是系统韧性的基石，扩展时必须保持一致。

### 1.2 现有架构对本文 5 个方向的承载能力

| 方向 | 现有架构能否承载 | 关键缺口 |
|------|---------------|---------|
| 方向一：图像视觉审核 | **能**（可复用 `moderation_bot` 的异步队列模式 + `AiWorker` 预算模型） | 无视觉推理后端抽象；无图像哈希复用；无视频帧提取管线 |
| 方向二：LLM 护栏层 | **部分能**（`AiBackend` 是单一调用点，可插入中间件链） | 无即插式中间件架构；无输入/输出过滤器的 seam；无安全审计表 |
| 方向三：AI 网关层 | **不能**（当前是直筒：`AiBackend` → 固定 provider → 固定模型） | 需要 `Provider` trait 抽象 + 路由策略引擎 + 断路器 + 语义缓存 |
| 方向四：租户生命周期 | **不能**（工作区是静态孤岛，无跨工作区操作能力） | 需要 `parent_id`、合并事务状态机、CLI 工具集、模板序列化 |
| 方向五：团队健康分析 | **部分能**（`message_sentiment` + `analytics.rs` + `org_chart.rs` 数据已就位） | 缺聚合引擎 + 时序表 + 看板 API + 角色隔离（`health_viewer`） |

### 1.3 关键架构债务（影响本文方向的实施）

以下债项若不优先偿还，会成为本文 5 个方向的实施阻力：

- **`AiBackend` 是单体结构**（`anthropic.rs` 内嵌所有调用逻辑）。方向三（网关）必须重构此模块——且重构应是方向二/三/五的共同前提。
- **迁移不可逆**（`down.sql` 全无）。方向四（租户合并）的数据迁移操作需要更细粒度的 schema 版本管理，单向迁移无法支撑复杂的 data migration rollback。
- **无即插式中间件链**。方向二（护栏层）需要一个干净的「输入处理器栈」概念——当前 `AiBackend::answer_question` 是简单的 `request → call_api → response`。若先加方向二，方向三的网关路由可以复用同一中间件 API。

---

## 二、扩展方向分析

### 2.1 方向一：图像与视觉内容安全审核管线

**为什么需要**

合规是平台生存底线而非功能。GDPR/DSA/《网络安全法》对不同介质的内容审核有明确要求。当前对文字有 `AERO_BLOCKED_WORDS` + `AERO_AI_MODERATION`，但对图片/视频**零覆盖**——这是审核体系的结构性缺口。

**核心挑战**

| 挑战 | 难度 | 说明 |
|------|------|------|
| 推理后端选型 | **M** | 云 API（Rekognition/Cloud Vision）低运维但按量付费 + 数据出境；ONNX 本地部署（nsfwjs/CLIP）免数据出境但准确率略低 + GPU 需求 |
| 延迟与异步 | **M** | 同步审核增加上传延迟 200-500ms，必须走异步；但异步意味着违规内容在审核完成前已短暂可见 |
| 视频帧提取 | **L** | ffmpeg 关键帧提取每 5-10s 一帧，长视频（1h+）产生 360-720 帧 → 单视频审核成本 ~$0.5-1.0 |
| 哈希去重 | **M** | PDQ（开源）vs PhotoDNA（需授权）——近似哈希的碰撞率与性能需要 bench；Redis 存储 10M 哈希约 1.5GB |
| 误报/漏报调解 | **M** | 无人工审核队列时，自动删除的误报率不可接受；需 `human_review_queue` 作为兜底 |

**建议的架构变更**

```
当前：
  upload → content_sniff (幻数) → av_scan (ClamAV) → Block::File 持久化 → RoomEvent 扇出

建议：
  upload → content_sniff → av_scan → ╭→ Block::File 持久化（快速路径）→ RoomEvent
                                  ╰→ visual_moderation_queue (bounded mpsc)
                                        → visual_moderation_worker
                                            → 分类器 (云API / ONNX)
                                            → PDQ 哈希存储（Redis 去重）
                                            → 违规 → soft_delete_audited + Deleted
                                            → 待人工 → human_review_queue
                                            → 通过 → 标记 verified_at
```

关键设计决策：
- **异步审核**：上传先放行（低延迟），审核命中后软删 + 广播 `Deleted`。与 `ModerationVerdict::Block` 同路径但不同触发源。
- **复用 `moderation_bot` 预算模型**：有界 mpsc（默认 1024）+ per-ws `KeyedCostBudget` + 全局 `CostBudget`。视觉审核的每条 job 消耗更高权重（如权重 5 vs 文本的 1）。
- **哈希去重作为独立 seam**：`PdqHashStore` trait（Redis 实现 + 可选内存实现），在 `visual_moderation_worker` 内调用。重复图像直接拦截（`identified_via_pdq_hash` 审计理由），不调用分类器。

**预期影响范围**

| 影响 | 范围 |
|------|------|
| 新增 crate | 不需要（视觉审核逻辑作为 `aero-server` 内新模块 + `moderation_bot` 扩展） |
| 新增模型/迁移 | `visual_audit_log` 表（记录每张图/每帧的审核结果、模型版本、耗时） |
| 外部依赖 | 可选：`aws-sdk-rekognition` / `onnxruntime` / `phash` crate |
| 配置项 | `AERO_VISUAL_MODERATION_PROVIDER`（`none`/`aws`/`gcp`/`onnx`）、`AERO_PDQ_REDIS_URL` |
| 性能影响 | 上传路径不受影响（异步）；Redis 增加 ~50 ops/s（10M 图片平台） |

---

### 2.2 方向二：Prompt 注入防御与 LLM 护栏层

**为什么需要**

方向二与方向三存在**实施顺序依赖**：安全护栏应先于网关/路由。理由是：有网关但无安全层 = 你能路由到多个模型，但每个模型都暴露在注入攻击面前。有安全层但无网关 = 单一 provider 但有保护。安全 > 效率。

**核心挑战**

| 挑战 | 难度 | 说明 |
|------|------|------|
| 分层防御的延迟叠加 | **M** | 规则层（~1ms）+ LLM 层（~200-300ms）= 每次 AI 调用增加 200ms+。策略：规则层快速放行明显安全的请求，可疑请求才走 LLM 层 |
| 对抗性绕过 | **H** | 注入攻击会持续演化。规则库需要定期更新 + 重新训练分类器 |
| 间接注入 | **M** | RAG 检索到的历史消息可能含有注入指令。需在 `AiBackend::answer_question` 的 context 构建阶段插入净化步骤 |
| 安全失败的 fail-open | **M** | 安全层自身故障时不阻断 AI。这是现有 fail-open 文化的延续 |

**建议的架构变更**

核心引入**中间件链**模式到 `AiBackend` 的调用路径：

```
当前：
  用户输入 → AiBackend::answer_question → Anthropic Messages API → 原始输出

建议：
  用户输入 → ╭→ InputFilterChain
              │   ├─ RuleFilter（关键词匹配已知注入模式）
              │   ├─ LLMFilter（可疑请求走 Anthropic 验证）
              │   └─ TokenBudgetFilter（max_tokens 上限 + prompt 长度门控）
              ├─ AiProvider（实际的 LLM 调用）
              ╰→ OutputFilterChain
                  ├─ TopicGuard（工作区配置的禁止话题）
                  ├─ PIIFilter（复用 pii_detect.rs 脱敏）
                  └─ ContentFilter（有害分类器）
```

关键设计决策：
- **`Filter` trait**：`trait Filter<Req, Res> { async fn filter(&self, ctx: &FilterContext, input: Req) -> Result<Req, FilterVerdict> }`。请求过滤器修改/拒绝输入；响应过滤器修改/屏蔽输出。
- **两阶段策略**：规则层（正则 + 已知模式库）快速放行 >80% 的安全请求；LLM 层只处理可疑请求（减少延迟和成本开销）。
- **独立预算池**：安全层本身不应消耗租户 AI 配额。设独立 `safety_budget`，不计入 `AiWorker` 的 `CostBudget`。
- **审计日志扩展**：`ai_usage` 表新增 `prompt_hash`、`safety_verdict`、`filter_chain` 列。审计日志是合规文档的核心。

**预期影响范围**

| 影响 | 范围 |
|------|------|
| 新增 crate | 推荐新 crate `aero-guard`（安全层，与 AI 功能分离。独立的安全 crate 不会被 AI crate 的依赖变更影响） |
| 新增模型/迁移 | `ai_usage` 表扩展列；`safety_rules` 表（工作区级注入模式 allowlist/blocklist） |
| 外部依赖 | 无（全部复用现有 Anthropic moderation 端点） |
| 配置项 | `AERO_SAFETY_INPUT_LLM_THRESHOLD`（LLM 层调用阈值，默认 0.6 置信度触发） |
| 性能影响 | 规则层 ~1ms（极低）；LLM 层 ~200ms（仅 ~20% 请求会触发） |

---

### 2.3 方向三（P2 · AI 基础设施）：AI 网关与模型路由层

**为什么需要**

当前 AI 层是架构中最脆弱的单点：`AiBackend` 封装了所有 provider 逻辑，但事实上只支持 Anthropic + Voyage。Provider 故障 = AI 面全瘫。模型不分级路由 = 成本失控。无缓存 = 重复问题每次都花全价。

**核心挑战**

| 挑战 | 难度 | 说明 |
|------|------|------|
| `Provider` trait 设计 | **M** | 不同 provider 的 API 差异大（消息格式、streaming 方式、token 计数、错误码）。抽象必须保留 provider 特异性但不泄露到上层 |
| 语义缓存 | **L** | pgvector 存 `(query_embedding, response, model)`，>0.95 相似度命中。需 **per-model 隔离**（不同模型对同一问题的回答可能不同） |
| 断路器参数调优 | **M** | 5 次失败断路 60s → 恢复后立即再失败 → 频繁开关。需指数退避 + 半开状态探针 |
| 本地模型降级 | **L** | `HashEmbedder` 已证明可实现（1024 维对齐），但 completion 的本地模型（Ollama/llama.cpp）与 Sonnet 的质量差距在复杂推理任务上可达 >30% |

**架构变更方案（选项 A：分层 vs 选项 B：统一）**

| 维度 | 选项 A：分层（推荐） | 选项 B：统一 Provider |
|------|---------------------|---------------------|
| 结构 | `AiRouter`（路由 + 缓存 + 断路器）→ `AiProvider`（单次调用）→ `anthropic.rs`/`openai.rs` | 一个 `AiGateway` trait 包裹路由、缓存、断路、provider 全部逻辑 |
| 优点 | 关注点分离；路由策略可独立于 provider 实现替换；缓存可对不同 provider 独立 | 接口更少；调用方只需感知一个 gateway |
| 缺点 | 层之间有额外空调用；故障排查时跨层追线索 | 测试困难（gateway 涉及缓存+路由+断路器）；扩展新路由策略要改 gateway 本身 |
| 适合场景 | **多 provider 长期演进**（3+ provider） | 只有 1-2 个 provider 的短中期 |

**推荐选项 A**：

```
AiWorker → AiRouter (路由策略选择 provider)
              ├─ SemanticCache (pgvector, TTL=5min) → 命中直接返回
              ├─ CircuitBreakerProvider (5次失败→断路60s→降级)
              ├─ ChainProvider (主 → 备 → 备2)
              ╰─ Provider 实现:
                  ├─ AnthropicProvider (现 anthropic.rs 重构)
                  ├─ OpenAIProvider (新增 openai.rs)
                  ├─ OllamaProvider (新增, 本地模型)
                  └─ HashEmbedderProvider (现 HashEmbedder 包装为 Provider)
```

**预期影响范围**

| 影响 | 范围 |
|------|------|
| 新增 crate | 不需要（`aero-ai` 内新增 `gateway/` 模块） |
| 新增迁移 | `semantic_cache` 表（`query_embedding vector(1024)`, `response text`, `model varchar`） |
| 外部依赖 | `reqwest`（已有）（OpenAI provider 复用现有 HTTP 栈） |
| 配置项 | `AERO_AI_ROUTING_STRATEGY`（`cost_first`/`latency_first`/`capability_first`）、`AERO_AI_CACHE_TTL_SECS` |
| 性能影响 | 路由选择 ~0.01ms（无锁）；缓存命中 ~3ms（pgvector 查询）；断路检查 ~0.001ms（原子计数器） |

---

### 2.4 方向四（P2 · 企业运营）：租户生命周期管理

**为什么需要**

这是 5 个方向中**体量最大但业务场景最窄**的一个。它的价值集中在企业 M&A、组织重组、教育/ MSP 这三类场景。对于只有单一工作区的中小客户，这些功能完全不相关。建议**不先做**，等待有明确客户需求时启动。

**核心挑战**

| 挑战 | 难度 | 说明 |
|------|------|------|
| 数据一致性 | **XL** | 合并操作涉及 ~50+ 表的跨工作区数据迁移。ULID 天生唯一（不冲突），但外键约束（`room_id`→`rooms.id`、`sender_id`→`participants.id`）需要完全重建 |
| 频道名冲突 | **M** | `#general` vs `#general`——自动重命名 `#general (merged)` vs 暂停等待人工决策 |
| 通知风暴 | **M** | 合并批量插入消息触发 NATS 事件 → 用户收到数千条通知。需**静默合并模式**（不触发通知扇出） |
| 模板化 | **L** | 工作区配置（频道、角色、策略）的序列化/反序列化。`rooms` 表的配置字段已有 JSONB，可复用 |

**建议的分阶段实施策略**

```
阶段一（P2 · 2周）：工作区模板
  功能: 导出模板 + 从模板创建工作区
  架构: workspace_config 表 (JSONB 存频道/角色/策略)
  风险: 低（无数据迁移，只涉及新建）

阶段二（P2 · 1周）：工作区归档
  功能: workspaces.status (active / frozen / deleted)
  架构: rooms 级别已有 is_archived，扩展到 workspace 级
  风险: 低（已有类似模式）

阶段三（P2 · 4周）：合并工具 CLI
  功能: aero-cli workspace merge <source> <target> (dry-run + 实际执行)
  架构: 离线 CLI 工具，在 PG 事务中操作。强制合并前完整备份。
  风险: 高（数据不可逆）。必须 dry-run → 验证 → 实际执行三阶段

阶段四（P3 · 6周）：父/子租户
  功能: workspaces.parent_id → 策略继承树、跨子工作区搜索、全局管理员
  架构: 策略合并（父策略 + 子策略覆盖）、跨工作区搜索走 UNION ALL
  风险: 高（架构级变化，影响所有路由的租户隔离逻辑）
```

**预期影响范围**

| 影响 | 范围 |
|------|------|
| 新增 crate | 不需要（`aero-server` 内新增 `workspace_lifecycle/` 模块 + `aero-cli` 扩展） |
| 新增迁移 | `workspaces.parent_id`、`workspaces.status`、`workspace_templates` 表 |
| 外部依赖 | 无 |
| 配置项 | `AERO_WORKSPACE_MERGE_REQUIRE_BACKUP`（默认 true，强制先备份再合并） |
| 性能影响 | 合并操作是离线/低峰期执行，不影响在线路径 |

---

### 2.5 方向五（P2 · AI 差异化）：团队协作健康与情感分析平台

**为什么需要**

这是 5 个方向中**产品差异化价值最高**的——Slack/Teams 都没有原生组织健康分析。但它依赖方向二的安全护栏就位（情感分析是 AI 调用，若无安全层，分析结果可能被注入攻击污染）。

**核心挑战**

| 挑战 | 难度 | 说明 |
|------|------|------|
| 情感分析噪声 | **M** | 短消息（"收到"、"好的"）占消息量的 ~40%，情感分析噪声大。需按消息长度加权聚合 |
| 隐私伦理边界 | **H** | 团队健康分析易滑向「员工监控」。必须：只聚合不溯源、角色隔离（`health_viewer` vs `admin`）、默认 opt-in、数据保留有限（90 天） |
| 离职风险敏感度 | **XL** | 「预测谁会离职」是 HR 高度敏感场景。误报（标记无离职风险的员工）和漏报（漏掉真实风险）都有严重后果。建议 v1 不做离职预测 |
| 跨文化校准 | **M** | 直接表达 vs 含蓄表达在不同文化中情感含义不同。需文化校准或明确标注「此分析基于 English corpus」 |
| LLM 成本 | **M** | 全量消息走 LLM 情感分析成本不可控。策略：规则层（关键词 + 表情符号）先过，仅可疑/复杂消息走 LLM；设每日每工作区上限 |

**建议的架构变更**

```
定时聚合引擎（每 5-15 分钟）:
  messages WHERE sentiment IS NOT NULL
    → GROUP BY (workspace_id, room_id, date_hour)
    → 聚合指标: 情感分布、平均毒性、参与人数、回复率、消息深度
    → UPSERT INTO team_health_snapshots

看板 API（REST）:
  GET /api/workspaces/:id/health          → 聚合健康评分（0-100）
  GET /api/workspaces/:id/health/trends   → 日/周/月趋势
  GET /api/workspaces/:id/health/anomalies → 异常信号列表（3-sigma 偏离）
  GET /api/workspaces/:id/health/teams    → 按 org_chart 团队拆解

异常检测（v1 简单 → v2 复杂）:
  v1: 滑动窗口移动平均 + 3-sigma → 捕获 90% 有意义信号
  v2: Prophet / 轻量时序模型 → 捕获季节性 + 趋势变化

隐私护栏:
  - team_health_snapshots 只存聚合数据，不存个人级情感
  - health_viewer 角色独立于 admin（默认为空，需工作区所有者手动授予）
  - 90 天滚动窗口（可配置）
  - 所有分析数据不可下钻到个人
```

**预期影响范围**

| 影响 | 范围 |
|------|------|
| 新增 crate | 不需要（`aero-server` 内新增 `health/` 模块；聚合逻辑可复用 `AiWorker` 模式） |
| 新增迁移 | `team_health_snapshots` 表（`workspace_id`, `room_id`, `snapshot_at`, `metrics_jsonb`） |
| 外部依赖 | 无 |
| 配置项 | `AERO_HEALTH_SWEEP_SECS`（默认 300，聚合间隔）、`AERO_HEALTH_RETENTION_DAYS`（默认 90） |
| 性能影响 | 聚合 SQL + UPSERT 每 5 分钟写入 ~N rows（N = 工作区数 × 活跃频道数）。中等规模平台 < 1000 writes/interval |

---

## 三、接口设计建议

### 3.1 跨方向的接口抽象

以下抽象是**多个方向共用的骨架**，应先设计再实施：

**`Filter` trait（方向二 + 方向三 共用）**

```rust
// aero-guard 或 aero-ai 基础库
#[async_trait]
pub trait Filter<I, O>: Send + Sync {
    /// 唯一标识（供审计日志引用）
    fn name(&self) -> &'static str;
    /// 处理输入/输出
    async fn filter(&self, ctx: &FilterContext, input: I) -> Result<I, FilterVerdict>;
    /// 该过滤器的权重（用于预算计算）
    fn cost_weight(&self) -> u32 { 1 }
}
```

方向二的输入/输出过滤器实现此 trait；方向三的路由中间件也实现此 trait（如 `ModelRouter` 实现 `Filter<CompletionRequest, CompletionResponse>`）。

**`Provider` trait（方向三）**

```rust
#[async_trait]
pub trait AiProvider: Send + Sync {
    fn provider_name(&self) -> &'static str;
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse>;
    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse>;
    async fn moderate(&self, req: ModerateRequest) -> Result<ModerateResponse>;
    /// 健康检查（breaker 使用）
    async fn health(&self) -> Result<()>;
}
```

**`HashStore` trait（方向一）**

```rust
#[async_trait]
pub trait HashStore: Send + Sync {
    /// 存储图像哈希 + 审核结果，返回是否已存在（命中）
    async fn insert_if_not_exists(&self, hash: &ImageHash, verdict: &Verdict) -> Result<bool>;
    /// 批量查询（视频多帧）
    async fn batch_lookup(&self, hashes: &[ImageHash]) -> Result<Vec<Option<Verdict>>>;
}
```

### 3.2 向后兼容性策略

| 方向 | 兼容策略 |
|------|---------|
| 方向一 | 默认关闭（`AERO_VISUAL_MODERATION_PROVIDER=none`），不影响现有 upload 路径。启用后仅扩展 `moderation_bot` 的 job 类型 |
| 方向二 | 默认降级（安全 filter 为空链，`AiBackend` 行为不变）。启用后在 `AiBackend` 构造函数中添加 filter 参数 |
| 方向三 | `AiBackend` 保留当前构造函数签名作为 deprecated 包装。新代码走 `AiRouter::new(providers, strategy)` |
| 方向四 | 不影响现有工作区操作。新路由全部以 `POST /api/workspaces/:id/{merge,archive,template}` 追加 |
| 方向五 | `message_sentiment` 路由不变。聚合引擎作为新后台定时任务，独立于现有情感标注路径 |

**关键原则**：每个方向的第一行代码都必须是**Feature Flag 保护的**。`AiBackend` 的重构应该在特征门控下进行，新代码走新路径，旧代码继续走旧路径，流量逐步迁移。

---

## 四、技术选型

### 4.1 各方向的技术选型建议

| 方向 | 推荐方案 | 备选方案 | 评估依据 |
|------|---------|---------|---------|
| 方向一（视觉审核） | **ONNX Runtime `ort` crate + nsfwjs 模型** | AWS Rekognition / GCP Vision API | ONNX 免数据出境、免 API 费用、低延迟（GPU 环境下 50-100ms/张）。云 API 的按量计费在平台规模增长后不经济 |
| 方向一（哈希去重） | **`pdqhash` crate + Redis** | Blockhash + PG | PDQ 是 Facebook 开源，近似哈希的鲁棒性和社区成熟度最高。Redis 内存操作适合高频去重 |
| 方向二（护栏层） | **纯 Rust 规则引擎 + 复用 Anthropic Moderation** | 引入 `guardrails-ai`/`rebuff` 等 Python 库 | 不引入 Python 运行时。Anthropic Moderation 端点已存在且质量高（同一 API key） |
| 方向三（语义缓存） | **pgvector 缓存表 + `HashEmbedder`** | Redis + `redisearch` | 现有 pgvector 已部署（无需新基础设施）。`HashEmbedder` 是确定性嵌入（无需 voyage API key），召回率足够 |
| 方向三（LLM 回退） | **Ollama HTTP API（`reqwest` 调用 localhost）** | `llama-cpp-rs`（C++ 绑定） | Ollama 是独立进程，crash 不影响主进程；HTTP API 与现有 Axum 栈一致。`llama-cpp-rs` 增加编译复杂度和 C++ toolchain 依赖 |
| 方向四（合并工具） | **CLI（`aero-cli` 扩展）+ 离线 PG 事务** | Web API + 在线迁移 | 合并操作不可回滚，CLI 可强制 dry-run → 备份 → 执行。Web API 容易被用户误触 |
| 方向五（聚合引擎） | **纯 SQL（窗口函数 + GROUP BY）+ `sqlx` 轮询** | 引入 `apache-flink`/`timely-dataflow` 流处理 | 数据量级不足以支撑流处理框架的运维开销。Postgres 窗口函数对日均百万级消息的聚合完全胜任 |

### 4.2 是否新 crate 的判断

| 方向 | 新 crate？ | 理由 |
|------|-----------|------|
| 方向一 | ❌ 不推荐（`aero-server` 扩展 + `moderation_bot` 增强） | 视觉审核是 moderation bot 的 job 类型扩展，非独立功能域 |
| 方向二 | **✅ 推荐 `aero-guard`** | 安全层是跨 AI/IM/直播的横切关注点。独立 crate 防止安全逻辑被业务 crate 的依赖变更影响。也避免 `aero-ai` 膨胀 |
| 方向三 | ❌ 不推荐（`aero-ai` 内 `gateway/` 模块） | 网关是 AI 层的内部架构重组，不暴露新 API 面 |
| 方向四 | ❌ 不推荐（`aero-server` 内 `workspace_lifecycle/` + `aero-cli` 扩展） | 租户生命周期是业务操作，非基础设施 |
| 方向五 | ❌ 不推荐（`aero-server` 内 `health/` 模块） | 健康分析是业务功能，复用 `aero-server` 现有定时器模式 |

### 4.3 自建 vs 采购决策矩阵

| 功能 | 自建 | 采购 | 决策理由 |
|------|------|------|---------|
| 图像 NSFW 分类器 | ONNX nsfwjs（开源，Apache 2.0） | AWS Rekognition（~$0.001/张） | 无 key 依赖 + 数据不出境 + 前向费用可控。自建质量和成本都优于采购 |
| Prompt 注入防护规则库 | 复用开源 `protect-ai` prompt 库 + 自增规则 | 采购 `rebuff` / `guardrails-ai`（商业版） | 规则库是静态数据，非核心算法。自建维护成本低 |
| LLM provider 路由 | **自建**（核心基础设施） | 采购 `portkey` / `helicone` / `langfuse` | 路由层包含业务逻辑（租户分级、成本策略），外部网关无法感知 Aero IM 的租户模型 |
| 工作区合并工具 | **自建**（必须） | 无成熟商业方案 | 工作区合并涉及 Aero IM 特有的数据模型（~50+ 表），无法套用通用 ETL 工具 |
| 团队健康分析 | **自建**（差异化护城河） | 无成熟商业方案 | 这是产品差异化点，非 commodity。采购意味着放弃数据飞轮 |

---

## 五、实施路线图

### 5.1 优先级与阶段划分

```
P0（优先技术债）
  ├── AiBackend 中间件化（方向二/三的共同前提）
  ├── 迁移 down.sql + 回滚能力（方向四的前置条件）
  ╰── 审计日志表统一扩展（ai_usage + moderation 审计）

P1（立即启动，并行 2 团队）
  ├── Team A: 方向一（图像视觉审核）
  │   ├── Sprint 1: Provider 抽象 + ONNX 集成 + 异步队列
  │   ├── Sprint 2: PDQ 哈希去重 + 人工审核队列
  │   ╰── Sprint 3: 视频帧提取 + 工作区级策略配置
  ╰── Team B: 方向二（LLM 护栏层）
      ├── Sprint 1: Filter trait + InputFilterChain + 规则库
      ├── Sprint 2: OutputFilterChain + RAG 上下文净化
      ╰── Sprint 3: 工作区级安全策略 + 审计日志完备

P2（方向一/二 稳定后启动）
  ├── 方向三（AI 网关层）
  │   ├── Sprint 1: Provider trait + Anthropic/OpenAI/Ollama 实现
  │   ├── Sprint 2: 路由引擎（策略驱动）+ 断路器
  │   ╰── Sprint 3: 语义缓存（pgvector）
  ╰── 方向五（团队健康分析）
      ├── Sprint 1: 聚合引擎 + team_health_snapshots 表
      ├── Sprint 2: 看板 API + 异常检测（简单统计）
      ╰── Sprint 3: Web UI + 角色隔离（health_viewer）

P3（有明确客户需求时启动）
  ╰── 方向四（租户生命周期）
      ├── Sprint 1: 工作区模板（导出 + 从模板创建）
      ├── Sprint 2: 工作区归档（frozen 状态）
      ├── Sprint 3: CLI 合并工具 + 备份 + dry-run
      ╰── Sprint 4: 父/子租户（策略继承 + 跨工作区搜索）
```

### 5.2 里程碑

| 里程碑 | 时间 | 交付物 |
|--------|------|--------|
| M1 | 第 4 周末 | **方向一 MVP**：上传图片/视频后自动 NSFW 检测 + 违规自动删除。人工审核队列可用。PDQ 去重对重复上传即时拦截 |
| M2 | 第 6 周末 | **方向二 MVP**：AI 调用路径的输入注入检测 + 输出有害内容过滤。审计日志完整。工作区级安全策略可配置 |
| M3 | 第 10 周末 | **方向三 MVP**：Anthropic + OpenAI 双 provider，按策略路由（默认成本优先）。语义缓存就位。Anthropic 故障自动降级 OpenAI |
| M4 | 第 12 周末 | **方向五 MVP**：团队健康聚合引擎运行。看板 API 返回健康评分和异常信号。Web UI 展示趋势图 |
| M5 | 第 18 周末 | **方向四 阶段一/二**：工作区模板 + 归档。CLI 合并工具（dry-run 模式） |

### 5.3 风险矩阵

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|---------|
| ONNX 本地模型准确率不达标（方向一） | **中** | **高**（审核误报/漏报不可接受） | 预置 fallback：ONNX 置信度 <0.8 时走云 API 二次验证。或设 `review_threshold`，低于阈值自动进人工审核队列 |
| Provider 响应不一致导致缓存污染（方向三） | **高** | **中**（用户收到不同答案） | 语义缓存 per-model 隔离。不同模型的缓存键包含 `model_family`。缓存命中时标注模型名 |
| 租户合并操作中途失败（方向四） | **中** | **XL**（数据损坏） | 强制 dry-run（`COMMIT` 前验证完整一致性）。备份完整 DB（`pg_dump`）后执行。合并操作设计为幂等（可重试） |
| 团队健康分析被解读为「员工监控」（方向五） | **低** | **XL**（品牌 + 法律风险） | 默认 opt-in + 聚合不溯源 + 90 天自动清除 + 透明文档说明数据用途。外部隐私审计 |
| 方向二拦截合法请求（误杀） | **高** | **中**（用户体验下降） | 输出过滤标记而非删除（退回要求修改）；工作区级 allowlist；安全决定记录 + 申诉入口（`ban_appeals.rs` 可复用） |
| AiBackend 重构破坏现有 AI 路径（方向二/三前提） | **中** | **XL**（AI 面全瘫） | 分支开发 + 全量回归测试（`scripts/truth-check.sh` 级别确保）。特征门控灰度切换 |

### 5.4 依赖关系图

```
方向二（安全） ← 前提 ─ 方向三（网关路由）
    ↓ 复用 Filter trait ↓
方向五（健康分析）── 依靠 AiRouter 的模型路由能力，但不直接依赖安全层

方向一（视觉审核）── 独立，仅依赖 moderation_bot 的预算模式（已有）
    ↓ 可复用人工审核队列 ↓
方向四（租户生命周期）── 独立，仅依赖迁移回滚能力（需先偿还）
```

**关键依赖链**：
1. 方向二 → 方向三：安全层应先于网关层。无安全的网关=多个入口给同一个漏洞。
2. 方向三 → 方向五：健康分析依赖 `AiRouter` 的模型路由能力（简单分析走 Haiku，复杂分析走 Sonnet，成本可控）。若无网关层直接调用 Anthropic 单模型，成本不可控。
3. 方向一独立：可先启动。与方向二/三/五无交叉依赖。
4. 方向四独立但依赖迁移回滚能力：应先偿还迁移技术债（`down.sql`）。

---

## 六、总结

| 方向 | 推荐优先级 | 价值维度 | 体量 | 风险 | 启动时机 |
|------|-----------|---------|------|------|---------|
| 方向一：图像视觉审核 | **P1** | 合规底线 | ~4 周 | 中（模型准确率、视频延迟） | `moderation_bot` 稳定后立即 |
| 方向二：LLM 护栏层 | **P1** | 企业采购刚需 | ~3 周 | 高（误杀风险、对抗绕过） | `AiBackend` 中间件化后立即 |
| 方向三：AI 网关层 | **P2** | 可用性和成本 | ~4 周 | 中（provider 响应不一致） | 方向二之后 |
| 方向四：租户生命周期 | **P2 → P3** | M&A 刚需 | ~8 周 | 高（数据一致性） | 有客户需求时 |
| 方向五：团队健康分析 | **P2** | 产品差异化 | ~4 周 | 中（隐私伦理） | 情感分析稳定运行后 |

**最重要的架构建议**：不要按文档顺序实施。正确的启动顺序是：

1. **先偿还技术债**：`AiBackend` 中间件化 + 迁移 `down.sql`（2 周）。这是方向二/三/四的共同前提。
2. **P1 双线并行**：方向一（团队 A）+ 方向二（团队 B）——它们无交叉依赖，可各自独立交付。
3. **方向三接方向二**：复用 `Filter` trait + `AiBackend` 重构成果。安全层加固后再建网关。
4. **方向五靠后**：依赖情感分析的生产数据积累 + 方向三的模型路由能力。先有数据再分析。
5. **方向四最后**：体量最大、场景最窄。仅在有明确企业客户诉求（M&A 整合、教育批量创建）时启动。
