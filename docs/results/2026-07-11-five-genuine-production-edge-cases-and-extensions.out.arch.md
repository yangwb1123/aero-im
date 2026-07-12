现在我已完整阅读了分析文档，以下是架构层面的深度分析。

---

# 架构分析：Aero IM 五个生产级缺口的系统评估

> **分析依据**：`docs/requirements/2026-07-11-code-scan-gaps.md`（14.3 KB，5 个方向，grep 验证），交叉参考 AGENTS.md 与既有代码库结构。  
> **视角**：系统架构 — 关注耦合、抽象边界、演进能力、技术债务。  
> **约束**：不提供代码，仅架构层面的权衡与推荐。

---

## 1. 架构评估

### 1.1 当前架构的优势

分析文档中呈现的代码库结构，印证了 AGENTS.md 描述的「事件驱动 + 进程内扇出」骨架的核心优势：

- **NATS JetStream 作为跨实例事实源**：`im.room.*` durable consumer + `live.stream.*` ephemeral consumer 的分层设计合理。持久化 subject 保证 at-least-once 交付 + 跨实例水平扩；ephemeral subject 允许每实例独立消费弹幕类事件，重启无补偿压力。这是整体架构中最坚固的决策之一。

- **分层清晰（crate 地图）**：基础层（`aero-common`/`aero-bus`/`aero-storage`）→ IM 层（`aero-im-core`/`aero-im-call`/`aero-ai`/`aero-push`）→ 直播层（`aero-live-*`）→ 组合层（`aero-server`）。依赖方向严格自下而上，无成环风险。

- **Redis 用于集群级状态而非进程内存**：presence、viewer count、call roster 用 Redis sorted-set 保证跨实例一致性，正确。

- **仓储模式（`XRepo`）**：每个功能域独立 repo，通过 `lib.rs` 统一 re-export。清晰，可单元测试。

### 1.2 当前架构的局限性（与五个缺口相关）

| 缺口 | 根因架构问题 | 影响等级 |
|------|------------|---------|
| **邮件基础设施成熟度** | `mailer.rs` 作为同步 SMTP 客户端嵌入在请求 handler 中，无抽象层、无队列、无模板引擎。这是「工具函数 → 基础设施服务」的未分化状态 | ⚠️ **架构债务**：抽象层级不足 |
| **负载性能测试真空** | 非功能测试在架构中**无位置**——`benches/` 目录不存在，CI 管线无性能门禁 | ⚠️ **架构债务**：可观测/可测试性缺失 |
| **深度链接缺失** | Web SPA 采用柱塞式视图切换（`showAuth/showChat`），无客户端路由抽象。这是「原型 → 生产」的未分化状态 | ⚠️ **技术债**：UX 架构缺失 |
| **注册身份验证真空** | 注册路径在认证架构中缺少「挑战 - 响应」步骤。JWT 签发前无多因素身份因子验证（邮箱所有权是基本因子） | ⚠️ **架构债务**：认证流程不完整 |
| **邮件通知缺失** | 通知架构只覆盖了两个渠道（WS 实时 + 移动推送），缺第三个。渠道抽象层（`NotificationChannel` trait）不存在 | ⚠️ **架构债务**：渠道抽象缺失 |

### 1.3 关键设计决策合理性评估

| 决策 | 评估 | 理由 |
|------|------|------|
| `mailer.rs` 同步发送 | **❌ 不合理（在 2026 年的企业级系统）** | 注释自称设计决策，但同步 SMTP 在请求路径中阻塞是已知反模式。且无队列、无重试、无退信处理——这是架构债务 |
| 注册无邮件验证 | **❌ 不合理** | 缺乏身份验证的第一步（邮箱所有权验证）。即使 MVP，也应有 `email_verified_at` 列和可选门控 |
| Web SPA 无客户端路由 | **❌ 不合理（超出 MVP 阶段后）** | `app.js` 的柱塞式切换是原型阶段的遗留物。5.9K JS 的应用应该已经有路由抽象 |
| No load tests at all | **❌ 不合理** | 819 单元测试 + 零负载测试 = 测试架构缺少「非功能测试」维度。CI 门禁不完整 |
| 通知渠道单一抽象 | **⚠️ 可改进** | 推送 bot 和 WS 扇出走各自路径，无统一 `NotificationChannel` trait。加邮件渠道时会面临重复的偏好评估逻辑 |

### 1.4 架构债务与技术债务分类

