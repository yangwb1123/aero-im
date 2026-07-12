以下为基于文档 `2026-07-11-five-engineering-transitions-from-prototype-to-product.md` 的全方位架构分析。

---

# 架构分析：Aero IM — 从原型到产品的工程跃迁

## 1. 架构评估

### 1.1 现有架构的核心优势

Aero IM 的架构在「功能丰富度」维度做出了几个非常正确的设计决策，值得肯定：

| 决策 | 优势 | 原因 |
|------|------|------|
| **事件驱动的跨实例扇出**（NATS → Hub → WS） | 水平扩展透明、职责清晰 | 事件溯源（Event Sourcing）模式天然适合 IM/直播场景；`im.room.*` durable + `live.stream.*` ephemeral 分离保障了投递可靠性分层 |
| **Crate 分层严格**（基础 → IM → 直播 → 组合） | 依赖方向明确，编译边界清晰 | `aero-common` 为纯叶子，无反向依赖；16 个 crate 无循环依赖，这在大中型 Rust 项目中是难得的成就 |
| **集群状态走 Redis sorted-set** | 跨实例一致性不依赖进程内存 | 关键状态（presence、roster、viewer count）通过 Redis 一致性共享，避免多实例缓存漂移 |
| **预算约束的 AI 子系统**（CostBudget + Semaphore + DLQ） | 成本可控、水平扩展安全 | `AiWorker` 的 weighted budget + `SKIP LOCKED` 轮询是经过深思熟虑的设计——不因 AI 后端抖动拖垮系统 |
| **多协议直播摄入**（RTMP/WHIP/SRT → HLS） | 兼容主流推流器 | 摄入口统一 `LiveIngest` trait，底层实现可替换 |

### 1.2 架构债务与技术债

文档识别的 5 个制约，从架构角度可归因为三类债务：

#### 类别 A：工程基建债务（制约三、四）

这是最危险的一类——它们影响**交付质量与运维成熟度**。

| 债务项 | 严重程度 | 技术体现 |
|--------|----------|----------|
| **无 CI 集成测试** | 🔴 高 | `ci.yml` 是注释模板，integration-test job 被注释掉，smoke 脚本全部手动。819 个单元测试覆盖了函数级逻辑但未覆盖多实例、NATS 故障、at-least-once 重投等关键分布式可靠性契约 |
| **无容器化 server** | 🔴 高 | 缺少 `Dockerfile`，部署路径只有 `cargo run`。这与生产环境的 container orchestration（K8s/Nomad）之间存在断层 |
| **多实例测试缺失** | 🟡 中 | 核心架构卖点是水平扩展，但从未验证过 2+ 实例下的正确性。持久 consumer 的 at-least-once 语义、去重、缓存一致性——这些是**分布式系统的关键不变量**，没有测试覆盖 |

**本质问题**：后端架构设计正确但缺乏「验证设计正确性的工程管线」。这不是功能缺口，而是**测试架构缺口**。

#### 类别 B：前端架构债务（制约二）

这是最影响用户体验的一类——后端有企业级功能但前端无法呈现。

| 债务项 | 严重程度 | 技术体现 |
|--------|----------|----------|
| **零组件模型、零路由、零测试** | 🔴 高 | `render.js` 用字符串拼接 DOM，19 个模块无任何测试。对于 87+ 个 API 端点、40+ WS 帧类型，重构风险极高 |
| **管理控制台空白** | 🟡 中 | SSO/SAML/SCIM/2FA/审计/法务保全等企业功能后端完备，但前端 UI 全缺。这意味着系统对非技术管理员「不可用」|

**本质问题**：README 将 SPA 定位为「debug client」但功能矩阵已远超 debug client 的合理范畴。定位与现实的错位导致前端工程投入系统性不足。

#### 类别 C：知识管理债务（制约一、五）

这是影响团队效率的隐性债务——复杂度在增长但知识基底没有同步演进。

