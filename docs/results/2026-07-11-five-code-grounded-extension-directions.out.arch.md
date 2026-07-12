好的，作为资深架构师，我来基于上述事实核查结论与 AGENTS.md 中反映的系统现状，做系统性的架构分析。

# 架构分析：Aero IM 系统

## 1. 架构评估

### 优势

Aero IM 的事件驱动骨架（NATS JetStream → 进程内 Hub bounded mpsc → WebSocket 扇出）是架构层面最正确的决策之一。它带来了几个关键收益：

- **水平扩展透明**：`hub.stream_watchers` 只负责本地扇出，跨实例靠 NATS subject 和 consumer group 分发。新增实例只是多一对 `run_bus_listener` + `run_live_bus_listener`，无状态争用。
- **背压边界清晰**：`bounded mpsc` 限定了每进程扇出队列长度，NATS 层有 at-least-once 重投兜底——这是典型的「缓冲-重试-降级」三级策略。
- **鲁棒的集群状态分离**：Redis sorted-set 作集群级事实来源（presence/roster/viewer count），避免了「状态粘在进程内存」的常见陷阱。

另一个架构亮点是 **crate 依赖的单向性**（`aero-common` → `aero-bus`/`aero-storage`/`aero-auth` → `aero-im-core` → `aero-server` 等组合层）。这在 Rust 生态里靠编译期强制保障，比微服务间的运行时依赖图更可靠。

### 关键设计债务

| 债务 | 性质 | 风险等级 |
|------|------|---------|
| **SFU 媒体路径未生产接线**（`SfuMediaSession::bind`/`run` 仅在 `#[cfg(test)]` 调用） | 功能性缺口 | **P0**——核心能力不可用 |
| **API 无版本化**（REST + WS 均无版本协商） | 演进性债务 | **P0**——阻断外部生态 |
| **HLS 无访问控制**（`ServeDir::new(&hls_dir)` 直出文件） | 安全性债务 | **P1**——非认证用户可访问 |
| **WS 帧 handler 不全**（`MessageSeen`/`Interaction` 等缺处理） | 功能性缺口 | **P1**——客户端功能缺失 |
| **WHIP→WHEP 桥缺失**（直播看端无拉流路径） | 功能性缺口 | **P1**——直播闭环未完成 |
| **「kind」标签撞名陷阱**（serde `tag="kind"` 与字段名冲突） | 实现性债务 | **P2**——偶发运行时 panic |

### 架构层面的问题诊断

该系统的核心薄弱点不在于单个组件，而在于**生产可观测性与缝合层的缺失程度**：

1. **媒体平面与控制平面耦合过紧**：`SfuMediaSession` 的 `run` 方法直接绑定 `SfuForwarder`（进程内），跨节点桥接 `CallEgress` 虽然已建但 `ensure_egress` 未接线——这导致了媒体层的两层隔离（进程级 ← 节点级）都处于碎块状态。对生产而言，没有哪条媒体路径是完整的。

2. **安全边界不统一**：HLS 直出 vs REST 路由（经 `AuthUser` extractor + `assert_room_access`）有显著的安全水位差。这种悬殊在混合部署场景中尤其危险——攻击者只需找到一条未校验的路由即可绕过。

3. **枚举类型演进无向前兼容机制**：`RoomEvent`/`StreamEvent` 的 serde `tag="kind"` 标签编组在添加新 variant 时会反序列化失败（`unknown variant`）。这对 at-least-once 消息系统是致命问题——旧版本无法跳过不理解的事件类型。

---

## 2. 扩展方向

基于事实核查分析——方向②③④⑤已有覆盖，方向①（WHIP→WHEP）为较新颖切入点——我列出以下高价值扩展方向：

### 方向 A：WHIP→WHEP 桥补全（优先级 P1）

