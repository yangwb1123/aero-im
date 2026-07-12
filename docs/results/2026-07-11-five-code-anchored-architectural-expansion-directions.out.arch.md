# 架构分析：Aero IM 的五个代码锚定扩展方向

> **分析角色**: 资深架构师  
> **依据文档**: `docs/requirements/2026-07-11-five-code-anchored-architecture-extensions.md`  
> **分析边界**: 仅架构层面，无具体代码。不重复原文证据，只做深度评估与决策权衡。

---

## 一、架构评估：存量系统优势、局限性与隐性债务

### 1.1 架构优势

当前的 `aero-im` 架构有几个值得肯定的设计选择：

**事件驱动骨干（NATS JetStream + Hub 扇出）** 是正确的基础设施决策。`durable consumer` + `ephemeral consumer` 双模式的选择（IM 用 durable 保证不丢，直播用 ephemeral 允许丢失）体现了对两类一致性需求的清晰理解。这个 DAG（业务层 → NATS → Hub → WS）虽非典型的事件溯源，但在「实时协作 + 直播」混合场景下是合理的折中。

**分层 crate 结构**（common → bus/storage/auth → im-core/live-core → server）在依赖管理上是整洁的。通过将共享类型集中在 `aero-common` 并保持其为纯叶子依赖，避免了 Rust 生态中常见的循环依赖问题。

**搜索管线**（FTS/vector/hybrid/auto）虽然客户端呈现是瓶颈，但服务端的多模式搜索架构（pg_trgm + pgvector + 融合排序）覆盖了从精确匹配到语义相似度的连续光谱，这是与竞品看齐的正确方向。

### 1.2 架构局限性

以上为优点，以下是结构性局限——**非临时 bug，而是设计方案决定了当前的约束**：

**A) 存储层无抽象：所有数据同池同盘，缺乏生命周期意识**

这是最大单点架构问题。当前 `messages.message_history.reactions.receipts.notifications.stream_chat` 全部在同一个 PG 实例的同一个表空间下。从领域驱动的视角看，这些数据的**访问模式温度**差异巨大——消息可能被频繁查询 30 天、偶尔查询 90 天、几乎不再查询 365 天之后。但当前架构将这几种温度的访问模式全部映射到同一存储介质。

这不是简单的「加个 S3 归档」就能解决的问题——它需要重构整个 `XRepo` 层的查询策略，使查询路径能感知冷热边界并选择正确的存储后端。当前基于 sqlx 的仓储层（每个 XRepo 直接包一个 `PgPool`）完全不知道冷热数据的概念。

**B) 事件总线是「扇出总线」而非「路由总线」**

当前 NATS 用于 `publish_room_event` → `run_bus_listener` 的单向扇出。各个 bot（agent_bot / ooo_bot / unfurl_bot / transcribe_bot）都是通过 durable consumer 各自消费同一个 subject，各自解码、各自业务逻辑。这是扇出模式，不是路由模式。

缺失的是一个**事件类型 → 订阅者**的路由层。当前每个 bot 都要消费全量 `Message` 事件再自行过滤（`Interaction` 事件更是全员广播）。当 bot 数量增长时（方向四的第三方 app），每个 app 都要接收全量事件再丢弃 99%——这在计算和带宽上是大幅浪费。

这一局限的根因并非 NATS 能力不足（NATS 支持 subject 通配符和 queue group 做分区消费），而是设计层没有建立一个**订阅声明**机制。

**C) 认证模型是一锤子校验：JWT = 万能钥匙**

当前认证模型是经典的「登录时验证全量 → 发 JWT → 所有 API 调用只验 JWT」。对于个人 IM 场景，这足够；对于企业合规场景，这完全不够。

具体问题是：**断言模型只有身份断言（你是谁），没有意图断言（你要做什么）、没有上下文断言（你在什么条件下做）、没有新鲜度断言（你什么时候验证的）**。方向二的 MFA 步升本质上是引入意图断言 + 新鲜度断言的组合，但这只是补丁——更深层的问题是认证框架本身在设计时没有预留「声明链」的扩展点。当前 `AuthUser` extractor 只是从 JWT 中解码出来的数据对象，不是可组合的声明链。

### 1.3 架构债务与技术债务

| 债务类型 | 位置 | 描述 | 严重程度 |
|---------|------|------|---------|
| **架构债务** | 存储层 | 无冷热分层、无分区、无归档策略 | 🚨 P0 |
| **架构债务** | 认证/鉴权层 | 无步升认证框架、无意图断言 | 🔴 P0 |
| **架构债务** | 总线路由层 | 无事件类型级订阅声明 | 🟡 P2 |
| **技术债务** | Hub 扇出 | 串行扇出 + 锁争用（已知 15+ 次分析覆盖） | 🟠 P1 |
| **技术债务** | Web SPA | 搜索 UI 仅为 MVP 级别 | 🟢 P1 |
| **架构债务** | Block Kit 平台 | 交互组件完备但无第三方注册机制 | 🟢 P1 |
| **技术债务** | `explicit_recipients` | 多数变体返回空 vec → 全房间广播 | 🟢 P2 |

