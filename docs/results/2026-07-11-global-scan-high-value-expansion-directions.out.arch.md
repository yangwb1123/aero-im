# 架构师分析：Aero IM 方向审计反馈的深层剖析

---

## 1. 架构评估

### 1.1 当前架构优势

**事件驱动骨架设计质量高。** 从反馈的代码验证可以确认几个扎实的架构决策：

| 架构决策 | 成熟度信号 | 架构价值 |
|---------|-----------|---------|
| W3C traceparent 跨进程传播 | `events.rs` 中 `stamp_traceparent` 已嵌入 NATS 发布路径 | 可观测性基础设施**非侵入式埋入**，而非事后补丁 |
| `auto_join_defaults` + `DefaultChannelRepo` | 注册回调中已挂载默认频道加入逻辑 | Onboarding 路径的**架构预留**——设计者考虑过"新用户该进哪些房间" |
| 邀请 API 的 `generate_token/hash_token` + 幂等 + 审计 | 功能完整但前端未暴露 | 证明后端架构层有**独立于 UI 的完整性**——API 设计先于 UI 开发 |
| 安全响应头的分层策略 | `serve.rs` 中 `CorsLayer` + 逐 header 设置 + `AERO_CORS_REQUIRE_ORIGINS` 安全门 | **防御深度 + 环境感知**——开发模式宽松、生产模式严格 |
| `cargo deny` CI job + `deny.toml` 已写就待激活 | CI 配置先于 CI runner | 供应链安全**配置先行**，而非功能完成后再补 |

这些信号指向一个关键结论：**核心架构层已经内置了大多数"事后才想到"的可观测性、安全、和 onboarding 基础设施**。问题不在架构，在**集成和 UI 层未跟上**。

### 1.2 存在的架构局限性

**局限性一：Web 前端是架构的短板。**
- 后端有完整的邀请 API，前端无"邀请按钮"
- 后端有 `auto_join_defaults`，前端无"已加入房间"的可视反馈
- 后端有 `publish_room_event` traceparent，前端无 `performance.now()` 客户端接收
- 后端有 `metrics.rs` 预留的 `message_processing_duration_seconds`，前端的 `performance.now()` 未连接到任何上报

**这不是简单的"缺 UI 逻辑"，而是架构分层中的集成缺失**——前后端之间的**能力间隙（capability gap）** 比代码行数差距更值得关注。

**局限性二：安全防御的左移（shift-left）不足。**
- CSP 是 opt-in（通过 env var），无默认值
- 请求体大小限制无全局 DefaultBodyLimit
- 这些应该是最低防御基线，而非可选配置

**局限性三：资源配置缺少反馈闭环。**
- SFU、SRT、Hub 连接没有 per-connection/per-stream 资源计量
- 没有资源消耗 → 自动治理的闭环（只能靠重启）
- 这是架构层面的可观测性欠债——**有指标名注册但无 observe 调用**

### 1.3 技术债识别

| 类型 | 严重程度 | 描述 |
|------|---------|------|
| **死配置债** | 中等 | `deny.toml` 的 `advisories.ignore` 为空注释模板，someone 需要根据真实 advisory 填充 |
| **光注册债** | 低 | `message_processing_duration_seconds` histogram 已 `describe` 但未在关键路径调用 `observe()` |
| **遗留前端债** | 中高 | `auth_ui.js::onAuthSuccess()` 直接 `enterChat()`——零 onboarding。这是最早期的快速交付遗迹，前端架构需要重构但优先级长期被后端功能压后 |
| **默认安全缺失债** | 中 | CSP opt-in 而非默认打开——与项目的安全 posture 不匹配 |

---

## 2. 扩展方向

我从审计反馈出发，提炼**架构层的扩展方向**，而非产品功能方向。

### 方向 A：可观测性统一层（建议优先级：P0）

**为什么需要：**
- 反馈中确认 traceparent 已存在于事件管线，`message_processing_duration_seconds` 已注册
- 但延迟指标无 observe 调用、WS 扇出无 timing、客户端无上报
- 根本问题：**可观测性基础设施存在但碎片化，缺少统一接入点**

