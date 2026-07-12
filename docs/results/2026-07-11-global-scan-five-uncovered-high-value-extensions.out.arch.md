以下是我从架构师视角，对您分享的文档（5 个新方向：WebAuthn / Search Quality / Onboarding / VOD / Query Performance）进行的分析。

---

# 架构师分析：五个未覆盖高价值扩展方向

> **分析基线**：您共享的分析文档（交叉验证 5 方向 vs 既有 `post-full-scan` 的 cache/NATS/web-sec/voice/policy 方向，确认独立）+ 源码参考（`AGENTS.md`、`ROADMAP.md`、`design-spec`）+ 既有 `docs/analysis/` 实施计划。

---

## 核心判断

**五个方向全部有效，但优先级和实现路径需要重新排序**。它们代表了从「功能完整」到「平台成熟」的四类阶梯——认证安全基础、数据质量纵深、用户体验闭环、基础设施可运维——每个阶梯的 ROI 窗口和架构风险不同。

---

## 一、架构评估

### 1.1 当前架构的优势（与新方向相关的）

| 优势 | 对应新方向的价值 |
|------|----------------|
| **认证体系已模块化**：`aero-auth` crate 独立，JWT（RS256）+ Argon2id + TOTP + PAT + OIDC 共存在统一 extractor `AuthUser` 下 | ① WebAuthn 可以在一处插入，`AuthUser` extractor 加新的 `Passkey` variant |
| **event-sourcing 骨架完整**：NATS 持久化 + per-subject seq + Hub 进程内扇出 | ② Search Quality 的索引事件源（message_created/edited/deleted）→ embedding pipeline 已经清晰 |
| **参与者和房间模型已抽象**：`ParticipantKind`（Human/Agent/Bot）、`RoomKind`、RBAC | ③ Onboarding 的「首次登录」检测点已有多个可挂入点（`POST /api/auth/login`、`GET /me`） |
| **BlobStore 可插拔**：`LocalFsBlobStore` / `S3BlobStore` 通过 `blob_store_from_env` 选择 | ④ VOD recording 可在活存储基础上加 `RecordingStore` trait 扩展，不走 `Block::Voice` 的附件机制 |
| **可观测性基础设施就位**：Prometheus metrics + OTLP + `aero-common/metrics.rs` 常量注册 | ⑤ Query Performance 可在 `slow_query_log` 和现有 gauge sampler 框架上加层 |

### 1.2 关键架构约束（AGENTS.md §4.2 硬规则）

分析需尊重的不可逆约束：

1. **`assert_room_access(participant, room)` 是唯一租户守卫**——所有新路由都必须过此门
2. **`tag="kind"` enum 陷阱**——新 `Block`/`RoomEvent`/`StreamEvent` variant 内不能有名为 `kind` 的字段
3. **迁移编译期嵌入**——加新表必须 `cargo build` 再 migrate
4. **`unsafe_code = "forbid"`**——WebAuthn 的 crypto 操作必须走纯 Rust 库
5. **AI 无 key 退化**——搜索质量管线必须能在无 Voyage/无 Anthropic key 时回退

### 1.3 当前架构的局限性（五个方向的共同挑战）

| 局限性 | 被哪个方向最先撞到 | 严重程度 |
|--------|-------------------|---------|
| **无认证因子插件体系**——TOTP 是硬编码在 `twofa.rs` 的一个分支 | ① WebAuthn 需要新增第二因子，当前缺乏 `MfaProvider` trait | **高**：每次加新因子都改 `AuthUser` |
| **搜索管线无事件源驱动**——embedding 由 `embedding_backfill` 定时器（300s）轮询扫描，非实时 | ② Search Quality 需要实时索引 | **高**：定时扫描的延迟不可接受 |
| **无用户状态机（signup→activated→onboarded→active）**——`auth/login` 没有 `first_login` 概念 | ③ Onboarding 没有可挂入的 hook | **中**：可新建 `user_lifecycle` 表 |
| **直播流的录制产物只有 HLS 片段**——无 `Recording` 记录表、元数据、生命周期 | ④ VOD 需要新数据模型 | **中**：可从 `StreamEvent::Status{Ended}` 总线事件驱动 |
| **DB 查询无 explain 分析收集**——无 `pg_stat_statements` 的 Prometheus exposure | ⑤ Query Performance 需要被动采集 | **低**：纯增强层 |

---

## 二、扩展方向分析（架构详细设计建议）

### 方向一（P0）· WebAuthn / FIDO2 / Passkey——补齐认证因子插件体系

