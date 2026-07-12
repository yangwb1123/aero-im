# 架构师分析报告：Aero IM 系统工程债审计

> 分析范围：`aero-im` 全仓库，聚焦 Rust 后端 ↔ Web SPA 前端的架构缝隙
> 基于跨语言合约校验（Rust `ServerFrame` emit → JS `ws.on` consume）和运行期数据面验证

---

## 1. 架构评估

### 1.1 当前架构优势

Aero IM 的架构在**后端层面**是成熟且先进的：

| 属性 | 当前状态 | 评价 |
|------|---------|------|
| **事件驱动** | NATS JetStream durable/ephemeral consumer 分层 | ✅ 正确的异步解耦模式 |
| **进程内扇出** | Hub bounded mpsc → WebSocket | ✅ 避免了跨进程瓶颈 |
| **集群状态** | Redis sorted-set，非进程内存 | ✅ 无状态水平扩展 |
| **媒体层** | str0m SFU + 跨节点 call-bridge | ✅ 纯 Rust、DTLS-SRTP 端到端 |
| **数据持久化** | sqlx + 迁移编译期嵌入 | ✅ 工程纪律良好 |
| **可观测性** | Prometheus metrics + OTLP | ✅ 运营就绪 |
| **权限模型** | assert_room_access + member_role 双守卫 | ✅ 清除 IDOR 风险 |
| **迁移优先** | 迁移 → 仓储 → 路由 → 鉴权 → 实时，五步配方 | ✅ 可复用的架构模式 |

**后端架构评级：A-（0.5 扣在 bot secret 空串和 Interaction 广播无定向）**

### 1.2 架构债务分布

```
债务集中度热力图：

          轻量    中等    严重    关键
Rust 后端    ████████████████░░░
Redis 状态   ████████████████████
NATS 总线    ████████████████████
Web SPA     ░░░░░░░░░░░░░░░░████  ← 债务密度最高
合约边界    ░░░░░░░░░░░░░░██████  ← 缺口密度最高
```

**核心发现：架构债务不均匀分布。90% 的后端架构是健康的，但前端（Web SPA）和边界（跨语言合约）的债务密度在「关键」级别。**

### 1.3 三处关键设计决策评估

#### D1: 「零依赖 ES2020 SPA」（无框架、无 bundler、无类型系统）

**原始动机**：避免 NPM 依赖爆炸、减小包体积、简化 CI。

**2023 年的评估**：合理的取舍。对于原型 / MVP，零依赖 SPA 从 0 到上线最快。

**2026 年的评估**：**已成为关键架构债**。
- 11 个 JS 文件 → 11 次 HTTP 往返，无 HTTP/2 push
- 64 处 `textContent` 赋值 → i18n 改造成本 = 替换所有字符串 + 基础设施
- 全局状态单例（`context.js`）→ 状态与 UI 紧耦合
- 0 类型检查 → `ws.on('msg:interaction')` 缺失的 root cause

**当前建议**：这是正确的阶段性选择（MVP → 产品化），但**已经超越了它的适用窗口**。

#### D2: 「CallMode 只分 P2p / Sfu，没有 Mesh」

**原始动机**：简化路径，单节点 mesh 延迟低于 SFU。

**2026 年的评估**：正确的权衡被写死了。`decide_call_topology()` 让单节点永远走 mesh，**不是代码问题，是枚举设计问题**——`CallMode` 枚举当前只有 `{P2p, Sfu}`，没有表达「mesh 是 <=4 人的默认策略，Sfu 是 >=5 人的策略」的语义。

**架构修正建议**：枚举应改为 `CallMode { Mesh, Sfu }`，由 `CallOrchestrator` 策略层决策用哪个，而不是由拓扑函数隐含。

#### D3: 「Bot webhook 无 per-subscription secret」

**原始动机**：简化实现，使用空 HMAC secret。

**2026 年的评估**：**这是严重的设计妥协**。`build_delivery(&sub.webhook_url, "", &body, now)` 的 `""` 意味着：
- HMAC 签名对给定 payload 是确定性的（无 key = 无随机性）
- 任何能捕获 webhook URL 的攻击者可以伪造 `X-Hub-Signature-256`
- 接收方无法区分合法请求和重放

**修复路径**：给 `bot_subscriptions` 表加 `secret` 列，迁移补丁生成随机 32 字节，bot 管理 UI 支持 secret 轮换。

