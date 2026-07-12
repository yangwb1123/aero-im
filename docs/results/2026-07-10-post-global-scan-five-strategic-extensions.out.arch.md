# 架构分析：Aero IM 平台演进评估

> 基于上述修正与补充观察，从架构师视角对 Aero IM 的当前状态、演进方向和风险进行系统分析。

---

## 1. 架构评估

### 1.1 当前架构的优势

| 维度 | 评价 | 依据 |
|------|------|------|
| **事件驱动骨架** | ✅ 成熟 | NATS JetStream 的 durable/ephemeral consumer 区分充分体现了 at-least-once 与 best-effort 的业务语义差异 |
| **进程内扇出** | ✅ 高性价比 | Hub::fan_out_raw 用 bounded mpsc 避免背压传染，不依赖 Kafka/Redis PubSub 做扇出——架构轻但够用 |
| **crate 层级** | ✅ 清晰 | 自下而上、无环、每位 crate 有明确职责边界。AI 的 Embed/Summarize/Moderate/Answer 同属 aero-ai 但内部隔离 |
| **状态放置** | ✅ 正确 | 集群状态在 Redis sorted-set，非进程内存；本地状态（SfuRouter、Hub 名册）仅用于自身扇出 |
| **幂等设计** | ✅ 多处嵌入 | ooo_bot 的 ON CONFLICT DO NOTHING、AiWorker 的 SKIP LOCKED + dead-letter 机制 |

### 1.2 关键架构债务

以下债务按修复成本 × 影响面排序：

#### 债务一：零依赖 SPA 约束成为瓶颈（P0）

当前 ES2020 无构建 SPA 是一个**有意的极简设计决策**，但已被证伪——项目当前真实情况：

- 已经有 5+ `.js` 文件（`render.js`、`livecards.js`、`polls.js`、`push.js`、`api.js`）需要模块间协调
- 交互式 Block（Button/Select）的 `submitBlockInteraction` 在 `livecards.js` 但被架构分析遗漏——**因为无模块系统，文件间的依赖全靠人工维护**
- 多文件时类型安全完全缺失

**修复成本**：引入 Vite/Typescript 需要约 2-3 天的构建脚本 + 迁移 + CI 调整。但零碎 UI 的缺失（虚拟列表、i18n）正是因缺乏工程化平台导致——**先修债务才有能力加功能**。

#### 债务二：bot 平台 API 存在但无开发者体验（P1）

AGENTS.md §4.2 说 token helper 同名不可在 crate root re-export——这已经是团队内部工程规范。但面向第三方开发者的规范完全空白：

- 没有签名验签 SDK（HMAC signing = 0）
- 没有 rate limit headers 的文档化契约
- 没有沙箱/测试 token 环境

**后果**：即使 `/api/bots` 有完整 CRUD，第三方开发者需要自己处理所有鉴权细节——降低了方向③的可行度。

#### 债务三：公网 Webhook Receiver 模式为 0（P1）

当前所有入口是 Axum 网关（HTTP/WS/WHEP/RTMP），但**没有一个「接收外部异步回调」的模式**：

- Reply-by-Email 需要 `POST /api/email/inbound` ——这是外部 SMTP 服务器 POST 到你的公网端点
- Incoming webhook（Slack/GitHub 等）需要公网可访问的 receiver
- 当前 IP allowlist 中间件（`ip_allowlist::enforce_layer`）是整个项目唯一的外部安全层，但它是**管理端白名单**，不是**公网 receiver 的认证层**

**这不是一个路由问题，是一个模式问题**：公网 receiver 需要独立的安全模型（HMAC 校验 + 签名验证 + IP 白名单）、独立的 rate limit 策略（免受信源爆表）、独立的 consumer 分组。应该在项目内建立 `receiver/` 子模块或独立 crate。

#### 债务四：跨区域基础设施约束未文档化（P2）

- NATS JetStream 不支持跨区域——桥只能做异步、非严格顺序的消息泵
- 房间内消息顺序依赖 per-subject seq——跨区域桥必须能在 subject 级别保持单调性，但两个 NATS 集群各自 mint seq，桥接时的 seq 冲突或乱序目前无解
- Call-bridge 的 `TODO(real-transport)` 标记意味着单区域跨节点还没跑通

**这不是 bug，是无文档化的架构边界**。当前产品文档里没有明确告诉运营团队：「多区域部署时 IM 消息可能有 0.1% 乱序率，实时通话不可用」。

### 1.3 设计决策复盘

