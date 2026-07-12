现在我已完整了解现有架构景观、设计规范和产品方向文档。以下是我的全面分析。

---

# 架构分析：Aero IM 的 5 个未覆盖产品方向

## 1. 架构评估

### 现有架构的优势

该代码库展示了一个异常清晰的**分层事件驱动架构**，具有以下强项：

| 层面 | 优势 | 证据 |
|---|---|---|
| **事件骨干** | NATS JetStream 作为跨实例事实来源 + 进程内 Hub 扇出创建了清晰的写入路径 | durable `aero-server` consumer + ephemeral `live.stream.*` — 语义清晰地为投递可靠性分层 |
| **Crate 边界** | 16 个 crate 具有严格的自下而上依赖关系，无循环依赖 | 从 `aero-common`（叶子节点）到 `aero-server`（组合根）的单调 DAG |
| **存储分离** | `XRepo` 模式 + `BlobStore` trait + `EventBus` trait 使基础架构可替换 | `blob_store_from_env` 根据 env 切换 LocalFs ↔ S3BlobStore |
| **后台任务治理** | 所有 bots/workers/timers 在 `bin/boot/` 中统一组装，在 AGENTS.md 中有清晰的 opt-in 门控 | 有/无预算的 `OPT-IN` env-gated bots、ephemeral retention sweepers |
| **AI 成本控制** | 双重预算（per-ws + 全局）+ 按类型加权 + 真实 token 计费 + 谨慎 fail-close | `KeyedCostBudget`、`CostBudget`、`MAX_ATTEMPTS=5→dead` |
| **媒体管道** | str0m 的纯 Rust SFU + H264 解包/打包 + MPEG-TS mux — 无 C 绑定 | `aero-live-webrtc` + `aero-live-hls` 具有完整单元测试 |

### 关键架构债务与局限性

**1. Web SPA 作为「调试客户端」的架构约束**

这是该文档中所有 5 个方向中最深的结构性限制。该设计规范将 Web 客户端明确定位为「调试/联调客户端」，零外部依赖。但本文中描述的每个方向（持久空间、会议内协作、开发者平台、地点感知发现）**都需要丰富的交互式 UI**。5900 行 vanilla ES2020 无法在不增加依赖的情况下渲染地图/白板/API Playground。

这是架构债务，不是 bug——设计围绕这一约束是**正确的**（对于 MVP 是明智之举），但每个方向都迫使需要对 Web 架构进行重新思考。

**2. Bus 层中的序列化负担**

当前的 `RoomEvent` → `run_bus_listener` 路径执行**两阶段解码**（原始 JSON lift seq → 类型化 RoomEvent）以处理 per-subject seq。这在没有 schema registry 的情况下是必要的，但在高吞吐（>1000 个房间事件/秒）下，每个事件两次 JSON 反序列化会变得昂贵。这一约束直接影响了方向三的会议内投票计数（需要高吞吐的事件流）。

**3. 可扩展性：单 PG 主库瓶颈**

ROADMAP 正确识别了此问题：所有查询命中单个 PG 主库，默认 16 个连接。文档中的 5 个方向都放大了这一压力：
- 方向一（持久空间）：每个用户通过 WebSocket 连接 + presence ZADD 的持续活动
- 方向二（消息级 TTL）：更细粒度的清扫操作
- 方向三（会议内工具）：投票计数、画布操作日志、举手事件
- 方向四（开发者平台）：API Playground 调用执行实际查询
- 方向五（地理空间）：半径查询需要 PostGIS 索引扫描

**4. AI 推理管道仍然串行**

尽管有 `AiWorker` + `SKIP LOCKED` 任务窃取，但 AI 推理路径（`answer_question`、`moderate`、`transcribe`）**在每次调用期间都会阻塞一个 tokio 任务**。方向三（会议内白板 + AI 摘要）和方向一（空间内的 AI 活动检测）将显着增加 AI 并发。当前预算系统（per-ws 60、全局 300）可能在 50 人会议场景中被证明不足。

**5. 媒体上层 seam 状态**

