# Aero IM — 扩展路线图（ROADMAP · 第六版）

> **资深架构师 / 产品经理视角。** 基于一次性战略全局代码扫描（16 crates / 150+ 迁移 / web SPA），5 个架构维度并行映射「现状 → 缺口 → 高价值扩展」，每条结论附 `file:module` 证据，可逐条核验。
> 本文只做规划与论证，**不含任何代码**。工程约定见 [`AGENTS.md`](../AGENTS.md)，功能矩阵见 [`README.md`](../README.md)。

---

## 缘起：广度与正确性已挖尽，本版攻「平台纵深」

前五版攻的是**功能广度**与**已交付能力的收口**（投递一致性、计量准入、零停机部署、媒体面通电、GDPR 正确性…），**均已逐一交付并核验退休**。最近一轮工作进一步做了 **9 轮对抗式收敛扫描**（安全 / DoS / GDPR / IDOR / 并发 / SQL 正确性 / 状态机 / 缓存 / 异步安全…共 ~22 类，末轮干涸 0 确认），把应用层的 bug 与正确性缺口挖到收敛。

**到此，「再加一个功能」或「再修一个 bug」已是边际递减。** 真正未被任何一版覆盖、且决定这个平台能否从「功能丰富的 demo」走向「可规模化运营的产品」的，是另一类更深的实体——**平台纵深**：当**真实流量、AI 成本、滚动运维、合规审计**同时压上来时，架构在哪一层先暴露根性短板。本版 5 个方向即针对这四类压力，全部在本次扫描中 grep + 读码实测**现状成立、缺口真实**，并给出**建立在既有架构之上**的扩展（非推倒重来）。

---

## 总览与优先级

| 优先级 | 方向 | 一句话缺口 | 守住的支柱 | 体量 | 状态 |
|---|---|---|---|---|---|
| **P0** | 一、AI 成本治理与检索质量 | 每个琐碎问答都按 Sonnet 满价计费，无分层路由 / 无语义缓存 / 无按租户计费台账 | 商业模型（AI-native 是差异化也是最大成本中心） | L | **✅ 本会话交付**（用量台账 + 模型分层路由 + 房间级答案缓存，全 opt-in 可验证） |
| **P0** | 二、端到端分布式追踪 + 每租户 SLO/成本归因 | ~~追踪在每个 NATS 进程边界被切断~~ | 可运维性 | L | **✅ 实为已实现**（核实更正：trace 经 envelope `traceparent` stamp/extract 全链贯通——入站 HTTP→NATS 生产→消费 set_parent→出站 webhook + JSON 日志 + 每租户成本归因经方向一用量台账；扫描误判为缺口。剩仅 SLO/告警规则/看板=ops 配置非代码） |
| **P1** | 三、每用户持久投递台账 + 紧凑离线同步 | 重连只能按房重放全量历史，多设备各刷一遍，无 per-user 投递游标 | 核心 UX（IM 的根本承诺：消息必达 + 离线无缝追平） | L | 待做（核实：确无 delivery_cursor） |
| **P1** | 四、读副本路由 + 消息表自动分区 + Redis 热键分片 | 万人房在「水平脊柱」生效前先撞 PG 连接池 + Redis 单热键 | 规模就绪（第一道扩展悬崖在 5–20k 并发） | L | 待做（核实：replica 无；分区已起 shadow `0148` 未 cutover） |
| **P2** | 五、企业合规纵深 + 实时协作原语 | 无数据驻留 / 无静态加密 KMS / 审计可变 / canvas 是后写覆盖无 CRDT | 上探高端（解锁受监管 + 企业合同） | L | 待做（核实：region_code/KMS/canvas-CRDT 确无） |

> **核实更正（动手前必做)**：ROADMAP 的现状映射来自一次性扫描,**会高估缺口**——方向二的 trace 续传 + 方向二里点到的 JSON 结构化日志**均已实现**(本会话核实)。教训:执行任一方向前先 grep 核实「真缺」,别重建已有功能(false-completion)。方向一/三/四/五经核实为真缺,方向二已退休。

