现在我对代码库有了透彻的了解。以下是全面架构分析。

---

# Aero IM — 架构分析报告

> **日期**: 2026-07-12 | **基线**: `AGENTS.md` + 源码（16 个 crate，157+ 迁移，17 个 JS 模块）
> **方法**: 所有锚点已通过 grep + 源码读取进行交叉验证。以下是架构综合。

---

## 1. 架构评估

### 1.1 优势（已验证）

**事件驱动脊柱是健壮的。** 核心数据路径——`ImService::publish_room_event` → NATS subject → `run_bus_listener`（两阶段解码）→ `Hub::fan_out_raw` → 有界 mpsc → WS——在 878 行的 `hub.rs` 中经过充分测试。它有明确的 OOM 保护（有界队列、缓慢消费者断开/丢弃）、中毒消息处理（在确定性不可解码时确认丢弃，而非 nack 导致无限重投）、以及用于跨消费者重启恢复的持久游标。

**crate 隔离得到实际执行。** 依赖图严格遵循规定的方向：`aero-common`（叶子）→ `aero-bus` / `aero-storage` / `aero-auth` → `aero-im-core` / `aero-live-*` → `aero-server`。依赖检查脚本强制执行。没有 crate 引入 `str0m`（文档化约束已核实）。

**授权护栏是系统性的——且经过巧妙测试。** `authz_lint.rs` 测试（~200 行）对每个处理函数进行静态源扫描，查找强制授权守卫的模式。它不依赖运行时覆盖，而是确保每个从路径提取 `RoomId`/`WorkspaceId` 的处理函数都必须包含一个认可的守卫调用，否则测试失败。这是 Rust 中编译期安全分析的少见的实用方法。

**配置表面虽大但可归类。** 16 个 crate 的配置被分解为：
- `AppConfig`（figment，`AERO__SECTION__KEY`）用于核心基础设施（PG、Redis、NATS、Auth）
- `GatewayConfig`（`AERO_*` 平面变量）用于中间件堆栈
- 每个 bot 的 `AERO_AI_MODERATION_*` 等用于功能门控
- 每个迁移都是幂等的，编译时嵌入（`sqlx::migrate!("../../migrations")`）

**Web SPA 是无打包器 ESM 架构的一个深思熟虑的示例。** 原生 ESM 模块、零构建步骤、`context.js` 作为明确的共享脊柱、全局状态与业务逻辑分离。`polls.js`（169 行）是一个很好的模块化模板。

### 1.2 局限性

**重大且未充分利用的测试基础设施。** 有 1,406 个 `#[test]` 函数和 415 个 `#[tokio::test]`，但只有 11 个 `#[ignore]`（= PG 门控）。CI 配置有完全注释掉的 `integration-test` 作业。实际上：
- ~1,395 个测试在没有 PG 的情况下运行（纯单元 / mock）——范围良好
- 11 个 PG 门控测试**在 CI 中根本不运行**
- 0 个服务容器（PG、Redis、NATS）被配置
这意味着任何与数据相关的逻辑（查询、仓储方法、完整工作流）从未在 CI 中进行过集成测试。

**Web SPA 作为单块在增长。** 19 个 JS 文件 / 5,939 行。`app.js` 在 1,009 行达到 HARD 限制。没有组件模型，没有类型检查（vanilla JS + 可选 ESLint），所有 DOM 操作都是手动的。CSS 是 1,236 行的单个文件。`context.js` 脊柱（208 行）容纳了 30+ 个状态字段、DOM 引用和 WS 单例——它是正确的抽象，但随着应用增长，它将成为一个接缝。

**配置表面虽然可分类但很复杂。** 双重命名约定（`AERO__SECTION__KEY` 与 `AERO_*` 平面）增加了运营开销。配置没有集中文档化或版本控制的操作模式——每个字段都在使用它的 crate 中被发现。没有 `config dump` 端点。

**缺少部署拓扑文档。** 有一个 `docker-compose.yml` 但未读。系统需要 PG 17、Redis 7、NATS（带 JetStream）、可选的 S3、可选的 FCM/APNs、可选的 Voyage/Anthropic——但没有正式的定义这些模式的清单（最小部署、仅 IM 与完整直播、多区域等）。