文档正确指出了 `call_bridge_supervisor` 和 `sfu_media_session` 是「已构建 + 已测试，但 socket 循环在接线之前不要写」。方向一完全依赖这些组件——如果你不能跨节点桥接，就不能有真正的 Discord 级持久空间。这是架构中最关键的未接线 seam。

---

## 2. 高价值架构扩展方向

基于对文档的分析和代码库的验证，以下是我从架构角度对 5 个方向的评估和建议的重新排序：

### 方向 A（P0 · 架构基础）：修复接线媒体 seam + 多节点 SFU 验证

**为什么是 P0（非文档中的 P1）**：
方向一和方向三从根本上依赖于生产级 SFU。在 SFU 场景稳定之前，不应该向服务器添加持久的 Discord 式语音频道。这不仅是 add-feature——它暴露了一个未经测试的分布式媒体架构。

**核心挑战**：
- 当前 `call_bridge_supervisor` 具有 `ensure_egress`（本地 RTP → UDP 到远程节点），但生产代码路径未实例化
- 在没有真实 WebRTC 对端的情况下，桥接延迟/抖动模式无法预测
- 当前测试使用 localhost 环回，无法暴露真实网络问题

**预期架构变更**：
1. `sfu_media.rs` 中的 `run(cancel)` 循环从 `#[cfg(test)]` 门控移动到生产连线路径
2. `CallBridgeSupervisor` 获得基于网格的节点发现（通过 NATS 或 Redis），以取代当前静态配置
3. 用于 SFU 分配和桥接路由的每节点负载指标（`AERO_STREAM_ROUTE_HEARTBEAT_SECS`）

**对现有系统的影响**：
- 对现有 IM 或直播业务代码**零更改**
- 需要额外的 Prometheus 指标（`bridge_packets_sent`、`bridge_rtt_ms`）
- 如果桥接延迟变得不可接受，回滚到单节点 SFU 很简单

### 方向 B（P1 · 产品范式）：持久音频/视频空间（文档的方向一）

**为什么这是正确的 P1**：
它优化了现有投资（SFU、CallRosterStore、hub.stream_watchers）并将其重新定位为长期抽象。它不是加功能——它是**重构「房间」的含义**。

**核心挑战**：

| 挑战 | 复杂度 | 缓解 |
|---|---|---|
| 倒置生命周期：永久 space_id vs session_id | 高 | 新表 `voice_spaces` + 将 space_id 映射到 channel_id，与 rooms 表无关 |
| SFU 资源静默管理（空闲时待机） | 中 | 复用 `AERO_CALL_ROUTE_HEARTBEAT_SECS` 定时器 + 最后一个用户离开时使用 `tokio::select` 自动驱逐 |
| 音频级混音以实现 50+ 人扩展 | 高 | 音频混音器（将 N 个流混合为 1 个）是一个独立的 crate 级项目；Phase 1 仅用于选择性转发（最多 20 人） |
| Web SPA 状态管理 | 中 | `state.activeSpaces` 是与 `state.room` 并行的新维度 |
| 移动端/后台音频 | 极高 | 该文档不提供移动客户端。PWA Service Worker 可以保持 WebRTC 连接，但功能有限 |

**为什么音频级混音应被推迟**：
方向一的文档提到了 50+ 人的密度。Discord 用选择性转发处理最多 50 人（没有混音）。在客户端（而不是服务器）使用 Simulcast + 空间音频 WebRTC 扩展，在 N 达到 ~100 之前都可以接受。在此之前不要实现服务器端混音。

**对现有系统的影响**：
- 架构影响：`SfuRouter` 获得一个「持久」模式，该模式在无媒体时保持但分配最小资源
- 数据影响：需要 `voice_spaces` 迁移 + 现有通道类型中新增 `kind: voice`
- 操作影响：每个持久空间的 SFU 内存占用增加（约 50KB/流）

### 方向 C（P1 · 平台）：开发者体验平台（文档的方向四）