### 1.4 技术债分类账

| 债项 | 类型 | 严重程度 | 自哪个阶段 | 预计修复工时 |
|------|------|---------|-----------|------------|
| `Interaction` 无定向广播 | 功能缺陷 + 性能 | **Crit (DoS 放大)** | 初始引入 | 0.5 天（Rust）+ 1 天（JS） |
| Bot webhook 空 secret | 安全缺陷 | **Crit (无签名)** | 初始引入 | 0.3 天（迁移 + Rust） |
| 群通话 mesh-only | 架构债务 | P1（产品力） | 通话功能加入 | 2 天（JS 信令）+ 0.5 天（Rust 枚举） |
| Web SPA 单文件架构 | 架构债务 | P1（可维护性） | 早期迭代积累 | 5 天（分拆 + bundler） |
| JS 无类型检查 | 工程债务 | P2（效率） | 初始决策 | 3 天（TypeScript 迁移） |
| 审计 UI 缺失 | 功能缺口 | P1（合规） | 从未实现 | 2 天（纯 UI） |
| Bot 管理 UI 缺失 | 功能缺口 | P2（平台化） | 从未实现 | 3 天 |
| 中文硬编码字符串 | i18n 债务 | P2（产品化） | 早期迭代 | 伴随方向二 |
| CI 无前端类型检查 | 流程债务 | P2（工程纪律） | 初始缺失 | 0.5 天（eslint 配置升级） |

---

## 2. 扩展方向

> 基于输入文档的验证 + 我的架构视角补充

### 方向一：跨语言合约检验【P0 → Crit】

**为什么需要：**
这是被系统性忽略的盲区——分析者只看 Rust 或只看 JS，导致 emit 端和 consume 端之间存在静默的合约断裂。它不只是一个功能缺口，它是**工程过程的缺陷**（没有合约验证机制）。

**技术价值：**
- 消除无声的「广播浪费」（当前 `Interaction` 发给房间全员）
- 使 ws.on handler 注册成为可审计的
- 减少带宽和客户端处理开销

**核心挑战：**
1. **静态验证**：Rust `ServerFrame` 枚举 → JS `ws.on('msg:*')` handler 的映射需要一个外部工具或 lint 规则。当前没有任何机制保证两端匹配。
2. **重构阻力**：每新增一个 `RoomEvent` variant 都要记得在 JS side 加 handler——这个「记得」是不可靠的。

**架构变更：**

```
现状：
  ServerFrame::Interaction { ... }  →  Rust: publish_room_event() → fan_out_raw
                                     →  JS: ws.on('msg:message') 处理 {message}, 但忽略 {interaction}

目标方案 (A) — 手动合约文档 + CI 校验（推荐）：
  ┌─────────────────────────────────────────┐
  │ ws_contract.md（或 ws_contract.rs）       │
  │ - 每个 ServerFrame variant               │
  │ - 对应的 JS handler 文件名 + 行号          │
  │ - explicit_recipients 策略              │
  │ CI 脚本 grep 两边生成缺失矩阵              │
  └─────────────────────────────────────────┘

目标方案 (B) — 全自动（不推荐，过度工程）：
  Rust 端导出类型定义 JSON Schema → JS 端 CI 验证 handler 覆盖
  成本：引入 schema 生成依赖 + 维护费用
```

**对现有系统的影响：**
- 方向一本身改动小（Rust 1 文件 + JS 2 文件）
- 但引入的**合约检验机制**会影响所有未来的 `RoomEvent`/`StreamEvent` 增加
- 需要纳入 AGENTS.md §4 的全局约束

### 方向二：Web SPA 架构可维护性重构【P1 — 分三个阶段】

**为什么需要：**
当前架构是「单 HTML 文件 + 11 个全局 JS 文件 + 全局状态单例」——这个模式在 100 行逻辑时有效，在 3000+ 行时已成为维护负担。

**核心挑战：**

1. **render.js 的混合职责**（685 行）——这是最大的阻碍。纯渲染、渲染调度、交互绑定混在一个文件里，使得任何单一修改（如 i18n 的 `textContent` 替换）都必须覆盖整个文件。
2. **全局状态单例**（`context.js`）——所有模块直接读写同一个全局对象，没有变更通知机制，没有局部重渲染能力。
3. **无 bundler**——11 个 JS 文件 = 11 次 HTTP 请求，首次加载延迟高，无 tree-shaking，无代码分割。

