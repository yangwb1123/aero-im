Now I have a thorough understanding of the codebase. Let me compose the architectural analysis.

---

# Aero IM — 架构深度分析

## 1. 架构评估

### 1.1 当前架构的优势

**领域隔离设计良好。** 16 个 crate 按职责自下而上分层，依赖指向明确（common→bus→storage→auth→im-core→server），无循环依赖。每个 crate 对应一个功能域（AI、Push、Signaling、Live 各子协议），遵循了「feature-first 单位是 crate」的项目公约。这是 Rust workspace 组织的最佳实践——编译边界 = 团队协作边界。

**事件总线抽象层稳健。** NATS JetStream 作为跨实例事实源（durable consumer `aero-server`），`Hub` 作为进程内扇出（bounded mpsc），两阶段解码（先 raw JSON lift seq 再 typed RoomEvent）加 at-least-once 语义，设计上平衡了可靠性与性能。per-subject 单调 seq 解决了 NATS 分布式环境下消息排序的经典问题——不依赖 NATS 原生有序性，自己维护 seq，与 JetStream 的 at-least-once 投递正交。

**Repository 模式贯彻一致。** `aero-storage/src/` 下 129 个文件，每个实体一个 Repo struct 包裹 `PgPool`，SQL 不出 crate 边界。这种一以贯之的模式使得新加功能的迁移路径稳定：迁移 → 仓储 → 路由 → 鉴权 → 实时。AGENTS.md §4.1 的「加功能配方」能写出来，本身就说明抽象是成功的——不是每个项目都能总结出可复用的步骤。

**路由注册的可组合性。** `routes.rs::build()` 中 ~100 个 `.merge(crate::xxx::routes())` 调用，每个模块自行声明自己的路由。这是一种轻量级的「按模块注册」模式，不需要微服务化的部署成本，却获得了功能级模块化的好处。缺点是路由注册顺序无关性需要小心（axum 的 merge 语义是后注册覆盖前注册的同路径，但项目中没有冲突路由，工作正常）。

**幂等性和失效模式的深思熟虑。** 从 AGENTS.md 可以看到大量关于幂等键（`ON CONFLICT DO NOTHING`）、fail-open（AI 无 key 退化 HashEmbedder）、budget 闭环（AiWorker per-ws + 全局双预算）、死信队列（`MAX_ATTEMPTS=5→dead`）的设计决策。这不是「先实现后补坑」，而是系统性的事前规划。

### 1.2 架构局限性

**单体路由文件膨胀。** `routes.rs` 2854 行——虽然 AGENTS.md §4.2 允许 routes.rs 到 3000 行，但 2854 已经逼近硬上限。更重要的是，这个文件做了太多事：类型导入、中间件定义、`build()` 函数、辅助函数（`parse_room_id`、`merge_hits`）。`build()` 函数本身虽然是通过 `.merge()` 委派路由注册，但依然显式罗列了所有路由组和顶层路由（auth、me、rooms、messages、streams 等）。当顶层路由也到 50+ 后，`build()` 的**可读性问题**会掩盖其**组合性优势**。

**Web 前端是架构中最大的技术债。** 4703 行纯手工 JavaScript SPA（总共 14 个文件），无构建工具链、无类型系统、无组件模型、无响应式框架。具体表现为：
- 全硬编码中文（UI 字符串散落在 14 个 `.js` 文件中，无 i18n 抽象层）
- 零可访问性标记（`rg 'aria-|role=|tabindex' web/` 仅 HTML 中有少量由 Chrome 自动生成的 role，其余为零）
- 状态管理分散（`app.js` 中的全局变量、DOM 属性、localStorage 混用）
- 无离线支持（manifest 已具备，但无 service worker、无 IndexedDB 缓存策略）
- 无构建优化（无 tree-shaking、无代码分割、无懒加载）

这个前端的技术栈选择（零依赖 ES2020，CDN 加载 hls.js）当初显然是为了快速原型验证服务的后端能力。但当前后端已有 P0–P24 + 87+ 功能点全部就位后，前端已经成为**交付瓶颈**——每个新的后端功能都需要手写 DOM 操作、手动拼接 HTML、手动管理 WS 帧类型映射。

**单租户假设已与功能矩阵矛盾。** 设计 spec 声明「不做多租户/SaaS化（单租户起步）」，但当前代码库已有：
- `rooms.workspace_id NOT NULL`（多租户 schema 骨架已强行到位）
- `WorkspaceRepo::member_role`、`assert_admin`、`can_administer`
- `scim::routes()` 多租户供给
- `ws_rate.rs` 工作区限流档
- `ai_usage.rs` per-workspace AI 配额

