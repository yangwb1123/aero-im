以下是在 **Aero IM 架构的完整上下文**中，对「5 个运营 Edge Case」分析文档的架构师评审。

---

# 架构师分析：「5 个运营 Edge Case」评审

## 1. 架构评估

### 1.1 当前架构的优势

系统在**隔离性**和**事件驱动骨架**上做得很好。文档中揭示的 5 个方向恰恰印证了架构的几个隐性优势：

- **CancellationToken 框架已经铺好**（方向一）：WS 连接级已有 token 就绪，缺的只是从全局 shutdown cascade 注入——说明当初设计时为 graceful draining 预留了 seam，只是优先级没排到。这是「可扩展的抽象」的典范。
- **Webhook 与 moderation_bot 的对比**（方向二）：moderation_bot 的有界 mpsc + N worker 模式侧面说明团队**知道如何进行背压设计**——webhook 管线的串行模型是**有意为之还是渐进疏漏**值得追问。如果是渐进增量，说明团队先交付功能再补运维脊梁的策略是务实的。
- **Blob 无缓存**（方向三）暴露的是另一个问题：当前 blob 存储层做得简单但正确（鉴权闭合、X-Content-Type-Options 安全头、inline_safe 判定），是在**「正确」和「高性能」之间选了前者**。对于协作 IM 的早期阶段，这是合理取舍。
- **PG 健康监控**（方向五）的 INDEX_SIZE_BYTES gauge sampler 说明系统已经有一个**扩展机制**（observability_gauge_samplers + 按间隔的采样循环 + 查询 Err 留旧值的弱保证），只是采样目标集不全。这是架构弹性好的信号——加新采样器的工作量是 copy-paste 级的。

### 1.2 局限性

| 领域 | 局限 | 根因 |
|------|------|------|
| **连接生命周期管理** | WS 排干未与进程生命周期绑定 | `CancellationToken` 拓扑是扁平树（每连接独立叶子），缺从根到叶的传播链路 |
| **投递管道并发模型** | Webhook 是单事件→单线程→串行目标，缺乏并发原语 | 可能最初的 assumption 是 webhook 数量少+快速（like Slack 的 outgoing webhook），但 IM 生态中 webhook 的 endpoint 数量和响应方差都很大 |
| **HTTP 缓存语义** | Blob 下载路径完全跳过 HTTP 缓存层 | 架构上 blob 被当作「内网私有资源」而非「CDN 就绪的公共资源」设计——每次下载都鉴权无可厚非，但缓存协商和 CDN 前置并不与鉴权矛盾 |
| **客户端可靠性模型** | WS 消息发送是 fire-and-forget 且调用方不检查返回值 | 前端架构设计时乐观更新优先级高于投递确认。这是**产品决策**（即时反馈 > 消息确达）留下的技术债——不是架构错，是产品优先级取舍的无意后果 |
| **可观测性深度** | 基础设施指标（连接/池/积压）全，应用层内部指标（死元组/长事务/seq scan）缺 | 监控架构按「外部依赖的健康等价于系统健康」来设计——这在稳定态成立，但退化态下 PG 内部状态恶化在前，外部指标恶化在后 |

### 1.3 关键设计决策合理性评估

| 决策 | 合理性 | 评估 |
|------|--------|------|
| WS 每连接独立 CancellationToken | 正确但应派生 | 独立 token 在慢消费者驱逐场景下合理；但从全局 shutdown cascade 派生（像一棵树）就可同时满足两种场景 |
| Webhook 单事件串行投递 | ❌ 不合理 | 对比同一代码库的 moderation_bot 使用了有界 mpsc + worker pool + Semaphore，webhook 管线缺少了同等水平的背压保护 |
| Blob 鉴权 + 全量传输 + 无缓存 | 合理但有代价 | 对于早期 To-B 协作 IM（文件数少、用户少），这是合理取舍；但扩展到媒体密集场景（直播截图/视频消息/语音消息）时带宽浪费放大 |
| 客户端乐观更新 + 不检查 WS 返回值 | 产品决策遗留的技术债 | 优先级：前端渲染速度 > 消息必达。这对社交产品合理，对 To-B 协作不合理——Slack/Teams 都有 pending 指示器 |
| DB 只采样池状态不采样内部指标 | 合理的阶段性选择 | 内部指标（死元组/seq scan）是对 DBA 有价值但非生存关键的指标——缺了不会立即宕机，但会无声退化 |

### 1.4 架构债务

