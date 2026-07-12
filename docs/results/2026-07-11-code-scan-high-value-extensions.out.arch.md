# 架构分析报告：Aero IM — 五个持久性生产缺口再验证

## 1. 架构评估

### 1.1 当前架构优势

经过对代码库的实际调研，Aero IM 展现了一个**成熟的事件驱动型微服务架构**，其核心设计决策具有以下优势：

| 优势 | 证据 |
|------|------|
| **清晰的层级分离** | 严格自下而上的 crate 依赖图（`aero-common` → `aero-bus` → `aero-storage` → `aero-im-core` → `aero-server`），无循环依赖 |
| **事件 DAG 设计合理** | NATS JetStream 作为跨实例事实源，`Hub` 仅做进程内 bounded mpsc 扇出，水平扩展通道清晰 |
| **幂等与降级嵌入骨架** | `delivery_cursors` 单调 seq upsert（`ON CONFLICT ... WHERE EXCLUDED.last_seq > delivery_cursors.last_seq`），`online.rs` Redis 故障回落本地 Hub |
| **审计与合规内建** | 软删审计事务、法务保全跳越、GDPR 逐表清扫——非后加补丁 |
| **AI 退化路径完整** | 无 key 时 `HashEmbedder`(1024维) + 启发式 completion，保持 200 响应而非 500 |

### 1.2 架构债务与技术债

#### **P0 安全债：TURN 静态凭证**（`routes.rs:2828-2855`）

```rust
fn rtc_config_payload() -> serde_json::Value {
    // ...
    if let (Ok(url), Ok(user), Ok(pass)) = (
        std::env::var("AERO_TURN_URL"),
        std::env::var("AERO_TURN_USERNAME"),
        std::env::var("AERO_TURN_PASSWORD"),
    ) {
        // 相同静态凭证返回给每一个客户端
        ice_servers.push(serde_json::json!({...}));
    }
}
```

**严重性**：每个 `GET /api/rtc-config` 响应都将相同的 TURN 长期凭证以明文形式发送给所有客户端。这意味着：
- 任何客户端可以提取凭证并消耗 TURN 带宽（成本攻击）
- 凭证无法逐用户吊销
- 不符合 TURN REST API 最佳实践（RFC 7065 §3.2 / coturn `static-auth-secret` 模式）

**标准做法**：使用 TURN REST API 流程——服务端用共享密钥为每个用户生成 `username={timestamp}:{userId}` + HMAC `credential`，有效期为 TTL。coturn 原生支持此模式。

#### **P1 协议债：@everyone 通知风暴无防御**

`n_token()` / `is_all_broadcast_token()` 函数在测试中存在，但实际 `NotifyBatch` 展开逻辑缺乏广播提及的门控措施。现有状态：

- 成员展开是 O(N) DB 往返 → 大房间（1000+成员）的 `@everyone` 产生等量通知行 + 推送
- 无 per-sender 广播冷却（`AERO_RATE_LIMIT_PER_SEC` 是全局 API 限流，非通知扇出控速）
- 无广播范围限制（是否允许在 >100 人的频道使用 `@everyone`？）

#### **P2 协议债：Delivery Cursor 客户端未接线**

基础设施端比分析文档所述的更完整：
- ✅ `DeliveryCursorRepo` — 已实现并含 monotonic upsert
- ✅ `WsParams.cursors` — WS 升级参数已支持
- ✅ `AppState.delivery_cursors` — 已在 state 中装配
- ❌ **web SPA 未调用** `PUT /api/rooms/:id/delivery-cursor` — `ws.markRead()` 只发 `mark_read` 帧
- ❌ **reconnect 路径未读取** `GET /api/rooms/:id/delivery-cursor` — 使用全局 `?since=`而非逐房间 cursor

这意味着 `DeliveryCursorRepo` 实际上零行——所有基础设施都在，但客户端从未写入。

#### **P3 架构债：Interaction/MessageSeen 代码不存在**

根据代码库搜索：

```
# 在 crates/ 和 web/ 中搜索 Interaction / MessageSeen → 0 结果
```

被分析文档声称的 `ServerFrame::Interaction` 和 `ServerFrame::MessageSeen` **在当前 master 分支上不存在任何代码**。这暗示原分析文档可能基于：
- 某个 git worktree 或未合并分支
- 已废弃的设计草图

