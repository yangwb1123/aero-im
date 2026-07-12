以下是对 `2026-07-11-second-round-global-scan-five-uncovered-directions.md` 的架构分析报告。

---

# 架构分析：第二轮全局扫描 5 个方向

## 1. 架构评估

### 1.1 当前架构的显著优势

**事件驱动骨架的方向是正确的。** Aero IM 选择「NATS JetStream 作为跨实例事实源 + 进程内 Hub 扇出」这一架构模式，是经过深思熟虑的：

- **横向扩展路径清晰**：durable consumer 带 at-least-once 语义，多实例间无协调成本。CRDT 式的最终一致性（seq 驱动去重）降低了实时路径的强一致性需求。
- **读写分离意图到位**：热路径走 NATS + 内存 mpsc，冷路径走 PG + Redis sorted-set。消息发送不等待 PG 写入完成（先发布事件，异步持久化）。
- **单进程 enough-for-now**：16 crate 分层明确，依赖方向自下而上无环。在 <5k 并发规模下，单进程架构的 OPS 密度更高（无 RPC 序列化开销）。

### 1.2 需要警惕的架构局限

**五个方向揭示了一个共性模式：架构的「外部接口」和「内部治理」之间存在剪刀差。**

具体来说：

| 维度 | 外部（面向用户）成熟度 | 内部（面向运维/开发者）成熟度 |
|------|-----------------------|------------------------------|
| 认证 | JWT + OIDC + TOTP + PAT，功能完整 | **无刷新机制**导致长连接周期性断连 |
| 配置 | 功能丰富，40+ env var | **零校验、零契约、零运行时重载** |
| 监控 | Prometheus metrics + OTLP | **运行时完全黑盒**——无任务队列延迟、无连接池按用途拆分 |
| AI | 8 种文本能力，预算管控 | **多模态零覆盖**，竞品已全部具备 |
| 测试 | 819 单元测试 + 35 REST smoke | **WS 16/18 帧类型仅 1 种有集成测试** |

这张表说明了什么？**Aero IM 在「用户能直接感知的功能」上投入了大量工程质量（MLS scaffold、live streams、calls、AI 流水线），但在「用户不直接感知但决定生产可靠性」的内部治理上存在系统性欠账。** 这不是某个模块的失误，而是整个工程文化的偏好——feature-first, operations-second。

### 1.3 关键设计决策的合理性

| 决策 | 评价 | 理由 |
|------|------|------|
| 单进程巨石 | ✅ 当前合理 | <5k 并发，单体比微服务 OPS 密度更高，跨 crate 编译优化好（LTO + monomorphization）。但阈值在 10-20k 并发时需重新评估 |
| NATS JetStream 事件总线 | ✅ 正确 | 跨实例去重（per-subject seq）、durable cursor、queue group 扇出——这在同规模项目中是最优的组合 |
| PG 连接池全系统共享 | ⚠️ 当前可接受但有隐患 | 验证报告修正后（AI worker 默认 4 并发而非 12），16 连接在典型负载下够用，但 20 并发峰值会压到剩余 12 给热路径。阈值临界非紧急，但已接近 |
| Hub 进程内 mpsc 扇出 | ✅ 正确 | 跨进程扇出已由 NATS 解决；进程内用 mpsc 而非 broadcast 是合理的选择（channel 满时阻塞发端而非丢消息） |
| AI 预算用进程内存（DashMap）而非 Redis | ✅ 当前合理 | Budget 丢失（进程重启）只意味着一个 60s 窗口的重置，无副作用。用 DashMap 避免了 Redis 往返延迟。但在 AI Worker 独立进程后必须迁移到 Redis |

### 1.4 架构债务评估

**三类债务，需区别对待：**

#### 良性债务（可接受，有清晰偿还路径）
- 单进程共享 tokio 运行时：知道问题所在，有明确隔离方案（独立 pool → 独立进程流水线），短期内用 `yield_now()` 和独立 PgPool 即可缓解。
- 配置散落各处：文档已给出三个阶段，阶段 A（配置校验）2 天内可完成。

