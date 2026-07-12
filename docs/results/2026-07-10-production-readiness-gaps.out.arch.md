Now I have a thorough understanding of the codebase. Let me compile the architectural analysis.

---

# Architecture Analysis: Aero IM

## 1. 架构评估

### 核心优势

**领域分离的 Crate 架构。** Aero IM 的 workspace 设计严格遵循"一个 crate = 一个能力域"的原则（`aero-common`/`aero-bus`/`aero-storage`/`aero-auth`/`aero-im-core`/`aero-live-*` 等 16 个 crate）。这种粒度在 Rust 生态中是健康的——它强制了编译期的依赖方向约束（如 `aero-common` 是叶子节点），避免了后期再拆分时破坏性重构。

**事件总线的进程/跨进程分界清晰。** 系统设计了双层扇出模式：
- **跨进程：** NATS JetStream 作为事实源，每个 subject 有单调 seq，durable consumer 提供 at-least-once 投递
- **进程内：** `Hub` 基于 bounded `mpsc` 通道扇出，每连接有独立的 `CancellationToken` 兜底断线

**背压策略务实。** `WsSender` 的 `try_send` + 丢帧 + `RESYNC_FRAME` 的退化模式，比阻塞 backpressure 更适合实时消息场景。9 个 bot 消费者各自独立的 `loop { subscribe → process → ack }` 结构保证了隔离开关——一个 bot panic 不会影响其他 bot。

**迁移管线嵌入二进制。** `sqlx::migrate!("../../migrations")` 在编译期将迁移文件嵌入二进制，避免了生产环境中迁移与二进制不匹配的典型事故。

### 关键架构债务

#### 1）安全策略不一致（SSRF）

`assert_webhook_url_safe`（在 `webhooks.rs`）做了完整的 DNS 解析 + 私有 IP 检查，但 `ReqwestUnfurler::fetch`（在 `unfurl.rs`）只有 timeout + UA，没有 IP 过滤、没有 redirect 限制。这是典型的安全不一致——同一种模式（服务器发起出站 HTTP 请求）有两种不同的安全姿态。

更根本的问题是**缺少统一的出站 HTTP 客户端抽象**。当前有两个深度耦合了 `reqwest::Client` 的位置（webhook 派发和 unfurl 抓取），外加至少三个地方有潜在的出站 HTTP 调用（AI API 调用、OIDC 发现端点、call-bridge 控制调用）。如果将来增加第 N 个出站 HTTP 路径，该路径需要重新实现 IP 过滤——这是架构层面的职责重复。

#### 2）消费者生命周期管理不可观测

`background.rs` 中的每个 bot/dispatcher 通过 `TaskTracker::spawn` 启动，但系统没有以下能力：
- 查询每个消费者的健康状态（是运行中、挂起、还是 exit 了）
- 知晓消费者的 NATS backlog 积压程度（metrics_tasks.rs 只追踪 2/9 个消费者）
- 在消费者连续处理失败时自动熔断

当前模式是"spawn 后忘记"——如果某个消费者的 `loop` 因为未捕获的 panic 退出，整个进程不产生告警。`TaskTracker` 提供 `wait` 能力但缺少 `is_alive` 查询。

#### 3）前端的令牌存储和传输架构属于安全负债

Web 客户端通过以下方式暴露令牌：
- `localStorage` 存储 access/refresh token（任何 XSS 或 CDN 投毒即可盗取）
- WebSocket 连接时令牌通过 URL query 参数传递（`/ws?token=...`，出现在服务器日志、浏览器历史、Referer 头中）
- 没有 CSP（默认关闭）
- hls.js 从 CDN 加载（无 SRI）

这不是"前端架构"问题，而是**客户端安全架构模式**问题。当前的架构决策是把可信边界放在了浏览器沙箱边界上，但事实上这个边界已经被 XSS/扩展/CDN 攻破。

#### 4）API 设计在合规导向的路径上功能不完整

`conversation_export.rs` 有硬编码的 `EXPORT_CAP = 5000`，响应中 `count` 是实际返回的消息数而非房间消息总数，且没有 cursor 续传。对于 GDPR 数据可携带权要求来说，这个接口只能导出"最近的 5000 条消息"，而非"我的全部消息"。这不是 bug——它是一个设计决策（单次请求大回复 = 内存压力），但这个决策没有为用户暴露"总大小"或"分批导出"的能力。

### 合理但需关注的设计决策