#### 为什么需要（业务 / 技术价值）

- **企业合同门槛**：SOC2 / ISO27001 要求 phishing-resistant MFA（TOTP 是 something-you-have，但可钓鱼；WebAuthn 是 phishing-proof）
- **UX 提升**：Passkey 在 macOS/iOS/Android 的免密体验是「登录即 Face ID/Touch ID」，比 TOTP 输入快得多
- **安全纵深**：当前 TOTP 是唯一第二因子。TOTP 密钥存储在服务器（`twofa_secrets`），服务器被攻破则可伪造。WebAuthn 私钥永远在客户端

#### 核心技术难点

| 难点 | 架构决策 | 选项与权衡 |
|------|---------|-----------|
| **WebAuthn 依赖硬件/平台 attesation 验证** | 集成 `webauthn-rs` crate（纯 Rust）vs 自写 | ✅ `webauthn-rs`（3K GitHub stars，active maintenance，支持 Passkey + CTAP2） |
| **跨设备 Passkey 同步** | Apple/Google/MS 各有自己同步协议，服务端不可控。服务端只需 `credential_id` + `public_key` + `counter` | 不可控部分接受，服务端只管验证签名的 RSA/ECDSA 公钥操作 |
| **TOTP 与 WebAuthn 的共存逻辑** | `AuthUser::mfa_verified` 标记需要扩展为 `Vec<MfaMethod>` | 新建 `MfaProvider` trait：`trait MfaProvider { fn verify(&self, participant, challenge) -> Result<bool> }` 抽象 |
| **U2F / CTAP2 的计数器增量化** | 防克隆：每验证次数 `counter` 需 ≥ 上次记录值 | 存储 `credentials.counter`，验证时检查单调递增 |
| **注册与登录的 UX 差异** | 注册 = `credential.create()`，登录 = `credential.get()` | 两个端点 `POST /api/auth/webauthn/register/{begin,finish}` 和 `POST /api/auth/webauthn/login/{begin,finish}` |

#### 预期架构变更

```
现状：AuthUser extractor → JWT decode → verify → (optional TOTP)
未来：AuthUser extractor → JWT decode → verify → vec![TOTP, WebAuthn, ...] 
                                          ↑ new MfaProvider trait
```

具体变更：

1. **`aero-auth/src/mfa.rs`**（新文件）：`MfaProvider` trait + `MfaMethod` enum（`TOTP | WebAuthn`）
2. **`aero-auth/src/webauthn.rs`**（新文件）：`WebAuthnProvider` impl `MfaProvider`，包裹 `webauthn-rs` 的 `Webauthn` 实例
3. **`aero-storage/src/webauthn_credentials.rs`**（新仓储）：`webauthn_credentials` 表 `(pid, credential_id, public_key, counter, device_name, created_at)`
4. **`aero-server/src/twofa.rs`** 重构：`verify_totp` → use `mfa_providers.verify(pid, challenge)`
5. **迁移** `NNNN_webauthn_credentials.sql`
6. **路由** `POST /api/auth/webauthn/*`，挂到 `routes/auth.rs`

#### 对现有系统的影响

| 影响面 | 强度 | 说明 |
|--------|------|------|
| `AuthUser` extractor | **中** | 需要从 `mfa_verified: bool` 改为 `mfa_methods: Vec<MfaMethod>` |
| `twofa.rs` | **中** | 需要支持 `verify_totp` 与 `verify_webauthn` 的并集 |
| `AuthUser::require_mfa()` 等守卫 | **小** | 只要任一因子验证通过即可 |
| 现有 TOTP 注册用户 | **无** | 向后兼容：未注册 WebAuthn 的用户仍走 TOTP |

#### 边界情况

- **共享/公共电脑**：Passkey 注册后设备不再安全。需「信任此设备？」gating step——注册时要求额外 TOTP 确认或管理员批准
- **丢失 Passkey 设备**：需要恢复码（backup codes）回路。Passkey 是私钥，丢失即无法登录。应提供每次注册时生成 8 个一次性恢复码
- **iOS Safari `autocomplete="webauthn"`**：支持条件 UI（Conditional Mediation），但需要检测 `isConditionalMediationAvailable`

---

### 方向二（P1）· Search Quality Pipeline——从轮询驱动到事件驱动的检索质量系统

#### 为什么需要