#### 有害债务（需要优先处理）
- **WS token 生命周期管理**：用户直接感知的质量问题。「用户每 15-60min 被断开一次」在验证后虽修正为 1h（3600s），但大直播场景（1-3h）必然触发一次断线。且重试耗尽后永久断开的设计是产品层面的缺陷，不是性能优化——这是需要立即修复的 bug。
- **测试金字塔塌陷**：WS 帧 18 种只有 1 种被集成测试覆盖。随着 RoomEvent/StreamEvent variant 继续增加（当前 ~15 个），重构 WS 扇出路径时几乎不可能不引入回归。

#### 可接受债务（在可预见的未来不处理）
- 密钥轮换（SOC 2）：当前阶段无合规审计需求，JWT 密钥轮换可以在 2027 H1 再处理。
- 文档 AI / 图像生成：竞品对标（Slack AI 99 美元/月/用户）不是 Aero IM 当前的核心战场。

---

## 2. 扩展方向——架构视角

基于验证报告修正后的数据，我重新评估五个方向，并补充三个验证分析中未覆盖的架构级方向。

### 方向 A（修正后 P0）：WS 连接生命周期管理 ← 原方向一

**为什么需要：直接影响 100% 长连接用户。**

验证修正了 JWT TTL 为 3600s（文档原称 900s），但核心论点不变：
- 1h 过期必然在典型直播（1-3h）中触发
- 重试耗尽后的永久断连是产品缺陷，不是性能杂音
- 客户端状态恢复不完整（不重建在线列表、不重建直播上下文）

**核心挑战：**
- 客户端无任何 JWT `exp` 解码逻辑（纯 base64 解码 payload，无需库，但需要写）
- 解法 A（WS 帧级刷新）会引入新的 `ClientFrame` variant——这需要方向五的 WS 测试框架支持
- 解法 B（REST 刷新 + 热切换）更简单但需要 `_pendingToken` 状态管理

**架构变更：**
- 解法 B：零后端变更，前端约 20-30 行 JS。推荐作为第一阶段
- 解法 A：后端加 `RefreshToken` frame → `TokenRefreshed` 响应，约 40 行 Rust。推荐在 WS 测试框架到位后补充

**系统影响：**
- 无新增依赖
- 无数据库 schema 变更
- 解法 B 无需任何服务端修改——这本身就是 XS 体量的最佳证据

### 方向 B（修正后 P1→P2）：运行时可见性与工作负载隔离 ← 原方向三（降级）

**验证修正使紧迫性降低：AI Worker 从 12→4 并发，idle_sleep=1s，实际 PG 争用比文档声称小 3 倍。** 但这不意味着方向消失——只是从 P1 降为 P2。

**核心论点的重新评估：**

| 文档声称 | 验证后实际 | 对结论的影响 |
|----------|-----------|-------------|
| 12 worker × 每秒 1 轮询 = 12 QPS PG | 4 worker × 每秒 1 轮询 = 4 QPS PG | 争用减小 3 倍，非紧急 |
| 12 worker 占 8-10 连接 | 4 worker 占 2-4 连接 | 16 池的 25% vs 75%，显著不同 |
| 默认 16 连接池 | 默认 16 连接池 | 不变。4 worker 占 4/16，热路径剩 12——在 <1k 并发下够用 |

**修正后的建议：** 方向三的阶段 A（运行时可见性）应提升优先级——即使 AI Worker 不隔离，你也需要知道连接池的使用分布。阶段 B（工作负载隔离）推迟到可观察的数据证明了争用之后。

**架构变更建议（修正版）：**

1. **阶段 A（S）——增加 `aero_worker_pg_connections` 指标**，按标签区分 `worker=ai|http|sweep`。这是最小的可观察性投资，可以回答「AI Worker 到底占用了几个连接」这个验证报告未能回答的问题（验证报告只检查了代码配置，未运行实际负载）。
2. **阶段 B（M）——仅在指标证明争用 >= 30% 的情况下**，为 AI Worker 创建独立 PgPool。如果指标显示 AI Worker 峰值只占 3-4/16，则不需要隔离。
3. **阶段 C（L）——AI Worker 独立进程**。这是一个跨架构大变更（需要 NATS `ai_jobs` subject + Redis budget + 独立二进制发布），应该在 PgPool 隔离被证明不够之后再考虑。

