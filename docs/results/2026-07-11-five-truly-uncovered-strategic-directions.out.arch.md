以下是对 `2026-07-11-truly-uncovered-expansion-directions.md` 的架构分析。

---

# 架构分析报告：Aero IM 真正未被覆盖的扩展方向

## 1. 架构评估

### 1.1 核心优势

该文档揭示了一个已经在数据层和传输层非常成熟的系统，而服务端代码的质量之高掩蔽了客户端侧的缺口。我已验证文档中的核心代码证据：

- **`ServerFrame` 枚举** 在 `crates/aero-server/src/ws/ws_impl/mod.rs:244` 定义了 `MessageSeen` variant，在 `:247` 定义了 `Interaction` variant，且 `crates/aero-server/src/ws/frame.rs:55-59` 将它们从 `RoomEvent` 映射到 `ServerFrame`
- **`explicit_recipients`** 匹配在 `crates/aero-common/src/model/event.rs:142` 将 `Interaction` 和 `MessageSeen` 落入 `_ => Vec::new()` 通配分支——验证了这两类事件被广播给**所有房间成员**
- **`web/app.js:105-116`** 的事件绑定清单中确实缺少 `msg:interaction` 和 `msg:message_seen`

这表明架构在服务端侧是**前瞻性设计**（事件类型预定义、扇出机预搭建），但客户端侧存在**不对称实现**。

### 1.2 关键架构债务

按严重排序：

| # | 债务 | 影响面 | 根因 |
|---|------|--------|------|
| 1 | **客户端-服务端契约不可追踪**：`ServerFrame` 的每个 variant 没有对应的客户端 handler 注册要求 | 事件静默丢弃（如 Interaction/MessageSeen）；新加事件类型必须人工逐帧检查两端 | 无契约强制机制（无代码生成、无 schema registry、无测试） |
| 2 | **Web SPA 作为单层调试客户端**：11 个 JS 文件串行加载、硬编码简体中文、无离线支持 | 产品化阻力；企业客户不可达；维护成本递增（每加 API 加 JS 文件） | 初始决策「先搭 IM 引擎再磨 UI」的合法债，但已到偿债临界点 |
| 3 | **SFU 基础设施孤立在 crate 边界内**：`aero-live-webrtc` 的 `SfuRouter`/`SfuForwarder` 对 `aero-im-call` 不可见 | 群通话停留在 O(N²) mesh；直播 SFU 的字节级完备复用于通话需跨 crate 提升 | Crate 划分纯以领域正交性（直播 vs IM）为准，忽略了两者共用 WebRTC 基础设施的需求 |
| 4 | **Bot 投递无重试、空 HMAC 签名**：`bot_dispatch.rs` 的 one-shot 投递与 `webhook_delivery_log` 的背退重试不对等 | 生产级 bot 不可靠；第三方 bot 无法验证 webhook 来源 | Bot 事件订阅系统（mig 0141/0142）是最近才完善的，可靠性机制尚未跟进 |
| 5 | **`Interaction` 无细化扇出**：落入 `explicit_recipients` 的通配臂，导致全员广播 | 带宽浪费；大型房间下帧洪水 | 扇出逻辑 `bus.rs:79` 对所有事件统一调用 `explicit_recipients`，而 `Interaction` 本应只发给原发帖 bot + 交互者 |

### 1.3 设计决策合理性评估

| 决策 | 合理性 | 说明 |
|------|--------|------|
| Crate 按领域垂直切割 | ✅ 合理 | 保持编译边界和依赖方向，但需警惕 crate 边界阻断了跨领域基础设施复用（如 SFU） |
| Web SPA 使用零依赖 ES2020 | ⚠️ 当时合理，现需演进 | 快速原型阶段的正确选择。但 16 crate × 46K Rust 的服务端 vs 5.9K 纯 JS 的客户端，投入比已严重失衡 |
| NATS JetStream 作为跨节点事实源 | ✅ 合理 | durable consumer + per-subject seq 是经过验证的模式 |
| Redis sorted-set 作为集群状态 | ✅ 合理 | 避免单进程内存状态的漂移问题 |
| 事件扇出统一走 `Hub::fan_out_raw` | ✅ 合理 | 一致的扇出抽象，便于加 tracing 和速率控制 |
| Bot 基础设施与参与度模型集成 | ✅ 合理 | `ParticipantKind::Bot/Agent` 是一等公民设计 |

---

## 2. 扩展方向（优化的优先级重新评估）