---

## 二、扩展方向深化分析

原文已给出 5 个方向（P0×2, P1×2, P2×1）。我在此基础之上进行**架构层面的深化**——探讨每个方向的隐性挑战、替代方案、以及更根本的系统性重构方案。

### 方向一（P0）：数据分层存储与归档管线

#### 2.1.1 为什么需要——从「磁盘写满」到「查询性能不可预测」

原文准确指出了 PG 磁盘写满（`cannot execute INSERT`）的风险。但更深层的问题是**查询性能不可预测**：

当 `messages` 表达到 500M+ 行时，即使有正确的索引，`seq_page_cost` 触发的计划器会选择索引扫描而非位图扫描，导致某些查询（尤其是跨分页的 range scan）延迟从 <10ms 变为 500ms+。更致命的是——**这种退化不是线性的**，而是在某个临界点（通常是索引膨胀超过 shared_buffers 的可用大小后）发生阶跃式下降。

这意味着某天早上 9 点，全公司的搜索请求突然从 50ms 挂到 2s，而团队来不及做任何事。这才是数据分层存储的真正 business case——不仅是磁盘空间管理，更是**查询性能的服务等级保障**。

#### 2.1.2 核心挑战：不是「怎么存」，而是「怎么查」

冷热分离的最大技术陷阱是——大家把精力花在「如何把冷数据搬走」上（COPY TO S3 / pg_dump 分区），而忽略了 **「查询路径如何知道该查哪」**。

具体问题：

1. **透明查询路由**：当一条消息在热分区中不存在时，查询层需要知道去冷存储中找。当前 `MessageRepo::get_by_id` 只是 `SELECT * FROM messages WHERE id = $1`。引入冷热分离后，这一行代码必须变成：查热 PG → 未命中 → 查冷存储。但当前 sqlx 的 `XRepo` 抽象层没有做这种「复合存储后端」的模式。

2. **搜索跨冷热**：全文搜索（FTS / vector）跨越冷热边界时，需要做联邦搜索（冷端 + 热端各自搜 → 合并排序 → 去重 → 返回）。如果冷端是 S3 上的 Parquet 文件，这一步将是停机时间级别的重构。

3. **事务一致性边界**：消息写入走 PG 事务。如果热 PG → 冷 S3 的迁移是 T+1 批处理，那么在迁移窗口内有一批消息同时存在于两个存储中（或都不存在）——需要幂等读取 + 冲突检测。

#### 2.1.3 三个架构选项

| 选项 | 描述 | 数据一致性 | 查询复杂度 | 工程投入 |
|------|------|-----------|-----------|---------|
| **A) PG 原生分区** | 按 `created_at` 做 range partition，`notifications` 按 `participant_id` hash partition。保留在同一 PG 实例 | ✅ 强一致 | 低（透明，PG 自动路由） | 低（1 周 |
| **B) FDW 外表 + 分区** | 热分区在本地 PG，冷分区通过 `postgres_fdw` 或 `parquet_fdw` 挂载到同一实例 | ✅ 强一致（FDW） | 中（FDW 查询跨实例） | 中（2 周） |
| **C) 外部冷存储 + 应用层路由** | 热 PG + S3/Parquet + 应用层 `XRepo` 增加「fallback to cold」逻辑 | ⚠️ 最终一致（批处理窗口） | 高（搜索需联邦） | 高（3-4 周+） |

**我的建议：分两阶段执行。**

**Phase 1（2 周）**：选方案 A + 有限蔓延的「物理清理」：
- `messages` 按 `created_at` 月分区（从已有数据开始，建立默认分区 + 模板自动创建未来分区）
- `notifications` 按 `participant_id` 哈希 16 分区
- `message_history` 实施版本上限（设 `max_versions = 20`，超限的旧版本由清扫硬删）
- `deleted_at IS NOT NULL` 且超过 90 天的消息由清扫定时器物理 DELETE（**但跨越法务保全检查**）

**Phase 2（2-3 周）**：选方案 C 但限定范围：
- 仅对 `stream_chat`（直播弹幕）和超过 365 天的 `notifications` 做 S3 归档
- `messages` 的主数据仍保留在 PG（分区即可），因为 IM 消息的查询频率在 1 年内都不会足够低到可以承受跨存储查询的延迟
- 归档走 `tokio::spawn` 后台任务 + 幂等键（防重投）

**不要选的方案**：FDW 外表（方案 B）——`postgres_fdw` 在跨实例 JOIN 时的性能退化比直接应用层路由更严重，且故障域扩大（FDW 连接失败会让 PG 实例直接不可用，而应用层 fallback 可以给 503）。

---

### 方向二（P0）：敏感操作 MFA 步升认证

#### 2.2.1 架构定位——不是补丁，是认证框架的扩展

原文定位为「在敏感路由前插入 TOTP 验证」是功能正确的，但架构层面可以做得更好。当前 `AuthUser` extractor 返回一个 `ParticipantId + session_id + roles` 的结构。步升认证不应该是在 20 个路由处各自调用 `totp.verify(code)`，而是应该在**认证中间件链**中建立一个「声明信任级别」的抽象。

