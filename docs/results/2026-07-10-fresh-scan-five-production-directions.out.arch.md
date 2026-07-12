现在我有完整的上下文了。以下是我的架构分析。

---

# 架构分析：Aero IM 的五个未覆盖生产级扩展方向

> **分析依据**: `docs/requirements/2026-07-11-round-32-global-scan-five-uncovered-prod-expansion-directions.md`
> **交叉验证**: 用户验证报告已确认 5 个方向均通过代码 grep 验证，且与现有 238+22 份分析零重叠
> **视角**: 资深架构师，聚焦架构评估、扩展方向、接口设计、技术选型、实施路线图

---

## 1. 架构评估

### 1.1 当前架构的优势

**分层 crate 架构是正确的长期赌注**。16 个 crate 的依赖自下而上（common → bus/storage/auth → im-core/live-core → server），每层只依赖下层，这为五个方向中的三个（邮件、多区域、webhook）提供了清晰的切入点：

- **邮件**可以新增 `aero-mail` crate 或在 `aero-server` 内扩展，不影响 IM 核心层
- **Webhook 签名**是 `aero-server/outbound` 的过滤层，改动局限在一个模块
- **多区域**涉及 storage 层的连接池路由，但 `AppState` 已设计为可容纳多个 `Pool`/`RedisPool`

**事件总线（NATS JetStream + Hub）是架构中最具韧性的部分**。durable consumer 提供 at-least-once 语义，为邮件队列、webhook 重试、多区域事件桥提供了现成的投递原语——不需要重新发明消息队列。

**代码质量基线很高**：`unsafe_code = "forbid"`、clippy pedantic、`truth-check.sh` 检测死代码，这确保五个方向不会因为有未使用的函数而迷失在死代码中。

### 1.2 关键局限性

**单一的「Debug Client」前端是产品化最大的单点阻力**。后端 25K Rust 对应前沿的架构设计（SFU Simulcast、JetStream 事件桥、分层 crate），而前端 5.9K 纯 ES2020 零构建零测试——这种投入失衡意味着所有需要前端配合的功能（邮件设置 UI、webhook 订阅管理、区域选择器）都面临高集成成本。

**邮件仅做带外场景，忽略了最大的用户触达渠道**。125 行的 `mailer.rs` 只处理密码重置和邀请——这两种邮件加起来在 Slack/Mattermost 的邮件总发送量中占比不到 5%。用户打开邮箱的频率远高于打开 Aero IM 客户端的频率，当前的邮件策略等于主动放弃了最大的拉回渠道。

**Webhook 安全模型的缺失是平台化前的安全债**。一个 IM 系统的出站 webhook 承载着消息内容、用户事件、房间变更——如果这些 payload 无法被消费者验证真实性，任何接入 webhook 的外部系统都暴露在 spoofing 攻击下。这不仅是缺失功能，而是安全架构的漏洞。

**可观测与可运维之间存在断层**。Prometheus 指标 + Grafana 面板 + 22 条告警规则是坚实的可观测种子。但灾备恢复、容量规划、变更管理、事故响应流程均为零——这意味着每次 PG 宕机都是 fire drill，每次迁移发布都靠专家记忆。

### 1.3 架构债务评估

| 类型 | 严重程度 | 具体表现 |
|------|---------|---------|
| **前端架构债** | **严重** | 零组件模型、零状态管理、零测试、零构建——5.9K 行单体 JS 无法增量演进，只能整体替换或长期并行 |
| **邮件架构债** | 中等 | 同步 SMTP 调用阻塞请求处理线程，无队列缓冲意味着 SMTP 故障直接导致 reset/invite 失败率高 |
| **邮件交付债** | 低 | 无退订、无 DKIM、无 HTML——纯文本在当前邮件客户端（Gmail/Outlook）中被归为「低质量发件人」，可能被归类到垃圾箱 |
| **安全债** | **严重** | Webhook 无签名——consumer 无法验证 payload 来源，此为平台 API 的安全基线 |
| **运维债** | 中等 | 无备份恢复验证、无容量规划、无事故响应流程——系统复杂度的增长速度超过了运维能力的增长速度 |
| **全球化债** | 低 | 架构层无数据主权概念，但系统尚未服务多区域客户——这是可以接受的延迟负债 |