基于文档的 5 个方向，我提出以下架构视角下的扩展建议。注意我的优先级排序与文档略有不同——从架构影响面而非纯产品价值出发：

### 2.1 方向 A（P0）：建立客户端-服务端契约的强制一致性

**对应的文档方向**：方向一（事件处理缺口）的**系统性根因修复**

**为什么需要**：
当前 `ServerFrame` 枚举的每个 variant 与客户端 `ws.on('msg:*', ...)` 处理器的对应关系完全是**人工约定**。没有编译时检查、没有运行时验证、没有测试。这意味着：
- 每次新增 `ServerFrame` variant（如未来的 `ThreadUpdate`、`ScheduledMessage`），开发人员必须**记住**去 web/ 加 handler——容易遗漏
- 没有客户端 handler 的 variant **静默浪费带宽**（当前 `Interaction`/`MessageSeen` 就处于此状态）
- 重构时移除某个 variant 可能遗留死 handler 在客户端

**核心挑战**：
1. 服务端（Rust enum）和客户端（JS object）的类型系统不互通
2. Web SPA 没有 TypeScript——无法从共享类型定义生成 handler 骨架
3. 不能简单地在 Rust 侧加一个 `#[must_use_handler]` 注解（JS 侧无法检查）

**预期架构变更**：

```
选项 A（轻量）: 在测试层加合约
  - 在 Rust 侧加一个测试，将 ServerFrame 的所有 variant 序列化为 JSON
  - 在 JS 侧加一个测试，解析该 JSON 并验证每个 variant 有对应的 handler
  - 优势：无架构侵入，纯测试层
  - 劣势：跑测试才报，开发周期偏后

选项 B（重量）: 代码生成
  - 在 build.rs 中扫描 ServerFrame enum，生成一个 JSON schema 或 TypeScript type
  - 在 CI 中对比 Rust schema 与 JS handler 注册表的差异
  - 优势：编译期/CI 时捕获
  - 劣势：build.rs 侵入，前期成本高

选项 C（中间）: schema registry + handler 注册表
  - 客户端维护一个声明式的 handler 注册表（已半存在 `app.js:105-124`）
  - 服务端在 CI 时产出一个 `expected_handlers.json`
  - CI 脚本对比两者差异，不匹配则 break
```

**对现有系统的影响**：低。纯工具链和 CI 层变化，不改运行时逻辑。建议选选项 C 作为过渡方案。

### 2.2 方向 B（P0/P1）：Web SPA 渐进式工程化（非重写）

**对应的文档方向**：方向二

**为什么需要**：
这不是「要不要用 React」的问题。本质问题是：
1. **11 个 JS 文件串行加载**：HTTP/1.1 下每次页面加载 11 轮 RTT
2. **80+ 处硬编码中文**：进入国际市场意味着每加一种语言就要改全库
3. **无离线缓存**：断网白屏，无法满足企业移动场景
4. **无类型安全**：`state` 对象无约束，运行时 `undefined` 错误无法提前发现

**核心挑战**：
- 渐进式迁移**不是重写**——文档正确地指出了这一点。但「渐进式」的粒度需要精确设计
- 当前 11 个 JS 文件互相依赖（`context.js` 是全局状态、`app.js` 引用所有模块），引入 bundler 需要处理循环依赖和全局命名空间
- i18n 的引入需要触碰**所有**用户可见字符串——无法分批做，改动面大

**预期架构变更**：

```
阶段 1: 构建工具层
  - 引入 esbuild（最轻量，零配置，拆包/压缩一步到位）
  - 将 11 个 JS 文件变为 ES modules，用 `import` / `export` 替换全局变量
  - target: 1 个 app bundle + 按需 code split

阶段 2: 数据层
  - 抽取 i18n 模块（`i18n.js`），JSON 语言包（先 zh.json，锁现有中文为基线）
  - 所有 `textContent` / `innerText` 赋值点改为 `t('key', default)`
  - 加 `service-worker.js`：静态资源 CacheFirst + API NetworkFirst

阶段 3: 类型层
  - 加 `.d.ts` 类型定义（先覆盖 `context.js` 的全局 state）
  - 逐步将核心模块（`ws.js`、`api.js`）迁移到 `.ts`
```

**对现有系统的影响**：中。构建工具改动不影响服务端；i18n 改造需要逐个文件扫描字符串，但**不改逻辑**。

### 2.3 方向 C（P1）：SFU 基础设施提升——`CallBridge` 作为共享 crate 拆出