```
技术债（可快速修复，有标准方案）：
├── 方向三（深度链接）：方案成熟（hash routing + history API），纯前端，影响范围小
├── 方向一（模板/队列/DKIM）：方案明确，mailer 扩展即可
├── 方向四（注册限流）：加入 token bucket，标准做法
└── 方向四（邮箱验证）：新增表 + 两个路由 + 一个定时任务

架构债务（需系统层重构/新增抽象，影响面大）：
├── 邮件基础设施从「工具函数」→ 「基础设施服务」的抽象升级
├── 通知渠道统一抽象层（WS + Push + Email 统一调度）
├── 非功能测试基础设施（bench/CI 回归/fuzz 集成）
└── Web SPA 从「柱塞式切换」→ 「路由驱动视图」的架构迁移
```

**关键洞察**：五个缺口中有四个是**技术债**（有标准方案、边界清晰）而非架构债务。唯一需要系统层抽象变更的是「通知渠道统一抽象」——但这在 P2 阶段，有缓冲时间。

---

## 2. 扩展方向

> 以下 5 个方向从文档的 5 个缺口出发，但上升到架构层面。每个方向给出：**为什么 → 核心挑战 → 架构变更 → 影响范围**。

### 方向 A（对应缺口一、四、五）：统一通知渠道抽象层

#### 为什么需要

当前通知架构是**硬编码的手动路由**：

```
消息 → RoomEvent → 总线 → push_bot（FCM/APNs）
                      → WS 扇出（hub）
                      → （邮件通知缺失）
```

每次新增渠道都需要：
1. 新建一个 bot（类似 `push_bot.rs`）
2. 在总线订阅中硬编码
3. 在 `notif_prefs` 中重复偏好评估逻辑

随着邮件、SMS、Webhook 等渠道增加，这种模式不可扩展。需要一个 `NotificationChannel` trait 将「通知内容 + 目标用户 + 优先级」路由到已注册的渠道。

#### 核心挑战

1. **渠道编排的优先级和回退**：WS 在线 → 移动推送 → 邮件，这条链需要在单次通知请求中原子完成，而不是三个独立 bot 竞争消费
2. **渠道偏好合并**：用户可能在邮件中设置为「只收 @mention」，在推送中设置为「收所有」——渠道策略是 per-channel per-event-type 的矩阵
3. **幂等去重**：同一通知经多渠道发送后，用户看到重复内容。需要通知 ID 在各渠道共享

#### 预期的架构变更

```
当前：事件 → NATS → [push_bot, ws_fan_out, ...] — 每个 bot 独立
未来：
事件 → NotificationOrchestrator
         ├── 评估用户偏好（notif_prefs）
         ├── 按优先级排序渠道
         ├── 逐一尝试（高级：并行 + 降级）
         └── 每个渠道实现 NotificationChannel trait
              ├── WsChannel（hub → WS）
              ├── PushChannel（push_bot → FCM/APNs）
              └── EmailChannel（mail_queue → SMTP）
```

#### 对现有系统的影响

| 影响 | 程度 | 说明 |
|------|------|------|
| `push_bot.rs` 重构 | 中 | 从 NATS consumer 变为 `PushChannel` 实现，逻辑不变 |
| `hub.rs` 扇出 | 低 | Hub 继续扇出 `RoomEvent`，但通知调度可以新建一个独立 consumer |
| `notif_prefs` 扩展 | 低 | 增加字段，不影响现有查询 |
| **现有系统兼容** | ✅ | 可增量迁移：先加 `EmailChannel` 而不动 `push_bot`，待稳定后逐步将 push_bot 重构为 channel 实现 |

---

### 方向 B（对应缺口二）：非功能测试基础设施——从「测试缺失」到「测试基建化」

#### 为什么需要

> 这是所有方向中**ROI 最高**的一个——零依赖、立即可执行、为所有后续变更提供安全网。

当前 819 单元测试覆盖了函数正确性，但完全没覆盖：
- **性能不会退化**（无基准）
- **WebSocket 不会 panic**（无模糊测试）
- **API 不会意外 break 客户端**（无契约测试）
- **系统能撑住生产负载**（无负载测试）

在单体 IM 系统中，**性能退化是静默的**——不抛异常、不报错，只是变慢。但用户感知是「为什么消息发送迟了 2 秒？」

#### 核心挑战

1. **基准测试的隔离性**：消息发送全路径涉及 DB + NATS + Hub，CI 中需要这些基础设施。解决方式：模拟层（mock NATS/client）OR 专门基准环境
2. **WS 模糊测试的状态空间爆炸**：~30 种帧类型，状态组合是指数级。需要有针对性的状态机模糊而非随机 byte fuzzing
3. **负载测试的维护成本**：k6 脚本需要随路由变化更新。解决方案：API 契约快照 + 自动生成部分负载场景