| 债务项 | 严重程度 | 技术体现 |
|--------|----------|----------|
| **分析文档膨胀失控**（372 份） | 🟡 中 | 没有冲突消解、没有优先级排序、没有「决定不做」的记录。每次新的 agent session 独立分析已覆盖的领域——这是典型的**协调失败** |
| **架构决策不记录** | 🟡 中 | ADR 只有 2 条（DECISIONS.md）。为什么选择 meshed WebRTC 而非复用 SFU？为什么 AI 预算用加权估计而非真实 token？这些核心决策靠源代码注释承载，不可搜索、不可追溯 |
| **新贡献者上手成本高** | 🟢 低 | 读 README(32KB) + AGENTS.md(28KB) + design spec + ROADMAP + 多份分析文档才能理解全局。缺少 5 分钟架构概览 |

### 1.3 关键设计决策合理性回顾

| 决策 | 合理 | 备注 |
|------|------|------|
| NATS JetStream durable consumer（at-least-once） | ✅ | 正确——IM 消息丢失不可接受。但缺少故障注入验证 |
| 直播事件用 ephemeral consumer | ✅ | 正确——弹幕丢失可接受，关注吞吐而非可靠性 |
| AI 预算用 weighted 计数而非真实 token | ⚠️ 合理但有隐患 | 文档已识别「weight vs real token 偏差」是一个已知 seam。当 AI 用量增长后需校准权重 |
| 前端零构建工具 | ❌ 不再合理 | 在 87 个 API、40+ WS 帧的规模下，零构建工具的选择已成为交付瓶颈。当初「debug client」的定位已不适用 |
| `Hub` 进程内 mpsc 扇出 | ✅ | 正确的单进程扇出选择——不引入 intra-process 消息队列，保持低延迟 |
| 跨节点媒体走 call-bridge（明文 RTP） | ⚠️ 可接受但需关注 | 明文 RTP 仅在服务器间传输，客户端腿仍然是 DTLS-SRTP。安全边界清晰。但缺少 DTLS 服务器间加密意味着跨数据中心传输不安全 |

---

## 2. 扩展方向

### 方向一：部署与运维成熟度工程化（P0）

**为什么需要**：这是当前从「原型」到「产品」最直接的阻塞点。没有容器化 server 意味着：
- 无法在 CI 中自动运行集成测试
- 无法在 K8s/Nomad 中部署
- 无法做蓝绿发布、滚动更新、自动扩缩容
- 开发环境的 `data/` 权限摩擦反复影响开发者体验

**核心挑战**：
1. 二进制体积：`aero-server` 依赖 C 链接（libpq、libssl），多阶段构建需精确控制 stage
2. 迁移时序：`aero-cli migrate` 必须作为 init container 在 server 启动前执行。需确保幂等
3. 配置注入：`config.toml` + 环境变量混合，容器化后需明确配置分层

**预期架构变更**：
- 新增 `Dockerfile`（多阶段：builder → runner）
- `docker-compose.yml` 增加 `aero-server` 服务
- 新增 `charts/aero-im/` 目录（Helm chart）
- 新增 `docs/deployment/production.md`
- `data/` 卷权限修复（docker-compose 映射 1000:1000 而非 root）

**对现有系统的影响**：无侵入性变更。纯新增文件 + `docker-compose.yml` 修改。风险极低。

---

### 方向二：前端工程化基线（P0）

**为什么需要**：后端企业功能以 `#[deprecated]` 的速度在增长（SSO + SCIM + 2FA + 审计 + 法务保全 + 分析仪表盘），但没有 UI 意味着这些功能对于非 curl 用户不可见。前端是整个系统的**交付瓶颈**。

