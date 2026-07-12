# Tech Lead 分析报告：五个生产缺口再验证（2026-07-12）

## 验证摘要

基于以下文档/代码的重验证结论：

| 方向 | 代码现状 | 分析文档准确性 |
|------|---------|--------------|
| **D1 — TURN凭证** | 静态明文，时长受限凭据基础设施已存在但未接线 | ✅ 问题真实，有既有分析 |
| **D2 — @everyone成本** | `NotifyBatch`已存在，但缺少资费防护/UI门控 | ✅ 问题真实，有既有分析 |
| **D3 — Interaction/MessageSeen断头** | 服务端完整发送，Web端零处理 | ✅ 问题真实，多处已分析 |
| **D4 — Delivery Cursor客户端缺口** | 服务端基础完备，PUT端点缺失，Web SPA未调用 | ✅ 问题真实，有既有分析 |
| **D5 — 优雅降级** | Redis降级已实施；NATS静默丢帧/AI超时无保护仍真实 | ⚠️ 断言有误(Redis→500不存在)，核心子问题成立 |

---

## 1. 任务分解 — 可执行任务列表

### D1: TURN凭据动态化

| ID | 任务标题 | 涉及文件 | 前置 | 工时 | 验收标准 |
|----|---------|---------|------|-----|---------|
| **TASK-001** | 抽离 `TurnConfig` 至共享层 | 新建 `aero-common/src/turn.rs`；修改 `aero-live-srt/src/lib.rs`、`aero-common/src/lib.rs` | 无 | 3h | `TurnConfig` 位于 `aero-common`；`aero-live-srt` re-export；编译通过 |
| **TASK-002** | 为 `TurnConfig` 添加参与者绑定凭据 | `aero-common/src/turn.rs` | TASK-001 | 2h | `ephemeral_credential` 新增 `participant_id` 参数；用户名含 `{expiry}:{name}:{pid}` |
| **TASK-003** | 重写 `rtc_config_payload()` 使用动态凭据 | `crates/aero-server/src/routes/routes.rs` | TASK-002 | 2h | 端点返回 `IceServer` 含 `username`/`credential`，且凭据有 TTL(默认24h)；移除 `AERO_TURN_USERNAME`/`PASSWORD` 静态 env 读取 |
| **TASK-004** | 添加 TURN 凭据 TTL 配置 | `config.example.toml`, `crates/aero-common/src/config.rs` | TASK-002 | 1h | `AERO_TURN_CREDENTIAL_TTL_SECS` 可配置(默认86400)；<=0 退化为静态模式 |
| **TASK-005** | 添加迁移测试 + 兼容旧客户端 | `crates/aero-server/src/routes/routes_tests.rs` | TASK-004 | 1.5h | 旧 client 不带 `since` 仍能获得合法凭据(可能非时间受限)；凭据格式变化不引入 JSON 破坏 |

**小计：9.5h**

### D2: @everyone 成本治理

| ID | 任务标题 | 涉及文件 | 前置 | 工时 | 验收标准 |
|----|---------|---------|------|-----|---------|
| **TASK-006** | Web UI：广播提及确认对话框 | `web/app.js`、`web/mentions.js` | 无 | 3h | 输入 `@channel`/`@everyone`/`@all` 后弹确认弹窗：「将通知所有成员(n人)」；确认后才发送 |
| **TASK-007** | 客户端广播提及限频 | `web/app.js`、`web/ws.js` | TASK-006 | 2h | 同一用户 60s 内最多 1 次 `@everyone`；超限弹 toast「广播提及过于频繁」 |
| **TASK-008** | 服务端广播提及 DoS 防护 | `crates/aero-im-core/src/service/orig.rs` | 无 | 2h | `wants_all` 分支添加 `per_sender_broadcast_budget`：同房间 300s 内最多 3 次（可配置 `AERO_BROADCAST_MENTION_BUDGET`） |
| **TASK-009** | 广播提及审计日志 | `crates/aero-im-core/src/service/orig.rs`；`crates/aero-storage/src/audit.rs` | TASK-008 | 1.5h | 每次广播提及记录 `(sender, room, timestamp, member_count)` 到 `audit_events` 表 |
| **TASK-010** | 静音/离线用户广播豁免 | `crates/aero-im-core/src/service/orig.rs` | 无 | 3h | `wants_all` 展开时过滤掉 `notif_prefs` 中静音广播的用户；DND/凌晨时段不走推送 |

