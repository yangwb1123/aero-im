# 架构评估与跃迁路线：Aero IM 从「功能完备」到「可交付产品」

> 基于 `docs/2026-07-10-five-engineering-leaps.md` 及项目实际状态（16 crate / 1511 测试 / 87+ 功能点 / 149+ 迁移 / ~100KB 分析文档）

---

## 一、架构评估

### 1.1 核心架构优势

当前架构已经做出了若干**正确的长期决策**，值得肯定：

| 决策 | 为什么正确 | 证据 |
|------|-----------|------|
| **NATS JetStream 作为跨实例事实源** | 相比 Redis pub/sub，JetStream 提供持久化游标、at-least-once 语义、consumer group 水平扩。这是实时系统最关键的可靠性基石。 | Durable `aero-server` consumer + ephemeral `live.stream.*` 的双模式设计 |
| **进程内 Hub 扇出 + 外部 NATS 总线** | Hub 的 bounded `mpsc` 扇出防止单进程内 O(N²) 瓶颈；跨实例由 NATS 水平扩展。避免了「每房间一个 NATS subject」的爆炸。 | `hub.rs` + `run_bus_listener` 分离设计 |
| **Redis sorted-set 做集群状态** | 避免单进程内存状态（presence/roster/viewers）：心跳过期靠 `zremrangebyscore` 驱逐，天然支持多写。 | `live_presence.rs`、`CallRosterStore`、`StreamViewerStore` |
| **Crate 分层的严格单向依赖** | 基础层（`aero-common`/`aero-bus`/`aero-storage`）→ IM/直播层 → 组合层（`aero-server`）。无环。 | AGENTS.md crate 地图 |
| **MLS E2E 做不透明字节透传** | 正确的分层：服务端不参与客户端密码学，只透传 KeyPackage 和群状态。这是安全边界的最佳实践。 | `common/src/mls.rs` |

### 1.2 架构债务与隐患

| 类别 | 具体问题 | 严重度 | 技术债说明 |
|------|---------|--------|-----------|
| **测试架构** | 1511 单元测试但 0 端到端自动化 | **高** | 实时系统的核心契约（2 实例下消息去重、NATS 故障注入、持久 consumer 重投）完全没有自动化验证。`smoke.sh`/`smoke_p2.py` 是手动脚本，CI 中无任何 end-to-end 步骤 |
| **部署架构** | 无 Dockerfile、无 Helm、无 production docker-compose | **高** | 从 `docker compose up` 到生产需要跳跃的 gap 最大。`data/` root 权限 bug 是典型表征 |
| **前端架构** | 0 组件模型、0 测试、0 路由、0 状态管理 | **中** | 定位为「debug client」是合理的早期决策，但 87+ 功能点后，企业功能（SSO/2FA/审计/分析）全无 UI，成为交付瓶颈 |
| **知识管理架构** | 40+ 分析文档 / ~100KB 但无索引、无 ADR、无决策记录 | **中** | 每一次 agent session 都从零开始扫描，知识不累积。AGENTS.md 的 28KB 已达极限，`docs/requirments/` 的文件增长 > 代码产出 |
| **配置架构** | 单层 config.toml + AERO__ 环境变量 | **中** | 无 `config/production.toml`、无 vault 集成、无配置热加载。部署时需要硬编码敏感信息或手动凭 env 注入 |
| **API 文档架构** | 手写示意图 OpenAPI | **低** | AGENTS.md 明确说明「非全量契约」，但 150+ 路由、40+ WS 帧类型的系统没有契约文档，意味着每个新客户端集成（移动端、第三方 CLI 等）需要逆向工程 |

### 1.3 关键设计决策评估

**✅ 做对的：**

1. **NATS ≥ Kafka 的选择**：对于本系统规模（单集群 ≤50 节点），NATS 的运维简单度（无 ZooKeeper/KRaft）胜过 Kafka 的持久化能力。`max_deliver=16` + DLQ + at-least-once 的组合足够。

2. **str0m 而非 webrtc-rs**：纯 Rust DTLS-SRTP 避免了 C 库绑定（libwebrtc 的编译痛苦和线程模型冲突）。AGENTS.md 明确说明 str0m 声明在 `aero-live-whip`/`-webrtc` Cargo.toml，root 无 str0m——这是正确的隔离原则。