#### 为什么需要
直播摄入（WHIP/RTMP/SRT）已可推流→HLS，但**推流→实时观看（WHEP）的闭环未完成**。`WhepSession::run` 零调用方意味着直播端只能等 HLS 切片更新（数秒延迟），无法使用 WHEP 亚秒级延迟。这是推流与拉流功能不对称的明显缺口。

#### 核心挑战
- **NalSource 频道扇出设计**：直觉上是推线程的 H.264 NAL 单元以单生产者多消费者模式扇出给 N 个 WHEP 订阅者。关键问题包括：背压策略（慢消费者如何处理）、GOP 缓存（新连接是否发关键帧提示 PLI）、频道注销与重连时序。
- **跨实例 stic ky routing**：推流实例与 WHEP 拉流实例可能不同。需要将 `NalSource` 注册到某种实例可寻址的 registry（Redis 或 NATS key-value store），让 WHEP 接入请求能路由到推流所在节点。已有 `call_route` / `stream_route` heartbeat 模式可复用。
- **RTCP PLI/FIR 信令回传**：WHEP 订阅者请求关键帧时，需要跨实例信令回推到推流端 str0m 会话，触发编码器产 IDR 帧。

#### 预期架构变更
1. `aero-live-webrtc` 新增 `NalSource` registry（基于 Redis 的 `(stream_id → node_id)` 映射）
2. WHIP 推流侧增加 `NalSource` 生命期管理（register on publish / unregister on stop）
3. WHEP 拉流侧增加跨实例桥接 logic（本地无 NalSource 时，通过 `call-bridge` 机制拉取）
4. 新增 `RtpRelay` trait 抽象本地与跨实例两种路径

#### 对现有系统的影响
- 无破坏性变更（所有新增代码，不影响既有 HLS 路径）
- 需要修改 `LiveStreamConfig::go_live` 在 publish 时注册 NalSource

### 方向 B：API 版本化与枚举演进协议（优先级 P0）

#### 为什么需要
该项目依赖 **at-least-once 消息 + serde 标签枚举**的组合，但目前没有任何版本协商机制。这意味着：
- 新后端 variant 无法推送存活旧实例（反序列化失败）
- 外部开发者无法依赖稳定 API 契约
- WS 帧协议与 REST 路径都没有 release lifecycle

#### 核心挑战
- **枚举的向前兼容**：serde 的 `tag="kind"` 默认是严格模式，无法静默跳过未知 variant。需要引入 `#[serde(deny_unknown_fields)]` 的反模式，或改为 `tag` + `content` 带 `other` 兜底。
- **WS 版本协商**：`/ws` 路径需要在 upgrade 握手阶段协商协议版本，RFC 6455 不直接支持——需要自定义头 `Sec-WebSocket-Protocol` 传版本号。
- **REST 版本策略**：URL path variant（`/v1/` vs `/v2/`）vs Content-Type negotiation（`Accept: application/vnd.aero.v1+json`）。前者更易实现和测试。

#### 预期架构变更
1. 新增 `aero-api` crate 定义 WS 帧 schema 和 REST contract（swagger/OpenAPI 手写 → 逐步自动化）
2. 调整 `RoomEvent`/`StreamEvent` 的 serde 配置：`#[serde(tag = "kind", content = "data")]` + `#[serde(other)]` 兜底 variant
3. `Hub` 层增加 `ProtocolVersion` 协商逻辑，根据客户端版本决定扇出哪些 event
4. 新增 `Sunset` header 中间件标记废弃路径

#### 对现有系统的影响
- 枚举结构的 serde 调整需要全量审计每位使用方（反序列化 + 匹配臂）→ **测试覆盖率高方可安全操作**
- WS 版本协商涉及 SPA 端改动（连接时传版本号，处理 426 Upgrade Required）

### 方向 C：HLS 安全访问控制（优先级 P1）

#### 为什么需要
`ServeDir::new(&hls_dir)` 无任何鉴权——任何人知道 `.m3u8` URL 即可拉流。对直播平台这是价格不菲的风险：未授权流可被直接嵌入、录制、分发。