**核心挑战：**
1. **跨进程时序对齐**——traceparent 提供 W3C 上下文，但客户端 `Date.now()` 与服务器 `Instant::now()` 存在时钟偏差
2. **指标注册 vs 指标采集的分离**——`metrics.rs` 定义了指标但业务层没调用 observe
3. **前端没有上报通道**——没有类似 `PerformanceObserver` + Beacon API 的轻量上报

**预期架构变更：**
```
当前状态:
  events.rs → NATS (stamp traceparent) → bus.rs (两阶段解码) → hub.rs (fan_out_raw) → WS
                                                                   ↑
                                                              metrics.rs define 但无 observe

目标状态:
  events.rs → NATS (stamp traceparent + timing) → bus.rs (record received_at + decode_duration)
                → hub.rs (record queue_depth + fan_out_duration) → WS
                     → ws.js (record client_rendered_at via PerformanceObserver)
                          → periodic batch report back to /api/telemetry/perf
                               → metrics.rs: message_processing_duration_seconds.observe()
                               → metrics.rs: NEW hub_queue_depth, ws_write_duration
```

**对现有系统的影响：**
- `hub.rs` 需要加 `#[instrument]` 或手动 `Instant::now()` —— 侵入低
- `ws.js` 需要加 `PerformanceObserver` —— 前端新代码约 30 行
- 新增 `/api/telemetry/perf` POST 端点 → 需要鉴权（限内部/管理员）
- **预期 0 天回溯兼容性破坏**

### 方向 B：Onboarding 集成管线（建议优先级：P1）

**为什么需要：**
- 反馈确认 `auto_join_defaults` + `DefaultChannelRepo` 已存在，前端未利用
- 邀请 API 完整但前端无"邀请按钮"
- 根本问题：**配置/管理端功能与终端用户体验脱节**

**核心挑战：**
1. **注册流程的时序问题**——`onAuthSuccess()` 是同步回调，但加入默认房间是异步的（需等 WS 连接就绪）
2. **欢迎消息的幂等性**——用户断线重连后，不该再次看到"欢迎来到 #general"
3. **多语言支持**——前端 `me.locale` 已从 JWT 解码但未被 onboarding 流程读取

**预期架构变更：**

```
注册流程重构:

当前:
  POST /auth/login → {participant, token} → onAuthSuccess() → enterChat() → ws.connect → msgEmpty 隐藏/显示

目标:
  POST /auth/login → {participant, token, default_rooms: [...], pending_invites: N}

  前端:
    onAuthSuccess() → 1. show welcome slide (first_login boolean)
                       2. ws.connect
                       3. wait ws.ready
                       4. fetch pending invites count → badge
                       5. auto-open first default room (if any)
                       6. show system welcome message as first message (auto-scroll to it)
```

**对现有系统的影响：**
- `auth/login` 响应体需扩展 `default_rooms` 和 `pending_invites` 字段 → 后端 `AuthService` 需新增查询
- `ws.js` 需加 "ready" 后执行的动作队列（类似 `onReady(cb)`）
- 邀请按钮需嵌入 `chat_header` 组件

### 方向 C：安全配置基线化（建议优先级：P1）

**为什么需要：**
- 反馈确认 CSP 是 opt-in，但有立即低成本修复的路径
- 根本问题：**安全配置从"最好有"到"默认有"的范式转变**
- `CorsLayer::permissive()` 是开发环境模式的合理默认值，但缺少安全警告之外的强制门

**核心挑战：**
1. CSP 默认值设计——太严会破坏功能，太松无意义
2. 请求体大小限制需要兼容既有功能（blob 上传的 body 通常 >1MB）
3. WS 帧速率限制需要量化正常使用基线

**预期架构变更：**

```
serve.rs 变更:

当前:
  let csp = std::env::var("AERO_CSP_POLICY").ok();
  if let Some(policy) = csp {
      app = app.layer(...);
  }

目标:
  let csp_policy = std::env::var("AERO_CSP_POLICY")
      .unwrap_or_else(|_| DEFAULT_CSP_POLICY.to_string());  // "default-src 'self'; ..."
  
  if std::env::var("AERO_CSP_REQUIRE").is_ok() && csp_policy.is_empty() {
      panic!("AERO_CSP_REQUIRE set but AERO_CSP_POLICY is empty");
  }
  
  app = app.layer(CspLayer::new(&csp_policy));

  同时新增:
    - DefaultBodyLimit(10 * 1024 * 1024)  // 10MB default
    - WS frame rate limiter in hub.rs (per-connection, configurable)
    - Blob upload per-user rate limit
```