### 1.3 架构债务

**旧的 `MessageEnvelope` 载荷。** `run_bus_listener` 包含一个备用路径，在 `RoomEvent` 解码失败时，回退到 `serde_json::from_slice::<MessageEnvelope>`。这是一个迁移兼容性垫片——它增加了每个消息处理路径的代码和序列化开销。一旦所有生产者都升级，就可以清理。

**`routes.rs` 阈值接近极限。** 2,854 行，接近 3,000 HARD 限制。文档说它是 ~100 个子模块路由的一体化装配点。随着子模块数量增长，这一行将成为一个堵塞点。

**时序器模式不一致。** 有些时序器携带 `CancellationToken`（用于优雅关闭），有些不携带（`blob_gc_drain`、`observability_gauge_samplers`）。有些是可配置的（`AERO__SERVER__RETENTION_SWEEP_SECS`），有些是硬编码的（`embedding_backfill` @ 300s）。它们都使用 `MissedTickBehavior::Skip`（正确），但启动/停止生命周期不一致。

---

## 2. 扩展方向

根据代码验证，这是五个**真正的、未被现有 100+ 分析覆盖的**高价值方向：

### 方向 1：测试基础设施——关闭“无 CI 集成测试”漏洞（P0）

**为什么需要：** 这是一个架构风险，而非功能缺失。如果重要的 `im-core` 或 `storage` 查询发生回归，1,406 个测试中没有一个能在 CI 中捕获它（11 个 `#[ignore]` 测试从不运行）。该系统声称“经过充分测试”，但重要的集成测试门是关闭的。

**核心挑战：**
- CI runner 需要 PG 17、Redis 7、NATS 服务容器
- `aero-cli migrate` 需要一个运行中的 PG + 编译时嵌入的迁移
- 11 个现有的 `#[ignore]` 测试需要使用一次性数据库（`make migrate-smoke` 模式）
- 新的集成测试增加了 CI 运行时间（从 ~2 分钟到 ~10+ 分钟）

**选项 A（低成本）：** 取消注释 CI 中的 `integration-test` 作业，添加 3 个服务容器，勾选 `#[ignore]` → 运行整个套件。成本：~4 小时配置。

**选项 B（高覆盖率）：** 新建一个 `harness/` crate，包含针对 throwaway 数据库的 E2E smoke 测试（每个场景 = `CREATE DATABASE` → `migrate` → 运行 → `DROP DATABASE`）。使用现有的 `scripts/smoke_*.py` 脚本作为参考。成本：~20 小时。

**推荐：** 选项 A 立即可行。选项 B 是后续目标。

**架构变更：** 向 `docker-compose.ci.yml` 添加服务容器。在 CI 中取消注释 `integration-test`。向 README 添加 `make smoke-ci` 以在开发者机器上本地模拟。

---

### 方向 2：Web SPA 架构演进——从 Flat ESM 到分层前端（P1）

**为什么需要：** 该系统认为其前端是“零依赖 ES2020 SPA”，但 `app.js` 在 1,009 行达到 HARD 限制，`style.css` 在 1,236 行，并且没有组件模型。关键路径 `ws.js`->`context.js`->`render.js`->`app.js` 正在变成一个可维护性的瓶颈。

**核心挑战：**
- 无 bundler 或无 TypeScript（选择是有意的，但限制了架构选项）
- 无组件模型——所有 UI 逻辑都是面向过程的 DOM 构建
- CSS 作用域——所有样式都是全局的
- 状态突变是可变的且非事务性的（`state.rooms.set()` 是即时突变）
- ESM `import` 图创建了一个有向无环图，但 `import` 用于代码组织，并非架构

**提案：**
1. **视图组件契约：** 每个 UI 域（聊天列表、频道面板、搜索、设置）获得一个函数 `renderView(state, container)`，它返回一个 `{ mount, unmount, update }` 控制器。这创建了一个类似 React 的契约而不需要框架。

2. **CSS scoping via convention：** 每个 `module.css` 文件（或在 `style.css` 中注释分隔的部分）遵循 `.module-*` 类名前缀以防止泄漏。向 `scripts/web-check.sh` 添加 lint。

