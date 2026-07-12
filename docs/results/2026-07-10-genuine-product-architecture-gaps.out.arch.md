现在我已经掌握了充分的代码上下文，可以撰写我的架构分析了。

---

# 架构分析：五方向审查的元评估与技术深化

> **范围**：针对 `docs/requirements/2026-07-11-five-unanalyzed-expansion-directions.md` 及其审阅意见的架构级再分析。
> **视角**：系统架构师，专注跨方向权衡、系统约束、接口契约和工程分期。
> **所有行号引用均为源代码锚点，不在此处硬编码。**

---

## 1. 架构评估

### 1.1 现有架构的优势

体现在审阅文档的**代码证据表**的精确性里：

| 优势领域 | 代码证据 | 架构含义 |
|---------|---------|---------|
| **REST 路由 `routes.rs` 一处装配** | `routes.rs::build()` 使用 `.merge()` 链挂载所有子模块 | 良好的关注点分离；每条路由两个文件（`routes.rs` + handler） |
| **服务端 AI 能力完整** | `smart_replies.rs`、`translate.rs`、`ai_rewrite.rs` 在 `lib.rs` 中注册且挂载到 `routes.rs` | 后端提供 10+ AI API 端点，SPA 仅消费其中 2 个 |
| **消息存储已具备分区基础** | `migration 0148` 创建 `messages_partitioned` + 后备填充函数 + `ensure_messages_partitions()` | 存储架构已为数据生命周期管理做好前置准备 |
| **管理 API 虽散落但完整** | `webhook_admin.rs`、`sessions.rs`、`legal_holds.rs`、`audit/mod.rs` 等 | 所有管理操作都可通过 REST 调用；缺口在 UI 层，不在 API 层 |

### 1.2 关键设计缺陷（审阅未充分讨论的）

审阅文档正确识别了表面缺陷。以下是我透过表层的分析：

**缺陷 A：无形式化客户端数据流协议**

当前 `app.js` 中 `state.messagesByRoom` 是**全局可变对象 + 隐式变异**。来自 `ws.js` 和 `api.js` 的写入没有经过统一的 `ingest()` 方法：

```
ws.on('msg:message', (m) => {
  if (!state.messagesByRoom[m.room_id])
    state.messagesByRoom[m.room_id] = [];
  if (!state.messagesByRoom[m.room_id].some(x => x.id === m.id))
    state.messagesByRoom[m.room_id].push(m);
});

// 在 app.js 的其他地方：
let msgs = await api.listMessages(roomId);
state.messagesByRoom[roomId] = msgs;  // ← 覆盖
```

这不是一个小 bug——这是一种**数据流模式**：WS 是迭加的，REST 是替换的，且两者之间没有仲裁层。审阅文档将 `RoomStore` 作为解决方案提出，这是正确的，但应该将其视为**客户端架构模式**，而非一个实用类。

**缺陷 B：无客户端查询模型抽象**

当前 app.js 将消息按房 ID 存储在平面映射中。查询模式（"获取我所有未读消息"、"查找来自用户 X 的最后一条消息"、"获取该线程的消息"）通过**O(n) 数组遍历**逐次实现。不存在索引视图（按 `room_id + sender_id`、`room_id + thread_id`、`room_id + is_unread`）。对于 < 100 条消息来说可以接受，但大房间（1000+ 条消息）的性能将不可预测。

**缺陷 C：管理 API 前缀碎片化**

审阅文档注意到管理路由散落在不同模块中，但没有深入探讨这意味着什么。目前：

```rust
// routes.rs 中的路由（概念性，非字面引用）：
.merge(crate::webhook_admin::routes())     // /api/webhooks/*
.merge(crate::sessions::routes())          // /api/sessions
.merge(crate::admin_sessions::routes())    // /api/admin/sessions
.merge(crate::legal_holds::routes())       // /api/legal-holds/*
.merge(crate::audit::routes())             // /api/audit
```