- **当前 embedding 是 300s 定时轮询**（`embedding_backfill`），不符合实时搜索的预期。用户发消息→立刻可被检索的延迟是搜索可信度的关键
- **无检索质量评测回路**：当前 RRF 融合（FTS + vector）是固定权重，无 NDCG/MRR 追踪，无法证明「vector search 比 FTS 好」或「参数调优有效」
- **`search_click_events` 表（迁移 0133）已存在但从未使用**——这是比建新表更低的果实

#### 核心挑战

| 挑战 | 架构决策 |
|------|---------|
| **实时索引 vs 批处理** | 消息发布路径（`ImService::send_message`）同步触发 embedding 创建 → 延迟劣化。应采用异步：NATS `ai.queue.embed` + AiWorker 消费 |
| **已有 `search_click_events` 表的启用** | 无使用成本但存在陷阱：`search_click_events` 列 schema 可能偏离当前搜索路由。需先读迁移 0133 确认列定义 |
| **NDCG/MRR 计算需要人工标注** | 自动化：用 `search_click_events` 作为隐式反馈（点击=相关），跑离线 `aero-cli eval-search` 命令 |
| **查询改写（query rewriting）** | 用户输入 `"上周的会议记录"` → 改写为 `"会议 记录 上周 {time_filter}"`。需集成 AiWorker 的 `answer` 路径但提示不同 |

#### 预期架构变更

```
现状：定时器(300s) → SELECT messages WHERE embedding IS NULL → enqueue Embed jobs
未来：ImService::send_message → publish RoomEvent → 事件驱动 → enqueue Embed jobs
      (无需定时器，被消息事件驱动)
```

1. **消息发布路径加 enqueue 钩子**：`ImService::publish_room_event` 对 `Message` variant 触发 `AiWorker::enqueue(Embed{message_id})` 而非等待 300s 轮询
2. **`search_click_events` 表启用**：`SearchRepo::record_click()` 在 `GET /api/rooms/:id/search` 结果点击时调用
3. **离线评测 CLI**：`aero-cli eval-search --ndcg --mrr --dataset N`，读取 `search_click_events` 计算
4. **查询改写路径**：`POST /api/rooms/:id/search?mode=auto` 加前置 `AiService::rewrite_query(query)` 步骤（退化为原 query 无 key）

#### 对现有系统的影响

| 影响面 | 强度 | 说明 |
|--------|------|------|
| `message/orig.rs::SendMessage` | **小** | 加一行 `enqueue_embed` 调用（NATS publish，异步非阻塞） |
| `embedding_backfill` 定时器 | **中** | 从主调度降级为「兜底定时器」——只扫漏网的消息（新路径挂了才激活） |
| `search.rs` 路由 | **小** | 加 `rewrite_query` 前处理步骤，opt-in |
| `search_click_events` 表 | **中** | 需要 migration 修复可能偏离的 schema + 加索引 |

#### 边界情况

- **重投（redelivery）产生重复 embedding 任务**：`enqueue_embed` 需要幂等键（`message_id + kind=Embed`），AiWorker 的 `enqueue_unique` 已有 dedup
- **历史消息不触发实时索引**：backfill 定时器继续处理历史消息，两路径不冲突
- **查询改写在 AI key 未配时退化**：`rewrite_query` 返回原 query，搜索走既有 `mode=auto`（FTS + vector 融合）

---

### 方向三（P1）· User Onboarding——从「能登录 = 完成注册」到「激活引导」

#### 为什么需要

- 当前 `POST /api/auth/register` 成功后直接返回 JWT——用户跳转到全功能但空白的 UI，无任何引导
- SCIM 入站供给的用户第一次登录时和自注册用户不可区分——《无 `signup_method` 标记》
- 产品留存的基本事实：任何协作工具的 7-day retention 高度依赖于「第一天的引导质量」——无引导 = 放弃

#### 核心难点

| 难点 | 方案 |
|------|------|
| **如何判定「第一次登录」** | `participants` 表加 `first_login_at TIMESTAMPTZ` 或 `onboarding_completed_at`，`login` 路径 `COALESCE` 判断 |
| **SCIM 用户 vs 自注册用户的引导差异** | SCIM 用户跳过 workspace 创建引导（已有 workspace），走频道发现引导；自注册用户走 workspace 创建引导 |
| **多 workspace 用户的重复引导** | 加入第二个 workspace 时不再显示初次引导，只显示 workspace-welcome |
| **引导进度持久化** | `onboarding_state` 表 `(pid, step, completed_at)`，允许用户中断后恢复 |

#### 预期架构变更