**核心挑战**：
1. **渐进式迁移**：不能一次性重写 5.9K JS。选择渐进式框架（Preact/Lit），新页面用新框架、旧页面逐步迁移
2. **测试从零到一**：第一次引入测试框架需要选择适合 ES module 零构建现状的方案（Playwright / Vitest / uvu）
3. **管理控制台优先**：企业功能的管理表单（SSO 配置页、SCIM 配置页、审计日志查看页）提供 UI 价值远高于打磨聊天界面的像素

**预期架构变更**：
- `package.json` 加入第一个前端框架依赖（Preact + HTM 或 Lit）
- 新增 `web/test/` 目录 + 测试运行器配置
- 新增管理控制台页面（`web/admin/` 子目录），新页面使用新组件模型
- 旧聊天 SPA 保持原样，标记为 `legacy/`，通过路由过渡
- `web/index.html` 增加 `<link rel="modulepreload">` 优化关键模块加载

**对现有系统的影响**：
- 旧页面无需改动，新页面走新框架 → 无回归风险
- 但需引入第一个 npm 非 dev 依赖（框架），这是之前故意避免的——需要在 AGENTS.md 中更新约束说明

**技术选型建议**：

| 选项 | 框架体积 | JSX 需求 | 构建工具 | 渐进迁移 | 评估 |
|------|----------|----------|----------|----------|------|
| **Preact + HTM** | ~3KB gzip | 无（HTM 用 tagged template） | 无（原生 ES module） | ✅ | **推荐**——最小变动，零构建步骤，`htm` 使 JSX-like 语法在模板字符串中 |
| **Lit** | ~5KB gzip | 无 | 无 | ✅ | 次选——Web Component 标准，但声明式渲染不如 Preact 熟悉 |
| **Svelte** | ~1.7KB gzip | 无 | 需编译器 | ❌ | 需编译器，不符合「零构建」约束 |
| **React** | ~42KB gzip | 需 JSX → 需构建工具 | 最小 Rollup/esbuild | ❌ | 过度——42KB 对于「管理控制台 UI」而言过于重 |

---

### 方向三：CI 集成测试管线（P1）

**为什么需要**：这是**交付质量的安全网**。一个实时通信系统在以下场景下没有自动化验证：
- 多实例下的消息去重
- NATS 故障后的重投与 poison pill
- 跨实例缓存一致性
- 40+ WS 帧类型的前端兼容性

**核心挑战**：
1. **环境依赖**：测试需要 PG + Redis + NATS + 已迁移的 schema。CI 中需要 services 矩阵
2. **多实例编排**：启动两个 server、验证正确的扇出行为，需要进程管理
3. **NATS 故障注入**：测试 `max_deliver=16` 超限后的 dead-letter、NATS 断连后重连

**预期架构变更**：
- 新增 Makefile target：`make ci-test` 全链路
- 将已有 Python smoke 脚本碎片整合为可重复测试套件
- `ci.yml` 取消 integration-test job 注释，增加 services 定义
- 新增 `tests/multi_instance.py` 或 Rust 集成测试（需编译到 server binary 作为测试 harness）

**对现有系统的影响**：
- 整合 smoke 脚本可能需统一认证机制（脚本们使用不同的 API 路径假设）
- 基础设施无变更，纯自动化改进

---

### 方向四：知识管理基础设施（P1）

**为什么需要**：372 份分析文档、2 条 ADR、无索引——当前的知识结构不是资产而是负债。每个新 session 花 30-60 分钟扫描已有文档才能确认不重复劳动。

**核心挑战**：
1. **冲突消解**：40+ 份分析的方向重叠需要人工整理。无法自动合并
2. **ADR 习惯养成**：决策记录需要纪律。机制上要在 AGENTS.md 中要求「决策前查索引、决策后写 ADR」
3. **归档不是删除**：旧分析文档不能丢失信息，需标记 superseded-by 后移至 `docs/archive/`

