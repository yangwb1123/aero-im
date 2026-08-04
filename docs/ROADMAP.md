# Aero IM — 扩展路线图（ROADMAP · 第六版）

> 本文是平台纵深方向的**源码事实与运维剩余项**，不保存会漂移的迁移或
> 测试计数。工程约定见 [`AGENTS.md`](../AGENTS.md)，完整功能矩阵见
> [`README.md`](../README.md)。

---

## 缘起：平台纵深从规划进入交付与验收

前五版覆盖功能广度与正确性收口。本版原先列出的成本、可观测、离线投递、
扩展性、合规和协作方向，已有对应实现进入主工作树。本文不再把一次性扫描的
“未发现”当作永久事实，而以当前模块、迁移和运行路径为准。

当前“完成”分两类：可由源码、单测、PG 门控测试与 localhost smoke 证明的
实现；以及必须由真实环境或运维授权完成的验收。真实外部供应商凭据、跨主机
WebRTC/NAT 与生产 `messages` 分区切换属于后一类，不能由 hermetic 测试代替。

---

## 总览与优先级

| 优先级 | 方向 | 当前源码状态 | 剩余验收 |
|---|---|---|---|
| **P0** | 一、AI 成本治理与检索质量 | 用量台账、模型分层、租户隔离的答案缓存已实现 | 真实模型账单与质量基准校准 |
| **P0** | 二、端到端追踪 + 每租户归因 | envelope `traceparent`、消费端 parent 恢复、结构化日志和租户成本归因已实现 | 生产 SLO/告警/看板与真实 collector 验收 |
| **P1** | 三、持久投递游标 + 离线同步 | per-room `delivery_ordinal`、participant/room 游标、完整回放 barrier 与客户端 ACK 已实现 | 真实多设备断线重连与长时运行 staging 验收 |
| **P1** | 四、读副本 + 分区 + Redis 热键 | 一致性路由和 presence/viewer 256 路分片已实现；分区 shadow/backfill/runbook 已就绪 | 生产分区 cutover 必须获批维护窗 |
| **P2** | 五、合规 + Canvas | workspace 区域路由、legal-hold-aware 审计与签名导出、S3 SSE-KMS、Canvas op log/快照基线/幂等已实现 | 真实 S3/KMS 与受监管环境验收 |

> **核实原则**：执行或汇报前先 grep 当前源码；一次性扫描、迁移序号和测试
> 数量都会漂移。尤其不要把 shadow/runbook 就绪写成生产 cutover 完成，也不要把
> staging 联调项误写成未接线代码。

---

## 方向一（P0）·AI 成本治理与检索质量——把「会答题」升级为「答得准、付得起、算得清」

**已实现**：Voyage/FTS 经 RRF 融合并守住房间/工作区成员边界；
`CostBudget` / `KeyedCostBudget` 继续负责准入。每个付费 provider operation
在出网前以稳定 id 和 fencing token 写 reservation；成功按真实 token 成本
finalize 并同时保存最小可重放结果，普通失败 cancel，传输结果不明则保留并在
过期后按保守估算结算。因此业务写入失败后的稳定重试不会再次调用 provider。
最终台账由 `AiUsageRepo` 按 workspace 持久化并经 outbox 更新指标。答案路径
支持模型分层和房间隔离缓存，`Edited`/`Deleted` 事件会失效对应房间缓存。

**剩余验收**：用真实 provider 账单校准 token 单价、模型桶和预算阈值；以固定
语料持续测 RAG 的召回/排序质量。此项是运营调参与外部账单对账，不是缺少
服务端调用路径。

**关键边界**：缓存 key 必含租户/房间边界；消息变更必须失效；付费路径必须
先 reserve 后出网；不明确的 provider 结果宁可保守计费，也不能用同一稳定
operation id 再次出网。台账记录真实 token 成本而预算仍可使用保守估算，
两者口径不能混为一谈。锚点：
`aero-ai/service/accounting.rs`、`aero-ai/usage.rs`、`aero-ai/metrics.rs`、
`aero-storage/ai_context.rs`、`aero-storage/ai_usage.rs`。

---

## 方向二（P0）·端到端分布式追踪 + 每租户 SLO/成本归因——把「有指标」升级为「可规模化运营」