有些前缀是 `/api/`，有些是 `/api/admin/`。身份验证检查内联在各自 handler 中（有些调用 `assert_admin`，有些调用 `member_role`），没有统一的 `admin_middleware`。这意味着添加新的管理端点时，开发者必须自己记住做身份验证——这是可维护性的隐患。

**缺陷 D：迁移 0148 作为资产 vs 负债**

`messages_partitioned` 影子表 + `backfill_messages_partition()` 是超前的工程：它们在需要之前就具备了分区能力。但**未启用**状态本身就是债务：

- 影子表增加了 PG 对象计数（有效负载为零）
- `ensure_messages_partitions()` 函数存在于数据库中但未被调用
- 热路径（`INSERT INTO messages`）完全绕过分区基础设施
- 随着 `messages` 表增长，回填操作变得越来越昂贵

影子表方法本身是正确的（它避免了原地迁移的停机时间），但在备份期间其存在价值不应被视为主流——它应该要么被激活，要么被移除。

### 1.3 现有边界检查修正

审阅文档对 `AGENTS.md` 中的约束做了很好的工作。其**一致结论（零新外部依赖、增量无损、SPA 免打包工具、WS 协议不变）** 是合理的设计约束。但有一条约束我没有看到被充分质疑：

> **"方向二的管理 API 重构不改变已发布的 API 签名"**

这应该是**方向二的阶段 1**，而非永久约束。当前管理 API 散落在 `/api/`、`/api/admin/`、`/api/webhooks/` 下，前缀也不一致。永久保持这种不一致会锁定一个设计缺陷。更好的方法是在 `V2` 前缀下提供一致的路由，保留旧路由的弃用 shim，六个月后淘汰。

---

## 2. 扩展方向（另有视角）

审阅文档从产品 + 架构角度列出了五个方向。以下是我重新分组的三个架构性扩展方向，产品边界不同：

### 方向 A（审阅的 1+3+4）：客户端架构重组——状态管理、交互、AI 接入

**为什么需要**：三个审阅方向（REST/WS 一致性、消息交互、AI 接入）都归结为同一件事：**SPA 缺乏结构化状态管理层**。逐个修复这三个方向意味着在 `app.js` 中打三个互不相连的补丁。修复状态层可以一次性解决所有问题。

**核心挑战**：
- 在没有构建工具的前提下，对 5.9K 行 SPA 进行**增量重构**——`RoomStore` 不能一次性替代所有内容
- 保留现有 API 形状，同时引入新抽象
- AI 驱动的 UI 元素（智能回复、翻译、重写）需要**无感知的性能配置**——不能在每次悬停/点击时阻塞

**预计的架构变更**：
```
当前：app.js (global state) + ws.js + api.js → 互相直接修改 state
预计：RoomStore (app.js 中，单一方法) + 自 WS/API 调用的 ingest()
      + MessageIndex (按字段索引)
      + AIFeatureLayer (智能回复、翻译、重写的控制器)
```

**对现有系统的影响**：
- 低冲突剖面——`RoomStore` 可以包裹现有状态，随后逐步淘汰直接数组写入
- 保留 100% 的 WS 帧协议和 REST API 端点
- 但需要改变 `app.js` 中约 30 个直接引用 `state.messagesByRoom` 的位置

**与审阅中方案 A 的权衡**：审阅建议将 1 和 5 作为 P1，4 和 3 作为 P2。我主张 1（RoomStore）+ 4（AI 接入）应该合并为**方向 A Phase 1**，因为智能回复需要 RoomStore 的去重基础设施（否则 AI 建议可能重复施加）。

### 方向 B（审阅的 2 + 重述）：管理 API 标准化 + 角色模型

**为什么需要**：方向二正确地将管理 SPA 识别为 XL 规模。但我认为**API 标准化比 SPA 本身更重要且价值更高**。13+ 个管理 API 端点，前缀不一致、身份验证不规则、审计覆盖不完整——这首先是一个 API 架构缺口，其次才是 UI 缺口。