**预期架构变更**：
- 新增 `docs/INDEX.md`（所有文档目录索引 + 类型 + 状态 + 一句话结论）
- 新增 `docs/ARCHITECTURE.md`（一页架构概览）
- 新增 `docs/decisions/ADR-003.md` 等（ADR 记录制度）
- 将 40+ 份分析文档中已 superseded 的移动至 `docs/archive/`
- 新增 `docs/session-logs/`（agent session 学习日志）

**对现有系统的影响**：无代码变更。纯文档操作。

---

### 方向五：AI 成本治理进阶（P2）

**为什么需要**：当前 AI 预算系统（`CostBudget` + weighted 计数）是一个合理的一期实现。但当用量增长后，weighted 预算的偏差会累积——一个 "高成本" 的 Summarize（weight=3）可能实际消耗 5x cost，而 Embed（weight=1）可能只需 0.5x cost。

**核心挑战**：
1. **真实 token 计量**：从 weighted 估算迁移到 Anthropic API 返回的 `usage` 字段真实 token 计数
2. **budget 校准**：需动态调整 per-kind weight 以匹配近期的实际 token 消耗
3. **多模型路由**：`claude-sonnet-4` vs `claude-haiku-3.5` 的成本差异可达 5-10x。需根据问题复杂度选择模型

**预期架构变更**：
- `AiService` 增加 token 计量返回
- `CostBudget` 从静态 weight 表改为动态校准（定期从 DB 读取历史 token 消耗计算新权重）
- 新增 `ai_model_router` 模块：短问题 → Haiku，长/复杂 → Sonnet，嵌入 → Voyage
- 考虑引入 `answer_cache`（相同问题 hash 命中后复用，减少 API 调用）

**对现有系统的影响**：
- `AiWorker` 的 job claim + budget 计算逻辑需修改，但接口可向后兼容
- 新增 DB 表 `ai_token_usage`（记录 per-job token 消耗）
- 现有 weighted 预算作为 fallback 保留

---

## 3. 接口设计建议

### 3.1 关键模块的接口原则

| 模块 | 当前接口类型 | 设计原则 | 建议 |
|------|-------------|----------|------|
| **ImService** | 方法集合（`publish_room_event`, `assert_room_access` 等） | 单一职责 | ✅ 合理的 facade 模式。保持 |
| **Hub** | `fan_out_raw` + `stream_watchers` | 最小接口 | ✅ Bounded mpsc + WS 扇出职责清晰。保持 |
| **EventBus (NATS)** | trait（`aero-bus` crate） | 依赖反转 | ✅ 已抽象为 trait，可 mock。保持 |
| **AiService** | trait（`answer_question`, `moderate`, `transcribe` 等） | 功能内聚 | ⚠️ 当前是同步 trait + 内部异步，建议统一返回 `Result<AiResponse>` |
| **LiveIngest** | trait（`aero-live-core`） | 策略模式 | ✅ RTMP/WHIP/SRT 统一 trait。保持 |
| **BlobStore** | trait（`LocalFs` / `S3BlobStore`） | 策略模式 | ✅ 按 env 选后端。保持 |

### 3.2 需要引入的新抽象层

| 缺失的抽象 | 为什么需要 | 建议 |
|-----------|-----------|------|
| **`TestHarness`** | 端到端测试（尤其是多实例测试）需要统一的启动/停止/断言接口 | 不应在 38 个 Python 脚本中各自实现。应有一个 Rust `#[cfg(test)]` 的 `TestHarness` struct，封装 server 启动、client 注册、WS 连接、消息断言 |
| **前端 `ApiClient`** | 当前每个 JS 文件直接 `fetch()` + 硬编码路径。API 路径变更时无法集中处理 | 封装一个 `ApiClient` 类（或模块），所有 HTTP 请求和 WS 通信通过它。切换 base URL、注入 auth header、统一错误处理 |
| **前端 `Router`** | 当前 `showAuth()`/`showChat()` 切换 `hidden` | 无需全功能路由库。一个 50 行的 hash-based router（`window.addEventListener('hashchange', ...)`) 即可支持浏览器前进/后退和深层链接 |