3. **ImService::assert_room_access 做统一鉴权收口**：单一守卫函数串接 room→workspace 成员→2FA 门→停用门。减少 IDOR 面。CI 有 `authz_lint` 兜底扫描。

**⚠️ 值得关注的：**

1. **AI 预算用加权估计而非真实 token**：budget.rs 用预定义权重（Embed1/Mod2/Sum3/Ans5），metrics.rs 同时记录真实 token。两个口径存在 gap——方向一的用量台账若基于 metrics 会与 budget 不一致。这是需要消解的。

2. **call-bridge 传输面已建但未接线**：「^2 媒体 seam 的零调用 builder」是设计上有意识预留的 seam，但 6 个 `truth-check` 标 UNWIRED 的 builder 意味着跨节点媒体在「测试覆盖了」和「生产可用」之间有 gap。AGENTS.md 的标注策略（「待联调」而非「完成」）是正确的，但需要明确的验收条件。

3. **AGENTS.md 约定 vs 实际状态**：文档称「819 个单元测试」，但 SESSION_HANDOFF.md 记录 1511。这不是 bug（测试在增加是好现象），但说明 AGENTS.md 的更新频率跟不上代码变化。

---

## 二、扩展方向

### 方向 1（P0）：生产部署就绪——容器化 + 配置分层 + K8s 部署

#### 为什么需要

这是 5 个跃迁中唯一一个**阻塞其他所有方向**的底层制约。没有 Dockerfile，意味着无法在 CI 中自动化运行集成测试（制约三）；无法在 production 中一键部署（制约四）；无法向客户演示或交付（所有方向）。

当前 `make up` + `cargo run` 的模式只适合开发者本地，任何一个外部利益相关方（测试人员、产品经理、客户、开源贡献者）都无法一键启动完整系统。

#### 核心挑战

1. **多阶段构建的缓存优化**：16 个 crate 的 Rust 工作区，`cargo build --release` 在 CI 中约 15-30 分钟。Docker 层缓存策略至关重要——先 copy `Cargo.toml` 做依赖层缓存，再 copy 源码。
2. **migration 作为 init container**：server 启动前必须先跑 `aero-cli migrate`。K8s 中需要 init container 执行 migration，然后 main container 启动 server。迁移失败的处理（回滚？重试？）需要设计。
3. **NATS cluster + durable consumer 的零停机部署**：当前 durable consumer `aero-server` 是单节点。多个 server 实例消费同一 durable 时，一个新实例加入后需要 drain 旧实例。`CancellationToken` 已实现优雅 drain，但需要验证 2+ 实例下的行为。

#### 预期架构变更

- 新增 `/Dockerfile`（多阶段构建：`rust:bookworm` → `debian:bookworm-slim`）
- 修改 `docker-compose.yml`：增加 `aero-server` 服务，加 `production` profile（Prometheus + Grafana，无 Jaeger）
- 新增 `charts/aero-im/`：Helm chart（Deployment + Service + ConfigMap + migration Job）
- 新增 `scripts/drain.sh`：NATS consumer drain 脚本
- 新增 `docs/deployment/production.md`

#### 对现有系统的影响

**低**。不修改代码，只增加部署基础设施。Dockerfile 是纯加法。唯一需要代码变更是让 `config.toml` 支持从环境变量分层加载（`AERO__DATABASE__URL` 已支持，只需文档化生产模式）。

---

### 方向 2（P1）：前端工程化基础——从 debug client 到产品 UI

#### 为什么需要

当前 Web SPA 是 0 测试、0 组件、0 路由的「有状态的 HTML 集合」。87+ 后端功能点中，企业级功能（SSO 配置、SCIM 管理、2FA 强制、法务保全、分析仪表盘、Webhook 管理、审批流配置）**全都没有前端 UI**。

这不是「前端不够漂亮」的问题，而是「产品无法交付」的问题——每个企业功能的交付周期 = 后端开发（1-2 天）+ 写 curl 文档（半天）+ QA 说服测试人员用 curl 验证（1 天）。而如果管理层需要 UI，则额外需要 3-5 天的前端开发。结果是**后端功能点完成 ≠ 产品功能可用**。

#### 核心挑战

