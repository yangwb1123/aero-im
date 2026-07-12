Now I have enough context to provide a thorough architectural analysis. Let me synthesize everything.

---

# 架构分析报告：Aero IM 五个高价值企业级扩展方向

> **分析日期**: 2026-07-12  
> **分析文档**: `docs/requirements/2026-07-10-five-truly-uncovered-high-value-enterprise-directions.md`  
> **源码基线**: 16 crate / ~35k Rust / 120+ 路由模块  
> **验证结果**: 全部五点代码证据经实际源码交叉验证——确认文档描述准确，无一虚假断言

---

## 1. 架构评估

### 1.1 当前架构的核心优势

分析文档揭示了一个隐蔽但重要的系统特性：**数据层的超前设计**。多处基础数据（`audit_events` 表自迁移 0007、`message_receipts` 表自迁移 0077）在早期就被埋入，并且数据结构设计清晰（ULID 时间可排序、`detail JSONB`、每日 RANGE 分区、法务保全豁免）。这说明架构师在设计之初就考虑了可审计性和可扩展性——这在早期创业项目中相当罕见。

从 `routes.rs` 的 `2854` 行路由装配可以看出系统架构的另一优势：**高度模块化的 `.merge()` 模式**。每个能力域被封装在独立模块（`crate::workspaces::routes()`、`crate::polls::routes()` 等），路由、鉴权、仓储三层的分离清晰。这为扩展新方向提供了很好的基础——新模块只需实现 `pub fn routes() -> Router<AppState>` 并在 `build()` 中 `merge`。

NATS 事件总线（`aero-bus` crate）的 DAG 设计（durable vs ephemeral consumer、per-subject seq、at-least-once 语义）是另一个正确决策——它为方向一（邮件网关的事件发布）和方向二（已读聚合的事件驱动）提供了现成的投递基础设施。

### 1.2 关键架构债务

分析文档揭示了一个系统性的架构债务：**数据层与展示层之间的断层**。

```
有数据 → 有仓储 → 无路由 → 无前端
───────────────────────────────
audit_events (0007) → AuditRepo (完备)  →  路由为零  →  UI 为零
message_receipts (0077) → MessageReceiptRepo (完备)  →  路由为零  →  UI 为零
```

这不是偶发——它是可预防的架构治理问题。每次添加新表时，应该形成强制性的 Checklist：存储层 → HTTP 路由（至少只读 GET）→ 展示层。当前流程缺少这个强制检查点。

第二个架构债务在 `AGENTS.md` §4.2 已提及但分析文档未深入：**枚举 tag 的 `kind` 字段撞名陷阱与 SOA 粘连**。方向三（OAuth Provider）如果引入新的 token 类型，可能与现有的 `revoked_tokens` 表（全局封禁）和 PAT 的 `scope:*` 产生概念冲突——这是另一个需要预清理的架构债。

第三个债务是**单一全局 SMTP 配置**（`mailer.rs` 第 16 行注释明确标注 `future concern`）。方向一（邮件网关）的企业部署场景必然要求每工作区独立 SMTP 配置——这是方向一的技术前提，也是 mailer 的架构债。

### 1.3 关键设计决策评估

| 现有决策 | 评价 | 与五个方向的关系 |
|---------|------|----------------|
| `audit_events` 每日 RANGE 分区 | ✅ 正确——为方向四的百万级查询准备 | 方向四直接受益 |
| `message_receipts` ON CONFLICT DO NOTHING 幂等 | ✅ 正确——为方向二的聚合引擎准备 | 方向二依赖此设计 |
| mailer 无队列、纯文本 | ⚠️ 合理但需重构——方向一要求异步 MTA | 方向一迫使 mailer 升级 |
| PAT 无 scope 粒度 | ⚠️ 短期合理——但方向三要求 scope 系统 | 方向三需要全新的授权模型 |
| `assert_room_access(participant, room)` 签名 | ✅ 正确——方向五的批量操作可复用 | 方向五需要"一次校验，批量执行" |

---

## 2. 扩展方向深度分析

### 2.1 方向一：邮件到频道网关（Inbound Email Gateway）—— P1

#### 为什么需要