1. **EventBus trait 签名缺少 headers 参数**：ROADMAP 方向二已指出——`publish` 签名无 `headers`，traceparent 只能塞进 payload。这不是 5 个方向之一但仍是跨进程追踪脊柱的瓶颈。**债务等级：中等**——改 trait 签名需审计所有调用方。

2. **webhook drawbridge 与 delivery 管线的耦合**：webhook 的 breaker 状态是 per-target 的（`circuit_breaker.rs`），但投递调度策略只有简单的「breaker open→skip」二元决策，没有「breaker half-open→probe→backoff」的状态机。**债务等级：低**——breaker 当前工作，但慢端点场景下过于粗糙。

3. **客户端所有消息共享同一 WS 连接但无消息优先级**：弹幕（时效性高但可丢）和通话信令（必须可靠）共用同一 WS mpsc 发送队列，无优先级标记。当前由于 WS 连接畅通时这不成问题，但在弱网/拥塞下可能导致通话信令被弹幕排队延迟。**债务等级：低**——但需要前瞻注意。

---

## 2. 扩展方向

基于文档的 5 个方向的完整分析，我提出以下扩展方向建议（与文档方向有重叠但侧重架构层面）：

### 方向 A：连接生命周期管理框架（P1·高价值）

**为什么需要**：方向一（WS 排干）和方向四（客户端重试）本质上共享同一个根——**WebSocket 连接生命周期只有「创建」和「暴力终结」，缺少「排干→关闭→重连」的状态契约**。系统把连接当作「字节管道」而非「有状态会话」来管理。

**核心挑战**：
- 进程级 shutdown 需要在所有 WS 连接上并发触发排干，但每个连接排干时长不同（取决于 mpsc 队列积压）
- 1001 frame 发送后不能立即 shutdown——须等客户端 ACK 或超时
- 与现有 `Hub::fan_out_raw` 的 bounded mpsc 的交互：排干期间新事件还扇不扇入？

**预期架构变更**：
- `Hub` 增加 `shutdown_signal: CancellationToken` 参数，收到 cancel 后 fan_out_raw 返回 `Err(ShuttingDown)` → 上游 bus listener 停止新事件处理
- 每个 WS 连接的 `close` token 从 `Hub.shutdown_signal` 派生（`child_token = shutdown_signal.child_token()`）
- 客户端 `WsClient` 新增 `onClose(code)` 处理：`code===1001` → 静默重连（不弹 toast，不等指数退避）；`code===1006` → 当前行为（弹 toast + 退避）

**对现有系统的影响**：
- `Hub` 构造函数接口改变
- `run_socket` 的 `select!` 分支新增一个 `hub.shutdown_signal.cancelled()` 臂
- 客户端侧：已有 `reconnect()` 逻辑，只需 in `onclose` handler 中识别 1001 vs 1006
- **影响范围可控**，不涉及数据模型变动

### 方向 B：投递管线分层（P1·中价值）

**为什么需要**：方向二（Webhook 并发）和方向五（DB 监控）的交集处存在一个深层问题——**当前所有投递（Webhook、Push、Bot）复用相同的 bus consumer 事件流但各自独立消费**。没有统一的投递管线抽象，导致每个投递路径都有自己的 concurrency/failure/retry 策略实现。

**核心挑战**：
- 不同投递目标的要求不同：webhook 需要 breaker + 指数退避、push 需要 dead-token 回收、bot 需要 AI 调用的预算门控
- 共性：都需要 at-least-once 语义 + 幂等键 + backoff + 死信
- 如何定义「投递接口」使得各路径可插拔

**预期架构变更**：
- 引入 `DeliveryPipeline<Target, Payload>` 抽象：
  - `component: &DeliveryComponent` — 处理实际发送
  - `concurrency: usize` — 最大并行数（通过 Semaphore 控制）
  - `retry_policy: RetryPolicy` — 指数退避/最大尝试次数
  - `circuit_breaker: Option<CircuitBreaker>` — 可选熔断器
  - `dead_letter: DeadLetterStrategy` — 死信或重试耗尽后的行为
- 现有 webhook dispatcher 重构为 `WebhookComponent` + `DeliveryPipeline` 的实例化
- Push bot、event-bus bot 也可选择接入（**可选**，非强制迁移）

**对现有系统的影响**：
- 模块级重构（`webhooks.rs` 重写内部逻辑），对外接口不变
- 新增抽象层带来测试成本；但隔离了投递策略使其可单元测试
- 对既有功能零影响（保持行为一致，只改结构）
- **推荐但不强制**：如果团队资源紧张，webhook 优先做最简单改进（Semaphore + spawn per-target，不引入抽象层））