---

## 2. 扩展方向

### 方向 A：邮件通信渠道（原方向一，P1 上调至 P0）

**为什么需要**：邮件是 IM 系统的「离线安全网」。一个用户在三种情况下最可能回到平台：被人 @提及、收到未读摘要邮件、收到直接回复邮件。当前这三种触达机制全部缺失。对于 To-B 协作产品，邮件渠道的日送达量通常是 API 消息量的 3-10 倍。

**核心挑战**：

| 挑战 | 技术难点 | 严重程度 |
|------|---------|---------|
| 队列化与背压 | 当前同步发送不能让 SMTP 慢速影响 HTTP 请求 | 中等——可用 NATS 或 PG 表的 `email_queue` 解耦 |
| 模板渲染与跨团队协作 | 设计师需要能预览邮件模板，不依赖运行后端 | 中等——`minijinja` 的模板编译时校验能力比 `tera` 更适合 |
| 入站网关的安全模型 | 邮件伪造（spoofing）是最常见的攻击向量，DKIM 验证失败与误报之间的平衡 | **高**——入站网关的错误接受率直接决定消息系统的信任边界 |
| 退订合规 | CAN-SPAM 要求退订在 10 天内被处理，GDPR 要求更严格 | 低——已有 `notif_prefs` 模型可扩展 |

**预期的架构变更**：

```
mailer.rs (125 行)
    ↓
aero-mail crate:
├── mailer/           # 复用现有 lettre transport
│   ├── mod.rs        # Mailer struct (扩展为队列驱动)
│   ├── transport.rs  # 从 config 构建 SmtpTransport
│   └── dkim.rs       # DkimSigner 封装 (P2)
├── templates/         # minijinja HTML 模板 (编译期嵌入)
│   ├── reset.jinja
│   ├── invitation.jinja
│   ├── digest.jinja
│   └── notification.jinja
├── queue.rs           # email_queue 表 + run_email_dispatcher
├── gateway.rs         # POST /api/email/inbound + DKIM verify
├── unsubscribe.rs     # List-Unsubscribe 头 + one-click 处理
└── prefs.rs           # NotificationChannel::Email 枚举扩展
```

**对现有系统的影响**：低。`Mailer` struct 保持兼容（`send_password_reset`/`send_invitation` 签名不变），新功能通过队列异步化。`notif_prefs` 需要加枚举变体，这是向后兼容的。

**系统加固建议**：邮件队列使用 `email_queue` 表 + `FOR UPDATE SKIP LOCKED` 轮询，复用现有 `AiWorker` 的 worker 调度模式。不引入独立队列系统（RabbitMQ/Kafka），因为邮件队列对吞吐量要求低（单实例每秒 <50 封即可）。

### 方向 B：前端工程基础设施（原方向二，P1 但配套依赖高）

**为什么需要**：这不是「UX 改进」——这是「产品从不可用变为可用」的基础。一个没有测试、没有构建、没有路由、没有 i18n、没有 a11y 的 SPA 在 5.9K 行时已经达到单体极限，每增加一个功能都增加整体复杂度而非模块边界。

**核心挑战**：

| 挑战 | 技术难点 | 严重程度 |
|------|---------|---------|
| 零依赖原则的打破 | 从无 `package.json` 到 Vite 构建，CI 和开发流程都需要变化 | 中等——引入 `devDependencies` 不影响运行时 |
| 渐进式替换 vs 重写 | 重写会直接停产 2-3 周，渐进式会导致新旧代码风格混用 | **高**——工程决策失误会导致过渡期代码更难维护 |
| i18n 的中期成本 | 每个新功能都要同步更新两套 locale 文件，忘记翻译的检测机制 | 低——`eslint-plugin-i18n` 可以 CI 阻断 |
| 虚拟列表的消息高度不确定性 | 消息含图片、代码块、内嵌视频时渲染高度不可预测 | 中等——`ResizeObserver` 补偿 + 估算高度池 |