1. **`migrations/NNNN_onboarding.sql`**：`participants` 加 `first_login_at` / `onboarding_method`（`self_registered | scim | invitation | oidc_jit`）+ `onboarding_state` 表
2. **`aero-server/src/onboarding.rs`**（新文件）：`OnboardingState` enum（`Welcome | WorkspaceCreate | ChannelDiscover | InviteMembers | AISummary | Complete`），`POST /api/me/onboarding/step` 推进
3. **`aero-server/src/routes/auth.rs`** `login` 响应加 `onboarding_required: bool` 和 `onboarding_step: Option<String>` 
4. **`web/onboarding.js`**（新文件）：分步引导 UI
5. **`aero-storage/src/onboarding_repo.rs`**：CRUD

#### 对现有系统的影响

| 影响面 | 强度 | 说明 |
|--------|------|------|
| `POST /api/auth/login` 响应 | **小** | 加两个可选字段 |
| `GET /api/me` | **小** | 加 `onboarding_step` 字段 |
| `participants` 表 | **小** | 加列，`NOT NULL DEFAULT` 兼容 |

#### 特化边界情况

- **SCIM 用户**：`signup_method = 'scim'` → `first_login` 触发 channel discover 而非 workspace create
- **OIDC JIT 用户**：`signup_method = 'oidc_jit'` → 同 SCIM
- **Invitation 链接注册**：`signup_method = 'invitation'` → 直接进入已邀请频道，引导跳过 channel discover
- **恢复码创建窗口**：引导中应包含 Passkey/恢复码创建建议（依赖方向一完成）

---

### 方向四（P2）· VOD Recording Productization——从 HLS 片段到持久化录制管理

#### 为什么需要

- 直播流结束后的产物是 `.ts` / `.m3u8` 片段，**没有任何 recording 记录/元数据/生命周期管理**
- 当前回看只能由观看者端实时 HLS 播放，无法「流结束后 7 天重播」
- 现有 `S3BlobStore` 基础设施完整（`aero-storage/src/s3_blob_store.rs`），可以作为录制存储后端模板

#### 核心难点

| 难点 | 方案 |
|------|------|
| **录制生命周期** | 同 `messages` 的 `deleted_at` 软删 + `retention_days` 策略 |
| **HLS 片段归并** | 一场直播 N 个 `.ts` 片段需要聚合为一条 `Recording` 记录 |
| **录制开始/停止的触发时机** | 用 `StreamEvent::Status{Live→Ended}` 驱动——开播建 Recording，结束 finalize |
| **存储成本控制** | 每个 Recording 独立 TTL；`expired` 状态触发 `BlobStore::delete` |

#### 预期架构变更

1. **`migrations/NNNN_recordings.sql`**：`recordings` 表 `(id, stream_id, title, duration_secs, retention_days, status: {recording, finalizing, ready, archived, deleted}, hls_playlist_path, thumbnail_path, metadata JSONB, created_at, deleted_at)` + FK to `streams`
2. **`aero-storage/src/recording.rs`**：`RecordingRepo` CRUD + `finalize()` / `expire()`
3. **`aero-live-core/src/recording.rs`**：`RecordingManager` trait，`LiveRecordingManager` 实现（监听 `Status{Live}`/`Status{Ended}` 总线事件）
4. **路由**：`GET /api/streams/:id/recordings` / `GET /api/recordings/:id/hls` / `DELETE /api/recordings/:id` / `PATCH /api/recordings/:id/retention`

#### 对现有系统的影响

| 影响面 | 强度 | 说明 |
|--------|------|------|
| `golive_bot` → `Status{Live}` 路径 | **小** | 加一行 `recording_manager.on_live_start(stream_id)` |
| `StreamEvent::Status{Ended}` 消费路径（需确认是否存在） | **中** | 当前直播结束事件可能不可靠。需要补 `Ended` 事件的生产方 |
| HlsWriter 的段写入路径 | **小** | 录制记录只做元数据，不影响 HLS 写入路径自身 |

#### 边界情况

- **录制中断恢复**：流意外断开（网络问题）→ `Status{Ended}` → `Recording::finalizing` → 重连后 `Status{Live}` 但录制作为新记录（不要 append 到旧 recording）
- **录制大小限制**：超长直播（>24h）分段：每 12h `finalize` + start new recording
- **thumbnail 生成**：录制结束时取首帧（依赖方向四的截帧基础设施的复用以避免重建）

---

### 方向五（P2）· Query Performance Intelligence——从被动慢查询到主动优化循环

