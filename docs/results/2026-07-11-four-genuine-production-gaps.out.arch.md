# 架构师分析：Aero IM 缺口评估与架构演进路线

---

## 一、架构评估

### 1.1 当前架构的优势

**领域驱动的 crate 拆分是正确决策。** 从 `aero-common`（叶子类型）到 `aero-bus`/`aero-storage`/`aero-auth`（基础层），再到 IM/直播/媒体功能 crate，最后到 `aero-server` 组合层——依赖方向自下而上清晰，无循环依赖。这在 Rust 生态中是一个难得的、可维护的分层架构。

**事件的统一扇出模型（NATS → Hub → WS）是第二大架构优势。** 将跨实例通信（NATS）与进程内扇出（Hub 的 `mpsc`）解耦，使水平扩展成为声明式配置而非架构改造。durable consumer + ephemeral consumer 的两类语义选择（IM 消息 vs 直播弹幕）也体现了对场景的正确差异化处理。

**第三大优势是幂等设计的一贯性。** `ON CONFLICT DO NOTHING`、`soft_delete_audited` 事务化、SeqGate 去重——这在 at-least-once 投递语义下是生存必需的，且代码中贯彻得相当一致。

**第四点：状态管理在正确的地方。** Redis 存集群级状态（presence、roster、viewer count），PG 存持久数据，进程内存不存共享可变状态（`SfuRouter` 是明确例外且有 `Arc<RwLock>` 保护）。这个策略避免了分布式系统最常见的陷阱之一。

### 1.2 架构债务

以下债务按修复成本从低到高排列：

| 债务 | 影响 | 修复时间 | 紧急度 |
|------|------|---------|--------|
| `forgot_password` token 存储失败返回 `200` 但永不到达 | 用户死胡同 | 0.5 天 | P1 |
| 邮件通道无告警仪表板 | 故障时无感知 | 0.5 天 | P1 |
| WS 全局 seq 在重连回溯时跨房间误判 | 消息丢失 | 2 天 | P0 |
| 前端零测试覆盖 | 回归风险 | 1 周初始 | P0 |
| 无 API 版本化基础设施 | 企业客户流失 | 3 天初始 | P0 |
| `SfuMediaSession.run` 生产未接线 | 功能不可用 | 2 周（需真对端） | P2 |
| 邮件通道静默失败无度量 | 运维盲区 | 1 天 | P1 |
| Webhook payload 无 schema version | 消费者断联 | 1 天 | P2 |
| 密码重置 token 通过 URL query 传递 | 日志泄露 | 0.5 天 | P2 |
| frontend 685 行 `render.js` 纯函数裸写 | 测试难度高 | 2 天重构 | P2 |

**核心诊断**：当前系统的架构债务集中在**两个薄弱的 seams**——前端与后端的接口契约（无版本化、无 schema 合约、无测试）和**外部集成 seam**（邮件、推送、S3 的失败路径无可观测性）。这两个 seams 恰恰是生产系统最容易出问题的边界。

### 1.3 关键设计决策评估

**决策 1：前端采用零框架 SPA vs React/Vue/Svelte**

评估：**在项目早期是合理的权衡**。零依赖意味着零框架升级压力、零构建工具链、零包体积膨胀。对于一个至少 100+ 路由的系统（`routes.rs` 约 3000 行），当前 5,939 行 JS 大约覆盖了 40-50% 的 API 端点。当覆盖率达到 80%+ 时，组件组合复杂度将超过零框架的承受阈值。

**关键指标**：当 `web/` 目录的总 JS 行数超过 12,000 行（约翻倍），或单一文件超过 1,000 行时，应考虑引入轻量级框架（Preact/Svelte）。

**决策 2：NATS 作为唯一跨实例传输层**

评估：**绝对正确**。NATS JetStream 提供了 at-least-once、持久化 cursor、consumer 语义（durable/ephemeral/queue-group），这是一个分布式系统所需的最低基础。如果选择了 RabbitMQ 或 Kafka，会引入不必要的复杂性和运维负担。仅当未来需要消息重放/审计日志保留超过 NATS 的存储能力时，才需要考虑 Kafka 作为补充。

