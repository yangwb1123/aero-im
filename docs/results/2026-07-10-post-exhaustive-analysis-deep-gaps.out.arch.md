以下是对基于代码库验证的真实性审查报告及其修正版本（方向一~五）的架构层面分析。

---

# 架构分析：Aero IM 安全·搜索·产品·媒体·运维扩展方向

## 1. 架构评估

### 1.1 当前架构的优势

**1. 事件驱动骨架成熟，水平扩展路径清晰**
- NATS JetStream 作为跨实例事实源（durable `im.room.*` + ephemeral `live.stream.*`），与本地进程内 `Hub::fan_out_raw`（bounded `mpsc`）解耦，已形成标准的「总线入 → Hub 扇出 → WebSocket 出」管线。
- 每 subject 独立单调 seq（`bus/seq.rs`），at-least-once 投递配合客户端去重，解决了分布式实时系统的核心一致性问题。
- 这一骨架为方向四（VOD 录制事件化）、方向五（查询性能事件化）提供了可直接复用的基础设施——新能力只需在总线上注册 consumer。

**2. 模块边界清晰，crate 依赖方向单一**
- 16 个 crate 的依赖 DAG 严格自下而上（common → bus/storage → auth/signaling → im-core/im-call → live-* → ai/push → server），不存在循环依赖。每 crate 有明确定义的范围。
- `AiBackend` trait（`state.rs`）解耦了 server 层对 `aero-ai` 的具体依赖，使 AI 功能可在编译时开关、运行时退化（无 key → `HashEmbedder` + 启发式 completion）。这种 seam 设计使搜索质量管线（方向二）的 embedding provider 可插拔变得可行。

**3. 安全基线已被低估，但基础扎实**
- 修正后的评估确认：5 个基础安全响应头（X-Frame-Options / X-Content-Type-Options / HSTS / Referrer-Policy / Permissions-Policy）已全量激活。
- Argon2id + RS256 JWT + TOTP + OIDC + PAT + IP allowlist — 认证鉴权栈覆盖全面，比文档原有描述更强。
- 这意味着方向一（WebAuthn）不是在"废墟上重建"，而是在**已加固的认证地基上叠加一层更强的因子**。

### 1.2 关键架构局限性

**局限 1：搜索质量可观测性的完全缺失（方向二的根因）**

当前搜索管线是典型的"黑盒 pipeline"：用户输入 query → 多路并行（FTS + vector + hybrid merge）→ 返回结果。中间没有任何「查询→结果质量」的度量点。

```
    用户 query
       │
       ▼
  ┌────────────────────┐
  │  search_fts        │──→ 无 latency / row count 指标
  │  search_vector     │──→ 无 recall / precision 度量
  │  merge_hits        │──→ 无排名位置分析
  └────────┬───────────┘
           ▼
     结果列表（无解释，无反馈回路）
```

这是一个架构债务。157 张表 + pgvector + pg_trgm 的搜索能力在 IM 产品中属于第一梯队，但**没有反馈闭环的搜索系统无法自我改进**——任何 AI 策略（RAG、推荐、自动建议）都建立在搜索基础上，搜索质量不可观测会级联削弱整个 AI 能力的可信度。

**局限 2：身份认证因子层缺乏扩展点**

当前认证流程（`sessions.rs`）硬编码了密码 + TOTP 两阶段：

```
login(email, password)
  → verify_password
  → if totp_active → verify_totp
  → issue JWT
```

这一逻辑没有 `Authenticator` trait 这样的抽象层，要叠加第三因子（Passkey）需要在 `sessions.rs` 中插入条件判断。这不是不可行，但长期来看，随着认证因子增多（Passkey + SMS + magic link + backup codes），硬编码的条件链会成为维护负担。

**架构债务指标**：因子选择器是 `if totp_active` 这种标量判断，而非可组合的策略链。方向一（WebAuthn）实现时，应同步引入或预留因子编排层，而不是在 `sessions.rs` 里继续堆 `if webauthn_active`。

**局限 3：录制存储层 vs 产品层的心智不匹配**

当前 HLS 录制管线在技术层面是完整的（`HlsWriter` + `FlvToTsConverter` → `.ts`/`.m3u8` on disk），但 `Vod` 结构体本质上只是**元数据指针**（title / duration_secs / hls_path），没有文件操作、没有生命周期、没有访问控制。这意味着：