### 方向 C（P1→P1）：配置生命周期管理 ← 原方向二

**验证后保持 P1。** 这是生产运营的基础设施，不是功能锦上添花。

**补充架构分析：**

当前配置管理的核心问题不是「40 个 env var 太多」，而是**配置契约缺失**——每个 `std::env::var` 调用点独立决定缺失时的行为（panic / silent default / degrade），没有统一的声明式 schema。

**关键设计决策：**

应该采用「分层配置」模型而非简单地增加校验函数：

```
层 1: 内置默认值（compiled-in, 在 Config struct 的 Default impl 中）
层 2: 配置文件覆盖（config.toml, figment）
层 3: 环境变量覆盖（AERO__*）
层 4: 密钥存储（Vault / Docker secrets / K8s Secrets，通过 AeroSecret trait）
```

这个分层在 figment 中已部分实现，但缺少：

- **Schema 声明**：每个配置键应该有类型、合法性范围、必需/可选标志
- **版本化**：配置 schema 版本（与二进制版本对齐），新版本引入新键时告知管理员
- **运行时重载**：仅限「安全可热更」的配置域（限流参数、CORS 源、日志级别），不允许重载 DB URL 或 JWT 密钥

**对现有系统的影响：**
- 阶段 A 不需要改任何业务代码——只加一个 boot-time 的 `validate_config()` 函数，遍历已知配置键
- 阶段 B 的 `AeroSecret` trait 是新增抽象，不影响现有 `std::env::var` 调用

### 方向 D（P2→P2）：多模态 AI ← 原方向四

**验证后确认 P2。** 我比原文档更谨慎一些——不是因为不认同方向的价值，而是因为多模态 AI 的 ROI 在当前阶段低于其他 P2。

**架构层面的几点不同意见：**

1. **阶段 A（图片问答）的核心难点不是 API 调用——是「谁触发 vision 调用」。** 当前 AI 问答（`AiBackend::answer_question`）由 `agent_bot` 收到 @mentions 后调用。要让 agent bot 能「看」图，需要：
   - 解析消息中附带的 `Block::File { kind: FileKind::Image }` 引用
   - 通过 `blob_store.get` 拉取图片二进制
   - 编码为 base64 拼接进 Anthropic Messages API
   - 这三步中，`blob_store.get` 可能涉及 S3 网络调用——这在 agent_bot 的同步路径中会阻塞 till S3 响应，延长 AI 响应延迟

2. **文档智能（阶段 B）的开箱成本被低估了。** PDF 解析不是「丢给 Claude Vision 截帧」这么简单——多页 PDF 的 token 消耗巨大，单份 20 页文档可能消耗 20 万 token（$3-5）。在没有用户确认或 workspace-level budget 控制的情况下，RAG 文档解析会快速消耗 AI 预算。

3. **图像生成（阶段 C）有产品定位问题。** Aero IM 是「AI-native IM + 直播平台」，不是设计工具。用户期待的是文档理解和分析（阶段 A/B），而非生成图片。DALL-E/Flux 集成应该是一个「nice to have」而非规划路线。

**建议调整后的多模态路线：** 只做阶段 A（图片问答），阶段 B（文档智能）标记为「待评估」（需要产品侧确认用户用例），阶段 C 降级为 P3。

### 方向 E（P2→P2）：WS 集成测试 ← 原方向五

**验证后确认 P2。但我认为这是所有五个方向中**长期技术债最高、当前被最严重低估的。

**为什么？**

1. **18 种 ServerFrame vs 1 种测试覆盖**——这不是 5% 覆盖率，这是 5.5%。任何 WS 扇出路径的重构（`Hub::fan_out_raw` 改为批量写入、`room_event_to_frame_json` 映射逻辑调整）都会引入无法被现有测试捕获的回归。

2. **直播/通话帧零测试**：`StreamEvent` 和 `Call` 帧是通过 ephemeral NATS consumer 消费的——这是全系统唯二使用 ephemeral consumer 的路径。任何 consumer 配置错误（queue group 误设、ack 超时过短）都会导致**帧静默丢失**，没有错误日志、没有告警、没有测试。