**对现有系统的影响：**
- CSP 默认值可能破坏 CDN 加载的外部资源——需要审计 `index.html` 的 CDN 链接
- `DefaultBodyLimit` 可能破坏大 blob 上传——需要在 blob 上传路由上 `.layer(DefaultBodyLimit::max())` 覆盖
- **需要 3-5 天的回归测试周期**，不适合紧急上线

### 方向 D：资源生命周期管理框架（建议优先级：P2）

**为什么需要：**
- 反馈确认 SFU 无空闲检测、SRT 无 CPU 治理、Hub 无 per-connection 计量
- 但这些消耗型资源**共享一个架构模式**：它们都是长时间运行的协程/线程，创建后不会自动回收
- 根本问题：**缺少统一资源"有界生命周期"管理器**

**核心挑战：**
1. 资源类型异构——SFU 是 `Arc<RwLock<HashMap>>`，SRT 是 TCP socket，Hub 连接是 `mpsc::Sender`
2. 空闲判定标准不同——SFU 可以用 `last_active_at`，SRT 用 `last_data_received`，Hub 用 `last_pong`
3. 优雅关闭的时序——资源管理器不能强行 kill 正在处理媒体帧的 SFU session

**预期架构变更：**

```
引入 ResourceLifecycle trait:

trait ResourceLifecycle {
    type Key: Hash + Eq + Display;
    type Handle: Send + 'static;
    
    fn is_active(&self, handle: &Self::Handle, now: Instant) -> bool;
    fn idle_timeout(&self) -> Duration;
    fn on_expire(&self, key: Self::Key, handle: Self::Handle) -> impl Future<Output=()>;
}

// 统一的扫表定时器
async fn resource_sweeper<T: ResourceLifecycle>(
    registry: Arc<Registry<T>>,
    tick: Duration,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(tick) => {
                let now = Instant::now();
                registry.sweep(now);
            }
        }
    }
}

// SFU 实现
struct SfuLifecycle;
impl ResourceLifecycle for SfuLifecycle {
    type Key = (CallId, PeerId);
    type Handle = SfuMediaSession;
    
    fn idle_timeout(&self) -> Duration { Duration::from_secs(300) } // 5min
    fn is_active(&self, session: &SfuMediaSession, now: Instant) -> bool {
        now.duration_since(session.last_rtp_at()) < self.idle_timeout()
    }
    fn on_expire(&self, key: (CallId, PeerId), session: SfuMediaSession) {
        session.shutdown().await;
    }
}
```

**对现有系统的影响：**
- 新抽象层 `ResourceLifecycle` trait（约 50 行 trait 定义 + 每个资源 ~20 行实现）
- `sfu_media.rs` 需新增 `last_rtp_at: AtomicInstant`
- `hub.rs` 可复用同一框架追踪 WS 连接
- 与 `background.rs` 的既有定时器体系**不冲突**——可以独立运行

### 方向 E：客户端可观测上报通道（建议优先级：P2）

**为什么需要：**
- 方向三（延迟可观测）的完成度取决于客户端是否上报
- 当前无客户端-服务端的性能反馈回路
- 根本问题：**架构缺少从浏览器到后端的轻量可观测协议通道**

**核心挑战：**
1. Beacon API 的可靠性——页面关闭时的数据丢失
2. 上报频率与性能开销的平衡
3. 数据隐私——不应上报消息内容，只上报 timing 元数据

**预期架构变更：**