**排期逻辑**：P0 两条是「今天就在漏血 / 盲飞」——成本未分层即每月在补贴琐碎问答，追踪断裂即故障晚 30 分钟才告警；先做。P1 两条是「一长大就断」——核心投递 UX 与第一道扩展悬崖；紧随。P2 是「上探收入天花板」——合规与协作纵深解锁高端市场，依赖 P0 的成本台账与 P1 的可靠性底座，最后做。

---

## 方向一（P0）·AI 成本治理与检索质量——把「会答题」升级为「答得准、付得起、算得清」

**现状（已建得相当扎实）**：双路检索（Voyage 非对称 query/doc 嵌入 + FTS）经 **RRF 融合**（`aero-ai/rerank.rs`）+ 房/工作区边界 + 可选 agentic tool-use 环；成本治理有**全局 `CostBudget` + 每租户 `KeyedCostBudget`**、按 kind 加权计费（Embed1/Mod2/Sum3/Ans5，`aero-ai/budget.rs`）、真实 token 计费（`token_micros`，`aero-ai/metrics.rs`）、prompt caching（system+tool 前缀）。

**缺口**：① **无按难度的模型分层路由**——琐碎查找与复杂综合都打同一个 `ANTHROPIC_MODEL`（默认 Sonnet 4.6），无难度估计、无 haiku/opus 选择。② **无语义答案缓存**——近似重复问题每次重跑全量 RAG + completion。③ **只有按租户成本 _指标_，无按租户 _计费台账_**——无发票 ledger、无信用额度消耗、无用量阈值/告警，无法做定价档位或自助成本可见性。④ **检索质量无评测回路**——无 NDCG/MRR 追踪、无嵌入消融调参，无法证明也无法改进「答得准」。

**扩展提案**：(1) **难度估计 + 模型分层路由**——按 query token 数 / 领域关键词 / 检索命中质量分 easy/medium/hard 桶，easy→haiku、hard→opus；典型部署 60–70% 是 easy，预计答案生成成本降 40–50%。(2) **每租户用量台账 + 计费 API**——`(tenant_id, job_kind, real_cost_micros, ts, job_id)` ledger，job 完成即记账，暴露 `/billing/usage`（按 kind 拆分 / 时序 / 信用余额 / 阈值告警）。(3) **语义答案缓存**——归一化 query → 嵌入聚类 → Redis 查 `similarity>0.95` 的 `(answer, citations)`，命中直返、未命中跑 RAG 再缓存（每工作区 TTL 24h），典型 20–30% 命中。

**为何高价值**：AI 是产品的**陈述性差异化**（「AI-native IM」）也是**最大单用户成本中心**。无分层路由，每个琐碎查找都付 Sonnet 满价，**直接侵蚀毛利**；无用量台账，SaaS **无法公平计费、无法限额、无法向客户展示其消费**——既失信任又丢 upsell 空间；语义缓存是 LLM 产品的 table-stakes。

**关键边界情况**：缓存的**多租户隔离**（A 租户的答案绝不命中给 B——缓存 key 必含 workspace + 成员边界）；**陈旧失效**（被引用消息编辑/删除后缓存须失效，挂到现有 `Edited`/`Deleted` 事件）；分层路由的**降级**（opus 限额耗尽 → 退 sonnet，不是拒答）；台账与既有预算窗口的**口径一致**（ledger 记真实 token，预算用加权估计——两者别打架）。

**体量 L**·切入锚点：`aero-ai/service/service_impl.rs`（answer 路径）、`rerank.rs`、`budget.rs`、`metrics.rs::charge_cost`、`anthropic.rs`（模型选择）。

---

## 方向二（P0）·端到端分布式追踪 + 每租户 SLO/成本归因——把「有指标」升级为「可规模化运营」