- 录制内容实际存在于磁盘但无法被用户发现
- 存储成本不可控（无归档/清理策略）
- 回放体验与「产品化」之间隔着一层存储抽象

这不是代码问题，是 **抽象边界划分问题**——录制的「生产」（HLS writer）和「消费」（VOD 产品）之间缺少一个 `RecordingStore` 抽象层。

### 1.3 方向评估的优先级合理性分析

| 方向 | 文档优先级 | 我的评估 | 理由 |
|------|-----------|---------|------|
| WebAuthn/FIDO2 | P1 | **P1** ✅ | 企业采购短名单的门槛能力；无阻塞依赖；独立模块 |
| 搜索质量管线 | P2 | **P1** | AI 策略的基石，搜索质量不可观测则 RAG/AI 均盲目；成本低（8.5 天） |
| 用户 Onboarding | P2 | **P2** ✅ | 留存影响明确，但第一批用户通过企业采购渠道获取，自然流失率低于消费者 SaaS |
| VOD 录制产品化 | P3 | **P3** ✅ | 工作量最大（12 天），直播变现线在路线图中优先级应低于核心 IM 和 AI |
| 查询性能智能 | P3 | **P2** | 157 张表的系统无查询性能监控是运营风险，但短期内可由 DBA 手动补位 |

**我的关键调整建议**：将搜索质量管线（方向二）提升至 P1。理由是：Aero IM 的差异化定位是"AI-Native IM"，而搜索质量恰是 AI 能力（RAG、推荐、自动问答）的底层。搜索质量不可观测 = AI 质量不可观测 = 核心定位无法量化和迭代。这一点应在路线图中明确反映。

---

## 2. 扩展方向（含原文档未深入的技术架构建议）

### 方向 1（P1）：认证因子编排层（Authentication Factor Orchestration）

**为什么需要**：原文档只讨论了 WebAuthn 的单点叠加。但从架构角度，WebAuthn 的引入应同时解决认证因子的**可组合性问题**——未来可能叠加 SMS、Magic Link、硬件 OTP 等。不应每次加因子就改 `sessions.rs`。

**核心挑战**：
1. **因子依赖拓扑**：某些因子是独立的（Passkey 可单独认证），某些是叠加的（Passkey as 2FA 需要密码先通过）。需要声明式策略描述。
2. **认证状态保持**：多因子认证需要跨请求的 session 状态（因子一通过后，等待因子二验证）。当前 TOTP 用临时 `session_token` 实现，但这一机制未抽象化。
3. **降级路径**：WebAuthn 不可用（浏览器不支持/硬件丢失）时透明降级到 TOTP 或恢复码。

**建议架构变更**：引入 `FactorProvider` trait 层：

```
trait FactorProvider: Send + Sync {
    /// 因子标识（passkey / totp / sms / recovery_code）
    fn kind(&self) -> FactorKind;

    /// 开始认证流程：生成 challenge / 发送验证码
    async fn begin_auth(&self, participant: &Participant)
        -> Result<AuthChallenge>;

    /// 完成认证流程：验证断言 / 验证 TOTP 码
    async fn complete_auth(&self, participant: &Participant, challenge: &AuthChallenge, assertion: &[u8])
        -> Result<bool>;

    /// 检查用户是否已配置此因子
    async fn is_configured(&self, participant: &Participant) -> bool;
}
```

`SessionAuthState` 维护「已通过因子集合」的状态，`AuthenticationPolicy` 定义满足认证所需的最小因子集合（"any(Passkey, TOTP) AND Password" 这样的策略表达式）。

**对现有系统影响**：中等。`sessions.rs` 中的 `auth_login` 函数需重构为策略驱动，但 JWT 签发逻辑不变。现有 TOTP 流程可适配为第一个 `FactorProvider` 实现。

**替代方案对比**：

| 方案 | 优点 | 缺点 |
|------|------|------|
| 直接插入 `if webauthn_active` | 改动最小（1-2 天） | 因子增多后不可维护 |
| 引入 `FactorProvider` trait | 架构清晰，扩展友好 | 需重构现有 TOTP 流程（3-4 天） |
| 用现有 `twofa.rs` 模式扩展 | 模式一致 | 但 twofa 模式不支持"唯一因子"场景 |

**建议**：短中期走方案 1（快速交付 WebAuthn），但后期在技术债 backlog 中标记重构为方案 2 的计划。

---

### 方向 2（P1→重定为 P1）：嵌入质量管控与搜索观测层（Embedding Quality & Search Observability）