**核心挑战**：
- 路由前缀迁移（`/api/webhooks` → `/api/admin/webhooks`）需要进行端到端测试——不能中断现有集成
- 角色模型扩展（Owner/Admin → +Auditor/+Moderator/+SuperAdmin）触及每个管理 handler 的身份验证检查
- 与方向一的拮抗关系未在审阅中讨论：一旦 `RoomStore` 开始缓存 REST 响应，管理 API 的缓存失效必须遵循同样的模式

**预计的架构变更**：
```
当前：散落的 {webhook_admin, sessions, legal_holds, audit} 模块
预计：
  - admin/mod.rs：统一 admin_middleware + admin_audit_layer
  - admin/routes.rs：/api/admin/* 下的单一路由组
  - admin/roles.rs：角色位掩码枚举 + 权限检查器
  - 保留旧路由作为弃用 shim，为期 6 个月
```

**对现有系统的影响**：
- 向后兼容性 shim 增加短期维护开销
- 角色模型变化需要与方向二的 SPA 开发协调
- 较低的其他方向耦合风险

**与审阅中方案 B 的比较**：审阅称方向二为 XL 并建议推迟到 P2-5。我同意其规模，但主张 API 标准化部分（`admin/routes.rs`、中间件、角色模型）本身是 M 级，可以比 SPA 更早开始。

### 方向 C（审阅的 5 + 分开的 Phase 1/2）：从影子分区到运营的存储生命周期

**为什么需要**：审阅对 Phase 1（分区启用 + 物理删除）和 Phase 2（冷热分层 + S3）的拆分是正确的。但还缺少一个依赖项：**监控。** 在启用分区之前，运营团队需要能够看到当前表的增长、删除率以及分区裁剪是否生效。没有 `GET /api/admin/storage` 和相关仪表板，分区启用就是靠猜测。

**核心挑战**：
- `messages` → `messages_partitioned` 的切换窗口需要精确的协调（回填追赶 → 检查 → 重命名 → FK 重建）
- 物理删除管道（`DELETE FROM messages WHERE deleted_at < NOW() - interval '7 days'`）需要有复制安全的批量大小

**预计的架构变更**：
```
当前：messages（无分区）+ retention_sweep（仅设置 deleted_at）
预计：
  - 分区启用：重命名 messages → messages_old，messages_partitioned → messages
  - 物理删除：retention_sweep 中的新阶段（批量 DELETE + sleep）
  - 监控：存储端点 + 每个分区的 pg_total_relation_size 指标
  - 法务保全冷却：legal_hold_expiry → cooling_period → 可清理标志
```

**对现有系统的影响**：
- 高风险——分区切换涉及停机窗口或至少一次短暂锁
- 在该切换期间，所有 `INSERT INTO messages` 查询需要路由到新分区表
- 如果 7 个子表 FK 尚未重建，搜索索引可能临时不可用

**与审阅中方案 C 的比较**：我同意将方向五拆分为 Phase 1（优先级 P1）和 Phase 2（优先级 P2）的判断。但我增加了一个依赖：**Phase 1 需要 `GET /api/admin/storage` 作为前置条件**，否则"分区已启用"无法在运营上验证。

### 方向 D（未在审阅中）：可观察性基础——存储之外的仪表板

方向二提到了管理 SPA 应包含仪表板。但缺失的不仅仅是存储监控——审阅文档遗漏的还有：

**缺失的内容（代码证据）**：
- `GET /api/ai/usage` 存在但无客户端 UI
- `GET /api/admin/storage` 不存在（审阅正确）
- 但 `GET /metrics` 存在（用 bearer 门控）、`GET /health` 存在（就绪探测）
- 缺少可消费的仪表板端点：`GET /api/admin/dashboard` 返回 {key_metrics, recent_alerts, storage_growth, ai_usage_top, websocket_connections}

**方向 D 论点**：仪表板端点（将 5+ 运营查询聚合为一次调用）比 13 个独立的管理端点更早提供业务价值。成本低（~200 行 Rust）且可立即被运营团队使用——甚至在管理 SPA 建成之前。