**现状**：有 Prometheus 指标（well-known 常量在 `aero-common/metrics.rs`：`MESSAGES_SENT_TOTAL`/`AI_COST_MICROS_TOTAL`/`NATS_CONSUMER_PENDING_MESSAGES`/`DB_POOL_IN_USE`…）、OTLP trace、`/health{,/live,/ready}`、三个 observability gauge sampler、HTTP RED 中间件 + 可选 per-tenant workspace label（`AERO_PER_TENANT_METRICS`），AI 成本可按 workspace 归因（`charge_cost(workspace)`）。

**缺口**：① **追踪在进程边界断裂**——`EventBus::publish`（`aero-bus/traits.rs`）签名无 `headers` 参数，trace 上下文只能塞进 payload 字段而非 OTEL 标准 header；出站调用（webhook/AI provider/S3/跨节点 call-bridge）从不注入 `traceparent`；HTTP 层不提取入站 `traceparent`。**一个卡死的 NATS consumer（「看起来健康其实已死」的黑洞）在跨进程边界没有任何 trace 连续性信号**——日志散落，告警要等 backlog 阈值跳了（故障开始 30 分钟后）才响。② **日志是 stdout 纯文本**，无 JSON / 无 trace_id 关联。③ **有指标但无 SLO / 无告警规则 / 无仪表盘**——只是原始信号。④ **无备份/恢复 + DR 叙事**。

**扩展提案**：分两期。**A 期（追踪脊柱）**——`EventBus::publish` 加可选 `headers`，生产端把当前 span 注入 NATS header（W3C `traceparent`）、消费端从 header 提取 `set_parent`（替代现 payload 字段法）；HTTP TraceLayer 提取入站 + 向出站 webhook/AI/S3 注入；日志切 `.json().with_current_span(true)`（`AERO_LOG_FORMAT=json`，`request_id`/`trace_id` 扁平进每行）。**B 期（SLO 层）**——定义核心 SLO（消息扇出 p99、AI 答案延迟、readiness）+ 告警规则（consumer pending 持续 >N、错误率、池饱和）+ 每租户成本/用量仪表盘（复用 per-tenant label）。

**为何高价值**：平台「有指标 + 有告警规则」看起来运维成熟，但**三道地基缝在规模下崩**：静默故障跨进程边界无连续信号（黑洞晚 30 分钟才告警）、租户级问题无法定位到具体 workspace、成本归因不可审计则无法做容量规划与公平限额。**多租户 SaaS 的本质是「可运营」，不是「有功能」**；A 期的追踪连续性是把一切串起来的脊柱，B 期把它变成主动的 SLO/告警而非事后挖日志。

**关键边界情况**：`EventBus` trait 改签名要**向后兼容**（`headers: Option<…>`，既有调用方传 None）；header 注入**不能拖慢热路径**（fan-out 每事件一次，须廉价）；JSON 日志**不泄密**（承密字段已 `<redacted>` Debug，别在 span 里漏）；trace 采样率（`AERO_TRACE_SAMPLE_RATE`）在高吞吐下要可控，避免追踪自身成为成本。

**体量 L**·切入锚点：`aero-bus/traits.rs`（publish 签名）、`ws/ws_impl/bus.rs`（消费端 set_parent）、`aero-common/telemetry.rs`、`aero-server/metrics.rs::http_metrics_layer`、各 sampler。

---

## 方向三（P1）·每用户持久投递台账 + 紧凑离线同步——兑现 IM「消息必达 + 离线无缝追平」的根本承诺

**现状（脊柱已对）**：发布期 per-subject 单调 seq（`aero-bus/seq.rs`，本地原子 or Redis INCR）+ 客户端按 seq 去重；durable consumer + poison 边界（max_deliver=16/ack_wait=120s，`jetstream.rs`）+ ack-on-success/decode-fail；Hub 有界 mpsc + 慢消费者背压（drop+resync 标记 或 断连，`hub.rs`）；重连 `?since=message_id` 按房 `backfill_since`（cap 200）；per-message 读回执表。

