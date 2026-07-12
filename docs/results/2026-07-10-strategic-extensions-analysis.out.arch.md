Now I have a thorough understanding of the project. Here's the architectural analysis:

---

# Aero IM 架构分析报告

> 基于项目源码结构（README.md、AGENTS.md）及输入文档的综合分析。
> 分析范围：16 crate 工作区，157 次迁移，~4700 行 web 资产，完整 IM+直播+SFU+SRT 能力栈。

---

## 一、现有架构评估

### 1.1 优势

| 层面 | 评估 |
|------|------|
| **分层清晰度** | 16 crate 按依赖自下而上排列（common → bus → storage → auth → im-core → live-* → server），无成环依赖。每个 crate 对应一个能力域，符合「feature = crate」治理原则 |
| **事件总线成熟度** | 双层扇出（NATS 跨实例 + Hub 进程内 bounded mpsc）是工业级实时架构。两个命名空间（`im.room.*` durable，`live.stream.*` ephemeral）的设计准确地反映了「聊天消息必须不丢 vs. 弹幕可丢」的语义差异 |
| **媒体面完整性** | str0m 纯 Rust DTLS-SRTP、SFU 选择性转发、Simulcast、RTCP、SRT HSv5 + AES-CTR — 这些都是**真实现代流媒体协议栈**的组成部分，而非 mock 或 scaffold |
| **测试覆盖** | 819 单元测试 + ~35 PG 门控 db_test + 30+ Python smoke 脚本。`truth-check.sh` 能抓出死代码和未接线组件，这是 CI 管线的高质量资产 |
| **可观测资产管理** | `monitoring/` 内有完整的 Prometheus recording rules + alert rules + Grafana dashboard，且规则**全部基于实际发射的 metrics**，不是样板。这是投产级资产，非「样例」 |
| **迁移即代码** | 157 次 SQL 迁移编译期嵌入二进制，`sqlx::migrate!("../../migrations")` 保证迁移与应用版本严格耦合 |

### 1.2 局限性

#### 1.2.1 缺少客户端幂等键（架构缺口，P0.5）

`send_message` 不接受 `client_message_id` 或等效的幂等令牌。这意味着：

- 移动端离线消息队列重送**必然产生重复消息**
- WebSocket 断开重连后重发未确认的 `send_message` 帧也无法去重
- 数据库层面没有 `(client_id, client_message_id)` UNIQUE 约束

**影响面**：这是移动端 SDK（方向一）的**先决条件**，也是任何生产级客户端 SDK 的基础设施。缺了这个，所有客户端侧的「重试逻辑」都会产生数据质量问题。

#### 1.2.2 缺少 `draining` 模式与蓝绿部署能力（SRE 缺口，P0）

`/health/ready` 探活 PG/Redis/NATS/Blob 但**没有 draining 模式**。kubernetes 滚动更新时：

1. 新 Pod 的 `readiness` 通过了（因为它只检查后端连接）
2. 旧 Pod 立即被 SIGTERM
3. 旧 Pod 上的 WebSocket 连接被**强制断开**，未等 drain
4. 客户端感知到重连抖动

标准做法是：`/health/ready` 收到 SIGTERM 后立即返回 503，让 k8s service 的端点控制器先摘掉 Pod，然后等待 `graceful_shutdown_timeout` 秒让长连接 drain，最终收到 `CancellationToken` 触发真正关闭。

#### 1.2.3 前端架构极简导致的复用困难

`web/` 目录共 ~4700 行（18 JS 文件 + 1 HTML）。按文件大小分布：

```
1009 app.js        (主控制器)
 685 render.js     (渲染)
 639 calls.js      (通话)
 336 api.js        (REST 客户端)
 260 ws.js         (WebSocket 协议)
 208 context.js    (上下文菜单)
 169 polls.js      (投票)
 164 media.js      (媒体)
 144 search.js     (搜索)
 141 notifications.js
 108 mentions.js
  96 modals.js
  92 livecards.js
  71 live.js
  71 auth_ui.js
  42 chrome.js
  34 emoji.js
```