1. **渐进式迁移而非大爆炸重写**：一次性重写 6K JS SPA 风险极高（没有前端测试基线，重构 = 重写）。正确路径：新页面（管理控制台）用新框架，旧聊天 UI 逐步迁移。
2. **框架选型**：
   - **Preact**（~3KB gzip）：React 生态兼容，但需要 JSX 编译（新构建工具）
   - **Lit**（~5KB）：Web Component 标准，零编译，但模板语法是 JS 模板字面量
   - **Svelte**（~1.6KB + 编译产物）：极致小，无运行时，但需要 Svelte 编译器
   - **放弃独立 SPA**：用 htmx + 服务端渲染（再引入一个后端路由层）
   - **建议**：Lit + Vite。理由：Web Component 是标准 API（非框架 lock-in），Vite 提供零配置的 HMR + 构建 + module preload 支持，Lit 的 `@lit/task` 提供声明式数据加载。迁移成本最低（旧页面保持原样，新页面用 Lit 构建）。
3. **模块合并与加载性能**：当前 15 个 `<script type="module">` 各自独立 HTTP 请求。Vite 构建后自动合并 + tree-shaking + 代码分割。先 `<link rel="modulepreload">` 热修复即可。

#### 预期架构变更

- 新增 `web/package.json` + `vite.config.js`
- 新增 `web/src/` 目录（新框架组件），`web/pages/`（管理控制台）
- 新增 `web/tests/`：至少 20 个测试（smoke + critical path + a11y）
- 旧文件（`web/app.js`、`web/chat.js`）逐步迁移
- `web/index.html` 从两个 `<script type="module">` 入口改为 Vite 构建入口

#### 对现有系统的影响

**中**。后端不需要变（已有完备 REST API）。前端会有一段「双框架共存」期——旧聊天 UI 走原生 JS，新管理控制台走 Lit。期间 eslint 配置需要支持两个范式。旧文件在迁移完成后可归档到 `web/legacy/`。

---

### 方向 3（P1）：CI 集成测试自动化——从「1511 单元测试」到「可信交付」

#### 为什么需要

当前测试格局的脆弱性：单元测试覆盖了「每个函数在隔离状态下是否正确」，但**实时通信系统的核心风险在于集成**——NATS 重投、WebSocket 扇出、多实例去重、持久 consumer 游标。这些全部在单元测试中不可模拟。

手动冒烟脚本（`smoke.sh`、`smoke_p2.py`、`smoke_live.py`）的存在本身就是该问题的证据——它们知道需要端到端验证，但没有自动化。

#### 核心挑战

1. **测试编排的复杂性**：端到端测试需要：启动 PG/Redis/NATS → 编译 server → 运行迁移 → 启动 server → 执行测试 → 停服 → 清理。在 CI 中需要容器化编排（docker compose）。`make ci-test` 需要每一步都有退出码检查和超时。
2. **多实例测试的状态管理**：两个 server 实例共享同一 NATS/Redis/PG，它们的消费者行为可能互相影响（durable consumer 集群模式 vs 独立消费）。测试用例需要精确定义「预期哪个实例收到哪个事件」。
3. **NATS 故障注入的工具缺失**：要测试 `max_deliver=16` 的 poison pill 行为、at-least-once 重投、consumer 暂停/恢复，需要 NATS 的管理 API（`$SYS.REQ.ACCOUNT.*`）或通过 nc 模拟故障。可考虑集成 `async-nats` 的 test-utils 或 `nats-server` 的嵌入式模式。
4. **前端 smoke 的 headless 依赖**：Playwright/Puppeteer 需要 Chromium binary，CI 镜像可能不包含。需要确保 `Dockerfile` 的构建阶段包含测试工具。

#### 预期架构变更

- 新增 `Makefile` 目标：`make ci-test`（一步集成测试）、`make smoke`（冒烟别名）
- 新增 `.github/workflows/ci.yml`（或类似 CI 配置）
- 新增 `tests/integration/` 目录（端到端测试脚本）
- 新增 `tests/multi-instance/` 测试（bash 或 Python）
- 新增 `web/tests/smoke.test.js`（前端 smoke）
- 修改 `docker-compose.yml`：增加测试 profile

#### 对现有系统的影响

**低**。纯加法，不修改生产代码。唯一风险是 CI 时间增加（完整端到端测试可能 10-20 分钟），可通过分层策略缓解：单元测试（快）→ 集成冒烟（中等）→ 多实例测试（慢）各自独立触发条件。

---

### 方向 4（P2）：知识管理基础设施——从「40 份分析」到「可导航的知识库」

#### 为什么需要