**建议**：将方向 D 作为方向二的 Phase 0。首先构建聚合仪表板端点，将其连接到 CI 烟雾测试，**然后**构建 SPA：这将每个管理 API 端点的验收标准从"端点存在"转变为"端点在仪表板中可见"。

### 方向 E（未在审阅中）：事件 Schema 注册表与版本控制

审阅文档提到方向二的 webhook `generate_token` 和方向四的 AI API 端点。但有一个共同的架构缺口：**事件 Schema 没有版本控制。**

**缺失的内容（代码证据）**：
- `ws.js` 中的 WS 帧是结构化的，但缺少 `event_schema_version` 字段
- webhook 投递负载没有版本声明
- `common/src/model/` 中的 `RoomEvent` 和 `StreamEvent` 是通过 `#[serde(tag="kind")]` 序列化的——在 Rust 端是类型安全的，但消费方（webhook/SPA/第三方）看到的是没有版本承诺的 JSON
- 迁移 0150 增加了幂等的 webhook 投递，但**投递出去的 JSON schema 不能保证不同部署之间的稳定性**

**方向 E 论点**：**在 Schema 版本化之前，没有管理 API 是稳定的。** websub 端点、AI API 端点和 WS 帧各自定义了自己的 JSON 形状，但都没有声明"这是 v1，破坏性变更将提升为 v2"。在企业销售中，schema 稳定性是一个硬性要求。

**建议（最小可行方案）**：
1. 向 `RoomEvent` 和 `StreamEvent` 添加 `event_schema_version: 1` 字段
2. 为 webhook 负载添加 `Accept-Version` 头支持
3. 记录每个事件的稳定字段和弃用字段（markdown，位于 `docs/events/`）
4. 添加一个 `GET /api/admin/events/schemas` 端点，返回所有活跃事件 schema

---

## 3. 接口设计建议

### 3.1 关键接口

**接口 1：`RoomStore` 摄取契约**

```typescript
// 建议的 I/F 签名——非代码，而是契约
interface RoomStoreIngest {
  // WS 帧和 REST 回复的单一入口点
  ingest(roomId: string, source: 'ws' | 'rest', items: Message[]): void;
  
  // 保证：同一 roomId+messageId 的去重合并
  // 保证：ullid 排序（items 按 ullid 排序）
  // 保证：gap 检测——如果 ullid 空间出现间隙，设置 roomGaps[roomId] = true
}
```

**为什么是接口**：在同一个 API 下统一 REST 和 WS 通道。消除审阅中确定的重叠脆弱性。

**向后兼容性**：现有 `state.messagesByRoom[roomId].push(m)` 可以逐步迁移：先引入 `RoomStore`，然后一次覆盖一个 `app.js` 调用点。

---

**接口 2：管理 API 路由组**

```rust
// 建议的 Rust I/F——非代码，而是架构
// admin/routes.rs：
pub fn routes() -> Router<AppState> {
    Router::new()
        .nest("/api/admin", api_group())
        .layer(AdminAuditLayer)  // 自动化审计日志
        .layer(AdminRoleLayer)   // 统一角色检查
}

fn api_group() -> Router<AppState> {
    Router::new()
        .route("/sessions", get(list_sessions).post(revoke_session))
        .route("/webhooks", get(list_webhooks).post(create_webhook))
        .route("/legal-holds", get(list_holds).post(create_hold))
        .route("/storage", get(storage_metrics))  // 新
        .route("/dashboard", get(dashboard_aggregate))  // 新
        // ...
}
```

**退化 shim**：

```rust
// 为向后兼容保留旧路由
.merge(crate::webhook_admin::routes_legacy())
```

---

**接口 3：AI 功能层契约**

