现在我已全面掌握项目架构。以下是深度分析。

---

# Aero IM 架构分析报告

## 1. 架构评估

### 1.1 当前架构优势

**事件驱动骨架是正确的选择。** 该项目围绕 NATS JetStream 构建了坚实的事件驱动核心——DAG（`im.room.*` 用于持久消息，`live.stream.*` 用于投递质量较低的事件）将每个进程 Hub 扇出的职责与跨节点持久性明确分离。这种模式与 WebSocket 推送架构自然契合，因为 WebSocket 本质上只是持久（长连接）的本地扇出管道。

**Crate 分层与职责边界优秀。** 自下而上的 crate 依赖图（common → bus → storage → auth → im-core → ai → server）没有循环，并明确区分了基础类型、基础设施和业务逻辑。每个 crate 都有清晰的领域边界，代码组织有序。

**测试策略扎实。** 全 hermetic 单元测试（>800 个）+ 使用孤立 seam（`#[async_trait]` trait 如 `Unfurler`/`WebhookSender`）进行网络/DB 离线测试 + 独立 PG 门控集成测试。测试策略确保核心逻辑在 CI 中是可验证的，不依赖基础设施。

**媒体面协议栈深度显著。** WHIP/WHEP、SFU（包含 Simulcast + RTCP）、SRT（包含 AES-CTR + ACK/NAK）、HLS（含真 MPEG-TS muxing）——覆盖范围比大多数自建 WebRTC/SRT 项目更深。

### 1.2 关键架构局限性

**5 个分析方向暴露了 3 个系统性盲点：**

1. **SSRF 保护完全缺失（P0）** ——`ReqwestUnfurler` 和 `ReqwestSender` 都没有设置 `redirect()` 策略、IP 黑名单，没用 `danger_accept_invalid_certs`，也没对目标 URL 进行网络级校验。这表明存在一个模式层面的问题：**任何出站 HTTP 调用都没有经过中央安全网关。** 这是架构分离需要修复的地方。

2. **分布式追踪存在缺口（traceparent）** ——`common` 层已经声明了 `traceparent` 类型和 stamp 函数（`stamp_traceparent`），但基于 AGENTS.md 的描述和实际代码，这个追踪头在事件生产-消费链中并没有被始终传播。**可观测性骨架存在但未被实际接通。**

3. **消息撤回收敛性不足**——虽然 round-18 已覆盖，但收敛性保证（重试期间不插入重复的撤回事件）并未在架构层面得到解决。

4. **NATS 流声明式但缺乏变更管理**——流（`IM_MESSAGES`、`LIVE_EVENTS` 等）在 `JetStreamBus::bootstrap()` 中通过 `get_or_create_stream` 声明，但这意味着首次创建后配置文件不可变。NATS 生产化需要进行正式的配置漂移管理。

### 1.3 架构债务

| 债务 | 严重程度 | 说明 |
|---|---|---|
| **出站 HTTP 缺乏安全网关** | **P0** | 每个 `reqwest::Client` 都独立配置。没有集中式的出站连接策略（redirect 策略、IP/网络黑名单、证书校验、超时策略）。威胁模型应假定不可信 URL 作为输入。 |
| **NATS `-js` 缺失** | **P2** | 连接路径 `async_nats::connect(&cfg.url)` 没有附加 `-js` 参数。JetStream API 在核心 NATS 之上，但 `-js` 参数确保了客户端使用正确的协议版本——看似是个小问题，但在生产集群中可能产生微妙的不兼容。 |
| **drain 与暂停语义未完全实现** | **P1** | 活跃度/就绪探针存在，但出站 HTTP 客户端的断路器逻辑（如 webhook 背压）停留在 token-bucket 层面，未实现真正的 circuit-breaker 状态机（已关闭 → 已打开 → 半开）。 |
| **迁移编译期嵌入** | **P1（可操作性）** | `sqlx::migrate!("../../migrations")` 将整个目录编译进二进制文件——这在开发中很方便，但使部署原子性变得复杂；二进制文件必须精确匹配数据库状态。 |
| **配置治理** | **P1（可操作性）** | 双下划线（`AERO__SECTION__KEY`）与单下划线（`AERO_RATE_LIMIT_PER_SEC`）env 变量并行——这种不一致导致了可发现的系统性问题。|

---

## 2. 扩展方向

### 方向 A：中央出站 HTTP 安全网关（P0）