**已实现**：HTTP 入站提取 W3C `traceparent`，房间/直播事件 envelope 携带该
上下文，NATS 消费端恢复 parent；webhook 等出站路径保留追踪上下文。
`AERO_LOG_FORMAT=json` 使用 current-span JSON，Prometheus、OTLP、
`/health{,/live,/ready}`、RED 中间件和观测 gauge 已接线，可选 workspace
label 与 AI 用量台账承担租户归因。

**剩余验收**：本机 Jaeger/OTel collector 往返已通过；SLO、告警规则、仪表盘、
采样率和备份/恢复演练属于部署环境的运维配置，真实外部 collector 及供应商
凭据仍必须在 staging/production 验证。

**关键边界**：高吞吐下控制采样成本，span 不记录密钥/令牌等敏感值。锚点：
`aero-common/telemetry.rs`、`aero-bus/seq.rs`、
`aero-server/ws/ws_impl/bus.rs` 与 metrics samplers。

---

## 方向三（P1）·每用户持久投递台账 + 有序离线同步——兑现 IM「消息必达 + 离线无缝追平」的根本承诺

**已实现**：消息事务在房间级 counter 下取得正的、连续累积语义的
`delivery_ordinal`；creation outbox 以该 ordinal 保序。服务端
`delivery_cursors` 按 `(participant,room)` 保存
`last_delivery_ordinal`，NATS `seq` 只作诊断/实时去重，绝不再把独立的
ULID 最大值或 bus seq 拼成错误的“连续前缀”。

重连时服务端从每房 ordinal 完整分页回放；没有 cursor 的房间只给最新初始
窗口，旧历史继续走 REST。所有回放帧入队后才发送 `delivery_ready`，live 帧
在 barrier 前单独有界缓冲。Web 客户端在同步 handler 成功应用消息后才发送
`delivery_ack`，游标以 participant 作为本地存储命名空间，并以 socket/account
generation 防止旧连接污染新会话。

**状态**：实现、针对性测试、全工作区门禁与 localhost runtime 验收均纳入
交付流程。真实多设备、弱网断线重连和长时运行仍须在 staging 验证；源码就绪
不等于某个部署环境已经发布。

**关键边界**：游标只进不退；软删/过期消息不能作为明文回放；分页不能在任意
上限处提前发 barrier；多设备 ACK 以 ordinal 最大值收敛。锚点：
`aero-storage/delivery_cursor.rs`、`aero-storage/message/query.rs`、
`aero-server/ws/ws_impl/backfill.rs`、`web/ws.js`。

---

## 方向四（P1）·读副本路由 + 消息分区准备 + Redis 热键分片——跨越第一道扩展悬崖

**已实现——读副本合同**：`database.replica_url` 与 `QueryRouter` 只服务显式
`Eventual`。历史接口先在 primary 做完整鉴权，并由 primary 证明 `before`
后确有更新可见消息，才把它当作真正的旧页；未来 cursor 仍是最新页语义，
留在 primary。消息上下文默认 `strong`，只有显式
`consistency=eventual` 才可用副本。鉴权、安全状态、跨房查询、重连追平和
变更回放永远 strong。副本启动建连失败或 eventual 查询失败都会回 primary。

**已实现——Redis 热键**：房间 presence 与直播 viewer 分别按 participant
低位分成 256 个 sorted-set shard，写入散列、读取并发聚合并逐 shard 驱逐
过期成员。它们不再是单个房间/直播热键。通话 roster 是另一套状态，不应据此
宣称也完成相同分片。

**已预构建、未生产切换——消息分区**：`messages_partitioned` monthly shadow、
DEFAULT partition、增量幂等 backfill、未来分区维护函数及完整 cutover runbook
已经存在。生产交换涉及复合主键、消息索引和多张子表外键，必须在获批维护窗
停写、校验 parity、由 DBA 执行并保留回滚；禁止放进普通自动迁移链，也不得
描述为已完成 production cutover。

**锚点**：`aero-storage/query_router.rs`、`aero-server/routes/handlers/rooms.rs`、
`aero-server/message_context.rs`、`aero-storage/presence.rs`、
`aero-storage/live_presence.rs`、`docs/runbooks/messages-partitioning.md`。

---

## 方向五（P2）·企业合规纵深 + 实时协作原语——上探受监管与高端市场