**小计：11.5h**

### D3: Interaction/MessageSeen 断头修复

| ID | 任务标题 | 涉及文件 | 前置 | 工时 | 验收标准 |
|----|---------|---------|------|-----|---------|
| **TASK-011** | Web SPA: 添加 `msg:message_seen` handler | `web/app.js`、`web/render.js` | 无 | 2h | handler 解析 `{room_id, message_id, participant}` → 在对应消息下方追加"X 已阅"指示 |
| **TASK-012** | Web SPA: 添加 `msg:interaction` handler | `web/app.js`、`web/livecards.js` | 无 | 2h | handler 解析 `{room_id, message_id, participant, action_id}` → 交互式组件的制作者看到点击反馈(toast/标记) |
| **TASK-013** | WS 帧类型注册表补全 | `web/app.js` hookWs() | TASK-011, TASK-012 | 0.5h | 确认所有 `ServerFrame` variant 都对应 `msg:*` handler；添加集成 smoke test |

**小计：4.5h**

### D4: Delivery Cursor 客户端持久化

| ID | 任务标题 | 涉及文件 | 前置 | 工时 | 验收标准 |
|----|---------|---------|------|-----|---------|
| **TASK-014** | 添加 `PUT /api/rooms/:id/delivery-cursor` 端点 | `crates/aero-server/src/routes/routes.rs` | 无 | 2h | 新路由，接收 `{message_id, seq}`，调用 `DeliveryCursorRepo::advance()`；成员门控 |
| **TASK-015** | Web SPA: `markRead` 后同步调 delivery-cursor | `web/app.js`(`maybeMarkRead`)、`web/api.js` | TASK-014 | 2h | 每发送 WS `mark_read` 帧后，异步 `PUT /api/rooms/:id/delivery-cursor`；失败降级(不阻塞) |
| **TASK-016** | 恢复连接时使用 `cursors` 参数 | `web/ws.js` `_open()` | TASK-015 | 1.5h | WS 连接 URL 附加 `&cursors=1` 当且仅当存在至少一个 delivery-cursor 记录；确保多设备收敛 |

**小计：5.5h**

### D5: 优雅降级治理

| ID | 任务标题 | 涉及文件 | 前置 | 工时 | 验收标准 |
|----|---------|---------|------|-----|---------|
| **TASK-017** | AI 服务超时断路器 | `crates/aero-ai/src/service.rs` | 无 | 3h | 每 AI 请求超时门(默认 30s)；连续 3 次超时=open circuit 60s；有 Metrics 指标 |
| **TASK-018** | NATS 扇出满队列告警 + 指标 | `crates/aero-server/src/hub.rs`、`ws/ws_impl/bus.rs` | 无 | 2h | `fan_out_raw` bounded mpsc 满时 ①打 `hub_queue_dropped_total` counter ②warn 日志 ③发 `msg:resync` 帧令客户端 REST 补拉 |
| **TASK-019** | 直播 HLS 写入非阻塞化 | `crates/aero-live-hls/src/writer.rs` | 无 | 3h | HLS writer 写 `.ts`/`.m3u8` 在独立 `spawn_blocking` 线程，不阻塞 RTMP/WHIP 摄入主循环 |
| **TASK-020** | 制定降级矩阵文档 | `docs/operations/degradation-matrix.md` | TASK-017~TASK-019 | 2h | 每个下游故障模式的系统行为文档化；通过代码审查 |

**小计：10h**

---

## 2. 执行顺序与依赖图