**对应的文档方向**：方向三，但更激进

**为什么需要**：
当前 `SfuRouter`、`SfuForwarder`、`CallBridge`（跨节点）全部在 `aero-live-webrtc` crate 内。通话 crate（`aero-im-call`）想要复用就必须依赖整个直播 crate——这不合理。

正确的架构是：将 **SFU 核心（转发引擎 + RTCP 处理 + Simulcast 重映射）** 拆到独立的 `aero-sfu-core` crate，让直播和通话共同依赖它。

这个方向的技术价值高于产品价值——释放被 crate 边界封存的 SFU 能力。

**核心挑战**：
1. `aero-live-webrtc` 与直播状态（房间列表、HLS writer 引用）深度耦合——提取 SFU core 需要识别并剥离这些依赖
2. 通话场景的 SFU 需求与直播 SFU 略有不同：通话需要双向音频、屏幕共享、更低的延迟容忍（通话≤150ms vs 直播≤2s）
3. 信令流不同：直播 WHIP/WHEP 用 REST SDP 交换，通话用 WS 帧 + NATS 协调

**预期架构变更**：

```
当前拓扑:
  aero-live-webrtc (SfuRouter + SfuForwarder + CallBridge)
    ↑
  aero-live-core (直播业务)

目标拓扑:
  aero-sfu-core (SfuRouter + SfuForwarder + seq/ts remap + RTCP)
    ↙          ↘
  aero-live-webrtc     aero-im-call
    ↑                    ↑
  aero-live-core      aero-server boot 层（统一提升 SfuRouter 实例）
```

**对现有系统的影响**：高。涉及 crate 拆分、依赖方向反转、初始化重构。建议分两步：先在不改变直播 crate 的情况下，将 `SfuForwarder` 和 `remap.rs` 提取到 `aero-sfu-core`；再将 `aero-im-call` 的 SFU 模式实现在新 crate 之上，保留 mesh 作为倒退备选。

### 2.4 方向 D（P1/P2）：Bot 管理面 + 投递可靠性

**对应的文档方向**：方向五

**为什么需要**：
文档正确地指出了 Bot 基础设施的「API 全但 UI 零」问题。从架构角度，更严重的问题是 **Bot 投递的可靠性不足**：

- `bot_dispatch.rs`（我已验证存在）的投递是 **one-shot**：`tokio::spawn` 后如果挂（网络超时、bot 侧 500），事件静默丢失
- HMAC 签名为空 secret——第三方 bot 无法验证事件来源，这是安全漏洞
- 没有投递重试，与 webhook 体系（`mark_failed_with_backoff`、`dead_letter_queue`）形成鲜明对比

**核心挑战**：
1. `bot_event_subscriptions` 表需要加列：`hmac_secret`（当前空）、`max_retries`（当前无）
2. 投递重试需要持久化状态表（`bot_delivery_log` 已有但缺重试时间戳），与既有 webhook 队列复用相同的后台清扫逻辑
3. UI 面涉及 5-6 个新页面/模态框（Bot 列表、创建表单、订阅管理器、投递日志）——在方向 B 的 i18n 和 bundler 未就绪前，直接加页面会加重架构债

**预期架构变更**：

```
数据层:
  bot_event_subscriptions
    + hmac_secret TEXT NOT NULL DEFAULT ''   -- 自动生成，不再为空
    + max_retries SMALLINT NOT NULL DEFAULT 3

  bot_delivery_log
    + retry_count SMALLINT NOT NULL DEFAULT 0
    + next_retry_at TIMESTAMPTZ
    + （已有 status, response_code, error）

后台:
  复用既有 retry_sweep 定时器（webhook_delivery 的背退逻辑），增加 bot_delivery 的清扫

UI:
  /bots → 列表/创建/编辑
  /bots/:id/subscriptions → 事件类型过滤器 + action_id 过滤器
  /bots/:id/deliveries → 投递日志
```

**对现有系统的影响**：中。数据层改动是向后兼容的（默认空 secret、max_retries=0 维持现状）。UI 是纯新增。

### 2.5 方向 E（P2）：审计/合规的最后一公里

**对应的文档方向**：方向四

**为什么需要**：
文档识别了一个容易被忽视的模式：**API 已全但 UI 为零** 是整个项目中多个模块的共同问题。审计系统（`audit_events`、`legal_holds`、`info_barriers`、`channel_retention`）只是其中最典型的一个。