**架构已经超越了单租户的设计假设，但部署模型还是单租户的。** 这意味着项目在「单租户起步」和「多租户功能已实现」之间处于尴尬中间态——既有租户隔离的数据库设计，又没有 SaaS 化的租户自服务、注册计量、价格计划。这种中间态如果不尽快收敛到明确的多租户模型，后续每个新功能都需要判断「这条数据是否需要 workspace_id」——架构决策成本会持续积累。

**Live streaming 链路存在「已完成但未接线」的 seam。** AGENTS.md §2 明确指出了 `sfu_media_session` 的 `bind`/`run` 在生产中无实例化（唯一调用在 `#[cfg(test)]`），`call_bridge_supervisor` 的 `ensure_egress` 已建+单测但未接线。这不是问题——项目中 seam 管理是刻意为之的。但风险在于这些 seam 是跨 crate 的（`aero-live-webrtc` 的 `SfuMediaSession` + `aero-server` 的 `call_bridge_supervisor`），如果未来接线时 crate 接口变动，需要同时修改两端的测试代码，耦合度比表面看起来高。

---

## 2. 扩展方向

### 方向一：从单实例到全球部署架构（Multi-Region / 数据主权 / DR）

**为什么需要：** 在文档的 200+ 篇分析里，跨区域/DR 仅出现在零散的一句话中提到，没有架构方案。但当前架构在数据库层已经具备多租户 schema，唯一的物理部署限制是：所有工作在单区域运行。支持欧美/东南亚客户的核心前提是数据主权（GDPR Art.44 合规、中国数据本地化法、巴西 LGPD）。

**核心挑战与技术难点：**

| 难点 | 详细 |
|---|---|
| **数据库复制拓扑** | 当前 157 个迁移线性 apply。切到跨区域 PG 逻辑复制时，迁移的幂等性需要审计（已有 `IF NOT EXISTS` 不是全部覆盖）。破坏性迁移（`ALTER TABLE DROP COLUMN`、数据重分区）的逻辑复制行为不可预测——目标区域可能处于中间迁移状态。 |
| **seq 在跨区域下的唯一性** | 当前 `bus/seq.rs` 的 per-subject seq 是单进程 Redis 或 PG 序列生成的。跨区域下，纬度 A 和纬度 B 各自产生的事件序列号可能冲突或乱序。需要引入**区域前缀**（`eu:seq`、`us:seq`）或**混合逻辑时钟**（HLC）来保证跨区域 seq 全局唯一。 |
| **NATS 跨区域桥接** | 当前 NATS 是单集群。跨区域需要 NATS 超级集群（Super Cluster）或 Leaf Node 模式，或者引入 Kafka MirrorMaker 式的主动复制。前者受 NATS 内置能力限制（NATS 跨集群复制在 2026 年仍不如 Kafka 成熟），后者是自定义工程。 |
| **用户路由（sticky routing）** | 每个用户需要被路由到最近的区域，但跨区域操作（如跨区域发消息、跨区域通话）需要区域协调。当前 `call_route/stream_route heartbeat` 的 Redis 心跳机制不足以做区域级路由决策——需要引入区域拓扑发现和跨区域负载均衡。 |
| **媒体面跨区域延迟** | WebRTC 通话和 WHIP/WHEP 直播的 RTP 流对延迟极度敏感。跨区域 SFU 中继（当前 `call_bridge_supervisor` 的 `ensure_egress`）会引入 100-300ms 额外延迟，对通话质量的影响需要评估。 |

**预期的架构变更：**
- 引入**区域抽象层**：每个 crate 的基础设施连接（PG、Redis、NATS、S3）需要带上区域标签
- `aero-bus` 需要支持多区域事件复制（NATS Leaf Node 或自定义 topic → topic 复制）
- `aero-common` 中增加区域 ID 类型和拓扑数据结构
- 部署模型从「单 docker-compose」变为「区域级 Helm chart + 区域间网络策略」

**对现有系统的影响：** 中高。现有代码假定基础设施是单实例或单集群。引入区域概念会穿透几乎所有 crate（每个 crate 都与某种基础设施有关）。建议用 feature gate 控制——`--features multi-region` 开启跨区域拓扑，单区域模式行为不变。

---

### 方向二：Web 前端工业化（PWA / a11y / i18n / 可维护性）

**为什么需要：** 这是当前架构最大的交付瓶颈。4703 行无类型 JavaScript 的管理成本随着功能数量线性增长，无障碍合规缺失会使欧美政府/教育合同流标，全中文硬编码使非中文用户无法使用，无离线支持意味着移动端弱网场景用户体验断崖。

**核心挑战与技术难点：**