这是一个**无框架、零依赖、直接操作 DOM** 的 SPA。优点是极致的轻量和加载速度；缺点是：

- 没有组件抽象（`app.js` 里散落大量 `addEventListener` 和 `innerHTML` 拼接）
- 没有状态管理（react 式 re-render 路径）
- 没有 virtual DOM 或 diffing
- 每个新视图需要写完整的 DOM 构建代码

**这对移动端的含义**：不可能在移动端复用任何逻辑。移动端 SDK 需要从零编写 REST/WS 客户端层，重写所有 UI。这和 React Native / Flutter 的代码复用场景完全不同——web 端不是「略作适配即可」的状态，而是一个**独立的、自包含的、不可跨平台的实现**。

#### 1.2.4 迁移仅单向（技术债）

`sqlx::migrate!("../../migrations")` 只执行前向迁移。157 个迁移文件没有任何一个包含 `-- DOWN:` 注释块。`aero-cli migrate down` 不存在。

在快速迭代期这还可行。但在 P0 生产就绪之前，必须建立 rollback 能力——至少需要一个 `migrate down 1` 命令。迁移回滚不是「不上线就没必要」，而是**发布流程的最低要求**——当 P0 bug 出在迁移中的数据约束变更时，不是「回滚代码」就能解决的，必须回滚 schema。

#### 1.2.5 审计日志有结构但缺少汇聚查询

`audit.rs` 有分区表结构（迁移 0154），但缺少：
- 按时间范围/用户/操作的聚合查询端点
- 报表导出（CSV/JSON）
- 预设审计报表模板（登录趋势/数据导出记录/角色变更历史）

合规审计人员需要的不是 SQL 查询能力，而是预设报表 + 按需导出。这是一个「数据已有、查询能力缺失」的典型缺口。

---

### 1.3 关键设计决策评估

| 决策 | 评价 |
|------|------|
| **事件总线双层扇出** | ✅ 优秀。NATS 做跨实例事实源，Hub 做进程内扇出，既避免了单点瓶颈，又实现了横向扩展 |
| **集群状态走 Redis sorted-set** | ✅ 正确。presence/观看数/通话 roster 都是天然可丢弃的瞬时状态，Redis 的 TTL + zremrangebyscore 恰好匹配心跳驱逐模型 |
| **SFU roster 走进程内存而非 Redis** | ✅ 正确。SFU 转发面需要纳秒级查找，Redis 往返延迟不可接受。但需要注意 SFU 重启后的重建逻辑 |
| **assert_room_access 单一守卫函数** | ⚠️ 可维护性风险。单一函数承担了：workspace 解析 + 成员检查 + 停用检查 + 强制 2FA 检查。每个新条件都在增加函数复杂度。考虑拆为 strategy chain |
| **call-bridge 明文 RTP** | ⚠️ 跨节点 RTP 没有 DTLS-SRTP 保护。在内网场景可接受，但跨公网节点时需要在 bridge_frame 层加加密 |
| **1009 行单文件 app.js** | ❌ 严重。这是启动阶段正确的设计（最小可行），但在 24 波功能迭代后应该已经完成了模块化分解。继续积累逻辑在单文件里会拖慢所有前端工作 |

---

## 二、扩展方向

### 方向 A：客户端幂等键层（P0.5，**移动端先决条件**）

#### 为什么需要
移动端在网络不稳定时会自动重试未确认的请求。没有幂等键，每条重试都是重复消息。这不是用户体验问题——这是**数据损坏**。

#### 核心挑战
1. **幂等键的生命周期**：客户端生成的 UUID → 服务器去重存储 → 多久可以清理？
2. **幂等响应缓存**：重试时不能返回「重复错误」，必须返回**与首次成功完全相同的响应**（包括服务器分配的 `MessageId`、`created_at`）
3. **幂等键的唯一性约束**：(client_id, client_message_id) 联合 UNIQUE 还是全局 UNIQUE？
4. **现有消息的兼容**：已经存在的 157 次迁移 + 无数历史消息，不加幂等键的旧客户端怎么办？

#### 建议方案