**三个阶段重构路线：**

```
阶段一 (3 天): 引入 bundler + 模块化
  ├── 目标：零架构变更，只加构建步骤
  ├── 方案：esbuild（零配置，快，输出仍然是 ES2020）
  ├── 风险：极小——esbuild 几乎兼容任何 JS
  └── 产出：1 个 bundle.js → HTTP 请求从 11→1

阶段一.五 (2 天): 渲染架构拆分 ← 关键插入阶段
  ├── 目标：拆分 render.js 的混合职责
  ├── 产出：
  │   render/msg.js         — renderMsg(), renderEdited(), renderDeleted()
  │   render/blocks.js      — renderBlock(), renderButton(), renderSelect()
  │   render/stream.js      — renderStreamCard(), StreamStatusBar
  │   render/sidebar.js     — renderSidebar(), renderRoomList()
  │   render/admin/         — audit, legal holds, compliance
  └── 关键设计决策：render return HTML string vs DOM node?
      建议：返回 DocumentFragment（性能好、XSS 安全）

阶段二 (3 天): i18n 基础设施
  ├── 依赖：阶段一.五（64 处 textContent 分布在明确模块中）
  ├── 方案：ICU MessageFormat JSON + 运行时 resolve
  ├── 产出：i18n/en.json, i18n/zh.json, i18n.js resolver
  └── 注意：t() 函数需覆盖所有 textContent 赋值点
```

**技术选型权衡：**

| 决策 | 选项 A | 选项 B | 推荐 |
|------|--------|--------|------|
| Bundler | **esbuild** | webpack/vite | **A**（零配置、快、无 lock 文件） |
| 类型系统 | 保持 JS + JSDoc | 迁移到 TypeScript | **A 中期切换 B**（TS 迁移可在阶段三做） |
| 状态管理 | 改进 context.js | 引入 Redux/Zustand | **A**（B 是过度工程） |
| 模板方案 | 字符串模板 | lit-html/JSX | **A**（无依赖） |

### 方向三：群通话 SFU 复用【P1】

**为什么需要：**
方向三的验证揭示了一个重要的架构事实：**服务端 SFU 基础设施已经完整接入了**——不是「未完成」或「设计未实现」，而是合约缺口只在客户端一侧。

我在代码中验证了：
- `CallOrchestrator::new().with_sfu(sfu_router)` ✅ 已接线
- `CallMode::Sfu` 枚举 ✅ 已存在
- `CallJoin` 帧处理 → `SfuRouter::add_peer()` ✅ 已调用

**真正的架构问题：** `CallMode` 枚举的设计不足以表达「拓扑策略」。当前 `{P2p, Sfu}` 中 Sfu 意味着「跨节点桥接模式」——没有表达「单节点下 SFU 聚合」的语义。

**核心挑战：**

1. **信令流程变更**：当前信令是 peer-to-peer 的 SDP 交换（offer/answer 在两端之间）。SFU 模式下，客户端发给服务器的 SDP 必须被重新分派给 SFU。
2. **mesh→SFU 动态升级**：4 人以下 mesh，第 5 人加入时迁移到 SFU。这意味着通话中间的 ICE 连接重建。
3. **Simulcast 适配**：SFU 支持 Simulcast 时，客户端需要发送多个编码层级。

**架构变更：**

```
现有 CallMode 枚举 (aero-im-call/src/types.rs):
  CallMode { P2p, Sfu }

建议改为:
  CallMode { 
    Mesh,           // ⇐ 新增，当前默认拓扑
    Sfu {
      kind: SfuKind,  // Local | Bridged(Vec<Url>)
      transcode: bool,
    }
  }

新增拓扑策略层:
  CallTopologyStrategy {
    fn decide(
      room_size: u32,
      peers: &[PeerInfo],
    ) -> (CallMode, Vec<CallLeg>)
  }
```

**关键设计决策：** 是否支持通话中从 mesh 升级到 SFU？

| 方案 | 优点 | 缺点 |
|------|------|------|
| **A: 预定拓扑**（通话建立时决策，不可变） | 实现简单，无 ICE 重建 | 第 5 人加入时体验降级 |
| **B: 动态升级**（超过阈值时迁移到 SFU） | 更好的可扩展性 | 短暂断连、ICE 重建复杂度高 |