| 难点 | 详细 |
|---|---|
| **迁移路径不是全量重写** | 全部 14 个 `.js` 文件 + `index.html` 的 DOM 结构都需要穿透。不能「框架化后新旧并存三个月」——SPA 是全有或全无的重构。建议的迁移策略是：保留当前 SPA 作为「legacy 客户端」用于功能验证，同时在新目录 `web-next/` 中构建新前端，两套并行直到新前端达到功能覆盖。 |
| **无障碍是穿透性改造** | 当前 UI 大量依赖 `hidden` 属性控制视图切换（`<section id="view-auth" class="view view-auth" hidden>`）。加 `role`、`aria-*`、键盘导航需要理解每个视图的语义角色——这不是机械添加属性，而是每个视图都需要重新设计其无障碍模型。`render.js`（685 行）的 `renderMessage` 等函数生成的 DOM 结构都需要加语义标记。 |
| **i18n 工具链空白** | 当前零 i18n 基础设施。字符串散落在 14 个文件中，纯 JS 模板字符串（`el.textContent = \`你好 ${name}\``）。抽字符串需要：a) 定义 key + 默认值映射；b) 修改所有渲染路径使用 `t()` 函数；c) 维护翻译文件。三个步骤都与其他重构（无障碍、组件化）有交叉依赖。 |
| **Service Worker 的离线消息策略** | 不是「缓几个静态文件」——IM 的离线需要：a) WS 断线重连+消息队列；b) IndexedDB 本地消息存储+schema 设计；c) 冲突解决（离线发的消息在线上了怎么办）。这比普通 PWA 复杂一个数量级。 |

**预期的架构变更：**
- 新前端 `web-next/` 作为独立 workspace（可考虑 Vite + TypeScript + React/Vue，团队熟悉即可）
- 后端增补 `manifest.json` 中的 scope 配置、service worker 注册端点
- 后端增补 `/api/i18n/translations`（或直接前端构建时打包）的 i18n 资源端点
- `ws.js` 的 WS 连接逻辑需要同步到新前端（当前 WS 帧类型映射是硬编码 `switch case`，可以提取为共享的帧类型定义，也许放到 `aero-common` 中作为 Rust→JS 的类型参考）

**对现有系统的影响：** 低（后端几乎不变）。主要是前端自身的架构重组。风险在「并行维护两套前端」的人力成本。

---

### 方向三：开放平台——Bot API / App Marketplace / Slash Commands Extensibility

**为什么需要：** 当前 bot 系统全部是内置 bot（agent_bot、ooo_bot、unfurl_bot、transcribe_bot 等），通过 NATS durable consumer 接收事件。对外的 bot 集成能力为零：没有 incoming webhook、没有事件投递、没有 slash command 注册。企业客户的长尾集成需求（GitHub 通知、Jenkins 构建、PagerDuty 告警、Salesforce 同步）只能通过内置 bot 实现，无法扩展。

**核心挑战与技术难点：**

| 难点 | 详细 |
|---|---|
| **事件投递的可靠性与安全性** | 外部 bot 需要 at-least-once 事件投递，但需要防重放、防死循环（bot 发消息触事件→事件触发 bot→bot 再发消息…）。当前 internal bot 使用同一 NATS durable consumer `aero-bot` 消费 `im.room.*`，对外 bot 需要独立的 consumer group 和 consumer 生命周期管理。AGENTS.md 中提到的「3 层深度限制」是很好的基线，但对外 API 还需要 bot 级别的幂等键。 |
| **鉴权模型** | 当前 `AuthUser` extractor 接受 JWT 或 PAT。外部 bot 需要自己的鉴权（bot token），并有粒度控制——bot 可以订阅哪些事件（message、reaction、member_join 等）？可以读写哪些房间？这需要引入 `BotScope` 权限模型，类似于 Slack 的 Bot Token Scopes。 |
| **Slash Command 注册** | 当前 `/commands.rs` 中 `/me`、`/shrug`、`/giphy`、`/remind` 全部硬编码。对外开放 slash command 需要：a) 运行时注册 API；b) 命令参数校验 + 自动补全；c) bot 的响应时限（Slack 要求在 3 秒内响应，否则需要 deferred response）。 |
| **App Marketplace 不是纯工程问题** | 技术上可以搭建：开发者上传 manifest → 系统签发 bot token → 管理员安装。但 marketplace 的核心是 a) 审核流程；b) 计费集成；c) OAuth 授权流（用户安装时 app 请求权限）。工程实现只是其中一部分，产品设计和运营策略才是决定因素。 |

**预期的架构变更：**
- 新增 `aero-bot-sdk` 或 `aero-app` crate，定义 `BotApp` trait（`on_event`、`on_command`、`on_interaction`）
- `bot_dispatch.rs` 扩展为从 NATS consumer 路由到外部 webhook 目标（目前已有 webhook 出站发送器，需要复用）
- 新增 `bot_event_subscriptions` 表（AGENTS.md 提到「清理专用 bot」，说明部分代码已就绪）
- 路由新增 `/api/apps/*`（App manifest CRUD、安装/卸载管理）
- `aero-auth` 中增加 `BotToken` extractor（与 `AuthUser` 类似但携带 scope）