```typescript
// 建议的 AI 功能层 I/F——非代码，而是契约
interface AIFeature {
  kind: 'smartReply' | 'translate' | 'rewrite' | 'askContext';
  trigger: 'hover' | 'click' | 'composerAction' | 'rightClick';
  
  // 生命周期
  onActivate(context: AIContext): Promise<AIResult | null>;
  onDismiss(): void;
  
  // 约束
  debounceMs: number;        // 防止重复触发
  maxCacheAgeMs: number;     // 结果缓存
  roomCoolDownMs: number;    // 单个房间的限速
}
```

**为什么是接口**：方向四的 5 个场景（智能回复、翻译、重写、上下文提问、审核反馈）各自有相似的 API 调用模式（POST → 接收文本 → 显示/替换）。一个通用接口意味着场景 1（智能回复）和场景 2（翻译）可以共享限速和缓存逻辑，而无需各自独立实现。

---

### 3.2 新的抽象层

审阅文档暗示但未完全明确需要的两个抽象层：

**抽象层 A：客户端 `CacheStore`**

```typescript
// 概念：api.js 的包装器
class CacheStore {
  private cache: Map<string, { data: any; cachedAt: number }>;
  
  async fetch<T>(url: string, options?: {
    ttlMs?: number;
    staleWhileRevalidate?: boolean;
    invalidateKeys?: string[];
  }): Promise<T>;
}
```

与审阅的方向一关系密切。**审阅文档的补充**：缓存应位于 `api.js` 后面，这样现有代码（`api.listMessages()`、`api.listRooms()`）无需变更——`CacheStore` 替换的是 `request()` 函数体，而非其签名。

**抽象层 B：管理审计层**

```rust
// 概念：用于记录管理操作的 axum 中间件
pub struct AdminAuditLayer;

// 自动记录到 audit_events：
// (admin_id, action, target_type, target_id, request_body, timestamp)
```