**已实现——驻留与加密**：workspace 持久化 `region_code`，`RegionRouter`
只接受已配置 code，并在每个 blob reservation 上快照不可变
`workspace_id/storage_region`。房间上传从已鉴权房间解析 workspace，客户端
不能自报区域；旧 `/api/blobs` 仅保留个人/迁移读取兼容，未限定 workspace 的
对象不能进入新消息。S3 PUT 支持并签名 SSE-KMS headers，显式选择 S3 时配置
缺失或 header-unsafe KMS key 会启动失败。

**已实现——审计**：应用仓储仅暴露追加事件；active legal hold 会阻止行级
retention 与分区 DROP，审计 CSV 可用 `AERO_AUDIT_SIGNING_KEY` 输出
HMAC-SHA256 防篡改签名。常规保留期结束后的授权清理仍存在，因此这里的
“不可变”是追加接口、保全期不可清除与签名导出，不宣称外部 WORM 存储。

**已实现——Canvas**：服务端以 canvas-row lock 分配 gap-free `op_seq`，
`client_op_id` 在 `(canvas,author)` 范围幂等；同 key 不同 payload 返回冲突。
快照更新同时校验 `expected_version` 和 `snapshot_op_seq`，确保 blocks 的基线
与仍需重放的 tail 不重叠、不漏项。Web reducer 从快照基线有序拉取 ops，并
处理 gap、重试和实时 `canvas_op`。这是有序操作日志与确定性 reducer，不把它
夸写成通用 CRDT。

**剩余验收**：本机 MinIO 已通过，但真实外部 S3/KMS 供应商凭据与区域网络
往返仍必须在 staging 验证；受监管部署若要求 WORM、跨区搜索策略或客户自管
密钥，仍需对应基础设施与合规验收。锚点：
`aero-storage/region_blob_store.rs`、
`aero-storage/s3_blob_store.rs`、`aero-storage/audit.rs`、
`aero-storage/canvas_op.rs`、`aero-server/canvas.rs`、`web/canvas.js`。

---

## 跨方向的共识工程基线（动手前对齐 `AGENTS.md`）

- **迁移**：所有新表/列照既有配方，加后**先 `cargo build` 再 migrate**
  （编译期嵌入）。生产消息分区 cutover 只走获批维护窗，不进入迁移链。
- **鉴权**：billing/usage、canvas ops、audit export 和 workspace region 路由仍
  先走 `assert_room_access` / `effective_member_role`，并过 `authz_lint`。
- **可验证性**：每方向都用 db_test + 全新一次性库 E2E smoke；读副本故障
  回退、投递 barrier 与 Canvas 并发基线需要针对性回归。
- **媒体面**：SFU media session 与 call-bridge egress 已在生产 lifecycle
  接线。持久单调 call-leg generation 与 legacy caller reconnect 兼容围栏；
  PostgreSQL 与 Redis route/roster 的 generation CAS、WS/SFU 事件和精确清理
  共同围栏旧 socket、心跳与 leave。internal subscriber 使用 60 秒 lease、
  每 15 秒认证 refresh 与 best-effort unsubscribe；新请求携带
  `lease_secs` + `subscription_id` + `wire_version`，RTP 帧绑定 call 与订阅
  代际。真实旧 v3 / 当前 v4 二进制已完成双向媒体与 wire-version 降级验收；
  无绑定 v2 只支持新 owner 服务旧 puller，发布链仍含 v2 时必须先
  drain/重连。当前 generation-fenced 构建已通过真实 Chrome 单机双 gateway
  late-subscriber、同参与者跨 gateway 重连与旧连接延迟清理，真实 Firefox
  双向媒体与本机 coturn 强制 relay-only 也已通过。Chrome WHEP、OBS RTMP 和
  ffmpeg 加密 SRT 周期 SEK 轮换均有真实运行证据。跨主机公网 UDP/NAT/防火墙、
  物理设备与 Safari 仍是 staging 验收。

> 第六版的实现方向已经落地到不同验收阶段。后续路线图应围绕量化容量、
> 真实环境验收和运维授权展开，而不是继续沿用“投递无游标、全读主库、
> presence 单热键、无驻留/KMS/Canvas op log”等已经失效的缺口描述。