**决策 3：str0m（纯 Rust WebRTC）vs webrtc-rs / pion**

评估：**冒险但合理的技术选型**。str0m 的作者是 WebRTC 规范的核心贡献者，API 语义更接近 RFC。但它的社区和生态远小于 pion（Go）或 webrtc-rs。选择它意味着所有媒体层问题都需要自行 debug。考虑到 `aero-live-webrtc` 和 `aero-live-whip` 是核心差异化能力，这个风险是值得承担的。

**决策 4：fail-open 作为默认错误模式**

评估：**在用户面是正确的，在运维面是错误的。** 所有 auth 相关的错误返回统一消息（防枚举）、邮件失败只 warn 不中断请求、Redis 故障降级到 fail-open——这些都保护了用户体验，但代价是运维盲区。解决方案不是改变 fail-open 策略，而是给每个 fail-open 路径挂载一个**度量**（counter/gauge），让运维知道「今天有多少请求在降级模式下运行」。

---

## 二、扩展方向

### 方向 1：外部集成 Seam 治理层（P0）

**为什么需要：** 当前邮件、推送、S3、webhook 四条外部集成路径全部缺乏重试、死信、可观测性。从初创到生产的关键差距不在于功能，而在于「当外部依赖挂了，系统能否自愈并通知人」。

**核心挑战：**
- 四条路径的错误语义完全不同（SMTP 不可达 vs S3 鉴权过期 vs webhook 4xx vs FCM 速率限制）
- 作业队列需要统一的调度语义（背压、退避、最终投递）
- 幂等键的生成和存储策略需要标准化

**预期的架构变更：**

```
当前：
  Handler → 直接调用 mailer/s3/push (同步 .await, 失败 warn)

目标：
  Handler → enqueue(Job) → 后台 drain loop → 重试+退避+死信
           ↕ metrics/counters → 告警
```

具体引入一个 `OutboxRepo`（PG 表作为作业队列）+ 一个 `OutboxWorker`（带退避+死信+度量），让邮件/webhook/推送统一走 outbox。S3 因 blob 体积大，作为例外保留直接路径但挂载 metrics wrapper。

**对现有系统的影响：**
- 邮件：`build_mailer` 改为 `enqueue_mail` 写入 `outbox` 表（而非直接 `smtp.send`），原生异步 handler 不等待 SMTP 往返
- webhook：已有 DLQ 逻辑，合并到同一 outbox 框架
- 推送：已有重试意愿但未实现，outbox 统一接管
- mailer.rs 主体逻辑保留，只加一层出队调度

**工作量估算：** 2 周（outbox 表 + 仓储 + worker + 度量 + 集成改造）

---

### 方向 2：前端渐进式现代化（P0 → P2）

**为什么需要：** 5,939 行零测试 JS 是项目最大的技术债。但全面换框架的冲击太大。需要一条渐进路径。

**核心挑战：**
- 纯函数（`render.js`）和副作用操作（`ws.js`/`api.js`）耦合在一起
- 无模块系统，全局命名空间
- ES2020 的 `import` 可用但未使用
- 无组件树，当 UI 复杂度增长时事件监听的管理成本线性增长

**渐进路径而不是一次重写：**

| 阶段 | 内容 | 工作量 |
|------|------|--------|
| Phase 1 | `render.js` 纯函数提取 + vitest 测试覆盖 | 2 天 |
| Phase 2 | `api.js` 解耦为独立模块 + 测试 | 2 天 |
| Phase 3 | `ws.js` 连接管理 + SeqGate + 重连回溯测试 | 3 天 |
| Phase 4 | `auth_ui.js` + `stream_chat_ui.js` + `polls.js` 逐个追加测试 | 每个 1 天 |
| Phase 5 | 引入 Svelte（最小侵入，替换最复杂的 DOM 部分） | 2 周 |
| Phase 6 | 全局 E2E（Playwright）覆盖核心流程 | 1 周 |

**关键架构决策：不要预先选框架。** 先构建测试覆盖层，让重构安全。选框架（如果最终需要）时，Svelte 优于 React 的理由：零运行时（文件更小）、更接近原生 DOM 模型（`render.js` 的迁移路径更平滑）、编译时优化。