**业务价值**：这是替换 Slack/MSTeams 的"不可绕过"的门槛能力。企业采购协作平台时，"能与客户/供应商通过邮件协作"经常是必选功能而非加分项。当前 Aero IM 只有一个单向的 mailer，无法接收邮件——这意味着外部人员必须注册 Aero IM 才能参与讨论，这在跨组织场景中是一个无法接受的门槛。

**技术价值**：邮件网关提供了系统里最核心的"协议桥接"模式（SMTP ↔ NATS RoomEvent），这个模式可复用于方向三（OAuth 的邮件验证码）和其他非 IM 协议（SMS/传真）的导入。

#### 核心挑战

**挑战 1：SMTP 是一个状态密集的协议**（HELO/EHLO → MAIL FROM → RCPT TO → DATA → QUIT）。使用 `mailin-embedded` 可以简化，但邮件大小限制（`DATA` 阶段）、超时处理（RFC 5321 的 SMTP 超时）、退信（Bounce）的生产级处理是一个被低估的复杂度。当前系统没有任何异步消息队列（`mailer.rs` 注释明确 "No queue"），但入站 SMTP 天生就是异步的。

**挑战 2：MIME 解析的边界情况**（7-bit/8-bit 编码、`Content-Transfer-Encoding` 的 base64/quoted-printable、多语言字符集、嵌套 MIME 的 `multipart/alternative` 中取哪个部分、`cid:` 内嵌图片的 blob 映射）——这不是一个周末项目。`mail-parser` crate 可以处理大部分，但附件去重（SHA256）和大小限制需要额外的管线。

**挑战 3：身份解析的双向映射**——内部参与者通过邮箱地址匹配，外部发件人需要创建"虚拟参与者"（Guest 类型），这个虚拟参与者的生命周期管理（什么时候清理？外部发件人能否自行回复？能否被 @提及？）需要新的参与者模型扩展。

#### 架构变更

```
当前：
  external → (no path) → ❌

目标：
  external email → MX → SMTP server (tokio :25)
                      → MIME parser (mail-parser)
                      → channel_email_routes 路由表
                      → 参与者映射 (内部/外部)
                      → BlobStore 附件上传
                      → ImService::publish_room_event
                      → (同时) 外发回复 SMTP 回执
```

#### 对现有系统的影响

- **`aero-server/src/mailer.rs`**：需要一个队列层（当前同步发送）。最简单的方案是 tokio `mpsc` 通道 + 单 consumer worker，不需要完整的 job queue。
- **`aero-storage/src/participant.rs`**：可能需要新的 `participant_kind = 'email_guest'`，带 `external_email` 字段。
- **`aero-common/src/model/`**：新的 `RoomEvent::EmailReply` variant（如果希望区分邮件来源的回复）。
- **NATS**：不需要新 consumer——直接走现有 `im.room.*` durable consumer，邮件来源的消息通过 `source: "email"` 元数据标记。
- **限流**：按发送者域每小时 100 封（分析文档建议）需要在 SMTP 阶段做，不是 `publish_room_event` 阶段。

### 2.2 方向二：Communication Intelligence & Read Analytics—— P1

#### 为什么需要

**业务价值**：这是五个方向中 ROI 最高的——数据已有的前提下，投入产出比最优。企业管理者会将"消息阅读率""团队响应速度"作为协作效率的 KPI。没有这些指标，IM 平台对企业采购决策者而言是一个"黑箱"。

**技术价值**：这是一次"将 OLTP 数据转化为 OLAP 洞察"的架构范式验证。如果这个模式在 `message_receipts` 上验证成功，同样的模式可以复用于 `stream_chat`（弹幕参与度）、`call`（通话时长分析）、`poll`（投票参与率）。

#### 核心挑战

**挑战 1：增量聚合的一致性**。`message_receipts` 是高频写入表（每次客户端窗口聚焦或滚动都触发 `mark_read`），如果聚合引擎直接扫描原始表，在百万级 `message_receipts` 行上做 `COUNT(*) GROUP BY message_id` 是不可接受的。必须走增量更新路径：`insert_receipt` → PG 的 `NOTIFY` / Redis pub/sub → 聚合 worker 更新 `message_read_stats`。