**核心挑战**：
1. 这些 API 是 RESTful 的，但**没有设计为 UI 驱动**——分页参数、过滤维度、聚合查询可能不够完善
2. 合规 UI 需要高级过滤和搜索，而当前审计 API 的 `GET /api/audit` 可能只支持基础的 time range 和 event type 过滤
3. 法务保全令的操作涉及事务一致性（创建保全令 + 标记被保全消息），API 是否是原子操作需要验证

**预期架构变更**：轻。**不涉及后端改动**（除非发现现有 API 不足以支撑 UI 场景）。纯 UI 工作：
- 审计事件浏览器（基于已有 `GET /api/audit?kind=&participant=&from=&to=`）
- 合规 Dashboard（聚合视角，可能需加 1-2 个聚合 API 端点）
- 法务保全令管理器（创建/撤销 UI 包装已有 API）

**对现有系统的影响**：低。建议在方向 B 阶段 2（i18n+缓存）之后或并行进行。

---

## 3. 接口设计建议

### 3.1 客户端-服务端契约接口

当前缺口暴露了一个系统性问题：**WebSocket 帧的客户端消费没有强制合同**。建议的接口原则：

```
原则 1: 每个 ServerFrame variant 必须有一个对应的客户端 handler
  实现: CI 阶段维护一个 schema registry，比较两端

原则 2: 扇出粒度由事件类型决定，不留通配臂
  实现: explicit_recipients 应对每个 variant 显式处理，不加 _ => Vec::new()
```

### 3.2 SFU 复用接口

如果拆出 `aero-sfu-core`，其公共接口应为：

```
trait SfuRouter:
  fn subscribe(peer: SfuPeer, stream_id: StreamId) → Result<()>
  fn unsubscribe(peer_id: PeerId, stream_id: StreamId)
  fn on_rtp(origin: PeerId, stream_id: StreamId, rtp: RtpPacket)
  fn request_keyframe(peer_id: PeerId, stream_id: StreamId)  // PLI/FIR 转发

// 直播和通话各自实现 SfuPeer 的填充逻辑
```

关键设计点：**SFU 核心不关心业务语义**（不知道什么是「房间」或「通话」），只处理媒体流的路由和转发。上层 crate 负责将业务标识符（`room_id`、`call_id`）映射到 SFU 的 `stream_id`。

### 3.3 Bot 投递重试接口

复用既有的 webhook 重试模式：

```
trait DeliveryRetry:
  fn mark_failed_with_backoff(delivery_id, status_code, error)
  fn attempt_redelivery(delivery_id) → Result<()>
  fn dead_letter(delivery_id)
```

`bot_dispatch.rs` 当前直接 `spawn + POST` 的逻辑应改为：
1. 立即记录 `bot_delivery_log` 行（status = `pending`）
2. 后台 `bot_delivery_worker` 轮询未完成的 delivery（`FOR UPDATE SKIP LOCKED`），尝试投递，失败则 `mark_failed_with_backoff`
3. 达到 `max_retries` 后标记 `dead`

### 3.4 向后兼容性

| 变更 | 兼容策略 |
|------|---------|
| `Interaction` 扇出收窄到只发 bot + 交互者 | 当前行为是全员广播，收窄后已有客户端从「收到但丢弃」变为「收不到」——**不破坏** |
| `MessageSeen` 加限频 | 加在服务端，客户端不受影响 |
| Bot 表加 `hmac_secret` 列 | 默认生成，既有的空 secret bot 保留（不破坏已有集成） |
| SFU core crate 拆分 | 直播 crate 继续导出兼容的 re-export + 新 crate 独立版本 |
| i18n 改造 | `t('key')` 回退到硬编码值，保证 `t(key)` = key 时行为不变 |
| esbuild bundler | 纯构建阶段，不改变运行时行为 |

---

## 4. 技术选型

### 4.1 需要引入的新技术