**推荐**：方案 A 作为第一阶段，方案 B 作为里程碑目标。

### 方向四：审计/合规管理 UI【P1 — 企业的最后 5%】

**为什么需要：**
审计数据的**存在**不等于**可用**。当前 `audit_events` 表已按月分区（0146 迁移），`audits.rs` 路由完整，信息屏障、频道留存、法务保全的 API 全部就位——但它们没有被消耗。对于希望达到 SOC 2 / ISO 27001 的企业客户，没有管理界面等于功能不存在。

**核心挑战：**
方向四是**纯 UI 工作**，无后端变更。但它的架构价值在于：

1. **查询性能**：`audit_events` 按月分区但查询参数 API 允许全时间段——需要 `work_mem` 调优或限制查询时间范围。
2. **数据生命周期**：审计数据以每月 10GB+ 增长时，当前 API 没有分页上限——这是 DoS 风险的来源。
3. **RBAC 边界**：当前 `audits.rs` 的鉴权是 `auth_user` + `WorkspaceRepo::member_role`——确认了范围正确性，但 UI 层需要继承这个边界。

**最小可行架构：**

```
web/admin/
├── audit-log.html/j     — 审计日志浏览 + 过滤器（时间段、事件类型、用户）
├── legal-holds.html/j   — 法务保全管理（创建、释放、检查）
├── retention.html/j     — 频道保留策略配置（覆盖工作区默认）
├── sessions.html/j      — 活跃会话 + 远程登出
└── info-barriers.html/j — 信息屏障配置

技术方案：静态 HTML + 端点调用 + 表格渲染
复用：web/modals.js 模态基础设施 → 过滤器面板
```

**关键设计决策：是否将管理 UI 与主 SPA 耦合？**

| 方案 | 优点 | 缺点 |
|------|------|------|
| **A: 同一 SPA，admin 路由** | 单页体验、复用水润 | 增加主包大小、管理 UI 用户少 |
| **B: 独立管理页（推荐）** | 独立包体积、独立鉴权、不影响主 SPA | 需要第二个入口 |

**推荐：方案 B**。管理控制台和日常 IM 的使用模式完全不同，合并只会拖慢两者。

### 方向五：Bot 生态开放平台【P1 (安全) + P2/P3 (平台)】

**为什么需要：**
两个互相独立的问题：

**5a (P1 — 安全缺陷)：** 空 HMAC secret 使得所有 bot webhook 投递无签名。这不是未来风险——它是当前在线的安全缺口。任何能捕获到 bot `webhook_url` 的攻击者可以伪造事件。

**5b (P2/P3 — 平台化)：** 没有管理 UI 意味着 bot 订阅的发现、管理、调试全靠直接数据库操作。

**核心挑战：**

1. **迁移即修复**：给 `bot_subscriptions` 表加 `secret` 列是 forward-only 的。已有的订阅需要补丁填充 secret。
2. **secret 轮换**：UI 需要支持 bot 的 secret 轮换，但不影响正在投递的 webhook（at-least-once 语义下，使用旧 secret 签名的投递可能仍在飞行）。
3. **重试机制**：当前 one-shot delivery 意味着网络波动等 transient 错误直接丢事件——需要 backoff + DLQ 模式。

**架构变更：**

```
bot_dispatch.rs 修复链：
  1. 迁移: bot_subscriptions 加 secret VARCHAR(64) NOT NULL DEFAULT ''
  2. 迁移后补丁: 所有 DEFAULT '' 行更新为 random_bytes(32)
  3. 逻辑修复: build_delivery(&url, &sub.secret, &body, now)
  
重试机制（子方向）:
  bot_deliveries 表 + bg worker 重试 backoff
  拓扑: 当前 one-shot → 表持久化 + backoff + DLQ ("events" subject)
  
管理 UI（子方向）:
  web/admin/bots.html/j
  ├── 订阅列表 + 事件投递状态
  ├── secret 管理和轮换
  ├── 投递日志（最近 N 条）
  └── 暂停/启用订阅
```

**安全 vs 平台的依赖关系：** 两个子方向互不依赖，可以并行。安全修复应在下一个部署周期立即上线。

---

## 3. 接口设计建议

### 3.1 跨语言合约：接口是「事件契约」，不是函数签名