**挑战 2："目标读者"的定义**。`target_readers`（消息可读成员数）的计算不是简单的房间成员数——需要考虑 Bot/Agent 参与者（不计入）、被禁言成员（不计入）、消息发送者（不计入）、消息发送时不在房间的成员（不计入？）。这是一个业务逻辑问题，需要明确的设计决策。

**挑战 3：响应 SLA 的时区/节假日处理**。方向二的阶段 D（响应 SLA 追踪）中，"非工作时间不计入 SLA"听起来简单，但需要支持工作区级别的时区配置 + 节假日日历 + 用户个人的 OOO 状态。这是一个远比表面看起来复杂的工程。

#### 架构变更

```
当前：
  message_receipts (OLTP) → 直接查询

目标：
  message_receipts (OLTP) → 增量聚合 worker (background)
                           → message_read_stats (OLAP 预计算)
                           → message_read_trends (daily 聚合)
                           → REST API (GET /api/workspaces/:id/analytics/…)
                           → SPA 仪表板
```

#### 对现有系统的影响

- **无新 crate**：聚合 worker 是 `aero-server` 内的一个 `tokio::spawn`，与现有 `bin/boot/background.rs` 模式相同。
- **新增表**：`message_read_stats`（per-message 聚合）、`room_read_trends`（每日快照）——需要 2 次迁移。
- **鉴权**：管理者仪表板的路由复用 `usage_report.rs` 的 `member_role(Owner/Admin)` 守卫模式。
- **性能**：聚合 worker 间隔 30s（非实时），增量更新用 Redis pub/sub 触发——与现有 `participant_cache` 的失效模式相同。

### 2.3 方向三：OAuth 2.0 / OIDC Provider—— P2

#### 为什么需要

**业务价值**：这是从"应用"到"平台"的质变。OAuth Provider 是 API 第一性原理的实现——没有标准的认证授权协议，第三方开发者就永远只能通过"自带 PAT 的机制"集成，而这在企业合规审计中是不可接受的。同时，OAuth Provider 意味着 Aero IM 的用户身份可以被其他企业应用消费（SSO 的反向场景），这是生态扩展的核心能力。

**技术价值**：当前 `AuthUser` extractor 的授权模型是二元的（有效/无效 JWT），没有 scope 粒度。OAuth 的 scope 系统为整个 API 层引入了**细粒度的、可声明的、可审计的**授权模型。这不仅是方向三本身的价值，也是方向四（审计日志的 scope 追踪）和方向二的（分析 API 的 scope 守卫）的基础设施。

#### 核心挑战

**挑战 1：授权码流程的状态管理**。标准 OAuth 2.0 Authorization Code Flow + PKCE 需要服务端维护 short-lived 的 `code` 状态（通常在 Redis 中，5 分钟 TTL）。已消费的 `code` 必须即时失效（`ON CONFLICT DO NOTHING` 模式的幂等守卫）。同时，refresh token rotation 需要每次刷新返回新的 refresh token 并使旧 token 失效——这是一个经典但容易被忽略的边界情况。

**挑战 2：scope 系统的侵入性改造**。当前 `AuthUser` extractor 被用于 120+ 路由模块。如果要逐路由标注 scope 需求（`#[scope("message:read")]`），这是一个全仓范围的改造，涉及 `aero-server/src/routes/` 的每个模块。替代方案是分层迁移：先在 OAuth token 端点层面校验 scope，路由层的 scope 守卫作为第二阶段渐进式引入。

**挑战 3：Client 凭证的安全存储**。`client_secret` 只在创建时展示一次（与 PAT 相同模式），但 BCrypt hash 在 OAuth client 管理中是必要的——不同于 PAT 的固定 bearer token，OAuth client 需要支持多种认证方式（client_secret_basic、client_secret_post、none for public clients）。

#### 架构变更

```
当前：
  AuthUser extractor → JWT or PAT → 二元授权

目标：
  OAuth 2.0 AS ─→ Authorization Code + PKCE flow
               ├→ Client Credentials (机器身份)
               ├→ Refresh Token Rotation
               └→ JWKS / OIDC Discovery

  路由层：#[scope("message:read")] middleware
  PAT 兼容：现有 PAT → scope:*
```

#### 对现有系统的影响