**`aero-storage/lib.rs` 只 re-export 一个 `generate_token`。** 这是一个精细的决定——避免 `webhook`/`invitation`/`scim` 的同名 helper 在根级撞名。但这意味着新增模块时开发者必须理解"哪个 helper 被 re-export 了、哪个需要`use`子路径"。这是架子上的地雷——新贡献者很容易在 `lib.rs` 中加一个 `pub use invitation::generate_token` 从而破坏现有引用。

**枚举 `kind` 的 tag 碰撞处理。** `#[serde(tag="kind")]` 在 variant 内字段重命名（`call_kind`/`notify_kind`）规避了运行时 panic。这是一个序列化层的工作区，而非架构层的优雅解决方案。如果 LLM 生成的代码加了另一个带有 `kind` 字段的 variant，这个坑还会重复出现。

---

## 2. 扩展方向

### 方向 A：构建统一出站 HTTP 网关层

**为什么需要。** 当前至少有 5 个出站 HTTP 调用点：
1. Webhook 派发（`webhooks.rs` — SSRF √）
2. Unfurl 抓取（`unfurl.rs` — SSRF ✗）
3. AI API 调用（Anthropic/Voyage/Whisper）
4. OIDC 发现端点
5. Call-bridge 跨节点控制调用

各自独立构建 `reqwest::Client`、各自设置 timeout、各自决定安全策略。这种分散模式导致 SSRF 防护的覆盖是随机的——必须有人记得每个新 HTTP 调用点也需要 IP 过滤。

**核心挑战。**
- 抽象边界：HTTP 客户端的配置选项（timeout/redirect/UA/proxy）与业务关心的"是否可以访问这个 URL"之间的边界在哪里
- IP 过滤的 DNS rebinding 防御：目前 `assert_webhook_url_safe` 只在创建时 resolve——`assert_webhook_url_safe` 是创建时检查，投递时没有 pin 到 IP。对于 unfurl 这种即时抓取的场景，区分「创建时的 SSRF 检查」和「运行时 resolve 到私有地址」需要不同的策略

**预建议的架构变更。**
```
// 新增 crate: aero-http-client 或 aero-common::http
// 提供统一的 ClientFactory 或 HttpClient 类型

pub struct HttpClient {
    inner: reqwest::Client,
    ip_filter: Arc<dyn IpFilter>,
    redirect_policy: RedirectPolicy,
    // ...
}

pub trait IpFilter: Send + Sync {
    fn check(&self, addrs: &[SocketAddr]) -> Result<(), IpBlocked>;
}

// 标准实现: 禁止私有/loopback/link-local + metadata IP
// 测试实现: 允许 127.0.0.1（用于集成测试）
// 宽松实现: 无过滤（用于 AI API 调用，或需要告警而非拒绝）
```

**对现有系统的影响。**
- 中等迁移成本：替换 `reqwest::Client` 的 5 处直接构造
- `aero-storage::unfurl::ReqwestUnfurler` 需接纳新的 `Client` 或 trait 参数
- 与 webhook 模块的 `assert_webhook_url_safe` 去重——后者逐步由新结构体替代

### 方向 B：消费者生命周期注册与观测

**为什么需要。** 当前 9 个 NATS 消费者各自的连接状态、积压量、错误率不可观测。当生产出现"消息没被处理"时，运维人员无法区分"消费者挂了"和"消费者在处理但很慢"和"NATS 丢了消息"。当前 metrics_tasks.rs 只追踪 `aero-server` 和 `aero-ai` 两个 consumer，其他 7 个完全不可见。

**核心挑战。**
- NATS JetStream 的 consumer API 本质是流式的——查询 backlog 需要额外的 JetStream 管理 API 调用（已经做了 `consumer_pending`），但每个 stream 的 consumer 枚举需要 `list_consumers`。对于 durable consumer（bot 使用的），这是可行的；对于 ephemeral consumer（live bus listener），需要额外设计
- "消费者挂死"的检测不能只靠失败率——消费者可能还在运行但逻辑错误导致每条消息都 skip。需要消息级成功率/跳过率计数
- 熔断器 vs 重启：如果消费者连续 N 条失败，应该重启还是应该进入降级模式？当前所有消费者都是纯 `warn!` + continue，不可能通过监控识别