当前系统的**真正接口**不是 REST route 或 Rust trait——它是事件总线上的 `RoomEvent`/`StreamEvent` 枚举。这是事件驱动架构的核心优势，也是当前合约断裂的 root cause。

**建议引入：事件契约清单**

```
docs/specs/event-contract.md（或 auto-gen 工具）:
  | ServerFrame variant | explicit_recipients? | JS handler    | 状态   |
  |---------------------|----------------------|---------------|--------|
  | message             | [Message recipients]  | onMessage     | 🟢 完整 |
  | edited              | [room members]       | onEdited      | 🟢 完整 |
  | deleted             | [room members]       | onDeleted     | 🟢 完整 |
  | reaction            | [room members]       | onReaction    | 🟢 完整 |
  | interaction         | [room members]       | ❌ 缺失       | 🔴 缺口 |
  | message_seen        | [sender only]        | ❌ 缺失       | 🔴 缺口 |
  | ...                 |                      |               |         |
```

这不是文档工作，是**架构约束**——每个新 `ServerFrame` variant 的增加必须包含这个清单的更新。

### 3.2 前端模块化：接口是「模块导出函数」，不是全局状态

**当前模式（问题）：**
```javascript
// context.js — 全局可变单例
window.context = { user, rooms, activeRoom, ... };
// render.js — 读写全局
renderMsg(window.context.activeRoom, msg);
```

**目标模式（推荐）：**
```javascript
// context.js — 持有状态，提供 getter + 变更通知
export class AppState {
  get activeRoom() { ... }
  subscribe(listener) { ... }
  dispatch(action) { ... }
}

// msg.js — 纯渲染，接受 state 参数
export function renderMsg(state, msg) => DocumentFragment
```

**接口原则：**
1. 所有渲染函数是纯函数（入参 = state + data，出参 = DOM）
2. 状态持有者（context）通过订阅通知，不通过全局赋值
3. WS handler 只做 dispatch，不做 DOM 操作

### 3.3 通话信令：接口是「SFU 信令帧」，不是 mesh 扩展

当前 `calls.js` 中的信令流是 mesh 拓扑的：`CallJoin` → `CallOffer` → `CallAnswer` → `CallIceCandidate`，两端直接交换 SDP。

SFU 模式需要新增帧类型：

```
当前帧集（mesh 拓扑）:
  CallJoin → CallOffer → CallAnswer → CallIceCandidate (×N) → CallEnd

新增帧集（SFU 拓扑）:
  CallJoin(mode: 'sfu') 
    → CallSfuOffer(peer_sdp)        // 客户端 → 服务器 SFU
    → CallSfuAnswer(sfu_sdp)        // 服务器 SFU → 客户端
    → CallSfuIceCandidate           // ICE candidate 通过服务器转发
    → CallSfuMediaActive            // SFU 确认流活动
    → CallEnd
```

**向后兼容：** `CallJoin` 可以带 `mode` 参数，不传则默认 mesh（4 人以下）。已经在生产中的客户端继续使用 mesh，新客户端可以 opt-in SFU。

### 3.4 保持向后兼容性的总体策略

| 变更类型 | 兼容策略 |
|---------|---------|
| 新增 WS 帧（`CallSfuOffer` 等） | 新帧只在新路径发送，旧客户端永远不会收到 |
| 修改枚举（`CallMode` 加 `Mesh`） | 新 variant 不影响旧客户端（JS 仅检查已知值） |
| 数据库迁移（`bot_subscriptions.secret`） | `NOT NULL DEFAULT ''` + 迁移后补丁，旧 app 代码回退到空 secret ⚠️ 需要 gate |
| IFrame 合约 | **软期**：新 handler 的缺失只导致功能缺失，不产生错误 |
| 错误处理 | 所有新 handler 应该 fail-open（日志记录，不崩溃） |

---

## 4. 技术选型

### 4.1 前端 bundler 评估

| 选项 | 优势 | 劣势 | 适用于 |
|------|------|------|--------|
| **esbuild** | 零配置、Go 实现、快（<100ms）、ES2020 输出 | 无 HMR、无 dev server | ✅ **阶段一** |
| **vite** | HMR、esbuild under hood、生态丰富 | 需要配置、dev/prod 差异 | ❌ 当前阶段过度 |
| **webpack** | 生态成熟 | 配置复杂、速度慢 | ❌ 不推荐新项目采用 |
| **Rollup** | 纯库打包优异 | 应用开发需额外插件 | ❌ |