#### 核心挑战
- **Token 签名 + 过期**：对每个 HLS segment 请求校验 signed token（HMAC-SHA256），典型做法是用 Axum middleware 拦截 `/hls/*` 路径，校验 `?token=` 参数。
- **CDN 集成**：若部署 CDN（CloudFront/Cloudflare），需要 CDN 侧理解 token 格式，把鉴权压力移出源站。这涉及 CDN 侧 custom origin policy 或 edge function。
- **预录制 vs 直播差异化策略**：预录制 VOD 可用长期 token，直播需要短期 token + 按 segment 刷新——影响客户端 HLS player 的 token 刷新逻辑。

#### 预期架构变更
1. 新增 `HlsAuthLayer` 中间件（Axum `Middleware`），拦截 `/hls/:stream_id/*.ts` 和 `.m3u8`
2. 在 go-live 和录制完成时生成 signed token，下发给授权用户（通过 WS 帧或 REST API）
3. 客户端 hls.js 集成 token 刷新回调（`getLicenseUrl`/custom `fetch` wrapper）
4. CDN 探测：若检测到 `AERO_CDN_ORIGIN_SECRET` env，切为 CDN 原生鉴权（CloudFront signed cookies / Cloudflare Token Authentication）

#### 对现有系统的影响
- 无 HLS writer 或切片逻辑改动
- SPA 端需升级 hls.js 集成代码
- 对低延迟场景（LL-HLS）需处理 token 性能开销

### 方向 D：跨节点媒体平面完善（P0→需先于其他方向）

#### 为什么需要
`call-bridge` 的设计（`SfuMediaSession.on_rtp` → `SfuForwarder.on_rtp` → 本地 + `CallEgress` 跨节点）已经建好，但 `ensure_egress` 未接线。这意味着多实例部署时，SFU 媒体无法跨实例转发——通话的媒体平面在多于一个实例时断裂。

这与方向①（WHIP→WHEP）不同：通话媒体走的是 RTP over UDP relay（`bridge_frame`），而非 NATS 总线。

#### 核心挑战
- **UDP hole punching / NAT 穿透**：`call-bridge` 目前假定两端可直接 UDP 通信。对跨 NAT 场景需要 STUN/TURN fallback。
- **时序重映射**：两节点间的 RTP 流需要 seq/ts 重映射以保持单调性（str0m 侧已在 `SfuForwarder` 做，但跨桥时的时钟差需补偿）。
- **集群发现**：`CallBridge` 需要知道 peer 节点的 endpoint。目前 `call_route` heartbeat 提供了 registry，但 `SfuMediaSession` 没有消费这个 registry。

#### 预期架构变更
1. `SfuMediaSession` 接入 `call_route` heartbeat 的订阅 view，在 `run` 循环中动态建立/销毁 egress
2. 新增 `BridgeConnector` 负责 UDP hole punching 和 keepalive
3. 每节点 `CallBridgeConfig` 增加 `advertise_address`（节点公网地址）
4. 故障场景：给 watchdog 超时关闭 peer egress，回退本地 only

#### 对现有系统的影响
- `sfu_media.rs` 的 `run` 循环需改——目前只停在 `#[cfg(test)]`
- `CallOrchestrator` 需感知 SFU 桥接 state（节点数＞1 时启用跨节点，单节点跳过）

### 方向 E：客户端 WS 帧 handler 补齐（P1）

#### 为什么需要
`MessageSeen`/`Interaction` 等帧在 WS 协议中有定义，但服务端 handler 缺失。这导致客户端发送的某些帧被静默丢弃——UI 不更新（「已读」状态不显示、「互动」计数归零），用户感知为 bug。

#### 核心挑战
- **幂等性**：`mark_read` 本质是游标推进，可重入幂等。`Interaction`（消息表情互动）需要原子递增，与 `Reaction` 领域重叠但交互语义不同——需确认是否拆分。
- **帧路由派发**：当前 WS 帧框架（`ClientFrame` match）可能缺分支。最佳实现是新增 handler，而非扩展现有的 `send_message` 等 match 臂。