这是 5 个制约中「最软」但「最深」的一个。当前知识状态的核心问题不是「缺文档」，而是**文档的生产速度超过了消费速度**，导致：

- 同一方向被 12 个不同 session 独立分析（缓存一致性被覆盖 ~12 次）
- 重要决策（为什么不做 federation、为什么选加权估计而非真实 token）没有成文
- 新加入的 agent 或贡献者需要读 100KB+ 文档才理解系统

#### 核心挑战

1. **ADR 制度的执行成本**：架构决策记录（Architecture Decision Records）是最容易「开始写一周后放弃」的实践。关键不是模板，而是习惯——每次做架构决策时停下来写 5 行记录。建议：第一次 ADR 在 `docs/decisions/` 建立后钉一条 `/decision` slash 命令，允许从 Web UI 提交 ADR。
2. **过期分析文档的清理**：40+ 份分析文档中的方向被 ROADMAP 吸收后，原文档理论上应标记 `superseded-by` 并归档。但实际操作中，谁来判断「这份分析是否被 ROADMAP 覆盖」？ROADMAP 本身在变。正确策略：每份新文档反向链接它讨论的既有方向，而非 PR 审核者人工判断。
3. **跨 session 知识共享的机制**：当前的 `.claude/worktrees/` 各自独立。根本解决需要 agent 启动时载入 `docs/INDEX.md` 和最近的 `docs/session-logs/`。这是 agent 工具链的集成问题，而非纯文档问题。

#### 预期架构变更

- 新增 `docs/INDEX.md`（目录索引 + 状态标注）
- 新增 `docs/ARCHITECTURE.md`（一页纸架构概览）
- 新增 `docs/session-logs/` 目录 + 初始化日志
- 修改 `docs/decisions/DECISIONS.md` 或拆分为单个 `docs/decisions/NNNN-*.md` ADR 文件
- 修改 `.claude/settings.json`（若存在）：启动时提示载入 `docs/INDEX.md`
- 标记/归档 `docs/requirements/` 中过期的文件

#### 对现有系统的影响

**极低**。纯文档工程，不涉及代码变更。唯一瓶颈是：谁来做标记和归档？建议将这项工作纳入 agent 的 session 启动模板——每次 agent 启动时先读 `docs/INDEX.md`，session 结束时写一条 `docs/session-logs/` 日志。

---

### 方向 5（P2）：运营可观测性深化——从 Prometheus 指标到告警 + SLO + Runbook

#### 为什么需要

当前可观测栈：Prometheus 指标 + OTLP 链路 + 3 个 gauge sampler + HTTP RED 中间件。这覆盖了「看见现状」，但不覆盖「知道出问题了」和「知道怎么修」。

具体缺口：
- 无告警规则（PrometheusAlertManager / OpsGenie / PagerDuty 未配置）
- 无 SLO 定义（消息投递成功率 p99 < 500ms？可用性 99.9%？）
- 无 Runbook（PG 连接池打满怎么办？NATS consumer lag 飙升怎么办？）
- 日志聚合（当前 JSON 日志，但无 Loki/Splunk/Elastic 集成）

#### 核心挑战

1. **告警规则的目标维度**：在「功能完备」阶段，告警静默期（未定义 SLO）和告警疲劳之间需要平衡。建议初始阶段只覆盖：PG 连接池 > 80%、NATS consumer lag > 1000、`MESSAGES_SENT_TOTAL` 突降 > 50%。而非开箱即用的全量告警。
2. **Runbook 的知识源头**：运营知识分散在 AGENTS.md 和各个开发者的脑中。Runbook 需要基于真实故障场景编写，而非理论推导。建议：每一个生产宕机后写一个 Runbook，而非提前编写 50 个。

#### 预期架构变更

- 新增 `monitoring/alerts/prometheus-rules.yml`（AlertManager 规则）
- 新增 `monitoring/grafana/dashboards/`（dashboard JSON，当前也许已有）
- 新增 `docs/runbooks/`（故障处理手册）
- 修改 `docker-compose.yml` 的 `production` profile：增加 AlertManager
- 新增 `docs/operations/slo.md`：定义初始 SLO

#### 对现有系统的影响

**极低**。纯操作层配置，不修改代码。唯一可能代码级变更：为 `MaxDeliverExceeded` 等不可恢复错误增加结构化日志字段，便于告警匹配。

---

## 三、接口设计建议

### 3.1 Critical API 设计原则