**推荐：esbuild**。它对无框架的纯 JS 项目来说是最小侵入的选择。没有 `package.json` lock 文件需要维护，没有 loader 配置。

**执行方式：**

```bash
# 最简集成：不需要 npm init，直接 npx
npx esbuild web/app.js --bundle --outfile=web/dist/bundle.js --target=es2020

# 集成到 CI：truth-check.sh 加一步
# 集成到开发：Makefile 加 esbuild-watch 目标
```

### 4.2 国际化（i18n）方案

| 选项 | 适合场景 | 运行时开销 | 零依赖可能性 |
|------|---------|-----------|-------------|
| ICU MessageFormat | 企业级 i18n（复数、性别、上下文） | 中（需 parser） | 可自实现简单 parser |
| 键值对 + printf-style | 简单替换 | 低 | ✅ 纯函数，30 行 |
| gettext `.po` 文件 | GNU 生态 | 高 | ❌ |
| **JSON + t() 函数（推荐）** | 当前不需要复数规则 | **最低** | **✅ <50 行** |

**推荐方案：**

```
web/i18n/
├── en.json    — { "msg.sent": "Sent", "room.created": "Room created", ... }
├── zh.json    — { "msg.sent": "已发送", "room.created": "房间已创建", ... }
└── index.js   — export function t(key, params?) { ... }
```

t() 函数处理最简 `{param}` 替换，不支持 ICU 语法。优点：零依赖、可 tree-shake、JSON 文件可被翻译工具处理。

### 4.3 类型安全：JSDoc → TypeScript 渐进式迁移

当前不用 TypeScript 的决策在 MVP 阶段合理，但 3000+ 行无类型 JS 已经出现了因拼写错误导致的静默 bug。

**渐进式路径：**

```
阶段零（当前）: 纯 JS，无类型
阶段一（方向二后）: JSDoc + ts-check 注释
  // @ts-check
  /** @param {import('./context.js').AppState} state */
  export function renderMsg(state, msg) { ... }
  
阶段二（独立节奏，可与方向二并行）: .ts 文件
  tsconfig.json (strict: false → true)
  esbuild 原生支持 .ts（无类型检查）
  类型检查交给 tsc --noEmit（CI 步骤）
  
阶段三（远期）: 全量 TypeScript
  tsconfig strict + esbuild emit
```

**不建议立即大爆炸式迁移 TypeScript**。JSDoc + `@ts-check` 可以在现有架构下立即获得类型安全收益，且不改变构建流程。

### 4.4 后端变动：任何新 crate？

| 方向 | 是否需要新 crate |
|------|-----------------|
| 方向一 | 否（既有 `common/src/model/`） |
| 方向三 | 否（既有 `aero-im-call/`） |
| 方向四 | 否（既有 `server/src/audits.rs`） |
| 方向五 | 否（既有 `server/src/bot_dispatch.rs`） |

**结论：方向一至五都不需要新 crate。** 所有后端工作都是既有模块内的增量变更。这与 AGENTS.md §4.1「feature-first 单位是 crate」的治理保持一致。

### 4.5 自建 vs 采购

| 需求 | 自建 | 采购 | 推荐 |
|------|------|------|------|
| Bot webhook secret 签名 | ✅ 20 行 HMAC-SHA256 | N/A | **自建** |
| i18n 运行时 | ✅ <50 行 t() 函数 | `intl-messageformat`（额外 10KB） | **自建** |
| 管理 UI 框架 | ✅ 基于既有 modal 模式 | Vue/React 单页（引入新依赖） | **自建（当前阶段）** |
| 通话 SFU 信令 | ✅ 既有 frame 系统加新 variant | Twilio/Agora API（外部依赖） | **自建** |

**自建阈值：** 当实现成本 ≤ 3 天且领域逻辑简单 > 基础设施复杂度时，走自建。

---

## 5. 实施路线图

### 5.1 优先级最终矩阵

```
     紧急（安全/生产缺陷）    非紧急（产品力/可维护性）
高影响  ┌─────────────────┬─────────────────┐
        │ P0: 方向一      │ P1: 方向三      │
        │    (DoS 放大)   │    (通话扩展)    │
        │ P0: 方向五a     │ P1: 方向四      │
        │    (安全缺陷)    │    (合规 UI)    │
        ├─────────────────┼─────────────────┤
低影响  │ P1: 无          │ P2: 方向五b     │
        │                 │    (Bot 平台)   │
        │                 │ P2: 方向二      │
        │                 │    (SPA 重构)   │
        │ P2: 无          │ P2: JSDoc 类型  │
        └─────────────────┴─────────────────┘
```