3. **测试框架本身将成为架构资产**：一旦建立了 WS 集成测试套件，新加 `RoomEvent` variant 的成本将从「手工测试 + 祈祷」降低为「加一个 test case 然后 `pytest`」。

**我对原文档的补充建议：**

- **阶段 A 的测试框架不要用 Python。** 原因：
  - CI 环境中额外依赖（`websockets` PyPI 包）的管理负担
  - Python 端无法直接使用 Rust 侧的 `ClientFrame` / `ServerFrame` 类型定义——序列化/反序列化测试与业务代码的解耦意味着测试只能验证「字符串 JSON」，而非「类型正确的帧」
  - 更好的选择：Rust 集成测试（`tests/` 目录下的 `#[tokio::test]`），可以直接 `use aero_server::ws::ServerFrame`，进行类型正确的断言
  - 但 Rust 集成测试需要启动完整的 `aero-server`（包括 PG、NATS、Redis）——这是可接受的（docker-compose CI），但增加 CI 时间
  
  折中方案：Event-based 测试校验——用 Rust 的 `assert_matches!` 直接验证 `Hub::fan_out_raw` 的输入/输出，不需要启动完整服务栈。这是单元测试级别的 WS 协议验证，体量 S，效果 M。

---

### 补充方向 F（新）：Live / Graceful Shutdown 架构——零停机部署

**验证报告未覆盖，但这是一个架构级缺口。**

当前 `CancellationToken` 已在 bin 中实现优雅关停，但**没有「连接排空」（connection draining）机制**：

1. WS 连接排空：K8s 滚动更新时，旧的 Pod 收到 SIGTERM → 立即关闭监听端口 → 仍在服务中的 WS 连接被直接切断 → 用户体验：断连
2. 标准的做法：`preStop` hook + 延迟关闭（`sleep 10` + `SIGTERM`），让负载均衡器有时间将连接迁移到新 Pod
3. 但当前没有排空 API——无法在工作进程进入 draining 模式时停止接受新连接、但继续服务现有 WebSocket 直到它们自然断开

**建议：**
- 在 `/health/ready` 中增加 draining 门：`CancellationToken::is_cancelled() → ready=false` 或返回 503
- 添加 `GracefulShutdownLayer` tower 中间件：draining 时拒绝新 HTTP 请求，但现有 WS 连接继续服务
- K8s Probe 配置：`readinessProbe` 用 `GET /health/ready`，当 draining=true 时返回 503 → 从 Service Endpoints 中移除

### 补充方向 G（新）：Crate 边界校验——依赖方向交叉验证

这是一个架构治理工具，而非功能。

**问题**：AGENTS.md 定义了 crate 分层（基础→IM→直播→组合），但没有任何编译期或 CI 机制强制跨 crate 引用边界的合法性。

**具体风险**：
- `aero-im-core` 间接依赖了 `aero-live-webrtc`（直播 crate 层引用 IM crate）。反之亦然——IM crate 引用直播 crate？这会违反依赖方向，但当前没有任何检查。
- `aero-common` 是叶子 crate，但如果有代码在 `aero-storage` 引用 `aero-common`——这允许吗？是的。但反过来？不。

**建议**：
- 在 CI 中引入 `cargo-deny` 或自定义 `scripts/dep-check.sh`，按 `AGENTS.md` 定义的层验证 crate 引用方向
- 使用 `[workspace.dependencies]` + `[lints]` 在 `Cargo.toml` 中标记违规依赖为 `deny`

---

## 3. 接口设计建议

### 3.1 三个需要重新审视的接口点

#### 3.1.1 配置读取接口

当前问题：`std::env::var` 散落在 10+ 模块中，每个调用点独立决定缺失行为。

**建议的抽象**：

```
trait ConfigProvider {
    fn get(&self, key: &str) -> Option<String>;
    fn get_or_default(&self, key: &str, default: &str) -> String;
    fn required(&self, key: &str) -> Result<String, ConfigError>;
}
```

三个实现：
- `FigmentConfigProvider`（包装当前的 `Config` struct，从 figment 读取）
- `EnvConfigProvider`（直接从 `std::env::var` 读取，向后兼容）
- `CompositeConfigProvider`（先查 figment，再查 env，再查默认值）