| 决策 | 当初正确否 | 今天应否重新评估 |
|------|-----------|----------------|
| **零依赖 SPA** | 正确（MVP 阶段避免 JS 工具链） | **需要重新评估**——用户量增长后 UI 工程化缺失导致迭代效率下降 |
| **NATS 做事实源** | 正确（轻量、纯 Rust、JetStream 支持 durable） | **依然正确**——但跨区域场景需要补充设计 |
| **str0m 纯 Rust DTLS-SRTP** | 高风险但合理的战略选择 | **需要验收**——`sfu_media.rs::run()` 生产未接线，需要评估是否继续投资 vs 换 mediasoup |
| **Redis sorted-set 做集群状态** | 正确（P99 < 5ms） | **依然正确**——presence/roster/viewer count 不需要强一致性 |

---

## 2. 扩展方向

### 方向 A：开发者平台（Bot SDK + Webhook 签名 + 开发者门户）

**为什么需要**：方向③（Bot/集成平台）的缺口本质不是 API CRUD，而是**开发者信任**。没有签名验证的 webhook 不可被生产使用，没有 SDK 的 API 是半成品。

**核心挑战**：
1. 签名算法选择：HMAC-SHA256 vs Ed25519 vs JWS。——推荐 HMAC-SHA256（最常见、库零依赖），允许开发者用 openssl CLI 或任何 HTTP 库验签
2. 密钥轮换：bot rotate token 已有（`POST /api/bots/:id/token`），但旧 token 需 grace period
3. Rate limit headers 标准化：当前 `X-RateLimit-*` + `Retry-After` 对 WebSocket 路径不适用

**架构变更**：
- `aero-storage/src/webhook.rs` 新增 `sign_payload(secret, body) -> String` 和 `verify_signature`
- `aero-server` 新增 bot-specific rate limiter（与 WS rate limit 独立）
- 可选：`aero-bot-sdk` crate（Rust SDK）+ 开发者文档站点

**影响面**：
| 组件 | 影响 |
|------|------|
| `webhook.rs` | 增 2 方法（sign/verify），零现有代码改动 |
| 路由层 | 增 webhook delivery 的 `X-Aero-Signature` header |
| 外部 | 无——不改变现有 WS/HTTP 契约 |

**工作量估计**：签名 ~1 天，SDK 文档 ~2 天，Rate limit header 标准化 ~0.5 天。总 3-4 天。

### 方向 B：Mailer 邮件产品化（P0 → P2）

**为什么需要**：业务上邮件是增长渠道（邀请、通知、摘要），技术上 DKIM 和 HTML 模板是基本前提。

**核心挑战**：
1. Reply-by-Email 需要公网 receiver（见债务三）——这是最难的架构变更
2. HTML 模板引擎选型：当前零依赖 —— 建议引入 `minijinja`（纯 Rust、兼容 Jinja2 语法、无 unsafe，编译期模板加载）或 `maud`（宏 typed 模板）
3. DKIM 签名：需要 `rsa` + `sha2` 或 `ed25519-dalek`，DNS TXT 记录需 ops 配合

**架构变更**：
| 能力 | 新增组件/变更 | 优先级 |
|------|--------------|--------|
| HTML 模板 | `aero-common/src/mail_tpl.rs` + 模板文件 | P1（先能发） |
| DKIM 签名 | `aero-common/src/dkim.rs` | P2（先配 DNS） |
| Reply-by-Email | `aero-server/src/receiver/email.rs` + 新架构模式 | P2（需要公网端点） |

**选项分析**：

| 方案 | 模板引擎 | 工作量 | 风险 |
|------|---------|--------|------|
| 维持纯字符串拼接 | 无 | 0 | HTML 注入、不可维护 |
| `minijinja`（推荐） | 编译期加载 `.j2` 文件 | 1 天 | 需要 `.j2` 文件嵌入 bin |
| `maud` | Rust 宏 | 1-2 天 | 编译慢、模板在 Rust 代码中不易维护 |

**推荐**：minijinja——团队已经用 `sqlx::migrate!("../../migrations")` 嵌入迁移，模板嵌入模式一致。

### 方向 C：前端工程化转型（零依赖 → Vite + TypeScript）

**为什么需要**（也是输入文档强调的核心）：零依赖约束已经阻碍 UI 能力扩展。虚拟列表（5000+ 消息）、i18n、交互式 Block 的状态管理都是「有则必须动工程体系」的功能。

**核心挑战**：
1. 破坏设计约束——需要明确在 roadmap 文档声明「零依赖被评估为瓶颈，决定牺牲」
2. 渐进迁移——不能一次重写所有 `.js` 文件
3. CDN 回退策略——部分已有的 hls.js / RTCPeerConnection / SpeechRecognition CDN 引用需继续工作