**核心挑战**（超越文档的深度分析）：

1. **pgvector 索引退化不可观测**：pgvector 的 IVFFlat/HNSW 索引在大量插入后（`embedding_backfill` 批量写入）会衰减，需定期 `REINDEX`。当前无任何索引健康度指标。

2. **Embedding 成本与质量不可分拆**：当前 `AiBackend::embed` 是单一方法，voyage-3 和本地 `HashEmbedder` 的嵌入不可在同一查询中对比。无法回答"voyage-3 的 embedding 是否比 HashEmbedder 带来更好的搜索质量？"

3. **搜索结果可解释性缺失**：用户看到搜索结果但不知道为什么匹配。这不仅是 UX 问题，还是**信任问题**——当用户搜不到期望结果时，"为什么"是调试入口。

**建议架构变更**：

```
 ┌─────────────────────────────────────────────┐
 │              Search Pipeline                │
 │                                              │
 │  query → query_rewrite → retrieval → rank → │
 │                    explain                   │
 │                                              │
 │  中间产出：                                   │
 │  ├─ rewritten_query — 拼写纠正/同义词扩展     │
 │  ├─ retrieval_hits — 每路召回结果携带来源标签   │
 │  ├─ rank_scores — 每结果得分+原因（关键词高亮） │
 │  └─ search_event — 完整事件的 OB 埋点         │
 └─────────────────────────────────────────────┘

 ┌─────────────────────────────────────────────┐
 │         Embedding Health Monitor             │
 │                                              │
 │  gauge: pgvector_index_freshness{index_name} │
 │  gauge: embedding_backfill_lag_seconds       │
 │  gauge: embedding_cost_per_workspace         │
 │  gauge: search_zero_result_rate{workspace}   │
 │  gauge: search_ctr_by_rank{rank}             │
 └─────────────────────────────────────────────┘
```

**对现有系统影响**：小-中。主要是**新增**模块而非重写。`MessageRepo::search_*` 返回签名可扩展（加 `SearchMeta` 而不破坏调用方），搜索埋点可追加到既有 `search_feedback` 表中。

---

### 方向 3（P2）：Onboarding 对架构的影响评估

**技术债分析**（文档未深入）：

Onboarding 看似是纯 UI/UX 工作，但触及两个深层架构问题：

1. **缺少"系统用户"抽象**：欢迎消息 bot、引导 bot 等需要系统级身份。当前 `agent_bot.rs` 已经以 bot 身份发消息，但 onboarding 需要**无参与者的系统操作**（用户注册完成但还没有初始化首页）。需引入 `SystemActor` 概念（非参与者 bot，而是框架级操作者）。

2. **引导进度状态机的放置**：Onboarding 进度（`completed: TEXT[]` / `skipped: TEXT[]`）应该在服务器端维护（用户跨设备同步），但这引入了"用户状态机"的通用需求——目前 `participants` 表无此类状态。建议从 Onboarding 开始建立「用户生命周期状态」模式（`participant_states` 或扩展 `participants` 表），后续可用作账单/激活/留存的统一底座。

**对现有系统影响**：小。Onboarding 是独立新模块，不修改既有路由。`default_channels.rs` 可能需要扩展以支持"注册时自动加入欢迎频道"。

---

### 方向 4（P3）：录制产品化 → 存储抽象 + 媒体生命周期

**核心架构问题**（超越文档）：

当前录制管线缺少的最关键抽象是**存储层 trait**：

```
// 当前（隐式，不可扩展）：
Vod { hls_path: "/data/hls/stream-abc/index.m3u8" }

// 目标：
trait RecordingStore: Send + Sync {
    fn store(&self, stream_id, segments: Vec<Segment>) -> Result<RecordingHandle>;
    fn manifest(&self, handle: &RecordingHandle) -> Result<String>;
    fn delete(&self, handle: &RecordingHandle) -> Result<()>;
    fn signed_url(&self, handle: &RecordingHandle, ttl: Duration) -> Result<String>;
}
```

有了这个抽象：
- 热/温/冷分层变为策略选择（SSD → S3 → Glacier）
- CDN 签名 URL 变为 `signed_url` 方法
- Local vs S3 切换变为 `RecordingStore` 实现选择
- 不修改任何业务代码

**技术难点**：真正挑战不是存储抽象本身，而是**切片边界**。当录制横跨 HLS 分段边界（segment 边界）时，`RecordingStore` 需要保证「一个录制」的所有分段可以被整体引用和删除。当前 `HlsWriter` 生成的 `.ts` 文件是时间分片的，缺少一个录制会话范围的清单。