#### 预期的架构变更

```
项目根目录新增：
├── benches/
│   ├── message_throughput.rs      # 消息发送全路径基准
│   ├── rag_search.rs              # RAG 检索延迟基准
│   └── ws_frame_codec.rs          # 帧编解码基准
├── tests/
│   ├── fuzz/
│   │   └── ws_protocol.rs         # WS 状态机模糊测试
│   └── contract/
│       └── api_snapshots.rs        # API 契约快照测试
├── k6/
│   ├── scenarios/
│   │   ├── message_throughput.js   # 消息吞吐阶梯
│   │   ├── ws_connections.js       # 并发连接
│   │   └── mixed_load.js           # 混合负载
│   └── config.js
└── .github/workflows/
    └── perf-regression.yml         # CI 性能回归门禁
```

#### 对现有系统的影响

| 影响 | 程度 | 说明 |
|------|------|------|
| 产品代码 | **零** | 完全独立。不修改任何现有 .rs 文件 |
| CI 时间 | 增加 | 基准测试需在专用 runner 上运行（避免噪音）|
| 开发流程 | 正向 | PR 合并前自动检测性能退化 |

---

### 方向 C（对应缺口三）：Web SPA 从柱塞式切换升级为路由驱动架构

#### 为什么需要

5.9K JS 的应用，零客户端路由——这是一个**产品化的缺口**。深度链接直接影响：

- **用户协作效率**：分享消息链接是 IM 产品的基本操作
- **外部集成能力**：第三方系统链接到 Aero IM
- **会话连续性**：刷新不丢上下文
- **可访问性**：屏幕阅读器依赖 URL 导航

#### 核心挑战

1. **渐进式迁移**：现有 `app.js` 是单体 ~5.9K JS，重写路由架构不能一次性替换整个 SPA。需要「路由层先覆盖当前两个视图，再增量扩展」
2. **无前端构建工具**：当前 SPA 是零依赖 ES2020，引入 Webpack/Vite 是重大变更。解决方案：使用原生 `hashchange` + `history.pushState`，无需构建工具
3. **消息锚定的滚动恢复**：当 `?msg={id}` 存在时，需要等待消息数据加载完成后再滚动——这与渲染时序耦合

#### 预期的架构变更

```
web/ （纯前端，新增文件）：
├── router.js                # 路由表 + hashchange/history 适配
├── views/
│   ├── auth.js              # 从 app.js 抽取
│   └── chat.js              # 从 app.js 抽取
└── hooks/
    ├── useDeepLink.js       # ?msg={id} → 滚动高亮
    ├── useSession.js        # sessionStorage 持久化
    └── useDraft.js          # localStorage 草稿自动保存

迁移策略：
1. 先将 app.js 中 showAuth/showChat 改为 history.pushState('auth'/'chat')
2. 添加 hashchange 监听器，驱动同一视图切换逻辑
3. 增加 #/room/{id} 路由 → 解析 → 调用现有 joinRoom()
4. 增量添加消息链接、草稿、会话恢复
```

#### 对现有系统的影响

| 影响 | 程度 | 说明 |
|------|------|------|
| `app.js` 重构 | 中 | 提取视图切换逻辑到路由，但业务逻辑（消息发送、房间切换）不变 |
| `notifications.js` | 低 | 可能需适配路由变化，但核心逻辑不变 |
| `polls.js` | 低 | 投票 UI 独立，只需确保路由切换时 DOM 重新挂载 |
| 后端 | **零** | 纯前端变更 |

**关键决策点**：

| 选项 | 权衡 |
|------|------|
| **hash-based routing**（`#/room/{id}`） | ✅ 零构建工具，零后端配置，渐进可用。❌ URL 不好看，SEO 差 |
| **history API routing**（真 URL） | ✅ 漂亮 URL，SEO 友好。❌ 需要后端 fallback（`/*` → `index.html`），需构建工具 |
| **混合策略（推荐）** | 先用 hash routing 快速落地深度链接，未来加构建工具后升级到 history API |

**推荐**：hash routing 先行（无需构建工具，1-2 天可落地）。这是 P2 方向，**不建议此时引入前端构建工具链**。

---

### 方向 D（对应缺口四）：认证架构升级——从「单因子」到「多因子 + 渐进验证」

#### 为什么需要

当前认证架构只做了：
```
password → Argon2id → JWT（一步到位）
```