**架构变更**：
| 阶段 | 变更 | 工作量 |
|------|------|--------|
| Phase 1 | 引入 Vite + TypeScript，`web/` 改 `src/` 目录，基础配置 | 2 天 |
| Phase 2 | `api.js` 重写为 typed `ApiClient` | 1 天 |
| Phase 3 | `render.js` 梳理、拆组件 | 2 天 |
| Phase 4 | 虚拟列表实现（借用 `IntersectionObserver` 或自建） | 2 天 |
| Phase 5 | i18n 框架 | 1 天 |

**风险缓解**：
- Phase 1 和 2 之间没有功能回归——纯构建迁移
- **Phase 3/4/5 每个独立逆序可选**——不做 Phase 5 也能发版
- CDN 引用通过 Vite 的 `script` tag 内联或 `vite-plugin-cdn-import` 保留

### 方向 D：Webhook Signature 优先做（从方向④独立拆出）

**为什么需要**：这不仅是方向④（Webhook 出站签名）的组件，也是方向③（Bot 平台）的前提条件。没有签名，webhook consumer 无法信任 payload 来源。

**建议**：从 P2 提至 P1，优先级在 bot CRUD 之后、SDK 文档之前。

### 方向 E：多区域（仅 IM 消息，不包含实时媒体）

**为什么需要**：用户增长后延迟是刚需。但输入文档明确指出的 NATS 跨区域约束和 call-bridge 状态意味着只能做有限多区域。

**核心挑战**：
| 层 | 挑战 | 解法 |
|----|------|------|
| NATS subject seq 单调性 | 多区域桥接时 seq 乱序 | 区域本地 seq + 全局 wall-clock timestamp（客户端排序） |
| Redis 集群状态 | 读本地 write region，写就近 | 写后读一致性窗口——presence 可接受 |
| 实时媒体 | 不可行 | 文档明确声明 |
| Hub state（`stream_watchers`、`call_rosters`） | 进程本地，不需跨区域 | 不处理 |

**架构变更**：
| 组件 | 变更 |
|------|------|
| `bus/seq.rs` | 新增区域 ID 前缀（如 `us-east/seq:im.room.123`） |
| `ws/ws_impl/bus.rs` | `run_bus_listener` 新增 seq 去重逻辑（跨区域重投） |
| Redis 键空间 | 键前缀加区域 ID |
| 部署 | 区域独立 NATS + Redis + PG，bridge 只桥 `im.room.*` subject |

**选项分析**：

| 方案 | 工作方式 | 一致性 | 工作量 |
|------|---------|--------|--------|
| 单区域多节点（当前） | 一个 NATS + Redis + PG | 严格 | — |
| 多区域读副本 | 写在主区域，读副本提供只读 REST | 最终一致，写有跨区域延迟 | 3-4 天（PG 流复制 + 路由） |
| 多区域独立部署 + subject bridge（推荐 Phase 1） | 各区域独立集群，NATS bridge 同步 `im.room.*` | seq 前缀去重，~99.9% 顺序 | 5-7 天 |
| 多区域完全一致 | 强一致跨区域 | 不可能（NATS 限制） | 不推荐 |

---

## 3. 接口设计建议

### 3.1 核心原则

1. **不破坏已有 WS 帧结构**——当前 `RoomEvent` / `StreamEvent` 的 tag=kind 模式已在 web 端有对应 handler。新能力如交互式 Block 回复，应作为新 `RoomEvent` variant（如 `BlockInteraction`），而非扩展现有 variant。

2. **外部 API 契约标准化**
   - 所有 REST 响应已有 `x-request-id` ✅
   - 所有 REST 缺少 `X-Aero-Signature` header（webhook delivery）← 需补
   - 所有 REST 缺少开放 API spec（OpenAPI 只有示意性文档）← 方向③需要先做

3. **Webhook receiver 模式统一**
   ```
   /api/receiver/{provider}/{event_type}
   ```
   所有入站 receiver 共享 HMAC 验证、IP 白名单（可选）、rate limit、payload 日志层。这是当前完全缺失的架构模式。

### 3.2 新抽象层建议

| 抽象层 | 原因 | 实现位置 |
|--------|------|---------|
| `BlockInteractor` trait | 交互式 Block 的点击提交、select 变更等需要可扩展的 handler 注册 | `aero-common` 或 `aero-server` |
| `WebhookSigner` trait | 支持多种签名算法（HMAC/Ed25519） | `aero-storage` |
| `Receiver` trait | 统一公网入站 receiver 模式 | 新建 `aero-receiver` crate 或 `aero-server/src/receiver/` |