### 方向 C：HTTP 资源服务化（P2·中价值）

**为什么需要**：方向三（Blob 缓存）指向了一个更大的问题——**文件/媒体资源的 HTTP 服务路径缺乏「资源服务」思维**。当前 blob_download 把每次请求当作独立事务（鉴权+查寻+读取+发送），没有缓存层、没有 CDN 对接、没有流式处理。

**核心挑战**：
- 鉴权不能跳过：私密附件即使 CDN 前置，CDN 也要验证 token
- 流式读取与 `BlobStore` trait 的当前 API 冲突：`BlobStore::get` 返回 `Vec<u8>`，不支持 `AsyncRead`
- 内容协商（WebP/AVIF 转码）需要图片处理管线，是基础设施级别的投入
- Range 请求需要支持 `Accept-Ranges: bytes` + 对 `BlobStore` 做部分读取

**预期架构变更**：
- `BlobStore` trait 扩展：增加 `get_stream(id, range: Range<u64>) -> impl AsyncRead` 方法，`get` 作为全量读取的 fallback（向后兼容）
- blob_download 路由增加：
  - `ETag`：基于 blob.sha256（已存在 meta 中）
  - `Cache-Control: private, max-age=31536000, immutable`（对 `inline_safe` 的媒体资源）
  - `If-None-Match` → 304
  - `Range` + `Accept-Ranges: bytes`
  - `Vary: Accept`（为内容协商做准备）
- 新增 `/api/blobs/:id/transformed` 路径用于可选的内容协商
- 可选鉴权 token 附加在 URL（如 `?token=...`），使 CDN 可通过 URL 签名鉴权

**对现有系统的影响**：
- `BlobStore` trait 变更是**向后兼容的**（增加默认方法 `get_stream` 回退到 `get` 包装）
- `local_fs.rs` / `s3.rs` 实现需更新以支持范围读取
- 路由层的改造是纯加法（新增响应头、条件判断），不改变现有路径
- **难点**：S3 的后端天然支持 `Range`，但 local_fs 需要实现 partial read

### 方向 D：客户端离线优先架构（P2·中价值）

**为什么需要**：方向四（发送重试）是离线优先的子集。完整的离线优先需要：
1. **消息在 local storage 持久化**（当前所有消息都是内存态——`app.js` 的 `state` 对象）
2. **离线时写入 local storage，在线时 sync**（Service Worker + Background Sync API）
3. **离线发送队列的可见性**（UI 上的「pending」状态指示器）

**核心挑战**：
- Web SPA 当前是纯内存状态——`state.roomMessages`、`state.pendingByTempId` 都是 JS 对象。迁移到 indexedDB/localStorage 需要重写消息存储层
- 离线队列的冲突解决：如果用户在离线期间对同一条 pending 消息编辑了两次，但只有一次真正发出去了……
- Service Worker 是独立于 SPA 的生命周期——它的 cache 策略和 SPA 的状态同步是不同的问题
- Background Sync 在 iOS Safari 上**不原生支持**——苹果限制意味着 iOS 用户需要退回到 indexedDB + 连接恢复时 drain

**预期架构变更**：
- `web/storage.js`（新增）：IndexedDB wrapper，提供 `savePending(msg)`、`getPending(roomId)`、`deletePending(tempId)`、`getMessage(roomId, msgId)`
- `web/app.js` 中 `optimisticAdd` 改造：添加消息后写入 indexedDB；发送队列从内存移入 storage
- `web/ws.js` 中 `WsClient` 新增 `pendingQueue: Map<roomId, PendingMessage[]>`，`on('open')` 触发 drain
- `web/sw.js`（新增）：Service Worker 监听 `fetch` 事件（当前 unregistered），可选的 Background Sync 注册

**对现有系统的影响**：
- 前端改动较大，但**分阶段可实施**：第一阶段只做内存重试队列（不持久化）；第二阶段加 indexedDB；第三阶段加 Service Worker
- 后端零改动——所有变更在前端
- 风险：iOS Safari 的 indexedDB 行为（私有模式下可能不可用）、Service Worker 更新策略

### 方向 E：PG 内部健康监控体系（P1·高价值，文档方向五的完整实现）

**为什么需要**：文档已充分论证。补充一个架构理由——**当前系统有「外部依赖健康」的监控，缺少「数据面健康」的监控**。PG 的内部指标是数据面的「体检报告」：死元组比率 = 动脉硬化程度，seq_scan 比率 = 血流不畅程度，长事务 = 血栓。