3. **状态变更透明度：** 包装 `state` 对象，使其在每次变更时发出 `onStateChange` 事件，使视图层能够增量重新渲染（类似于 Redux 的 `subscribe`，但像 ~50 行那样轻）。

**架构变更：** 零新依赖。向 `web/` 添加约 3 个约定文档（`docs/frontend-component-contract.md`）。重构 `app.js` 中的渲染逻辑到域级视图模块。

---

### 方向 3：配置操作化——使部署可重现（P1）

**为什么需要：** 当前 16 个 crate 的配置分布在 8+ 个模块中，命名约定混合，无版本控制的操作模式，无转储端点。现场事故诊断意味着 grep 日志 + 猜测环境变量。

**核心挑战：**
- figment `AppConfig` 从 `config.toml` + `AERO__*` env 变量加载
- `GatewayConfig` 从 `AERO_*` 平面变量加载
- 每个 bot 都有自己的 `AERO_AI_MODERATION_*` / `AERO_UNFURL` / 等
- 有些值在 `Cargo.toml` features 中编译期（`str0m` 门控）
- 没有单一的真实来源

**提案：**
1. **添加 `GET /api/admin/config-dump`**（管理员 Bearer token 门控），返回去除机密的扁平配置 Map（隐藏密码、API 密钥）。
2. **创建 `config.default.toml`** 来替换 `config.example.toml`——包含每个字段的文档注释作为内联 YAML 式文档，可由 `serde` 注释自动转换为 JSON Schema。
3. **在启动时记录操作模式**（`INFO mode=minimal` 或 `mode=full_with_live`），这样现场工程师只需查看第一条日志行就能知道部署范围。

**架构变更：** 零新依赖。向 `aero-common/src/config.rs` 添加约 100 行。在 `aero-server` 中添加一个管理路由。

---

### 方向 4：时序器统一——从临时时序器到托管系统（P2）

**为什么需要：** 当前有 10+ 个独立的 `tokio::spawn` 时序器循环，分散在 `background.rs`、`retention.rs`、`metrics_tasks.rs` 中。有些有关联 Token，有些没有。有些是可配置的，有些是硬编码的。生命周期（启动、关闭、监控）是非标准化的。

**核心挑战：**
- 时序器生命周期差异（`blob_gc_drain` 缺少 cancel token）
- 时序器间配置不一致（`embedding_backfill` @ 硬编码 300s vs `retention_sweep` @ 可配置 3600s）
- 无健康检查——死掉的时序器（静默 panic）是无声的
- 重复代码——每个时序器都重复 `loop { select! { … tick.tick() } }` 模式

**提案：**
1. **向 `aero-common` 添加 `PeriodicTask` 辅助**，包装计时器 + token + 健康跟踪。
2. **将所有时序器迁移到托管注册表**，向 `/health/ready` 贡献任务运行状态。
3. **标准化配置前缀**为 `AERO__SCHEDULER__*`（带 `_SECS` 后缀的默认值）。

**架构变更：** 新的 `aero-common/src/periodic_task.rs`。每个时序器模块中的最小 diff。约 200 行核心 + 约 50 行迁移/时序器。

---

### 方向 5：Web SPA 错误恢复——从尽力而为到可恢复（P0）

**为什么需要：** WebSocket 层 (`ws.js`) 有重新连接逻辑，但应用状态不能从断开连接中存活。当 WS 断开连接时：
- `state.messagesByRoom` 保留内存中
- `state.unreadByRoom` 被孤立
- 重新连接触发全屋 backfill（回到方向 3 中的 P1 ROADMAP gap）
- 但**没有持久的状态检查点**——页面刷新就是完整的应用重新启动

**核心挑战：**
- 无存储 API（`localStorage` / `IndexedDB`）用于状态持久化
- 无服务工作者用于离线缓存
- `context.js` 状态是纯内存的
- ROADMAP 方向 3（投递台账）将最终解决服务器端的问题，但客户端缓存是独立且紧迫的