```
web/ 侧新增:

// perf.js — 独立模块，零依赖，~60 行
class PerfMonitor {
    constructor(ws, sessionId) {
        this.sessionId = sessionId;
        this.buffer = [];
        this.flushInterval = setInterval(() => this.flush(), 30_000);
        // 拦截 ws 消息，记录 arrival time
        ws.addEventListener('message', (e) => {
            const now = performance.now();
            const parsed = JSON.parse(e.data);
            if (parsed.seq) {
                this.buffer.push({
                    seq: parsed.seq,
                    kind: parsed.kind,
                    client_recv_ts: now,
                    render_start: null,
                    render_end: null,
                });
            }
        });
    }
    
    // 组件渲染时调用
    markRendered(seq) { /* update buffer entry */ }
    
    async flush() {
        if (this.buffer.length === 0) return;
        const payload = { session: this.sessionId, samples: this.buffer.splice(0) };
        // 使用 sendBeacon 或 fetch
        navigator.sendBeacon('/api/telemetry/perf', JSON.stringify(payload));
    }
}

服务端:

// 新增 /api/telemetry/perf POST
// 只存储 timing 聚合，不存储用户/消息内容
// 聚合为 Prometheus histogram
```

---

## 3. 接口设计建议

### 3.1 关键模块的接口原则

| 模块 | 当前接口风格 | 建议 |
|------|------------|------|
| `Hub` | `fan_out_raw(msg)` — 内部 mpsc | **保持不变**，但周边加 `num_connections()` `connection_metrics()` 查询接口 |
| `ImService` | `publish_room_event` — 同步返回 | 加 `with_timing` 模式——当 `Instant::now()` 传入时，延迟指标会自动 observe |
| `metrics.rs` | 静态 `lazy_static!` 指标 | **改为 `MetricsRegistry` struct**，支持热替换和 mock（测试时无需初始化全局 metrics） |
| `serve.rs` 中间件 | 硬编码 Chain | 引入 `SecurityPipeline` struct——CSP/BodyLimit/CORS 为可组合层 |

**设计模式建议：装饰者而非条件判断。** 当前 `serve.rs` 用 `if let Some(policy) = csp` 选择性加 layer。更好的模式：

```rust
// 当前
if let Some(policy) = csp { app = app.layer(SecurityHeadersLayer::new(policy)); }

// 建议——始终加 layer，内部处理空策略
app = app.layer(SecurityHeadersLayer::new(csp.unwrap_or_default()));
// SecurityHeadersLayer 内部：
// - 策略为空时：使用 safe-defaults 而非跳过
// - 策略非空时：使用用户配置
```

### 3.2 是否需要新的抽象层

**是，但只加两个轻量层：**

1. **`ResourceLifecycle` trait**（方向 D）——统一 SFU/SRT/Hub 的空闲检测逻辑，避免每个资源类型写一个定时器
2. **`PerfSample` protocol**（方向 E）——客户端上报后端的轻量 schema，由单一 endpoint 收口

**不需要**：
- 不引入新的事件总线抽象（现有 NATS + Hub 已够）
- 不引入新的配置框架（figment + env var + TOML 已够）
- 不引入新的 metrics 系统（Prometheus + OTLP 已够，只需补 observe 调用）

### 3.3 向后兼容性策略

| 变化 | 兼容策略 | 过渡方案 |
|------|---------|---------|
| `auth/login` 响应加字段 | **完全兼容**——前端 `onAuthSuccess` 只解析已存在的字段 | 加 `default_rooms: Vec<RoomPreview>` — 前端 fallback 为空数组 |
| `hub.rs` 加 `num_connections()` | **新方法**——不存在于 trait 接口中，无需实现 | 旧代码不会调用此方法 |
| CSP 默认值 | **behavioural change**——可能破坏 CDN 加载 | 加 env var `AERO_CSP_STRICT=false` 保持宽松模式 |
| `metrics.rs` 改为 struct | **breaking**——需要重构 `lazy_static!` 引用 | Option A：保留静态 forwarder -> 新 struct（适配器模式）Option B：所有 crate 访问 MetricsRegistry 单例 |

---

## 4. 技术选型

### 4.1 需要引入的新技术

| 场景 | 推荐 | 理由 | 备选 |
|------|------|------|------|
| 客户端性能上报（方向 E） | **无新依赖**——`navigator.sendBeacon()` + service-side `axum::Json` | 零新增 dep，浏览器原生 API | `posthog`/`sentry`（太重，需第三方服务） |
| CSP 构建（方向 C） | **无新依赖**——字符串拼接 + regex 验证 | CSP policy 是静态字符串，无需框架 | `tower-http::set_header` 已够用 |
| ResourceLifecycle（方向 D） | **无新依赖**——`trait` + `Arc<Registry>` | 纯 Rust 泛型，无需额外框架 | `tokio-util::CancellationToken` 已内置 |
| SBOM 生成（方向五） | **`cargo sbom`**（`cargo install cargo-sbom`） | 专门为 Rust 设计的 CycloneDX SBOM | `cdxgen`（需要 Node.js） |