**教训**：分析引用必须指定确切 commit/branch，否则结论可能误导。

---

## 2. 扩展方向

### **方向一：动态 TURN 凭证系统**（P0）

**为什么需要**：
- 防止 TURN 带宽盗用和凭证泄露
- 满足企业安全合规要求
- 支持多租户用量计费

**核心挑战**：
1. 需要引入 TURN REST API 服务端流程（coturn `static-auth-secret` 模式）
2. 凭证需有 TTL（通常 1-6 小时），但 WS 连接可能在凭证过期后仍然存活
3. 需要在 WebRTC 连接失败时优雅刷新凭证（`iceconnectionstatechange` → `failed`）

**预期架构变更**：
```
当前: rtc_config_payload() → 直接从 env 读取 → 返回静态 JSON

未来: TurnCredentialService
  ├── generate(user_id, ttl) → { username, credential, urls }
  ├── 端口：REST GET /api/rtc-config (返回有时间戳的凭证)
  ├── 支持：coturn REST API 格式（/turn/{username}/{ttl} 鉴权）
  └── 无 TURN 配置时：退回到纯 STUN
```

**对现有系统的影响**：
- 仅影响 `routes.rs` 的 `rtc_config_payload()` → 单项替换
- `aero-signaling/src/types.rs` 的 `default_rtc_config_from_env()` 仍然有效作为 fallback
- 前端 WS 连接逻辑需支持凭证刷新（`RTCPeerConnection` 的 `setConfiguration()`）

**选项对比**：

| 方案 | 复杂度 | 安全性 | 维护成本 |
|------|--------|--------|----------|
| **A. 自建 HMAC 签发器** | 中（~100 行 Rust） | 高 | 低 |
| **B. 代理 coturn REST API** | 低（反向代理配置） | 高 | 低（但运维依赖） |
| **C. 保持现状 + 限制 IP** | 低 | 低（仅网络层控制） | 中（运维负担） |

**推荐**：方案 A + B 结合——自建 HMAC 签发点，同时支持直连 coturn REST API。

---

### **方向二：广播提及治理与通知风暴防护**（P1）

**为什么需要**：
- 防止 `@everyone` 请求导致 O(N) DB 写放大 + 推送风暴
- 大型工作区（10000+ 用户）的 `@channel` 可能产生百万级通知行
- 用户投诉+基础设施成本双重问题

**核心挑战**：
1. 广播提及需要区分「谁被通知」和「谁收到推送」
2. 大通知批次的事务边界——Postgres `VALUES` 批量写入 + NATS 扇出顺序
3. 用户静音/免打扰/消失模式（`notif_prefs`）需要在此阶段过滤

**预期架构变更**：

```
广播提及处理管线：

1. 解析块 → 提取 token（保持现有 group_handle_tokens）
2. 广播 token 识别（现有 is_all_broadcast_token / is_here_token）
3. → 门控检查：
   - 房间成员数 > 广播阈值（例如 200）→ 提示确认
   - per-sender 广播冷却（例如 5min）
   - 管理员/角色豁免
4. → 批量写入通知表（现有 NotifyBatch 逻辑）
5. → NATS 扇出 → 推送筛选
```

**关键设计决策**：

| 维度 | 选项 A：严格门控 | 选项 B：异步+预算 | 选项 C：隔离通知优先级 |
|------|-----------------|-------------------|----------------------|
| 模型 | 广播前检查+确认 | 广播入队列，worker 慢消费 | 广播通知写入高优表，推送单独限流 |
| 延迟 | 用户可见（确认弹窗） | 秒级异步 | 无额外延迟 |
| 公平性 | 完全公平 | 全局预算共享 | 每个广播独立预算 |
| 实现复杂度 | 低 | 中（已有 `AiWorker` 模型可复用） | 中高 |

**推荐**：选项 A（门控）+ 选项 C（推送分级）。广播确认作为第一道防线，推送侧使用 per-workspace 速率预算防止下游 FCM/APNs 超限。

---

### **方向三：Delivery Cursor 端到端集成**（P2）

**为什么需要**：
- 当前 `?since=` 单游标方案在多房间场景下回放大量不必要数据
- 多设备间投递状态无法收敛（设备 A 读了，设备 B 重连不知道）
- `DeliveryCursorRepo` 已建但零数据——投入产出比极高