根据 AGENTS.md 的系统骨架，以下接口设计原则应当强制执行：

**1. 鉴权守卫的统一签名**

当前 `assert_room_access(participant, room)` 的 participant-first 约定是好的。建议扩展到所有资源类型：`assert_call_access`、`assert_stream_access`、`assert_workspace_admin`。统一签名风格减少 IDOR 风险。

**2. 可观测性的结构化契约**

每个 NATS subject 生产者在消息中附加 `traceparent`（已实现，方向二已核实）、`producer_id`（哪个实例？）、`seq`（单调递增）。消费端在 `fan_out_raw` 时保留这些字段。这使端到端追踪在全链贯通——这是正确设计，应保持。

**3. 测试接口 vs 生产接口的分离**

当前 `#[cfg(test)]` 暴露的 `SfuMediaSession::bind`/`run` 是正确模式。建议标准化：所有「外部依赖 seam」（call-bridge、S3、push gateway、AI providers）都用 trait + `#[cfg(test)]` fake impl，而非 feature flag 或 env gate。确保测试时注入 fake、生产时注入 real。

### 3.2 是否需要新的抽象层

**不需要新的抽象层，但需要对既有抽象做两处强化：**

1. **EventBus trait（aero-bus）**：当前是 trait + NATS 实现。生产已够用，但测试时没有 `InMemoryBus` fake。新加 `MemoryBus`（用 `tokio::sync::broadcast` 模拟 subject 的发布/订阅）可以让集成测试不依赖 NATS 进程。这是「CI 集成测试自动化」方向的关键前置。

2. **Seam trait 化**（call-bridge/S3/blob/AI）：当前 S3 已 trait 化（`S3BlobStore`/`LocalFsBlobStore` 通过 `blob_store_from_env` 选择）。同样的模式应推广到：call-bridge（`BridgeFactory` trait，fake 实现 loopback）、AI providers（`AiBackend` trait，fake 实现 `HashEmbedder`——已部分实现）。

### 3.3 向后兼容性

当前系统处于 pre-1.0 阶段，向后兼容的约束较低。但有两个方向**从现在开始就需要契约化**：

1. **WS 帧格式（ServerFrame/ClientFrame）**：如果未来引入移动端 SDK 或第三方 bot SDK，WS 帧的 `kind` tag、payload 结构必须有版本号或向下兼容的 schema。建议：每个帧加 `"v": 1` 字段预留版本扩展点。

2. **NATS subject 命名**：`im.room.{id}` 和 `live.stream.{id}` 的结构已固化。未来若引入命名空间（多集群/数据驻留/联邦），需要 subject 前缀策略。建议：当前保持平展结构，但消费方处理未知 subject 时应跳过而非 panic。

---

## 四、技术选型

### 4.1 建议引入的依赖

| 领域 | 推荐 | 替代方案 | 选型理由 |
|------|------|---------|---------|
| 前端框架 | **Lit** + Vite | Preact, Svelte, htmx | Web Component 标准（零框架 lock-in）、Vite 零配置、`@lit/task` 提供声明式数据加载。旧页面不动，新页面迁移 |
| 前端测试 | **Playwright** | Puppeteer, Cypress | 跨浏览器、API 测试能力（可同时测前端 + 后端）、`playwright-test` 可零配置在 CI 中运行 |
| 端到端测试 | **pytest** + requests/websockets | curl 脚本, node.js | Python 已有 7 个 smoke 脚本（`smoke_p2.py` 等），换成 pytest 可复用现有逻辑 + 加夹具 + 加断言 |
| 容器编排 | **docker compose profiles** + Helm | Nomad, docker swarm | 项目已用 docker compose，profiles 是零学习成本的扩展；Helm 是 K8s 事实标准 |
| CI | 当前未知（GHA/GitLab/自建？） | — | 需要在根目录查 `.github/` 或 `.gitlab-ci.yml`。若没有，建议 GitHub Actions（与项目的公开仓库倾向一致） |

### 4.2 不建议引入的依赖