**为什么需要：** 当前 unfurl + webhook + AI 回调 + OIDC 发现每个都创建独立的 `reqwest::Client`，各自的 timeout/user-agent/redirect/cert 策略各不相同。SSRF 是真正的 P0，因为 unfurl bot 允许攻击者发布一个包含 URL 的消息，该 URL 指向 `http://169.254.169.254/`（云 metadata API），如果该 URL 被重定向到内部地址，甚至也能访问。这不是一个理论问题——这是真实漏洞。

**核心挑战：**
- 需要在模块化（每个调用方可自定义配置）与集中安全（全局禁止 RFC 1918/私有 IP 范围、禁止云 metadata 端点、强制 redirect 限制）之间取得平衡
- 需要与令牌桶配合良好——如果不做正确的架构抽象，bounded 的并发与全局连接池会互相冲突
- DNS 解析是难点——你需要在建立 TCP 连接 *之前* 检查目标 IP，而不是在 HTTP 响应返回后

**预期架构变更：**
- 新增一个 `aero-egress` crate（或扩展 `aero-common`）作为中央出站 HTTP 网关
- 抽象：`EgressGateway` trait + `SecureEgressGateway` 实现，包含：
  - 预解析 DNS → 检查 IP 是否属于私有/本地/云 metadata 范围
  - 全局 redirect 策略（最多 3 次重定向 + 禁止跨协议 redirect HTTP→file 等）
  - 校验响应 Content-Type / body 大小
  - 全局连接池 + 超时策略
  - 可选：mTLS 用于企业 webhook 端点
- 所有现有出站 HTTP 调用都迁移到这个网关

**对现有系统的影响：** 中。影响到 `ReqwestUnfurler`、`ReqwestSender`、`aero-ai/anthropic.rs`、`aero-ai/embed.rs`、`aero-ai/transcribe.rs`、`aero-auth/oidc.rs`、`S3BlobStore`、`WhepUpstreamSource`。每个调用点都需要改为使用网关，但 API 变化很小（`gateway.fetch(url)` vs `client.get(url)`）。

### 方向 B：实时事件管道可观测性与收敛性（P1）

**为什么需要：** 分析发现 traceparent 骨架已存在但从未实际流通。在跨 10+ 个总线订阅者、每个都有独立 ack/nack 的异步事件管道中，没有分布式追踪意味着：
- 无法 debug 端到端延迟（消息发布到 WebSocket 扇出完成）
- 无法追踪消息丢失（nack 后发生了什么？重投次数？）
- 无法关联事件与下游副作用（webhook delivery、AI 审核判决、push 通知）

**核心挑战：**
- 追踪头需要在每个总线边界传播：`publish_room_event` → stamp traceparent → NATS → consumer → ack/nack → 扇出 → WS
- 必须向后兼容（不支持追踪的旧消费者必须忽略这个头）
- 追踪采样策略（head-based vs tail-based）——对于高频 `live.stream.*`，你可能只想采样 1% 的事件

**预期架构变更：**
- 将 `stamp_traceparent` 集成到 `JetStreamBus::publish` 中（默认注入头）
- 在 consumer 端：解码 traceparent → 创建一个 tracing span → 将传播标记为已继续
- 暴露 OTLP span 导出（gRPC 或 HTTP）到 `common/src/telemetry.rs`
- 新增 `with_tracing` 方法到 `EventBus` trait（可选，向后兼容）

**对现有系统的影响：** 低。traceparent 骨架已存在。主要是连线工作。

### 方向 C：消息撤回的一致性保证（P1）

**为什么需要：** 当前的分析正确指出，消息撤回需要幂等/收敛保证。如果撤回事件因 NATS 重投或下游扇出竞争条件而被投递两次，客户端可能收到重复的 `Deleted` 事件。

**核心挑战：**
- 如何处理“撤回已不存在消息”的情况（幂等性——`soft_delete_audited` 已经支持，但需要验证）
- 如何向客户端保证撤回事件只被应用一次（基于 seq 的幂等性）
- 如何处理撤回期间的消息编辑竞争

**预期架构变更：**
- 为撤回操作扩展 seq 机制：撤回本身应该像一个事件一样拥有自己的 seq，这样客户端可以判断是否已经处理过
- 将撤回与 NATS ack 语义绑定：只有当事务提交后，撤回的 seq 才被认为是 committed
- 新增：`MessageLifecycle` 概念——一个消息经历了 Created → Edited（0 或多次）→ Deleted（软删除）→ Tombstoned（硬清理）的过程