**核心挑战**：
1. 客户端侧需要维护游标状态（Web SPA 的 IndexedDB 或内存状态）
2. `mark_read` 帧 vs `delivery_ack`——已读回执与投递确认是两个语义，需要清晰分离
3. 游标收口策略：多设备同时 ACK 时，单调 seq 确保正确收敛

**预期变更**：

```
客户端侧（web SPA）：
1. reconnect 时：如果 ?cursors=1 || ?cursors=true，调用 GET /api/rooms/:id/delivery-cursor
   → 替代全局 ?since= 回放
2. 收到每条消息：比对已存消息 ID，更新内存游标
3. 周期/DOM 卸载前：PUT /api/rooms/:id/delivery-cursor { last_message_id, seq }
4. markRead 保持仅发 ws mark_read 帧（视觉已读），不改变 delivery cursor

服务端侧：
✅ WsParams.cursors 已存在
⬜ 需要实现 GET /api/rooms/:id/delivery-cursor
⬜ reconnect 路径：当 cursors=true 时，为每个房间读取独立游标
```

**对现有系统的影响**：
- 服务端：新增 REST 端点（~30 行），修改 backfill 逻辑（~50 行）
- 客户端：新增 `deliveryCursor.js` 模块（~150 行），修改 `ws.markRead()`（~20 行）
- **不改变**现有 `mark_read` 路径的语义

---

### **方向四：集群级弹性与退化行为统一（P2-P3）**

**为什么需要**：
- 当前退化策略在各个模块中分散定义、行为不一致
- 分析文档中方向五的 Redis→500 断言虽然在 `online.rs` 不成立，但这因为恰好有人写了 fallback，而非框架级机制

**核心挑战**：
1. 每项外部依赖（PG/Redis/NATS/Blob store）都需要声明退化策略：`hard-dependency | degrade-gracefully | degrade-with-cache`
2. 多依赖同时故障时的组合行为需要定义
3. 运维可观测（哪个 fallback 被击中、频率）

**预期架构变更**：

```
trait DependencyHealth {
    /// 当前依赖的健康状况
    fn health(&self) -> DependencyStatus;
    /// 是否关键路径必须可用
    fn is_critical(&self) -> bool;
    /// 降级策略
    fn on_degradation(&self) -> DegradationStrategy;
}

enum DegradationStrategy {
    FailOpen,        // 返回空/旧值，不断言错误
    FailClosed,      // 返回 503，安全失败
    FallbackToCache, // 使用 TTL 缓存（如 participant_cache）
    DegradeFeature,  // 降级功能但不断言错误（如 presence fallback to Hub）
}
```

**具体影响分析**：

| 依赖 | 当前行为 | 问题 | 建议策略 |
|------|---------|------|---------|
| Redis | `member()` 有 fallback；`presence.count()` 有 fallback；`ws_rate` 未知 | 不一致——部分路径直接 500 | 统一至少 `DegradeFeature` |
| NATS | `publish` 失败 `propagate` | bus 是核心扇出路径，NATS 宕机意味着实时消息全挂 | `FailClosed` 合理，但应增加 `is_healthy` 探针 |
| AI | 正常 fallback 到 `HashEmbedder` | 已经是正确行为，但仅在连接时检测 | 增加运行时活跃探针 |
| PG | 所有仓储 `propagate` | 合理——无 PG 则无持久状态 | `FailClosed` 但应提前在 /health 暴露 |

---

### **方向五：WebSocket 协议规范与 API 契约维护**（P2）

**为什么需要**：
- Interaction/MessageSeen 的代码不存在暴露了一个更大的问题：WS 协议没有集中维护的规范文档或契约测试
- 前端 handler 注册表（`ws.on(...)`）和服务端 `ServerFrame` 枚举之间没有自动校验——帧可被添加但永远无人消费

**核心挑战**：
1. 如何在不增加过多维护负担的情况下确保 WS 协议的前端-后端同步
2. 现有 15+ 帧类型 + 持续增长，人工检查不可持续

**预期变更**：

```
方案 A：契约文件（最轻量）
- 维护 WS_PROTOCOL.md，列出所有帧类型 + 方向（S→C / C→S）
- CI 步骤检查 server 的 ServerFrame match arms 和 client 的 ws.on() 注册表

方案 B：TypeScript 类型导出（中量）
- 从 Rust 的 ServerFrame/ClientFrame 枚举导出 JSON Schema / TypeScript types
- 前端消费导出的类型来注册 handler

方案 C：端到端帧覆盖率测试（重量）
- 每个 ServerFrame variant 有一个对应的集成测试
- 测试发送事件并通过 mock WS 连接验证客户端收到正确帧
```

