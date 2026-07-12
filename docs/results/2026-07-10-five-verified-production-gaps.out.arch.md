以下是基于验证文档和代码库深层交叉引用的架构分析。

---

# 架构分析：Aero IM — AI-Native 即时通讯 + 直播平台

## 1. 架构评估

### 1.1 当前架构优势

该架构充分体现了现代 Rust 后台设计中罕见的成熟度。值得强调的几点：

**事件溯源骨架（NATS JetStream）**
- `im.room.*` subject 使用 per-subject 单调 seq（`bus/seq.rs`）为 at-least-once 投递提供客户端级去重。这是正确的设计——幂等由消费者处理，而非寄希望于恰好一次投递。
- durable consumer（`aero-server`）与 ephemeral consumer（`live.stream.*`）之间的区分体现了对数据丢失容忍度的深刻理解：房间事件必须重放（durable），直播弹幕则可以容忍丢失（ephemeral）。
- 所有 crate 依赖图是 DAG（第 4 层自下而上的正确性证明——`aero-common` 没有反向边）。

**测试基础设施质量**
- `FakeUnfurler`、`FakeSender`、`FakeMailer`（如果存在）、`FakeGateway`——每个带 I/O 的模块将网络隐藏在一个 trait 后面。这是正确的方式。
- 841 个 hermetic 测试，数据库测试用 `#[ignore]` + `DATABASE_URL` 门控——测试基础设施远比许多生产部署更成熟。

**预算约束的 AI 集成**
- `AiWorker` 架构（`FOR UPDATE SKIP LOCKED` + per-ws/全局 `CostBudget` + 加权 all-or-nothing + `MAX_ATTEMPTS=5` → dead letter）是深思熟虑的。它解决了一个难以解决的问题——多租户 LLM 成本控制——且没有锁争用。
- API 层同时提供同步（`/api/ai/ask`）和异步（`ai_jobs` 表）路径是正确的架构选择。

**结构化部署**
- `bin/boot/` 下的 13 个模块按职责明确分离——这是 Rust `fn main()` 常见的大泥球模式中难得的品质。
- 启动后台智能体（`background.rs`）的模式——每个 bot 是一个独立的 `tokio::spawn`，带有 gate 检查 + 自回环防护——是生产级的设计模式。

### 1.2 关键架构债务

**路由层是全系统最大的耦合点**
- `routes.rs` 在单文件中 2854 行——这是一颗定时炸弹。单个 `Router::new()` 链与 50+ `.merge()` 调用意味着启动时任何合并冲突或路由前缀重叠都会导致运行时静默路由被屏蔽（Axum 会覆盖而非报错）。
- 每条路由都有一个内联 handler 函数，形成匿名函数的大爆发——模块化被绑定破坏。

**API 版本化的缺失已达到架构风险级别**
- 87 个功能点 + 157 次迁移 + 可能在运行的 bot/SDK 生态——但没有 `/api/v1/` 或 `/api/v2/`。向后兼容的变更（新增字段）可行，但破坏性变更（重命名字段、更改分页格式）需要原子性的客户端+服务器升级。
- 2054 年：当需要更改 `POST /api/rooms/:id/search` 的请求体格式时会发生什么？有 10 个外部集成在用。答案：要么永远不更改，要么在全系统协调日那天。这两者都不好。

**邮件子系统是架构单点故障**
- 不到 125 行，`send_password_reset()` 返回 `() `——异步 SMTP 调用后的任何错误都会被静默记录并丢弃。这意味着密码重置是"即发即忘"——这是错误的，因为密码重置不可重试（token 单次使用）。如果 SMTP 宕机，用户无法登录。
- 没有队列意味着请求处理时间受 SMTP 往返影响（100-500ms）——为了这个关键路径存储电子邮件队列而承受的锁存器。
- 没有 HTML 模板意味着品牌化受限；欢迎来到 2026 年，企业产品仍发送纯文本电子邮件。

**媒体面处于"已写但无法执行"的状态**
- `sfu_media.rs` 的 `bind`→`run(cancel)` 循环是真实的，但它在 `#[cfg(test)]` 之后——生产中没有实例化。`call_bridge_supervisor` 的 `ensure_egress` "已建 + 单测"但"生产未接线"。
- 这不是死代码——这是一个集成 seam。但代码库将其列为"完成"（`真 MPEG-TS muxing`），而 CI 从未运行过。这不同于测试覆盖率缺口——这是端到端验证缺口。