#### 为什么需要

- 无 `pg_stat_statements` 收集意味着生产中的慢查询只有 `log_min_duration_statement` 文本日志，无聚合/趋势/报警
- 无部署前查询计划回归检测——迁移加索引时无人发现查询计划劣化
- Prometheus 已有的 `DB_POOL_IN_USE` / `DB_POOL_ACQUIRE_DURATION` 指标但不覆盖查询级别

#### 核心难点

| 难点 | 方案 |
|------|------|
| **`pg_stat_statements` 的 Prometheus exposure** | 新 task `query_metrics_collector` 每 30s 查询 `pg_stat_statements` → 设 gauge |
| **`pg_stat_statements` 依赖安装** | `migrations/NNNN_enable_pg_stat_statements.sql`：`CREATE EXTENSION IF NOT EXISTS pg_stat_statements`（幂等） |
| **慢查询告警** | `deploy/prometheus/alert_rules.yml` 加 `QueryMeanLatencyHigh` / `QuerySharedReadHigh` |
| **部署前查询计划回归** | CI pipeline 步骤：从 `PG_QUERY_MIRROR_URL` 获取生产环境的 `ANALYZE` 统计 → 对 PR 的 SQL 调用 `EXPLAIN (ANALYZE, BUFFERS)` → diff 计划耗时 |

#### 预期架构变更

1. **`aero-server/src/query_metrics.rs`**（新文件）：`query_metrics_collector` task，每 30s `SELECT queryid, calls, total_exec_time, rows, shared_blks_hit, shared_blks_read FROM pg_stat_statements ORDER BY total_exec_time DESC LIMIT 50` → Prometheus gauge
2. **`deploy/prometheus/alert_rules.yml`**：加 `HighQueryLatency` / `HighSharedRead` / `QueryRegression` 规则
3. **`scripts/query-plan-regression.sh`**（新）：CI 步骤，对 PR 的 `*.rs` grep sqlx query → `EXPLAIN (ANALYZE, BUFFERS)` → diff 与 baseline
4. **`aero-server/src/routes/health.rs`**：`/health/db/queries` 暴露 top-N 慢查询（bearer gate）

#### 对现有系统的影响

| 影响面 | 强度 | 说明 |
|--------|------|------|
| 运行时性能 | **无** | `pg_stat_statements` 本身 < 1% 开销 |
| CI pipeline | **中** | 需要 `PG_QUERY_MIRROR_URL` 环境——可以是开发 PG 而非生产 |
| 迁移顺序 | **小** | `pg_stat_statements` extension 需要在其他查询前安装 |

#### 边界情况

- **`pg_stat_statements` 的 `queryid` 稳定性**：pg 升级后 queryid 可能改变（已知 pg15→pg16 变化）。告警规则需要用 `query` 文本 fallback
- **查询计划回归镜像的维护成本**：数据量级一定要与生产相似（不然 PG 优化器因统计信息偏差产生不同计划）。CI runner 需要定期 `ANALYZE` mirror DB
- **安全性**：`pg_stat_statements` 可能暴露包含用户数据的 query 参数（`WHERE email = $1` 的 `$1` 不暴露值，但 `WHERE id = 123` 的 literal 会被记录）。需要脱敏或权限限制

---

## 三、接口设计建议

### 3.1 公因子——认证因子插件体系（影响多个方向）

**方向一（WebAuthn）、方向五（Query Performance 的管理端访问）** 都受益于一个统一的认证因子抽象。

**建议**：

```rust
// aero-auth/src/mfa.rs — 新建
#[async_trait]
pub trait MfaProvider: Send + Sync {
    fn kind(&self) -> MfaKind;
    async fn register(&self, participant: &Participant, challenge: serde_json::Value)
        -> Result<MfaRegistration>;
    async fn verify(&self, participant: &Participant, credential: &MfaCredential, 
                    challenge: serde_json::Value) -> Result<bool>;
    async fn deregister(&self, credential_id: &str) -> Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MfaKind {
    Totp,
    WebAuthn { device_name: String, platform: WebAuthnPlatform },
    BackupCode,
}

// AuthUser::mfa_verified → mfa_methods: Vec<MfaKind>
```

**为什么抽象而非硬编码**：
- 未来加 SMS/Email OTP、硬件 TOTP token、YubiKey 时只需新 impl `MfaProvider`
- `require_mfa()` 守卫改为检查 `mfa_methods` 非空即可
- 管理员可在 `/api/me/security` 查看/撤销已注册的 MFA 设备