**预建议的架构变更。**
```rust
// 新增: ConsumerRegistry（可以是 AppState 的一部分）
// 背景任务创建 consumer 时注册
pub struct ConsumerRegistry {
    consumers: Arc<DashMap<String, ConsumerState>>,
}

#[derive(Clone)]
pub struct ConsumerState {
    pub name: &'static str,
    pub subject: &'static str,
    pub consumer_kind: &'static str, // "durable" | "ephemeral"
    pub started_at: Instant,
    pub messages_processed: Arc<AtomicU64>,
    pub messages_failed: Arc<AtomicU64>,
    pub last_error: Arc<RwLock<Option<String>>>,
    pub cancel: CancellationToken,
}
```

- `metrics_tasks.rs` 遍历 `ConsumerRegistry` 上报 Prometheus gauge（当前只上报 2 个 hardcoded consumer）
- 健康 API 端点在 `GET /health/consumers` 返回状态
- 可选：加入熔断器（连续 N 条失败后向 cancel token 发信号，外层 spawn 循环自动重启）

**对现有系统的影响。**
- 低迁移成本：每个 `background.rs` 中的 spawn 块只需在 spawn 前注册 `ConsumerState`
- 不影响现有消息处理逻辑
- `metrics_tasks.rs` 的 NATS backlog 采样可以改为从 `ConsumerRegistry` 读取 consumer 列表，然后对每个 consumer 调用 `consumer_pending`

### 方向 C：前端安全架构升级

**为什么需要。** 当前前端安全依赖于：
- `localStorage` 存储 token（任何 XSS = 账号被盗）
- WS 认证通过 URL query parameter（token 暴露在日志/Referer/浏览器历史）
- CSP 默认关闭（第 3 方 CDN 脚本有全量浏览器权限）
- CDN 脚本无 SRI（hls.js 被投毒可执行任意代码）

对于 To-B 协作平台的定位，这相当于前端安全栈近乎裸奔。

**核心挑战。**
- httpOnly cookie 不能直接用于 WebSocket 认证，因为浏览器 WebSocket API 不会自动附加 `Cookie` 头用于 ws:// 连接。需要设计替代方案：
  - **选项 A**：WS 建立后通过第一条帧进行认证（当前 `ws.js` 的 `send({ type: 'authenticate', token })` 模式），但这样连接已经建立，攻击者可以在认证前发送恶意帧
  - **选项 B**：REST API 使用 httpOnly cookie + CSRF token，WS 使用短期 token（通过 REST 获取），这样 WS token 泄露的风险范围缩小到 token 有效期
  - **选项 C**：全站走同源策略 + 无第三方资源 + SRI + CSP——完全消除 XSS 向量，那么 localStorage 也就够用了

- 退守 localStorage 的混合方案：PAT（Personal Access Token）本质上是长期静态密钥，无法存储于 httpOnly cookie（因为 PAT 需要被 JS 读取才能放入 `Authorization` 头）

**预建议的架构变更。**

分两阶段：

_Phase 1（防 XSS 横向移动）：_
- 启用 CSP（`AERO_CSP_POLICY` 从默认空变成默认严格策略 `default-src 'self'; script-src 'self' 'strict-dynamic' https://cdn.jsdelivr.net;`）
- 为 CDN 资源添加 SRI（`integrity` 属性 + 失败时 polyfill fallback）
- WS 认证改为通过第一条帧内的短期 token（非 URL query parameter）

_Phase 2（存储安全）：_
- REST API 切换到 httpOnly cookie + CSRF token（`Set-Cookie: aero_token=...; HttpOnly; SameSite=Strict; Secure; Path=/`)
- WS 使用从 REST 获取的短期 token（1 分钟过期，每次 WS 重连重新获取）
- PAT（长期密钥）仍走 `Authorization: Bearer` 头——但 PAT 用户可以接受更高级别的安全责任

**对现有系统的影响。**
- 中等：REST 侧需要注入 cookie 设置逻辑（当前 `auth_login` 只返回 JSON）
- 低：WS 认证帧需要在 `ws/mod.rs` 中增加一个"认证前缓冲区"——连接建立后不立即加入 Hub，等认证帧确认后再加入
- 低：前端 `api.js` 需要适配 cookie 自动包含（fetch credentials: 'same-origin'）

### 方向 D：合规数据导出管线

**为什么需要。** GDPR/CCPA 数据主体访问权请求要求"全部个人数据"。当前 `conversation_export.rs` 的 5000 条硬限制和 `me_export.rs` 的完整导出策略之间有一个缺口——大房间的用户无法完整导出自己的对话数据。

