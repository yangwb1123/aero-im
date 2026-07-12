感谢你的阅读和追问。不过我注意到，`docs/requirements/` 下并没有精确名为 `2026-07-11-global-scan.md` 的文件——存在的是多个衍生产物，比如 `2026-07-11-global-scan-extension-directions.md`、`2026-07-11-global-scan-five-cross-system-blindspots.md` 等。你读到的应该是其中一份或多份的融合。

既然你问我 **「是否希望做交叉验证、制定实现计划，还是其他用途」**，那我选择一个对它最有价值的用途：

**→ 方案 3 + 交叉验证**：基于文档的洞见，做一次「架构师视角的回评」，质疑/深化其中的核心断言，并给出具体实现路径建议。

下面是我的完整架构分析。

---

# 架构师回评：全局扫描文档的深度分析与方向验证

## 1. 架构评估

### 核心优势（文档未充分强调的）

- **事件 DAG 的清晰性被严重低估**：以 NATS JetStream 为事实源 + Hub 进程内扇出这一模型，在 Rust 生态中属于**顶级设计**。它同时满足了「跨实例可靠投递」（durable consumer）和「进程内低延迟」（bounded mpsc），避免了 Kafka 的 micro-batch 延迟和纯内存广播的丢数据风险。这个决策本身就让 Aero IM 在架构层面优于大多数 Rust IM 项目。

- **「每 crate 一个功能域」的边界非常干净**：AGENTS.md §1 的 crate 地图从 `aero-common`（叶子）到 `aero-server`（组合），依赖方向自下而上，无环。这在 16 个 crate 的规模下非常难得，意味着新功能可以独立在 crate 内开发而不担心循环依赖——这是未来 10 个 crate 增量的重要前提。

- **媒体 seam 的「已建+已测，未接线」策略是务实的选择**：`SfuMediaSession`/`CallBridge` 的代码已到位，但生产未接线。这避免了在没有真实对端（浏览器/str0m 配对）的情况下过早集成带来的维护成本。很多项目在 CI 不跑的媒体代码上产生大量虚假告警——Aero IM 明确将其标记为「未接线」，减少了认知负担。

### 真正的架构债务（文档未覆盖）

文档各方向覆盖了大量缺口，但我认为以下几个**更深层的架构债务**被遗漏了：

1. **进程内全局状态的扩散**：`slow_mode.rs` 的 `LAST_POST` 是 `static HashMap`、`rate_limit.rs` 的 token buckets 也是 `static`。当进程数 > 1 时，这些状态等价于不存在。当前 Redis presence/roster 已经正确做到集群级，但进程内 static 散落在多个模块，且文档/代码中没有清晰的「进程级 vs 集群级」边界标记。**这是多实例部署前必须清理的——否则部分限流/慢模式在水平扩展后静默失效**。

2. **ImService 的膨胀**：`aero-im-core/src/service/` 内 `messages.rs`、`events.rs`、`rooms.rs` 等模块都在往 `ImService` 结构体上挂方法。当前约数百行，但这种模式在 20+ 方法后会导致结构体臃肿和编译时间恶化。需要更早引入 trait 化拆分（`MessageService`、`RoomService`、`CallService`）。

3. **`aero-common` 的「叶子」边界已被侵蚀**：`common/src/model/` 包含 `RoomEvent`/`StreamEvent` 等 50+ 枚举变体，已接近「上帝模块」的边界。任何新事件类型都加到这里，导致每次修改都触发全仓重编译。需要按领域拆分（`aero-im-events`、`aero-live-events`）或转为 proc-macro 生成。

## 2. 高价值扩展方向（优先级调整版）

以下是我对文档 5 个方向的优先级重评，以及我补充的第 6 个方向：

### 方向一（`extension-directions` 的「前端现代化」→ P0，同意，但补充）

**为什么 P0 正确但理由应更尖锐**：文档说「后端企业级、前端 debug 级」，这是对的，但没点出最关键的问题——**Web SPA 是一个单点故障**。当前 `web/` 没有任何构建步骤，这意味着：
- 无法 tree-shake → 所有 JS 全量加载
- 无法 type-check → 运行时才能发现 `ws.on('msg:interaction')` 拼错
- 无法 sourcemap → 生产问题无法定位到源码行