**向后兼容策略**：不修改现有 `Config` struct，只新增一个 `validate_config(&Config) -> Vec<ConfigIssue>` 函数，在 boot 时调用。所有旧代码保持不变。

#### 3.1.2 AI Backend 的输入接口

当前 `AiBackend::answer_question` 只接受文本。多模态 AI（方向四）需要接受 `blocks` 参数。

**建议**：

```rust
pub struct AnswerQuestionInput {
    pub messages: Vec<ChatMessage>,
    pub blocks: Vec<Block>,        // 新增：消息中附带的文件/图片
    pub workspace_id: WorkspaceId,
    pub participant_id: ParticipantId,
}
```

**向后兼容**：保持旧的 `answer_question(&self, messages, ...)` 签名不变，内部转发到新签名。这样现有调用点（agent_bot、summarize、translate）不受影响，新调用点（图片问答）使用新签名。

#### 3.1.3 WS 帧协议的版本化

当前 `ClientFrame` / `ServerFrame` 没有版本号。随着 variant 增加（RefreshToken、新的交互式 Block 帧等），需要考虑帧协议的向前兼容。

**建议**：

- 不在 WS 协议层面引入版本号——太重量级（需要握手协商、兼容矩阵）
- 采用「忽略未知字段」策略（`#[serde(deny_unknown_fields)]` 放开为默认行为）
- 新增 `ServerFrame::Unknown { raw: serde_json::Value }` variant：服务端收到无法识别的 client frame 时返回 unknown 但不断开连接
- 客户端收到无法识别的 server frame 时静默忽略 + console.warn（而非崩溃）

### 3.2 是否需要引入新抽象层？

| 抽象层 | 是否必要 | 理由 |
|--------|---------|------|
| `ConfigProvider` trait | ✅ 是 | 统一配置读取行为，消除 panic/degrade/default 的三不一致 |
| `SecretProvider` trait | ✅ 是 | 密钥轮换、多后端（Vault/Docker Secrets）的必要前提 |
| `HealthCheck` trait | ⚠️ 值得引入 | 当前 `/health` 是硬编码检查，每个子模块都直接注册；可以抽象为 `HealthCheck` trait 让子模块自注册 |
| `WorkerPool` trait | ⚠️ 可推迟 | AI Worker 当前是 `tokio::spawn` × N，独立进程后才需要抽象。当前建议用 `Semaphore` 限流即可 |
| `EventPublisher` trait | ❌ 不必要 | `publish_room_event` 已经有清晰的函数签名。增加 trait 只会增加间接性而不增加价值 |

### 3.3 接口兼容性的核心原则

对于这个阶段的项目，我最推荐的核心原则是：

> **新增不修改，扩展不覆盖。**

具体来说：
- 需要新配置的模块，在 Config struct 中加新字段（`Option<T>`），不改变现有字段类型/默认值
- 需要新 WS 帧的变体，扩展 enum（加 `#[serde(rename = "new_type")]`），不改现有 variant 的序列化格式
- 需要新参数的函数，给参数加 `Option` 或定义新的 input struct，不改变现有签名的必参类型

这个原则看起来很简单，但在实际开发中经常被违反（因为「改现有调用点省事」的冲动很强）。

---

## 4. 技术选型

### 4.1 是否需要引入新的技术栈？

基于验证报告修正后的分析，**五个方向都不需要引入新的生产路径依赖。**

| 方向 | 需要的新栈 | 理由 |
|------|-----------|------|
| 方向一 WS 认证 | **无** | 解法 B 纯前端；解法 A 扩展已有 `ClientFrame`/`ServerFrame` enum |
| 方向二 配置管理 | **无** | 阶段 A 纯 Rust `Vec<String>` 校验；阶段 B `AeroSecret` 是新增 trait，无外部依赖；阶段 C `Arc<RwLock<>>` 是标准库 |
| 方向三 运行时可见性 | `console-subscriber`（dev 可选） | 生产环境不需要——`tokio-console` 只用于开发调试。生产环境用 Prometheus + tracing |
| 方向四 多模态 | **无** | 图片问答：base64 编码是标准库；文档 AI：Tesseract 或 Claude Vision（已有 API）；图像生成：DALL-E API（已有 HTTP client） |
| 方向五 WS 测试 | `websockets` PyPI | 仅在 CI/开发环境，非生产路径依赖。且我建议改用 Rust `#[tokio::test]`，零新依赖 |