**预期的架构变更**：

```
web/
├── package.json          # 新增 (仅 devDependencies: vite, eslint, ...)
├── vite.config.js        # 构建配置
├── index.html → src/index.html  # Vite 入口
├── src/
│   ├── main.js           # 入口：初始化 + 路由
│   ├── router.js         # Hash-based 路由 (#/rooms/:id)
│   ├── store.js          # 单向数据流（替代全局 state）
│   ├── components/       # 可复用组件
│   │   ├── Message.js
│   │   ├── RoomList.js
│   │   ├── Composer.js
│   │   └── ...
│   ├── pages/            # 页面级别组件
│   │   ├── ChatPage.js
│   │   ├── ThreadPage.js
│   │   └── SearchPage.js
│   ├── api/              # API 客户端 (从 api.js 拆分)
│   ├── ws/               # WebSocket 客户端 (从 ws.js 拆分)
│   ├── locales/          # i18n 语言包
│   │   ├── zh.json
│   │   └── en.json
│   └── style/            # CSS Modules 或 CSS 变量文件
│       ├── variables.css
│       ├── themes/
│       │   ├── light.css
│       │   └── dark.css
│       └── components/
├── public/
│   ├── manifest.json     # PWA
│   └── sw.js             # Service Worker
└── tests/                # 测试 (vitest)
    ├── ws.test.js
    ├── store.test.js
    └── api.test.js
```

**对现有系统的影响**：高但不破坏。当前 `index.html` + `app.js` 可以在迁移期间继续工作。建议策略：**Vite 构建现有代码 → 逐步模块化 → 最后清理**。Vite 的 ESM 开发模式可以直接 serve 现有代码，不需要先重写再构建。

**重要的设计决策**：不要引入 React/Vue/Svelte。当前团队没有前端人员，引入框架意味着框架维护成本。自定义 `AeroComponent` 基类（围绕 `<template>` + `attachShadow` 的轻量封装）足以支撑 L1-L2，且零运行时依赖。

### 方向 C：Webhook 出站签名（原方向四，P2 但建议提升至 P1）

**为什么需要**：如果一个 Bot 平台/Webhook 消费者收到的 payload 无法验证来源真实性，整个集成生态的信任基础是空的。Stripe/GitHub/Slack/Twilio 全部在第一天就实现了签名——这是商业 API 的**安全准入基线**，不是可选增强。

**核心挑战**：

| 挑战 | 技术难点 | 严重程度 |
|------|---------|---------|
| 签名密钥的生命周期管理 | 创建、旋转、吊销、过期 | 低——复用已有的 `generate_token`/`hash_token` + `bot_rotate_token` 模式 |
| timestamp 偏差的处理 | 消费者服务器时钟不同步 | 低——5 分钟默认窗口，可配置 |
| body 序列化一致性 | 签名时的 body bytes 与验证时的一致 | **中等**——不能对 JSON 重新序列化，必须使用原始 bytes |
| 幂等键暴露 | 内部幂等键暴露到 HTTP header 的设计 | 低——直接复用已有 `delivery_idempotency_key` |

**预期的架构变更**：最小。这是五个方向中变更影响最精确的：

```rust
// webhook.rs 现有递送函数签名
pub async fn deliver_webhook(
    delivery: &WebhookDelivery,
    payload: Value,
) -> Result<DeliveryStatus, DeliveryError>;

// 扩展为：
pub async fn deliver_webhook(
    delivery: &WebhookDelivery,
    payload: Value,
    signing_secret: &SigningSecret,  // 新增参数
) -> Result<DeliveryStatus, DeliveryError> {
    let body = serde_json::to_vec(&payload)?;          // 原始 bytes
    let ts = chrono::Utc::now().timestamp();
    let sig = hmac_sha256(signing_secret, format!("{ts}.{}", body));
    let headers = vec![
        ("X-Aero-Signature-v1", sig),
        ("X-Aero-Timestamp", ts.to_string()),
        ("X-Aero-Delivery-Idempotency-Key", delivery.idempotency_key),
    ];
    // ... 带 headers 做 HTTP POST
}
```