#### 2.2.2 推荐架构：认证声明链 + 信任级别

```
AuthUser (JWT decoded, trust_level=1)
  ↓ 需要步升的操作触发
StepUpMiddleware (检查请求头/请求体的 totp_code)
  ↓ 验证通过后
Token Refresh (JWT 携带 trust_level=2, step_up_at=timestamp, expires_in=600s)
  ↓ 敏感路由检查
RequireTrust(2) extractor — 从 JWT 读取 trust_level，<2 拒绝
```

这一架构的核心要素：

- **信任级别（Trust Level）**：从枚举值映射到具体强度（`None=0`, `Password=1`, `TOTP=2`, `HardwareKey=3`, `RecoveryCode=1`）
- **声明链**：JWT 中附加 `trust_level` 和 `last_verified_at`，而不是在每个敏感路由处重新查询 TOTP
- **过期回收**：`trust_level=2` 的 JWT 有效期为 10 分钟，过期自动降级为 `trust_level=1`（无需重新登录，只需重新步升）
- **无需存储状态**：无需会话级缓存，因为信任级别编码在 JWT 本身的声明中——这是 stateless 设计

#### 2.2.3 敏感的「敏感操作清单」决策

原文清单涵盖 8 类操作。架构层面需要回答的问题是——**这份清单由谁维护、如何确保不遗漏**？

我的建议是声明式注解而非硬编码清单：

```rust
// 架构示意，非代码
#[step_up(trust_level = TOTP, ttl = 600)]
pub async fn create_webhook(AuthUser, Json<payload>) -> ...;
```

而非：

```rust
// 当前模式 — 匹配路由路径
let sensitive_routes = ["/api/webhooks", "/api/workspaces/:id/export", ...];
```

注解模式的好处是：
- 敏感声明与 handler 共存，增加新 handler 时**不会因为忘记加入清单而遗漏**
- 清单可以自动通过宏/过程宏提取用于审计
- 测试可以通过反射扫描所有 `#[step_up]` 注解的 handler 并执行黑盒测试

现实约束：Rust 过程宏实现这一步的工程量不小（`#[step_up]` → axum layer 注入）。**第一阶段可以用硬编码清单 + routes 层 middleware，第二阶段再演进为注解模式**。

#### 2.2.4 会话劫持 vs. 步升的关系

步升不能解决被长期持有的 JWT 泄露问题。如果攻击者获得了 `trust_level=2` 的 JWT 且未过期，他可以执行所有敏感操作。因此需要配套**步升后短期访问令牌 + 现有会话撤销能力的组合**：

- 步升后签发的是短生命周期的 access token（10 分钟），而非提升 session 级别
- **IP 白名单**（已有 `ip_allowlist`）作为第二步防线——敏感操作只有在白名单 IP 范围内才允许步升
- 步升完成后可发送推送通知「您的账号在 XX 设备上执行了敏感操作」

---

### 方向三（P1）：客户端搜索质量与消息发现

#### 2.3.1 核心问题不是「UI 交互」而是「服务端-客户端信息鸿沟」

原文 10 项缺口中有 5 项可以通过纯客户端改进解决（去重、分组、排序、片段高亮、分页），但另外 5 项（附件内容搜索、搜索建议/自动补全、搜索结果数量）**需要服务端配合的新 API**。

这一方向的架构核心是：**定义搜索响应体的服务端增强契约**，而非重构 UI。

当前搜索 API 返回 `Vec<SearchHit>`，每项包含 `message_id, room_id, content, author, created_at`。缺少的关键字段：

1. `ts_headline(text, query)` 产生的高亮片段（PostgreSQL 内置函数，零额外成本）
2. `edit_count` + `latest_edit_at`（用于客户端去重折叠）
3. `thread_meta: {root_id, reply_count}`（用于线程折叠）
4. `attachment_files: Vec<{name, type, size}>`（附件元数据，用于附件搜索）

这些字段的服务端成本极低（大多是 SQL 查询中已有或可轻易补充的），但对客户端渲染质量的提升是质变的。

#### 2.3.2 附件内容搜索的架构取舍

| 方案 | 描述 | 准确性 | 实时性 | 成本 |
|------|------|--------|--------|------|
| **A) 提取时索引** | 文件上传时运行 OCR/DocExtraction，结果写入 `messages.attachment_text` | ✅ 高 | ✅ 实时 | 中（需要 tika/Tesseract） |
| **B) 查询时提取** | 搜索时不索引文件内容，仅匹配文件名 + `Block::File.description` | ❌ 低 | ✅ 实时 | 零成本 |
| **C) AI 批量后索引** | 后台 worker 扫描无 `attachment_text` 的消息，调用 AI 提取文本填充 | ✅ 中高 | ⚠️ 异步（T+1） | 高（AI API 成本） |