**提案（最小的、无依赖的增量）：**
1. **将 `context.js` 中的选定状态检查点到 `sessionStorage`**（在刷新时存活，在选项卡关闭时清除）：`me`、`currentRoomId`、`lastEditAt`。
2. **将 `state.unreadByRoom` 持久化到 `localStorage`**，以便重新连接时无需重新获取未读计数。
3. **添加 `GET /api/me/state-sync` API**，返回自给定 `since` 时间戳以来的投递游标 + 未读计数 + 通知计数——这是 ROADMAP 方向 3 之前的一个更轻量级的替代方案。

**架构变更：** 对 `ws.js` 的最小约 50 行变更。新 API 端点（约 30 行）。零新依赖。

---

## 3. 接口设计建议

### 3.1 什么有效并且应该保持稳定

- **`EventBus` trait** (`aero-bus/traits.rs`)：pub/sub 抽象是干净的。NATS 是唯一的实现，但 trait 边界意味着测试可以用内存总线替换它。不要改变签名，除非添加向后兼容的 `headers: Option<…>`（如 ROADMAP 方向二所讨论的）。

- **`Hub::fan_out_raw`**：`&[ParticipantId], &str` 签名经过实战检验。它不暴露内部 `DashMap` 结构。保持原样。

- **`XRepo` 模式**：每个 PostgreSQL 实体一个仓库，在所有路由处理程序中使用 `XRepo::new(pool.clone())`。这个模式一致且易于审计。继续下去。

- **`assert_room_access(participant, room)`** 签名：第一个参数是参与者，第二个是房间。这是 CI 强制执行的一个锚点约定。所有新路由必须遵守。

### 3.2 新抽象有用的地方

- **测试基础设施：** 一个 `TestContext` 辅助（`create_test_db()`、`migrate()`、`seed_baseline()`、`cleanup()`）将鼓励更多的 `#[ignore]` DB 测试。目前，每个需要数据库的测试都必须处理自己的设置——增加编写集成测试的心理开销。

- **配置模式：** 一个 `ConfigSource` trait（枚举 `TomlFile | EnvVar | K8sSecret`）将使配置集中化并使现场配置可审计。目前，每个 crate 单独读取其 env 变量。

- **时序器生命周期：** 一个 `PeriodicTask` 包装器（如上所述）将标准化时序器模式并启用健康监控。

### 3.3 向后兼容性

当前系统通过在 `run_bus_listener` 中保留旧负荷格式来处理旧 wire 格式。当确实需要迁移时，应遵循这种“双读取、单写入”模式：

1. 添加新格式生产者（保持旧生产者运行）
2. 添加双读取消费者（处理新旧格式）
3. 迁移所有生产者后删除旧消费者路径
4. 删除旧生产者

`RoomEvent`→`MessageEnvelope` 回退已经遵循这种模式，并且应该保持直到所有 `RoomEvent` 生产者都上线。

---

## 4. 技术选型

### 4.1 现有栈总结

| 层 | 技术 | 状态 |
|---|------|--------|
| HTTP | axum 0.7 | 经实战检验，稳定的选择 |
| 数据库 | sqlx 0.8 + Postgres 17 | 正确——异步、编译时检查、PG 是 IM 的正确 RDBMS |
| 缓存/集群状态 | fred 9 + Redis 7 | 使用 sorted-set 作为存在感展示是正确的 |
| 消息队列 | async-nats 0.36 + JetStream | 持久消费者是 at-least-once delivery 的正确抽象 |
| WebRTC | str0m 0.19（纯 Rust，无 CGO） | 正确选择——Rust 生态系统中最成熟的纯 Rust WebRTC |
| 前端 | 原生 ESM，零依赖 | 对于调试客户端来说，这是一个深思熟虑的选择。对于生产 UI 来说，需要演进理解 |

### 4.2 应该引入的内容

**不要引入框架依赖。** 该代码库精心避免在 Rust 方面使用 `async-trait` / `dyn-*` 过度抽象，在前端方面避免使用 bundler / TypeScript / React。这项纪律是一个核心优势。新依赖必须满足与现有代码相同的标准：
- 纯 Rust（无 C 绑定）
- 在 `AGENTS.md` 级别维护（至少达到常见标准）
- 审查对编译时间的影响