- **新 crate？不**——`aero-auth` 已有 JWT 签名、OIDC consumer 代码，OAuth Provider 是同一 crate 的扩展。
- **新端点**：`/oauth/authorize`、`/oauth/token`、`/oauth/revoke`、`/oauth/jwks.json`——这些是未受 `AuthUser` extractor 保护的公开端点（需要通过 client_id + client_secret 或 redirect_uri 验证）。
- **scop 标注**：激进方案 = 全仓 Rust 属性宏（`#[scope("ai:ask")]`），保守方案 = 新路由使用 `ScopeGuard` 中间件 + 旧路由渐进迁移。
- **PAT 兼容**：现有 PAT 默认 `scope:*`，新 PAT 支持 `scope` 字段指定。

### 2.4 方向四：审计日志可视化与合规仪表板—— P2

#### 为什么需要

**业务价值**：这是五个方向中"数据准备度最高的"——`audit_events` 表从第一天开始就持续记录，分区分好、法务保全豁免接好、CSV 导出写好、sweep 逻辑就绪，唯独没有 HTTP 路由和 Web UI。这是"数据价值未释放"的最典型例子。

**技术价值**：审计日志可视化是合规审计（SOC 2、ISO 27001、GDPR）的硬性要求。没有这个能力，任何合规审计都不会通过——审计员不会接受"请用 psql 查询"作为审计手段。

#### 核心挑战

**挑战 1：审计事件的 PII 脱敏**。`detail JSONB` 中可能包含 IP 地址、邮箱地址等 PII。不同权限的管理员看到的细节应该不同（工作区 Owner 看到完整信息，Auditor 角色看到脱敏信息）。这需要 `audit.rs` 的 `list` 方法支持可选的脱敏策略。

**挑战 2：大范围导出的性能**。`export_csv()` 方法已经存在（分析文档确认），但百万级事件的 CSV 导出不能是同步响应——需要走异步导出作业队列（与 `conversation_export` 或 `workspace_export` 模式相同）。

**挑战 3：搜索性能**。`audit_events` 的索引是 `(workspace_id, created_at)` 复合索引 + `action` 独立索引（猜测，需确认）。如果用户需要按 `actor_id` 或 `target` 搜索，查询计划可能不同。方向四的搜索功能需要确认现有索引足够，或新增 `(workspace_id, action)` 覆盖索引。

#### 架构变更

```
当前：
  audit_events → AuditRepo → 无路由 → ❌ UI

目标：
  audit_events → AuditRepo → GET /api/workspaces/:id/audit-log (增强)
                             GET /api/workspaces/:id/audit-log/export (CSV)
                             → audit.js SPA 仪表板
                             → legal-holds 管理 UI
```

#### 对现有系统的影响

- **最小影响**：`AuditRepo` 已经存在，只需要在 `routes.rs` 中添加 `.merge(crate::audit::routes())`——但 `crate::audit` 模块可能不存在，需要在 `aero-server/src/` 新建。
- **新依赖**：无——纯后端路由复用现有 `audit.rs`。
- **前端**：需要一个新的管理页面组件（`audit.js`），独立于现有的"debug client"风格 UI。

### 2.5 方向五：批量操作与模板引擎—— P2

#### 为什么需要

**业务价值**：这是"企业规模化管理"的刚性需求。500 个频道的工作区，管理员需要批量归档/批量成员管理/频道模板。当前逐个操作的 O(N) 时间成本在大型组织中是管理瓶颈。

**技术价值**：批量操作迫使系统解决"事务性逐条处理 + 汇总错误"的问题——这不是简单的 `DELETE FROM messages WHERE id = ANY($1)`，而是每条操作的独立审计、独立错误处理、独立幂等性。这个模式可复用于其他批量场景。

#### 核心挑战

**挑战 1：批量操作的失败语义**。不像单资源 CRUD 的全部成功/全部失败原子性，批量操作需要"逐条处理、汇总错误"的语义——50 条删除中 48 条成功、2 条因权限不足失败。这要求审计事件也合并为单条（`"messages.batch_deleted"` 而不是 50 条 `"message.deleted"`），但失败的 2 条需要单独记录原因。

**挑战 2：模板引擎的变量替换**。`{project}` 变量替换听起来简单，但模板中引用的 user group（`@team-leads`）可能在工作区中不存在——需要 fail-open 语义（跳过不存在的引用）。同时，模板的版本管理（创建后修改 vs 不可变模板）是一个设计选择。