**关键判断：这 5 个方向都不需要引入新的第三方框架。** 这与反馈中确认的"已有基础设施成熟度高"一致——需要补的是**集成和配置，而非新技术栈**。

### 4.2 第三方依赖评估标准

鉴于方向五（供应链安全）已识别 `deny.toml` 有基础配置但缺少 `advisories.ignore` 和 `bans.skip`，以下依赖评估标准应该写入 `deny.toml` 的反模式阶段：

```
[advisories]
# 每个 ignore 必须附理由和到期日
ignore = [
    # "RUSTSEC-2026-XXXX" — 不影响音频/视频路径，等待上游 fix，到期 2026-09-01
]

[bans]
# 不允许同一 crate 多版本
multiple-versions = "deny"
# 不允许特定高危 crate
deny = [
    # { name = "time", version = "0.1" },  # 已知 Y2K 问题
]
```

### 4.3 自建 vs 采购

此阶段的 5 个方向全部落在**自建**范围内：

| 方向 | 不采购理由 |
|------|-----------|
| Onboarding | 产品差异化功能——需与既有 `auto_join_defaults` 和邀请系统深度集成 |
| 安全 | 配置项而非产品——CSP/CORS/BodyLimit 全在 axum middleware 层 |
| 延迟可观测 | 需要与既有 traceparent + metrics.rs 集成——通用 APM 不会理解业务 stage |
| 能源效率 | 资源治理需要领域特定知识（SFU 的 `last_rtp_at`、SRT 的 idle timeout） |
| 供应链安全 | 全 CI 脚本生态——`cargo deny`/`cargo audit`/`cargo sbom` 已有现成 CLI |

**唯一的采购候选是方向三中的客户端 RUM（Real User Monitoring）。** 但如果考虑：
- 数据隐私（不能上报消息内容）
- 需要与已有 traceparent 集成
- 轻量需求（仅 5-10 个 gauge/histogram）

结论依然是**自建 ≤60 行前端代码 + 1 个后端 endpoint**，不值得引入第三方 RUM SDK。

---

## 5. 实施路线图

### 优先级调整确认

基于反馈的代码验证，我从架构角度对优先级做最终调整：

| 方向 | 建议优先级 | 理由 | 预估总工时 |
|------|-----------|------|-----------|
| **三·延迟可观测** | **P0** | traceparent 基础设施已就绪，仅缺 observe 调用。快速实现后为其他方向提供量化输入 | ~1.5d |
| **一·Onboarding** | **P1** | A 期 ~2.5d，产品留存影响最高。但依赖方向三的部分基础设施（时序量化） | ~2.5d (A期) |
| **二·边缘安全** | **P1** | CSP 默认值 0.5d 可完成。BodyLimit + WS 限帧 + blob 限频共 ~3d | ~3.5d |
| **五·供应链安全** | **P2** | SRI 0.5d + `cargo audit` CI 步骤 0.5d + SBOM 生成脚本 1d。CI 配置已写但未激活 | ~2d |
| **四·能源效率** | **P2** | 资源快照 0.5d 可用既有基础设施。SFU/SRT 空闲检测 (方向 D) 依赖方向三的 metrics 基线 | ~5-8d |

### 阶段划分

**Phase 0 — 基础设施加固（2-3 天）**
- 方向三：`message_processing_duration_seconds` 在 `publish_room_event` + `hub::fan_out_raw` + `bus_listener` 三处加 `observe()`
- 方向三：前端的 `PerfMonitor` 最小实现（仅 performance.now() + Beacon API 上报）
- 方向五：`index.html` SRI integrity 注入
- **产出：延迟仪表盘就绪、CDN 加载安全**