### 3.2 搜索质量的事件源驱动

**现状**：`embedding_backfill` 定时器 + `SELECT WHERE embedding IS NULL`——轮询而非事件驱动。

**建议变更模式**——消息发布路径加异步 enqueue：

```rust
// ImService::send_message 末尾，publish_room_event 之后
if matches!(&msg.blocks, blocks if blocks.iter().any(|b| matches!(b, Block::Text { .. }))) {
    let _ = bus.publish(
        &format!("ai.queue.embed"),
        &EmbedJob::new(msg.id, msg.room_id, msg.created_at),
    ).await; // 失败静默——backfill 兜底
}
```

**关键接口决策**：只在含有 `Text` Block 的消息上触发 embedding，排除纯 `Voice` / `File` / `Mention` / `Card` 消息。

### 3.3 向后兼容策略

| 方向 | 兼容策略 |
|------|---------|
| WebAuthn | 新 `MfaProvider` trait 默认不启用；`POST /api/auth/webauthn/register/begin` 404 直到 `WebAuthn` 配置。现有 `POST /api/auth/verify-2fa` 继续支持 TOTP |
| Search Quality | `enqueue_embed` 在 NA 的 `EventBus` 回调失败时退化为现有定时器路径。`search_click_events` 表启用加 `IF NOT EXISTS` |
| Onboarding | `login` 响应加可选字段，WS Client 忽略未知字段。`onboarding_completed_at IS NULL` 为「未完成」|
| VOD | 新 `recordings` 表不修改既有 `streams` / `stream_chat` 等表。旧 HLS 路径不受影响 |
| Query Performance | `pg_stat_statements` installation 是 `IF NOT EXISTS`；`/health/db/queries` 加 `pub` 路由但不公开文档 |

---

## 四、技术选型

### 4.1 新依赖评估