**建议**：引入 `RecordingSession` 概念——录制开始时生成一个 session ID，HLS writer 在此 session 下产出文件，录制结束时 session finalize 产生完整 manifest。这比录制结束后拼接 `m3u8` 更健壮。

---

### 方向 5（P3→ 建议升级为 P2）：查询性能智能

**这是一个被低估的方向**。文档将其归为 P3（运维成熟度），但考虑到：

- Aero IM 有 157 张表
- `assert_room_access` 自连 5 张表
- 搜索跨 4 表 JOIN
- `ai_jobs` 的 `FOR UPDATE SKIP LOCKED` 是高竞争查询
- AI backfill 批量写向量索引

...这是一个 **高流量 OLTP 系统（IM）叠加了 OLAP 负载（搜索 + 向量 + AI）** 的混合负载场景。Postgres 是中心瓶颈，查询性能退化会直接体现为用户可见的延迟。

**建议的具体监控指标体系**：

| 指标 | 来源 | 告警阈值 |
|------|------|---------|
| Top-10 查询的 mean_exec_time 7d 趋势 | `pg_stat_statements` | 周环比 >20% |
| 顺序扫描表数量（seq_scan > 1000） | `pg_stat_user_tables` | >5 张表 |
| 未使用索引数量（idx_scan = 0） | `pg_stat_all_indexes` | >10 个 |
| AI job claim 等待时间（`NOW() - created_at`） | `ai_jobs` 表 | P50 > 500ms |
| 向量索引查询延迟（`<->` operator） | 应用层 metric | P95 > 200ms |
| 慢查询日志事件率（>100ms 查询占比） | `pg_stat_statements` | >5% of all queries |

**对现有系统影响**：极小。纯新增模块（轮询任务 + Prometheus 指标），不修改任何现有查询代码。

---

## 3. 接口设计建议

### 3.1 关键设计原则

**原则 1：查询结果始终附带可观测性元数据**

每个搜索/列表类 API 的响应应包含 `_meta` 字段（可选，向后兼容）：

```jsonc
// 当前响应（无 meta）：
{ "messages": [...], "total": 42 }

// 目标响应（向后兼容）：
{
  "messages": [...],
  "_meta": {                           // ← 新字段，已部署客户端忽略未知字段
    "search": {
      "execution_time_ms": 45,
      "strategies_used": ["fts", "vector"],
      "zero_result": false,
      "did_you_mean": "aero"           // 可选：拼写建议
    }
  }
}
```

**原则 2：认证链应为可组合策略而非硬编码分支**

当前 `if totp_active` 模式应逐步进化为 `FactorProvider` trait + `AuthPolicy` 组合子：

```
AuthPolicy = AnyOf(Password, Passkey)
           | AllOf(Password, AnyOf(TOTP, Passkey))
           | AllOf(Passkey)
```

这里的权衡是：过早抽象会增加首次实现的复杂度。建议 WebAuthn 首次交付走简单分支，但在代码中留下 `// TODO: extract to FactorProvider` 注释。

**原则 3：存储层 vs 产品层分离**

方向四（VOD 录制产品化）的核心接口决策是：`RecordingStore` trait 不应知道 `Vod`（业务元数据）的存在，`VodRepo` 不应知道 `.ts` 文件在磁盘上的位置。二者通过 `RecordingHandle` 关联：

```
       RecordingStore（文件操作）         VodRepo（业务元数据）
              │                                │
              │  RecordingHandle                │  VodId
              │      ↑                          │     ↑
              └──────┼──────────────────────────┼───┘
                     │                          │
                RecordingSession（录制会话期间，关联两者）
```

### 3.2 是否需要新抽象层

| 领域 | 需要的新抽象 | 理由 | 工作量 |
|------|------------|------|-------|
| 认证 | `FactorProvider` trait | 避免 `if` 链地狱 | 3-4 天（含重构现有 TOTP） |
| 搜索 | `SearchMeta` / `SearchQualityStore` | 可观测性 | 2 天（纯新增） |
| Embedding | `EmbeddingProvider` trait（文档已提） | 模型 A/B 测试 + 质量对比 | 1 天（现有 `AiBackend` 可扩展） |
| 录制 | `RecordingStore` trait | 存储分层 + 生命周期管理 | 2 天 |
| 查询性能 | `PgQueryMonitor` | DB 可观测性 | 1.5 天 |