| 场景 | 推荐方案 | 备选 | 理由 |
|------|---------|------|------|
| Bundler | **esbuild** | Vite, webpack | 零配置、极速、充分满足「11 JS 文件→2-3 bundle」的需求；Vite 后续可以加但当前不需要 HMR |
| i18n 框架 | **Intl API（内置） + 自建轻量 `t()` 函数** | i18next, FormatJS | 当前 80+ 字符串的规模无需全量框架；轻量 `t()` 可在后续平滑替换 |
| 离线/缓存 | **Workbox**（service worker 构建工具） | 手写 service worker | Workbox 封装了 CacheFirst / NetworkFirst 等策略模式，避免手写缓存逻辑 |
| 类型安全 | **JSDoc + `.d.ts`**（渐进）→ **TypeScript**（最终） | Flow | JSDoc 可以在不改变文件后缀的情况下渐进加入类型；TypeScript 迁移从核心 `context.js` 开始 |
| 契约验证 | **自建 CI 脚本** + 已有的 JSON schema | OpenAPI, protobuf | 当前只有 WS 帧路由需要验证，不值当引入全量 IDL；但可预留升级路径到 OpenAPI 3.1 |
| 客户端测试 | **Playwright**（E2E） + Vitest（单元） | Cypress, Jest | Playwright 对 WebSocket 原生支持好；Vitest 与 esbuild 共享转译管线 |

### 4.2 自建 vs 采购

| 决策点 | 建议 | 理由 |
|--------|------|------|
| i18n 语言包 | **自建** | 80+ 字符串，JSON 语言包 + 20 行 `t()` 函数足够了 |
| Bot 重试机制 | **自建** | 复用既有 webhook 清扫逻辑，扩展 `bot_delivery_log` 表即可，无第三方库能直接解决 |
| SFU core crate 拆分 | **自建** | 已有代码（`SfuForwarder`、`remap.rs`）是成熟的，只需拆解和重新打包 |
| 审计 UI 管理页 | **自建**（纯 web 组件） | API 已全，不需要第三方 BI 工具嵌入 |
| Bundler / TS | **esbuild + TypeScript** | 业界标准选择，社区支持好 |

### 4.3 第三方依赖评估标准

为 Aero IM 引入新依赖时，应按此标准评估：

1. **编译目标**：是否支持 wasm？如果未来考虑 Web SPA 的 SSR 或 PWA，依赖不应绑定 Node.js 专用 API
2. **包体积**（前端）：引入 npm 包时，检查 tree-shakeable 与否（esbuild 打包后的实际增量）
3. **维护活跃度**：最近 6 个月有提交、issue 响应及时、有 release 计划
4. **Rust 生态适配**：如果引入 Rust 端依赖（如新的 WebRTC 库），需兼容 MSRV 1.80 + no unsafe
5. **许可证兼容**：AGPL 互斥（Aero IM 未公开许可证但须考虑商业分发）

---

## 5. 实施路线图

### 5.1 优先级排序

| 优先级 | 方向 | 预估工作量 | 依赖 |
|--------|------|-----------|------|
| **P0** | 客户端-服务端契约强制（方向 A） | 2-3 天 | 无 |
| **P0/P1** | Web SPA 工程化阶段 1：esbuild bundler（方向 B-1） | 3-5 天 | 无 |
| **P1** | Interaction/MessageSeen 客户端 handler（方向 1 的具体 bug 修复） | 2-3 天 | 方向 A 的 CI 校验确保不再遗漏 |
| **P1** | `Interaction` explicit_recipients 修复（只扇出给 bot） | 1-2 天 | 无 |
| **P1** | Bot 投递重试 + HMAC 签名修复（方向 D 的数据层） | 5-7 天 | 无 |
| **P1** | Web SPA 阶段 2：i18n + service worker（方向 B-2） | 10-15 天 | 方向 B-1 |
| **P1/P2** | SFU core crate 拆分（方向 C 的第一步） | 10-15 天 | 无（可与 B 并行） |
| **P2** | Bot 管理 UI（方向 D 的 UI 层） | 7-10 天 | 方向 B-2（需要 i18n 就绪） |
| **P2** | 审计/合规 UI（方向 E） | 7-10 天 | 方向 B-2（需要 i18n 就绪） |
| **P2** | 通话 SFU 模式（方向 C 的第二步） | 20-30 天 | SFU core crate 拆分完成 |
| **P2/P3** | TypeScript 迁移（方向 B-3） | 持续渐进 | 方向 B-1 |

### 5.2 阶段划分

**Phase 0（2 周）——止损 + 基础设施**：

```
Week 1:
  □ 方向 A: CI 契约校验（ServerFrame → handler 清单对比）
  □ 方向 1 bugfix: Interaction/MessageSeen 客户端 handler + explicit_recipients 修复
  □ 方向 B-1: esbuild 引入，11 JS 文件 → 2-3 bundle

Week 2:
  □ 方向 D 数据层: bot_event_subscriptions 加 hmac_secret + bot_delivery_log 加 retry 字段
  □ 方向 D 后台: bot_delivery_worker（复用 webhook 的背退逻辑）
  □ 方向 C 准备: 识别 aero-live-webrtc 中可提取的 SFU core 模块
```