**对现有系统的影响：**
- 测试文件位于 `web/__tests__/`，不影响现有代码
- Phase 5 可选择性地替换单个视图（如 `stream_chat_ui.js`）而非全量重写
- E2E 测试（Phase 6）跑在 CI 中，需要 Firefox/Chrome headless

---

### 方向 3：合约驱动 API 治理（P0 → P3）

**为什么需要：** 企业客户签 6 个月 SLA 的前提是「API 不会在未通知的情况下变化」。当前无版本化 == 技术上所有端点随时可做 breaking change。

**核心挑战：**
- WS API 的版本协商比 REST 更难（协议级无 `Accept` 头等价物）
- Webhook payload 的 schema 进化需要消费者同步更新
- 三种 API 面（REST/WS/Webhook）需要统一的生命周期管理策略

**推荐的渐进方案：**

| 层 | 短期（2 天） | 中期（1 周） | 长期（1 月） |
|----|------------|------------|------------|
| REST | `VersioningLayer` 中间件读取 `Accept` 头，记录版本、设 `X-Api-Version` | API 兼容性合约测试 | `/api/v1/` 别名路由 |
| WS | 帧内加 `"version": 1` 字段，服务端忽略未知字段 | `seq` 回溯按房间独立 | 客户端声明 `min_version`/`max_version` |
| Webhook | payload 加 `"schema_version": "1.0"` | 消费者 registry + migration guide | schema registry |

**对现有系统的影响：**
- 最短路径（2 天）只需增加一个 axum 中间件，零路由改动
- 合约测试不改变生产逻辑，只增加 CI 门禁
- WS 帧加版本字段需要前端处理 `ServerFrameV2` 的向后兼容

**关于 WS 的版本协商策略（比 REST 复杂）：**

REST 有成熟的 `Accept: application/vnd.aero.v2+json` 模式。WS 需要不同的方案：

```
选项 A（推荐）：帧内版本声明
  客户端发送：{"type": "hello", "version": 2}
  服务端响应：{"type": "hello_ok", "version": 2}
  优点：服务端和客户端显式协商，日志清晰
  缺点：增大了每帧处理复杂度

选项 B：连接路径版本化
  ws://host/ws/v2
  优点：简单，匹配 REST 的 URL 前缀
  缺点：连接生命周期内不能升降级，多版本需要多个连接

选项 C（不推荐）：协议级协商
  WS 的 subprotocol 协商
  缺点：浏览器 API 支持有限，消息内版本控制更灵活
```

**推荐的 WS 策略**：走选项 A，帧内 `version` 字段。原因：`connect` 时不绑定版本，`hello` 帧协商，服务器按客户端声明的版本选择帧编码路径（v1 编码器 vs v2 编码器）。这允许 WS 连接持续复用，也允许多版本客户端共存。

---

### 方向 4：媒体层生产就绪化（P1 → P2）

**为什么需要：** `SfuMediaSession.run` 生产未接线意味着两个节点的 SFU 间不能互通。如果直播是核心产品能力，这需要解决。

**核心挑战：**
- 需要真正的 WebRTC 对端（浏览器/str0m peer）集成测试
- `CallBridge` 的 `transport.rs` 需要一个 UDP 端口分配和管理策略
- 跨节点 RTP 流的密钥协商（DTLS-SRTP 只在客户端腿，bridge 腿是明文）

**架构建议：**

```
当前 seam：
  SfuMediaSession.on_rtp → SfuForwarder.on_rtp → 本地订阅者
    （egress 组件存在但未接线）

目标：
  SfuMediaSession.on_rtp → SfuForwarder.on_rtp → 本地订阅者
                                              → CallEgress → bridge_frame → UDP
  CallIngress → SfuPeer.poll(unbridge_frame) → SfuForwarder.on_rtp → 本地订阅者
```

**关键决策：如何绑定 `egress` 和 `ingress` 到 `SfuMediaSession`？**

```
选项 A（推荐）：在 SfuMediaSession 初始化时传入 CallBridge 通道
  优势：生命周期耦合清晰，session 销毁时自动清理 relay
  劣势：SfuMediaSession 的单元测试需要 mock CallBridge

选项 B（你代码中的当前模式）：通过外部 supervisor 驱动
  优势：分离关注点，SfuMediaSession 纯化
  劣势：状态同步更复杂（supervisor 须监控 session 生命周期）
```