```mermaid
graph TD
    %% D1 — TURN
    T001[TASK-001: 抽离TurnConfig至common] --> T002[TASK-002: 参与者绑定凭据]
    T002 --> T003[TASK-003: 重写rtc_config_payload]
    T002 --> T004[TASK-004: TTL配置]
    T003 --> T005[TASK-005: 迁移测试+兼容]

    %% D2 — @everyone
    T006[TASK-006: 广播确认对话框] --> T007[TASK-007: 客户端限频]
    T008[TASK-008: 服务端DoS防护]
    T008 --> T009[TASK-009: 审计日志]
    T010[TASK-010: 静音/离线豁免]

    %% D3 — Interaction/MessageSeen
    T011[TASK-011: msg:message_seen handler]
    T012[TASK-012: msg:interaction handler]
    T011 --> T013[TASK-013: WS帧注册表补全]
    T012 --> T013

    %% D4 — Delivery Cursor
    T014[TASK-014: PUT端点] --> T015[TASK-015: markRead同步]
    T015 --> T016[TASK-016: 恢复连接使用cursors]

    %% D5 — 降级
    T017[TASK-017: AI超时断路器]
    T018[TASK-018: NATS满队列告警]
    T019[TASK-019: HLS写非阻塞化]
    T020[TASK-020: 降级矩阵文档]

    %% 分组标注
    subgraph D1[TURN 凭据动态化]
        T001;T002;T003;T004;T005
    end
    subgraph D2[@everyone 成本治理]
        T006;T007;T008;T009;T010
    end
    subgraph D3[断头修复]
        T011;T012;T013
    end
    subgraph D4[Delivery Cursor]
        T014;T015;T016
    end
    subgraph D5[优雅降级]
        T017;T018;T019;T020
    end
```

**可并行执行的任务组**：
- **组A**（Rust 后端）：TASK-001→TASK-002 → TASK-003/TASK-004；TASK-008/TASK-010；TASK-014；TASK-017/TASK-018/TASK-019
- **组B**（Web 前端）：TASK-006→TASK-007；TASK-011/TASK-012；TASK-015/TASK-016
- **组C**（文档/配置）：TASK-020；TASK-005

组A可投入 2 名后端开发并行通道；组B投入 1 名前端开发；组C由任一成员穿插。

---

## 3. 技术风险分析

### 高风险项

| 风险 | 方向 | 概率 | 影响 | 缓解策略 |
|------|------|-----|------|---------|
| TURN `TurnConfig` 从 `aero-live-srt` 抽离至 `aero-common` 引入循环依赖 | D1 | 中 | 高 | 预先验证 `aero-common` 当前不依赖 `aero-live-srt`（应成立）；抽离后 `aero-live-srt` 依赖 `aero-common`（已有） |
| Web 端 `msg:interaction` 帧结构可能与 REST 响应不同步 | D3 | 低 | 中 | 先读 `ServerFrame::Interaction` 定义(`ws/ws_impl/mod.rs:247`)，确保 handler 匹配字段名 |
| Delivery cursor 与现有 `mark_read` WS 帧的竞态：WS 帧先到、REST PUT 后到 | D4 | 中 | 中 | `advance` 方法已有 `last_seq` 单调递增闸（低 seq 不覆盖），PUT 后到不会回退 |
| AI 超时断路器在无 AI key 退化路径下行为 | D5 | 低 | 低 | `HashEmbedder` 路径不走网络，不应触发；只保护含 Anthropic/Voyage 凭据的路径 |

### 依赖外部系统

- **无新的外部服务依赖**：所有方向都在既有技术栈内完成（PG、Redis、NATS）
- **TURN 凭据**：需确认 coturn 已配置 `use-auth-secret` 以支持 REST 凭据（已有 `aero-live-srt` `TurnConfig::render()` 生成对应 conf）
- **浏览器兼容**：TURN REST 凭据是标准 WebRTC `RTCIceServer`，无兼容问题

### 性能瓶颈