```
分层部署，不破坏现有路径：

1. 存储层：message_idempotency_keys 表（幂等键 TTL = 7 天，后台清扫）
2. 服务层：send_message 新增可选参数 client_message_id: Option<String>
   - 有幂等键 → 检查/记录/返回缓存结果
   - 无幂等键 → 走原有路径（向后兼容）
3. 响应缓存：成功响应的完整 JSON body 存在幂等键表里（或 Redis 短期缓存）
4. 幂等键清理：定时器清扫过期记录（幂等键不需要永久保留）
```

#### 架构影响
- `aero-im-core` 的 `send_message` 签名变更
- 新增仓储 `IdempotencyRepo`
- 迁移添加 `message_idempotency_keys` 表
- WS 帧 `send_message` 新增可选字段（serde `#[serde(default)]` 保向后兼容）
- **对现有系统影响极低**（全部新代码，不修改现有路径）

---

### 方向 B：OAuth2 授权服务器（P1）

#### 为什么需要
当前认证是**同名认证**（JWT 包含 participant_id），不是**授权服务器**。OAuth2 让第三方应用能以用户身份访问 API，而无需接触用户密码/JWT。OAuth2 是：

- 移动端 SDK 的标准认证方式（PKCE + 授权码）
- Webhook 和 SCIM 的标准认证升级路径
- 集成平台（Slack/Teams 竞品）的入场券

#### 核心挑战
1. **scope 编码策略**：当前 JWT 只有 `sub`（用户 ID）和 `kind`（access/refresh），没有 scope。需要定义 scope 命名空间。建议借鉴 Matrix 的 `m.` prefix + 自定义 scope 组合
2. **assert_room_access 的 scope 叠加**：当前 `n()` 函数处理 participant + room → workspace → membership → deactivation → 2FA。scope 不应该侵入这个逻辑，而应该作为**守卫之后的外层过滤器**：`scope_check(claims.scopes, method, path) -> bool` 中间件
3. **PAT 机制的复用**：`pat.rs` 已有 bearer token 验证管线（PatVerifier trait → PatRepo），可以直接扩展为 OAuth2 access_token 的存储方案。PAT 是长期有效的静态令牌，OAuth2 access_token 是短期有 scoped 的令牌——两者可以共享存储模型但区分类型
4. **PKCE 强制要求**：RFC 7636 的 S256 是移动端 public client 的必须项。纯授权码流程对于可以反编译的 mobile app 是不够的

#### 建议方案

```
阶段 1（P1.0）：授权码 + PKCE + scope 基础
  - 迁移：oauth_clients / oauth_authorization_codes / oauth_tokens 表
  - JWT 扩展：Claims 增加 aud/scopes 字段（可选，向后兼容）
  - 端点：GET /oauth/authorize  GET/POST /oauth/token  POST /oauth/revoke
  - scope_check 中间件：方法路径 → 所需 scope → 校验 claims.scopes
  - 复用 PatVerifier：OAuth2 tokens 和 PAT 共用 token_hash 查找

阶段 2（P1.5）：token introspection + 客户端管理 UI
  - RFC 7662 token introspection 端点（用于微服务间验证）
  - 管理 UI：注册/吊销 OAuth2 clients，管理 scope
  - 可选的 client_credentials grant（机器对机器）
```

#### 对现有系统的影响

| 组件 | 影响 |
|------|------|
| `aero-auth` JWT | Claims 加 `scope: Vec<String>`（`#[serde(default)]` 向后兼容）→ 中等 |
| `PatVerifier` | 扩展为通用 `TokenVerifier` trait（支持 OAuth2 token + PAT）→ 低 |
| `assert_room_access` | 不修改，新增 `scope_guard` 外层中间件 → **无影响** |
| WS 层 | 当前 WS 用 JWT 握手，新增 OAuth2 token 可共存 → 低 |
| 现有 API 客户端 | 继续用 JWT/PAT 不受影响 → **无影响** |

---

### 方向 C：生产级 SRE 就绪（P0）

#### 为什么需要
项目已有高质量的可观测资产（monitoring/ 目录），但缺少部署运维的基本设施：