### 3.3 向后兼容性

当前项目没有对外部消费者的 API 版本承诺（没有 `v1/` 前缀），但以下策略应当文档化：

1. **WS 帧格式变更**：`RoomEvent` 的 serde tag `kind` 不能改——已有 web 端 `event.call_kind || event.kind` 兜底。要新增字段用 `Option` 或 `#[serde(default)]`
2. **REST API 路径变更**：新路径和旧路径应该共存至少一个版本周期（或直接全量替换——当前无外部客户，过度设计无意义）
3. **AI 接口签名变更**：`AiService` trait 是 crate 内部接口，改签名的成本低——只需修改所有实现者和调用者。编译器保障

**建议**：不必过早引入版本化 API。但应在 `docs/decisions/` 记录当前策略：「API 不稳定优先交付至 1.0，1.0 前允破坏性变更」。避免日后纠结。

---

## 4. 技术选型

### 4.1 是否需要引入新技术栈

| 领域 | 当前 | 建议 | 理由 |
|------|------|------|------|
| **前端框架** | 无 | Preact + HTM | 零构建、~3KB、渐进迁移 |
| **前端测试** | 无 | Playwright (e2e) + Vitest (单元) | Playwright 可无头运行，支持多浏览器；Vitest 与 ES module 兼容 |
| **容器化** | 无 Dockerfile | multi-stage Dockerfile | 标准做法，builder → distroless |
| **K8s 部署** | 无 | Helm chart | 标准做法，但初期可用 docker-compose profile 替代 |
| **API 文档** | 手写示意性 OpenAPI | 暂不引入自动生成 | 项目处于 pre-1.0，接口仍在快速演化。1.0 后用 `utoipa` 或 `aidez` |
| **前端构建工具** | 无 | 暂不引入 | Preact + HTM 可零构建运行。等迁移规模扩大后引入 esbuild 做代码分割 |

### 4.2 第三方依赖评估标准

当前 `deny.toml` 已有安全检查。建议补充以下评估维度：

| 维度 | 标准 | 判断示例 |
|------|------|----------|
| **维护活跃度** | 最近 12 个月有 release | Preact ✅ / Svelte ✅ / 某小众库 ❌ |
| **Rust 生态兼容性** | 支持 MSRV 1.80 + tokio + axum 0.7 版本范围 | str0m 0.19 ✅ |
| **二进制体积影响** | 依赖的链接代价 | serde ✅（已大量使用）/ rusqlite ❌（链接 SQLite 到 IM server？不合理）|
| **传输安全** | 不引入未审计的加密原语 | 手写 AES-CTR（`aero-live-srt`）已有，但建议用 `ring` 或 `aes-gcm` |
| **License 兼容性** | 与项目 MIT/Apache 2.0 兼容 | 注意 AGPL 库（需单独评估）|

### 4.3 自建 vs 采购决策

| 方向 | 自建 | 采购/引入现成 | 建议 |
|------|------|--------------|------|
| **前端框架** | — | Preact/Lit/Svelte | 引入现成——这不在项目的核心竞争优势内 |
| **CI runner** | — | GitHub Actions | 引入现成——ci.yml 已写只需挂钩 runner |
| **K8s 部署模板** | Helm chart | 无成熟现成（项目定制化高） | 自建——但可参考 `charts/` 社区模板 |
| **邮件/通知通道** | 自建（集成已存在） | SendGrid/Mailgun/SES | 已整合——保持现有后端集成 |
| **AI 模型** | — | Anthropic/Voyage | 引入现成——保持 `ApiService` abstraciton，允许多 provider |
| **监控告警** | 自建 Grafana dashboard | Grafana（开源自建） | 自建——已有 dashboard JSON，只需部署配置 |
| **移动端原生 SDK** | 自建 | Flutter / React Native | 明确出范围——`AGENTS.md` 已声明 Web 优先 |