**挑战 3：客户端多选交互**。当前 Web SPA 没有任何多选模式——Shift+点击、长按进入多选模式、全选/反选、批量确认对话框——这是纯前端的复杂度，且需要与现有消息列表/频道列表的虚拟滚动兼容。

#### 架构变更

```
当前：
  单条 CRUD × 120+ 路由

目标：
  POST /api/rooms/:id/messages/batch-delete  → 事务性逐条处理
  POST /api/rooms/:id/members/batch-add      → 上限 200
  POST /api/workspaces/:id/rooms/batch-archive → 批量
  POST /api/workspaces/:id/channels/from-template → 模板实例化
  plus 多选交互（纯前端）
```

#### 对现有系统的影响

- **审计合并**：批量操作的审计事件不能逐条触发——需要修改 `AuditRepo::append` 或增加 `append_batch` 方法，支持单次审计事件记录 N 条操作。
- **`assert_room_access` 一次性校验**：批量操作需要先校验操作者对**所有**目标资源的权限（一次查询），而不是逐条校验（N 次查询）。可能需要增加 `assert_room_access_batch(participant, room_ids)` 方法。
- **模板存储**：`template_data JSONB` 的 schema 设计需要稳定——模板中是否包含"预期创建后还要手动调整的"元素（如自定义权限），还是模板是固定结构。

---

## 3. 接口设计建议

### 3.1 批量操作 API 的接口契约

批量操作的 API 设计应该遵循以下原则，这些原则可供方向五（以及其他方向需要批量模式时）共用：

```
请求:  { items: [T], options?: BulkOptions }
响应:  { succeeded: [{ id, result }], failed: [{ id, error }], summary: { total, ok, fail } }
```

- **不透明失败优先**：永远返回 `{succeeded, failed}` 二分结构，而不是 HTTP 207 Multi-Status（对 Axum 客户端不够友好）。
- **上限硬约束**：请求体中的数组必须明确标注上限（`max_items = 100`），超限 422 拒绝。
- **一次性鉴权**：批量操作在 handler 入口执行一次权限校验，不在循环中逐条校验。
- **合并审计**：1 次批量操作 = 1 条 `"messages.batch_deleted"` 审计事件（detail 中包含 count），而不是 N 条 `"message.deleted"`。

### 3.2 增量聚合引擎的通用接口

方向二（Communication Intelligence）的增量聚合引擎可以抽象为一个通用模式，可复用于：

- 弹幕参与度聚合（`stream_chat`）
- 通话时长聚合（`call`）
- 投票参与率聚合（`poll`）
- 消息热点检测（`message_search` 的冷热数据分离）

通用接口建议：

```
Trait IncrementalAggregator<Event, Summary> {
    /// 收到一条新事件 → 更新聚合
    async fn on_event(&self, event: Event) -> Result<(), Error>;
    /// 查询当前聚合
    async fn summary(&self, key: &str) -> Result<Summary, Error>;
    /// 定时刷新（将内存中的增量写回 PG）
    async fn flush(&self) -> Result<(), Error>;
}
```

这种抽象允许聚合引擎在内存中缓存增量、定期 flush 到 `message_read_stats` 表，避免每次 `mark_read` 都写 `message_read_stats`。

### 3.3 OAuth Provider 的 scope 系统设计

OAuth scope 系统需要覆盖三个层面的授权粒度：

| Scope 层级 | 示例 | 守卫位置 |
|-----------|------|---------|
| **资源级** | `message:read` `message:write` `ai:ask` | 路由层 middleware |
| **管理级** | `admin:workspace` `admin:audit` `admin:oauth` | 路由层 + handler |
| **用户级** | `openid` `profile` `email` | OIDC UserInfo endpoint |

Scope 的设计原则：

- **Hierarchical naming**：`message:*` 匹配所有 message 子 scope（但不等于 `*`，防止滥用）
- **Scope downgrade**：OAuth client 不能请求比注册时更大的 scope（`allowed_scopes` 超集校验）
- **PAT 兼容**：现有 PAT 绑定 `scope:*`，新 PAT 支持 `scope` 列表