**核心挑战。**
- 无状态 API 不支持大数据导出：5000 条消息的 JSON 可能达 10-50 MB，单次 JSON 响应在反序列化和传输过程中都会产生尖峰内存
- 导出没有速率限制：用户可以反复调用 `GET /api/rooms/:id/export`，每次扫描一次 `list_recent`（`O(n)`），在没有 rate limit 情况下可以构成小的数据库压力
- 游标续传需要处理消息被删除导致游标指向已删除消息的问题

**预建议的架构变更。**
```rust
// 可选方案 A: 同步游标续传
// 在现有 API 上增加 `?before=` 和 `?limit=` 参数
// - `GET /api/rooms/:id/export` → 返回最旧 N 条 + 一个 cursor
// - `GET /api/rooms/:id/export?cursor=<cursor>` → 下一批
// 响应增加 `total_count` 字段（通过 SQL COUNT 查询，缓存 5 分钟）
// 
// 可选方案 B: 异步导出
// - `POST /api/rooms/:id/export-jobs` → 异步创建导出任务
// - `GET /api/export-jobs/:id` → 返回任务状态（pending/ready/expired）
// - `GET /api/export-jobs/:id/download` → 下载完整 JSON 文件（临时 blob 存储，24h TTL）
// 适合大数据量（>100k 消息），但增加了复杂性
//
// 可选方案 C: 流式 NDJSON 响应
// - `GET /api/rooms/:id/export?format=ndjson`
// - 每条消息一行 JSON，客户端通过 `fetch` + ReadableStream 逐行消费
// - 内存 O(1)，但需要服务端 streaming 支持
```

方案 A 影响最小，建议立即实施。方案 C 在 Axum 中可以通过 `BodyStream` 实现，是纯增量变更。

**对现有系统的影响。**
- 低（方案 A）：在现有 `export_conversation` 上增加 `before` 和 `limit` 参数。响应增加 `total_count`（需一个 `SELECT COUNT(*)` 查询，`count` 列为标准 BTREE 索引）
- Rust 端的变更集中在 `conversation_export.rs`，不影响其他模块

### 方向 E：结构化工程管线和治理框架

**为什么需要。** 当前项目有 50+ smoke 脚本和 6 个静态检查脚本。它们全部需要人工触发。而 CI 管线模板（`.github/workflows/ci.yml`）已经完整定义了 `check/test/size-check/truth-check/web-check/dependency-check/security-audit` 7 个 job，覆盖率检查和集成测试也有注释模板——只需要取消注释 + 配置 runner。

更本质的问题：管线不只是自动化，而是**工程治理的强制执行机制**。如果 `truth-check.sh` 永远不会自动跑，那么 "写了但没接线的 builder" 迟早会溜进 `main`。

**核心挑战。**
- 集成测试依赖 PG/Redis/NATS 三个外部服务，CI 分钟消耗可能高（方向 D 的 docker-compose 依赖镜像拉取）
- smoke 脚本部分是 Python（`smoke_*.py`）部分是 shell，需要一个统一的测试框架入口
- 慢测试（`#[ignore]` 标注的 PG 门控 test）不能放在快速反馈的 PR check 中，但也不能完全无人监管

**预建议的架构变更。**
```yaml
# CI 管线分层：
# Layer 1: 快速（<5min）— 每次 push
#   cargo check + clippy + file-size + truth-check + web-check
# Layer 2: 中等（<10min）— 每次 PR merge 到 main
#   cargo test --workspace --lib + deny check
# Layer 3: 慢速（<30min）— nightly / 标签触发
#   cargo test -- --include-ignored + smoke test suite
#   （需要 PG/Redis/NATS services matrix）
```

**对现有系统的影响。**
- 低：只需取消注释 CI 文件 + 配置 runner（GitHub Actions / GitLab CI / local runner）
- 需要为 smoke 脚本选择一个 wrapper（`make smoke` / `just` / 简单 shell `for f in scripts/smoke_*.py; do python $f; done`）
- 集成测试的 `DATABASE_URL` 需要 CI 环境变量或 docker-compose 服务

---

## 3. 接口设计建议

### 3.1 关键模块接口原则

**出站 HTTP（方向 A）**：接口应当允许调用方指定"我需要一个 HTTP 客户端，适用于 `{webhook|unfurl|ai-api|oidc}`"场景，而不是"我自己构建一个 Client"。这与当前 `ReqwestUnfurler::new()` 的构造方式相反。