---

## 5. 实施路线图

### 5.1 优先级矩阵

基于影响面 × 实施成本排序：

```
高影响 ▲
       │
       │   P0: 部署成熟度       P0: 前端工程化
       │   (Dockerfile +        (组件模型 +
       │    docker-compose)     管理控制台 UI)
       │
       │   P1: CI 集成测试      P1: 知识管理
       │   (smoke 自动化 +      (INDEX.md +
       │    多实例测试)           ADR 制度)
       │
       │   P2: AI 成本治理
       │   (真实 token 计量 +
       │    多模型路由)
       └───────────────────────────► 低努力 ──────────── 高努力 →
```

### 5.2 阶段划分

#### 阶段 1：快速获胜（1-2 天）→ P0 中的低努力项

| 任务 | 预计工时 | 交付物 |
|------|----------|--------|
| 编写 `Dockerfile`（多阶段） | ~1h | `Dockerfile` 存在，`docker compose up --build` 可用 |
| `docker-compose.yml` 增加 server 服务 | ~30min | 一键启动完整系统 |
| 修复 `data/` 权限 | ~15min | `make up` 不再产生 root 目录 |
| 新增 `docs/INDEX.md` | ~30min | 全部文档目录索引 |
| 新增 `docs/ARCHITECTURE.md` | ~45min | 一页架构概览 |
| 新增 `docs/decisions/ADR-003.md` 模板 | ~15min | ADR 制度就位 |

**里程碑 M1**：`docker compose up --build && make migrate` 一键运行完整系统；文档可导航。

#### 阶段 2：测试基建（2-3 天）→ P1 核心

| 任务 | 预计工时 | 交付物 |
|------|----------|--------|
| `make ci-test` 目标（启动容器 → 编译 → 迁移 → 冒烟 → 停服） | ~2h | 单命令全链路 |
| 整合 38 个 smoke 脚本为自动化套件 | ~3h | 关键路径（CRUD 消息 + WS 扇出 + 直播弹幕）自动化 |
| CI runner 接入 + integration-test job 启用 | ~1h | PR 合并前自动跑集成测试 |
| 第一个多实例测试（bash/Python） | ~2h | 验证跨实例扇出正确性 |

**里程碑 M2**：CI 中有 `make ci-test` 步骤，PR 被阻断于冒烟失败。

#### 阶段 3：前端工程化（3-5 天）→ P0 核心

| 任务 | 预计工时 | 交付物 |
|------|----------|--------|
| 引入 Preact + HTM（零构建） | ~30min | `package.json` 新增依赖，`web/` 开始使用组件 |
| 引入测试框架（Playwright + Vitest） | ~1h | 第一个前端测试通过 |
| hash-based 路由 | ~1h | 浏览器前进/后退支持、深层链接 |
| 第一个管理控制台页面（SSO 配置） | ~3h | 管理员可通过 UI 配置 OIDC/SSO |
| `ApiClient` 封装 | ~1h | 所有 JS 模块统一 fetch + WS 入口 |

**里程碑 M3**：管理控制台上线，前端测试覆盖关键路径。

#### 阶段 4：知识整理（1-2 天，与阶段 2/3 并行）→ P1 补充

| 任务 | 预计工时 | 交付物 |
|------|----------|--------|
| 归档 superseded 分析文档 → `docs/archive/` | ~2h | `docs/requirements/` 文档数从 372 降为 ≤50 |
| 新增 ADR 条目（至少 5 个核心决策） | ~1h | ADR 003-007 覆盖关键架构决策 |
| 新增 `docs/session-logs/` | ~15min | agent session 学习日志机制就位 |

#### 阶段 5：AI 成本治理（1-2 天，独立）→ P2

| 任务 | 预计工时 | 交付物 |
|------|----------|--------|
| 真实 token 计量（从 `usage` 字段读取） | ~2h | `ai_token_usage` 表 + 日志 |
| 动态 weight 校准（定期从历史消耗计算） | ~2h | 预算系统自适应 |