**基于字符串的 SQL 迁移，无向下兼容方案**
- 157 个顺序迁移文件（`NNNN_name.sql`），仅使用 `sqlx::migrate!("../../migrations")`。无法回滚。无 `VERSION` 表元数据用于向下兼容。对于开发来说可以接受，但企业部署需要零停机迁移策略。

---

## 2. 扩展方向

### 方向 A：容器化与部署拓扑（P0 → 高优先级）

**为什么需要**
- 无 Dockerfile、无 k8s manifest、无 helm chart。唯一的部署载体是 `docker-compose.yml`（只包含基础服务——没有 server）。
- 这对于企业 PoC 来说是阻塞项——你不能交付一个 tarball 并期望客户运行 `cargo run`。
- 当前配置模型（`AERO__SECTION__KEY` 双下划线 env var 示例化使用 `figment`）是合理的，但没有 docker-compose 叠加层用于健康检查、优雅停止或日志收集。

**核心挑战**
- 服务有 5 个外部依赖项（PG、Redis、NATS、Jaeger、MinIO）——容器 stack 需要编排生命周期。
- 水平扩缩容意味着每个 server 实例都需要自己的 `CancellationToken` 用于优雅关闭（已有），以及 `aero-server` s 之间的 sticky-routing（尚未有）。
- 媒体面（RTMP :1935、SRT UDP、WHIP）需要特定于网络的 k8s 配置（NodePort/LoadBalancer UPD）。

**预期架构变更**
- 一个 `Dockerfile`（多阶段构建：Rust 编译阶段 + 精简 `distroless` 运行阶段）。
- `k8s/` 目录：`StatefulSet` 用于 PG/Redis/NATS，`Deployment` 用于 server，`ConfigMap`，`Secrets`。
- 健康探测的 liveness probe（`/health/live`）和 readiness probe（`/health/ready`，检查 PG/Redis/NATS/blob）已经存在——这是加分项。
- Helm chart 用于参数化环境变量、副本数、存储类。

**对现有系统的影响**
- 纯新增文件——没有现有代码被触及。
- 风险低（部署层），但需要有人实际编写和验证模板。

---

### 方向 B：负载测试基础设施（P0）

**为什么需要**
- 841 个单元测试，零基准测试。`Cargo.toml` 中没有出现 `criterion`。
- AGENTS.md §1 声称"单消息端到端延迟 < 100ms（本地）"——但这一断言从未被验证过。
- 关键的不可变因素无法测试：`Hub::fan_out_raw` 能否在 bounded `mpsc` 缓冲区下处理 1000 个并发 WS 连接？`AiWorker` 的 `SKIP LOCKED` 轮询在 10 个并发 worker 下会造成多少 DB 争用？

**核心挑战**
- 端到端负载测试意味着模拟真实 WS 流量——比 HTTP 基准测试复杂得多。
- IM 负载测试不能采用简单的"保持连接打开并发送请求"模式——需要支持"同时 500 个用户在 10 个房间内互相发送消息"等场景。
- 媒体面负载（RTMP 推流 + HLS 拉流）需要原生负载工具——`k6` 无法做到这一点。

**技术选型选项**

| 工具 | 适合场景 | 局限性 |
|---|---|---|
| `criterion` | 纯 CPU 基准测试（总线编码/解码、Block 序列化、OG 解析） | 无法覆盖网络或 I/O |
| `k6` + WebSocket 扩展 | IM 负载（发送消息、接收实时帧） | 不支持媒体面；需要 JS 编写 |
| 自定义 Rust 负载生成器 | 端到端 IM + 直播负载 | 开发成本高；最准确 |
| `srs-bench` / `ffmpeg` | 直播摄入/拉流基准测试 | 仅限媒体面 |

**建议**：两阶段方法。第一阶段：criterion 用于纯热点路径（总线编码/解码、`Block` 序列化、消息扇出）。第二阶段：k6 用于 WS IM 负载。自定义 Rust 负载生成器暂缓——除非媒体面被验证为瓶颈。

**对现有系统的影响**
- 纯新增（`benches/` 目录，`tests/load/`）。无代码变更。
- 必须避免将 `criterion` 添加到 root `Cargo.toml`——它是一个 `[dev-dependency]`。

---

### 方向 C：事务性邮件基础设施（P1）