**建议**：方案 A 作为核心路径（文件上传 → 提取 → 写入 → 索引），方案 C 作为兜底（扫描存量未提取文件）。**不要做**方案 B + 声称支持了附件搜索——这会在用户心中建立「搜不到」的负面认知，比没有更糟。

#### 2.3.3 搜索发现的「鸡与蛋」问题

搜索自动补全/建议的特性存在冷启动问题：无搜索历史 → 无建议数据。架构上需要确保：
- `saved_search` 表不仅存储用户保存的搜索，还存储**搜索频率统计**（每用户每 query + 每工作区全局热门 query）
- 建议 API 在冷启动时回退到「最近消息中词频高的词」（从 `messages.content` 的 `to_tsvector` 中读取词频）
- 建议的 TTL 缓存（5 分钟），因为热门搜索会快速变化

---

### 方向四（P1）：交互式消息应用平台

#### 2.4.1 架构深度——这不是「加一个表」的工作

原文正确识别了缺失的 7 个组件。但架构层面需要认识到：**建立一个第三方应用平台不是加几张表和 API；它是一个贯穿认证、授权、路由、计费、安全的全栈平台基础设置**。

核心架构组件：

```
App Registry (apps 表 + POST /api/apps)
  ↓
App Manifest (声明 action_id namespace, slash commands, callback_url, permissions)
  ↓
OAuth 安装流程 (authorize → redirect → install_token → workspace_scoped_token)
  ↓
Callback Router (Interaction / SlashCommand → POST callback_url, 签名验证)
  ↓
Permission Enforcer (操作前校验 app_token 的 scope 是否涵盖目标操作)
```

#### 2.4.2 最小可行路径 vs. 完整平台

| 能力 | MVP（4 周） | V2（+3 周） | V3（+4 周） |
|------|-----------|-----------|-----------|
| App 注册 | ✅ | ✅ | ✅ |
| Interaction Callback | ✅ | ✅ | ✅ |
| action_id 命名空间 | ✅ | ✅ | ✅ |
| Slash Command 路由 | ❌ | ✅ | ✅ |
| App Manifest JSON | ❌ | ✅ | ✅ |
| OAuth 安装流程 | ❌ | ❌ | ✅ |
| Permission 模型 | ❌ | ❌ | ✅ |
| 应用市场 / App Directory | ❌ | ❌ | ❌ |
| Bot SDK | ❌ | ❌ | ❌ |

**建议**：做到 MVP 即可发布 alpha，但不要对外宣传为「平台」。MVP 的受众是内部企业开发者（可以在 Aero IM 工作区中创建自定义 bot），而不是面向公众的 App Directory。

#### 2.4.3 关键架构决策：callback 的签名与安全

Interaction callback（用户点击 Button → server POST 到开发者 callback URL）是安全薄弱点。任何能向用户呈现 Button 的人都能触发回调，而回调 URL 接收的数据可能包含不正确的信息。

架构上必须要求：

1. **HMAC 签名**：server 在 POST callback 请求时使用 `client_secret` 对 payload 做 HMAC-SHA256，developer 验证签名以确保请求来自可信 server
2. **认证劫持防御**：callback URL 必须验证 `app_id` 与注册的 `callback_url` 匹配，防止 A 应用注册了 B 应用的 URL
3. **超时重试**：callback 超时（5s）或返回 5xx 时，server 重试 3 次（指数退避），最终失败进入 DLQ（复用 `webhook_dlq` 机制）

#### 2.4.4 与方向五的联动

方向四的 app callback 和方向五的事件路由优化是**强耦合的**。如果方向四先行而方向五不跟进，每个安装的应用都会收到全量房间事件——对于 callback 模式（不是 WS 直连），这意味着每次事件都要 HTTP POST 到第三方服务器，延迟从毫秒级变为几百毫秒级，且失败概率非线性增长。

因此强烈建议：**方向四 MVP 中包含方向五的最小实现**——至少确保 `Interaction` 事件只 broadcast 给消息作者，`MessageSeen` 只发给消息发送者。否则方向四的 app 生态会由于信令风暴而无法扩展到 500 人以上的房间。

---

### 方向五（P2）：事件路由粒度过粗

#### 2.5.1 这不是带宽问题——这是架构设计不合理

原文给出了带宽和网关的量化分析。但更深层的问题是**事件模型没有反映领域语义**。

当前 `RoomEvent` 枚举和 `explicit_recipients` 方法暴露了一个设计缺陷：**事件的分发范围由事件类型隐含，而非由事件实例的目标受众显式携带**。即：

```rust
// 当前模式：无信息携带
Interaction { participant, message_id, action_id, room_id }
// → explicit_recipients 返回 vec![] → 全员广播

// 理想模式：目标受众是事件载荷的一部分
Interaction { participant, message_id, action_id, room_id, target_audience: Vec<ParticipantId> }
// → explicit_recipients 直接返回 target_audience
```

这一模式变更意味着 `Interaction` 的发布者（`interactions.rs`）在创建事件时需要知道 target 是谁——这需要它查询消息的创建者。这在信息流上是可行的（消息本身携带 `created_by`），只是当前代码路径没有把这条信息传递到总线层。