### 5.3 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|----------|
| **前端框架引入后破坏零构建约束** | 中 | 中 | 选 Preact + HTM（无构建步骤），非 Svelte。保持 `scripts/web-check.sh` 兼容 |
| **Docker 构建时间过长**（依赖 C 链接） | 中 | 低 | 多阶段构建 + cargo chef 缓存依赖层。CI 中可缓存 `target/` |
| **CI runner 接入慢** | 高 | 中 | 先本地可重复 `make ci-test`，CI runner 是 bonus。风险可控 |
| **38 个 smoke 脚本整合后发现路径假设冲突** | 中 | 中 | 逐步整合。先覆盖核心路径（smoke.sh + smoke_p2.py），其余脚本标记 deprecated 后归入 `scripts/legacy/` |
| **管理控制台 UI 开发后触发前端重构讨论** | 高 | 低 | 明确范围：**只做管理表单**。不重构聊天 UI。避免 scope creep |
| **372 份分析文档的归档工作过量** | 高 | 低 | 分批（先归档 2026-07 前的、REJECTED 的、superseded 的）。无需一次性 |


### 5.4 关键架构决策里程碑

```
时间
│
├─ [M1] 部署成熟度
│   ├─ Dockerfile 构建通过
│   ├─ docker compose up --build 一键启动
│   ├─ docs/INDEX.md + docs/ARCHITECTURE.md 存在
│   └─ ADR 003（容器化决策记录）
│
├─ [M2] CI 测试管线
│   ├─ make ci-test 单命令全链路
│   ├─ CI integration-test job 激活
│   ├─ smoke.sh 自动化
│   └─ 多实例测试首次通过
│
├─ [M3] 前端工程化
│   ├─ Preact + HTM 依赖引入
│   ├─ 首个管理控制台页面上线
│   ├─ 前端测试 ≥20 个
│   └─ ADR 004（前端框架选择记录）
│
├─ [M4] 知识治理
│   ├─ docs/requirements/ 文档数 ≤50
│   ├─ ADR 记录 ≥7 条
│   └─ session-logs 机制运行
│
└─ [M5] AI 成本成熟
    ├─ 真实 token 计量上线
    ├─ 动态 weight 校准
    └─ ADR 005（AI 成本模型决策记录）
```

---

## 总结

这份文档（2026-07-11）的诊断是准确的：Aero IM 的 5 个制约都不是「还缺什么功能」，而是「已有的东西为什么还不能交付」。从架构视角看，核心共识是：

1. **后端架构设计质量高**——事件驱动、crate 分层、集群状态走 Redis、AI 预算约束，都是正确的设计决策。不需要推翻重来。

2. **最大的架构债不在代码层，在工程管线层**——无容器化、无 CI 集成测试、无前端工程基础。这些是**组织级工程成熟度的缺失**，不是代码质量的问题。

3. **最快的价值交付路径是 P0 的两翼齐飞**：
   - **左翼**：Dockerfile + docker-compose（~1.5h）→ 打通部署管线
   - **右翼**：docs/INDEX.md + ARCHITECTURE.md + ADR 制度（~1.5h）→ 打通知识管线

   这两项互不依赖，可并行。合计约 3h 即可完成架构评估报告中的最高优先级行动项。

4. **前端工程化的转折点到了**：在 87+ 功能点、372 份分析文档之后，继续坚持「零构建工具」策略的成本已经超过了收益。建议采用 **Preact + HTM** 这一最小破坏路径——零构建、渐进迁移、管理控制台优先。

5. **严格防止第 41 份分析的产生**：文档最大的价值不在其内容，而在其结论——「分析的产能应转为实现的产能」。这是一个工程管理决策，不是技术决策。建议在 AGENTS.md 和 `.claude/settings.json` 中实施分析停止线。