| 方向 | 候选依赖 | 评估 | 决策 |
|------|---------|------|------|
| ① WebAuthn | [`webauthn-rs`](https://crates.io/crates/webauthn-rs) | 纯 Rust，3K+ stars，active，支持 Passkey + CTAP2 + Conditional UI | ✅ **推荐** |
| ① WebAuthn | 自写 ECDSA/RSA 验证 | 工作量 ~2w 且容易漏认证边界情况（counter 检查、origin 验证、challenge 超时） | ❌ 不推荐 |
| ② Search Quality | `criterion` (bench) | 已使用。离线评测复用。 | ✅ 无新依赖 |
| ③ Onboarding | `step-by-step` 引导框架（前端） | 无需依赖——纯前端 `web/onboarding.js` flow controller + localStorage 进度 | ✅ 无新依赖 |
| ④ VOD | 无（复用 S3BlobStore） | `RecordingStore` trait 包裹现有 `BlobStore` | ✅ 无新依赖 |
| ⑤ Query Performance | `pg_stat_statements`（PG 扩展） | 内置，无需 crate | ✅ 无新依赖 |

### 4.2 自建 vs 引入的判断矩阵

| 组件 | 自建 | 引入 | 判断 |
|------|------|------|------|
| MFA 因子抽象（trait） | ~200 行代码，一个 trait + 3 个 impl | N/A | **自建**——TOTP 已有，加 trait 层即可 |
| WebAuthn 验证 | 需手写 ECDSA/Ed25519 verify + origin validation + challenge 管理 | `webauthn-rs` | **引入**——验证边界细节多，自建风险高 |
| 搜索评测 CLI | 离线计算 NDCG/MRR——无新依赖 | N/A | **自建**——~150 行纯数字运算 |
| 查询计划回归 CI | bash + `EXPLAIN (ANALYZE, BUFFERS)` diff | N/A | **自建**——无合适 crate |

### 4.3 不当引入会出问题的情况

- **`webauthn-rs` 版本锁定**：需要确认其 `serde` 版本与 workspace `serde` 兼容（当前 workspace 可能锁定 serde 版本）。`cargo add webauthn-rs` 后 `cargo check --workspace` 验证
- **`webauthn-rs` 的 Tower 中间件集成**：如果选型 `webauthn-rs` 的 tower layer，需要确认它与 axum 0.7 的兼容性。可选只用其核心 `Webauthn` 类型 + 自写 axum extractor

---

## 五、实施路线图

### 5.1 优先级排序（基于 ROI / 依赖 / 风险）

```
P0 (Week 1-3)    ─── P1 (Week 3-6)    ─── P2 (Week 6-10)
                      │                      │
① WebAuthn           ② Search Quality       ④ VOD Recording
                     ③ Onboarding           ⑤ Query Performance
```

**排序逻辑**：

| 方向 | 优先级 | 理由 |
|------|--------|------|
| ① WebAuthn | **P0** | 企业销售的硬性门槛；直接提升认证体系架构（MfaProvider trait），为后续因子铺路；与既有 TOTP 共享 `AuthUser` 层，改动面集中 |
| ② Search Quality | **P1** | 高技术价值（实时索引），但当前定时器方案可用；`search_click_events` 表启用的低果实可在 P0 窗口以 ~1d 实现 |
| ③ Onboarding | **P1** | UX 价值高，无技术风险，但需要前端工作量。可在方向一 WebAuthn 后端完成后并行 |
| ④ VOD Recording | **P2** | 依赖直播生产化（方向四的截帧/metrics 在另一份计划中）稳定后才有意义；用户需求不紧急 |
| ⑤ Query Performance | **P2** | 纯可观测增强，不阻阻塞任何业务功能 |

### 5.2 阶段划分

#### Phase 1（P0 · Week 1-3）：认证安全基础

| 子项 | 工时 | 交付 |
|------|------|------|
| `MfaProvider` trait + TOTP 重构 | ~2d | `aero-auth/src/mfa.rs`，TOTP 由硬编码变 impl |
| `webauthn-rs` 集成 + `WebAuthnProvider` | ~3d | 注册/登录 begin+finish 端点 |
| `webauthn_credentials` 表 + 仓储 | ~1d | 迁移 + `WebAuthnRepo` |
| `AuthUser` 改为 `mfa_methods: Vec<MfaKind>` | ~1d | 向后兼容：无 MFA 用户返回空列表 |
| 恢复码生成（8 个随机码，Argon2id hash） | ~1d | `POST /api/auth/backup-codes/generate` |
| 集成测试 | ~2d | `webauthn_e2e.rs`（mock authenticator） |
| **Phase 1 小计** | **~10d** | |

#### Phase 2（P1 · Week 3-6）：搜索质量 + 用户引导

| 子项 | 工时 | 交付 |
|------|------|------|
| 消息发布路径加 `enqueue_embed` 钩子 | ~1d | `ImService::send_message` |
| `embedding_backfill` 降级为兜底定时器 | ~0.5d | 配置 `AERO_EMBEDDING_BACKFILL_ENABLED=false` |
| `search_click_events` 表启用 + 补迁移修复 | ~1d | `SearchRepo::record_click` |
| 离线评测 CLI `aero-cli eval-search` | ~2d | NDCG/MRR 输出 |
| 查询改写 `AiService::rewrite_query` | ~2d | opt-in，无 key 退化 |
| `migrations/NNNN_onboarding.sql` | ~0.5d | `participants.first_login_at` + `onboarding_state` |
| `OnboardingFlow` + 路由 | ~2d | `POST /api/me/onboarding/step` |
| `web/onboarding.js` 引导 UI | ~3d | Welcome → Workspace→ Invite → AI 引导 |
| 集成测试（搜索+引导） | ~2d | |
| **Phase 2 小计** | **~14d** | |

#### Phase 3（P2 · Week 6-10）：VOD + Query Performance

| 子项 | 工时 | 交付 |
|------|------|------|
| `recordings` 表 + `RecordingRepo` | ~1d | 迁移 + CRUD |
| `RecordingManager` + bus listener | ~2d | `Status{Live}` → 新建，`Status{Ended}` → finalize |
| VOD 路由 + 鉴权 | ~2d | 公开放映 + 管理员删/改 retention |
| `pg_stat_statements` extension + 迁移 | ~0.5d | `IF NOT EXISTS` |
| `query_metrics_collector` task | ~1d | 每 30s gauge |
| `scripts/query-plan-regression.sh` CI 步骤 | ~2d | `EXPLAIN` diff |
| 告警规则 + 文档 | ~1d | |
| 集成测试 | ~2d | |
| **Phase 3 小计** | **~11.5d** | |

### 5.3 风险点和缓解策略

| 风险 | 方向 | 概率 | 影响 | 缓解 |
|------|------|------|------|------|
| `webauthn-rs` 锁定 serde 版本与 workspace 冲突 | ① | 中 | 高 | Phase 1 起始就是 `cargo add webauthn-rs` + `cargo check --workspace` 验证。冲突则退回 `cargo add webauthn-rs --no-default-features` 手动选 feature |
| iOS Safari Passkey 条件 UI 不可用 | ① | 中 | 低 | 检测 `isConditionalMediationAvailable`，不支持时退到 modal 弹窗 |
| 消息发布路径的 `enqueue_embed` 导致 `send_message` 延迟突增 | ② | 低 | 中 | 使用 `tokio::spawn` + `try_send` 而非 await；如果 bus 不可用则静默失败（backfill兜底） |
| Onboarding Step 超过 5 步导致用户放弃 | ③ | 中 | 中 | Step 设计 ≤4 步（Welcome → Workspace/Discover → Invite → AI）。支持跳过 |
| 直播 `Status{Ended}` 事件在生产中不可靠 | ④ | 中 | 高 | `RecordingManager` 加心跳超时检测——流 5 分钟无推流视为 ended。不依赖单一事件源 |
| `pg_stat_statements` 在 PG 连接池耗尽场景下不可查询 | ⑤ | 低 | 低 | 连接池保留一个管理连接（`min_connections = +1`）专门给 `/health/db/queries` |
| 方向一依赖方向五已有实施计划中的 CircuitBreaker 来保护 WebAuthn 的 Redis 依赖 | ① | 低 | 中 | WebAuthn 的 credential 存储在 PG 而非 Redis，不影响 |
| `search_click_events` 迁移 0133 的 schema 与当前搜索路由不兼容 | ② | 高 | 中 | 实施 Phase 2 第一天先读 `0133_*.sql` 确认 schema，如果不兼容则新建 `search_events` 表而非修复旧表 |

---

## 六、跨方向协同效应

### 6.1 方向之间的显式依赖

```
① WebAuthn (MfaProvider trait) 
    → ③ Onboarding：引导的「安全设置步骤」可包含 Passkey 注册引导
    → 无直接依赖，但 UI 可联动

② Search Quality (enqueue_embed 钩子)
    → 无直接依赖，但实时索引的「事件源」模式是 ④ VOD 的 RecordingManager 的可参照
    
④ VOD Recording (RecordingManager)
    → 依赖 ② 的截帧基础设施（来自另一个计划的直播 metrics 方向）
    → 依赖 ② 的 AI 审核管线（流结束的 thumbnail 提取复用）

⑤ Query Performance (pg_stat_statements) 
    → 受益于所有方向上线后的查询监控
```

### 6.2 与既有 ROADMAP 方向的关系

| 本文件方向 | 既有 ROADMAP 方向 | 关系 |
|-----------|-------------------|------|
| ① WebAuthn | 方向五（企业合规） | 补充——合规需要 phishing-proof MFA |
| ② Search Quality | 方向一（AI 成本与检索质量） | 补充——实时索引 + 评测回路补全方向一的「检索质量」闭环 |
| ③ Onboarding | 方向一（AI 成本）的语义缓存 | 间接——良好引导可减少「不知道如何问 AI」的冗余查询 |
| ④ VOD Recording | 方向四（读副本/分区） | 独立——VOD 存储成本管理与读副本正交 |
| ⑤ Query Performance | 方向四（读副本）的查询路由 | 补充——读副本的查询需要在 pg_stat_statements 中可见 |

---

## 七、总结

| 维度 | 判断 |
|------|------|
| **文档质量** | 五个方向全部有效，交叉验证确认与既有 50+ 文档不重复。关键发现（`search_click_events` 表已存在但未使用）是高价值低果实 |
| **最大架构增益** | **方向一（WebAuthn + MfaProvider trait）**——不仅新增认证因子，还在架构层面建立了认证因子的可扩展性。这是五个方向中唯一一个「修改架构形状」的方向 |
| **最大风险** | **方向四（VOD Recording）**——对直播结束事件的可靠性和截帧基础设施的依赖意味着它必须先等另一份计划的直播 metrics 和截帧成熟，否则可能建了一个 recording 表但录制不完整 |
| **最易落地** | **方向五（Query Performance）第一阶段**——`pg_stat_statements` 的 extension 安装 + 30s gauge task 约 1.5d，零风险，立即获得生产查询可见性 |
| **最高性价比** | **方向二（Search Quality）的 `search_click_events` 启用**——表已存在，只需写路由 `SearchRepo::record_click` + 离线评测 CLI，~2d 获得搜索质量的量化基线 |
| **实施建议** | P0 专注方向一的 `MfaProvider` 体系；P1 并行做方向二的 `search_click_events` 启用 + 方向三的 Onboarding 后端；P2 做 VOD 和 Query Performance |