**核心挑战**：
- 采样查询的效率：`pg_stat_user_tables` 等系统表的查询是轻量的（microsecond 级），但高并发下频繁采样仍需注意
- 告警阈值的确定：死元组 30% 做 warning 合理吗？对写入密集的表（messages）和高 read 的表（users）阈值应不同
- 历史趋势 vs 绝对值：单次采样值意义有限，需要时序累积才能判断退化趋势

**预期架构变更**：
- 新增 `metrics_tasks.rs` 中的采样器（遵循既有模式：`tokio::spawn` + `Interval` + 查询 Err 留旧值）
- `metrics.rs` 增加 gauge 定义（前缀统一 `aero_pg_*`）
- `monitoring/prometheus/alert_rules.yml` 增加告警规则
- **注意**：`pg_stat_activity` 查询需要 `pg_stat_activity` 的 SELECT 权限，检查当前 DB 用户是否已授权

**对现有系统的影响**：
- 纯加法，对现有功能零影响
- `monitoring/` 目录下的告警规则文件是已有约定，只需追加
- 最小工作量（估计 2-3 天全搞定），最大收益

---

## 3. 接口设计建议

### 3.1 关键模块接口设计原则

基于对现有代码库的系统性扫描，以下为各方向应遵循的接口设计原则：

| 方向 | 设计原则 | 原因 |
|------|---------|------|
| WS 排干 | **CPS（Continuation-Passing Style）** — 排干命令沿 child_token 树级联 | 现有 `CancellationToken` 树已支持 `child_token()` |
| Webhook 并发 | **Semaphore-gated fan-out** — 资源约束优先于并发便利 | 避免慢端点耗尽连接池 |
| Blob 缓存 | **Layered middleware** — 缓存作为路由层中间件而非 blobl 处理函数的内部逻辑 | 保持 blob_download 函数的职责单一 |
| 客户端重试 | **Queue + drain lifecycle** — 发送队列是独立的状态机，不耦合 UI 渲染 | 与 `optimisticAdd` 解耦 |
| PG 监控 | **Prometheus gauge + sampler pattern** — 复用既有代码模式 | 最小化新代码、最大化一致性 |

### 3.2 是否需要新的抽象层

**需要引入**：`DeliveryPipeline`（方向 B 提案）——把 webhook、push、bot 等投递路径共同的核心逻辑（并行投递、重试、breaker、死信）抽象出来。

**不需要引入**：其他方向——WS 排干复用既有 CancellationToken 框架、Blob 缓存是路由层的增补而非新抽象、客户端重试是 frontend-pattern（消息队列）、PG 监控是已有 sampler 模式的 copy-paste。

### 3.3 向后兼容性

所有方向都满足**向后兼容**的条件：

| 方向 | 兼容方案 |
|------|---------|
| WS 排干 | 排干超时后「fall through」到当前行为（暴力关闭）——慢排干不会阻塞进程退出 |
| Webhook 并发 | 默认 `concurrency=1`（行为等价于串行）——旧用户零感知 |
| Blob 缓存 | 新增头不改变响应体的结构——旧客户端忽略不可识别的头 |
| 客户端重试 | 重试队列后台 drain，不影响 `sendMessage` 的现有逻辑 |
| PG 监控 | 新 gauge 不影响既有指标——新告警规则无人认领前只是静默数据 |

---

## 4. 技术选型

### 4.1 新引入技术栈评估

| 技术/框架 | 适用方向 | 推荐度 | 理由 |
|-----------|---------|--------|------|
| `axum_extra::headers::Range` | 方向三 Blob Range | ✅ 推荐 | 已依赖 axum_extra，直接用不新增依赖 |
| `bytes::Buf` + tokio `AsyncRead` | 方向三 流式 Blob | ✅ 推荐 | 已在依赖树中 |
| `indexedDB` via `idb-keyval` | 方向四 客户端 persist | ✅ 推荐 | 无 dependency（ES2020 内置），`idb-keyval` 是 1KB wrapper |
| `pg_stat_*` queries | 方向五 PG 监控 | ✅ 推荐 | 免新依赖，纯 SQL |
| `image` / `webp` crates | 方向三 图片转码 POSTPONE | ❌ 暂缓 | 增加编译时间、安全攻击面、运维复杂度——方向三的优先级是 P2，先做缓存优化（低成本的 ETag/Cache-Control/Range）再考虑转码 |
| Service Worker + Background Sync | 方向四 离线消息 | ⚠️ 谨慎 | iOS 不支持 Background Sync；可以先用 indexedDB + 连接恢复时 drain 替代 |