你的代码当前走的是选项 B（`call_bridge_supervisor` 轮询模式），既然已经实现且经过测试，我建议不要推翻它。只需要给 `SfuMediaSession::run` 添加一个 `Option<CallEgress>` 参数（而非通过 supervisor 异步注入），让 session 运行时直接知道是否需要 relay。

---

### 方向 5：多租户隔离强化（P2）

**为什么需要：** 当前租户隔离依赖 `WorkspaceRepo::member_role` + `assert_room_access` 两条防线。合规团队（Info Barriers、Data Residency）会要求更强的隔离。

**核心挑战：**
- 所有 DB 查询默认 scope 到 `workspace_id`（当前大多数已做到，但需审计数百条查询）
- info barriers 需要运行时检查（写入时检查接收者是否在同一 barrier group 内，而非只检查写入权限）
- 数据驻留需要指定特定 Postgres 集群的能力（当前所有工作区共享同一个 `DATABASE_URL`）

**推荐的架构模式：** 引入 `TenantContext` 作为所有 DB 操作的第一个提取参数：

```
当前：
  repo.find_messages(room_id: Uuid) → 查询未显式约束 workspace_id

目标：
  repo.find_messages(tenant: TenantContext, room_id: Uuid) → WHERE room_id = $1 AND workspace_id = $2
```

**注意这不是新概念**——你的代码已经通过 `assert_room_access` 在服务层完成了。但信息屏障需要在写入/读取路径的两端都做检查：

- 写入路径：消息写入后，检查所有接收者是否在接收方 barrier 内
- 读取路径：用户请求房间消息列表时，检查是否跨 barrier

**对现有系统的影响：**
- `ImService` 层的所有方法已经接受 `participant`，基本已经 tenant-scoped
- 需要加的是 `InfoBarrierRepo` + 在 `fan_out_raw` 路径中加入 barrier 过滤
- 跨 barrier 消息应以「红色acted」占位形式出现在消息列表

---

## 三、接口设计建议

### 3.1 关键接口设计原则

**原则 1：所有外部依赖的失败路径必须是可观测的。**

每个外部调用（SMTP、S3、FCM、webhook）需要：

```rust
// 理想接口模式
trait ExternalDependency: Send + Sync {
    /// 执行操作，返回 Result 的同时记录度量
    /// - 成功：inc counter "dependency.success"
    /// - 失败：inc counter "dependency.failure" + 记录 error
    /// - 超时：inc counter "dependency.timeout"
    fn execute(&self, request: Request) -> impl Future<Output = Result<Response, Error>>;
    
    /// 健康检查
    fn is_healthy(&self) -> impl Future<Output = bool>;
}
```

当前状态：mailer 无 `is_healthy`，push 无 `is_healthy`，S3 无 `is_healthy`。这导致 `/health/ready` 探针只能检查 PG/Redis/NATS/S3（blob），但无法检查邮件和推送通道。

**原则 2：API 合约的 schema 必须可以版本化。**

所有序列化边界的 payload（REST response bodies、WS frames、webhook payloads）需要一个 schema 标识：

- REST：`Content-Type: application/vnd.aero.v1+json`
- WS：帧内 `"version": 1`
- Webhook：payload 内 `"schema_version": "1.0"`

**原则 3：幂等键必须是所有写入路径的一等公民。**

当前幂等策略散落在各个模块中（`ooo_bot` 用 `ON CONFLICT DO NOTHING`，`AiWorker` 用 `SKIP LOCKED`，webhook 用自己的幂等表）。建议统一为 `IdempotencyKey` trait：

```rust
pub trait IdempotencyKey: Display + FromStr + Clone {
    fn generate(&self) -> String;
    fn store(&self, pool: &PgPool, ttl: Duration) -> impl Future<Output = Result<()>>;
    fn exists(&self, pool: &PgPool) -> impl Future<Output = Result<bool>>;
}
```

这听起来像过度设计，但当前散乱的幂等策略意味着审计合规时不能统一回答「一个事件最多触发几次副作用」。