**对现有系统的影响：** 中。新增 crate 和表不影响现有路由。但需要仔细设计不破坏已有的 internal bot 模式——internal bot 继续用 NATS consumer，external bot 走 webhook。两者可能共享同一事件路由逻辑（`bot_dispatch.rs` 需要同时处理两种输出）。

---

### 方向四：合规工程——SIEM 集成 / EDiscovery / 不可篡改日志

**为什么需要：** 当前合规能力只有法务保全（`legal_holds.rs`，跳过清扫）、数据导出（`me_export.rs`，个人数据导出）和基础审计（`audit.rs`，审计事件表）。金融/医疗客户准入需要：a) 跨工作区电子证据开示（EDiscovery）；b) 不可篡改审计日志（hash chain + 防删除）；c) SIEM 集成（Splunk/CloudWatch 事件导出）；d) DLP（数据防泄漏的出境检测）。

**核心挑战与技术难点：**

| 难点 | 详细 |
|---|---|
| **不可篡改日志的哈希链** | `audit_events` 表已有行需要回溯填 hash（一次性迁移可能很慢），且需要设计一个在 PG 上高效实现的哈希链——每行的 `prev_hash` + `row_hash`，计算 `row_hash = hash(prev_hash || event_data)`。问题：更新和物理删除必须禁止（可能需要 `REVOKE DELETE` + PG trigger 拦截 `UPDATE`），这影响现有的审计数据管理流程。 |
| **跨工作区 EDiscovery 的数据隔离** | 管理员搜索全部工作区的消息——这直接与当前的使用者数据隔离模型冲突。当前每个搜索都通过 `assert_room_access` 确保只返回参与者有权限的房间。EDiscovery 需要一个新的「超级管理员角色」或「eDiscovery 专员角色」，其搜索不受常规的 $membership$ 约束，但所有搜索操作都被审计。 |
| **SIEM 事件导出的格式与可靠性** | SIEM 集成需要：a) 事件格式化（CEF/LEEF/JSON 各厂商格式不同）；b) 可靠投递（at-least-once 到 Splunk HEC/CloudWatch Logs）；c) 失败重试和积压处理。当前 `webhook.rs` 有 outbound webhook 发送器，可以复用其退避逻辑，但 SIEM 的格式要求不同于 webhook 的 JSON POST。 |
| **DLP 扫描的实时性** | 消息发出前检测敏感数据（信用卡号、社会安全号码、API 密钥等）要求低延迟（<200ms），但又需要正则/ML 模型匹配。当前 `KeywordModerator` 是同步的（`AERO_BLOCKED_WORDS`），走的是关键词匹配。DLP 需要更复杂的模式匹配，可能需要引入异步扫描（类似 `AERO_AI_MODERATION` 的预算队列）但又要满足近实时体验。 |

**预期的架构变更：**
- `audit_events` 表新增 `prev_hash` 和 `row_hash` 列，新增 PG trigger 或应用层强制 hash chain
- 新增 `aero-compliance` crate 或扩展现有 crate，包含：`EDiscoveryRepo`（跨工作区搜索，带 audit trail）、`SiemExporter`（事件订阅 + 格式化 + 投递）、`DlpEngine`（模式匹配 + 异步扫描）
- 路由新增 `/api/admin/ediscovery/*`、`/api/admin/audit-log/*`、`/api/admin/dlp/*`
- `aero-auth` 新增 `AdminRole` extractor（与 `AuthUser` 解耦，因为管理员角色不同于 participant role）
- `Hub`/`run_bus_listener` 中增加 SIEM 事件导出 hook（复用 `metrics::MESSAGES_SENT_TOTAL` 的计数路径旁路）

**对现有系统的影响：** 中高。不可篡改日志改现有 `audit_events` 的写入路径，需要保证现有功能不受影响（难在「已有行回溯填 hash」的一步性迁移——可能需要 downtime 或后台批处理）。DLP 扫描如果要求同步拦截（拒绝发消息），则需要改 `ImService::publish_room_event` 的写入路径——当前审核是异步的（软删）。

---

### 方向五：平台 SRE——容量规划 / 负载测试 / 成本归因

**为什么需要：** 当前可观测性已有 `metrics.rs`（`MESSAGES_SENT_TOTAL`、`WS_CONNECTIONS` 等指标）+ OTLP 链路追踪。但缺少：a) 容量边界定义（「能支持多少人同时在线？单房间多少人？」）；b) 负载测试套件（没有 k6/Locust 脚本）；c) 成本归因（每个工作区消耗多少 AI tokens、多少存储、多少带宽）；d) 自动扩缩策略（当前无 HPA 配置）。没有这些数据，客户问「你们支持 10 万人同时观看直播吗？」的回答只能是「也许」。