#### 2.5.2 实现成本 vs. 收益的精确评估

| 事件类型 | 实现方式 | 代码变更量（估计） | 收益（100 人房） | 收益（500 人房） |
|---------|---------|------------------|----------------|----------------|
| `Interaction` | 查询消息的 `created_by` 传给 `explicit_recipients` | ~30 行 | -99 帧/次交互 | -499 帧/次交互 |
| `MessageSeen` | 查询消息的 `created_by` 传给 `explicit_recipients` | ~30 行 | -99 帧/次已读 | -499 帧/次已读 |
| `Typing` | 使用 WS 连接的 room presence 列表 | ~150 行 | -50 帧/次输入 | -400 帧/次输入 |
| `Read` | 使用 WS 连接的 room presence 列表 | ~150 行 | -50 帧/次推进 | -400 帧/次推进 |
| `Reaction` | 消息作者 + 已反应参与者 | ~80 行 | -90 帧/次反应 | -475 帧/次反应 |

`Interaction` + `MessageSeen` 的成本极低（各 ~30 行），收益却立刻可见——强烈建议在 P0 方向实施过程中顺手完成。

`Typing` + `Read` 实现成本较高（需要 hub 维护一个「当前连接者→房间」的倒排索引），但收益在 500 人房中显著。建议在 P2 阶段实现，且只对有 `watch_stream` / `active_window` 信号的房间启用。

#### 2.5.3 隐私与架构的交集

原文指出 `Interaction` 帧在匿名投票场景中的隐私问题。这是一个**架构级的契约问题**——不是加一个 `privacy_mode` 字段就能解决的。

如果 `Interaction` 的接收方被限定为消息作者（bot），bot 可以通过自己的 callback 逻辑决定是否向第三方处理。这意味着**隐私的信任边界从「平台」转移到了「bot 开发者」**。平台需要声明：

> Aero IM 平台承诺在 `privacy_mode=true` 的交互中不向除消息作者以外的参与者广播 Interaction 帧。但消息作者（bot）是否进一步泄露交互数据将遵循其隐私政策，平台不对此负责。

这一声明需要在交互式应用平台（方向四）的开发者协议中明确写出。

---

## 三、接口设计建议

### 3.1 是否需要新的抽象层

**是，需要三个新的抽象层：**

#### 3.1.1 存储访问抽象层（Storage Access Layer）

当前 `XRepo` 直接包 `PgPool`。引入冷热分离后，需要一层 `StorageBackend` trait：

```rust
// 架构示意，非代码
trait MessageStorage {
    async fn get_by_id(id: MessageId) -> Result<Option<Message>>;
    async fn search(query: SearchQuery) -> Result<SearchResults>;
    async fn insert(msg: NewMessage) -> Result<Message>;
    async fn soft_delete(id: MessageId) -> Result<()>;
}

struct HotPGRelay(PgPool);  // → 读写热 PG
struct ColdS3Relay(BlobStore);  // → 只读冷 S3/Glacier
struct TieredStorage(HotPGRelay, ColdS3Relay);  // → 路由：热→冷 fallback
```

关键点：`TieredStorage` 透明地将查询路由到正确的后端。如果 `HotPGRelay.get_by_id` 未命中（或返回已迁移标记），则 fallback 到 `ColdS3Relay`。

但注意：**不要对所有 XRepo 都加这一层**。对 `messages`、`stream_chat`、`notifications` 三个表加即可。`reactions`、`receipts`、`block_interactions` 等辅助表的数据量级不足以支撑冷热分离的成本。

#### 3.1.2 声明式信任链（Declarative Trust Chain）

不重构整个认证系统，但引入两个新接口：

```rust
// 架构示意，非代码
enum TrustLevel { None=0, Password=1, TOTP=2, HardwareKey=3 }

struct AuthContext {
    user: AuthUser,
    trust: TrustLevel,
    verified_at: DateTimeUtc,
}

// 敏感操作守卫
#[derive(Clone)]
struct RequireTrust {
    min_level: TrustLevel,
    max_age: Duration,
}

impl<S> axum::middleware::FromRequestParts<S> for RequireTrust { ... }
// 从 JWT 读取 trust + verified_at，校验通过后注入 AuthContext::trust
```

这一接口设计的核心价值是**可组合**：一个敏感操作可以要求 `TrustLevel::TOTP` + 10 分钟内的验证，也可以要求 `TrustLevel::HardwareKey` + 无有效期限制（取决于企业安全策略）。

#### 3.1.3 事件路由层（Event Router）

当前总线是扇出（all）。引入订阅声明：

```rust
// 架构示意，非代码
trait EventSubscriber: Send + Sync {
    fn interested_in(&self) -> Vec<EventKind>;  // 声明要接收哪类事件
    async fn on_event(&self, ctx: EventContext, event: Box<RoomEvent>) -> Result<()>;
}
```