**对现有系统的影响：** 低到中。基础设施（seq、事务、审计）已存在。主要是端到端保证的整理。

### 方向 D：从静态限流到自适应负载控制（P2）

**为什么需要：** 目前的 4 层限流策略（反向代理速率 → per-connection 中间件 → 每用户/每房间限流 → AI 预算）是静态的——固定阈值、固定窗口、固定成本分配。这无法处理：
- **流量突发**：静态限流要么过于宽松（让突发通过），要么过于严格（在正常流量下也触发 429）
- **故障降级**：当一个后端（数据库、AI API）变慢时，静态限流不会收紧——这意味着慢速下游会在需要减速时被请求淹没
- **成本管理**：AI 预算（per-ws 120 over 60s）是静态的——不会根据实际 API 延迟或错误率进行调整

**核心挑战：**
- 需要稳定的代理信号——当数据库变慢时，`sqlx` 连接池已经暴露了等待时间；当 AI API 返回 429 时，客户端已经知道
- 自适应控制必须在每租户隔离和全局资源公平性之间取得平衡
- 实现不当会导致限流器本身成为瓶颈（参见：有状态的令牌桶需要互斥锁）

**预期架构变更：**
- 对现有限流层进行**薄重构**：将 `RateLimitBudget` trait 改为可注入，以便实现自适应变体
- 新增：`AdaptiveBudget`——在滑动窗口内跟踪请求延迟和错误率，并动态调整阈值
- 将 AI 预算与上游响应状态绑定：如果 Anthropic 返回 429，则在全局范围内降低速率（不仅仅是“每个用户排队”）

**对现有系统的影响：** 低。现有限流器已接口化且隔离良好。主要是实现新的 budget 实现。

### 方向 E：NATS 生产化与跨集群灾难恢复（P2）

**为什么需要：** 目前的 NATS 设置假定单节点、无认证、无 TLS。分析中提到的 JetStream stream 声明式但不可变，这在开发中可接受，但在生产环境中，你需要：
- 跨 AZ 的 NATS 集群
- 持久流的数据复制
- 声明式 stream 配置与更改管理
- 细粒度权限（每个客户端只有发布/订阅特定 subject 的权限）

**核心挑战：**
- 保留开发和生产之间相同的 `bootstrap()` 调用——配置应该是声明式的，而不是命令式的
- 与 config.toml/env 变量集成——stream 配置（replicas、max_age、存储类型）应该是可配置的，而无需修改代码
- 与 CI/CD 集成——stream 配置漂移应该是可检测的（类似于 Terraform plan）

**预期架构变更：**
- 将 `JetStreamConfig` 扩展为包含每个 stream 的可选覆盖配置
- 新增：`nats-schema.toml`（或等价的 config 文件）→ 声明式 stream 定义
- 新增：`aero-cli nats apply` 子命令 → 将声明的配置与运行时的 NATS 状态进行协调
- 新增：细粒度权限支持（每个 subject 的 NATS NKey/RBAC）

**对现有系统的影响：** 中低。NATS 抽象层（`EventBus` trait）可以保持稳定。主要是底层实现的扩展。

---

## 3. 接口设计建议

### 3.1 出站 HTTP 网关接口

```rust
#[async_trait]
pub trait Egress: Send + Sync {
    /// Fetch a URL, returning the response body if successful.
    async fn fetch(&self, url: &Url, opts: FetchOptions) -> Result<FetchResult, EgressError>;
}

pub struct FetchOptions {
    pub method: Method,           // GET / POST / HEAD
    pub headers: HeaderMap,       // additional headers to send
    pub body: Option<Bytes>,       // for POST
    pub max_bytes: usize,         // response body cap
    pub timeout: Duration,
    pub allowed_content_types: Option<Vec<Mime>>,  // None = any
    pub redirect_limit: usize,    // 0 = no redirects
}

pub struct FetchResult {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub resolved_ip: Ipv4Addr,    // the IP that was actually connected to
}
```

**关键设计原则：**
- **fail-closed on private IPs**：默认情况下，拒绝任何解析到 RFC 1918、本地链路、云元数据（`169.254.169.254`、`fd00:ec2::254`）或已知不良 CIDR 范围的连接——除非被显式 `allow_private` 标记覆盖
- **DNS 预检**：`fetch()` 在建立 TCP 连接之前，先对 URL 的 host 进行 DNS 解析。这将私密 IP 检查提升到连接建立之前
- **redirect 安全**：默认限制为最多 3 次重定向 + 禁止跨协议重定向（HTTP→file、HTTPS→HTTP） + 每次重定向都重新验证 IP
- **可覆盖性**：`EgressGateway` 允许默认安全策略，但像 Call Bridge（内部端点）这样的组件可以提供覆盖（`allow_private: true` + `internal_cidrs: ["10.0.0.0/8"]`）