**核心挑战与技术难点：**

| 难点 | 详细 |
|---|---|
| **SFU 不可无状态扩缩** | 当前 SFU（`aero-live-webrtc` 的 `SfuRouter`）是进程内 `Arc<RwLock<HashMap>>`，媒体会话绑定到特定进程。这与 Kubernetes HPA 的无状态扩缩模型矛盾——新 pod 不会自动接管已有媒体流。需要要么 a) 引入媒体网关层（如 LiveKit 的每个房间 sticky 到一个 node），要么 b) 实现 SFU 会话迁移（极其复杂，需要 ICE restart）。 |
| **AI 成本归因的跨维度追踪** | `ai_usage.rs` 的 `CostBudget` 已经实现了 per-workspace AI 配额追踪和 `usage_ledger` drain。但要回答「哪个工作区、哪个用户、调用了什么 AI 能力、花了多少成本」需要更细的维度：`(workspace_id, user_id, ai_kind, model, tokens)`。数据量可能很大（每个 AI 调用一行记录），需要单独的时间序列表，不与操作表混合。 |
| **负载测试中模拟真实协议** | 不是简单 POST 一个消息——需要模拟 WebSocket 登录、房间加入、实时消息、WebRTC 信令、SFU 媒体投递。每个阶段都有握手和状态依赖。用 k6 可以模拟 HTTP+WS，但 WebRTC/SFU 需要浏览器或独立客户端库（如 str0m）参与。 |

**预期的架构变更：**
- `aero-server` 新增 `/debug/pprof`、`/debug/vars` 调试端点（`cfg(debug_assertions)` 门控）
- `ai_usage.rs`/`metrics.rs` 新增成本归因维度的指标（`AI_COST_BY_WORKSPACE` 等）
- 新增 `scripts/loadtest/` 目录，包含 k6 脚本 + docker-compose 负载测试环境
- 新增 `/api/admin/usage-report` 端点（`usage_report.rs` 已存在，可能需要扩展维度）
- 部署配置新增 HPA 示例 YAML，但标注 `# SFU 需要 pod anti-affinity` 的约束

**对现有系统的影响：** 低。主要是增补而非改造。风险在「获取了指标但没有行动」——指标的目的是指导决策，不是积累数据。

---

## 3. 接口设计建议

### 3.1 关键模块的接口设计原则

**Bus 层接口（`aero-bus`）：当前设计是合理的，但跨区域场景需要扩展。**

```rust
// 当前——单区域
pub trait EventBus {
    async fn publish(&self, subject: &str, payload: &[u8]) -> Result<()>;
    async fn subscribe(&self, subject: &str) -> Result<Box<dyn Subscription>>;
}

// 跨区域扩展建议：
pub trait EventBus: Send + Sync {
    async fn publish(&self, subject: &str, payload: &[u8]) -> Result<()>;
    async fn publish_with_region(
        &self, subject: &str, payload: &[u8], region: RegionId,
    ) -> Result<()>;  // 默认实现调用 publish
    async fn subscribe(&self, subject: &str) -> Result<Box<dyn Subscription>>;
    /// 返回当前 bus 连接的区域（单区域时返回本地区域 ID）
    fn region(&self) -> RegionId;
}
```

这种设计的权衡：trait 增加一个方法，默认实现向后兼容，但跨区域场景的实现者必须处理区域路由。

**Repo 模式：当前每个 `XRepo::new(pg)` 在路由函数内 inline new——这在 129 个 repo 的规模下是合理的。** 不需要引入 DI 容器。

**接口向后兼容的两个原则：**
1. **枚举（RoomEvent、StreamEvent、ServerFrame）新增 variant 永远兼容。** 序列化框架（serde + tag="kind"）确保旧客户端反序列化新 variant 时会跳过（`#[serde(deny_unknown_fields)]` 缺失时是兼容的）。建议明确保持 `deny_unknown_fields` 不开启——当前没有开启的。
2. **路由 URL 永不删除，只废弃（deprecate）。** 当前 `/api/rooms/:id/messages` 和 `/api/messages/:id` 共存。如果未来重构路由，旧路径应保持至少一个大版本周期。

### 3.2 是否需要引入新的抽象层

**建议引入两个新的抽象层：**

**A) ClusterState 抽象层（当前散布在多个 repo 中）**

当前集群级状态分布在 Redis sorted-set（presence、StreamViewerStore、CallRosterStore）和 PG（rooms、membership）中。但没有统一的「集群状态」抽象——一个路由函数需要知道哪些状态在 Redis、哪些在 PG、哪些在进程内存。

建议引入 `ClusterState` trait（或 struct），将「需要跨实例一致的状态」操作集中化：