**Receiver trait 设计示例**（仅结构描述，不写代码）：

```text
trait Receiver {
    type Payload: DeserializeOwned;
    async fn verify(&self, headers, body) -> Result<VerifiedPayload>;
    async fn process(&self, payload, metadata) -> Result<()>;
    fn rate_limit_key(&self, headers) -> Option<String>;
}
```

每个 provider（Email inbound、GitHub webhook、Slack slash command）实现此 trait。统一注册到 `routes::build()` 的 `.route("/api/receiver/:provider/:event", post(handle_receiver))`。

### 3.3 向后兼容

| 变更 | 兼容策略 |
|------|---------|
| Webhook 新增签名 header | 对已有 webhook consumer 无影响——签名是额外 header，不验不影响现有行为 |
| SPA 迁移 Vite | 构建产物仍在 `web/` 目录下（Vite 可配输出目录），现有 `static_file` 路由不变 |
| 新增 `BlockInteraction` event variant | 是新的 `RoomEvent` variant，web 端不处理就静默丢弃——零兼容问题 |
| 跨区域 seq 前缀 | 单区域部署用 `local/` 前缀，兼容；多区域增加不加 |

---

## 4. 技术选型

### 4.1 新依赖引入决策网格

| 场景 | 候选库 | 评估 | 决策 |
|------|--------|------|------|
| HTML 模板 | `minijinja` / `maud` / `tera` | minijinja：纯 Rust、无 unsafe、编译期模板、Jinja2 兼容 | ✅ 推荐 minijinja |
| 构建工具 | Vite / esbuild / Parcel | Vite 是事实标准，dev server HMR 能力强 | ✅ 推荐 Vite（已用 ES modules 语法，迁移成本低） |
| 签名 | `ring` / `hmac` crate + `sha2` | ring 是大型依赖（C 绑定），hmac+sha2 纯 Rust、轻量 | ✅ 推荐 `hmac` + `sha2` crate |
| 跨区域桥 | NATS 自带的 Leaf Node / 自建 bridge | Leaf Node 是 NATS 原生跨集群连接，支持 subject 白名单 | ✅ 推荐 Leaf Node（已有 async-nats，零新依赖） |

### 4.2 自建 vs 采购

| 能力 | 建议 | 理由 |
|------|------|------|
| Webhook 签名 | **自建** | 20 行 HMAC 代码，不值得依赖一个签名 SDK |
| Bot SDK | **部分自建** | 提供 Rust SDK 和 REST Client 生成（`openapi-generator`），不采购 |
| CDN/对象存储 | **已有 S3BlobStore** | 当前架构已覆盖（`AERO_S3_BUCKET` 门控），无需更换 |
| 推流/CDN 分发 | **保持自建** | HLS 切片已在 aero-live-hls，不需要第三方转码服务 |

### 4.3 不应引入的依赖

| 候选 | 理由 |
|------|------|
| mediasoup | Node.js/C++ native，与纯 Rust 架构冲突 |
| Kafka | NATS JetStream 已覆盖 durable consumer 场景，不需要 Kafka 的额外运维成本 |
| React/Vue/Svelte | 当前 SPA 用原生 Web Component 风格已经够用；Vite + TypeScript 不需要框架 |
| Redis Cluster 模式的 Redis 客户端 | 单 Redis 实例 sorted-set 性能已足够（presence 数据量级小） |

---

## 5. 实施路线图

### 5.1 优先级矩阵

| 方向 | 优先级 | 理由 | 依赖 |
|------|--------|------|------|
| Webhook 签名 (方向D) | **P0** | bot 平台的前提，开发者信任的门槛 | 无 |
| 前端工程化 Phase 1-2 (方向C) | **P0** | 当前最大的效率瓶颈 | 无 |
| Bot 平台 (方向③) | **P1** | API 已存在，缺 SDK + 文档 | Webhook 签名 |
| 邮件 HTML 模板 (方向B) | **P1** | DKIM 可后做，模板先上 | 无 |
| Receiver 模式 (债务三) | **P1** | 方向B 的 Reply-by-Email 和方向③都需要 | 无 |
| 多区域 IM (方向E) | **P2** | 投入产出比低，且需要 NATS Leaf Node 先验证 | Receiver 和安全基础打好后 |
| 实时媒体跨区域 | **P3** | call-bridge 单区域未跑通，跨区域不可行 | call-bridge 先完工 |
| 邮件 DKIM (方向B) | **P2** | 需要 ops 配合配 DNS TXT 记录 | HTML 模板完成 |