### 3.2 可观测性——追踪传播

当前的 `EventBus::publish(subject, payload)` 签名对追踪不可知：

```rust
// Current
async fn publish(&self, subject: &str, payload: Bytes) -> BusResult<()>;

// Proposed — backward-compatible via default impl
async fn publish(&self, subject: &str, payload: Bytes) -> BusResult<()> {
    self.publish_with_context(subject, payload, SpanContext::current()).await
}
async fn publish_with_context(&self, subject: &str, payload: Bytes, ctx: SpanContext) -> BusResult<()>;
```

`SpanContext` 应该与 W3C TraceContext 兼容（一个 format 为 `00-traceid-spanid-01` 的 `traceparent` 头），这样即使在内部分发，追踪也与 OpenTelemetry 生态系统兼容。

### 3.3 避免接口膨胀

**不要为每一件小事都引入 trait。** 例如，`Unfurler` trait 是好的（它隔离了纯逻辑与网络 I/O）。但添加一个 `EventuallyConsistentMessageLifecyle` trait 可能就过度设计了——现有的 `MessageRepo` + `seq` 机制可能已经足够。使用介绍性文档序列时适当精简。

---

## 4. 技术选型

### 4.1 出站网关：自建 vs 引入

**结论：自建薄抽象层。** 在 Rust 中，通过 `reqwest` 实现可靠、安全的 HTTP 出站大约需要 200-300 行精心编写的代码。引入一个新的代理/网关 sidecar（如 Envoy、Linkerd 或 OAuth2 Proxy）是可以的，但会增加运维负担，因为：
- 几乎无法对出站流量进行协议感知检查（检查 HTTP 响应体）
- 增加了延迟（即使是在 localhost 上额外的 HTTP 跳数）
- 需要另一组凭证/配置

建议：在 `aero-common` 或一个新的 `aero-egress` crate 中创建 ~250 行的 `SecureEgressGateway`。保持在每个进程中。

### 4.2 分布式追踪

**使用 `opentelemetry` crate 家族。** 当前项目使用 `tracing` + 一个 shim。引入 `opentelemetry` + `opentelemetry-otlp` 进行原生 OTLP 导出。依赖于：
- `tracing-opentelemetry`：桥接 tracing 到 OpenTelemetry
- `opentelemetry-otlp`：gRPC 导出（或 HTTP）
- `opentelemetry-sdk`：采样、批处理、优雅关闭

这使得项目能够与 Grafana Tempo、Datadog、Honeycomb 或任何兼容 OTLP 的后端兼容。

### 4.3 NATS 生产化

**需要：NATS 集群 + TLS + 认证。** 具体推荐：
- NATS 集群：至少 3 个节点，使用 route 连接
- TLS：使用 `async_nats::ConnectOptions::tls(true)` + 客户端证书
- 认证：使用 `NKey`/`JWT` 进行安全客户端连接，并根据 environment 变量或文件进行配置
- JetStream 副本：在 `bootstrap()` 中设置 `num_replicas: 3` 用于流（在开发中可覆盖为 1）

**不需要：NATS 全面替换。** 对于类似 IM + 直播的事件密度，NATS JetStream 是正确的选择。不需要 Kafka、RabbitMQ 或 Redis Streams 来替代。

### 4.4 不要追逐框架

* 不需要 Sidecar 代理网格（Envoy/Istio）——出站安全可以在进程内处理，运维更简单
* 不需要 GraphQL——REST + WebSocket 已覆盖需求
* 不需要 gRPC（除了 OTLP）——HTTP/REST 已足够，且与 web 兼容
* 不需要 Kubernetes Operator——配置驱动的 Docker Compose / Nomad 作业已足够

---

## 5. 实施路线图

### 阶段 0：立即修复（P0 — 1-2 天）

| 项目 | 工作量 | 风险 | 缓释措施 |
|---|---|---|---|
| **SSRF 防护 — unfurl bot** | 0.5 天 | 低 | 在 `ReqwestUnfurler::fetch()` 中添加 `redirect: redirect::Policy::none()` + IP 黑名单。不需要完整的网关。 |
| **SSRF 防护 — webhook delivery** | 0.5 天 | 低 | 与上面相同的修复。`ReqwestSender::deliver()` |
| **SSRF 防护 — 所有出站** | 1 天 | 低 | 扫描所有剩余的 `reqwest::Client` 使用点，确保它们都有 SecurityPosture。 |
| **NATS `-js` 参数** | 0.1 天 | 低 | 在连接 URL 中添加 `?js=1` |