### 3.2 是否需要新的抽象层

**需要：Outbox 抽象层**

当前邮件/webhook/推送三个外发通道的作业管理逻辑重复。一个统一的 `Outbox` 抽象层可以消灭重复：

```rust
trait Outboxable: Serialize + DeserializeOwned {
    type Payload: Serialize + DeserializeOwned;
    
    fn channel(&self) -> &'static str;  // "email" | "webhook" | "push"
    fn payload(&self) -> &Self::Payload; 
    fn idempotency_key(&self) -> String;
}
```

**不需要：ORM 抽象层**

`sqlx` + `XRepo` 模式（每个功能模块一个 repo）是正确的选择。不要引入 Diesel 或 SeaORM。当前的模式已经足够好，且避免了 ORM 的 N+1 和类型安全问题。

### 3.3 向后兼容策略

**REST 向后兼容的合同：**

```
v1 → v2 允许的变化：
  ✅ 在 response 中添加新字段（客户端使用 serde_json::Value 或 `#[serde(deny_unknown_fields)]` 会断）
  ✅ 添加新的枚举 variant（反序列化旧 variant 的客户端不受影响）
  ❌ 删除或重命名响应字段
  ❌ 更改请求体结构
  ❌ 更改枚举 JSON 表示（serde rename、tag 类型改变）
```

**WS 向后兼容的合同：**

```
v1 → v2 允许的变化：
  ✅ 在 ServerFrame 中添加新 variant
  ✅ 在现有 variant 中添加可选字段
  ❌ 移除 ServerFrame variant
  ❌ 给现有字段改类型（int→string）
  ❌ 添加新的必需字段
```

**Webhook 向后兼容的合同：**

```
v1.0 → v1.1 允许的变化（补丁版本，无 schema_version 变化）：
  ✅ 在 payload 中添加新字段

v1 → v2 允许的变化（主版本，schema_version 变化）：
  ✅ 删除/重命名字段（消费者按 schema_version 选择解析器）
  ❌ 在同一个 schema_version 内做 breaking change