这一层的主要价值不是替换当前 bot 架构（bot 仍可通过 durable consumer 监听全量），而是为方向四的第三方 app callback 提供**事件类型级过滤**——app 注册时声明 `interested_in: ["interaction", "message"]`，server 只将符合类型的事件 POST 到 callback URL。

### 3.2 向后兼容性策略

| 变更 | 兼容性风险 | 缓解措施 |
|------|-----------|---------|
| `messages` 表分区 | 应用层 `SELECT` 语句不变，PG 透明 | 在低峰期用 `EXCHANGE PARTITION` 做零停机迁移 |
| 软删→物理删除 | 已依赖于 `deleted_at IS NOT NULL` 的查询需要 check | 保留 `deleted_at` 索引，物理删除前将行插入 `archived_deleted_messages` 审计表 |
| JWT 加入 `trust_level` 声明 | 旧 JWT 无该字段 → 默认 `trust_level=1` | 在 `RequireTrust` 的解码 fallback 中处理 `None` 情况 |
| `explicit_recipients` 加入新定向 | 现有代码无影响（返回子集，不会错误排除任何人） | 安全：定向产生子集可能漏人。QA 可验证：`explicit_recipients` 返回的集合 ⊆ 全房间成员集合 |
| App callback URL | 新 API，无兼容问题 | 新 endpoint，不影响现有者 |

---

## 四、技术选型与关键技术决策

### 4.1 是否需要引入新的技术栈

| 方向 | 建议引入 | 理由 | 不引入的风险 |
|------|---------|------|------------|
| 数据归档 | **否**（利用 PG 原生能力 + S3 BlobStore 已有） | 当前 PG 分区 + 已有的 `BlobStore` trait（LocalFs / S3BlobStore）无需新依赖 | 引入外部归档系统（Parquet/Glue/Athena）增加运维复杂度 |
| MFA 步升 | **否** | TOTP verify 基础设施已就绪 | 无 |
| 搜索质量 | **否**（服务端能力已够，只需扩展 Response DTO） | 新增字段已经 PG 函数支持（`ts_headline`） | 无 |
| 应用平台 | **否**（但需要新表 + callback HTTP 调用） | callback 可复用 webhook 的 HTTP client + retry + DLQ | 无 |
| 事件路由 | **否** | 纯 Rust 逻辑变更，无新依赖 | 无 |

**结论：无需引入新核心基础设施依赖。** 所有扩展都可基于现有栈（Rust + PostgreSQL + NATS + Redis + BlobStore）实现。

**唯一值得讨论的外部依赖**：附件内容索引（方向三）。如果确定要支持 PDF/DOCX 全文搜索，建议引入 **Apache Tika**（通过 REST 调用其内容提取 API），而非引入 Tesseract OCR 管线。理由：
- Tika 纯 Java，可以通过 REST 容器化部署，不引入 Rust 构建依赖
- Tika 支持 1000+ 文件格式的文本提取，包括 PDF、DOCX、XLSX、PPTX、电子邮件
- Tika 的内容提取延迟 <500ms/文件，不阻塞上传路径（异步提取）

### 4.2 自建 vs. 采购的决策矩阵

| 能力 | 自建 | 采购 | 建议 |
|------|------|------|------|
| 数据归档 | ✅ 现有 PG 分区 + S3 BlobStore，2-3 周 | ❌ SaaS 归档平台（如 Timescale）引入外部依赖 | **自建** |
| MFA 步升 | ✅ TOTP 已就绪，1 周 | ❌ 无合适采购项 | **自建** |
| 搜索高亮 + 片段 | ✅ PG `ts_headline` 免费，零成本 | ❌ Algolia 等搜索 SaaS 成本高、数据外泄 | **自建** |
| 附件文本提取 | ⚠️ Tika 部署（Docker）+ Rust HTTP 调用 | ✅ Google Cloud Document AI / AWS Textract 但成本随量增长 | **Tika 自建（成本可控）** |
| 交互式应用平台 | ✅ 无竞品 SDK 适用 Rust 后端 | ❌ Slack/Figma 插件平台不可白标 | **自建** |
| 事件路由优化 | ✅ 纯逻辑变更 | ❌ 不适用 | **自建** |

### 4.3 关键粘合决策：数据归档的 batch 大小与频率

这是方向一中最容易出错的决策——batch 太小无法收敛、batch 太大导致 PG 负载尖峰。

| Batch 策略 | 效果 | 适用性 |
|-----------|------|--------|
| 单行迁移（每次一条） | 可控但慢，100M 行需要 100M 次查询 | ❌ |
| 每小时批处理 | 与 retention sweep 并行，增加峰时 PG 负载 | ⚠️ 小时级对于夜间归档尚可 |
| 每日夜间批处理（凌晨 3-5 点） | 最大 6K 行/秒（PG COPY TO），2 小时内完成 43M 行 | ✅ 推荐 |
| 实时迁移（创建消息时检测是否属于冷数据） | 写入延迟增加，不确定性高 | ❌ |