企业级认证架构至少需要：
```
step 1: 身份因子证明（邮箱验证 / 短信验证 / OIDC 断言）
step 2: 知识因子证明（密码 / TOTP）
step 3: 会话建立（JWT / Session Cookie）
```

邮箱验证是 step 1 的基本实现。当前架构完全跳过 step 1。

#### 核心挑战

1. **向后兼容**：现有用户都没有 `email_verified_at`。引入验证后，不能强制所有老用户验证。方案：`AERO_REQUIRE_EMAIL_VERIFICATION` 开关关，给新用户发验证邮件但不强制
2. **邮件验证令牌的安全性**：令牌是单次、有时限、SHA-256 哈希存 DB。需要防时序攻击（`timing_safe_eq`）和防暴力枚举（输入限流）
3. **注册限流的跨实例协调**：当前限流（登录限流）是 in-process，但注册限流需要在多实例间共享。需要 Redis 支持

#### 预期的架构变更

```
数据库新增：
- email_verifications 表
- participants.email_verified_at 列

认证流程变更：
注册 → INSERT participants（email_verified_at = NULL）
      → INSERT email_verifications（token_hash, expires_at）
      → 发验证邮件
      → 返回 JWT（但功能受 `AERO_REQUIRE_EMAIL_VERIFICATION` 门控）

新增路由：
- POST /api/auth/send-verification（60s 冷却，per-IP）
- POST /api/auth/verify-email（单次，过期→拒绝）

中间件变更：
- 可选中间件：未验证用户拒绝所有 mutation 操作

限流新增：
- Redis token bucket：per-IP 24h ≤ 5 注册

定时任务新增：
- 清扫 7 天未验证账号（软删除，释放邮箱）
```

#### 对现有系统的影响

| 影响 | 程度 | 说明 |
|------|------|------|
| `auth.rs` 注册路径 | 中 | 注册后增加验证记录 + 邮件发送 |
| `auth.rs` 登录路径 | 低 | 检查 `email_verified_at`（if require flag on）|
| 中间件栈 | 低 | 新增可选验证门控中间件 |
| 现有用户 | ✅ **无影响** | 老用户 `email_verified_at` = NULL，兼容 |
| `mailer.rs` | 中 | 需新增 `send_verification()` + HTML 模板 |

---

### 方向 E（对应缺口五 + 一）：邮件通道从「工具函数」升级为「基础设施服务」

#### 为什么需要

当前 `mailer.rs` 的角色是 **SMTP 工具函数**——被其他模块调用，同步阻塞，无状态管理。需要升级为**基础设施服务**——后台 drain、重试策略、退信处理、模板引擎、每工作区配置。

#### 核心挑战

1. **队列粒度的选择**：每个请求 handler 入队一封邮件 → 后台 drain。但批量场景（邀请 100 人）需要原子入队（要么全入要么全不入）
2. **退信处理与地址信誉**：SMTP 550 回复意味着地址永久无效。需要标记地址并停止向其发邮件。目前连邮件地址表都没有
3. **模板引擎的选择**：`tinytemplate` vs `handlebars` vs `minijinja`。约束：编译期模板验证（生产避免运行时模板语法错误）

#### 预期的架构变更

```
当前 mailer.rs：
- 同步 send() 函数
- 纯文本 format!
- 无队列，无重试

未来邮件服务：
MailQueue（tokio::mpsc 有界）
├── enqueue(Mail{to, subject, template, data})
├── drain() — 批量发送，≤3 次重试
├── 永久失败 → dead_letters 表 + 告警
├── 退信处理 → update email_reputation（future）
└── DKIM 签名

模板系统：
templates/email/
├── reset.html（+ .txt fallback）
├── invite.html
├── verify.html
├── mention.html（方向五）
├── digest.html（方向五）
└── golive.html（方向五）

配置扩展：
EmailConfig {
    smtp: host/port/credentials,
    dkim: { selector, private_key_path },
    from_name: String,          # 全局默认
    workspace_from: HashMap,    # 每工作区品牌化
}
```

#### 对现有系统的影响

| 影响 | 程度 | 说明 |
|------|------|------|
| `mailer.rs` 重构 | 大 | 从同步函数完全改为队列架构 |
| 调用者（`sessions.rs`, `invitations.rs`） | 中 | 从 `mailer.send_password_reset()` 改为 `mail_queue.enqueue(...)` |
| `notif_prefs` 扩展 | 中 | 新增邮件通知偏好字段 |
| 新增模板维护 | 低 | 模板文件独立于代码，不会导致编译错误 |

---

## 3. 接口设计建议

### 3.1 关键模块接口设计原则