```

---

## 四、技术选型

### 4.1 是否引入新技术栈

| 技术 | 推荐引入？ | 理由 |
|------|-----------|------|
| `minijinja`（HTML 模板） | **是** | 邮件模板最轻量的方案，无运行时反射，编译时检查语法。替代方案：`askama`（编译时但需要宏注解，侵入性稍高）或手写 `format!`（当前模式，不可维护） |
| `vitest`（前端测试） | **是** | 已经在你的短名单中，这是正确选择。补充理由：与 ESLint 兼容性优于 Jest，原生 ESModule，无需 `babel` 转译 |
| `tower-http` 的 `SetHeader` 和 `RequestId` | **是** | 用于实现 `VersioningLayer` 和 `X-Request-Id` 中间件。当前 `inject_request_id` 是手写中间件，`tower-http` 有现成的 |
| Kafka | **否（当前阶段）** | NATS JetStream 已经满足 at-least-once + durable cursor 需求。Kafka 的优势（长保留周期、重放、审计日志）在当前规模下不需要。当 retention history 超过 NATS 的磁盘预算时才需要评估 |
| OpenTelemetry SDK | **部分（已有 tracing）** | 当前已用 `tracing` + Prometheus。完全 OTLP 标准的收益（跨服务追踪、span 导出）需要多服务部署才有价值。单进程模式可跳过 |
| 前端框架（React/Vue/Svelte） | **不是现在** | 等 `render.js` 超过 1,000 行或文件数超过 10 个时再选型。Svelte 是首选候选 |

### 4.2 第三方依赖评估标准

对于 Aero IM 的背景，建议使用以下标准评估新依赖：

```
1. 维护状态：GitHub 最近 commit 是否在 6 个月内？Issue 响应是否活跃？
2. Rust 版本兼容：是否支持 MSRV 1.80？
3. 审计记录：自上次安全审计以来是否有 CVE？
4. API 稳定性：是否 > 1.0？Breaking change 频率？
5. 依赖树影响：是否会引入超过 50 个传递依赖？
6. 许可证：MIT/Apache 2.0/BSD？避免 AGPL/SSPL
7. 社区规模：GitHub stars > 500？Discord/Matrix 频道是否活跃？
8. 测试覆盖率：repo 自身是否有 CI 测试？[build] 是否检查？
```

以 `minijinja` 为例的评估：

| 标准 | 结果 |
|------|------|
| 维护状态 | ✅ 活跃，mitsuhiko 维护 |
| Rust 版本兼容 | ✅ MSRV 1.63+ |
| CVE | ✅ 无已知 |
| API 稳定性 | ✅ 1.x |
| 依赖树 | ✅ 零外部依赖（纯 Rust） |
| 许可证 | ✅ Apache 2.0 |
| 社区规模 | ⭐ 1.2k stars |
| 测试覆盖率 | ✅ ~90% |

### 4.3 自建 vs 采购决策

当前场景（邮件基础设施）：

| 方案 | 成本 | 控制力 | 复杂度 | 推荐 |
|------|------|--------|--------|------|
| 自建 outbox + SMTP | 仅开发工时 | 完全控制 | 中等 | ✅ **推荐**（当前路径的自然演化） |
| Amazon SES + lambda | 按用量付费（低） | 需管理 AWS 账号 | 低 | 备选（但如果团队已经用 Postgres + NATS，自建更适合现有运维模型） |
| SendGrid/Mailgun/Resend API | $15-30/月起 | API 集成 | 低 | 当邮件量 > 10,000/月时评估。当前量级自建 SMTP 足够 |
| Twilio SendGrid + webhook 回调 | $20-50/月 | API + 事件通知 | 中 | 未来需要送达率分析和 bounce 处理时升级 |

**推荐路径**：先自建（使用已有 Mailer + outbox worker），当邮件量达到每月数万级、送达率成为问题时再评估 Email API 提供商。自建到第三方 API 的迁移成本很低——只需要替换 `Mailer::send` 的实现。

---

## 五、实施路线图

### 5.1 优先级排序

```
P0（0-30 天，生产质量必须项）
├── 前端测试覆盖（render.js → api.js → ws.js）
├── API 合约治理（VersioningLayer + 合约测试）
├── WS 重连回溯按房间独立 seq（Bug 修复）
└── 邮件通道可观测性（counter + alert）

P1（30-60 天，基础设施就绪）
├── Outbox 作业队列（统一邮件/webhook/推送）
├── 邮件通知偏好 + 退订机制
├── SMTP 配置缺失告警
├── 密码重置 token 存储失败度量
└── IDOR 审计（所有 mutating 路由显式检查）

P2（60-90 天，功能增强）
├── HTML 邮件模板（minijinja）
├── 前端渐进式现代化（Svelte 评估）
├── 媒体层生产接线（SfuMediaSession → CallBridge）
├── Webhook payload schema version
└── 密码重置 token 通过 POST body 传递（URL hash 风险）