**Phase 1 — 用户可见改进（4-5 天）**
- 方向一：`onAuthSuccess` 的后注册流程——欢迎消息 + 默认房间自动跳转 + 邀请入口
- 方向二：CSP 默认策略 + `AERO_CSP_REQUIRE` 安全门
- 方向二：`DefaultBodyLimit` 全局限制 + blob 上传路由覆盖
- **产出：新用户留存影响、安全 posture 基线提升**

**Phase 2 — 资源治理（2-3 周）**
- 方向四：`resource_sweeper` 通用框架（方向 D 的核心抽象）
- 方向四：SFU idle timeout + SRT idle timeout 落地
- 方向四：Hub per-connection metrics 集成到 `observability_gauge_samplers`
- **产出：绿色模式 env var 门控就绪、可控的资源生命周期**

**Phase 3 — 持续安全（1 周）**
- 方向二：WS 帧速率限制 + blob 上传限频
- 方向五：`cargo audit` CI 步骤激活 + `deny.toml` advisories.ignore 填充
- 方向五：`cargo sbom` CI 集成 + 容器镜像 Trivy 扫描
- **产出：CI 安全门 + 供应链可审计**

### 风险与缓解

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|---------|
| CSP 默认值破坏 CDN 资源加载 | 高 | 中（CSS/font 失效） | Phase 1 前审计 `web/index.html` 所有外部资源 URL，CSP 默认值采用 "strictest-possible" 模式 + 中间 2 周灰度期 |
| W3C traceparent 时钟偏差导致延迟数据不准 | 中 | 低（相对趋势可用） | 客户端用 `performance.now()`（Relative Timing API），不依赖绝对时间戳；服务端 `Instant::now()` 采集 `queue_depth` 等非时钟指标 |
| SFU idle timeout 误杀活跃会话 | 低 | 高（断流） | Phase 2 初始 timeout 设到 30 分钟 + `observability_gauge_samplers` 监控误杀率；Phase 3 才缩短到 5 分钟 |
| 邀请按钮上线后被用于 spam | 中 | 中 | 加 rate limit（每个用户每分钟最多生成 5 个邀请链接）+ 已有的邀请链接非公开（需知道完整 URL） |
| `cargo deny` CI 激活后历史 advisory 导致 CI 全红 | 高 | 中 | 先在 `deny.toml` 的 `advisories.ignore` 中预先调研并忽略已知安全、不影响的 advisory；再激活 CI job |

### 跨方向依赖关系图

```
Phase 0 (2-3d)
  └─ 方向三: P0 延迟指标 observe()
        │
        ├─→ 方向三: PerfMonitor 前端最小实现
        │
        └─→ 为方向四提供 metrics 基线 ──→ Phase 2 资源治理的量化判断依据
  
Phase 1 (4-5d)
  ├─ 方向一: onAuthSuccess 后注册流程
  │     └─ 依赖: 方向三的 traceparent 时序为欢迎消息打时间戳
  ├─ 方向二: CSP 默认值 (0.5d)
  └─ 方向二: DefaultBodyLimit (0.5d)
  
Phase 2 (2-3w)
  ├─ 方向四: ResourceLifecycle trait + SFU/SRT idle timeout
  │     └─ 依赖: 方向三的队列深度/hub 连接 metrics
  └─ 方向四: Hub per-connection 计量

Phase 3 (1w) 
  ├─ 方向二: WS 帧速率 + blob 限频
  ├─ 方向五: CI 安全门激活
  └─ 方向五: SBOM + Trivy
```

---

## 总结

反馈的代码验证揭示了一个重要的架构真相：**Aero IM 的核心骨架（事件 DAG、跨进程追踪、安全中间件管道、CI 配置）质量高于功能完成度提示的形象**。五个方向中，方向三（延迟可观测）和方向一（Onboarding）是"已有基础设施但未集成"而非"完全缺失"——这是架构债务而非架构缺陷。

对架构师来说，更值得关注的是 **Web 前端作为架构短板**、**安全配置的 opt-in 而非 opt-out 模式**、以及 **资源治理缺少统一生命周期框架**。建议的 Phase 0（基础设施加固）应在 2-3 天内优先完成延迟指标的 observe 调用和 SRI 注入——这两项都是低成本、高安全/可观测回报的 quick wins。