### 4.2 自建 vs 采购

| 场景 | 建议 | 理由 |
|------|------|------|
| Blob 图片转码（方向三） | **自建但延后** | 上游云 API（Cloudinary/Imgix）是成熟的替代，但对于 P2 优先级不需要紧急决策 |
| CDN 前置（方向三） | **采购** | CloudFront/Cloudflare 的 CDN 是成熟的，自建 CDN 边缘节点不经济——但 CDN 对接的前提是 ETag/Cache-Control 就绪 |
| 客户端离线存储（方向四） | **自建** | indexedDB 层薄（<20 lines wrapper），不需要外部库 |
| APM/可观测性（方向五旁系） | **采购** | 当前 Prometheus + Grafana 已就绪，如需 APM（Datadog/Sentry）不自建 |

### 4.3 第三方依赖评估标准

对 Rust 生态依赖的审核标准（基于 AGENTS.md 中对安全性的高要求）：

1. **编译时间影响** — 是否增量增加显著（如 `image` crate 会加几十秒编译）
2. **unsafe 代码** — `unsafe_code = "forbid"` 是工作区 lint，引入有 unsafe 的依赖需 `#[allow(...)]`
3. **安全攻击面** — 是否需要解析用户输入（如 image 库需要 decode 用户上传的图片）
4. **维护活跃度** — last commit < 6mo？Security advisories？
5. **替代方案** — 是否可以用标准库 / 已有依赖实现

---

## 5. 实施路线图

### 5.1 优先级排序

基于**影响面 × 实施成本 × 依赖关系**三个维度：

```
影响面：      大 ▲
               │
        DB监控 │ WS排干
          P1   │   P1
               │
               │    Webhook并发
        Blob   │      P1
        缓存   │
         P2    │
               │
               │     客户端重试
          ─────┼───────▶ 影响面：小
               │        P2
               │
               │
               │
               ▼
             成本高
```

| 优先级 | 方向 | 排序理由 |
|--------|------|---------|
| **P1** | **WS 优雅排干** + **Client 重试队列** | 两方向互补——排干减少断连，重试弥补断连期间的消息丢失。且 WS 排干改动量极小（~50 行 Rust），client 重试是纯前端。组合交付一个完整的「连接可靠性」块。 |
| **P1** | **Webhook 并发边界** | 改动中等，但影响面大——单慢 endpoint 影响全系统的风险是真实存在的。且 moderation_bot 的既有模式可以完全借鉴，风险很低。 |
| **P1** | **PG 健康监控** | 成本最低（2-3 天）、收益最高（防线前置）。且完全独立，可随时插队。 |
| **P2** | **Blob 缓存优化** | 带宽节省和 CDN 就绪是高价值但非紧急。可分两阶段：先做 ETag/Cache-Control/Range（低成本快速），然后考虑内容协商和流式读取（高成本）。 |
| **P2** | **投递管线抽象（DeliveryPipeline）** | 重构性质的工作，在 Webhook 并发边界实现后再抽象更合理。否则为抽象而抽象。 |

### 5.2 阶段划分和里程碑