1. **draining/蓝绿 readiness**（见 §1.2.2）
2. **docker-compose 生产化**：当前 `docker-compose.yml` 能跑但缺少 log shipper、backup sidecar、健康检查
3. **Helm chart**：k8s 部署的标准态
4. **迁移回滚**（见 §1.2.4）
5. **CI runner 激活**：`.github/workflows/ci.yml` 配置完整但 runner 未接

#### 核心挑战
1. **迁移回滚的实现难度被低估**：157 个迁移都没有 `-- DOWN:` 注释块。如果要加，需要：
   - 写脚本修改所有 157 个文件，追加 down 迁移
   - 或者只在新迁移起始就要求双向迁移，旧迁移只允许回滚到最近 N 个
   - 验证 `sqlx migrate revert` 是否兼容运行时的 `sqlx::migrate!()`
2. **draining 模式需要 WebSocket 层配合**：`hub.fan_out_raw` 需要 drain 循环把剩余帧发完再关。同时 `/health/ready` 必须切换到 draining 状态
3. **Helm chart 需要容器化策略**：当前没有 Dockerfile（`Dockerfile*` 不存在），需要先编写多阶段构建

#### 建议路线图

```
步骤 1（P0.0）：CI runner 接入 + 冒烟通过
步骤 2（P0.1）：Dockerfile + docker-compose.prod.yml（backup + log shipper）
步骤 3（P0.2）：draining readiness + 迁移双向
步骤 4（P0.5）：Helm chart + blue-green 部署策略
```

---

### 方向 D：WORM 合规归档（P1，条件性 P0——面向金融客户）

#### 为什么需要
FINRA 17a-4 / SEC Rule 17a-4 要求某些记录在保留期内**不可修改、不可删除**。当前：

- `message_history` 记录变更但无法阻止硬删
- 保留清扫跳过「被法务保全的消息」，但技术层面 DB admin 仍然可以 truncate
- 导出格式为 JSON，不是 PST/MBOX

#### 核心挑战
1. **WORM 存储层**：需要 append-only blob store，文件写入后即不可修改/删除（直至保留期满）。`BlobStore` trait 可以新增 `WormBlobStore` 实现：
   - 写：append-only（不允许覆盖/删除）
   - 读：正常
   - 删：仅在 retention lock 期满后允许
   - 实现方案：本地 FS 用 chattr +i / S3 用 Object Lock
2. **合规归档的查询面**：审计人员需要按时间/用户/操作类型搜索归档内容。这不能直接查生产 DB（生产 DB 是可变状态），需要独立的归档存储
3. **导出格式标准化**：JSON 对技术人员友好，但对合规审计人员不是。PST/MBOX 是 eDiscovery 工具的标准输入格式

#### 建议方案

```
层次 1：WORM blob store
  - BlobStore trait 新增 WormBlobStore 实现
  - 迁移：compliance_archives 表（指向 blob 的指针 + 保留期 + retention lock ID）
  
层次 2：归档管线
  - 生产 DB → 归档管道：新消息异步写入 WORM 存储
  - 清扫跳过已归档消息
  
层次 3：eDiscovery 导出
  - 按查询范围导出 PST/MBOX
  - 审计 UI（简单的日期/用户/房间过滤）
```

---

### 方向 E：WebSocket 网关层抽象（P2，但为移动端铺路）

#### 为什么需要
当前 WebSocket 协议（`ws/ws_impl/` 的 `ClientFrame` / `ServerFrame`）是聊天专用的。移动端 SDK 需要：

1. **连接恢复**：掉线后恢复未确认的消息 + 拉取掉线期间的增量事件
2. **增量同步**：`?since=` 端点，按 seq 拉取缺失事件
3. **心跳/保活**：标准 WS ping/pong
4. **协议版本协商**：服务端告知客户端最低/推荐协议版本

这些需要 WebSocket 层引入**通用网关抽象**，而不是逐个路由加适配。