### 3.3 向后兼容性策略

- **新增字段到现有 API 响应**（`_meta`、`explanation`、`suggestion`）：JSON 客户端忽略未知字段，零迁移成本。
- **认证端点新增而非修改**：`POST /api/auth/passkey/login/begin` 是新端点，已集成的 OIDC/PAT 不受影响。
- **搜索扩展**：`mode` 参数新增值（当前：`fts`/`vector`/`hybrid`/`auto`）。客户端传未知 mode 时 fallback 到 `auto`——这已经是当前的安全设计，无需改动。

---

## 4. 技术选型

### 4.1 是否需要新技术栈

| 方向 | 需要新依赖？ | 建议 | 理由 |
|------|------------|------|------|
| WebAuthn | 否 | **零新增依赖** | WebAuthn 是 Web 平台 API（`navigator.credentials.create/get`），后端只需验证签名。Rust 侧用 `webauthn-rs`（社区成熟度 ★★★★☆）或自行验证（挑战签名验证逻辑约 200 行） |
| 搜索质量 | 否 | **零新增依赖** | 全在 PG 内：`pg_stat_statements` 是 PG 17 内置扩展，查询日志解析用标准 SQL |
| Onboarding | 否 | **零新增依赖** | 纯业务逻辑 |
| VOD 录制 | 可选的 | S3 SDK（`aws-sdk-s3` 或 `rusoto_s3`）用于温/冷存储 | 如果持续使用本地磁盘可零依赖 |
| 查询性能 | 否 | **零新增依赖** | `pg_stat_statements` 是 PG 内置扩展 |

**结论**：五个方向均**不需要引入新的技术栈**。这是架构健康的标志——现有栈（Rust + Axum + Postgres + NATS）覆盖充分。

### 4.2 第三方依赖评估标准

如需新增依赖，建议评估维度：

| 维度 | 最低要求 |
|------|---------|
| 维护状态 | GitHub 最近一次提交 < 6 个月 |
| 社区采用 | GitHub stars > 500 或 cargo 下载量 > 10^5 |
| 安全 | `cargo audit` 无已知漏洞；`unsafe_code` 在可接受范围 |
| 许可证兼容 | MIT / Apache-2.0 / BSD-2/3 |
| 与现有栈集成 | 无需引入新的异步运行时冲突 |

### 4.3 自建 vs 采购决策

| 方向 | 建议 | 理由 |
|------|------|------|
| WebAuthn 后端验证 | **自建（或 webauthn-rs 薄封装）** | WebAuthn 签名验证（ECDSA/RSA）是约 200 行的数学逻辑，无复杂状态。不推荐引入 Ory/Kinde 等服务（它们覆盖更多但集成成本高） |
| 搜索质量观测 | **自建（PG 内置即可）** | 无商业替代品适用于自托管 IM。New Relic/Datadog 做 APM 但无法获取 pg_stat_statements 的 per-query 指标 |
| VOD 存储分层 | **S3 API 兼容层（自建 trait + 可选 S3 SDK）** | MinIO/S3 是标准，无需采购商业存储平台 |
| 查询性能 | **自建（轮询 pg_stat_statements）** | pg_stat_statements + Prometheus 是开源社区标准做法 |

---

## 5. 实施路线图

### 5.1 阶段划分和里程碑