**为什么需要**
- 当前 `mailer.rs` 是架构中最大的未解决债务。密码重置返回 `void`——被调用者无法判断电子邮件是否已送达。
- 没有队列意味着邮件发送会串行阻塞请求处理程序。
- 没有 HTML 模板 = 没有品牌化 = 企业客户不满。
- `FakeSender` seam（`delivery.rs:185`）是一个可直接复用的模式——`FakeMailer` 同理。

**核心挑战**
- 邮件队列需要持久化存储（PG 表）和后台 worker（类似于 `webhook_delivery.rs` 的模式）。
- 退信处理（`Rejected`→`unregister`）继承自推送 bot——邮件同理。
- 每个工作区 SMTP 配置需要在 `Mailer` 中新增一个类似多例的工厂（当前是单例）。

**预期架构变更**
- 新增 `email_queue` PG 表（类似于 `webhook_delivery` 的模式）。
- 将 `Mailer` 重构为 trait（`EmailSender` + `FakeEmailSender`），已有的 seam 模式。
- HTML 模板引擎（`handlebars` 或 `tera`——无第三个选项）。
- 类似于 `run_webhook_dispatcher` 的 `run_email_dispatcher` worker。

**对现有系统的影响**
- `send_password_reset` 返回 `Result` 而不是 `void`——破坏性 API 变更，但仅在 `Mailer` 内部。
- `AppState` 将携带 `EmailSender` 而不是 `Option<Mailer>`——影响 2-3 个调用点。

---

### 方向 D：API 版本化 + 路由重构（P2）

**为什么需要**
- 2854 行的 `routes.rs` 是一个可维护性问题。单个文件的修改频率使其成为团队的瓶颈。
- 没有版本前缀意味着你不能在不破坏现有 API 客户端的情况下进行破坏性变更。
- 架构模式已经存在——每个模块的 `.merge(crate::<mod>::routes())`——问题在于中心化的路由注册表。

**预期架构变更**
- 新增 `/api/v1/` 前缀给所有现有路由。`Router::new().nest("/api/v1", api_v1_router)` + `.nest("/api", api_v1_router)` 提供向后兼容性。
- 将 `routes.rs` 拆分为 `routes/mod.rs` + `routes/v1.rs` + `routes/v2.rs`（未来）。
- 迁移策略：在过渡期内旧的 `/api/*` 和新的 `/api/v1/*` 都可用；文档更新指向 v1。

**对现有系统的影响**
- 中等。重构是机械化的——现有逻辑迁移到 `v1` 模块，新模块导入并合并。
- 无运行时影响——`/api/*` 和 `/api/v1/*` 在过渡期内都绑定到同一处理函数。

---

### 方向 E：SSRF 防线统一（P0→紧急）

**为什么需要**
- `webhook_ip_is_blocked` + `assert_webhook_url_safe` 已存在于 `webhooks.rs:206-275`——它们是 webhook 创建时的 SSRF 屏障。
- `ReqwestUnfurler`（`unfurl.rs:396-413`）完全不受保护——任何用户发送包含恶意 URL 的消息都会触发 server 向该 URL 发起 HTTP 请求。
- 这个修复很小（100 行可复用代码），而且攻击面比 webhook 更大（用户触发的 vs 管理员配置的）。

**技术解决方案**
- 将 `webhook_ip_is_blocked` 和 `assert_webhook_url_safe` 提取到 `aero-common` 或 `aero-storage` 的一个共享模块（例如 `safe_http.rs`）。
- 在 `ReqwestUnfurler::fetch()` 入口处调用 `assert_webhook_url_safe`。
- 将 `redirect::Policy::limited(5)` 应用于两个出站 HTTP 客户端（webhook sender 和 unfurl fetcher）。
- `ReqwestUnfurler` 已经有一个 `Unfurler` trait——屏障可以在此处应用为 trait 方法包装器，也可以通过 `SafeHttpClient` 中间件应用。

**对现有系统的影响**
- 影响小——纯重构 + 复制逻辑。
- 从 `webhooks.rs` 提取时需注意不要创建环形依赖（`aero-server` 依赖于 `aero-storage`，但共享 SSRF 屏障应位于 `aero-common` 或 `aero-storage` 中）。

---

## 3. 接口设计建议

### 3.1 关键抽象层

目前，代码库围绕几个关键的 trait 抽象具有良好的 seam：