**缺口**：**无 per-user 持久投递台账 / inbox**。今天重连完全靠 PG 按房重放历史（`list_since` keyset 扫描），**无 per-participant 投递游标**记录「谁 ack 了哪条」。后果：① 长离线窗（数天）须按房全量重放，无「已投递 vs 待投递」紧凑追踪；② 弱网频繁断连触发 backfill 有界 channel 溢出→每房每 gap 退化为 REST 兜底（分页循环，碎片化 UX）；③ **多设备各刷一遍**——一个用户三台手机各自独立重放同一 1000 条 backfill，在关键时刻（系统故障后全员首次启动）**三倍 DB 负载**；④ backfill 本身无投递确认，drop-only 模式回来一个 resync 标记但客户端无法区分「哪些被丢 vs 哪些是新 live」。

**扩展提案**：**每用户持久投递台账 + 紧凑增量同步**——`delivery_cursor(participant_id, room_id, last_acked_message_id, last_seq, last_cursor_at)` 唯一索引 `(participant, room)`。重连带 `?since=&seq=`，服务端在**索引化 seq 列上二分**只返 seq 之后的（O(log N) vs 现 O(N) 扫描，cap 200）；维护 per-participant Last-Known-Good（LKG）游标；backfill 发紧凑 `backfill_delta` 帧（含 gap 内总数），客户端 ack 后原子推进 LKG。**多设备共享同一 per-room LKG 游标**——第二台设备重连只见首台 ack 后的真正新消息，消灭 per-device 重放。unacked 超阈（1000+）发 `backpressure_for_rooms` 帧让客户端卸低优订阅。**法务保全/留存清扫时清 LKG**，保证被保全消息绝不被跳过。

**为何高价值**：IM 与创作者平台的核心承诺是**高移动性下的无缝恢复**（蜂窝切换/wifi 抖动/App 后台）与**长离线追平**（离线一天的创作者回来面对跨房 50k+ 消息）。今天系统强制 per-房 per-设备 per-重连 全量重放，**成本随 房×设备×重连 乘积膨胀**；故障后全员同时重连即 DB I/O 尖峰。持久台账把 O(N) 塌缩为 O(log N)、消灭多设备重复、给创作者「消息确达」的确定性，并为**离线优先客户端**（桌面 Tauri 预取后再渲染）与**E2E 加密消息的读写同步**（免二次解密）铺路。

**关键边界情况**：游标推进的**幂等**（at-least-once redelivery 同 seq，LKG 只进不退）；**多设备竞态**（两设备并发 ack，以 max(seq) 收敛，advisory lock 或原子 `WHERE last_seq < $1`）；法务保全/留存与游标的**交互**（清扫软删消息时游标不能让客户端跳过 held 消息）；seq **回绕/重置**（per-room 计数器迁移期与既有 per-subject seq 的兼容）。

**体量 L**·切入锚点：`aero-bus/seq.rs`、`ws/ws_impl/bus.rs`+`mod.rs`（backfill）、`hub.rs`（背压）、`storage/message/query.rs`（`changes_since`/`list_since`）、新增 `delivery_cursor` 迁移 + repo。

---

## 方向四（P1）·读副本路由 + 消息表自动分区 + Redis 热键分片——跨越第一道扩展悬崖

**现状**：水平脊柱已对（NATS durable consumer + Redis 集群态 + 多开实例扩消息吞吐）。但**应用层数据访问全打单 PG 主库**：默认 `max_connections=16`、`POOL_ACQUIRE_TIMEOUT`（`storage/db.rs`、`common/config.rs`）；presence/viewer/roster 是 per-room 单 sorted-set 热键（`storage/presence.rs`、`live_presence.rs`）；消息表单体（有 messages-partition runbook 但**未自动应用**，设计在 `runbooks/` 标了 7 个入站 FK 的 hard-STOP）。