```rust
// 当前散落:
let presence = presence::get_online_participants(&redis, room_id).await?;
let room = room_repo.get_room(pg, room_id).await?;
let roster = call_roster_store.get(redis, call_id).await?;

// 建议集中:
let state = ClusterState::new(redis.clone(), pg.clone());
let online = state.presence().online_in(room_id).await?;
let room = state.rooms().get(room_id).await?;
let call = state.calls().roster(call_id).await?;
```

权衡：这种集中化增加了代码组织的清晰度，但引入了一个新层——团队需要学习 `ClusterState` 而非直接调用 `call_roster_store`。收益在大规模功能增补时更明显（新功能的开发者不需要思考「这个状态放哪里」）。

**B) Auditor 抽象层（合规场景前置）**

当前审计日志散布在个别 `audit.rs` 方法中。合规场景（方向四）需要一个集中的 `Auditor` trait：

```rust
#[async_trait]
pub trait Auditor: Send + Sync {
    /// 记录一个可审计事件。事件被序列化为 JSON 并持久化。
    /// `hash_chain` 为 true 时自动计算并存储 hash chain。
    async fn record(&self, event: AuditableEvent) -> Result<AuditId>;
    /// 按条件查询审计日志（EDiscovery）。
    async fn query(&self, filter: AuditFilter) -> Result<Vec<AuditEvent>>;
    /// 验证审计日志从 `from_id` 到 `to_id` 的哈希链完整性。
    async fn verify_chain(&self, from_id: AuditId, to_id: AuditId) -> Result<bool>;
}
```

这种设计的理由是「审计不能是事后加的」——所有修改操作都应该在一个地方被记录，而不是在每个 handler 末尾自己调用 `audit::record_event`。建议作为中间件注入：写操作完成后自动 dump 请求 + 响应摘要到 auditor。

### 3.3 Web 前端的接口契约化

当前后端→前端的契约隐含在 `ws.js` 的 `switch(event.type)` 和 `render.js` 的 DOM 操作中。没有一个显式的契约文档或类型定义。

建议（不引入新框架的情况下）：
1. 在后端 `aero-common` 中增加 `web_api.rs`，用 Rust 的 `#[derive(Serialize)]` struct 定义所有 WebSocket 帧的 JSON schema（当前已有 `ServerFrame`/`ClientFrame` 枚举，但定义散布在 `ws/ws_impl/` 中）
2. 用 `schemars` crate 为这些 struct 生成 JSON Schema
3. 在 `web/` 中引入 TypeScript 类型定义（通过 `openapi.rs` 的 OpenAPI spec + 手动提取的 WS 帧类型）

这种方法不要求前端用 TypeScript，但提供一个可参考的「官方类型定义」，减少 WS 帧格式不匹配的 bug。

---

## 4. 技术选型

### 4.1 是否需要引入新的技术栈或框架

**对于后端（Rust）：不需要引入新框架。** 当前技术栈（axum + sqlx + fred + async-nats + str0m）在后端开发中都是成熟且活跃的选择。没有明显的缺口需要框架替换。

**对于前端：需要认真考虑引入一个框架/SDK 团队。**

这是一个艰难的决策——当前 SPA 的工作方式是「零依赖、纯 ES2020、CDN 加载」。其优势是：
- 无构建步骤（`web/` 目前是服务器静态文件，直接服务）
- 零构建工具链的维护成本
- 原型迭代速度快
- 部署简单（`cp web/ dist/`）

但劣势随着功能数量增长越来越明显：
- 无类型检查（`undefined is not a function` 是运行时常客）
- 无组件复用（消息渲染、用户头像、房间列表——每个都需要单独写 DOM）
- 无状态管理（app.js 中的全局变量列表在增长）
- 无构建优化（14 个 JS 文件全部独立加载，无 tree-shaking 无懒加载）

**建议：** 不强制框架切换，而是提供两条路径并行：

| 路径 | 适用场景 | 技术栈建议 |
|---|---|---|
| 保持当前 SPA | 快速验证新后端功能、内部工具、调试客户端 | 继续纯 ES2020，但引入 eslint 全面检查 + JSDoc 类型标注 |
| 新前端 `web-next/` | 面向最终用户的生产前端 | Vite + TypeScript + Preact（与 React 类似的 API，但 3KB，是零框架到全量 React 的平滑中间点） |

**不建议直接从零手写 JS 跳到 React + Redux + TypeScript 全量框架**——这会让团队同时适应 10 个新概念（React hooks、JSX、TypeScript、ES modules 在构建工具中的行为差异、状态管理、路由、CSS-in-JS）。Preact 的 API 面积极小（与 React 兼容的 hooks + JSX），迁移路径也更渐进——可以先从 `render.js` 中的一个 widget 开始重写，逐步扩展。

### 4.2 第三方依赖的评估标准

当前 workspace 依赖列表有 ~40 个外部依赖。对于新依赖的引入，建议遵循以下标准：