P3（90+ 天，合规 + 规模）
├── 租户信息屏障
├── 批量邮件/摘要
├── Kafka 评估（如果 NATS 持久化不足）
├── 多数据中心数据驻留
└── REST v2 路由别名
```

### 5.2 阶段划分和里程碑

**Phase 1（Day 1-14）：止血** — 解决最关键的生产风险

- **Week 1**：修复 WS 重连回溯的跨房间 seq 问题（P0 Bug） + `render.js` 纯函数测试（验证测试工具链）
- **Week 2**：`VersioningLayer` 中间件 + API 合约测试 + 邮件通道 counter/alert
- **里程碑 1**：CI 包含前端测试 + API 兼容性检查。邮件失败有告警路径。

**Phase 2（Day 15-35）：基础设施** — 构建作业队列和通知体系

- **Week 3-4**：Outbox 表 + worker + 邮件/webhook 切换
- **Week 5**：通知偏好（`notif_prefs` email channel） + 退订机制
- **里程碑 2**：邮件发送走 outbox，失败有重试+死信+告警。所有通知场景可偏好门控。

**Phase 3（Day 36-60）：质量提升** — 前端覆盖 + 媒体接线

- **Week 6-7**：`ws.js` + `api.js` + `calls.js` 测试覆盖
- **Week 8**：`SfuMediaSession` 生产接线（需与 WebRTC 客户端联调）
- **里程碑 3**：全部 WS 核心逻辑有测试覆盖。SFU 媒体可在双节点间转发。

**Phase 4（Day 61-90）：成熟** — 合规和规模

- **Week 9-10**：HTML 邮件模板 + 密码 token 安全性改进
- **Week 11-12**：信息屏障 + API v2 评估
- **里程碑 4**：满足 SOC2 邮件相关合规要求。API 版本化框架就绪。

### 5.3 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| Outbox 导致邮件重复（at-least-one 重复投递） | 中 | 中 | 幂等键 + `ON CONFLICT DO NOTHING`。邮件侧幂等比消息侧更难（SMTP 无原生幂等语义），但可以在邮件 body 中加入 `X-Idempotency-Key` 头供下游去重 |
| 前端测试覆盖 685 行纯函数后收益递减 | 高 | 高 | 按 ROI 顺序覆盖。`render.js` 纯函数完成后，下一个最有价值的是 `ws.js` 的 SeqGate + 重连逻辑——这里一次 bug 修复可以挽回千行测试成本 |
| SfuMediaSession 接线需要真实对端，CI 无法验证 | 中 | 高 | 创建 `#[cfg(test)]` 的模拟 str0m peer（用 str0m 的 `UdpSocket` + `IceAgent` + `Dtls` 自连接）。这需要 2-3 天，但使后续所有媒体层改动可以自动化回归 |
| 通知偏好+退订合规要求未满足前开始发邮件 | 中 | 中 | 在 Phase 2 开始前，邮件通道先不发任何非密码重置类邮件。退订机制必须在任何批量邮件前完成 |
| API VersioningLayer 误读了生产流量 | 低 | 低 | Phase 1 的 VersioningLayer 是观察模式，不影响路由。`Accept` 头无匹配时回退到当前行为。这从根本上不可产生 breakage |

### 5.4 关于「不要做的」申明

以下方向的建议花费精力但当前不应投入：

1. **不要重写前端**。当前 5,939 行 JS 的功能价值远高于代码质量价值。优先加测试，框架替换是 P2 末期的选项。

2. **不要引入 Kafka**。除非 NATS JetStream 的存储限制（默认文件大小上限 8GB）成为 retention 瓶颈，否则 Kafka 的运维复杂度不值。

3. **不要做 E2E 加密（MLS）的状态机实现**。`common/src/mls.rs` 的 scaffold 是客户端安全边界，服务端实现状态机只会增加攻击面而不增加安全值。

4. **不要为 API 版本化引入 gRPC**。当前 API 的风格是 REST+WS+Webhook，gRPC 的版本化虽然自然（package 版本），但引入协议栈的迁移成本远高于 URL 前缀 + `Accept` 头。

5. **不要做 SCIM 出站同步**。当前入站供给满足 90% 企业需求。出站同步的复杂度和收益不匹配。

---

## 总结

| 维度 | 状态 | 最需关注的债务 |
|------|------|---------------|
| 后端架构 | ✅ 优秀 | 媒体 seam 未接线（功能缺口而非设计缺口） |
| 前端代码质量 | ⚠️ 脆弱 | 零测试，全局命名空间，框架缺失 |
| API 治理 | ❌ 缺失 | 零版本化，零合约测试 |
| 外部集成韧性 | ⚠️ 脆弱 | 无重试、无死信、无可观测性 |
| 不可变性和幂等性 | ✅ 优秀 | 但散落各处，无统一抽象 |
| 安全性 | ✅ 良好 | 密码重置 token 日志泄露（次要） |
| 合规性 | ⚠️ 发展中 | 邮件退订缺失，无信息屏障 |
| 多租户隔离 | ✅ 良好 | 但跨 barrier 检查未实现 |

**一句话总结**：Aero IM 的架构设计和实现质量远高于同类项目的中位数，核心的领域驱动拆分和事件驱动模型是正确的。主要的架构债务集中在三个「seam」上——**不可观测的外部依赖 seam**、**无契约的 API seam**、**零测试的前端 seam**。这些都在可控范围内，1-2 月的系统化投入可以将其从「风险」降至「成熟」。