**Phase 1（3 周）——可维护性 + 产品化**：

```
Week 3-4:
  □ 方向 B-2: i18n 模块 + zh.json 基线语言包 + en.json（首次提取）
  □ 方向 B-2: service-worker.js（Workbox，CacheFirst + NetworkFirst）

Week 5:
  □ 方向 C 第一步: aero-sfu-core crate 提取（SfuForwarder + remap + RTCP）
  □ aero-live-webrtc 重构为依赖 aero-sfu-core
```

**Phase 2（4 周）——平台能力**：

```
Week 6-7:
  □ 方向 D UI: Bot 管理面板（列表/创建/订阅管理器/投递日志）
  □ 方向 E: 审计事件浏览器 + 合规 Dashboard

Week 8-9:
  □ 方向 C 第二步: aero-im-call 的 SFU 模式实现（信令扩展 + Web 端 calls.js 改造）
  □ Mesh 作为 ≤4 人备选保留，SFU 为 5+ 默认
```

**Phase 3（持续）——类型安全 + 深度优化**：

```
□ 方向 B-3: 渐进 TypeScript 迁移（从 context.js → api.js → 核心渲染逻辑）
□ MessageSeen 限频（同一房间同一用户 ≤1 条/秒 聚合事件）
□ Bot 交互式 Block 预览/测试器
□ 通话录制（服务端录流 → HLS）
```

### 5.3 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| esbuild 引入后现有 JS 模块循环依赖暴露 | 中 | 中 | 先在 CI 上跑 esbuild 检查（`--bundle` 模式），循环依赖会报错，提前修正 |
| i18n 改造漏掉字符串 | 中 | 低 | 引入 i18n 后保留退路：`t(key, fallback)` 中 fallback 为原中文，漏掉的字符串退化显示 key |
| SFU core crate 拆分导致直播回归 | 低 | 高 | 分步提取：先拆纯转发逻辑（无业务语义），直播 crate 保留 re-export，逐步迁移 |
| Bot 投递重试引入死循环 | 低 | 中 | 复用 webhook 的 `MAX_ATTEMPTS=5` + `mark_failed_with_backoff`（指数退避），不改动核心逻辑 |
| 通话音视频质量在 SFU 模式下退化 | 中 | 高 | 保留 mesh 倒退选项；SFU 模式默认只对 5+ 人开启；通话启动时做 SFU ↔ mesh 的选择决策，可动态切换 |
| Phase 0/1 并行方向多导致上下文切换 | 高 | 中 | 明确 assign owner per 方向；方向 A + 方向 1 bugfix 可由一人完成全部服务端+客户端改动；方向 B-1 需要前端基础设置，建议单独 Track |

### 5.4 关键架构里程碑

```
M1（2 周）: CI 可检测到 ServerFrame 遗漏的客户端 handler
M2（5 周）: 客户端 esbuild 就绪 + i18n 基线覆盖 web/ 全部用户可见字符串
M3（7 周）: Bot 管理面可用（创建/订阅/投递日志） + Bot 投递具备背退重试
M4（9 周）: SFU core crate 独立，直播不回归
M5（12 周）: 群通话可选 SFU 模式（≥5 人默认），保留 mesh 倒退
```

---

## 总结

这份分析文档的价值不在于指出「这里缺 UI」或「那里没 handler」——这些是症状。其深层价值在于暴露了 Aero IM 当前架构的两个系统性盲点：

1. **客户端-服务端契约没有强制一致性机制**——这是后续所有客户端侧债的根因。方向一的 Interaction/MessageSeen 遗漏只是第一个症状，不建立契约强制的话会有更多这样的遗漏。

2. **Crate 边界既是依赖防火墙也是复用壁垒**——SFU 无法被通话复用是最具体的案例。这提示了一个架构原则：**共享基础设施应放在独立的底层 crate，不要埋在某一个领域 crate 内部**。Bot 投递重试的缺失也是同一问题的变体（webhook 重试在 crate A，bot 投递在 crate B，各自独立演化出了不同的可靠性级别）。

以上分析建立在文档的代码级证据之上，但以架构师视角重新评估了优先级、提出了接口设计原则、并给出了分阶段的实施路线图。建议将方向 A（契约强制）作为**所有其他方向的前提条件**——在能力确保「服务端每发一个帧客户端都有 handler」之前，不应认为方向一已完全解决。