**为什么这是 P1 而非 P2**：
文档低估了这个方向的战略价值。对于 Aero IM——一个**AI 原生平台**——开发者平台不仅仅是 API 文档。它是第三方 bot/agent 集成的分发渠道。没有 API Playground 和 Webhook Inspector 的 Slack 是封闭的。

**核心架构决策**：

| 子方向 | 架构方法 | 建议 |
|---|---|---|
| **API Playground** | 纯前端（调用现有 REST 端点） | 使用现有的 `AuthUser` 提取器运行，但设置 `X-Aero-Playground: readonly` 头，由中间件强制执行 |
| **WS Debug Console** | 浏览器中的 WS 帧回显 | 为 WS 帧创建一个 `WsMirror` 结构体，发送到 `/ws/debug` 处的单独 WS 处理程序 |
| **Webhook Inspector** | 后端 + 前端 | 复用 `webhook_delivery_log` 表 + `webhook_admin.rs` DLQ 查询；添加 `replay_webhook` 路由 |
| **Bot Sandbox** | 新的临时 bot 运行时 | `bot_sandbox.rs` 创建一个临时 bot（生存期 24 小时），具有自己的孤立事件订阅 |

**后端与前端工作**：
- Phase A（3-4 周）：**主要是 UI**。现有后端 180+ 路由可直接用于 API Playground
- Phase B（6-8 周）：需要后端为 Bot Sandbox 创建新的 `ephemeral_bots` 表 + `bot_sandbox_events` 表

**对现有系统的影响**：
- 风险：API Playground 的**读/写安全**。建议：严格只读（`HEAD`/`GET` 端点），除非用户显式启用「写模式」
- 风险：WS Debug Console 的**隐私**。帧必须进行清理（在 `WsMirror` 层中剥离 `auth_token`）

### 方向 D（P2 · 安全/隐私）：时效性消息（文档的方向二）

**架构影响是中等的，但合规交互是复杂的**。

| 子系统 | 变更 |
|---|---|
| `messages` 表 | 新增 `expires_at TIMESTAMPTZ` + `burn_after_reading BOOLEAN`（加上正确的默认值） |
| 清扫定时器 | 新增 `sweep_expired_by_ttl` 阶段（在 retention sweep 之前），区分法务保全 |
| `Message` Rust 结构体 | 新增字段 + 在 `soft_delete_audited` 中新增 `delete_reason` 枚举 |
| 审核事件 | 扩展 `audit_events` 以区分 `deleted_by_ttl`、`deleted_by_burn`、`deleted_by_author` |
| AI 系统 | 过滤掉 `burn_after_reading = true` 的消息进入 `AiWorker`/`embedding_backfill` |
| OOO Bot | TTL 继承：OOO 回复继承原消息的 `expires_at` |
| 推送 Bot | `burn_after_reading` 的消息跳过预览文本 |

**关键架构风险**：
- **阅后即焚的多设备同步**：设备 A 阅读并触发删除。设备 B 尚未同步。需要在 `delivery_cursor`（ROADMAP 方向三）之上加一把**分布式锁**来协调 burn 触发
- **法务保全覆盖**：`legal_holds` 模块需要一个 `sweep_suppression` 函数，在 TTL 清扫期间跳过 `is_held` 的消息行
- **与信息隔离墙的交叉**：如果发件人和收件人处于不同的隔离墙墙，则阅后即焚不应产生已读回执

### 方向 E（P2 · 产品/直播）：通话中协作套件 + 空间感知直播（文档的方向三和五合并）

**为什么我合并了这两个方向**：
它们共享一个共同的基础架构问题：**富交互前端状态**。白板需要 CRDT/画布引擎。地图需要 Leaflet/MapLibre。两者对于零依赖的 Web SPA 都是不可行的。它们的架构依赖性——一个用于实时协作的**富客户端框架**——是相同的。

**架构预研**：