| 技术 | 为什么不需要 |
|------|-------------|
| **Kafka** | NATS JetStream 已覆盖持久消费需求。Kafka 运维复杂度（ZooKeeper/KRaft、重均衡、磁盘规划）与系统的当前规模不匹配 |
| **React** | 6K JS 的 SPA 引入 React（~40KB gzip + React DOM）显著膨胀。Lit 的 5KB gzip 更适合渐进式迁移 |
| **Terraform / Pulumi** | 在当前阶段（单集群，无多云），docker compose + Helm 足够。IAC 是 P3-P4 考虑 |
| **gRPC** | 全部客户端交互走 HTTP REST + WS + WebRTC，没有内服务间 RPC 增量。NATS 是服务间通信层，HTTP/2 是客户端通信层，gRPC 的引入需要独立的序列化和协议层 |
| **Redis Streams** | NATS JetStream 已覆盖。混用两个流式存储增加运维复杂度 |
| **OpenAPI codegen** | 150+ 路由的契约会生成~5000 行 OpenAPI spec。维护成本 > 收益。保持手写示意性文档是合理的 |

### 4.3 自建 vs 采购决策框架

对于本项目的 next leap，**99% 的能力应自建，0% 应采购 SaaS**。理由：

1. 这是开源项目（或准备开源？从 README 状态看是产品），自建是核心差异化的来源
2. AI-native IM 市场尚在早期，没有成熟的 SaaS 组件可采购（你能买到一个 AI 审核 API，但不能买到「按语义缓存答案+按难度分层路由」的 AI 网关）
3. 项目的核心价值是集成（IM + AI + 实时 + 企业合规），而非单个组件

唯一适合采购的边界：
- **TURN 服务**（Xirsys / Twilio / Metered）：SRTP 中继是带宽密集型的 infras，自建 TURN 服务器在 2026 年已不经济
- **SMS/邮件通道**（SendGrid / Twilio / Mailgun）：通知渠道不是差异化

---

## 五、实施路线图

### 优先级总览

```
P0 ─── 生产就绪 ───────────────────────────────────────
       （阻塞其他所有方向的底层制约）

P1 ─── 前端工程化 ──────  CI 集成测试 ─────────────────
       （用户体验障碍）    （交付质量基础）

P2 ─── 知识管理 ────────  运营可观测性 ────────────────
       （长期效率）         （运维成熟度）
```

### 阶段划分

#### 阶段 0（Week 1-2）：地基——生产就绪

| 任务 | 子任务 | 交付物 | 风险 |
|------|--------|--------|------|
| 0.1 Dockerfile | 多阶段构建 + 缓存策略 + `.dockerignore` | `Dockerfile` 存在，`docker compose up --build` 完整启动 | 16 crate 工作区的构建缓存策略需要调试 |
| 0.2 production docker-compose | 无 Jaeger profile + Prometheus + Grafana + 资源限制 | `docker-compose.prod.yml` | 监控栈的初始 dashboard 需要定义 |
| 0.3 `data/` 权限修复 | docker 卷映射非 root UID/GID | `make up` 不再产生 root-owned `data/` | — |
| 0.4 配置分层 | `config.production.toml` + vault 集成手册 | 生产部署文档 | vault 是推荐非必须；先走 env var |
| 0.5 Helm chart | Deployment + Service + ConfigMap + migration init container | `charts/aero-im/` | migration init container 的失败回滚策略 |

**退出条件**：`docker compose up --build -d` 无需 `cargo run`。外部贡献者可在 5 步内运行完整系统。

#### 阶段 1（Week 3-4）：质量——CI 集成测试

| 任务 | 子任务 | 交付物 | 风险 |
|------|--------|--------|------|
| 1.1 `make ci-test` | 编排：启动容器 → 编译 → 迁移 → 冒烟 → 停服 → 报告 | CI pipeline 中 `make ci-test` 通过 | CI 环境需要 docker-in-docker 或容器运行时 |
| 1.2 smoke 脚本 pytest 化 | 7 个 Python 脚本合并为 pytest 套件 | `tests/smoke/` + CI 中执行 | 现有脚本结构差异大，合并需要重构断言逻辑 |
| 1.3 前端 smoke test | Playwright 加载 index.html 无 404/JS 异常 | `web/tests/smoke.test.js` | Playwright browser binary 需要在 CI 中预置 |
| 1.4 多实例测试（基础） | 2 实例共享 NATS/Redis/PG，验证消息跨实例投递 | `tests/multi-instance/` 测试 | Docker 端口冲突处理、实例间时序不确定性 |
| 1.5 InMemoryBus | EventBus trait 的内存 fake，允许集成测试不依赖 NATS 进程 | `aero-bus/src/memory.rs` | 「无 NATS 的集成测试」可能漏测 NATS 特有行为（重投、backlog 等）——需要明确什么测试用 real NATS，什么用 fake |