**唯一合理的新增依赖：**

| 依赖 | 用途 | Rationale | 风险 |
|--------|---------|-----------|------|
| `web-push` (Rust) | Web Push VAPID 网关 | 现有 `aero-push` crate 架构自然扩展 | 🟡 中等——tokio 兼容性需要 POC 验证<br>备选方案：手写 HTTP/2（~200 行） |
| `chrono-tz` | 会议 iCal 时区 | 如果尚未引入——成熟的 crate，tz 数据编译时嵌入 | 🟢 低——零运行时依赖 |
| `playwright` 或 `puppeteer` (JS) | Web Push / SW E2E 测试 | 浏览器自动化是测试 Service Worker 推送的唯一实用方式 | 🟢 低——仅开发者/CI 依赖 |

**应该避免的内容：**

| 候选 | 理由 |
|--------|---------|
| 迁移到 Tokio `async`/RT + str0m | 已经完成——不改变 |
| 添加 React / Vue / Lit | 零依赖路线是产品差异化；添加框架会破坏它 |
| 添加 OpenAPI schema 生成器 | 手写文档已经足够；自动生成会增加 CI 时间且没有显著效益 |
| 添加 gRPC | 与现有 HTTP+WS 双协议模式的重叠不匹配 |
| 引入消息队列替代品（Kafka / RabbitMQ） | NATS JetStream 是正确的选择：更少的操作开销，足以满足 IM 负载，内置 at-least-once |

### 4.3 自建与采购

当前代码库的自建方法（`HashEmbedder` 而非 Voyage、`FakeGateway` 而非 FCM、自建 iCal 生成器）是**经过深思熟虑的**。它允许沙箱部署在零外部 API 密钥的情况下工作。此模式应保持不变：对每个外部依赖进行抽象，使系统在没有密钥的情况下降级优雅，并保持主干路径在隔离中可测试。

唯一缺少的抽象是一个**官方 SDK 层**（针对 bot/外部开发者）。目前，bot 开发人员需要手动 POST webhook。一个 `aero-bot-sdk` crate（版本化的、有文档记录的）将使 bot 生态系统成为产品现实。

---

## 5. 实施路线图

### 5.1 优先级

基于风险敞口（非功能需求！）的分层，而非功能吸引力：

| 优先级 | 方向 | 理由 |
|----------|-----------|---------|
| **P0** | 方向 1：CI 中的测试基础设施（集成测试） | 当前最大的架构风险：CI 中不运行数据库测试 → 回归被静默接受。修复这是基础设施，而非功能 |
| **P0** | 方向 5：Web SPA 错误恢复 | WS 重新连接 without 状态恢复是面向用户的错误。页面刷新 = 完全状态丢失。修复这是产品就绪性 |
| **P1** | 方向 2：Web SPA 架构演进 | `app.js` 处于 HARD 大小限制。下一个主要前端功能（Canvas、Tasks、Approvals UI）将突破它。在添加新功能之前修复 |
| **P2** | 方向 3：配置操作化 | 重要但非紧迫——在第一个生产事故后，对配置转储的需求将变得明显 |
| **P2** | 方向 4：时序器统一 | 抽象改进——在任何时序器静默死亡导致生产问题之前，有合理的时间 |

### 5.2 阶段划分