### 5.2 阶段划分

**Phase 1（4-5 天）——基础设施 + 开发者信任**

```
Week 1:
├── Day 1: Webhook signature (hmac + sha2) + X-Aero-Signature header  ✅
├── Day 2: Vite + TypeScript 迁移（Phase 1-2）：web/ → src/、构建脚本  ✅
├── Day 3: minijinja HTML 模板 + mailer.rs 改造                       ✅
├── Day 4: Receiver trait + /api/receiver/:provider/:event 统一路由     ✅
└── Day 5: 集成测试 + CI 流水线调整（web-check.sh 兼容 Vite 构建）      ✅
```

**产出**：
- Webhook delivery 带签名
- SPA 使用 Vite 构建（产物不改变部署方式）
- 邮件模板系统可用（仅文本→带 HTML）
- `POST /api/receiver/email/inbound` 可部署

**Phase 2（3-4 天）——Bot 平台 + 开发者门户**

```
Week 2-3:
├── Day 1-2: Bot SDK 文档 + API 参考页（OpenAPI 文档增强）
├── Day 3: Rate limit header 标准化
├── Day 4: 交互式 Block（render.js 已有，完善 livecards.js 的 submitBlockInteraction 回调注册）
```

**产出**：
- 开发者可在 15 分钟内接入 bot webhook
- bot list/create/delete/token 有文档化契约
- 交互式 Block 按钮回调可用

**Phase 3（3-4 天）——前端 UI 深化**

```
Week 4:
├── Day 1: 虚拟列表（IntersectionObserver，不引入第三方依赖）
├── Day 2-3: 交互式 Block UI 完善 + 状态管理
├── Day 4: i18n 框架（如果团队认为需要）
```

**产出**：
- 5000+ 消息房间流畅滚动
- 交互式 Block 双向交互可用
- 多语言界面（先英语 + 日语，中文已有）

### 5.3 风险矩阵

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|---------|
| Vite 迁移破坏现有 CDN 引用 | 中 | 高 | Phase 1 先做构建验证，保留 hls.js CDN 引用不动（Vite 支持 external） |
| NATS Leaf Node 跨区域桥的 seq 单调性 | 高 | 中 | 文档明确声明「99.9% 顺序」，客户端侧增加 seq 前缀 + wall-clock timestamp 排序 |
| 团队对 TypeScript 掌握度不足 | 低 | 低 | 渐进采用——.js 文件改 .ts 按需，不从 Phase 1 强制类型定义 |
| SPA 迁移期间 WebSocket 断连 | 低 | 高 | 构建迭代不重启 server，dev 模式 Vite proxy 保留现有 WebSocket routing |
| call-bridge 单区域仍未跑通 | 中 | 低 | 不影响 IM 和其他方向；文档保持「媒体 seam 未接线」诚实 |

### 5.4 不做清单（明确不做的）

- 🔴 不引入 React/Vue/Svelte——原生 ES2020 + Vite + TypeScript 足够
- 🔴 不做 MLS E2E 状态机——保持 `common/src/mls.rs` 为不透明 scaffold
- 🔴 不买 mediasoup 或任何非 Rust 媒体库——str0m 投资继续，失败则换回 mediasoup 作为回退但不能同时投资
- 🔴 不做原生移动 SDK——Web 优先，`aero-push` 保持服务端推送网关定位
- 🔴 不做联邦（federation）——设计边界明确
- 🔴 多区域 Phase 1 不包含实时媒体——文档明确声明

---

## 总结

这份修正后的分析本质上识别了一个核心矛盾：**Aero IM 的后端架构（Rust、NATS、Redis、纯异步）已经达到中等规模项目的成熟度，但前端工程化和开发者平台层仍处于 MVP 阶段**。这种失衡随着 bot 平台和邮件产品化方向推进会越来越严重。

修复建议按约束性排序：
1. **Webhook 签名**和**前端工程化**是 phase 1 的锁两步——先修基础设施债，再添能力
2. **Receiver 模式**是跨方向（邮件 + bot）的高价值抽象——值得建立一次而非每次都重新设计
3. **多区域**和**实时媒体**应明确标注为「后验」——不影响当前路线图，但技术债务文档化

最关键的架构建议是：将当前零依赖 SPA 约束从「不可变设计决策」改为「经评估牺牲的决策」，并写入 product/architecture 文档。这是团队诚实面对自身产品成熟度曲线的起点。