#### 核心挑战
1. **增量同步的状态存储**：每个 room 的 seq 是 per-subject 单调递增的，但客户端可能只关心特定 room 的增量。需要 `since` cursor 的存储和查询
2. **断线重连的消息确认**：客户端重连后，需要知道哪些 `send_message` 已被服务端接受（通过 `client_message_id` 的响应确认）
3. **协议版本演进**：WS 帧的 `kind` tag 是 serde tagged enum，新增 variant 是后向兼容的。但移除/重命名 variant 需要版本协商

---

## 三、接口设计原则

### 3.1 幂等优先

所有写操作（`send_message`、`edit_message`、`delete_message`、`react`）都应该接受可选的幂等键。

```
原则：写操作 = 幂等键 + 参数
      读操作 = 参数（无需幂等键）
      幂等键 = UUID v4（客户端生成）
      幂等响应 = 完整的成功响应（与首次相同）
```

### 3.2 可选的扩展参数通过 `#[serde(default)]` 保向后兼容

现有 API 消费者（web SPA、已有集成）不受影响。新参数全部为 `Option<T>` 或 `#[serde(default)]`。

```
原则：新功能 = 可选参数 + 缺省行为与旧版一致
      旧客户端 = 不感知新字段，不改变行为
      新客户端 = 渐进式采用新字段
```

### 3.3 Guard 函数分层而非膨胀

当前 `assert_room_access`（`n()` 函数）承担过多职责。建议拆分层级：

```
层级 0：身份认证（AuthUser extractor — 已有）
层级 1：授权守卫（scope_check 中间件 — 新建）
层级 2：资源守卫（assert_room_access / n() — 精简）
层级 3：租户守卫（workspace membership — 保留）
层级 4：合规守卫（legal hold / info barrier — 外部叠加）
```

每层职责单一、可独立测试、可单独绕过（如健康检查端点）

---

## 四、技术选型建议

### 4.1 现有技术栈不变量

| 组件 | 已确定 | 不可替换的理由 |
|------|--------|---------------|
| 数据库 | Postgres 17 + pgvector + pg_trgm | 157 次迁移、pgvector RAG、pg_trgm 搜索 |
| 缓存 | Redis 7（fred 9） | presence、roster、限流全部基于 sorted-set |
| 消息总线 | NATS JetStream | 双层扇出架构、durable consumer 容错 |
| 通话媒体 | str0m（纯 Rust DTLS-SRTP） | 整个 SFU/WHIP/SRT 栈耦合于此 |
| 前端 | 零依赖 ES2020 | 手动 DOM 操作不可迁移，但这是既成事实 |

### 4.2 需要评估的新技术

| 候选 | 用途 | 评估标准 | 推荐 |
|------|------|---------|------|
| `sqlx-cli` migrate revert | 迁移回滚 | 需要验证与运行时 `sqlx::migrate!()` 的兼容性 | **试用**，看是否能同时支持运行时和 CLI 迁移 |
| 轻量 OAuth2 服务器 crate | 授权服务器 | 是否支持 PKCE S256、scope、token introspection、client_credentials | **评估** OpenIDConnect 或自制（因 PAT 机制可复用） |
| k6 / locust | 负载测试 | 与现有 WS 协议兼容（NATS 排队的背压行为） | **推荐 k6**，JS 脚本与现有 smoke 风格一致 |
| `rpassword` / `secrecy` | 密钥管理 | 没啥，已有 env 配置 | 不引入，当前 env 够用 |

### 4.3 自建 vs 采购

| 决策点 | 建议 | 理由 |
|--------|------|------|
| OAuth2 授权服务器 | **自建**（基于 PAT 机制 + JWT 扩展） | 已有完整的 JWT 签发/验证 + PAT bearer 验证管线。OAuth2 的核心代码量在 400-600 行 Rust（token 签发 + scope 校验 + 授权码流），采购的 Ory Hydra 是 Go 服务，引入运维复杂度 |
| WORM 合规归档 | **自建**（BlobStore trait 新实现） | 存储层是当前 BlobStore trait 的自然扩展。合规归档全生命周期（写锁、保留期管理、期满解锁）在 Rust 里 500-800 行可完成。商用合规归档方案（如 GovCloud）成本高且与当前存储模型不匹配 |
| 移动端 SDK | **自建** | 第三方 IM SDK 无法适配 Aero 的私有 WS 协议 + call-bridge 媒体面。但建议分层：**薄客户端（REST/WS 协议层 + 本地缓存）** 先出，UI 层交给社区 |
| 工作流引擎 | **暂缓**（P3） | 当前连 webhook 生态都未成熟。等 OAuth2 平台就位、第三方 API 调用模式确立后再做 |