| Seam | 位置 | 状态 |
|---|---|---|
| `Unfurler` trait | `unfurl.rs:339` | ✅ 良好——`ReqwestUnfurler` + `FakeUnfurler` |
| `WebhookSender` trait | `delivery.rs:185` | ✅ 良好——`ReqwestSender` + `FakeSender` |
| `UnfurlRepo`（PG-backed） | `unfurl.rs:467` | ✅ 良好——纯函数 + 数据库查询 |
| `EventBus` trait | `aero-bus` | ✅ 良好——NATS 实现在 `bus/` 下 |
| `Mailer` struct | `mailer.rs` | ❌ 不是 trait——没有 seam，没有 Fake |

**需要新增的**：
- **`EmailSender` trait**：`Mailer` 当前是一个 struct。将其转为 trait（`send_password_reset(&self, ...) -> Result<(), EmailError>`）并为测试添加 `FakeEmailSender`。Webhook 的 `FakeSender` 模式（`delivery.rs:185`）是直接的模板。
- **`LiveIngest` trait**：在 `aero-live-core` 中，如果尚未存在——三种摄入路径（RTMP、WHIP、SRT）都需要统一的 trait 以进行链路层测试。
- **`SafeHttpClient` wrapper**：统一 webhook + unfurl 的 SSRF 防护——一个包裹 `reqwest::Client` 并应用 IP 阻塞和重定向绑定的 struct。

### 3.2 向后兼容性模式

代码库已经遵循了一些优秀的模式：

- **serde `deny_unknown_fields` 谨慎使用**：`RoomEvent`/`StreamEvent` 解码使用 `deny_unknown_fields=false`——新字段被静默忽略。保持此状态。
- **`Option<T>` 用于现有字段**：用于新增的可选字段时使用——而非 `Vec` 默认值，这会导致客户端在不期望时收到空数组。
- **serde `#[serde(alias = "...")]` 用于重命名字段**：当重命名 JSON 字段时，先添加别名，移除旧名称，最后移除别名。三个阶段，无破坏。

---

## 4. 技术选型

### 4.1 当前技术栈——所做的选择正确吗？

| 层 | 选型 | 评估 |
|---|---|---|
| HTTP/WS | axum 0.7 | ✅ 正确——类型安全，tokio 原生，生态系统 |
| SQL | sqlx 0.8（编译期检查） | ✅ 正确——`query!()` 宏在构建时捕获 SQL 错误 |
| 缓存 | Redis 7 + fred 9 | ✅ 正确——sorted-sets 用于 presence/roster 是合适的选择 |
| 事件总线 | NATS JetStream | ✅ 正确——对于 IM 来说，JetStream 的持久性比 Kafka 的延迟更适合 |
| WebRTC | str0m | ⚠️ 承担风险——纯 Rust 但未在生产中大规模验证 |
| 可观测 | tracing + OTLP | ✅ 正确——Telemetry 后端可替换 |

**Key gap**: 在 `str0m` 方面存在技术赌注。如果它在 SFU 场景下表现出不可预见的稳定性问题，目前没有备用方案——整个媒体面都押注于它。设计规格中提到 P6 对 `webrtc-rs` 进行 PoC 对照——这应该作为时间盒限制的 spike。

### 4.2 推荐新增技术

| 技术 | 用途 | 优先级 |
|---|---|---|
| `criterion` | 热点路径基准测试（总线编解码、RoomEvent 序列化、OG 解析） | P0（轻量——仅作为 dev-dep） |
| `k6` | WS IM 负载测试 | P0（与 CI 集成） |
| `handlebars` 或 `tera` | HTML 电子邮件模板 | P1（任选之一——无第三个选项） |
| `lettre`（已经有） | SMTP 交付 | 已有——无需新增 |

### 4.3 自建 vs 采购

| 功能 | 决策 | 理由 |
|---|---|---|
| AI 推理（Anthropic/Voyage） | ✅ 采购（云 API） | 非差异化层；本地推理增加 10 倍运维成本 |
| SFU（str0m） | ✅ 自建（纯 Rust） | 差异化竞争力；控制媒体管道 |
| TURN | ✅ 自建（开源） | coturn 已成熟——无需重造 |
| S3/MinIO 附件 | ✅ 采购（MinIO/S3 API） | 标准存储层——无需自定义协议 |
| 邮件基础设施 | ⚠️ 自建（轻量） | 没有必要引入 AWS SES/SendGrid——100 行代码可以构建一个队列 |

---

## 5. 实施路线图

### 阶段 1：安全与可观察性加固（1-2 周）