#### 预期架构变更
1. 在 `ws/ws_impl/` 下新增 `seen.rs` 和 `interaction.rs` handler
2. `Hub` 层注册对应的 `RoomEvent` variant（如果这些帧需要广播给同房成员）
3. 与 `reaction` 路径的边界划定：`Interaction` 是轻量级临时互动（投票/表情雨），`Reaction` 是持久化消息反应

#### 对现有系统的影响
- 无需迁移或存储层改动（如 `mark_read` 只在 Redis 游标上操作）
- SPA 端已有代码发送这些帧，补齐后功能自动生效

---

## 3. 接口设计建议

### 核心设计原则

| 原则 | 说明 | 违反后果 |
|------|------|---------|
| **向后兼容优先** | 新增 enum variant 不得破坏旧版本反序列化 | 滚动更新期间 panic |
| **显式契约高于隐式约定** | WS 帧格式、NATS subject payload、REST body 都应有文档化 schema | 跨团队协作断裂 |
| **单一版本的真理来源** | 帧 schema 定义在 `aero-common`，web 端和 server 端都从此 crate 派生 | 两端版本不匹配 |

### 需要引入的抽象层

**1. 总线事件版本化层**

当前 `RoomEvent`/`StreamEvent` 的直接 serde derive 没有版本间隙。建议引入轻量 `ProtocolMessage` 封装：

```rust
// 概念性设计
struct ProtocolMessage {
    version: u8,          // 当前 0，未来递增
    seq: u64,             // per-subject 单调seq（已有）
    event_type: String,   // "room_event" / "stream_event" / "system"
    payload: serde_json::Value,  // 实际event，宽松反序列化
}
```

这种**双层编组**（外层固定结构 + 内层宽松 payload）比当前单层 tag 枚举更能适应向前演进。`Hub` 扇出时按客户端协商版本做 payload 兼容转换。

**2. 媒体平面的 trait 抽象**

当前 SFU 链路（`SfuMediaSession` → `SfuForwarder` → `CallEgress`）是具体类型直连，未来多实例桥接需要接口抽象：

```rust
// 概念性设计
trait MediaEgress: Send + Sync {
    fn push_rtp(&self, ssrc: u32, seq: u16, payload: &[u8]);
    fn is_alive(&self) -> bool;
}

trait MediaIngress: Send + Sync {
    fn on_rtp(&mut self, ssrc: u32, seq: u16, payload: &[u8]);
    fn request_keyframe(&mut self);  // RTCP PLI
}
```

这样 `SfuForwarder` 可以同时持有本地 `Vec<Box<dyn MediaEgress>>` 和跨节点 `Vec<Box<dyn MediaEgress>>`，无需特化代码。

### 向后兼容策略

- **REST**：URL path prefix versioning（`/v1/rooms/:id` vs `/v2/rooms/:id`），旧路径加 `Sunset` header + deprecation duration
- **WebSocket**：升级握手时通过 `Sec-WebSocket-Protocol` header 协商版本；服务端返回 426 要求客户端升级
- **NATS subject**：`im.room.{id}.v1` → `im.room.{id}.v2`，旧 consumer 不自动切；过渡期双发
- **持久化数据**：DB 行加 `schema_version` 字段，仓储层做升级 on read（如 `SqlxRepo::upgrade_if_needed`）

---

## 4. 技术选型

### 缺口分析与候选