| 组件 | 前端引擎 | 同步模式 | 状态 |
|---|---|---|---|
| 白板 | tldraw / Excalidraw（如有必要，使用 iframe 沙箱） | CRDT（y-websocket over WebRTC DataChannel vs WebSocket） | 新依赖 |
| 共同笔记 | 复用 `canvas.rs` op 日志 | WS 上的 gap-free seq（已存在） | 带回现有 |
| 会议投票 | 现有 polls API + 新的 `CallEvent::PollOp` | WS `live.stream.*` 或 `im.room.*` | 后端变更小 |
| 举手/反应 | 新 WS 帧 `CallEvent::HandRaised` | WS `live.stream.*` | 小 |
| 地图 | Leaflet（CDN，轻量） | 现有 REST + WS | 新依赖 |

**架构观察**：会议内投票和举手**不应**使用 `live.stream.*` NATS subject。它们应使用与会议关联的 `im.room.*` subject，以利用 durable consumer 进行投递保证。

---

## 3. 接口设计建议

### 3.1 持久空间 API 设计原则

在 `CallOrchestrator` 旁边引入 `VoiceSpaceService`，而不是修改它。`CallOrchestrator` 的接口是 session 导向的，任何修改都会破坏现有设计。

```
// 新 trait — 不与现有通话 API 共存
trait VoiceSpaceService {
    async fn get_or_create_space(channel_id: &ChannelId) -> Result<SpaceId>;
    async fn join_space(participant_id, space_id) -> Result<SpaceConnection>;
    async fn leave_space(participant_id, space_id) -> Result<()>;
    async fn space_roster(space_id) -> Result<Vec<SpaceMember>>;
    async fn auto_deprovision_idle(timeout: Duration); // 内部，由定时器驱动
}
```

**关键设计选择**：
- 每个 `VoiceSpace` 映射到一个 `Channel`（而非 `Room`），以与文本兄弟对齐
- `SpaceConnection` 与 `CallSession` 不同——它在参与者断开时不会结束
- SFU 资源是一个实现细节：`VoiceSpaceService` 内部管理 `SfuRouter` 生命周期

### 3.2 消息生命周期接口

引入 `MessagePolicy` 作为消息元数据的一部分，而不是向发送 API 添加参数：

```rust
// 新结构体，添加到 Message 元数据
pub struct MessagePolicy {
    pub ttl: Option<Duration>,           // 发送后 TTL
    pub burn_after_reading: bool,
    pub max_views: Option<u32>,          // 仅查看次数（未来方向）
    pub retain_for_legal: Option<bool>,  // 由法规覆盖，表示保存
}
```

这避免了向后兼容性问题：现有客户端在 `send_message` 中发送的旧消息的 `policy` 为 `None`，接收默认的「永远保留」行为。

### 3.3 开发者平台 API 扩展

开发者平台不应修改现有 API——它应**包裹**它们：

```
// 新端点从外部包裹现有系统
GET  /api/dev/endpoints              → 检索所有 API 端点的 OpenAPI 规范
POST /api/dev/playground/exec        → 使用只读安全保护执行请求
GET  /api/dev/webhooks/deliveries    → 获取投递日志（现有）
POST /api/dev/webhooks/:id/replay    → 重放投递
GET  /api/dev/ws/session/:id/frames  → 检索已记录的 WS 帧（非实时）
```

建议在路由器级别添加 `X-Aero-ReadOnly: true` 头以及由 Playground 中间件验证的匹配 `AeroPlayground(policy: ReadPolicy)` 提取器。

---

## 4. 技术选型

### 需要评估的新依赖项

| 方向 | 候选 | 评估标准 | 推荐 |
|---|---|---|---|
| 白板（方向三） | tldraw vs Excalidraw vs yjs | 打包大小、Rust/wasm 兼容性、活跃维护、离线支持、许可证 | **Excalidraw**（更轻、MIT，如果是 iframe 沙箱，仅 HTML+JS 则无打包） |
| 地图（方向五） | Leaflet vs MapLibre GL vs Google Maps | 许可证（MapLibre 无限制）、不需要 API key、CDN 负载能力 | **Leaflet**（CDN <50KB gzip，成熟，够用） |
| CRDT（方向三/白板） | yjs vs automerge vs diamond-types | Rust 兼容性、活跃维护、二进制大小 | **yjs**（6kB gzip，y-protocols 用于 WS sync，活跃维护） |
| 地理索引（方向五） | PostGIS vs h3 vs geohash | PG 集成、半径查询性能、维护复杂度 | **PostGIS**（已经使用 PG 17，GiST 索引就够用了，无需新基础设施） |
| 状态管理（所有方向） | Zustand vs valtio vs vanilla | 与现有零依赖 SPA 的兼容性 | **无**——保持 vanilla ES2020。对于此体量的 SPA，类 Alpine.js 的响应式代理模式无需打包构建即可工作 |