#### 原则一：邮件服务对外接口——面向队列，而非面向函数

```
❌ 当前模式（同步函数）：
pub fn send_password_reset(to: &str, token: &str) -> Result<()>;

✅ 未来模式（消息队列）：
pub struct MailPayload {
    pub to: String,
    pub template: EmailTemplate,    // enum { Reset, Invite, Verify, Mention, Digest, Golive }
    pub data: HashMap<String, String>,  // 模板变量
    pub workspace_id: Option<WorkspaceId>,  // 用于品牌化 from
}

pub struct MailQueue {
    tx: mpsc::Sender<MailPayload>,
    // 后台: drain() 批量发送 + 重试 + 退信
}
```

**理由**：
- 解耦：调用者不需要了解 SMTP 配置、DKIM、重试策略
- 可靠：请求 handler 崩溃不丢邮件（队列在内存中，未来可持久化）
- 可观测：队列深度 → Prometheus gauge，发送延迟 → histogram

#### 原则二：通知渠道——Trait 对象统一接口

```rust
#[async_trait]
pub trait NotificationChannel: Send + Sync {
    /// 渠道唯一标识
    fn name(&self) -> &'static str;

    /// 发送通知。返回 Ok(()) 表示已接受（不保证送达），Err 表示临时/永久失败。
    async fn deliver(&self, notification: &Notification) -> Result<(), ChannelError>;

    /// 该渠道是否适用于此通知的偏好评估
    fn is_applicable(&self, prefs: &NotifPrefs, event: &NotifEvent) -> bool;
}
```

**理由**：
- `push_bot.rs` 实现 `PushChannel`
- 邮件模块实现 `EmailChannel`
- `Hub` 实现 `WsChannel`
- `NotificationOrchestrator` 遍历 channels，评估偏好，按优先级路由

**向后兼容**：先保留现有 `push_bot` 和 hub 扇出不变，新增 `NotificationOrchestrator` 作为可选路由层。待稳定后再逐步迁移。

#### 原则三：路由 → 仓储 → 服务的三层模式保持一致

分析文档提到的所有方向都应遵循现有模式（AGENTS.md §4.1 加功能配方）：

```
路由（handler）→ 调用 Service（或直接调用仓储）→ 仓储（XRepo）
```

方向四的邮件验证：
```
POST /api/auth/send-verification
→ handler 调用 VerificationService.send(user)
→ VerificationService 插入 email_verifications 行
→ VerificationService 入队 MailQueue（template=Verify）
```

方向五的邮件通知：
```
email_digest_dispatcher（定时任务）
→ 扫描未读消息 + 偏好
→ 编译摘要数据
→ 入队 MailQueue（template=Digest）
```

### 3.2 是否需要引入新的抽象层

| 抽象层 | 必要性 | 时机 |
|--------|--------|------|
| **`NotificationChannel` trait** | ✅ 必要——三个渠道后必须统一 | P2（邮件通知方向），现在可设计但不急着实现 |
| **`MailQueue` 队列抽象** | ✅ 必要——从同步到异步必须 | P1（邮件基础设施方向），第一批实施 |
| **`VerificationService`** | ⚠️ 可选但推荐——把验证逻辑从路由 handler 隔离 | P1（注册验证方向），与邮件基础设施同批 |
| **`Router` 前端抽象** | ✅ 必要——柱塞式切换不可持续 | P2（深度链接方向），引入 hash routing |

### 3.3 向后兼容性策略

| 变更 | 兼容策略 |
|------|---------|
| mailer.rs 从同步改为异步队列 | 保留 `send_password_reset()` 函数签名，内部改为 enqueue + await completion（或 fire-and-forget + log）。调用者无需修改 |
| `participants` 表加 `email_verified_at` | `DEFAULT NULL`。现有行兼容，旧代码读取时忽略 |
| 注册增加邮件验证 | `AERO_REQUIRE_EMAIL_VERIFICATION = false`（默认），仅新注册发验证但不门控 |
| Web SPA 增加 hash routing | 在 `app.js` 顶部注入路由层，`showAuth/showChat` 逻辑不变。路由只负责「进入视图前恢复状态」 |
| 通知抽象层 | 先新建，不修改现有。`push_bot` 继续运行，`NotificationOrchestrator` 作为可选层在旁边运行 |

---

## 4. 技术选型

### 4.1 是否需要引入新技术栈