**缺口**：**第一道扩展悬崖在「水平脊柱机制」生效前先撞**：万人活跃房 = 100 msg/s × 10k 成员 → 每事件 `room.members()` 查 + 回执查竞争 16 个连接槽（饱和时每连接 6+ 查/s）→ **消息延迟劣化 + 客户端超时级联，早于任何 NATS consumer rebalance / Hub 背压触发**；同时 presence 单热键在**同步用户涌入**下 ZADD 串行化。两者都表现为 5–20k 并发处突发请求延迟尖峰 + 连接超时。

**扩展提案**：(1) **多租户读副本路由**——`AppConfig` 加可选 `database.replica_url`，薄 `QueryRouter`（包 PgPool）把只读查询（`room.members()`/消息查/回执/反应）路由副本，写留主库，读独立扩。(2) **消息表自动分区**——按 `created_at` RANGE 月桶，**部署期自动应用**（非人工维护窗）：影子 `messages_partitioned` + 0148 式 backfill 函数 + `insert()` 期双写转发（7 天 ramp，flag 控）→ 追平后约束交换原子切换（安全处理 runbook 的 7 个 FK 重写）。(3) **Redis 热键分片**——presence/viewers 从 `presence:room:{id}` 改 `presence:room:{id}:shard:{uid%256}`，化解单热键 ZADD 串行化；读时聚合 256 分片。

**为何高价值**：大规模 IM（Slack/Discord/Teams）的生产部署都撞两堵可预测的墙：(A) **单热表（messages）的读写争用**早于应用逻辑扩展；(B) **同步用户涌入下 Redis 热键串行化**。两者都在 ~5–20k 并发处表现为请求延迟突刺 + 连接超时，然后短暂宽限期后雪崩。读副本 + 自动分区把读容量与表大小解耦，热键分片消除涌入串行化——**把「号称能水平扩」变成「实测能扛第一道悬崖」**。

**关键边界情况**：读副本**复制延迟**（刚写的消息从副本读不到→读己写一致性：写后短窗读主库，或关键路径强制主库）；分区**切换的零停机**（双写期一致性 + 原子 cutover + 回滚预案，绝不在另一次重构中途叠加，见 `AGENTS.md §4.2`）；热键分片的**计数一致性**（256 分片聚合 viewer 数，过期驱逐跨分片）；连接池**按主/副本分别配额**。

**体量 L**·切入锚点：`common/config.rs`（`database.*`）、`storage/db.rs`（pool/QueryRouter）、`storage/presence.rs`+`live_presence.rs`（分片）、`migrations/`（分区，参 `runbooks/messages-partitioning`）。

---

## 方向五（P2）·企业合规纵深 + 实时协作原语——上探受监管与高端市场

**现状**：GDPR 抹除扎实（tombstone + 显式删全部 participant-keyed PII 表）；`audit_events`（含分区 `ensure_partitions` + CSV 导出 `events_to_csv`）、`legal_holds`（`is_held` + sweep FK 排除）、`info_barriers`、工作区导出/删除（`workspace/export.rs`）均在；canvas 协作（`canvas.rs`）。

**缺口**：三道挡住上探的能力：① **无数据驻留**——EU/APAC 客户无法把数据钉到区域，S3 region 是全局部署配置，从不 per-workspace。② **审计可变 + 无静态加密**——`audit_events` 会被 sweep 老化（可变），blob **零静态加密**（无 KMS、无密封信封），legal hold 让数据活过删除但**审计轨迹本身没有同等不可变性**。③ **协作纵深不足**——canvas 是**后写覆盖**（`PUT` 读-合并-写，竞态丢失，`canvas.rs`），无 OT/CRDT，并发编辑静默互相覆盖；无在线感知（谁在编辑、活动光标、空闲超时）。