| 方向 | 当前瓶颈 | 优化策略 |
|------|---------|---------|
| D2 @everyone | O(N) 通知/推送写放大 | `NotifyBatch` 已收敛 NATS，剩下推送网关写放大（每个设备一条 FCM/APNs）；考虑推送去重/批次合并 |
| D4 Delivery Cursor | 每个 markRead 做一次 PG write | 去抖：WS `mark_read` 帧客户端已去抖(只发最高 id)；服务端 `advance` 有 `ON CONFLICT` 幂等 |
| D5 NATS 满队列 | 慢消费者丢帧 | `msg:resync` + REST 补拉是正确缓解；考虑为高优先帧(消息/通话信令)预留 LIFO 通道 |

---

## 4. 资源评估

### 人员需求

| 角色 | 人数 | 方向覆盖 | 技能要求 |
|------|-----|---------|---------|
| Rust 后端工程师(高级) | 1 | D1(后端)、D5(断路器/HLS) | 熟悉 tokio/nats/aero-crate 结构；能重构跨 crate 类型 |
| Rust 后端工程师(中级) | 1 | D2(@everyone 服务端)、D4(PUT端点) | 熟悉 axum/sqlx/仓储模式；能写迁移测试 |
| 前端工程师 | 1 | D2(UI)、D3(WS handler)、D4(客户端) | 熟悉 ES2020/WebSocket/原生 JS SPA；无需框架 |
| QA/DevOps | 0.5 | 所有方向 | CI 集成测试、migration replay、e2e smoke |

**合计 ~3.5 名 FTE**，如并行后端通道则最快 2 名后端 + 1 名前端的 3 人团队。

### 关键里程碑

| 里程碑 | 时间(假设 3 人全时) | 交付物 |
|--------|-------------------|--------|
| **M1** 基础设施完成 | 第 3 天 | TASK-001~TASK-002；TASK-008 基础防护；TASK-014 端点；TASK-017 断路器 |
| **M2** 核心功能封闭 | 第 6 天 | TASK-003~TASK-005；TASK-006~TASK-007；TASK-011~TASK-013；TASK-015~TASK-016 |
| **M3** 集成测试完成 | 第 8 天 | 所有任务代码完成；`cargo check --workspace` 干净；所有 #\[ignore\] PG 测试通过 |
| **M4** 发布 | 第 10 天 | 降级矩阵文档；CI pipeline green；Smoke test 通过 |

### 阻塞点

| 阻塞点 | 方向 | 性质 | 解决策略 |
|--------|------|------|---------|
| `TurnConfig` 坐标选择 | D1 | 设计决策 | 推荐放 `aero-common/src/turn.rs`（叶子 crate，所有消费方可达）；备选 `aero-signaling`（已含 `IceServer` 类型） |
| `@everyone` 广播预算状态存储 | D2 | 存储选择 | 进程内 `HashMap<(ParticipantId, RoomId), Vec<Instant>>` 即可（重启后重置保守）；需要持久化可加 Redis 但此为 P2 |
| Web SPA 无单元测试框架 | D3/D4 | 测试覆盖 | 当前 web 侧无框架，事件 handler 可用手动 `dispatchEvent` 测试；验收以端到端 smoke 测试为准 |

---

## 5. 质量保证

### 5.1 单元测试覆盖要求

| 模块 | 最低覆盖率(新代码) | 关键测试用例 |
|------|-------------------|-------------|
| `TurnConfig::ephemeral_credential()` | 100% | HMAC 输出不变性；TTL 边界(0, MAX)；UTC 时钟注入一致性 |
| `DeliveryCursorRepo::advance()` | 100% | 单调递增幂等；回退拒绝；并发冲突 |
| 广播提及限频(budget) | 95% | 阈值内/超/重置；不同房间独立；重启后重置 |
| AI 断路器 | 100% | 连续失败开门；成功关门；半开试探；并发安全 |
| WS 帧序列化/反序列化 | 100% | 每个 `ServerFrame` variant 都 roundtrip；tag 一致性 |