| 方向 | 技术引入 | 评估 |
|------|---------|------|
| **邮件模板** | 轻量模板引擎 | ✅ **必须引入**。现有 `format!` 纯文本不可扩展 |
| **邮件队列** | 无——复用 `tokio::mpsc` | 现有技术栈已支持。不需要消息队列（NATS 用于跨实例通信，邮件队列是进程内批次发送）|
| **性能基准** | `criterion` / `cargo-criterion` | ✅ **强烈推荐**。Rust 生态标准，CI 集成成熟 |
| **WS 模糊测试** | `fuzzcheck` / `libfuzzer` | ✅ 推荐。但需注意：fuzz 测试在 CI 中时间开销大，建议 nightly 运行 |
| **API 契约测试** | `insta`（快照测试） | ✅ **强烈推荐**。现有代码库已用？如果没，则引入。Rust 生态标准 |
| **负载测试** | k6 | ✅ 推荐。JavaScript 脚本，团队已有 JS 经验（5.9K SPA）|
| **Web 路由** | 无——原生 API | ❌ **不需要框架**。`hashchange` + `history.pushState` 原生 API 足够 |

### 4.2 邮件模板引擎选型

| 选项 | 优势 | 劣势 | 推荐 | 
|------|------|------|------|
| **`tinytemplate`** | 零依赖（serde 之外），编译时模板加载（`include_str!`） | 功能最小（无循环/条件简版） | ✅ **P0 推荐**——我们只需要变量替换 + 简单条件 |
| **`handlebars`**（rust版本） | 功能完整（循环/条件/partials），社区大 | 额外依赖，运行时编译（可能失败） | ✅ **P1 推荐**——当模板复杂度增加时升级 |
| **`minijinja`** | 兼容 Jinja2 语法，功能完整 | 额外依赖 | ⚠️ 可选——如果团队熟悉 Jinja2 |
| **手写 HTML + 字符串替换** | 零依赖 | XSS 风险，不可维护 | ❌ **不推荐**——mailer 已经是 `format!`，再加更乱 |

**推荐策略**：先用 `tinytemplate`（编译时 `include_str!("../../templates/email/reset.html")`，模板错误在编译期暴露）。后续如需循环/复杂条件，升级到 `handlebars` 或 `minijinja`。

### 4.3 自建 vs 采购的决策依据

| 组件 | 决策 | 理由 |
|------|------|------|
| **邮件发送** | **自建**（在现有 `mailer.rs` 上扩展） | 场景有限（密码重置/验证/通知摘要），SMTP 协议成熟。lettre 已引入。无需采购 SendGrid/Mailgun（但可将来作为可选 provider）|
| **HTML 邮件模板** | **自建** | 模板数量 ≤10 个，维护成本低。品牌化通过模板变量注入 |
| **性能基准框架** | **自建**（criterion benchmark）| Rust 生态原生，无需外部服务 |
| **负载测试** | **自建**（k6 脚本）| k6 开源，脚本在代码仓库中版本控制。无需采购 LoadImpact/Blazemeter（但未来可考虑托管运行） |
| **Web 路由** | **自建**（原生 API）| 不需要框架。5.9K JS 的 SPA 引入路由框架是过度工程 |

### 4.4 第三方依赖评估标准

对于本次新增依赖（模板引擎、基准框架），评估标准应遵循：

1. **编译时间影响**：`tinytemplate` 零额外编译时间（仅 serde）。`criterion` 仅 dev-dependency，不影响发布二进制
2. **安全审计面**：模板引擎不处理用户输入转义？模板内容是服务端控制的（品牌名、消息摘要），不是用户输入的原始 HTML。但需确保模板本身不引入 XSS
3. **维护活跃度**：`tinytemplate` 最后更新 2022（但功能稳定不需要频繁更新）。`criterion` 是 Rust 生态标准
4. **与现有技术栈的契合度**：所有推荐依赖都是纯 Rust，与 `tokio`、`serde` 生态兼容

---

## 5. 实施路线图

### 5.1 优先级排序（重新评估）

| 优先级 | 方向 | 理由 |
|--------|------|------|
| **P0** | 方向二（负载与性能测试） | 零依赖，立即可执行。为所有后续变更提供安全网 |
| **P0** | 方向一 Step 1（邮件队列 + HTML 模板）| 方向四、五的共同依赖。把 mailer 从「同步函数」升级为「基础设施服务」|
| **P0** | 方向四 Step 1（邮箱验证流程） | 依赖方向一的队列和模板。安全基线，企业客户最低要求 |
| **P1** | 方向一 Step 2（DKIM） | 邮件送达保障。但优先级可延后（先用无 DKIM 跑，监控送达率）|
| **P1** | 方向四 Step 2（注册限流 + 未验证清扫） | 依赖方向四 Step 1。僵尸账号防护 |
| **P1** | 方向五（邮件通知调度器 + 离线回退） | 依赖方向一的模板和队列。产品差异化能力 |
| **P2** | 方向三（深度链接） | 纯前端，可并行。产品体验提升，但非阻塞 |