```rust
// 建议接口模式

pub trait OutboundHttp {
    /// 获取一个适用于给定场景的 HTTP 客户端。
    /// 场景决定了安全配置（SSRF 过滤策略、redirect 策略、UA）。
    fn client_for(&self, scenario: HttpScenario) -> Arc<HttpClient>;
    /// 便捷方法: 获取 + 发送
    async fn fetch(&self, scenario: HttpScenario, req: Request) -> Result<Response>;
}

// 场景枚举使得"新增一个场景"是 type-safe 的——编译器会提示
// 每个场景需要配置哪些安全参数。
pub enum HttpScenario {
    Webhook,
    Unfurl,
    AiApi(AiApiKind),
    Oidc,
    CallBridge,
}
```

**消费者注册（方向 B）**：接口应当使得消费者在 spawn 时即完成注册，消除"我忘了加 metrics"的问题。

```rust
// 建议接口模式

pub trait ConsumerSupervisor {
    /// 创建一个命名的消费者任务。
    /// - `name`: 消费的标识，用于 metrics 和健康监测
    /// - `consumer`: 实际的消费逻辑（async fn）
    /// - 返回一个句柄，可以用于查询状态和取消
    fn spawn_consumer<F, Fut>(&self, name: &'static str, config: ConsumerConfig, f: F) -> ConsumerHandle
    where
        F: FnOnce(ConsumerCtx) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send;
}

pub struct ConsumerCtx {
    pub shutdown: CancellationToken,
    pub metrics: ConsumerMetrics,
}
```

这消除了当前 `background.rs` 中手动 `tracker.spawn(async move { ... })` + 散落的 metrics 计数的模式。

### 3.2 是否需要新的抽象层

**需要：统一出站 HTTP 层。** 原因：
- 当前 5 个出站 HTTP 调用点，安全策略不一致
- 未来还可能增加（如 webhook 重试、AI batch API、KMS 调用）
- 测试困难：当前 unfurl bot 测试需要 mock `Unfurler` trait，webhook 测试需要独立 mock——两层 mock 本可以统一

**不需要：业务层之上的编排层。** 有人可能会建议在 `aero-server` 之上再加一层工作流引擎（Temporal/暂态）。当前的事件 DAG + NATS + 9 个 consumer bot 的模式虽然朴素，但足够灵活，引入编排引擎只会增加复杂性和运维负担。

**边缘情况需要：数据导出 API 的 cursor 编解码。** 如果选择方向 D 方案 A（游标续传），需要决定 cursor 的序列化格式。建议使用 Web 安全的 base64 编码的内部结构：

```rust
// 内部结构（不暴露给客户端）
pub struct ExportCursor {
    pub last_message_id: MessageId,
    pub room_id: RoomId,
    pub timestamp: time::OffsetDateTime,
}

// 暴露给客户端的是 opaque string
// ExportCursor → base64(JSON) → opaque string
// 客户端在 query param 中传递 cursor=...
// 服务端解码后校验完整性（cursor 的 room_id 必须匹配 URL 路径中的 room_id）
```

### 3.3 向后兼容性

**当前 API 没有版本前缀。** 所有 REST 路由如 `/api/rooms/:id/export` 没有 `v1` 前缀——这是一个有意的简化决策（文档说"渐进式交付"）。对于方向 D 的扩展，有以下兼容策略：

- **方案 A（推荐）**：保持现有端点不变，增加可选的 `before`/`limit` 参数。现有客户端不需要 `before` 时行为不变。新增客户端可以使用 `before` 分批拉取。
- **方案 B**：新端点 `GET /api/v2/rooms/:id/export`。更清晰但需要维护两套路由。

如果选择方案 A，需要确保：当 `before` 不存在时，现有逻辑完全不变。这是一个纯增量的兼容变更。

---

## 4. 技术选型

### 4.1 是否需要引入新技术栈

| 方向 | 建议 | 理由 |
|------|------|------|
| 出站 HTTP 网关 | ❌ 不引入新框架 | 只需在 `aero-common` 中封装 `reqwest`，无需另加 crate |
| 消费者观测 | ❌ 不引入新框架 | 原生 `AtomicU64` + `DashMap` 足够，不需要 operator 框架 |
| 前端安全 | ❌ 不引入新框架 | CSP + SRI + httpOnly cookie 是浏览器原生能力 |
| 合规导出 | ❌ 不引入新框架 | cursor 基 pagination 已在 `me_export.rs` 中存在模式 |
| 工程管线 | ✅ 引入 CI runner | 这是基础设施决策而非技术栈决策 |