**结论：五个方向的体量加上「零新生产依赖」的约束，使它们都非常适合短期投入。**

### 4.2 如果必须引入依赖，哪些值得考虑？

| 候选依赖 | 使用场景 | 评估 |
|---------|---------|------|
| `console-subscriber` | tokio 任务级可观察性 | ✅ 推荐。仅在 dev 环境启用，CI/生产关闭。零风险。 |
| `proptest` | WS 帧协议属性测试 | ✅ 推荐。纯测试依赖，不进入生产路径。当前 `ws/ws_impl/` 已有单元测试，用 proptest 可以随机生成 `ClientFrame`/`ServerFrame` 变体做往返测试。 |
| `tesseract-rs` | OCR / 文档 AI | ❌ 不推荐。原生 C++ 依赖增加 build 复杂度。更好的方案：直接用 Claude Vision 截帧（已有 API key）。或异步调云 OCR API。 |
| `opencv-rs` | 屏幕共享分析 / 视频理解 | ❌ 极其不推荐。构建时间爆炸 + ABI 兼容性噩梦。应该是独立的 AI 微服务，而非嵌入单体。 |
| `hashicorp_vault` Rust SDK | 密钥管理 | ⚠️ 推迟评估。当前无生产合规需求。可以用 Docker Secrets + file-based provider 过渡。 |

### 4.3 自建 vs 采购的决策框架

在 Aero IM 的上下文中，我推荐的标准：

| 决策条件 | 自建 | 采购（第三方 API/服务） |
|---------|------|----------------------|
| 差异化核心能力 | ✅ AI Agent、实时信令 | ❌ |
| 通用能力、无差异化价值 | ❌ | ✅ 推送（FCM/APNs）、OCR、图像生成 |
| 合规/数据驻留需求 | ✅ 消息存储、AI 推理 | ❌ |
| 体量小、实现简单 | ✅ 配置校验、WS 帧解码 | ❌ |

图像生成（方向四阶段 C）是「采购」的典型案例：DALL-E / Stability AI 的 API cost-per-image 很低（$0.02-0.08），自建模型则需要 GPU + MLOps 管线。在 Aero IM 没有 10 万+ MAU 之前，自建图像生成没有任何价值。

---

## 5. 实施路线图

### 5.1 修正后的优先级矩阵

基于验证报告修正的数据：

| 方向 | 原始优先级 | 验证后优先级 | 体量 | 核心收益 |
|------|-----------|-------------|------|---------|
| 一：WS 认证生命周期 | P0 | **P0** | XS | 消除所有长连接用户的周期性断连 |
| 二：配置校验（阶段 A 仅） | P1 | **P1** | S~M | 生产故障降低 30%+（配置错误是最高频事故） |
| 三：运行时可见性（阶段 A 仅） | P1 | **P2** | S | 修正后 AI 争用不紧急，但可见性仍有价值 |
| 五：WS 测试（阶段 A 仅） | P2 | **P2** | M | 防止 18 种帧类型的回归，保护所有方向的重构安全 |
| 四：图片问答（阶段 A 仅） | P2 | **P2** | S~M | AI-Native 产品差异化，竞争对手已具备 |
| 三：工作负载隔离（阶段 B/C） | P1 | **P3** | M~L | 验证报告大幅降低了紧迫性 |
| 四：文档智能 / 图像生成 | P2 | **P3** | M | 需产品确认用例 |

**最重要的修正：方向三从 P1 降为 P2（阶段 A 可见性）/ P3（阶段 B/C 隔离）。** 不是方向错了——是 3 倍的数字偏差改变了紧迫性判断。

### 5.2 阶段划分