| 场景 | 现方案 | 缺口 | 候选方案 | 评估 |
|------|--------|------|---------|------|
| CDN 分发 HLS | 无 | 源站直出，无全球加速 | CloudFront / Cloudflare / Bunny.net | **Cloudflare** 因 edge function 更灵活，且 Worker 可跑鉴权逻辑；Bunny 成本最低 |
| 鉴权 token 签名 | 无 | HLS 无保护 | 自建 HMAC + CDN edge | 自建成本低，与 CDN 配合即可 |
| API 契约文档 | 手写 | 不一致·易过期 | OpenAPI 3.1 从代码注释自动生成 | `utoipa`（纯 Rust，与 axum 集成好）；或 `aides`（声明式） |
| 跨节点 UDP 桥接 STUN/TURN | 无 | NAT 穿透 | `str0m` 已有 ICE 实现 / 自建 TURN | 复用 str0m 降低新依赖风险 |
| 可观测性 | Prometheus + OTLP | 无结构化日志基线路由 | `tracing` 已有，加 `opentelemetry-otlp` exporter | 已有，只需正确配置 |

### 自建 vs 采购决策矩阵

| 决策 | 建议 | 理由 |
|------|------|------|
| CDN | 采购（CloudFront/Cloudflare） | CDN 是规模化的基础设施建设，自建 BGP 边界成本和复杂度远高于采购 |
| HLS 鉴权 | 自建 | 只需 HMAC 签名 + 中间件，≤200 行 Rust 代码，无第三方依赖成本 |
| API 文档自动生成 | 自建（utoipa 轻量集成） | 纯 Rust 生态已有成熟工具，无需外部服务 |
| STUN/TURN | 条件性采购 | 若目标用户都在公网或同一 NAT→自建 STUN（走 str0m 已含 ICE）足够；若面向复杂企业 NAT 环境→采购 TURN 服务 |
| 消息推送（APNs/FCM） | 已有自建 | 不要改——保持自建网关，避免依赖第三方 Push SDK 的隐私合规风险 |

### 技术栈扩展评估

不应引入的新依赖：
- **消息队列替代 NATS**：当前 NATS JetStream 满足所有需求（at-least-once/durable/ephemeral/key-value），无更换理由
- **微服务拆分**：进程内架构（crate dependency DAG + 进程内 Hub 扇出）对当前负载非常合适，拆分微服务只会增加延迟与运维复杂度

可考虑引入的新依赖：
- **utoipa**：零运行时开销，宏生成 OpenAPI 3.1 文档，与 axum 0.7 生态对齐
- **cloudflare** crate（若选 Cloudflare CDN）：用于边缘鉴权的 Rust SDK，非强制

---

## 5. 实施路线图

### 优先级判定

```
方向 B (API 版本化)       ─── P0
│                             引擎不可对外
├── 影响所有方向
│
方向 D (跨节点媒体)       ─── P0
│                             核心语音视频功能不可多实例
├── 阻塞方向 A 的部分实现
│
方向 A (WHIP→WHEP)       ─── P1
│                             直播延迟完整链路不闭环
│
方向 C (HLS 安全控制)     ─── P1
│                             安全风控缺口
│
方向 E (客户端 handler)   ─── P1
│                             功能静默不工作
│
```

### 阶段划分

#### 阶段 1：协议演进底座（4-6 周）

**目标**：建立版本化的去程框架，使新功能可在版本屏障后安全部署。

- [ ] 枚举向前兼容改造：`RoomEvent`/`StreamEvent` serde 调整，引入 `#[serde(other)]` 兜底 variant
- [ ] `ProtocolMessage` 外层封装设计，新旧格式双写过渡
- [ ] WS 版本协商实现（`Sec-WebSocket-Protocol` header 解析 + handler 版本分派）
- [ ] 版本化中间件在 `routes::build` 中 `merge` 时附加 version prefix
- [ ] 全量 CI 集成测试：新旧枚举 variant 混合发送验证不 panic

**风险**：serde 调整属于 break change——必须在正式发布前完成，且与前端版本对齐。缓解：feature flag gate，先开 staging 后进生产。

#### 阶段 2：媒体平面闭环（6-8 周，与阶段 1 可部分并行）

**目标**：WHIP→WHEP 桥接和跨节点 SFU 桥接完成生产接线。