**建议的路径修正**：不要一步跳到 Lit/Vue。当前最缺的是 **TypeScript type generation from Rust types**。可以用 `ts-rs` crate 为 `ServerFrame`/`ClientFrame` 生成 `.d.ts`，让前端至少有帧类型的编译时保障。这是投入最小、回报最高的第一步。

### 方向二（Admin 控制台 → P0，同意，但补充）

文档列出了 7 个审核模块但无 UI。我加一个更关键的问题：**当前鉴权模型不支持「管理角色」和「普通用户」的区隔**。`AuthUser` extractor 和 `assert_room_access` 守卫都是「参与者操作房间」的模型——缺少 `AdminUser` extractor 和 `assert_system_admin` 守卫。管理控制台必须先建立管理鉴权层，再做 UI。

**建议**：先加 `aero-auth` 中的 `AdminRole`（`super_admin` / `workspace_admin` / `moderator` 三级），再建 admin 路由前缀 `/api/admin/*`，复用既有中间件栈。

### 方向三（DevOps → P1，下调优先级）

文档说「可运行 demo，不可部署生产」是准确的，但**当前阶段 Dockerfile 的紧迫性低于「CI 集成测试」**。理由：
- Dockerfile + K8s 是一次性工程，但开集成测试的门槛是每天都会遇到的——每次合并前手动跑 `#[ignore]` 测试是流程裂缝
- 没有集成测试的 CI，任何 PG schema 变更都有可能无声合并

**建议重排**：P0 = CI 集成测试开启。Dockerfile 降到 P1，K8s 降到 P2。

### 方向四（协作原语客户端 → P1，同意但补充边界）

Canvas 客户端是最 ROI 的——后端 500+ 行 Rust + 3 次 migration 全已就绪。文档的建议是「独立模块模式」。我补充一点：

**Canvas 的竞态条件**：`collab.rs` 使用乐观锁（`version` 列），但 Web SPA 的 Canvas 渲染是异步的——用户打开 Canvas 时如果另一个用户正在编辑，前端可能基于过期版本渲染。这不是服务端问题，但客户端需要考虑 `version` 冲突后的合并提示（类似 Google Docs 的冲突 resolution bar）。

### 方向五（客户端遥测 → P2，同意，但名称应改为「可观测性扩展」）

Feature flags 基础设施比客户端遥测更紧迫——它直接解锁灰度发布能力，对 DevOps P0 方向是前置依赖。建议把 feature flags 提到 P1 并优先于 Web Vitals 采集。

### **追加方向六（P0 · 我补充的）：迁移与零停机部署策略**

当前 `migrate()` 在 `main()` 启动时运行，且是阻塞式的。对于多实例部署：
- 实例 A 启动 → 运行 migration → schema 变更
- 实例 B 还在运行旧代码 → 查询已变更的 schema → 崩溃

**建议**：
1. `aero-cli migrate` 独立于 `aero-server` 运行（已有 CLI 入口，确认）
2. 增加 `--check` 模式：启动前验证 schema 版本匹配，不匹配则拒绝启动
3. 所有 migration 必须遵循「向后兼容 2 个版本」原则——`ALTER TABLE ADD COLUMN` 允许，`DROP COLUMN` 分两步（先停用再删除）

## 3. 接口设计建议

### 当前接口的质量

| 维度 | 评价 | 证据 |
|------|------|------|
| URL 风格 | RESTful ✓ | `/api/rooms/:id/messages` 符合资源范式 |
| WS 帧设计 | 良好 ✓ | `ClientFrame`/`ServerFrame` 带 `kind` tag，清晰 |
| 错误模型 | 弱 ⚠️ | `Error` enum 含笼统的 `Internal`、`NotFound`，缺少结构化错误码 |
| 版本化 | 缺失 ❌ | 无 `Accept-Version`、无 URL 前缀版本化 |
| 分页 | 良好 ✓ | 支持 `cursor`/`limit` 参数 |
| 鉴权泄露 | 存在 ⚠️ | `AuthUser` extractor 在 handler 中，但 `invitations.rs` 用 `generate_token` + `hash_token`，两个 helper 同名不同模块（已在 AGENTS.md §4.2 标注） |