### 5.2 集成测试策略

| 测试类型 | 工具/方法 | 覆盖范围 |
|---------|----------|---------|
| **PG 门控测试** | `#[ignore = "requires live Postgres"]` + `DATABASE_URL` | `DeliveryCursorRepo` 集成(mig 0153)；审计日志写入 |
| **WS 协议一致性** | `ws.rs` 连接 + 帧交互 | 确认 `msg:interaction`/`msg:message_seen` 帧可接收解析 |
| **REST API** | `aero-server` routes_tests | `PUT /api/rooms/:id/delivery-cursor` 的 200/401/403/404 |
| **多节点烟雾** | `docker-compose` 双实例 | TURN 凭据跨节点一致；@everyone 不产生 NATS 风暴 |

### 5.3 代码审查要点

| 关注点 | 说明 |
|-------|------|
| **Crate 层级检查** | `TurnConfig` 移动后 `aero-common` → `aero-live-srt` 依赖是否单向；root `Cargo.toml` 不得新增 str0m |
| **serde tag 冲突** | D3 新 WS 帧确认无 `kind` 字段与 `tag="kind"` 冲突；已用 `#[serde(rename)]` 模式 |
| **幂等检查** | D2 广播审计行 `ON CONFLICT DO NOTHING`；D4 `advance` `ON CONFLICT ... WHERE EXCLUDED.last_seq > delivery_cursors.last_seq` |
| **鉴权检查** | D4 PUT 端点必须 `assert_room_access` 先于写入；CI `authz_lint` 会红 |
| **Fail-open 检查** | D5 断路器 open 时应返回 503 而非 panic；NATS 满队列不应阻塞 `run_bus_listener` 循环 |

### 5.4 性能测试需求

| 测试场景 | 方法 | 通过标准 |
|---------|------|---------|
| TURN 凭据生成延迟 | 压测 `GET /api/rtc-config` 1000 QPS | p99 < 5ms (仅 HMAC-SHA1) |
| @everyone 展开 5000 人房间 | 模拟 `NotifyBatch` + 推送通路 | PG/Redis 无慢查询；NATS subject 1 次 publish |
| Delivery cursor 写入 | `advance` 2000 QPS 单房间 | `ON CONFLICT` 无死锁；p99 < 10ms |

---

## 6. 实施计划

### 时间线 — 3 人全时 (2 后端 + 1 前端)

```
天 1-3
├── 后端 1 (高级)
│   ├── TASK-001: 抽离 TurnConfig → aero-common
│   ├── TASK-002: 参与者绑定凭据
│   └── TASK-017: AI 超时断路器
├── 后端 2 (中级)
│   ├── TASK-008: 服务端广播 DoS 防护
│   ├── TASK-010: 静音/离线广播豁免
│   └── TASK-014: PUT /api/rooms/:id/delivery-cursor
└── 前端
    ├── TASK-006: 广播确认对话框(label only, 后端TASK-008完成后通调)
    └── TASK-011: msg:message_seen handler

天 4-6
├── 后端 1
│   ├── TASK-003: 重写 rtc_config_payload (依赖 TASK-002)
│   ├── TASK-004: TTL 配置
│   └── TASK-018: NATS 满队列告警
├── 后端 2
│   ├── TASK-009: 审计日志 (依赖 TASK-008)
│   ├── TASK-005: 迁移测试+兼容 (依赖 TASK-003)
│   └── TASK-019: HLS 写非阻塞化
└── 前端
    ├── TASK-007: 客户端广播限频 (依赖 TASK-006)
    ├── TASK-012: msg:interaction handler
    └── TASK-015: markRead 同步 delivery-cursor (后端TASK-014完成后)

天 7-8
├── 后端 1 → 协助测试，修复 bug
├── 后端 2 → TASK-020: 降级矩阵文档初稿
└── 前端
    ├── TASK-016: 恢复连接使用 cursors 参数
    └── TASK-013: WS 帧注册表补全

天 9-10
├── 集成测试 → cargo check --workspace → cargo clippy → cargo test
├── migration replay smoke (make migrate-smoke)
├── web-check / truth-check / file-size-check
└── TASK-020: 降级矩阵文档终稿
```