**推荐**：先上方案 A（纯文档+CI grep 验证，~0 代码变更），后考虑方案 B 作为长期演进方向。

---

## 3. 接口设计建议

### 3.1 核心原则

1. **事件向前的兼容性**：`RoomEvent`/`StreamEvent` 使用 `#[serde(tag = "kind")]`，新增 variant 必须考虑旧客户端会静默丢弃未知帧。标记新帧为 `#[serde(deny_unknown_fields)]` 确保解析失败时早期暴露。

2. **仓储构造模式**：当前模式 `XRepo::new(s.pg.clone())` 在内联构造仓储——虽方便，但导致 `state.rs` 中字段膨胀。**建议过渡到 trait 化仓储**，使模块可以声明自己的依赖接口而非直接使用 `PgPool`。

3. **降级谱系显式声明**：每个服务/仓储组件应该声明其在依赖故障时的行为，而非散布在调用点中。

### 3.2 新抽象层建议

| 抽象层 | 职责 | 当前状态 | 建议 |
|--------|------|---------|------|
| `CredentialProvider` trait | 临时凭证生成（TURN HMAC 密钥） | 不存在 | 新建 |
| `NotificationThrottle` | 广播通知频率和范围控制 | 隐含在 ws_rate 中 | 提取为独立模块 |
| `DeliveryCursorService` | 投递游标聚合逻辑（非直接 DB 操作） | 仅有 storage repo | 新建服务层 |
| `HealthRegistry` | 依赖健康聚合 + 退化策略映射 | 分散在 `routes/health.rs` | 提取为共享组件 |

### 3.3 向后兼容性策略

1. **TURN 凭证**：
   - 旧客户端使用静态凭证继续工作（在迁移期内保留旧端点）
   - 新端点 `/api/rtc-config/v2` 返回 `RtcConfigResponse { credentials: TURN_CREDENTIALS_V2 }`
   - 客户端的 `RTCPeerConnection.setConfiguration()` 可热更新

2. **Delivery Cursor**：
   - `?since=` 保持为默认行为（`cursors` 参数 opt-in）
   - 标记旧行为为 `deprecated` 在文档中
   - 版本 2 发布后切换默认值

3. **广播门控**：
   - 第一阶段：仅日志记录（`warn!("broadcast too large")`），不改变行为
   - 第二阶段：强制确认（`confirm: true` 响应体字段要求客户端弹窗）
   - 第三阶段：严格限流

---

## 4. 技术选型

### 4.1 是否需要新技术栈

| 领域 | 建议 | 理由 |
|------|------|------|
| TURN 凭证 | **无需**新依赖 —— coturn `static-auth-secret` + HMAC = ~80 行纯 Rust | 已有 `sha2` 依赖 |
| 通知管理 | **无需**新依赖 —— 复用 `AiWorker` 模型（已有 `Semaphore` + `CostBudget`） |
| 协议契约 | **轻量工具**：类型导出用 `schemars`（可选）或手写 CI grep | 避免过度工程 |
| 可观测 | 复用现有 Prometheus metrics | 无需 OpenTelemetry 扩展 |

### 4.2 第三方依赖评估标准

对于新增依赖，逐一对照 AGENTS.md 的硬规则：

1. **`unsafe_code` 禁令**：依赖必须无 `unsafe` 或理由充分（`ring` 级）
2. **MSRV 兼容性**：1.80 以上必须验证
3. **transitive 膨胀**：优先选择 `thiserror`/`serde`/`tokio` 生态中已有的依赖，避免新增 major 框架
4. **双许可以及专利风险**：AGPL / BUSL 拒绝

### 4.3 自建 vs 集成决策矩阵

| 功能 | 自建 | 集成第三方 | 决策 |
|------|------|-----------|------|
| TURN 凭证 | ~80 行，纯 HMAC | coturn REST API，需要 TURN 基础设施 | **自建 HMAC 签发层** + coturn `static-auth-secret` |
| 通知限流 | 复用 AiWorker 的 `CostBudget + Semaphore` | 无成熟 Rust 库 | **自建**（模式已验证） |
| 协议契约检查 | grep + 正则，~30 行 CI 脚本 | `schemars` + `JSON Schema` | **先自建脚本**，按需增加架构导出 |