**结论：当前五个方向中没有一个需要引入新的第三方框架或底层技术栈。** 所有问题都可以通过"已有模式的一致化扩展"来解决。这是好信号——意味着当前技术栈的选择（Rust + tokio + axum + sqlx + NATS + Redis）足够应对这些架构缺口。

### 4.2 第三方依赖评估标准

**当需要引入新依赖时，建议评估以下维度（按权重排序）：**

1. **审计状态**：是否经过独立安全审计？如果没有，`// #![forbid(unsafe_code)]` + 纯 Rust 实现可视为替代（如 `bergshamra` 的定位）
2. **unsafe 使用**：项目 lint 强制 `unsafe_code = "forbid"`，新依赖必须提供 `#![forbid(unsafe_code)]` 或明确说明 unsafe 使用的范围
3. **编译时间**：如果引入大型框架（如 ORM/WebRTC stack/ML 推理库），需要在 workspace `Cargo.toml` 中作为可选依赖，不堵非相关 crate 的编译
4. **许可证合规**：项目采用 MIT OR Apache-2.0，新依赖需要兼容（cargo-deny 可自动检查）
5. **社区活跃度**：至少最近 6 个月有维护活动

**一个具体参考：str0m 的选择是合理的。** 纯 Rust WebRTC（`#![forbid(unsafe_code)]`），编译时间仅影响 `aero-live-webrtc`/`aero-live-whip` 两个 crate，不影响 IM 核心路径。项目将 str0m 声明在这两个 crate 各自的 `Cargo.toml` 中而非 root，是正确的依赖隔离策略。

### 4.3 自建 vs 采购的决策依据

项目当前阶段（MVP + 渐进式交付）的正确策略是**自建优先**，因为：
- 项目的核心差异化（AI-Native IM）在业务逻辑层，而非基础设施层
- Rust 生态提供的底层基础设施足够构建这些能力
- 引入 SaaS/PaaS 依赖会妥协模型的部署独立性（项目目标之一）

**何时考虑外部服务：**
- AI API（Anthropic/Voyage/Whisper）——已有抽象层 `AiService`，允许将来切换 provider
- 推送通知（FCM/APNs）——已封装 `push_gateway` trait + `FakeGateway`
- 未来：如果客户要求 SAML/OIDC IdP 集成，应考虑使用成熟库（当前自建 SAML 验证的逻辑在 `saml.rs`）

---

## 5. 实施路线图

### 优先级排序

| 方向 | 优先级 | 理由 |
|------|--------|------|
| A：统一出站 HTTP + SSRF 修复 | **P0** | 安全审计一票否决项，约 30 行核心代码关闭一个 SSRF 入口 |
| E：工程管线激活 | **P0** | "写了但没跑" 的模式如果不修正，未来所有变更都可能引入 "编译过但死代码" |
| B：消费者观测 | **P1** | 监控缺口影响排障速度，但系统在实际运行"如果没出问题"不会造成数据丢失 |
| C：前端安全升级 Phase 1（CSP+SRI+WS auth） | **P1** | 降低 XSS 横向移动能力，但需要前端配合变更 |
| D：合规导出 + 游标续传 | **P2** | 当前 5000 条 cap 对于大多数房间够用。GDPR 请求通常是低频事件 |
| C：前端安全升级 Phase 2（httpOnly cookie） | **P2** | 更高安全姿态，但需要前后端协调变更，且不影响功能交付 |

### 阶段划分

**Phase 1（2-3 周）— P0 安全 + 工程基础**

```
Week 1:
Day 1-2:  ReqwestUnfurler 加入 IP 过滤（复用 assert_webhook_url_safe 的 resolv + 检查逻辑）
          新增 unified IP filter function 在 aero-common 或 aero-server::http_util
Day 3-4:  ReqwestUnfurler 加入 redirect 限制（max_redirect=5 + 禁止 redirect 到私有 IP）
Day 5:    添加 url_allow_private 白名单 env 和文档安全说明

Week 2:
Day 1-2:  CI 取消注释（check/test/size-check/truth-check/web-check/dependency-check）
          配置 GitHub Actions runner 或 self-hosted runner
Day 3-4:  修复 CI 运行中发现的问题（如果有）
Day 5:    CI 安全审计 job（cargo deny）取消注释

Week 3:
Day 1-2:  文档更新（AGENTS.md §4.1 加"每个出站 HTTP 调用必须通过统一安全检查"规则
Day 3-5:  代码审查 + 性能测试 + 集成测试
```