### 3.4 邮件网关的插件式协议桥接

方向一的邮件网关应该设计为一个"协议桥接"模式，而不仅仅是 SMTP 处理：

```
trait InboundMessageGateway {
    /// 将外部消息转换为内部 RoomEvent
    async fn receive(&self, source: ExternalMessage) -> Result<RoomEvent, Error>;
    /// 将内部回复转换回外部消息格式
    async fn reply(&self, reply: InternalReply) -> Result<ExternalMessage, Error>;
}
```

这个 trait 允许未来添加其他协议桥接（SMS、传真、Slack Import Bridge）而不改变核心管线。

---

## 4. 技术选型

### 4.1 方向一：邮件网关

| 组件 | 候选 | 推荐 | 理由 |
|------|------|------|------|
| SMTP MTA | `mailin-embedded` / `mailin` / 自建 `tokio::net::TcpListener` | `mailin-embedded` | 嵌入式、tokio 集成友好、RFC 5321 兼容，不需要外部 MTA 进程 |
| MIME 解析 | `mail-parser` / `mime` / `mime_guess` | `mail-parser` | 支持 MIME 多部分、字符集转码、附件提取，Rust 生态中最成熟的邮件解析库 |
| SPF/DKIM | `trust-dns`(SPF) / `dkim-rs` / 自建 | `dkim-rs` + 自建 SPF | DKIM 验证是安全关键路径，自建易错；SPF 可用 `trust-dns` 的 TXT 记录查询 |
| 字符集转码 | `encoding_rs` / `charset` | `encoding_rs` | Firefox 核心团队维护的字符集转码库，覆盖几乎所有需要的编码 |

**决策理由**：`mailin-embedded` + `mail-parser` 组合在 GitHub 上有可参考的集成示例（虽然较少），但它们的 API 设计匹配 Aero IM 的"嵌入式、无外部依赖"哲学。需要特别注意：`mailin-embedded` 的 email size 限制需要自行在 `DATA` 阶段实现。

### 4.2 方向二：聚合引擎

- **不需要引入新框架**——Postgres 的 `INSERT … ON CONFLICT` + `NOTIFY` 模式足以支撑增量聚合。
- **聚合表**使用 Postgres 原生的 `BIGINT` 和 `NUMERIC`，不需要 TimescaleDB 或 ClickHouse。
- **热力图数据**（方向二阶段 D 的 `hourly_heatmap`）可以使用 Postgres 的 `generate_series` + `date_trunc` 在查询时计算，不需要预计算（除非查询频率极高）。

**决策理由**：当前技术栈（Postgres + Redis + NATS）已经足够支撑方向二的所有子方向。引入新数据库（如 ClickHouse）在当前阶段是过度工程。

### 4.3 方向三：OAuth Provider

| 组件 | 候选 | 推荐 | 理由 |
|------|------|------|------|
| OAuth 框架 | `openidconnect` crate / 自建 JWT | **自建** | `openidconnect` 是 client SDK，不是 server 框架。OAuth authorization server 在 Rust 生态中没有成熟的全功能库。自建 JWT 签名已在 `aero-auth` 中实现 |
| 授权码状态 | Redis / PG | Redis（`SET key EX 300`） | Token 端点是高频路径，Redis 的低延迟（<1ms）优于 PG 的 ~5ms，且 TTL 自动过期减少清理工作 |
| Scope 注解 | 属性宏 / 中间件 + error | **中间件** | 属性宏（`#[scope("...")]`）需要在每个路由 handler 上方标注，是侵入式改造。`ScopeLayer` 中间件可以在路由树上按路径匹配，允许渐进式迁移——先 cover 新路由，旧路由逐步标注 |

**决策理由**：自建 OAuth authorization server 是必要的——Rust 生态没有像 Java Spring Security 那样成熟的 OAuth 2.0 server 框架。`aero-auth/src/jwt.rs` 已有 JWT `sign()` / `verify()` 方法，可以在此基础上构建。

### 4.4 方向四：审计可视化

- **无新增后端依赖**——`AuditRepo` 的 `list` 和 `export_csv` 方法已经存在。
- **前端**：不需要引入 React/Vue——当前 SPA 使用零依赖 ES2020 模式，审计日志页面可以沿用同一模式（`audit.js`）。
- **虚拟滚动**：如果审计日志行数超过 1000 行，需要考虑虚拟滚动。可以使用 Intersection Observer API 实现增量加载（无依赖），或在行数较多时引入 `virtual-scroller` Web Component。