### 风险预留

- **Buffer**: 每天预留 1h 代码审查 + 1h 应急修复 => 10 天实际净约 70h 有效产出，与总预估 41h 核心开发 + ~15h 测试/文档吻合
- **Seam 处理**: §2 媒体 seam（sfu_media_session 未接线）受影响的边界仅 D5 HLS 写入优化 — 不交叉
- **Rollback 方案**: 每个方向独立 MR，新路由加 `.route()` 而非改既有，可逐 MR revert

### 提交流程

```
Step 1: 工具链统一
  git reset --hard master        # 基线校准
  cargo check --workspace        # 确认干净

Step 2: MR 拆分（每个方向至少一个 MR）
  MR-D1: TASK-001~TASK-005
  MR-D2: TASK-006~TASK-010
  MR-D3: TASK-011~TASK-013
  MR-D4: TASK-014~TASK-016
  MR-D5: TASK-017~TASK-020

Step 3: 每个 MR 合并前
  cargo check --workspace
  cargo clippy --workspace --all-targets  # 无新增警告
  cargo test --workspace --lib             # 全绿
  scripts/{truth-check,file-size-check,web-check}.sh  # 0 违规
```

---

## 附录：代码引用验证矩阵

| 分析文档声明 | 代码验证 | 状态 |
|------------|---------|------|
| D1: `rtc_config_payload()` 返回静态凭据 | `routes.rs:2824-2835` 确认：直接读取 `AERO_TURN_{URL,USERNAME,PASSWORD}` | ✅ |
| D1: 时长受限凭据基础已存在 | `aero-live-srt/src/lib.rs:890-930`：`TurnConfig::ephemeral_credential()` | ✅ 文档未提 |
| D2: `n_token()` 展开无限制 | `orig.rs:698-712`：`wants_all` = `member_set.clone()` | ✅ |
| D2: `NotifyBatch` 已存在 | `event.rs:95-105` | ✅ 文档未提 |
| D3: `ServerFrame::Interaction`/`MessageSeen` 存在 | `ws/ws_impl/mod.rs:244-250` | ✅ |
| D3: web `app.js:110-128` 无 handler | 代码确认：`msg:interaction`/`msg:message_seen` 不在 handler 注册表 | ✅ |
| D4: `delivery_cursors` 仓储 + REST API 存在 | `delivery_cursor.rs`、`routes.rs:92/1244` | ✅ |
| D4: WS `?cursors=` 参数存在 | `ws_impl/mod.rs:53-56` `WsParams.cursors` | ✅ 文档未充分提 |
| D4: Web SPA 不调用 delivery-cursor | `app.js:827` `markRead` 只发 WS `mark_read` | ✅ |
| D4: 无 PUT 端点 | `routes.rs:92`：只有 `get(get_delivery_cursor)` | ✅ 文档未提此具体缺口 |
| D5: Redis→PG fallback 已存在 | `online.rs:55-59` | ✅ 原分析文档断言有误 |
| D5: NATS 扇出静默丢帧 | `hub.rs` `fan_out_raw` bounded mpsc 满时行为需确认 | ⚠️ 需进一步验证 |

---

**总建议优先级排序**：
1. **P0 安全缺陷**：D1 TURN 凭据（静态凭据可被中间人捕获长期复用）
2. **P0 功能完整性**：D3 断头（WS 帧静默丢 = UI 状态不一致）
3. **P1 成本防护**：D2 @everyone（设施已有但缺安全阀）
4. **P1 一致性**：D4 Delivery Cursor（多设备收敛基础完备但未被客户端使用）
5. **P2 韧性**：D5 优雅降级（当前运行受影响概率低但积累技术债）