**对现有系统的影响**：低。`deliver_webhook` 的调用方（`webhook_admin.rs`）只需要传递 `signing_secret`（从 webhook_subscriptions 表读取），无需改造递送逻辑。新增 header 不影响现有消费者——它们只是忽略未知 header。

**系统加固建议**：这是**唯一可以在 2-3 天内独立完成的方向**。做完后立即发布并更新 SDK 文档，不需要等待任何其他方向。

### 方向 D：生产运维体系（原方向三，P2 但持续推进型）

**为什么需要**：监控只告诉你「出问题了」，但运维体系让你知道「该找谁」「怎么修」「怎么保证下次不犯」。无 runbook 的告警等于噪音。

**核心挑战**：

| 挑战 | 技术难点 | 严重程度 |
|------|---------|---------|
| 备份恢复验证是一个 CI 问题而非脚本问题 | 每周在 throwaway DB 上跑全量 `pg_restore` 需要 10GB+ 磁盘空间 | 中等——CI runner 需要足够的临时存储 |
| 事故严重性定义的执行 | 定义了 Sev0/Sev1/Sev2 但没有人遵守 | **文化问题，非技术问题**——runbook 可以强制执行 on-call 流程 |
| 容量规划需要历史数据 | 当前无基线数据去做增长率模型 | 中等——可以开始记录 `pg_database_size` 的 Prometheus gauge |

**预期的架构变更**：主要是文档和脚本，少量代码改动。`docker-compose.yml` 增加 `pg_dump` sidecar container。`background.rs` 增加 `run_db_health_check` 定时器。

**对现有系统的影响**：近乎零。运维是正交维度。

**系统加固建议**：方向 D 是五个方向中**技能门槛最低但长期价值最高**的——建议非 Rust 团队成员负责。

### 方向 E：多区域架构与数据主权（原方向五，P3）

**为什么需要**：全球团队场景的架构约束是整个系统中最难改造的部分——在单区域架构已经固化之后。好消息是：**当前不需要实现，但必须开始做架构决策**，否则 3-6 个月后的每个数据模型变更都可能破坏多区域可能性。

**核心挑战**：

| 挑战 | 技术难点 | 严重程度 |
|------|---------|---------|
| PG 逻辑复制的 DDL 限制 | `messages` 分区切换（`messages-partitioning.md`）与逻辑复制 slot 的冲突 | **高**——如果运行中分区迁移和逻辑复制同时操作，复制 slot 可能崩溃 |
| 写后读一致性 | 用户在区域 A 发消息后立即刷新，请求可能落到延迟未同步的区域 B 副本 | **高**——这是 IM 系统的核心体验问题 |
| NATS JetStream 的跨区域限制 | NATS 原生不支持跨区域复制，需要区域桥 | 中等——aero-bus 扩展可抽象区域桥 |
| `data_residency` 列对数据模型的侵入 | 每个 PII 表都要加分区/列，索引策略变化 | **高**——这是侵入性最强的数据模型变更 |

**预期的架构变更**：主要是文档 + 少量 schema 预留。建议只在 `messages`/`participants`/`workspaces` 表预留 `data_residency` 列（`VARCHAR(16)`，默认 `global`），不做读写分离路由。

**对现有系统的影响**：当前需零实现。但架构决策文档必须**立即产出**，以避免未来添加的约束破坏多区域可能性。

**系统加固建议**：方向 E 的建议 > 实施。产出一份 `docs/architecture/multi-region.md`（~50 行），定义：