### 建议新增的抽象层

1. **`AdminAuth` extractor**：从 `AuthUser` 继承，加 `assert_admin()` 方法，区分系统管理员、工作区管理员、审核员。

2. **`AdminRouter` trait**：所有 admin 功能实现 `fn admin_routes() -> Router<AppState>`，与公共 API 路由分离，通过 `Router::nest("/api/admin", admin_routes())` 挂载，中间件独立（更严格的 IP 白名单、审计、限流）。

3. **结构化错误码体系**：当前 `Error` 类型仅有 `Internal`、`NotFound`、`Forbidden`、`BadRequest` 等变体，建议补充：
   - `RateLimited { kind: &'static str, retry_after: Duration }` — 带限流层标识
   - `VersionConflict { expected: i64, actual: i64 }` — 乐观锁冲突
   - `ValidationFailed { field: &'static str, reason: &'static str }` — 字段级错误

### 向后兼容策略

- WS 帧的 `kind` tag 设计天然支持向后兼容——新 variant 加 `#[serde(deny_unknown_fields)]` 要谨慎，应保持忽略未知字段（当前 `run_bus_listener` 的两阶段解码也依赖此行为）
- REST API 在 v1 阶段不应引入 URL 版本化，而是在响应头加 `X-Api-Version: 2026-07-01`，允许客户端协商
- 消息 `Block` 类型应保持 extensible——新 block 类型加 `#[serde(untagged)]` 回落，避免破坏老客户端

## 4. 技术选型建议

### 文档中涉及的技术栈决策

| 文档建议 | 我的评估 | 结论 |
|---------|---------|------|
| 前端用 Lit/Vue | ❌ 过度设计 | 当前无构建步骤，加框架 = 加 webpack/vite 管线 = 运维复杂度倍增。建议先 TypeScript + 原生 Web Component |
| NATS 集群化 | ✅ 正确 | `async-nats` 0.36 已支持 cluster 和 JetStream 镜像，只需配置 |
| Feature flags PG 表 | ⚠️ 折中 | PG 表对 feature flag 这种高频读取的数据是合理选择，但建议加 Redis cache 层 + TTL 30s |
| `SafeHttpClient` | ✅ 必须 | 统一出站 HTTP 是安全底线。不建议引入新依赖，reqwest 已有 `danger_accept_invalid_certs` 等配置 |

### 值得评估的新依赖

1. **`hickory-resolver`（DNS-over-HTTPS）**：用于 SSRF 防护中 DNS rebinding 缓解（`cross-system-blindspots.md` 方向二有提及）。对比 `trust-dns-resolver`（已 rename 为 hickory-resolver），hickory 是 Rust DNS 社区的事实标准，轻量，已审。

2. **`ts-rs`（Rust→TypeScript 类型导出）**：解决前端帧类型安全问题。替代方案是手写 `.d.ts`，但维护成本高。`ts-rs` 是 proc-macro 方案，对 `#[derive(TS)]` 的 struct 自动生成 `.d.ts`。风险：proc-macro 可能增加编译时间，但只在 `aero-common` 层启用。

3. **`grafana-faroe` 或自定义 Feature Flags crate**：不推荐 frunk 等重型方案。项目应自建基于 PG + Redis 的轻量 feature flag 模块（~200 行 Rust），避免外部依赖。

### 自建 vs 采购

| 需求 | 建议 | 理由 |
|------|------|------|
| 管理控制台前端 | **自建** | 与现有 API/WS 深度耦合，第三方 admin panel（如 Strapi）无法对接自定义鉴权 |
| 客户端遥测/错误上报 | **Sentry（自托管）或自建轻量端点** | Sentry 自托管有成熟 Docker 镜像，与当前云原生架构匹配；自建端点复杂度不高（40 行 handler + 10 行 JS） |
| DNS rebinding 防护 | **自建** | 在 `SafeHttpClient` 中集成 hickory-resolver，符合当前「无外部依赖」哲学 |