### 自建 vs 采购决策

| 能力 | 自建 | 采购/集成 | 依据 |
|---|---|---|---|
| 白板引擎 | — | **Excalidraw**（CDN iframe） | 从零开始的自定义画布是浪费。iframe 沙箱提供安全边界，且可以安全地无第三方依赖 |
| 音频级混音 | **自建**（新的 `aero-live-mixer` crate） | — | 没有好的开源音频服务器混音器。Janus 太重量级，RFC 中推荐的 WebRTC 混音器是客户端。服务器端混音是特定领域的 |
| 地图瓦片 | — | **MapLibre + OSM/MapTiler** | 不构建地图基础设施。Aero IM 应仅提供 API 以将 lat/lng 附加到流上；客户端渲染的 UI |
| API Playground（API 文档工具） | **自建** | Swagger UI / Stoplight | Swagger/OpenAPI 处理程序无法理解 Aero IM 的 WS 帧类型。需要为 WS 帧构建自定义 Playground |
| 地理编码（反向） | — | **Nominatim**（OSM，免费）或 Google Places API（付费） | 将坐标转换为场所名称是标准 API 调用，不需要自建 |

### Crate 边界与提取建议

新功能应**尊重当前的 crate 图**：

```
方向一（持久空间）→ 新 crate aero-live-voice（而不是在 aero-live-webrtc 内部膨胀 SfuRouter）
方向二（TTL 消息） → 对 aero-im-core 的现有 MessagePolicy 的最小扩展 + aero-storage 中的新清扫模块
方向三（协作）    → aero-server 中的新会议室工具 + 嵌入到网站进程中的前端
方向四（开发者平台）→ 新 crate aero-dev-console（仅 UI + 路由，无业务逻辑）
方向五（地理空间）  → 对 aero-live-core 的扩展 + aero-server 中的新 /explore 路由
```

---

## 5. 实施路线图

### 优先级矩阵（修订版）

| 方向 | 我的优先级 | 文档优先级 | 差异原因 |
|---|---|---|---|
| **媒体 Seam 接线** | **P0** | 未提及（假定已就绪） | 方向一和三的阻塞依赖。这是软依赖的硬架构项 |
| **持久音频空间** | P1 | P1 | 正确——最大的产品差异化和 SFU 投资复用 |
| **开发者平台** | **P1** | **P1** | 一致——对生态锁定的战略重要性 |
| **时效性消息** | P2 | P2 | 合规驱动；不是立即的 DAU 驱动因素 |
| **会议内协作** | P2 | P2 | 前端工作具有对抗性。依赖于白板前端库 |
| **空间感知直播** | P2 | P2 | 地理空间是直播的第 4 个发现维度。PostGIS 很好理解 |

### 阶段划分

**Phase 0（4 周）：媒体基础 + 架构准备**

**与方向并行工作**：
- 将 `sfu_media.rs` 的 `run(cancel)` 从 `#[cfg(test)]` 移至生产连线路径
- 验证两个真实服务器节点之间的跨节点桥接（非 localhost）
- 为持久空间创建 `VoiceSpaceService` trait 和 `voice_spaces` 表
- 添加 `expires_at` + `burn_after_reading` 到 `messages` 表（数据模型变更，未接线清扫）

**里程碑 M0**：SFU 桥接像单个节点一样可靠工作。`voice_spaces` 表创建。`messages` TTL 列已迁移。

**Phase 1（6 周）：持久音频空间 + 开发者平台 Phase A**