- [ ] `NalSource` registry 设计（Redis-based，复用 `stream_route` heartbeat TTL）
- [ ] `MediaEgress`/`MediaIngress` trait 抽象（本地 + 跨实例）
- [ ] `SfuMediaSession::run` 生产实例化（从 bind 到完整 run loop 接线）
- [ ] WHIP 推流转 NalSource 注册 + 生命周期管理
- [ ] WHEP 拉流跨节点回退逻辑
- [ ] RTCP PLI 信令回传（跨实例 keyframe request）

**风险**：`SfuMediaSession` 的 str0m 路径在测试外未被运行过，真实浏览器对端的兼容性未知。缓解：在 staging 环境先用 OBS（RTMP→WHIP 转换）和 hls.js 做集成测试。

#### 阶段 3：安全加固 + 客户端补齐（4-6 周，可与阶段 2 并行）

**目标**：消除 HLS 泄露风险，补齐客户端帧处理路径。

- [ ] `HlsAuthLayer` 中间件 + token 签发生命周期管理
- [ ] CDN 边缘鉴权（Cloudflare Worker 或 CloudFront Lambda@Edge）
- [ ] hls.js 客户端 token 刷新回调集成
- [ ] `MessageSeen` handler 实现 + `Interaction` 帧处理
- [ ] 安全审计：扫一遍所有 `ServeDir` 直出路径

**风险**：HLS token 刷新对 LL-HLS（低延迟模式）的性能影响。缓解：token 有效期可配置，最小颗粒为 segment 时长（2-6s）。

#### 阶段 4：架构能见度 + 清理（持续）

**目标**：建立 API 文档、枚举演进自动化监控、技术债务清理。

- [ ] `utoipa` 或 `aides` 集成，构建时生成 OpenAPI 文档
- [ ] `api-docs/` 目录维护 + 版本化发布脚本
- [ ] serde tag 清理：全仓检查是否有其他 `tag="kind"` enum 撞字段
- [ ] 可观测性增强：`room_event` 版本号作为 Prometheus label
- [ ] 知识库同步：更新 `AGENTS.md` 中架构决策记录（ADR）

### 风险矩阵

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| serde 枚举改造导致生产实例反序列化失败 | 中 | 灾难 | 双格式过渡期 + 金丝雀部署 + CI 全量枚举测试 |
| `SfuMediaSession` 与真实浏览器握手不兼容 | 中 | 高 | 用 staging 环境 + 真实 WebRTC 客户端（Chrome/FF）做集成测试 |
| HLS token 签名增加 50ms+ 延迟 | 低 | 中 | token 校验在内存 HMAC 计算 < 1μs；CDN 边缘更近；可预签 token 批次 |
| 多实例 UDP 桥接丢包或重组乱序 | 中 | 中 | 显式调用 nack/pli；str0m 的 seq 重映射已有单测保障 |
| 方向④（API 治理）与方向②（媒体桥）争夺工程资源 | 高 | 高 | 阶段 1/2 之间用 feature team 隔离：一队做协议演进，一队做媒体桥 |

---

## 总结

Aero IM 的底层架构（事件驱动 DAG + 进程内扇出 + 集群 Redis 状态）是经过深思熟虑的——它对准了水平扩展和实时性的核心需求，而 crate 依赖的单向性保障了编译期架构整洁。真正的架构债务出现在**缝合层**：媒体平面的生产接线、API 的版本化、HLS 的安全防护——这些都是搭建桥梁而非重建地基的工作。

关键行动建议：
1. **先做协议底座（方向 B）再动媒体**——没有版本化，后续所有新 enum variant 都是滚动更新的地雷
2. **WHIP→WHEP 桥接采纳 NalSource 频道扇出**——已有的 `SfuForwarder` 订阅者模式是已验证的路径，站在既有模式上扩展优于重造
3. **每个方向都保持 crate 单向依赖**——避免在 `aero-live-webrtc` 中引入对 `aero-server` 的反向引用，如需组合逻辑走 trait + 注册模式