```
P0 窗口（今-2 周）
└── 方向一 WS 认证生命周期
    ├── 解法 B（前端 ~20 行 JS）：本周
    └── 解法 A（后端 WS 帧刷新）：下月（等 WS 测试框架到位）

P1 窗口（2-6 周）
├── 方向二 阶段 A（配置校验 ~2 天）
│   ├── 汇总所有 env var 调用点
│   ├── boot-time validate_config() 函数
│   └── GET /api/debug/config admin 端点
└── 方向五 阶段 A（WS 测试框架初始套件 ~3-5 天）
    ├── 消息生命周期（send→edit→delete→reaction）
    ├── 协作（typing→mark_read→message_seen）
    └── 错误路径（invalid_frame→expired_token）

P2 窗口（6-12 周）
├── 方向三 阶段 A（运行时可见性 ~2-3 天）
│   ├── 按用途标记 PgPool 指标
│   ├── tokio-console 开发环境集成
│   └── /api/debug/tasks admin 端点
├── 方向四 阶段 A（图片问答 ~2-3 天）
│   ├── AiBackend 支持 blocks 参数
│   └── agent_bot 触发 vision 调用
└── 方向五 阶段 B（WS 模糊测试 + proptest）
    ├── cargo fuzz ClientFrame 反序列化
    └── proptest room_event_to_frame_json 往返

持续（3-6 月）
├── 方向二 阶段 B（密钥管理，M 体量）
│   └── AeroSecret trait + FCM auto-refresh
└── 方向二/三 阶段 C（按需触发）
    ├── 运行时重载（配置 + 限流）
    └── 独立 AI Worker 进程
```

### 5.3 风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| 方向一解法 B（前端 token 热切换）引入竞态：重连时 token 恰好同时刷新 | M | M | 加锁：重连路径中，先检查 token 是否过期，过期则等待 refresh 完成再连。`_pendingRefresh` Promise 缓存。 |
| 方向二配置校验阶段 A 发现 40+ env var 中有「幽灵配置」（已不用但未清理） | H | L | 幽灵配置不属于 FATAL 档，只报 WARN。不影响启动。 |
| 方向五 WS 测试框架依赖 docker-compose 全栈启动 → CI 慢 | M | M | 两阶段：Rust `#[tokio::test]` 无需外部服务（直接测试 `Hub::fan_out_raw` 输入/输出），Python pytest 做端到端验证只在 nightly CI 运行。 |
| 方向四阶段 A 图片问答的 S3 延迟导致 AI 响应 >10s | M | M | `blob_store.get` 异步 + 超时（默认 5s）。超时后备 fallback：`"我无法获取这张图片，请稍后重试"`。 |
| 多个方向并行开发导致 CI 红锁时间增加 | M | M | 每个方向在独立 git worktree 开发（遵循 AGENTS.md §4.1），集成时统一合入。WS 测试框架（方向五）先到，保护其他方向的合入安全。 |

### 5.4 不建议做的事

1. **不要在方向三上投入超过阶段 A 的时间**，直到运行时指标证明需要阶段 B/C。
2. **不要在方向四阶段 B/C 上投入**，除非产品侧确认了用户用例。
3. **不要在 WS 协议中引入版本号**——忽略未知字段的策略更适合当前阶段。
4. **不要为了「微服务化」而拆分进程**——单进程巨石直到 10k+ 并发都是最优解。拆分的理由是隔离而非可扩展性。

---

## 总结陈述

这五个方向代表了 Aero IM 从「feature-complete」到「production-hardened」的必经之路。验证报告修正了方向三的数字偏差（3 倍夸大），但不改变核心方向的正确性。

**最关键的行动项（本周可启动）：** 方向一（WS 认证生命周期）——零后端变更、20 行前端代码、直接消除每连接 1 小时的最大用户痛点。这是所有五个方向中投入产出比最高的。

**最被低估的风险：** 方向五（WS 集成测试）。18 种帧类型 5.5% 的测试覆盖率意味着每一行 WS 扇出路径的代码都没有回归保护。这不是一个「迟早要做」的优化——这是在积累不可见的架构债务，利息每天都在增长。

**最后一点架构哲学：** 这五个方向的共性不是「添加新功能」——而是**完成未完成的设计契约**。JWT 的设计契约是「我认证你是谁」，但当 WS 连接超过 JWT 生命周期时，这个契约就断裂了。配置的设计契约是「给我正确的参数我就正常工作」，但没有任何机制保证参数是正确的。WS 帧的设计契约是「你发这个帧我就给你正确响应」，但没有任何测试验证这一点。这些不是 feature gaps——它们是设计契约的不完整实现。