**决策理由**：审计页面的交互复杂度较低（搜索 + 筛选 + 列表 + 导出），不值得引入框架。ES2020 模版字符串 + fetch API 足以支撑。

### 4.5 自建 vs 采购

| 方向 | 建议 | 理由 |
|------|------|------|
| 方向一：邮件网关 | ✅ **自建** | 邮件到频道的路由和身份映射是业务逻辑核心，没有现成的 SaaS 产品能同时做 SMTP 接收 + 频道映射 + 参与者身份解析 |
| 方向三：OAuth Provider | ✅ **自建** | 同上——OAuth Provider 需要深度集成到 `AuthUser` extractor、PAT 兼容、scope 守卫，第三方无法满足 |
| 方向二：分析仪表板 | 🤝 **前端自建，后端自建** | 聚合引擎是现有的 PG + Redis 模式，不需要外部 BI 工具。但仪表板的可视化（图表/热力图）可以考虑使用 `chart.js` CDN（当前 SPA 使用的零依赖模式不排斥 CDN 资源） |

---

## 5. 实施路线图

### 5.1 优先级排序与排期建议

```
优先级排序依据：企业价值 × 代码就绪度 / 投入成本

P0（立即启动）：
  方向二·阶段 A（已读聚合引擎）—— 数据已就绪，投入最低，产出最直接

P1（下一批并行）：
  方向一·阶段 A（基础入站 SMTP + 频道路由）
  方向四·阶段 A（审计日志 API 增强 + 基础 UI）

P2（功能丰富）：
  方向二·阶段 B/C/D（管理者 API + 已读 UI + SLA 追踪）
  方向一·阶段 B/C（附件 + 外发回复 + DKIM/SPF）

P3（平台级）：
  方向三·阶段 A/B（OAuth AS + Client 管理）
  方向五·阶段 A/B（批量操作 + 模板引擎）

P4（完善）：
  方向三·阶段 C/D（PKCE + OIDC Provider）
  方向五·阶段 C/D（批量 UI + 导出/归档）
```

### 5.2 阶段划分与里程碑

#### 里程碑 M1（4-6 周）——"数据第一次被看见"

**交付物**：

| 方向 | 交付 | 投入 | 关键风险 |
|------|------|------|---------|
| 方向二·A | 聚合引擎 + `message_read_stats` 表 + 增量 worker | ~800 Rust, 0 JS | `target_readers` 定义的分歧 |
| 方向四·A | `GET /api/workspaces/:id/audit-log` + 过滤 + 排序 | ~400 Rust, ~500 JS | 审计事件的 PII 脱敏策略 |

**成功标准**：工作区 Owner 可以打开一个"审计日志"页面，筛选操作类型和时间范围，看到可读的审计事件列表。

#### 里程碑 M2（8-12 周）——"邮件进来了"

**交付物**：

| 方向 | 交付 | 投入 | 关键风险 |
|------|------|------|---------|
| 方向一·A | SMTP 服务器 + MIME 解析 + 频道路由 + 参与者映射 | ~1200 Rust, 0 JS | MIME 编码边界情况 |
| 方向二·B | 管理者 REST API（GET /api/workspaces/:id/analytics/…） | ~500 Rust, 0 JS | 聚合缓存的 5 分钟 TTL 是否足够 |

**成功标准**：向 `project-abc@aero.company.com` 发送一封带附件的邮件 → 附件出现在频道的文件列表中，邮件正文作为消息显示。

#### 里程碑 M3（12-20 周）——"平台化起步"

**交付物**：

| 方向 | 交付 | 投入 | 关键风险 |
|------|------|------|---------|
| 方向三·A | OAuth Authorization Server + 授权码流程 + Token 端点 | ~1200 Rust, 0 JS | Refresh token rotation 的一致性 |
| 方向五·A | 批量 DELETE + 批量成员 ADD | ~600 Rust, ~300 JS | 批量审计事件的合并 |
| 方向四·B | 合规仪表板完善 + 法律保全 UI | ~300 Rust, ~300 JS | 无——代码就绪度最高 |