**并行**：
| 轨道 A：音频空间 | 轨道 B：开发者平台 |
|---|---|
| `VoiceSpaceService` 实现 | API Playground UI + 只读保护 |
| WS `join_voice` / `leave_voice` 帧 | WS Debug Console UI |
| URL 栏中的语音频道图标 | Webhook Inspector UI |
| 空闲自动停用定时器 | 后端添加 `AeroPlayground(readonly)` 提取器 |
| 每个频道的存在/活动指示器 | |

**里程碑 M1**：用户可以点击语音频道、加入、看到谁在线。开发者可以打开 Playground、尝试端点、查看 Webhook 日志。

**Phase 2（6 周）：时效性消息 + 会议内投票/举手**

**串行**（存在执法交叉）：
- TTL 清扫器与法务保全交叉验证
- 阅后即焚实施，包括多设备已读同步协调
- 会议内投票：`CallEvent::PollOp` → 前端叠加层
- 举手：WS 帧 → 前端
- 共同笔记：将 `canvas.rs` op 日志连接到通话会话

**里程碑 M2**：用户可以发送 5 分钟后消失的消息。会议中投票有效。通话参与者可以举手。

**Phase 3（8 周）：白板 + 地图 + 开发者平台 Phase B**

**依赖**：
- 白板需要 `yjs` 集成和 iframe 沙箱 → 在 Phase 1 的 SPA 架构之上
- 地图需要 PostGIS 扩展(`CREATE EXTENSION postgis`) → PG DBA 操作
- Bot Sandbox 需要 `ephemeral_bots` 表 + 隔离事件通道

**里程碑 M3**：白板在会议中工作。附近直播在地图上显示。开发者可以创建临时 bot。

### 风险矩阵和缓解措施

| 风险 | 可能性 | 影响 | 缓解措施 |
|---|---|---|---|
| 跨节点 Sfu 桥接延迟不可接受 | 中 | 高 | 仅限单节点部署的早期版本。网桥是可选的，而不是必需的 |
| 白板前端集成导致 Web SPA 资产膨胀 | 中 | 中 | Iframe 沙箱 Excalidraw。主 SPA 不增加 5k → 500k 的负债 |
| 阅后即焚与法务保全冲突使合规流程复杂化 | 高 | 高 | 法务保全交互的最早设计文档。在 Phase 2 之前与 legal_holds 模块集成 |
| PostGIS 为 PG 17 增加了运维复杂性 | 低 | 中 | `pgvector` 已经是一个非标准扩展。PostGIS 有完善的文档。备份/恢复处理程序很好理解 |
| 持久空间的移动端音频在 PWA 中不支持（Service Worker 中的 WebRTC 有局限性） | 高 | 中 | Web 是第一个平台。移动端稍后提供本机。Web 的 PWA 音频是「尽力而为」 |
| 开发者 Playground 的写入安全存在绕过漏洞 | 中 | 高 | 中间件强制执行 `AeroPlayground(readonly)`，与常规 authz 分开。此外，在 Phase B 为写操作使用单独的沙箱数据库 |

---

## 总结

本文档中确定的 5 个方向是该产品的有效架构扩展。通过对文档和代码库的分析，我有两个重要的重新评估：

1. **媒体 seam（`call_bridge_supervisor` + `sfu_media_session`）应晋升为独立的 P0 项目**。文档正确地将其标记为「待接线」，但低估了这个问题是方向一和三的阻塞依赖。

2. **方向五（空间感知直播）和方向三（会议内协作）共享相同的「客户端架构升级」瓶颈**。它们都应该作为 Web SPA 能力的阶段来对待，而不是独立的项目——从「零依赖调试客户端」到「基于组件的实时协作客户端」的过渡需要一个有意识的前端架构决策。

Web 客户端架构决策（保持零依赖 vs 引入 yjs + 地图库 vs 使用 iframe 沙箱）**是这里最具影响力的架构选择**。所有五个方向都增加了 SPA 的交互复杂性。如果 Aero IM 要追求其中任何一个方向，Web 架构必须升级——但可以通过廉价的 iframe 沙箱来完成，而无需 rewrite。