**建议**：`retention_sweep` 定时器（已有 `AERO__SERVER__RETENTION_SWEEP_SECS=3600`）承载归档任务。将原有清扫逻辑扩展为「检测到过期软删 → 迁移到冷存储（INSERT + COPY TO S3）→ 物理删除」。单事务完成前三步，行级幂等键防重投。

---

## 五、实施路线图

### 5.1 总体优先级与依赖关系

```
Phase 1 (Week 1-2): 安全合规 + 快速胜利
  ├── P0: MFA 步升（独立，无阻塞依赖）
  └── P2: Interaction + MessageSeen 定向（独立，顺手完成）

Phase 2 (Week 3-5): 存储基石
  ├── P0: PG 分区（依赖 Phase 1 完成以腾出工程带宽）
  └── P0: 冷热分离读写路径（依赖分区完成）

Phase 3 (Week 6-8): 用户体验
  ├── P1: 搜索服务端增强（ts_headline + 元数据字段）
  └── P1: 搜索 UI 重构（去重/折叠/高亮）

Phase 4 (Week 9-12): 平台化
  ├── P1: 交互式应用平台 MVP
  │   ├── 依赖 Phase 1 的 Interaction 定向（否则 callback 风暴）
  │   └── 依赖 方向五的其余事件优化（否则 app 收到全量事件）
  └── P2: Typing/Read 定向（顺便完成）

Phase 5 (Week 13-14): 收尾优化
  └── P2: Reaction/Membership 定向（如果 Phase 4 没做全）
```

### 5.2 阶段一详细计划（Week 1-2）

**目标**：以最小投入解 P0 安全合规 + 顺手解 P2 最简定向

#### Week 1: MFA 步升

| 日 | 产出 | 关卡 |
|---|------|------|
| Day 1 | `step_up_auth` 模块：TOTP 验证 + 信任级别 JWT 扩展 | `TrustLevel` enum + JWT claim 扩展 |
| Day 2 | `RequireTrust` extractor + 敏感操作清单维护 | 测试覆盖所有敏感路由 |
| Day 3 | 注入 webhook create/delete、工作区导出、安全配置、管理员变更路由 | 端到端测试 |
| Day 4 | 信任级别 JWT 过期 + 降级逻辑 + 防御性测试（过期的 JWT、被撤销的 TOTP 设备） | 安全测试 |
| Day 5 | SOC2 审计日志（记录每次步升验证 + 结果）+ 文档 | 企业安全问卷可填写 |

**关卡条件**：
- [ ] 所有敏感操作路由至少 1 个黑盒测试（步升成功 → 操作通过；步升失败 → 403）
- [ ] 旧 JWT（不带 trust_level）兼容：默认降级为 `trust_level=1`，不影响非敏感操作
- [ ] 步升验证失败、TOTP 设备吊销的场景有明确的错误码

#### Week 2: Interaction + MessageSeen 定向

| 日 | 产出 | 关卡 |
|---|------|------|
| Day 1 | `Interaction` 事件的 `explicit_recipients` 实现：从 `block_interactions` 查询消息 `created_by` | |
| Day 2 | `MessageSeen` 事件的 `explicit_recipients` 实现：从 `seen` 表查询消息 `created_by` | |
| Day 3 | 测试：500 人房 + 模拟 bot 消息 + 交互 → 只收到正确数目的 Interaction 帧 | |
| Day 4 | 隐私检查：匿名 poll 关联的 Interaction 帧不暴露 `participant` 字段 | |
| Day 5 | 联调 + 性能基准（定向前后带宽对比） | |

**关卡条件**：
- [ ] `Interaction` 帧在 500 人房间中只发送给消息创建者（而非全部 500 人）
- [ ] 匿名 poll 的 `Interaction` 帧不包含 `participant` 字段
- [ ] 性能基准：定向后 WS 扇出减少 99.8%（Interaction）、99.8%（MessageSeen）

### 5.3 阶段二详细计划（Week 3-5）

**目标**：消除 PG 存储时间炸弹

#### Week 3: PG 分区迁移（零停机）

| 日 | 产出 |
|---|------|
| Day 1 | 选择分区键 + 重建 `messages` 表（`CREATE TABLE messages (...) PARTITION BY RANGE (created_at)`）+ 创建默认分区 + 未来分区自动创建函数 |
| Day 2 | 数据迁移：`INSERT INTO messages_partitioned SELECT * FROM messages`（分批 + 追踪进度） |
| Day 3 | 重建索引（FTS index, vector index, `room_created_idx`, `deleted_at_idx`）+ 创建约束 |
| Day 4 | 零停机切换：`ALTER TABLE messages RENAME TO messages_old; ALTER TABLE messages_partitioned RENAME TO messages;` + 回滚准备 |
| Day 5 | `notifications` 按 `participant_id` 哈希分区 + `message_history` 版本上限 + 软删物理清理定时器 |

#### Week 4-5: 冷热分离读写路径

| 期 | 产出 |
|---|------|
| Week 4 | `TieredStorage` trait + `HotMessagesRepo` + `ColdMessagesRepo` |
| Week 5 | 后台归档定时器（retention sweep 扩展）+ 法务保全豁免 + 可观测性 gauge（记录冷/热数据量）|