| 项目 | 方向 | 工作量估计 |
|---|---|---|
| 统一 SSRF 防护（应用 `webhook_ip_is_blocked` 到 `ReqwestUnfurler`） | E | 2 小时 |
| 应用 `redirect::Policy::limited(5)` 到两个出站 HTTP 客户端 | E | 1 小时 |
| 新增 `criterion` 基准测试用于热点路径（总线编解码、`Block::serialize`） | B | 3 天 |

**风险**：极低——纯新增或重构代码。

### 阶段 2：部署基础设施（1 周）

| 项目 | 方向 | 工作量估计 |
|---|---|---|
| 多阶段 `Dockerfile`（编译 → 运行） | A | 1 天 |
| `docker-compose.yml` 包含 server 服务 | A | 2 小时 |
| 健康探针验证 + readiness 检查文档 | A | 4 小时 |
| `.dockerignore` + `.helmignore` | A | 1 小时 |

**风险**：低——纯新增文件。

### 阶段 3：邮件基础设施（2 周）

| 项目 | 方向 | 工作量估计 |
|---|---|---|
| 将 `Mailer` 重构为 `EmailSender` trait + `FakeEmailSender` | C | 1 天 |
| 新增持久化 `email_queue` PG 表 | C | 1 天 |
| `run_email_dispatcher` worker（类似 `webhook_delivery.rs` 模式） | C | 3 天 |
| 退信处理 + `retry_after` 尊重 | C | 1 天 |
| `send_password_reset` 返回 `Result`（无静默吞没） | C | 1 天 |
| 可选的 HTML 模板 + 品牌化 | C | 2 天 |

**风险**：中等——`send_password_reset` 的签名变更会影响调用者。需检查所有调用点。

### 阶段 4：API 版本化 + 路由拆分（2 周）

| 项目 | 方向 | 工作量估计 |
|---|---|---|
| 将 2854 行的 `routes.rs` 拆分为 `routes/mod.rs` + `routes/v1.rs` | D | 3 天 |
| 在 `/api/v1/` 下新增路由，同时保留 `/api/*` 向后兼容 | D | 2 天 |
| 新增文档指示优先使用 v1 | D | 1 天 |
| 新增跨方向端到端测试 | D | 2 天 |

**风险**：中等——重构需要谨慎以避免路由覆盖（Axum 在路由前缀冲突时不会报错）。

### 阶段 5：负载测试 + 容量规划（2-3 周）

| 项目 | 方向 | 工作量估计 |
|---|---|---|
| k6 脚本用于 WS IM 场景（多个房间、消息、连接/断开） | B | 5 天 |
| CI 集成 k6（阈值驱动的管道门控） | B | 2 天 |
| 记录 P99 延迟基准 + 内存/CPU 画像 | B | 3 天 |
| 如果发现问题，优化 `Hub::fan_out_raw` 瓶颈 | B | 3-5 天 |

**风险**：中等——负载测试可能发现未知的性能问题，需要架构变更。

### 优先级汇总

| 方向 | 阶段 | 工作量 | 业务价值 | 风险 |
|---|---|---|---|---|
| ⚠️ SSRF 统一（E） | 1 | 3 小时 | 高（安全性） | 低 |
| 🐳 部署（A） | 2 | 2 天 | 高（PoC 阻塞项） | 低 |
| 📧 邮件（C） | 3 | 8 天 | 中（PMF 杠杆） | 中 |
| 🏗️ 路由重构（D） | 4 | 8 天 | 中（可维护性） | 中 |
| 🔄 负载测试（B） | 5 | 10+ 天 | 高（容量验证） | 中 |

### 风险与缓解

| 风险 | 可能性 | 严重程度 | 缓解 |
|---|---|---|---|
| str0m SFU 稳定性 | 中 | 高 | 阶段 5 的基准测试涵盖 SFU 路径；保留 `webrtc-rs` PoC 作为预备方案 |
| 水平扩缩容竞争条件 | 中 | 中 | 依赖 Redis sorted-sets 作为集群状态——正确；新增特定的竞争条件负载测试 |
| 媒体面 CI 覆盖率 | 高 | 高 | 在容器化（阶段 2）之前无法完全解决；阶段 2 后可以在 CI 的 docker-compose 中集成真实浏览器 |
| 迁移 158—无向下兼容性 | 低 | 中 | 对于开发来说可接受；企业部署需要零停机策略——在阶段 2 中评估 |
| `routes.rs` 路由覆盖 | 低 | 高 | 阶段 4 应该增加路由冲突测试——Axum 不提供防护 |