**Phase 2（2 周）— 可观测性 + 前端 Phase 1**

```
Week 4:
Day 1-2:  ConsumerRegistry 数据结构 + 注册机制
Day 3-4:  background.rs 中 9 个 consumer 注入注册
Day 5:    metrics_tasks.rs 从 hardcoded 列表改为遍历 ConsumerRegistry

Week 5:
Day 1:    健康 API 端点 GET /health/consumers
Day 2-3:  前端 CSP 配置 + SRI 添加 + WS token 从 URL 移除
Day 4:    前端 WS 认证帧（第一条 frame 完成认证前不入 Hub）
Day 5:    集成测试 + 文档
```

**Phase 3（2 周）— 合规导出 + 前端 Phase 2**

```
Week 6:
Day 1-2:  export API 增加 before/limit 参数（保持向后兼容）
Day 3-4:  total_count 字段（缓存 5 分钟）
Day 5:    集成测试 + rate limit 保护（防止滥用）

Week 7:
Day 1-3:  后端登录响应增加 Set-Cookie + CSRF token 端点
Day 4-5:  前端切换 REST 认证到 cookie + WS token 获取模式
```

### 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| CI runner 配置后发现测试需要 Aero-specific 基础设施 | 中 | 中 | 先只激活 Layer 1（check + clippy + 静态检查），Layer 2/3 逐步开启 |
| `ReqwestUnfurler` IP 过滤导致合法 URL 被误拦截（如 corporate proxy 返回私有地址） | 低 | 低 | env gate `AERO_UNFURL_ALLOW_PRIVATE=1`（文档标注安全影响）；fail-open 而不是 fail-close（unfurl 失败只是 log + skip，不阻塞消息发送） |
| CSP 导致现有 web 功能异常 | 中 | 中 | CSP 先以 `report-only` 模式部署，收集违规报告一周后再切换到 enforce 模式 |
| WS 认证帧变更导致现有客户端断连 | 中 | 高 | 后端兼容两种认证模式（URL query token 和帧认证）至少一个 release 周期，记录 deprecation warn |
| 前端 cookie 切换后发现 PAT 无法工作 | 高 | 低 | PAT 保持 `Bearer` 头方案，token cookie 和 Bearer 两套认证并行 |
| `ConsumerRegistry` 的 `cancel` token 被错误调用导致消费者意外终止 | 低 | 高 | `ConsumerHandle` 不暴露 `cancel()` 方法，只有 `Supervisor` 可以取消；消费者通过 `ConsumerCtx.shutdown` 自然结束 |

---

## 总结

当前验证文档揭示的五个方向非常精准地指向了 Aero IM 的架构断层：

1. **SSRF** 不是"一个 bug"，而是缺少统一出站 HTTP 抽象的架构折射
2. **CI/CD** 不是"运维配置"，而是工程治理机制缺失的系统性风险
3. **导出截断** 不是"性能优化"，而是 API 设计与合规需求之间的接口分离
4. **令牌安全** 不是"前端疏忽"，而是客户端安全架构模式的根本选择
5. **消费者不可观测** 不是"监控遗漏"，而是背景任务生命周期管理的架构空白

这些断层具有一个共同特征：它们都是**正确的基础设施抽象缺失**导致的。代码层面修复很简单（每个方向核心变更约 30-200 行），但真正有价值的是**在修复的同时建立一致性模式**——如统一 HTTP 客户端抽象、统一 Consumer 注册机制——这样下次就不会再出现"有 5 个 HTTP 调用点但只有 1 个做了安全保护"的情况。

架构层面的建议优先级：**先修正一致性模式（Phase 1），再填充监控空洞（Phase 2），最后做安全前置防御加固（Phase 2-3）**。这种节奏使得每个阶段都有可交付的价值，且不阻塞功能开发。

**唯一需要谨慎对待的变更**是前端安全 Phase 2（httpOnly cookie）——因为它涉及前后端的认证流程重构，且 PAT 需要保留 `Bearer` 兼容路径。建议 Phase 2 的 WS 短期 token + cookie 方案通过 RFC-style 文档先内部设计审查，再投入实现。