### 5.2 阶段性实施计划

#### Sprint 1：安全 + 合约修复（P0，建议立即启动）

| 任务 | 工时 | 文件变更 | 风险 |
|------|------|---------|------|
| Interaction explicit_recipients 修复 | 0.5 天 | `common/src/model/event.rs` | 低 |
| JS ws.on('msg:interaction') handler | 0.5 天 | `web/app.js`, `web/render.js` | 低 |
| message_seen 帧 JS handler | 0.5 天 | `web/app.js`, `web/render.js` | 低 |
| Bot secret 迁移 + 补丁 | 0.3 天 | `migrations/`, `bot_dispatch.rs` | 低 |
| 合约清单文档 | 0.2 天 | `docs/specs/event-contract.md` | 低 |

**总计：2 天。风险：低。**

#### Sprint 2：审计管理 UI（P1，纯前端）

| 任务 | 工时 | 文件变更 | 风险 |
|------|------|---------|------|
| 审计日志视图（过滤器 + 表格） | 1 天 | `web/admin/audit.html`, `audit.js` | 低 |
| 法务保全管理视图 | 0.5 天 | `web/admin/legal-holds.js` | 低 |
| 频道保留策略 UI | 0.5 天 | `web/admin/retention.js` | 低 |

**总计：2 天。风险：低——所有 API 已存在，纯 DOM 操作。**

#### Sprint 3：Call SFU 信令增强（P1，中风险）

| 任务 | 工时 | 文件变更 | 风险 |
|------|------|---------|------|
| CallMode 枚举加 Mesh variant | 0.3 天 | `aero-im-call/src/types.rs` | 低 |
| 拓扑策略层 `decide_call_topology` 重构 | 0.5 天 | `aero-live-webrtc/` | 中（需回退旧行为） |
| JS calls.js 新增 SfuJoin 信令流 | 1.5 天 | `web/calls.js` | 中（WebRTC 联调环境） |
| 浏览器端 SFU 模式测试 | 1 天 | 测试 | 高（需真实浏览器对端） |

**总计：3.3 天。风险：中——需要真两端联调。可以后移至有真实通话需求的迭代。**

#### Sprint 4-5：Web SPA 架构重构（P2，高价值但可并行）

| 阶段 | 任务 | 工时 | 产出 |
|------|------|------|------|
| 4a | esbuild bundler 引入 | 1 天 | `web/dist/bundle.js` |
| 4b | render.js 拆分为模块 | 2 天 | 5 个 render/ 子模块 |
| 4c | context.js 改进为订阅模式 | 1 天 | `AppState` 类 |
| 5a | i18n JSON + t() 函数 | 1 天 | `web/i18n/` |
| 5b | 64 处 textContent 替换为 t() | 1.5 天 | 全模块替换 |

**总计：6.5 天。可与 Sprint 2/3 并行（不同文件集合，无冲突）。**

#### Sprint 6+：Bot 管理 UI + 远期改善（P2/P3）

| 任务 | 工时 | 依赖 |
|------|------|------|
| Bot subscriptions 管理 UI | 2 天 | 方向一 Contract 清单就绪 |
| bot_dispatch 重试机制（DLQ） | 1 天 | 方向五a 安全修复之后 |
| JSDoc 类型标注 + @ts-check | 2 天（可分散在多个 sprint） | 方向二 模块化后 |

### 5.3 依赖关系图

```
Sprint 1 (P0 安全)
  ├─ 无外部依赖
  └─ Sprint 6 (bot 管理 UI) ← 可选依赖

Sprint 2 (审计 UI)
  ├─ 无外部依赖
  └─ 可独立上线

Sprint 3 (Call SFU)
  ├─ 无外部依赖
  └─ 可独立上线

Sprint 4-5 (SPA 重构)
  ├─ 无外部依赖
  └─ Sprint 5 (i18n) 依赖 Sprint 4 (render 拆分)

Sprint 6 (Bot 平台)
  └─ Sprint 1 (安全修复) — 顺序依赖
```