```
    第 1 阶段 (Week 1-2) — 基础设施 + 恢复
    ┌─────────────────────────────────────┐
    │ P0: CI 集成测试                             │
    │   → 取消注释 integration-test 作业          │
    │   → 向 .github/workflows/ci.yml 添加服务容器 │
    │   → 验证 11 #[ignore] 测试绿色运行            │
    │   → 添加 3 个覆盖关键数据路径的 smoke 测试      │
    ├─────────────────────────────────────┤
    │ P0: Web SPA 状态检查点              │
    │   → context.js + sessionStorage 检查点   │
    │   → unreadByRoom → localStorage         │
    │   → ws.js 重新连接恢复状态               │
    ├─────────────────────────────────────┤
    │ 里程碑: CI 绿色 + 页面刷新保持登录态         │
    └─────────────────────────────────────┘

    第 2 阶段 (Week 3-4) — 前端架构 + 配置
    ┌─────────────────────────────────────┐
    │ P1: Web SPA 视图契约                │
    │   → 定义 renderView() 组件契约 DOC  │
    │   → 从 app.js 中提取前 3 个视图         │
    │   → 文档化 CSS 作用域约定               │
    ├─────────────────────────────────────┤
    │ P2: 配置转储 + 模式日志             │
    │   → GET /api/admin/config-dump      │
    │   → 启动时模式日志                      │
    │   → config.toml → 文档化              │
    ├─────────────────────────────────────┤
    │ 里程碑: app.js < 800 行 + 转储可用      │
    └─────────────────────────────────────┘

    第 3 阶段 (Week 5-6) — 时序器统一
    ┌─────────────────────────────────────┐
    │ P2: PeriodicTask 包装器             │
    │   → 迁移 blob_gc_drain / embedding  │
    │   backfill / viewer sampler         │
    │   → 添加健康跟踪                       │
    │   → 统一配置前缀                       │
    ├─────────────────────────────────────┤
    │ 里程碑: 所有时序器托管 + /health 覆盖       │
    └─────────────────────────────────────┘
```

### 5.3 风险与缓解

| 风险 | 可能性 | 影响 | 缓解 |
|------|----------|--------|--------------|
| CI 服务容器内存不足（PG 17 + Redis + NATS ~2GB） | 🟡 中等 | 🔴 高 | 使用 `docker compose -f docker-compose.ci.yml` 进行本地测试。CI runner 需要更大的机器。备选：单独运行 PG 测试（不是全栈） |
| `sessionStorage` 检查点未捕获所有状态（`Map` 对象、`Set`） | 🟢 低 | 🟡 中等 | 使用原语进行检查点。`Map` → `Array.from(m.entries())`。在 `context.js` 中显式白名单可检查点字段 |
| `app.js` 重构在拆分过程中破坏了现有渲染 | 🟡 中等 | 🔴 高 | 每个视图提取创建一个独立的 PR，并在提取前后进行 `scripts/web-check.sh` + 手动 VPanel 验证。使用功能标志进行渐进式发布 |
| 时序器健康跟踪增加了复杂性却没有带来好处 | 🟢 低 | 🟢 低 | 这是第 3 阶段，处于 P2——如果前期采用率低，可以跳过。`PeriodicTask` 包装器是 ~50 行——删除成本低 |

### 5.4 不做清单（本分析范围之外）

- **媒体面接缝**（`SfuMediaSession` + `CallBridge` 接线）：如 `AGENTS.md §4.5` 中所述，这些是需要真实 WebRTC 对等方的 staging seams。对架构来说并非紧急。
- **ROADMAP 方向 3 投递台账**：重要的产品领域，但已在 `docs/ROADMAP.md` 中捕获，需要一次 >L 的投入。上述方向 5 提供了即时的客户端修复。
- **MLS E2E 加密**：如 `AGENTS.md §4.4` 中所述——有意的非目标，客户端密码学。
- **联邦**：明确划定的范围。

---

## 总结

Aero IM 拥有一个**经过深思熟虑且健壮的架构**，crate 隔离良好，事件驱动的主干正确，授权模型系统化。100+ 现有分析已经彻底探讨了功能扩展和产品方向。

本分析确定的**五个真正的高价值方向**来自一个不同的缺口——运营和基础设施就绪：

1. **CI 集成测试（P0）**——架构风险，而非功能差距
2. **Web SPA 架构（P1）**——前端正在接近没有框架的 monolith 极限
3. **配置操作化（P1）**——大量配置表面没有运营工具
4. **时序器统一（P2）**——时序器生命周期不一致是 bug 的前兆
5. **Web SPA 错误恢复（P0）**——状态从 WebSocket 断开中丢失

最重要的见解是：**系统中最大的架构风险根本不是代码——而是 CI 中未运行的集成测试。** 1406 个测试和 0 个 CI 数据库测试意味着基于数据的回归（损坏的查询、损坏的迁移、缺少授权守卫）可以轻松通过审查。在下一个功能之前修复这个。