**调整说明**：

文档原排期「方向一 + 方向四紧耦合执行」正确，但我的路线图将方向二（性能测试）**提前到 P0 最优先**，因为：
- 方向二是「零依赖 + 立即可执行 + 为所有后续变更提供安全网」——先还测试债，再添加新功能
- 邮件基础设施和邮件验证涉及 DB 迁移、队列架构变更、新路由——这些变更如果没有性能基准门禁，**可能引入静默退化**

### 5.2 阶段划分和里程碑

#### Phase 1：地基（2-3 周）

```
目标：还清测试债 + 邮件基础设施升级

[Week 1] 方向二 Step 1 — 基准测试
- 添加 3 个 cargo bench（消息吞吐 / RAG / WS 编解码）
- CI 集成 cargo-criterion（基准比较 + 阈值告警）
- ✅ 里程碑：CI 中「性能回归」门禁激活

[Week 2-3] 方向一 Step 1 — 邮件队列 + HTML 模板
- 引入 tinytemplate，创建 templates/email/ 目录
- 将 mailer.rs 重构为 MailQueue（mpsc sender + drain task）
- 迁移现有 reset/invite 到模板 + 队列
- 添加 DKIM 配置选项（DKIM 签名实际在 Phase 2 激活）
- ✅ 里程碑：邮件发送改为异步队列，不阻塞请求 handler
```

#### Phase 2：安全基线（2-3 周）

```
目标：邮件验证 + 注册防护

[Week 1-2] 方向四 Step 1 — 邮箱验证流程
- email_verifications 表迁移
- POST /api/auth/send-verification + /verify-email
- participants.email_verified_at 列
- 可选 AERO_REQUIRE_EMAIL_VERIFICATION 门控（默认关）
- 现有用户兼容（email_verified_at = NULL 视为已验证）
- ✅ 里程碑：新注册用户可验证邮箱，老用户无影响

[Week 3] 方向四 Step 2 — 注册限流 + 清扫
- Redis token bucket（per-IP, 24h ≤ 5 注册）
- 定时清扫 7 天未验证账号
- 注册频率 Prometheus 告警
- ✅ 里程碑：僵尸账号防护激活
```

#### Phase 3：通知扩展 + 深度链接（并行，2-4 周）

```
目标：邮件通知渠道 + Web SPA 路由

[Week 1-2] 方向五 Step 1 — 邮件通知调度器
- notif_prefs 扩展（email_on_mention, email_digest 等字段）
- email_digest_dispatcher 定时任务
- mention/digest/golive 邮件模板
- ✅ 里程碑：用户可开启「@mention 邮件通知」「每日摘要邮件」

[Week 1-2] 方向三（并行）— 深度链接
- web/router.js — hash routing 核心
- #/room/{id}、#/room/{id}?msg={mid} 路由
- 复制消息链接按钮
- sessionStorage 会话恢复
- ✅ 里程碑：Web SPA 支持深度链接 + 浏览器前进后退
```

#### Phase 4：邮件送达保障（1-2 周，可选）

```
目标：DKIM + 退信处理 + 品牌化

- DKIM 签名激活（lettre 内置支持）
- 退信检测（SMTP 550 → 标记地址 → 停止发送）
- 每工作区品牌化 From 地址
- A/B test：DKIM 签名后送达率变化
- ✅ 里程碑：邮件送达率达到企业级标准（>95%）
```

### 5.3 依赖图与并行策略

```
Phase 1 (P0)                    Phase 2 (P0)              Phase 3 (P1)              Phase 4 (P2)
┌──────────────┐               ┌──────────────┐          ┌──────────────┐          ┌──────────────┐
│ 方向二       │               │ 方向四 Step 1│          │ 方向五       │          │ 方向一 Step 2│
│ 基准+CI      │               │ 邮箱验证     │◄────依赖─┤ 邮件通知调度 │◄────依赖─┤ DKIM+退信    │
│  （独立）     │               │              │          │              │          │              │
└──────────────┘               └──────┬───────┘          └──────────────┘          └──────────────┘
                                      │
Phase 1 (P0)                          │依赖
┌──────────────┐                      │
│ 方向一 Step 1│◄─────────────────────┘
│ 邮件队列+模板│
│  （独立）     │                      ┌──────────────┐
└──────────────┘                      │ 方向三       │
                                      │ 深度链接     │
                                      │  （独立，并行）│
                                      └──────────────┘
```