---

## 五、实施路线图

### 优先级矩阵

```
        高影响 ──────────────▶ 低影响
    高
    急    P0: SRE 生产就绪          P0.5: 客户端幂等键
    切    P0: CI runner 投产
    性
    │
    │    P1: OAuth2 授权服务器      P1.5: WORM 合规归档
    │    P1: 迁移双向回滚
    ▼
    低    P2: WebSocket 网关抽象     P3: 工作流引擎
    紧    P2: 移动端 API 适配
    迫
    性
```

### 路线图（三个里程碑）

#### 里程碑 1：「不可变的基石」（估计 3-4 周）

| 序号 | 项目 | 产出 | 风险 |
|------|------|------|------|
| 1.1 | CI runner 接入 | GitHub Actions 全绿（check + test + clippy + smoke） | 低 |
| 1.2 | 客户端幂等键 | `send_message` 支持 `client_message_id`，向后兼容 | **中**（需验证幂等响应缓存的 TTL 策略不导致存储膨胀） |
| 1.3 | 迁移双向 | `sqlx migrate revert` 验证 + 新迁移强制 `-- DOWN:` | **高**（157 个旧迁移的回滚策略需要慎选：全量补 down 还是只覆盖最近的 N 个） |
| 1.4 | Draining readiness | `/health/ready` 摘除 + WS drain 循环 | 中 |

**验收标准**：
- CI 全绿（包括 clippy 零新警告）
- 幂等键可防止同一 client_message_id 的重复消息
- `aero-cli migrate down 1` 可逆向上一次迁移
- 滚动更新时 WS 连接无"暴力断开"

#### 里程碑 2：「平台开放」（估计 6-8 周）

| 序号 | 项目 | 产出 | 风险 |
|------|------|------|------|
| 2.1 | OAuth2 基础 + PKCE | 授权码流 + token 端点 + scope 校验 | **中**（Scope 命名策略需要跨团队达成一致；当前 JWT 的 aud 字段未使用，复用需确认无冲突） |
| 2.2 | PAT 扩展为 TokenVerifier | OAuth2 token 和 PAT 共享验证管线 | 低 |
| 2.3 | WebSocket 协议版本协商 | WS 握手时协商协议版本 + 心跳 | **中**（协议版本更新后，旧版本 WS 客户端需被优雅拒绝而非静默失败） |
| 2.4 | 增量同步端点 | `GET /api/sync?since={seq}` | 中 |

**验收标准**：
- 第三方客户端可通过 OAuth2 授权码流获取 token 并调用 API
- 移动端可通过 `GET /api/sync` 实现"离线事件拉取"
- WebSocket 协议版本不匹配时返回协商错误

#### 里程碑 3：「生产强化」（估计 4-6 周）

| 序号 | 项目 | 产出 | 风险 |
|------|------|------|------|
| 3.1 | Dockerfile + docker-compose.prod.yml | 多阶段构建 + backup sidecar + log shipper | 低 |
| 3.2 | Helm chart | k8s 部署 + blue-green 策略 | **中**（需要确定 readiness probe 的 draining 行为与 k8s 的 lifecycle hook 配合无误） |
| 3.3 | 负载测试框架 | k6 脚本覆盖 WebSocket 发送 + REST 高频请求 | **中**（需要模拟 NATS JetStream 背压下的扇出行为；过于简单的脚本可能测不出真实的背压链） |
| 3.4 | WORM 归档层（P1.5） | BlobStore 新实现 + eDiscovery 导出 | **中**（金融客户可能需要先出 MVP；无金融客户则延迟） |

**验收标准**：
- `k6` 脚本可模拟 1000 并发 WS 连接的消息发送，系统无 OOM/backlog 爆炸
- Helm chart 支持蓝绿部署，滚动更新 WS 零中断
- WORM 归档可写不可删（本地 FS 验证 `chattr +i`，S3 验证 Object Lock）