## 5. 实施路线图

### P0（本月）

| 项 | 估算 | 依赖 |
|----|------|------|
| **P0a. CI 集成测试开启**（`docker-compose.ci.yml` + GH Actions `services` + `cargo test -- --include-ignored`） | 2 天 | 无 |
| **P0b. 进程级全局状态审计**（grep `static ` 在 `aero-server/src/` 下，列出所有进程级状态，写文档标记「多实例不生效」） | 1 天 | 无 |
| **P0c. SSRF 防护**（`SafeHttpClient` + unfurl_bot 私有 IP 拦截 + DNS rebinding 缓解） | 3 天 | 无 |
| **P0d. 管理鉴权层**（`AdminRole` / `AdminAuth` extractor + `assert_system_admin` 守卫） | 3 天 | 无 |

### P1（下月）

| 项 | 估算 | 依赖 |
|----|------|------|
| **P1a. 消息生命周期追踪**（trace_id + `message_lifecycle` 表 + 延迟告警） | 5 天 | P0d |
| **P1b. 撤回功能**（`Recalled` variant + `ImService::recall_message` + 前端定时器） | 3 天 | 无 |
| **P1c. Web SPA 最小 TypeScript 化**（`ts-rs` 导出的 `.d.ts` + `ws.js` 帧类型安全 + `msg:interaction` handler 补全） | 5 天 | 无 |
| **P1d. AI 端点限流**（`/api/ai/summarize` 等添 per-user 5 req/min + 输入长度校验） | 2 天 | 无 |

### P2（本季度）

| 项 | 估算 | 依赖 |
|----|------|------|
| **P2a. Canvas 客户端 UI**（`web/canvas.js` 独立模块 + `GET/PUT /api/canvas` 集成 + 版本冲突提示） | 5 天 | P1c |
| **P2b. Admin 控制台 MVP**（审核工作台 + 工作区设置面板，复用现有 API） | 10 天 | P0d |
| **P2c. Feature flags 基础设施**（`feature_flags` 表 + Redis cache + `GET /api/me/flags`） | 3 天 | 无 |
| **P2d. NATS 声明式流配置 + `ensure_stream`** | 3 天 | 无 |

### P3（下季度）

| 项 | 估算 | 依赖 |
|----|------|------|
| **P3a. 生产 Dockerfile + Health probe 集成** | 2 天 | P0a |
| **P3b. 零停机迁移**（`--check` 模式 + 2 版本向后兼容原则文档化） | 3 天 | P0a |
| **P3c. NATS 3 节点 cluster + 副本策略** | 5 天 | P2d |
| **P3d. 前端组件化评估**（Lit/Web Component POC + Service Worker + 暗色模式） | 5 天 | P1c |

### 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| CI 集成测试开启后发现 flaky test | 高 | 中 | 先标记 flaky test 为 `#[ignore]` 单独 ticket，不阻塞 CI 合并 |
| TypeScript 化后发现帧类型定义与运行时不一致 | 中 | 高 | `ts-rs` 生成的 `.d.ts` 先 review，用 `ts-verify` 脚本做差异对比 |
| SSRF 防护阻止合法内部 URL | 中 | 低 | fail-open（阻止时 log + 不放 preview，不影响消息发送）+ `AERO_UNFURL_ALLOW_PRIVATE` 逃生口 |
| 管理控制台鉴权遗漏（IDOR） | 中 | 高 | admin 路由强制 `AdminAuth` extractor + AGENTS.md §4.1 的 `authz_lint` CI check 扩展到 admin 路由 |

---

**总结核心判断**：文档的 5 个方向都很扎实，但优先级需要重排。CI 集成测试、进程级状态审计和 SSRF 防护是**所有其他方向的前置条件**——没有它们，在上面投入的前端/DevOps/AI 限流都是在沙上建塔。CI 集成测试开启是当前性价比最高的单步改进：2 天的工作量，解锁合并前 schema 变更的安全保障，且不依赖任何人。