**退出条件**：CI 中有 `make ci-test` 步骤。新 PR 合并前自动运行端到端冒烟。前端加载测试不抛异常。

#### 阶段 2（Week 5-7）：体验——前端工程化

| 任务 | 子任务 | 交付物 | 风险 |
|------|--------|--------|------|
| 2.1 引入 Lit + Vite | 创建 `web/package.json` + `vite.config.js` | `npm run build` 构建成功 | 旧 SPA 的 `.html` 引入方式需兼容 Vite 的构建产物 |
| 2.2 管理控制台 MVP | 第一个页面：SSO/OIDC 配置页（调用已有 REST API） | 管理员可通过 UI 配置 SSO | 后端 API 的 CORS 和 session 认证需一致 |
| 2.3 前端测试基线 | 20 个测试：smoke + critical path + a11y | `npx playwright test` 通过 | a11y 测试需要定义 WCAG 标准（建议 WCAG 2.1 AA） |
| 2.4 模块加载优化 | `<link rel="modulepreload">` + Vite 自动合并 | 首帧 HTTP 请求减少 3+ 倍 | — |

**退出条件**：第一个管理 UI 页面上线。JS 测试 ≥20。`web/` 目录有可复用的组件模式和测试框架。

#### 阶段 3（Week 8-9）：知识——管理基础设施

| 任务 | 子任务 | 交付物 | 风险 |
|------|--------|--------|------|
| 3.1 `docs/INDEX.md` | 目录 + 状态 + 核心结论 | 索引存在 | 维护索引的持续承诺——需要写 CI check 确保不 stale |
| 3.2 `docs/ARCHITECTURE.md` | 一页纸架构概览 | 架构概览存在 | — |
| 3.3 ADR 制度 | 首个 ADR（选择 NATS 而非 Kafka 的记录）+ slash 命令 | `docs/decisions/0001-*-*.md` | 制度执行成本——建议用 slash 命令降低门槛 |
| 3.4 分析文档归档 | 30+ 文档标记 superseded + 移至 `docs/archive/` | `docs/requirements/` 文件数下降 | 谁来判断 superseded？ROADMAP 负责，非 agent |
| 3.5 session-logs 初始化 | 首个日志 + agent 启动模板 | `docs/session-logs/2026-07-12-*.md` | 日志内容质量参差不齐——模板最小化（3 个关键洞察 + 2 个未解决问题） |

**退出条件**：新 agent session 启动时能通过 `docs/INDEX.md` 导航到所有活跃文档。`docs/requirements/` 不再新增文件（或由 AGENTS.md 明确例外条件）。

#### 阶段 4（Week 10-12）：运维——运营可观测性

| 任务 | 子任务 | 交付物 | 风险 |
|------|--------|--------|------|
| 4.1 告警规则 | AlertManager 规则（PG 连接池/NATS consumer lag/消息量突降） | `monitoring/alerts/prometheus-rules.yml` | 假阳性率——第一阶段只覆盖「明显出问题」的场景 |
| 4.2 初始 SLO 定义 | 消息投递 p99 < 500ms / 可用性 99.9% / AI 答案 p95 < 10s | `docs/operations/slo.md` | SLO 需要真实生产数据校准——先定义，再调整 |
| 4.3 关键 Runbook | PG 打满 / NATS backlog / server 崩溃 / migration 失败 | `docs/runbooks/` 4 份 | — |
| 4.4 日志聚合建议 | Loki 集成或 JSON 日志的 fluentd/vector 配置 | 操作文档 + docker-compose profile | Loki 是轻量选项，与 Prometheus 同源 |

**退出条件**：一个生产故障能被告警发现 -> 查 Runbook -> 执行恢复。时间从「用户报告后 30 分钟」降至「告警后 5 分钟」。