### 阶段 1：出站 Egress Gateway（P0 — 1 周）

| 里程碑 | 交付物 |
|---|---|
| M1.1 | `aero-egress` crate，包含 `EgressGateway` trait + `SecureEgressGateway` |
| M1.2 | DNS 预检 + IP 黑名单（RFC 1918、169.254.169.254、已知不良范围） |
| M1.3 | 迁移 ReqwestUnfurler、ReqwestSender、AI HTTP 调用 |
| M1.4 | 迁移剩余调用点（OIDC、S3、call-bridge、WHEP 上游） |
| M1.5 | 集成测试 + `#[cfg(test)]` FakeEgressGateway |

**风险：** 迁移 AI 调用点（Anthropic、Voyage、Whisper）影响最大，因为 AI 调用位于性能关键路径。**缓释措施：** 先为 AI 调用使用零成本包装器，然后逐步收紧安全策略。

### 阶段 2：分布式追踪（P1 — 1 周）

| 里程碑 | 交付物 |
|---|---|
| M2.1 | 将 `traceparent` 传播集成到 `JetStreamBus::publish` 中 |
| M2.2 | 在消费者端实现 traceparent 解码（`run_bus_listener`、所有 bot） |
| M2.3 | 集成 OTLP 导出 + 配置 |
| M2.4 | 仪表盘 dashboard + 跨事件管道的延迟告警 |

**风险：** traceparent 传播增加了一小部分每消息开销（~50 字节）。**缓释措施：** 对高频 `live.stream.*` 事件使用 head-based 采样（1% 采样率 vs IM 消息的 100%）。

### 阶段 3：消息撤回收敛性 + 多级限流（P1 — 1 周）

| 里程碑 | 交付物 |
|---|---|
| M3.1 | 撤回 seq 追踪：撤回操作获得唯一的 seq + 持久化 |
| M3.2 | 客户端幂等键（用于撤回事件） |
| M3.3 | rate limiter 自适应层（`AdaptiveBudget`） |
| M3.4 | AI 预算回溯调整（在 Anthropic 429 时降低费率） |

**风险：** 对于已处理数千个事件的生产部署，添加撤回 seq 可能是一次有损迁移。**缓释措施：** 默认为新撤回生成 seq；旧记录使用 seq=0。

### 阶段 4：NATS 生产化（P2 — 2 周）

| 里程碑 | 交付物 |
|---|---|
| M4.1 | 声明式 stream 配置（`nats-schema.toml` + `aero-cli nats apply`） |
| M4.2 | TLS + 认证集成 |
| M4.3 | 流副本 / 基于环境的覆盖 |
| M4.4 | NATS 集群存活监控 + 告警 |

**风险：** 声明式配置与当前命令式 `bootstrap()` 之间的语义差距可能导致配置漂移。**缓释措施：** 对 Nash apply 之前和之后的 stream 配置进行安全比较。

### 路线图时间线

```
Week 1:  [阶段 0]  SSRF 修复 (立即)
         [阶段 1]  出站 Egress Gateway 里程碑 M1.1–M1.3
Week 2:  [阶段 1]  完成 (M1.4–M1.5)
         [阶段 2]  追踪 M2.1–M2.2
Week 3:  [阶段 2]  追踪完成 (M2.3–M2.4)
         [阶段 3]  消息收敛性 M3.1–M3.2
Week 4:  [阶段 3]  限流 M3.3–M3.4
Week 5+: [阶段 4]  NATS 生产化
```

---

## 总结

5 个研究方向准确识别了系统中的真正差距。**P0 级别的 SSRF 漏洞是唯一必须立即修复的架构缺陷**——现有的出站 HTTP 模式缺乏任何集中的安全策略。分布式追踪缺口虽然不那么严重，但在投入生产后会加剧运维盲点。

最大的架构优势——事件驱动的骨架 + NATS 扇出——同时也是最大的挑战：随着系统跨 N 个节点扩展，当前的每个消费者模式（每个 bot 一个 durable consumer）将需要 stream 协调。但这是以后的问题。现在，修复 SSRF，添加追踪，然后稳步推进到生产可靠性。