**关键路径**：无。所有 sprint 可以并行，资源充足时 3-4 周完成全部。

### 5.4 风险登记册

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|---------|
| **Sprint 3 联调环境不足**（真实浏览器 ↔ str0m SFU） | 高（CI 跑不了 WebRTC） | 中（Sprint 3 延期） | 1. 先提交 Rust 枚举变更；2. JS 信令流代码 review；3. 浏览器联调作为独立里程碑，不阻塞 deploy |
| **方向二引入 bundler 后 HTML 引用路径变更**（`<script src="...">` 从 11 个变 1 个） | 中 | 低（构建步骤解决） | 1. `Makefile` 加 `build-web` 目标；2. `truth-check.sh` 加 `esbuild --version` 检测；3. Dockerfile 加构建步骤 |
| **全局状态从 context.js 单例改为 AppState 订阅模式时的竞态** | 低-中 | 中（ws.on 回调与渲染时序） | 1. 迁移不一次完成，逐步替换消费方；2. 每个模块一个 PR；3. 阴影 DOM 局部刷新 |
| **方向一 Interaction 修复后的客户端兼容性**（旧客户端收到新帧格式） | 中 | 低（forward-compatible） | 1. 只改 explicit_recipients 逻辑，帧格式不变；2. 新帧是新 variant（`msg:message_seen`），旧客户端静默忽略 |
| **迁移补丁后旧代码写空 secret 到数据库**（回滚时） | 低 | 中（webhook 验证失败） | 1. 迁移是 forward-only（不设计回滚）；2. 旧代码一旦更新新的 `secret` 列，空 string 写入不会破坏 DB schema；3. gate 验证逻辑：`if secret.is_empty() { skip_verify }` 兼容旧代码 |

### 5.5 里程碑检查点

```
M0（第 1 周结束）： Direction 1 全部完工 + Direction 5a 安全修复
  └─ 验收条件：
      - Interaction 帧只发帖主（contract code + unit test）
      - message_seen 帧在 JS 端已消费（grep ws.on msg:message_seen)
      - bot webhook 签名不再是空串（build_delivery 第三参数 != "")
      - event-contract.md 文档提交到 main

M1（第 3 周结束）： Direction 4 审计 UI + Direction 2 阶段一 Bundler
  └─ 验收条件：
      - 审计日志页面有数据、可过滤
      - esbuild 在 Makefile 中，production build 产出 bundle.js
      - truth-check.sh 验证 bundle 新旧大小更新

M2（第 6 周结束）： Direction 3 SFU 信令 + Direction 2 阶段一.五 渲染拆分
  └─ 验收条件：
      - CallMode::Mesh 新增，Cargo check 绿灯
      - calls.js 可以发送 CallJoin(mode:'sfu')
      - render/ 目录 5 个模块，render.js 从 685 行降到 <100 行
  
M3（第 8 周结束）： Direction 2 阶段二 i18n
  └─ 验收条件：
      - 64 处 textContent 全部替换为 t() 调用
      - en.json / zh.json 完整覆盖
      - 切换 locale 触发 UI 全重刷（分阶段可接受）
```

---

## 总结

这些方向共同揭示了一个更大的架构观察：**Aero IM 的后端架构（事件驱动 + NATS + 进程内扇出 + SFU）是企业级的，在中等规模的 SaaS 中可承受 10 万+并发用户。它的瓶颈不在服务器能力，而在客户端设计和两端合约对齐。**

这是典型的事件驱动架构的「成熟度悖论」：
- 后端投入了 90% 的工程力 → 企业级的事件架构
- 前端投入了 10% → 零依赖 SPA MVP
- **合约边界**（事件 emit → consume）几乎没有投入

三个 P0/P1 方向（方向一、四、五a）全部是「后端已就绪、前端未连接」的模式。修复它们不需要架构范式转变——只需要补齐最后 5% 的消耗端工程。

**对我所在组织的建议**：如果 Aero IM 要作为产品推向企业客户，优先顺序应该是 **方向四（合规 UI）> 方向五a（安全修复）> 方向一（合约修复）> 方向三（通话扩展）> 方向二（前端重构）**。方向二是最有长期价值的，但它的收益分散在「可维护性」上，不如方向四和方向五a的「不买就无」紧迫。

我没有其他问题了。以上是对 5 个方向的完整架构验证、修正和重构建议。感谢你提供了目前最成熟的分析方法论。