审阅文档方向二中提到了但未详述。关键设计决策：**审计是在中间件层（对所有 `POST/PATCH/DELETE` /api/admin/* 操作自动生效）还是由各 handler 显式调用？** 我主张**中间件 + 白名单**：审计所有 `modify` 操作，跳过 `GET` 和 `list`。

---

### 3.3 向后兼容性策略

| 区域 | 策略 | 过渡期 |
|------|--------|----------|
| REST API 前缀 | 旧路由保留 6 个月作为弃用 shim | 6 个月 |
| WS 帧格式 | 不改变（零影响） | N/A |
| 客户端状态层 | RoomStore 并行运行，逐步迁移 | 无限制——逐步 |
| AI API 端点 | 不改变前端调用；在前端添加新调用 | 立即可用 |
| 分区启用 | 在线切换 window](停机窗口 | 单次事件 |

---

## 4. 技术选型

### 4.1 `零新外部依赖` 约束——是否应该维持？

审阅文档断言这是一个全局约束。我对此进行评估：

| 方向 | 新依赖？ | 合理性评估 |
|----------|-----------|------|
| 方向一：RoomStore | 无 | 正确——纯 JS |
| 方向二：管理 SPA | 无 | 正确——纯 ESM |
| 方向三：右键菜单 | 无 | 正确——纯 JS |
| 方向四：AI 集成 | 无 | 正确——仅使用现有 API |
| 方向五 Phase 1：分区 | 无 | 正确——纯 Rust + 迁移 |
| 方向五 Phase 2：冷层 S3 | **可选项：`parquet_fdw`** | 如果使用 `parquet_fdw`，则为新 PG 扩展；也可以使用 `postgres_fdw`（已经存在） |

**PG 扩展的评价**：

| 扩展 | 是否需要 | 理由 |
|----------|-----------|---------|
| `postgres_fdw` | 强烈推荐用于 Phase 2 | 内置（contrib），无安装——为冷层消息启用跨 PG 集群查询 |
| `parquet_fdw` | 可回避 | 将 Parquet 文件存储在 S3 上，通过 FDW 查询；但有 `COPY` 到临时表作为替代 |
| `pg_cron` | 不需要 | 当前定时器通过 Rust `tokio::spawn` 处理——不需要为此引入 PG 定时器 |

**结论**：审阅的约束在 Phase 1 各方向均可维持。Phase 2 的冷分层可能是第一个应将约束放宽以引入 pg 级工具的地方——但 PG FDW 已经 built-in。

### 4.2 自建 vs 购买的决策依据

不相关——所有方向都完全在现有技术栈内部。

### 4.3 我所观察到的技术栈缺口

审阅文档没有讨论的一个新依赖：**IndexedDB 用于方向一的客户端缓存。**

方向一的 RoomStore 目前是纯内存的。刷新后丢失。对于**缓存 REST 响应**（审阅的方向一 3），一个 `Map<string, {data, cachedAt}>` 在 F5 后丢失——这比没有缓存更糟糕，因为用户会看到陈旧数据，然后看到真实数据，然后看到陈旧数据。

**选项 A：纯内存缓存（F5 后丢失）**
- 实现最简单
- 在 90% 的页面重载中，JavaScript 模块将从内存中重新执行，且 `api.js` 的纯内存缓存消失
- 对当前行为无改进

**选项 B：`sessionStorage`（同一标签页内跨导航保留，跨标签页不保留）**
- `+` 满足 F5 恢复
- `+` 浏览器 API——无外部依赖
- `-` 5MB 存储限制
- `-` 同步访问

**选项 C：`IndexedDB`（不刷新丢失，跨标签页持久化）**
- `+` 自 F5 后实际保留
- `+` 异步、非阻塞
- `+` 大存储限额
- `-` 事务性 API 的样板代码——在当前免打包工具周期中需谨慎实现

**建议**：从**选项 A**（纯内存）开始，因为它是增量的，在 RoomStore 磨合后再加入**选项 C**。一个"P1 方向一"里程碑不应将 IndexedDB 作为硬依赖——这增加了交付风险，收益甚微。

---

## 5. 实施路线图（修订版）

### 5.1 优先级排序

我同意审阅的整体 PIP（分区第一阶段 > RoomStore > AI > 消息交互 > 管理 SPA > 分区第二阶段）。但我增加了一个**初始绿色阶段**：

| 层 | 阶段 | 阶段 | 阶段 |
|-----|-------|-------|-------|
| **P0.5**（先决条件） | 阶段 0：可观察性基础 |
| **P1** | 阶段 1a：分区第一阶段 + 物理删除 | 阶段 1b：RoomStore 基础（摄取 + 去重） |
| **P2** | 阶段 2a：AI 集成 | 阶段 2b：消息交互 | 阶段 2c：管理 API 标准化 |
| **P3** | 阶段 3：管理 SPA | 阶段 4：分区第二阶段（冷分层） |

### 5.2 阶段详情

**阶段 0：可观察性基础（~2-3 天）**

之前缺失。在一切之前：构建 `GET /api/admin/storage` 和 `GET /api/admin/dashboard`。

| 交付物 | 位置 | ~ 行数 |
|----------|--------|---------|
| `storage_metrics` handler | `server/src/admin/storage.rs` | 60 |
| `dashboard_aggregate` handler | `server/src/admin/dashboard.rs` | 80 |
| 注册到 routes.rs | `routes.rs` `.merge(crate::admin::routes())` | 10 |
| 烟雾测试 | `tests/smoke/admin_dashboard.rs` | 40 |

**为什么第一阶段**：在本阶段可以衡量后续每一阶段的影响。分区启用是否生效？存储是否下降？AI 使用率是否上升？仪表板回答所有这些问题。

**阶段 1a：分区第一阶段 + 物理删除（~5 天）**

| 交付物 | 风险 | 缓解措施 |
|----------|------|-------------|
| `retention_sweep` 物理删除阶段 | 中等——批量大小需调整 | 从一次 500 行开始，`sleep 100ms`，可配置 |
| 分区切换窗口（迁移 0148） | 高——涉及锁 | 详见运行手册的"步骤 C"；先在预备环境演练 |
| 法务保全冷却期 | 低 | 在 `legal_holds` 中新增一列 `cooled_at` |

与审阅 Phase 1 的区别：我增加了一个法务保全释放工作流。审阅提到了，但作为后续事项，未纳入 "Phase 1"。由于法务保全行是物理删除的例外，Phase 1 必须处理它们，否则删除管道会被阻塞。

**阶段 1b：RoomStore 基础 + 增量渲染（~4 天）**

| 交付物 | 用户价值 | 技术债务减少 |
|----------|-----------|---------------|
| `RoomStore` 类 + `ingest()` | 消除 REST/WS 竞争条件 | 将隐式数据流转为显式 |
| 房间切换的增量渲染 | 切换房间速度感知提升 | 用 `appendChild` 取代 `replaceChildren` |
| `CacheStore` 用于 GET 请求 | 减少 REST API 调用 40% | LRU 淘汰策略 |

与审阅的差异：我使用**增量渲染**而非全量 diff（审阅未指定方法）。增量渲染（追加新 DOM 节点，而非 diff 整个列表）对于消息列表更合适，因为消息是追加的，而非随机更新的。

**阶段 2a：AI 客户端集成（~3 天）**

| 功能 | 新 JS 行数 | 新 API 调用 |
|---------|------------|-----------------|
| 智能回复按钮 | 60 | `POST /api/ai/smart-replies` |
| 翻译按钮 | 50 | `POST /api/ai/translate` |
| Composer AI 辅助 | 60 | `POST /api/ai/rewrite` |
| 审核反馈视觉 | 30 | 无（使用现有 `deleted` 帧） |

与审阅的差异：审阅将方向四估计为~200 行。我同意。但从架构角度，我坚持**需要一个 AI 功能层接口**（见第 3.1 节），这会增加约 80 行接口代码，使得总量升至 ~280 行。这部分开销投资是值得的——它确保场景 3（重写）和场景 4（上下文提问）共享同一个限速/缓存基础，而非各自实现。

**阶段 2b：消息交互增强（~5 天）**

审阅认为体量是 M-L（~830 行），而非 M。我同意审阅的修订体量——这是一个 L 级的交付，因为涉及跨模块状态共享。

| 功能 | ~~行数 | 跨模块影响 |
|---------|--------|----------------|
| 右键菜单 | 100 | 新增 `context.js` 模块 |
| 键盘导航 | 200 | 修改 `app.js`、`context.js` |
| 多选模式 | 200 | 修改 `app.js`（wrapper）、`context.js`（状态） |
| 固定交互 | 150 | 修改 `render.js`、`app.js` |
| 跳转上下文 | 100 | 修改 `context.js`（当前已有） |
| 引用视觉 | 80 | 修改 `render.js` |

**与方向二的拮抗关系**：右键菜单的「举报」需要管理 SPA 的审核队列「方向二」才能完成流程。如果方向二 PB 阶段在方向 2b 之后启动，举报在按下右键后就是一个死胡同。我们应在「举报」按钮上添加一个 toast「举报已提交，等待审核」，作为 UI 占位符，而非路由到不存在的审核面板。

**阶段 2c：管理 API 标准化（~4 天）**

与审阅的不同之处：审阅将完整的管理 SPA（XL）作为单一交付物，API 标准化只是其中的一个子任务。我将其拆分为：

| 子阶段 | 交付物 | 行数 | 依赖 |
|--------|---------|------|------|
| 2c-1 | `admin/routes.rs` + 统一中间件 | 150 | 阶段 0（仪表板端点） |
| 2c-2 | 角色模型扩展（+Auditor/+Moderator） | 200 | 无 |
| 2c-3 | 旧路由 → 新前缀的弃用 shim | 80 | 2c-1 |
| 2c-4 | `AdminAuditLayer` | 100 | 2c-1 |

阶段 2c-1 到 2c-4 可以独立交付，每个大约需要 1 天。管理 SPA 作为独立构建步骤稍后跟进。

**阶段 3：管理 SPA（按需）**

审阅认为体量 XL 且应该有一个独立的时间线。我同意。此处不作扩展——审阅的架构决策（独立 WS subject、独立构建产物、`/admin/` 路径）是合理的。还要补充一点：管理 SPA 的初始版本应针对**阶段 0 仪表板端点**，这样第一屏在第一天就可交付。

**阶段 4：分区第二阶段——冷热分层 + S3 归档**

与审阅的不同之处：方向五的这一部分被审阅恰当标记为 XL。我补充**依赖关系**：

- 前提条件：阶段 1a（分区已启用，物理删除管道已就绪）
- 前提条件：阶段 0（存储监控已上线）
- 新问题：如果 `RoomStore` 缓存了 REST 响应（阶段 1b），冷层查询延迟（~500ms）对缓存的**第一次缺失**影响更大——这印证了审阅中关于方向一 vs 方向五拮抗关系的观点
- 缓解：`GET /api/rooms/:id/messages` 应返回 `warm_or_cold: boolean`，好让 `RoomStore` 调整其 `staleWhileRevalidate` 行为

### 5.3 风险登记表

| 风险 | 概率 | 影响 | 缓解 |
|------|----------|--------|------------|
| 分区切换期间停机时间超出预期 | 中等 | 高 | 先在预备环境运行回填，将切换窗口时间定为 < 5 分钟 |
| RoomStore 引入回归（消息丢失） | 中等 | 高 | 阶段 1b 并行运行——新旧路径并存，`console.warn` 差异 |
| AI API 端点返回不一致数据（智能回复无结果） | 低 | 低 | 优雅降级——无结果时不显示（非报错） |
| 管理 API 旧路由弃用时间到期，但仍有消费者在使用 | 低 | 中等 | 弃用日志中记录 `x-deprecation-warning` 头；到期前公告 |
| 方向三的右键菜单与移动端触摸事件冲突 | 高 | 中等 | 统一使用 `pointerup` 事件处理，区分触摸与点击 |

---

## 6. 总结：与审阅意见的一致与分歧

| 议题 | 审阅 | 我的观点 | 一致？ |
|--------|--------|--------|---------|
| 方向一优先级 | P2（下修） | P1（保留但合并到方向 A） | ❌ 持有分歧 |
| 方向一体量 | P2 | 阶段 1b 作为方向 A 一部分保持 P1 | ❌ 持有分歧 |
| 方向二的管理 WS 模型 | 提及但未深入 | 推荐独立管理 WS subject | ✅ |
| 方向三体量 | M→L（~830 行） | 同意 L，但认为估算仍偏低（+跨模块成本） | 部分一致 |
| 方向四体量 | M（~200 行） | 同意，加入 AI 功能层接口增加 80 行 | 部分一致 |
| 方向五 Phase 1 vs Phase 2 拆分 | 核心贡献 | 完全同意，增加法务保全冷却期作为子任务 | ✅ |
| 跨方向拮抗关系 | 3 个已识别 | 额外 1 个：方向二（管理审核队列）与方向三（右键举报） | 补充而非分歧 |
| 零新外部依赖约束 | 已接受 | 在 Phase 1 维持；对 Phase 2 的 parquet_fdw 有疑问 | 部分一致 |
| 缺失的可观察性基础 | ❌ 未识别 | 增加为阶段 0 | 补全 |
| 事件 Schema 版本控制 | ❌ 未识别 | 增加为方向 E | 补全 |
| AI 助手 @mention + 翻译交叉利用 | 审阅审阅中识别 | 同意——有价值的协同效应 | ✅ |

**总体评价**：审阅文档是 `docs/requirements/` 中质量最高的产品之一，其拆分方向五 Phase 1/2 是值得工程化的洞见。我的分歧集中在三点：方向一的紧迫性（我主张保留 P1 但重新分组到方向 A）、缺失的阶段 0（可观察性基础），以及方向 E 作为架构先决条件的必要性。最终用户将感到最明显的是方向一的 RoomStore（消除 REST/WS 竞争条件）和方向的四的 AI 集成（将现有能力转化为可见功能）——出于不同的原因，这两个方向都值得优先关注。