### 全局风险矩阵

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|---------|
| 分析继续膨胀（第 41 份分析文档产生） | **高** | 中 | 立即建立 `docs/INDEX.md` 和「写分析前必须回答 3 个问题」的 gate |
| 前端迁移陷入重写泥潭 | 中 | **高** | 严格「新页面用新框架，旧页面不动」的渐进策略。初始 MVP 只做 1 个管理页面 |
| Docker 构建缓存策略不当导致 CI 15-30 分钟 | 中 | 中 | 先做分层拷贝 `Cargo.toml` → 缓存依赖层 → 再拷贝源码 |
| CI 时间过长导致开发者跳过端到端测试 | 中 | **高** | 分层策略：单元测试（~2min）→ 集成冒烟（~5min）→ 多实例（~10min）。PR 阻断只阻断冒烟层 |
| 多实例测试的时序不确定性导致假阳性 | 中 | 中 | 加重试逻辑 + 超时边界 + 日志 dump。NATS 的 at-least-once 本质意味着不确定性在测试中会暴露——这是正确的事情 |
| ADR 制度在 2 周后放弃 | **高** | 低 | 低影响：5 个 ADR 也比 0 个强。不要追求完美制度，追求「启动」。第一个 ADR 写「为什么选 NATS」本身就有价值 |

---

## 六、补充建议

### 6.1 关于「分析停止线」的机制设计

文档提出的「分析停止线」是 5 个跃迁中最核心但最脆弱的制度——它要求 agent 自律。我建议增加机制约束而非仅靠意志力：

1. **`docs/INDEX.md` 文件作为 agent session 的入口门禁**：agent 启动时强制读 INDEX.md，INDEX.md 明确列出「已分析方向清单 + 每个方向的分析文档引用」。新分析创建前必须索引中查找——如果找到，diff 是什么？
2. **如果 agent 仍然写了第 41 份分析，那这份分析的第一节必须是「为什么已有 40 份分析不足以覆盖这个方向」**——把冲突证明的责任交给分析者。
3. **ROADMAP 对分析的「主权声明」**：ROADMAP 明确声明它对方向的优先级的唯一话语权。分析可以补充、质疑、建议，但不能替代 ROADMAP 作为执行计划。

### 6.2 关于 AGENTS.md 的更新策略

AGENTS.md 的 28KB 已接近阅读上限，且测试数据（819 vs 1511）已过时。建议：

1. 将测试数量等动态数据从 AGENTS.md 剥离到 `docs/operations/test-state.md`（CI 自动生成）
2. AGENTS.md 保持为**不变量文档**——只写架构约束和工程规则，不写动态计数器
3. 新增 `docs/operations/` 目录：部署状态、测试状态、配置变更日志，这些都是动态的，可以 CI 自动生成或人工维护

### 6.3 关于「5 个跃迁」的节奏控制

5 个跃迁并非并行启动。建议的执行节奏：

```
Week 1-2:   生产就绪（P0）
             └── 这是所有后续工作的地基
               
Week 3-4:   CI 集成测试（P1）+ 知识管理（P2 开始）
             ├── CI 测试依赖阶段 0 的 Dockerfile
             └── 知识管理是低强度并行任务（写 INDEX.md 只需 1 个 session）

Week 5-7:   前端工程化（P1）
             └── 管理控制台需要 CI 确保后端 API 兼容性

Week 8-12:  知识管理完成（P2）+ 运营可观测性（P2）
             └── Runbook 和 SLO 需要真实部署经验输入
```

这个节奏的关键前提是：**阶段 0 必须在 Week 2 结束前完成**。如果 Dockerfile 卡在构建缓存策略上超过 2 周，后续所有阶段的基础都不稳。

---

## 七、总结

Aero IM 在 16 个 crate、87+ 功能点、1511 测试的规模上已经是一个**功能极其丰富的实时通信平台**。当前阶段的核心矛盾不是「缺什么功能」，而是「已有的东西为什么还不能一键运行、一键测试、一键部署」。

5 个工程跃迁的优先级排序基于一个原则：**首先解除阻塞其他所有方向的那个制约**。那就是生产就绪（Dockerfile + 部署文档 + 配置分层）。没有它，CI 无法自动化端到端测试（依赖容器化），前端无法被外部测试（没有 staging 环境），运营无法获得反馈（没有生产数据）。

一旦地基打好，CI 集成测试和前端工程化可以并行推进——前者提升交付信心，后者提升用户体验。知识管理和运营可观测性作为长期的低强度并行任务，在 12 周内逐步完成。

**最关键的一个建议**：立即建立 `docs/INDEX.md` 和 `docs/ARCHITECTURE.md`。这两个文件本身只需要 2 小时，但它们的效果——阻止第 41 份分析文档的产生、让新加入者在 5 分钟理解系统——是 5 个跃迁中投入产出比最高的。