1. 数据主权分类表（哪些表是 PII / 非 PII / 不可复制）
2. 复制策略图（同步/异步/不复制）
3. 区域故障转移层级（手动/半自动/全自动）
4. 禁止的架构模式（如：跨区域 FK 引用、全局自增 ID）

这份文档不需要审批——只需要让未来每个 PR 的作者知道他们是否违反了多区域约束。

---

## 3. 接口设计建议

### 3.1 需要引入的新抽象层

**邮件抽象层**（方向 A）：
- `Mailer` trait（当前是 struct）——允许 `SmtpMailer` / `FileMailer` / `NullMailer` 实现
- trait 方法签名保持兼容：`fn send(&self, envelope: MailEnvelope) -> Result<DeliveryReceipt>`
- `MailEnvelope` struct 封装模板名 + 变量 + 收件人 + 优先级——不直接操作 `lettre::Message`
- 理由：测试时需要 mock 邮件发送；邮件网关（入站）需要独立接口

**Webhook 签名层**（方向 C）：
- 当前 `deliver_webhook` 是自由函数，建议保留——不引入 trait，因为只有一个实现
- 签名逻辑封装为 `WebhookSigner` struct，可单独单元测试
- `verify_signature` 独立函数：`fn verify_signature(expected_sig: &str, body: &[u8], ts: i64, secret: &SecretKey) -> bool`

**数据主权策略层**（方向 E）：
- `DataResidencyPolicy` trait：`fn routing_region(&self, record: &dyn DataResidencyAware) -> Region`
- 允许 `StrictestPolicy`（默认 `eu_only`）vs `RelaxedPolicy` vs `PerRecordPolicy`
- 不侵入业务逻辑——只在 `insert`/`select` 的仓储层做路由

### 3.2 不应引入的抽象层

**前端组件框架**（方向 B）——不应抽象为独立 crate 或 NPM 包。Web 端是一个应用，不是平台 SDK。维护自定义组件基类即可，避免框架锁定。

**邮件模板引擎**（方向 A）——不应抽象为独立 crate。`minijinja` 的 `Environment` 在 `aero-mail` 内部初始化即可，不需要像 Django 的模板系统那样抽成独立服务。

### 3.3 向后兼容策略

| 变更 | 兼容策略 | 迁移期 |
|------|---------|--------|
| `Mailer` struct → trait | 保留 struct API 作为默认实现，新 trait 可选 | 无限期 |
| `notif_prefs` + Email variant | 默认 email 关闭，不改变现有通知行为 | 不需要迁移 |
| Webhook header 添加 | consumer 忽略未知 header，推荐逐步采用签名验证 | 3-6 个月过渡期 |
| `data_residency` 列 | 默认 `global`，旧行自动兼容 | 无限期 |
| Vite 构建引入 | 构建产物的 URL/文件名不变，`index.html` 入口不变 | 无缝 |

---

## 4. 技术选型

### 4.1 新增依赖评估

| 方向 | 候选 | 评估 | 推荐 |
|------|------|------|------|
| 邮件模板 | `minijinja` vs `tera` vs `maud` | `maud` 编译宏+Rust DSL → 设计师无法编辑模板；`tera` 运行时解析 → 启动慢；**`minijinja`** 编译期序列化 + 零运行时依赖 → 最轻量 | **`minijinja`** |
| 邮件 HTML 渲染 | `lettre` 原生支持 HTML | 无需额外依赖，`lettre::Message::builder().body()` 接受 `ContentType::TEXT_HTML` | 零依赖 |
| 邮件 DKIM | `lettre` `DkimSigner` | 已在内含，只需启用 feature | **`lettre/dkim`** |
| Webhook HMAC | `ring` vs `hmac + sha2` | `ring` 是 BoringSSL 绑定，体积大；RustCrypto 的 `hmac + sha2` 纯 Rust，与现有代码一致 | **`hmac + sha2`**（或 `aero-common` 已有的 crypto 依赖） |
| DB 健康检查 | `pg_stat_activity` 直接查询 | 不需要额外的 agent（如 `pg_stat_monitor`），减少依赖面 | **纯 SQL** |
| Vite 构建 | Vite + esbuild | Vite 的 dev server 支持 HMR，esbuild 构建速度快于 webpack 一个数量级 | **Vite** |
| 前端测试 | `vitest` vs `jest` | vitest 与 Vite 共享配置，零配置迁移 | **vitest** |