1. **编译时验证：** 必须是 `#[forbid(unsafe_code)]` 兼容的（项目级 lint 已禁用 `unsafe`）。对于 str0m 这种使用了 `unsafe` 的依赖（str0m 的 SIMD 优化），需要在 Cargo.toml 的注释中声明 unsafe 的用途和 scope。
2. **依赖树的增量成本：** 新依赖带来的传递依赖数不超过当前 `Cargo.lock` 的 5%（当前 ~1400 个 crate 在 lock 中）。
3. **Rust 版本兼容性：** MSRV 不超过 1.80（当前 toolchain 版本）。
4. **社区健康状况：** GitHub stars >500 且最近一次发布在 12 个月内（对于核心依赖）。

**几个具体评估：**

| 候选 | 用途 | 建议 | 理由 |
|---|---|---|---|
| `kafka` (rdkafka/rust-rdkafka) | 跨区域事件复制 | **不推荐** | 当前 NATS 体系已建立，再加 Kafka 是增加运维复杂度。NATS Leaf Node 或自定义 topic 复制更合适。 |
| `quick-xml` / `serde-xml-rs` | SAML XML 解析 | **推荐** | 当前 `saml.rs` 已引用但没有显式依赖声明（可能通过 transitive deps）。SAML 签名验证需要 XML 解析，应该显式依赖。 |
| `schemars` | Rust→JSON Schema 生成 | **推荐** | 用于前端契约文档化和潜在的类型生成 |
| `typst` | 合规报告 PDF 导出 | **观望** | typst 功能强大但二进制不包含在 crate 中，部署时需要单独安装 typst CLI。建议先输出 HTML→wkhtmltopdf |
| `k6` | 负载测试 | **推荐**（非 Rust，但作为测试工具） | 应加入 `scripts/loadtest/` |
| `arboard` | 剪贴板（桌面端） | **不需要** | 当前 web-only 场景不需要 |

### 4.3 自建 vs 采购的决策依据

| 场景 | 建议 | 理由 |
|---|---|---|
| **跨区域数据库同步** | 自建（基于 PG 逻辑复制 + 自定义冲突解决） | PG 原生逻辑复制成熟，需要自定义的是 seq 冲突解决和区域故障转移策略——这部分有明确的业务语义（每个区域的 seq 前缀），没有通用产品 |
| **SIEM 集成** | **采购**（Splunk/CloudWatch/Datadog 的既存连接器） | 每个 SIEM 厂商都有自己的事件格式和投递协议，自建维护成本高。建议用现有 webhook 出站逻辑作为基础，适配不同的 SIEM 目标。如果客户数量少，甚至可以要求客户自行配置 SIEM 对 http 端点的抓取 |
| **DLP 引擎** | 自建（关键词 + 正则 + 可选第三方 API） | 关键词审核已实现（`AERO_BLOCKED_WORDS`），正则扩展成本低。复杂的 PII 检测可以后续集成第三方 API（如 Microsoft Purview / Google DLP API） |
| **EDiscovery 界面** | 自建 | EDiscovery 本质上是搜索 + 导出 + 审计轨迹。搜索已有（`search_advanced.rs` 的操作符支持），导出已有（`conversation_export.rs`），缺的是管理员 UI |
| **移动端 SDK** | **推迟决策** | 当前项目公约明确「Web 优先，移动端后续」。在 Web 生产化尚未完成前启动移动端 SDK 开发，是同时管理两套 UI 平台的噩梦 |

---

## 5. 实施路线图

### 优先级排序

| 优先级 | 方向 | 短期 ROI | 长期影响力 | 依赖 |
|---|---|---|---|---|
| **P0** | 方向二·Web 前端工业化 | 高（用户痛点最直接） | 中（但后续所有方向的 UI 都依赖此基础） | 独立 |
| **P1** | 方向三·开放平台 | 中（企业集成需求明确） | 高（生态网络效应） | 方向二的部分完成（开发者 portal UI） |
| **P1** | 方向五·SRE 基础 | 高（「能支持多少人」是客户必问） | 中（可观测本身不是产品特性，但客户准入要求） | 独立 |
| **P2** | 方向一·全球部署 | 低（无明确区域客户时不紧急） | 高（数据主权合规是扩展欧美市场的硬前提） | 方向五（需要负载测试数据做容量规划） |
| **P2** | 方向四·合规工程 | 中（金融/医疗客户准入要求明确） | 中（合规是进入特定市场的门票） | 方向二的搜索 UI 增强 |

### 阶段划分与里程碑

**阶段一（1-2 月）：Web 工业化起步 + SRE 基础**

目标：将 Web 前端的技术债降低到可维护的范围内，同时建立容量基线。