### 风险缓解矩阵

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| 157 次迁移补 down 的工作量过大 | **高** | 迁移回滚无法实现 | 选择「只从当前迁移 N 号开始强制 down；旧迁移只做前向」策略，而非全量重写 |
| OAuth2 scope 策略与现有 RBAC 冲突 | 中 | 授权模型混乱 | 在 `aero-auth` 内独立定义 scope，不与 `channel_roles` 的 RBAC 交叉。scope = 「API 权限」，RBAC = 「房间资源权限」，两者正交 |
| str0m 更新导致 SFU 不兼容 | 低 | 通话功能退步 | str0m 是纯 Rust 库且在 cargo deny 管理下，版本更新可控 |
| 移动端 SDK 客户端幂等键实现后，旧客户端重试仍产生重复 | 中 | 数据质量 | 幂等键是**可选参数**——旧客户端不传幂等键，现有逻辑不变。数据质量提升是渐进的 |

---

## 关键架构决策记录（ADR）

### ADR-1：幂等键采用 `(client_id, client_message_id)` 联合唯一

- **状态**: 提议
- **上下文**: 唯一性约束需要在全局范围还是 per-client 范围？
- **决策**: per-client。全局唯一需要分布式 ID 生成器，per-client 使用客户端生成的 UUID v4 即可保证全局唯一。`(client_id, client_message_id)` UNIQUE 约束在 PostgreSQL 中高效。
- **后果**: 幂等键表以 client_id 为分区键，查询效率高。不需要集中 ID 分配器。

### ADR-2：Scope 认证叠加在 `assert_room_access` 之后

- **状态**: 提议
- **上下文**: Scope 校验应该修改现有的 `n()` 守卫函数吗？
- **决策**: 不修改。将 scope 校验实现为**中间件层**（`scope_guard(required_scope) -> middleware`），在路由 handler 中先调用 `assert_room_access`（验证用户可访问房间），再调用 `scope_guard`（验证用户拥有该 API 的权限）。两者正交。
- **后果**: `n()` 函数不需要感知 scope，scope 策略可以独立演进。每个路由 handler 需要显式标注 `scope_guard`，而不是自动继承。

### ADR-3：迁移回滚策略——只向前覆盖最近的 N 个迁移

- **状态**: 提议
- **上下文**: 157 个旧迁移完全没有 `-- DOWN:` 注释块。全量补写不可行（可能破坏已在生产运行的迁移状态）。
- **决策**: 
  1. 从迁移 N+1 开始强制要求 `-- DOWN:` 双向迁移
  2. 旧迁移（1..=N）只做前向，不做回滚
  3. `migrate down` 命令只允许回滚新增的迁移
  4. N 值由运维团队决定（建议 N = 当前迁移总数 - 10，保留 10 个可回滚的缓冲）
- **后果**: 迁移代码更规范但无法回滚 2026 年之前的迁移。对于投产半年内的项目这是可接受的风险。

---

## 总结

这个项目展现了一个**异常扎实的 Rust 实时系统**：NATS 双层扇出、str0m 媒体面、Redis sorted-set 集群状态——这些都是工业级选择。16 个 crate 无环依赖、819 个单元测试、157 次编译期迁移——工程质量在 Rust 生态中属上流。

**三个最紧迫的架构缺口**：

1. **客户端幂等键**（P0.5）——移动端 SDK 的先决条件，任何重试机制的前提
2. **生产级部署基础设施**（P0）——Dockerfile + draining + 迁移双向 + CI runner
3. **OAuth2 授权服务器**（P1）——平台化的入场券，可以基于已有的 PAT/JWT 资产以较低成本实现

前端（`web/app.js`）的 1009 行单文件 monolith 是已知领域中最薄弱的环节，但考虑到这是一个零依赖 SPA 且 4703 总行数覆盖了完整 IM+直播 UI，它完成了「极简再现」的设计目标。移动端适配时应**从零编写**，而不是试图重构这 1009 行。