**扩展提案**：(1) **数据驻留**——`workspace` 加 `region_code`(eu/us/apac/custom)，在 `blob_store_from_env` 选后端时按 workspace region 路由到区域桶/自定义端点；扩 `legal_holds` 豁免 `audit_events` 出 sweep，加**不可变审计导出**（签名、防篡改 CSV）。(2) **静态加密**——blob-store 包**信封加密层**（per-workspace DEK，DEK 存 KMS，所有 blob I/O seal/unseal）+ 轮换策略。(3) **canvas CRDT**——`PUT` 全量替换改为**操作日志**：`POST /api/canvases/:cid/ops` 追加增量编辑（insert/delete/format），客户端本地 OT/CRDT 解冲突，服务端存不可变 op log；加在线感知 `/api/canvases/:cid/presence`（WS 广播活动光标/编辑者/空闲超时）；**补 web 端 canvas 编辑器 UI**（实时同步、在线头像、撤销/重做）。

**为何高价值**：企业销售撞监管墙：FedRAMP/SOC2/ISO27001 买家要审计不可变 + 加密 + 驻留证明；GDPR 执法（2024+）处罚跨境默认。Slack/Teams/Discord 用合规变现 5 万+ 高端合同。协作 UX 缺口（无 CRDT）逼重度用户回 Notion/Figma 做异步文档，aero-im 沦为「纯消息」。**驻留 + 审计不可变 + KMS 加密是受监管行业的入场券，CRDT canvas 是现代工作区的对等项**——三者共同把产品从「消息工具」抬到「企业工作区平台」。

**关键边界情况**：驻留与**跨区功能**（跨区 workspace 的搜索/AI 检索如何不违反驻留——检索须区域内）；KMS 的**密钥轮换与历史 blob**（轮换后旧 blob 用旧 DEK 解，信封记 key 版本）；审计不可变与**GDPR 抹除的张力**（被抹除用户的审计行——保留治理记录但脱敏 PII，与现 erasure 策略对齐）；CRDT 的**离线编辑合并**（长离线后 op log 分叉收敛）与**op log 增长**（快照压缩）。

**体量 L**·切入锚点：`storage/audit.rs`、`storage/legal_hold.rs`、`storage/blob_store`/`s3_blob_store.rs`（信封层）、`server/canvas.rs`（op log）、`web/`（canvas 编辑器）、`workspace` 表（`region_code`）。

---

## 跨方向的共识工程基线（动手前对齐 `AGENTS.md`）

- **迁移**：所有新表/列照既有配方，加后**先 `cargo build` 再 migrate**（编译期嵌入）；分区/双写**绝不在另一次重构中途叠加**。
- **鉴权**：任何新路由（billing/usage、canvas ops、audit export）仍**先 `assert_room_access` / `member_role`**，过 `authz_lint`；计费/导出端点尤其要防 IDOR。
- **可验证性**：每方向都应有 db_test + 一次性库 E2E smoke（本会话的验证范式）；分区/副本切换要 `migrate-smoke` 在 throwaway 库 replay 全链。
- **媒体面 seam 不在本版**：SFU live socket / call-bridge egress 仍是「待真实 WebRTC 对端」的 staging seam（见 `AGENTS.md §4.5`），非本版方向——别误当扩展点重造。

> **本版与前五版的关系**：前五版把**广度与正确性**挖尽（功能矩阵 + 9 轮收敛的 bug/正确性收敛）。第六版回答下一个根问题——**当真实流量、AI 成本、滚动运维与合规审计同时压上来，平台在架构层先暴露什么短板**：答案是成本未分层（漏血）、追踪断裂（盲飞）、投递无 per-user 台账（一长大就断）、数据访问全打主库（第一道悬崖）、合规与协作未纵深化（够不着高端）。五者皆建立在既有脊柱之上，非推倒重来。