- 引入 eslint + JSDoc 全面覆盖现有 14 个 JS 文件
- 创建 `web-next/` 目录，用 Vite + Preact + TypeScript 搭建骨架
- 实现一个完整的功能模块（如 poll UI `polls.js` → 新前端的 Poll 组件）作为 PoC
- 编写 loadtest 脚本（k6）：1 个房间 100 人并发消息、1 个直播 1000 人 watcher
- 生成容量报告（瓶颈在 PG / NATS / Hub bounded mpsc？）

**里程碑 M1：** 新前端 PoC 可运行一个功能（polls）并切换到新前端不破坏旧功能。容量报告给出「10000 同时在线的瓶颈在 X」。

**阶段二（2-4 月）：开放平台 + 合规基础**

目标：企业客户可以集成自己的 bot，管理员可以进行基础 EDiscovery。

- 定义 `BotScope` 权限模型，扩展 `aero-auth` 支持 bot token（extractor + scope 校验）
- `bot_dispatch.rs` 扩展支持 webhook 出站事件（复用 `webhook.rs` 的 sender + retry + DLQ）
- 新增 `POST /api/bots/:id/subscriptions`（已存在于 routes.rs 但可能功能不完整）
- 审计日志 `hash chain` 迁移 + 新增 `Auditor` 中间件
- 新增 `/api/admin/ediscovery/search`（跨工作区搜索，`AdminRole` 门控）

**里程碑 M2：** 外部 bot 可以注册、订阅事件、接收 webhook POST。管理员可以跨工作区搜索消息并导出。

**阶段三（4-8 月）：全球部署 + 全合规**

目标：支持多区域部署架构，完成合规工程全链路。

- `aero-bus` 扩展支持区域感知（NATS Leaf Node 或 topic 复制）
- 引入 `RegionId`、区域拓扑发现、用户区域路由
- 数据库复制策略：PG 逻辑复制 + 自定义 seq 冲突解决（区域前缀）
- EDiscovery 完成 + SIEM 集成
- DLP 引擎（关键词 + 正则 + 可选第三方 API 已对接）
- 前端 `web-next/` 达到功能覆盖（与 legacy SPA 并行运行，可选切换）

**里程碑 M3：** 可在两个区域部署实例，数据按区域隔离，客户可选择数据存储区域。管理员完整合规面板可用。

### 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| **Web 重构中途失速** | 中 | 高（新旧两套前端都需要维护，人力摊薄） | 严格定义「PoC 功能」范围（不要试图一次性迁移全部功能）；新前端只覆盖高频功能（消息收发 + 搜索 + 通知），低频功能（投票、审批、直播设定、SCIM 管理等）继续用旧前端 |
| **跨区域 seq 冲突设计错误** | 低 | 高（跨区域消息乱序或丢失） | 早期原型验证：用 docker-compose 模拟两区域，确认 seq 生成 + 冲突解决逻辑正确；先做区域前缀做 seq 命名空间（`{region}:{seq}`），再考虑 HLC |
| **外部 bot 爆炸导致 DoS 风险** | 中 | 高（bot 循环发消息导致系统过载） | 方向三的「3 层深度限制」+ per-bot rate limit + bot 行为审计 + 超时后自动 suspend。在路线图中优先级排在 bot API 发布之前 |
| **合规的 hash chain 迁移导致长时间停机** | 低 | 高（已有 ~157 个迁移的 schema，再跑一级全表扫描填 hash） | 后台批处理实施：逐个表、分批（`UPDATE ... WHERE hash IS NULL LIMIT 10000`）、可暂停/可恢复、在低峰期运行。不停机。 |
| **SFU 扩缩限制影响直播容量** | 中 | 中（大主播开播时 SFU 节点过载） | 方向五的容量测试先测出 SFU 单节点瓶颈；部署时用 pod anti-affinity + over-provisioning 策略；如果瓶颈明确，再考虑媒体网关层 |
| **团队缺乏跨区域基础设施经验** | 中 | 中 | 方向一作为 P2（非紧急），留出足够的前期研究时间；先以「跨区域只做数据库只读副本」为起点，逐步推进到读写分离和区域自治 |

---

## 总结

Aero IM 的后端架构在 Rust 生态中达到了相当高的工程水准：清晰的 crate 边界、一致的事件驱动模型、成熟的 Repo 模式、系统性的事前可靠性设计。但前端架构需要优先工业化，否则将成为后续所有方向（开放平台、合规工程、全球部署）的交付瓶颈。

五个方向中，P0 是 Web 前端工业化（用户痛点和交付瓶颈的交叉点），P1 是开放平台 + SRE 基础（企业客户的硬需求），P2 是全球部署 + 合规工程（市场扩展的长期投资）。关键建议是：优先交付新前端的 PoC（单个功能模块）来验证迁移策略，避免全量重写带来的失速风险；同时在方向五中先获取容量数据来为方向一的全球部署决策提供定量依据。