```
Phase 1（2-3 周）："连接可靠 + DB 防线"
├── WS 优雅排干（Rust: ~2d）
│   ├── Hub 增加 shutdown_signal 参数
│   ├── run_socket 从 Hub 派生 child_token
│   ├── 1001 frame 发送 + drain 超时
│   └── metrics: ws_shutdown_draining gauge
├── Client 发送重试队列（JS: ~4d）
│   ├── ws.send() 返回值检查 + 内存重试队列
│   ├── reconnect 事件触发 drain
│   ├── 超时 zombie pending 清理 + toast
│   └── 可选：indexedDB 持久化（P2 级别）
├── PG 健康监控（Rust: ~2d）
│   ├── pg_stat_user_tables 采样（死元组/seq_scan/n_tup_mod）
│   ├── pg_stat_activity 采样（idle-in-transaction）
│   ├── pg_stat_all_indexes 采样（idx_scan 检测未用索引）
│   ├── 配套告警规则
│   └── DB 用户权限校验文档
└── ✅ 里程碑：滚动部署 WS 平滑 → 消息不丢失 → DB 衰退有预警

Phase 2（2 周）："投递管线加固"
├── Webhook 并发边界（Rust: ~5d）
│   ├── dispatch_event 内 tokio::spawn per-target
│   ├── tokio::sync::Semaphore 全局并发上限
│   ├── per-target 自适应速率限制（token bucket）
│   ├── 慢端点隔离（缩短超时 + breaker 增强）
│   └── metrics: webhook_concurrency/queue_depth/latency_histogram
├── 可选：Webhook 投递管线重构为 DeliveryPipeline（+3d，取决于资源）
└── ✅ 里程碑：单慢 endpoint 不阻塞全局投递

Phase 3（2-3 周）："资源传输优化"
├── Blob 缓存优化——第一阶段（Rust: ~3d）
│   ├── ETag（基于 blob.sha256）+ Cache-Control 头
│   ├── If-None-Match → 304 处理
│   ├── Range + Accept-Ranges: bytes 支持
│   └── blob_store 扩展：get_stream 接口
├── Blob 缓存优化——第二阶段（如果资源允许，Rust: ~5d）
│   ├── 内容协商：Accept → WebP/AVIF 转码（POSTPONE 可延期）
│   ├── URL 签名鉴权 token for CDN
│   └── CDN 对接文档
├── 客户端重试队列 indexedDB 持久化（JS: ~2d）
│   └── tab 关闭后恢复 pending 消息
└── ✅ 里程碑：附件带宽降 50-90%，CDN 就绪，离线消息恢复
```

### 5.3 风险点和缓解策略

| 风险 | 影响方向 | 概率 | 缓解 |
|------|---------|------|------|
| WS 排干超时导致进程退出延迟 | 方向一 | 低 | 默认 drain timeout 5s，超时后 fall through 到暴力关闭——即使排干不完整也保证进程退出 |
| Webhook per-target spawn 导致连接数激增 | 方向二 | 中 | `Semaphore` 限制全局并发数（默认 16）；per-endpoint 的 token bucket 限制单端点的发送频率 |
| `BlobStore` trait 变更多个实现受影响 | 方向三 | 低 | 给 `get_stream` 提供默认实现（回退到 `get` 全量读取然后分段），现有实现无需改动 |
| iOS Safari indexedDB 支持度 | 方向四 | 中 | 第一阶段不做 indexedDB（只做内存重试队列）；iOS 的 Service Worker 限制通过文档告知 |
| `pg_stat_activity` 查询权限不足 | 方向五 | 低 | sampler 启动时先做权限探测查询，失败则跳过该 gauge（行为同既有的 Err 留旧值模式）；在运维文档中说明所需权限 |
| WS 排干期间 fan-out 的新事件处理 | 方向一 | 低 | `Hub::fan_out_raw` 收到 cancel signal 后返回 `Err(HubShuttingDown)`，上游 bus listener 收到错误后停止消费新事件并 nack 正在处理的事件（使它们重投到健康的实例） |
| 多个团队并行工作时的 merge 冲突 | 全部 | 中 | 每个方向的工作界限清晰——方向一改 `Hub` + `ws_impl`；方向二改 `webhooks.rs`；方向三改 `routes.rs` + `BlobStore`；方向四仅前端；方向五仅 `metrics_tasks.rs` + `metrics.rs`。使用 git worktree 隔离，集成时先 `cargo check --workspace` |

---

## 总结

本分析文档做得扎实——5 个方向全部有代码级证据，且「方向一~四」已逐行验证。这是好分析的关键质量属性。

从架构层面看，这 5 个方向可以被归纳为**两个根本性的架构缺失**：

1. **连接可靠性缺失**（方向一 + 方向四）：系统对 WebSocket 连接的生命周期管理只考虑了「创建」和「中断」，缺少「排干→关闭→重连→补发」的闭环。方向一是服务端侧修复，方向四是客户端侧修复，两者互补。

2. **资源面隔离性缺失**（方向二 + 方向三 + 方向五）：系统对慢 consumer（方向二的慢 webhook endpoint）和资源退化（方向五的死元组膨胀）缺乏隔离机制——一个慢的组件可以无声地拖慢整个系统。方向三的 blob 无缓存属于同一类问题（每次请求都走全量链路，没有一个简单的缓存层来隔离高频重复请求）。

这两个根缺失都是**运营层面的架构债务而不是功能层面的缺口**——说明系统功能能力已强，但「可规模化运营」的韧性还需要加固。好在 5 个方向的改动量都不大、风险可控、且都有既有的代码模式可借。这是投入产出比很高的加固。