### 4.2 自建 vs 采购

| 场景 | 建议 | 理由 |
|------|------|------|
| **邮件发送**（SMTP 直发） | 自建，复用 `lettre` | 邮件量很小（通知 < 50/秒），不需要 SendGrid/Mailgun 的智能推荐功能 |
| **邮件入站网关** | 自建 | 入站邮件解析 + DKIM 验证 + 路由 token 都是定制逻辑，通用邮件网关（如 Mailgun Inbound Parse）只处理格式不处理业务 |
| **邮件模板设计** | 自建 HTML 模板 | 只有 4-5 种模板类型，不需要 MJML/Unlayer 等设计系统 |
| **事故响应值班** | 采购（PagerDuty/Opsgenie） | 值班轮换 + 升级策略 + 日历集成是已有 SaaS 做得很好的领域，自建成本远高于采购 |
| **备份存储** | 自建 `pg_dump` → S3 | 简单、无依赖、完全可管控 |
| **多区域流量路由** | 采购（Cloudflare / AWS Global Accelerator） | 全球流量路由 + DDoS 防护 + 边缘计算是已有商业 CDN 做得很好的领域，自建全局负载均衡器成本高 |

### 4.3 「不引入」的明确决策

- **不引入邮件 SaaS**（方向 A）：SendGrid/Mailgun 在邮件量 < 200/秒时成本高于自建 SMTP 直发 + 退信处理。
- **不引入前端框架**（方向 B）：React 的 JSX 编译 + 虚拟 DOM diff 对于纯聊天 UI 的收益不如自定义组件基类 + `<template>` 的渲染性能。
- **不引入 APM/SIEM**（方向 D）：OpenTelemetry 已有，不需要 Datadog APM。
- **不引入全局 ID 生成服务**（方向 E）：现有 Ulid 已经支持跨区域单调。

---

## 5. 实施路线图

### 优先级排序（修正版）

| 优先级 | 方向 | 修正理由 |
|--------|------|---------|
| **P0** | 方向 C：Webhook 签名 | 2-3 天可完成，安全准入基线，平台正式 launch 前必须完成 |
| **P0** | 方向 D（子集）：备份脚本 + SQL 健康检查 | 1-2 天可完成的事故预防措施，不影响其他方向的并行开发 |
| **P1** | 方向 A（子集）：邮件通知通道 | 邮件是方向 B 的上游依赖——在 SPA 还不好用的时候邮件是唯一可靠的通知渠道 |
| **P1** | 方向 B：前端工程化 | 可与方向 A 并行启动，但前端的收益需要至少 L2 阶段才能体现 |
| **P2** | 方向 A（剩余）：入站邮件网关 + 每工作区 SMTP | 两个高体量功能，依赖邮件通知通道就绪 |
| **P2** | 方向 D（剩余）：灾难恢复 runbook + 容量规划 | 持续的运维投入 |
| **P3** | 方向 E：多区域架构 | 只做文档 + 数据模型预留，不做实现 |

### 阶段划分