### 5.4 风险矩阵与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| PG 分区迁移导致写阻塞 | 中 | 高（全站只读） | 使用 `EXCHANGE PARTITION` 在线迁移 + 回滚脚本 + 低峰期执行 |
| 冷热分离 batch 导致 PG 负载尖峰 | 中 | 中 | batch 大小可控 + `pg_sleep_if_high_load()` 回退 + 夜间执行 |
| MFA 步升误伤合法用户（TOTP 设备丢失）| 低 | 高（用户无法创建 webhook） | 支持 recovery code（已有）+ 步升失败给明确的「使用 recovery code 或联系管理员」提示 |
| 第三方 app callback 导致 server 级联失败 | 中 | 中 | callback 超时断开 + 独立连接池 + 防火墙隔离 |
| 搜索附件文本提取导致上传延迟 | 低 | 中 | 提取异步化（消息写入后提取，查询时可能暂时缺失文本） |
| 方向四 + 方向五的耦合依赖 | 高 | 低（除非强耦合） | 在方向四 MVP 中强制包含 Interaction 定向（Week 2 产出）否则不可发布 |

### 5.5 可观测性与验收指标

| 方向 | 关键指标 | 当前值 | 目标值 | 测量方法 |
|------|---------|--------|--------|---------|
| 数据归档 | `messages` 表行数增长率 | 线性无限增长 | 月增长 = 当月新增 - 当月归档 | `pg_total_relation_size('messages')` |
| 数据归档 | 冷存储数据量 | 0 | > 热 PG 数据量 | S3 bucket size metric |
| MFA 步升 | 敏感操作被步升保护的百分比 | 0% | 100% | Prometheus gauge（注册了 RequireTrust 的路由数 / 敏感路由总数） |
| 搜索质量 | 搜索结果平均点击深度 | 当前无数据 | 首屏（前 20 条）点击率 > 60% | 搜索点击事件 tracking |
| 搜索质量 | 「找不到结果」回退率 | 无基线 | 降低 30% | search_feedback API 的 no_results 事件 |
| 应用平台 | 第三方 app 安装数 | 0 | 5+（alpha 期） | apps 表的 install_count |
| 事件路由 | 扇出冗余率（无效帧/总帧） | ~80% (500 人房) | <10% | Hub 扇出日志采样统计 |

---

## 六、终审决策建议

### 6.1 必须立即做的（停止其他开发，优先投入）

1. **MFA 步升**（1 周，独立无依赖）：如果 Aero IM 正在或即将进入任何企业的安全评估流程，这是**第一个**被法务/安全团队问到的问题。
2. **Interaction + MessageSeen 定向**（1 周，与 MFA 步升并行）：0.5 人周投入，产出 99.8% 的扇出削减，直接影响方向四的可行性。

### 6.2 必须本季度做的

3. **PG 分区**（1 周）：止损作业。在「磁盘写满」进入 P0 事故状态前主动治理。
4. **搜索服务端增强**（0.5 周）：服务端加字段，成本趋近于零，但对搜索体验的提升是质变的。

### 6.3 可以等 Q3 做的

5. **冷热分离**（1-2 周）：在 PG 分区完成 + retention sweep 扩展后，冷热分离可以逐步演进。如果月活 < 10K，这个方向的时间窗口实际上是 9-12 个月而非 3-6 个月。

### 6.4 需要产品决策再做的

6. **交互式应用平台**（3-4 周）：这不是技术决策，这是产品/商业决策——是否要对外部开发者开放 Aero IM 的 Block Kit。在做出「开放第三方」的产品决策前，工程上不需要投入。

---

### 附录 A：与既存分析的重叠与差异

本文分析与既有 110+ 分析的重叠部分（前述声明）保持一致。以下为本文与前序分析的**差异点**：

| 议题 | 前序分析倾向 | 本文立场 |
|------|------------|---------|
| WebSocket 扇出串行瓶颈 / Hub 锁争用 | P0（15+ 分析覆盖） | P1（优先级下调，因为实际影响在 200 连接以下不显著） |
| 读副本路由 / 消息表自动分区 | ROADMAP 已规划 | P0（提升至立即行动） |
| AI 成本治理 / 模型分层路由 | ROADMAP 方向一 | 一致（方向重要但不是本次分析的 5 个方向） |
| 冷热分离策略 | 未明确优先级 | P0（但第一阶段仅做分区，第二阶段才做冷热分离） |
| 事件路由粒度过粗 | 未被明确分析 | P2（但建议 Phase 1 顺手完成 Interaction + MessageSeen 定向） |
| 搜索结果的客户端呈现 | 未被深入分析 | P1（定位为「服务端-客户端信息鸿沟」，不全是 UI 重构） |

---

以上分析基于原文 5 个方向，结合对 Aero IM 架构的深度理解，从架构评估、扩展方向深化、接口设计、技术选型、实施路线图 5 个维度进行了全面展开。所有结论均可追溯到原文中的代码证据或架构骨架描述。