**成功标准**：第三方 bot 可以通过 OAuth Client Credentials flow 获取 scope 限定的 token → 调用 AI API。

### 5.3 风险点与缓解策略

| 风险 | 影响方向 | 概率 | 缓解策略 |
|------|---------|------|---------|
| MIME 解析的字符集边界情况导致邮件内容截断 | 方向一 | 中 | 阶段 A 使用 fail-open（记录日志 + 保留原始邮件体），阶段 C 再添加 character set recovery |
| `message_read_stats` 聚合与原始表不一致（由于增量 worker 崩溃） | 方向二 | 低 | 每晚全量 reconciliation job（source-of-truth 重置），增量 worker 使用 Redis pub/sub 的 at-least-once 语义 |
| OAuth 授权码被重放攻击 | 方向三 | 低 | `code` 是 128 位随机 + `SHA256(code_verifier)` PKCE 校验，已消费的 code 返回 `invalid_grant` |
| 批量操作中部分资源的权限在批量开始后变更（竞争条件） | 方向五 | 低 | 批量操作在事务中校验权限（`FOR UPDATE`），事务失败则全部回滚——虽然这是"全部成功或全部失败"不如"逐条处理"的语义，但可以接受（权限变更并发概率极低） |
| 方向四的审计日志搜索在百万级事件上性能退化 | 方向四 | 中 | 确保 `(workspace_id, created_at, action)` 复合索引就位。如果全文搜索（`detail JSONB`）成为瓶颈，可以在阶段 C 引入 GIN 索引 |
| 方向三的 scope 路由标注全仓改造冲突 | 方向三 | 高 | **缓解**：第一阶段不改造现有路由——OAuth token 只在 token 端点校验 scope，路由层 scope 守卫在第二阶段逐步引入。现有 `AuthUser` extractor 保持不变 |

### 5.4 跨方向的依赖关系

```
方向四（审计日志） ← 依赖 —— 方向三（OAuth）的 scope 系统

方向三（OAuth） ← 消费  —— 方向四（审计日志）的审计事件记录

方向一（邮件网关） ← 复用 —— 方向五（批量操作）的频道路由

方向二（聚合引擎） ← 可复用模式 —— 方向五（模板引擎）的变量替换

方向四（审计导出） ← 模式借鉴 —— 方向五（批量导出/归档）
```

**关键依赖路径**：方向三（OAuth）的前置条件是方向四（审计日志）的部分 API —— OAuth client 的创建/删除/轮换审计事件应该出现在审计日志中。但这不构成阻塞依赖——方向四的阶段 A（基础审计 API）可以在方向三的阶段 A 之前完成。

---

## 6. 总结

这五个方向的代码就绪度验证结果令人印象深刻——分析文档给出的每个断言都有对应的源码锚点，且所有断言都经得起交叉验证。这说明扫描方法论是严谨的。

**架构决策层面**，最值得关注的是：

1. **方向二的"增量聚合引擎"模式**应该被设计为通用基础设施，而不是 `message_read_stats` 的专属 worker。这样做可以避免后续每个分析场景都造一个不同的聚合轮子。

2. **方向三的 OAuth Provider** 的最大工程风险不是 JWT 签名或 token 端点实现，而是 `AuthUser` extractor 的 scope 标注改造。推荐**分层渐进策略**：先实现 OAuth 协议本身（不加 scope 守卫），再在第二阶段改造路由层。

3. **方向四的审计日志 API** 是五个方向中投入产出比最高的——数据已有、仓储已有、迁移已就绪，只需要 HTTP 路由和前端页面。建议作为 M1 的交付目标之一。

4. **方向一的邮件网关** 是企业采纳路径上不可绕过的能力，但工程复杂度被低估（MIME 解析、SPF/DKIM、退信处理）。建议阶段 A 专注于"邮件进来成消息"的最小可行路径，把 SPF/DKIM 验证和安全加固留给阶段 C。

5. **方向五的批量操作** 的核心贡献不是批量 DELETE API 本身，而是**确立"事务性逐条处理 + 汇总错误"的批量操作范式**。这个范式一旦确立，所有现有 CRUD 路由都可以在此基础上扩展批量变体。