```
Phase 1（6-8 周）—— 安全加固 + 搜索可观测
┌──────────────────────────────────────────────────────────────┐
│ 里程碑 M1（4 周）：WebAuthn 可交付                          │
│  ├─ webauthn_credentials 表 + repo                          │
│  ├─ register/begin+end  + login/begin+end 端点              │
│  ├─ Web 端 Passkey 对话框（navigator.credentials.create/get） │
│  ├─ Passkey 管理 UI（list / delete）                        │
│  └─ 管理员远程吊销凭证（集成 admin_sessions）                 │
│                                                              │
│ 里程碑 M2（6-8 周）：搜索质量可观测                          │
│  ├─ search_queries 表 + 埋点                                 │
│  ├─ 搜索质量指标定时计算（CTR / zero-result rate）            │
│  ├─ embedding_health 监控（滞后/覆盖率/成本）                 │
│  ├─ pg_stat_statements 轮询 + Prometheus 指标                │
│  └─ 慢查询告警规则                                           │
└──────────────────────────────────────────────────────────────┘

Phase 2（6-8 周）—— 留存 + 运维成熟度
┌──────────────────────────────────────────────────────────────┐
│ 里程碑 M3（4 周）：Onboarding 全流程                         │
│  ├─ onboarding_progress 表 + repo                            │
│  ├─ GET /api/me/onboarding + 步骤标记端点                    │
│  ├─ Web 端引导弹窗（3-5 步卡片）                              │
│  ├─ 邀请同事 UI（邮箱输入/CSV 上传）                          │
│  └─ 欢迎 bot 消息                                            │
│                                                              │
│ 里程碑 M4（6-8 周）：查询性能智能                             │
│  ├─ 基准查询集（10-20 条关键 SQL）                            │
│  ├─ 查询计划回归检测 CLI 工具                                 │
│  ├─ CI 集成（migrate 后自动执行）                             │
│  └─ Grafana 看板（合并 Phase 1 的 PG 指标）                  │
└──────────────────────────────────────────────────────────────┘

Phase 3（8-10 周）—— 直播商业化
┌──────────────────────────────────────────────────────────────┐
│ 里程碑 M5（8-10 周）：VOD 录制产品化                          │
│  ├─ RecordingStore trait + S3 实现                            │
│  ├─ 录制生命周期管理定时器（热→温→冷）                        │
│  ├─ VOD 库页面（grid 浏览 + 搜索 + 过滤）                     │
│  ├─ 回放播放器（进度记忆/倍速/章节跳转）                       │
│  ├─ 录制下载（GET /api/vods/:id/download → Signed URL）      │
│  └─ 主播录制管理面板                                          │
└──────────────────────────────────────────────────────────────┘
```

### 5.2 依赖路径与并行策略

```
Phase 1A（安全）← 无阻塞依赖 ─→ Phase 1B（搜索质量）← 共享 PG 监控管道 ─→ Phase 2B（查询性能）
         ↓                                     ↓
Phase 2A（Onboarding）                   Phase 1B → Phase 2B 可在同一 sprint 并行
         ↓
Phase 3（VOD 录制）— 依赖 HLS 管线稳定 + stream_route 就绪
```

**推荐并行策略**：
- Phase 1A（WebAuthn）和 Phase 1B（搜索质量）**可完全并行**——两个团队（或同一团队的两个成员）互不阻塞
- Phase 2B（查询性能）应在 Phase 1B（搜索质量）之后启动，但只需 1-2 周 overlap
- Phase 3（VOD）独立，可在 Phase 1/2 期间就开始设计存储抽象

### 5.3 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|---------|
| WebAuthn 在老旧浏览器不兼容（iOS <14 / Android <9） | 中 | 低 | 前端检测 `PublicKeyCredential`，不可用时降级提示；不阻塞现有密码+TOTP 流程 |
| pg_stat_statements 在托管 PG（如 RDS）上需超级权限 | 中 | 中 | 检测扩展可用性；不可用时报 warning 而非 crash；提供替代方案：应用层 EXPLAIN ANALYZE |
| Onboarding 的市场验证不足——投入后留存未改善 | 中 | 中 | 采用渐进式投资：先做「注册后自动加入欢迎频道+发送欢迎消息」（1 天），验证用户互动率后再投入全流程 |
| VOD 录制存储成本失控 | 低 | 高 | 默认保留期设为 30 天；超过时自动归档到 S3 Glacier（低存储成本）；在主播 dashboard 显示存储用量 |
| 搜索质量指标上线后无人查看 | 中 | 低 | 指标在 Grafana 看板中可见；同时设置告警阈值而非仅"仪表盘"；关键告警（零结果率 >5%）自动推送 webhook |

### 5.4 文档真实性审查反映的深层问题

**我赞同修正版评估的核心结论**：事实错误不影响方向有效性，但暴露了两个过程问题：

1. **分析文档缺乏 CI 门控**：如果 `av_scan.rs` 这样的文件名声明能通过编译检查来验证，这类错误本可避免。建议在 `.md` 文件中嵌入代码引用时，使用 `rg` 或 `find` 命令在 CI 中验证声明存在性。

2. **安全评估的「光」与「影」**：发现安全头已激活是好事（系统比预期更安全），但认证审查中的"乐观偏见"可能忽略了真正的差距——CSRF 保护完全缺失（修正版已承认）。这说明评估者在已激活的头（容易发现）上花了过多注意力，而真正的缺失（CSRF）获取的权重不足。**评估者应当反直觉地检查缺失而非存在。**