**并行策略**：
- 方向二（性能测试）与方向一 Step 1（邮件队列）**可以并行**——二者修改不同文件集
- 方向三（深度链接，纯前端）与以上全部**完全并行**——零依赖
- 方向四 Step 1（邮箱验证）必须等方向一 Step 1 完成（需要模板和队列）
- 方向五（邮件通知）必须等方向一 Step 1 完成 + 方向四（验证）完成

### 5.4 风险点和缓解策略

| 风险 | 等级 | 可能性 | 影响 | 缓解策略 |
|------|------|--------|------|---------|
| **邮件队列溢出**：批量发送时 mpsc channel 满，handler 阻塞 | 中 | 中 | handler 超时 | `try_send` + 降级（直接 SMTP 发送），或 `Vec` 批量入队。初始容量 1024 已足够 |
| **模板引擎选择错误**：`tinytemplate` 功能不足导致后期重写 | 低 | 低 | 工作时间浪费 | 先用 `tinytemplate` 实现最简单的 reset/invite 模板。复杂模板（digest 含循环）如果 `tinytemplate` 不够，再升级 `handlebars`——模板引擎接口是内部细节，不影响调用者 |
| **DKIM 私钥管理**：私钥泄露或轮换 | 中 | 低 | 高（邮件仿冒） | 私钥路径从环境变量读取，不在配置文件明文存储。轮换通过更新文件 + 重启实现 |
| **邮件验证影响注册转化率**：加验证后注册完成率下降 | 中 | 中 | 产品指标 | `AERO_REQUIRE_EMAIL_VERIFICATION = false` 默认关。先观测验证邮件送达率和验证率，再决定是否强制 |
| **性能基准噪音**：CI runner 负载不一致导致误报 | 高 | 高 | 中（误报疲劳） | 专用 CI runner（`self-hosted` tag），或比较 `p50` 而非 `mean`，或设置宽松阈值（>10% 退化才报警）|
| **Web SPA 路由与现有柱塞式切换冲突**：同时维护两套导航逻辑 | 中 | 低 | 中（维护成本） | 策略：hash routing 覆盖后，逐步淘汰 `showAuth/showChat`。在完成路由迁移前保留两套逻辑并行 |

### 5.5 不做（明确排除在本次范围外）

| 被排除项 | 理由 |
|----------|------|
| 引入 SendGrid/Mailgun 等第三方邮件服务 | 当前场景有限（密码重置 + 验证 + 摘要），SMTP 足够。未来若需要「批量营销邮件」「送达率分析」，可引入 |
| 邮件持久化队列（基于 DB/NATS） | 邮件是最终一致性场景，进程崩溃丢失几封邮件是可接受的。mpsc 足够了。未来如果需要可靠投递，可升级到 NATS-backed 队列 |
| 完整的邮件地址簿 / 联系人管理 | 超出本次缺口范围。退信处理只标记已知地址，不管理通讯录 |
| Web SPA 引入前端构建工具（Webpack/Vite）| 5.9K JS 的应用引入构建工具是过度工程。hash routing 原生 API 足够 |
| SMS 通知渠道 | 不在五个缺口范围内。未来若需要，可复用 `NotificationChannel` trait |
| 全量 OpenAPI 规范自动化 | 文档推荐的 API 快照测试（方向二）是第一步。全量 OpenAPI + 客户端代码生成是更远的工程投资 |

---

## 总结：架构决策一览

| 决策 | 选项 | 推荐 | 理由 |
|------|------|------|------|
| 邮件从同步改为异步 | `mpsc` / NATS-backed / 同步保留 | **`mpsc` 队列** | 最简单，恰好满足需求。NATS-backed 是未来选项 |
| 模板引擎 | `tinytemplate` / `handlebars` / 手写 | **`tinytemplate`**（P0）→ `handlebars`（P1）| 先简单，再升级 |
| Web 路由 | hash routing / history API / 框架 | **hash routing（原生 API）** | 零构建工具，渐进可用 |
| 通知渠道抽象 | `NotificationChannel` trait / 继续加 bot | **trait（设计先行，P2 实现）** | 三个渠道后必须统一 |
| 注册限流 | in-process / Redis / 无 | **Redis token bucket** | 多实例协调，与现有 Redis 复用 |
| 验证门控策略 | 默认开 / 默认关 / 环境变量切换 | **`AERO_REQUIRE_EMAIL_VERIFICATION=false`（默认关）** | 兼容现有部署，opt-in 激活 |