---

## 5. 实施路线图

### 优先级定义

| 优先级 | 含义 | 影响面 |
|--------|------|--------|
| **P0** | 安全/合规阻断项，上线必须修复 | 外部攻击面 |
| **P1** | 生产稳定性风险，常驻潜在问题 | 大规模部署 |
| **P2** | 功能完整度缺口，用户/运维可感知但非阻塞 | 产品体验 |
| **P3** | 架构规范性/可维护性提升 | 长期成本 |

### 阶段划分

#### **Phase 1：安全补丁 + 闪电修复**（2-3 天）

| 事项 | 优先级 | 工作量 | 风险 |
|------|--------|--------|------|
| TURN HMAC 凭证签发器 | **P0** | ~80 行 Rust（`aero-signaling`） | 低——无外部依赖 |
| Delivery Cursor web SPA 集成 | **P2** | ~200 行 JS + 新增 GET 路由 | 中——需验证多设备场景 |
| WS 协议契约 CI 检查 | **P2** | ~30 行 shell | 低——纯 CI 变更 |

**里程碑 1**：TURN 凭证不再明文暴露；DeliveryCursor 写入有数据；`ws.on()` 缺失 handler 被 CI 捕获。

#### **Phase 2：广播通知治理**（1 周）

| 事项 | 优先级 | 工作量 |
|------|--------|--------|
| `NotifyBatch` 预展开检查：房间尺寸 > 阈值时确认 | **P1** | ~60 行 Rust |
| per-sender 广播冷却（`LastBroadcast` HashMap + TTL） | **P1** | ~40 行 Rust |
| 推送分级：广播触发类推送独立限流 | **P1** | ~80 行 Rust |
| 大型广播异步化（复用 `AiWorker` 的 job 队列模式） | **P1** | ~120 行 Rust |

**里程碑 2**：`@everyone` 不再为 5000 人房间产生 5000 条同步通知行。

#### **Phase 3：退化策略统一**（1 周）

| 事项 | 优先级 | 工作量 |
|------|--------|--------|
| `DependencyHealth` trait 定义 + 实现（PG/Redis/NATS/Blob） | **P2** | ~100 行 Rust |
| `/health/dependencies` 端点暴露每项依赖状态 + 活跃退化策略 | **P2** | ~60 行 Rust |
| Redis 降级审计计数（`prometheus::counter!(...)`） | **P2** | ~30 行 Rust |
| 将现有散落 fallback 路径统一到新策略 | **P3** | ~200 行 Rust（分散在多个模块） |

**里程碑 3**：运维可以看到「Redis 降级了 N 次」而无需 grep 日志。

### 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| TURN 凭证迁移中断实时通话 | 低 | 中 | 保留旧端点作为 fallback，新旧并存 2 个版本生命周期 |
| `DeliveryCursor` 客户端竞态条件 | 中 | 低 | 单调 seq upsert 已确保收敛到 `max(seq)` |
| 广播门控误拦截合法请求 | 中 | 中 | 第一阶段仅 `warn!` 不拦截，观察后启用 |
| 退化策略统一引入抽象的过度设计 | 中 | 低 | 限制 trait 为 3 方法 + 枚举，反对泛型或宏 |
| 分析文档引用不存在代码（Interaction/MessageSeen） | 高 | 低 | 核对源 commit 再行动，不基于未合并分支做决策 |

### 总结

这五个方向中：

| 方向 | 真实性问题 | 优先级 |
|------|-----------|--------|
| **TURN 凭证** | ✅ 真实存在，且是安全漏洞 | **P0** |
| **@everyone 成本** | ✅ 真实存在，架构风险 | **P1** |
| **Interaction/MessageSeen** | ❌ 当前代码库不存在，需检查源文档的 commit | — |
| **Delivery Cursor** | ✅ 客户端断开真实存在，但服务端基础设施比分析文档描述的完整 | **P2** |
| **优雅降级** | ± Redis→500 断言不准确（`online.rs` 有 fallback），但系统性退化策略缺失 | **P2-P3** |

**核心行动项**：Phase 1 在 2-3 天内消除 P0 安全债 + 激活零数据的 DeliveryCursor。Phase 2 在一周内构建广播通知的防御体系。Phase 3 建立可观测的退化策略框架。