```
Phase 0（Week 1）——「止血」
├── 方向 C：Webhook 签名实现 + 集成测试
├── 方向 D：make backup 脚本 + docker-compose 备份 sidecar
├── 方向 D：run_db_health_check 定时器
└── 方向 E：产出一份 multi-region.md 架构文档

Phase 1（Week 2-4）——「找到用户」
├── 方向 A（子集）：minijinja 集成 + 邮件模板重构（3 个模板）
├── 方向 A（子集）：notif_prefs Email variant + email_queue 表 + run_email_dispatcher
├── 方向 A（子集）：List-Unsubscribe 头 + 一键退订
└── 方向 B（L1）：引入 Vite + 模块拆分 + CSS 变量系统

Phase 2（Week 5-8）——「产品化」
├── 方向 A（剩余）：每工作区 SMTP 配置
├── 方向 A（剩余）：每日/每周邮件摘要
├── 方向 B（L2）：自定义组件基类 + 单向数据流 + hash 路由
├── 方向 B（L2）：i18n（locales/zh.json + en.json）
└── 方向 D：灾难恢复 runbook + 事故严重性定义

Phase 3（Week 9-12）——「全球化准备」
├── 方向 A：入站邮件网关（Reply-by-Email）
├── 方向 B（L3）：虚拟列表 + 暗色模式 + a11y
├── 方向 D：备份恢复验证 CI job + 容量规划模型
└── 方向 E（预留）：data_residency 列在 messages/participants/workspaces 表添加（migration，非实现）
```

### 风险矩阵

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| 前端 L1 引入 Vite 后 CI 构建时间过长 | 中 | 中 | 缓存 `node_modules` + Vite 增量构建，首屏构建 <10s |
| 邮件队列积压导致 SMTP 背压 | 低 | 中 | `run_email_dispatcher` 每 tick 限批量 ≤50，单个 worker 异步发送 |
| 入站邮件网关被用于 spam | 低 | **高** | DKIM 验证 + 速率限制 + 路由 token 一次性使用 |
| PG 逻辑复制的 DDL 限制影响消息分区 | 中 | **高** | 在同一区间隔内不做分区迁移和逻辑复制同时操作，排期错开 |
| 团队无前端人员，Vite 迁移困难 | **高** | **高** | L1 阶段只做构建工具引入（不重构代码），降低技能门槛；考虑外聘前端专家 |

### 资源分配建议

```
Week 1-4:
  Rust 后端 2 人: 方向 A（邮件）+ 方向 C（Webhook 签名）
  运维 1 人: 方向 D（备份 + SQL 健康检查）
  架构师 0.5 人: 方向 E（multi-region.md 文档）
  （前端的 Vite 引入可以由后端工程师在维护窗口执行——Vite 不要求 JS 框架知识）

Week 5-8:
  Rust 后端 1 人: 方向 A（每工作区 SMTP + 摘要邮件）
  前端 1 人（引入）: 方向 B（组件化 + i18n + 路由）
  运维 1 人: 方向 D（runbook + 事故定义）

Week 9-12:
  Rust 后端 1 人: 方向 A（入站网关）
  前端 1 人: 方向 B（虚拟列表 + 暗色模式 + a11y）
  全体: 方向 D（容量规划 + 备份验证 CI）
```

---

## 结论

五个方向中最重要的**架构决策**不是「先做哪个」，而是：

1. **邮件是前端的替代品而非补充品**。在 SPA 还不好用的阶段，邮件是唯一能触达用户的渠道。P0 先做邮件通知通道，P1 再做前端工程化——这个顺序不改变，用户留存率就会在过渡期间受损。

2. **Webhook 签名是安全基线，不是可选增强**。2-3 天可以完成的方向应该排在最前面——它不需要等待任何依赖，做完了直接降低平台安全风险。

3. **多区域不是现在要做的，但现在要开始防**。`data_residency` 列可以等到 Phase 3 再加，但 `multi-region.md` 架构文档应该在 Week 1 就产出——否则未来 3 个月的每个 schema 变更都可能造成事后无法修复的多区域不兼容。

4. **前端工程化的风险最高但回报最高**。当前 5.9K 纯 JS SPA 在 15K 行时就会彻底不可维护——必须在 10K 行之前开始引入 Vite + 模块化。Phase 1 的 L1（Vite 只构建不重构）是降低风险的关键决策。

5. **生产运维是持续投入而非一次项目**。备份脚本可以一周内写完，但恢复验证 + 容量规划 + 事故响应流程是组织能力建设，需要持续 2-3 个月才能真正就绪。
