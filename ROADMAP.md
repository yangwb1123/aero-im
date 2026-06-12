# Aero IM — 扩展路线图（ROADMAP · 第五版）

> **资深架构师 / 产品经理视角。** 基于 2026-06-13 一次性全局代码扫描（16 crates / 125 迁移 / web SPA）。
> 本文只做规划与论证，**不含任何代码**。工程约定见 [`AGENTS.md`](AGENTS.md)，功能矩阵见 [`README.md`](README.md)。
> 每条结论附代码证据（`file:line`），可逐条核验。

---

## 缘起：第四版五方向本次会话已全部交付，逐一退休（retired），本版攻「平台级根性」

第四版攻的是「已交付能力的收口」——计量做了没准入、前向投递通了变更回放没通、加固做了一批零停机部署仍断、媒体面逻辑全建好却从未通电。**这五个方向已在本次会话内逐一落地并 grep + 读码核验，全部退休（migrations 已推进至 0125）。** 第四版到此，**应用层 / 收口层与 v1–v4 的整条 scale / correctness / cost / media-plane 轴线已被挖尽**。本版不再收口某个已交付功能的残影，而是攻一类全新的、更深的实体：**平台级根性缺陷**——把「看起来健康、其实已死」的自愈断点、一条切断在每个进程边界上的追踪脊柱、一个只会答题不会动手且看不见附件的 AI、一堆永不分区永不归档的单体堆表（外加两个 GDPR 抹除正确性 bug），以及一层尚未纵深化的滥用防御。

**第四版五方向均已交付，逐一退休（retired）：**

- **第四版方向三（零停机部署与多租户公平性加固）已交付**：就绪排空 `AtomicBool` 在 `cancel()` 前置位、`TaskTracker` 有界排空全部长驻任务、限流器空闲淘汰 `retain()`、`/health`+`/metrics` 探针旁路、blob 就绪探针、S3 有界退避重试、承密结构体 `<redacted>` Debug —— 七项全建。**退休。** 仅余 webhook 重试循环 / push_bot `run()` 不收 `CancellationToken`、靠排空超时兜底的装饰性残尾（sub-S），不值新方向。
- **第四版方向一（投递一致性闭环）已交付**：`changes_since` 变更回放端点 + web 重连 `replayChanges`、sweep 发 `RoomEvent::Deleted`（ephemeral + retention）、hub 按 socket 粒度只在最后一条 socket 关闭时摘路由、客户端 tombstone-aware 复活守卫 + 流重订阅 —— 四项全建。**退休。**
- **第四版方向二（广播提醒扇出数据面与索引收口）已交付**：批量 workspace-mute `muted_participants` + 复合索引 0123、多接收人 `RoomEvent::NotifyBatch` 单次 publish、thread-level 批量 `levels_for` + 索引 0124、查询期 `set_config('hnsw.ef_search', …)` + 索引 0125 —— 全建。**退休。**
- **第四版方向四（AI 成本治理从计数到真实美元）已交付**：**加权**预算准入 `weight_for(kind)` + all-or-nothing `try_acquire_n` + DEFER 不丢、真实 moderation 计费 `complete_with_usage` + `moderate_micros` 默认值、voyage 非对称 `embed_query('query')` vs 语料 `'document'` —— 三项全建。**退休。**
- **第四版方向五（媒体面真实跨节点化与多编解码正确性）已交付其可构建脊柱**：`SfuMediaSession` UDP 回路驱动 `on_rtp`、跨节点 `NodeRtpPullerFactory` 绑 UDP + `subscribe_to_peer`、AV1 N-bit + H.265 IRAP 关键帧检测、SRT `ReorderBuffer` —— 全建。**退休。** 唯一携往后续的是一条已记录的 **infra seam**（编码后的 REMB/PLI 背压已算出 + 入队 + 单测，但尚未经真实 DTLS-SRTP 写到发布者出站 RTCP，`forward.rs:46-51` 明示需真实浏览器 / 第二节点 ICE/DTLS/SRTP 握手），作为 staging 验证门，**非新方向**。

> **本版与第四版的关系**：第四版回答了「已经交付的能力，哪一半没做完、哪一处被重新引入」并系统性收口。第五版回答一个更根的问题：**当真实流量、滚动发布、合规审计与对抗性滥用同时压上来时，这个平台在架构层面最先暴露的根性缺陷是什么？** 下面 5 个方向均为本次扫描中 grep + 读码**实测仍存在**、且**前四版从未覆盖**的缺口——其中两条 P0 含的不只是功能空白，而是扫描全程发现的**最严重的真实 DEFECT**（自愈断点导致的静默全节点黑洞、礼物路径重复扣费、追踪在每个进程边界被切断、GDPR 抹除残留可再识别 PII），已逐一核对，不重复任何已完成项。

---

## 总览与优先级

三大产品支柱：**协作可信赖**、**互动直播是差异化**、**AI-native 可持续**，外加 **To-B 信任**的运维 / 合规底座。

| 优先级 | 方向 | 解决什么 | 支柱 | 体量 |
|---|---|---|---|---|
| **P0** | 一、自愈式韧性与金钱路径正确性 | 监督式 bus 监听重订阅 + 礼物幂等键 + 毒消息 DLQ（+ AI 优先级 / webhook 熔断尾） | To-B 信任 | M |
| **P0** | 二、端到端分布式追踪与 SLO 误差预算层 | 跨 bus 注入 / 提取 traceparent + 日志↔追踪↔header 关联 + 多窗多燃尽率告警 + 每租户 RED | To-B 信任 | L |
| **P1** | 三、从问答助手到能动智能体与可检索知识库 | Anthropic tool-use 循环 + 文件/附件 RAG 抽取 + 语言感知 FTS + 存查告警 | AI / 协作 | XL |
| **P1** | 四、数据生命周期：分区/归档与 GDPR 抹除正确性收口 | embedding 抹除 + legal-hold 守卫（先发）+ `created_at` 范围分区 + 未清扫表留存 + 采样降采样 | To-B 信任 / 协作 | XL |
| **P2** | 五、认证与滥用防御纵深 | 登录失败计数 / 锁定 + 上传魔数嗅探 + refresh 重用族撤销 + 行为刷屏检测 + JWT `kid` 轮换 | To-B 信任 | L |

> 建议落地顺序——**按严重度交错，而非严格按方向号**，因为最高价值的「活」分散在各方向：
> **Phase 0（数日即可合入）**：把方向四里两个 GDPR 抹除正确性 bug 拎出来先发——`delete_participant` 补 NULL embedding 列（S，无迁移）、抹除 `UPDATE` 加 legal-hold `NOT EXISTS` 守卫（M）；同步发方向一的礼物幂等键（M，用户明确点名的金钱路径计费正确性 DEFECT）。三者皆 tiny、今日可在沙箱构建、虽住在 P1 方向却带 P0 紧迫度。
> **Phase 1（方向一 P0）**：建监督式「重连退避重订阅」原语并铺到全部 9 个 bus 监听器，外加 WS 扇出消费者的毒消息 `max_deliver`/DLQ。一个原语修好 9 个监听器，是本版最高杠杆的 M。AI 优先级与 webhook 熔断作为 P2 子项后置同方向内。
> **Phase 2（方向二 P0）**：装 OTel `TextMapPropagator`、给 `EventBus::publish` 加 headers 参（小 trait 改、大解锁）、HTTP 出网 + NATS bus 注入/提取 traceparent、把 `request_id`/`trace_id`/`span_id` 录进 span 且 fmt 层切 JSON-with-current-span；**然后**叠 SLO recording rules + 多窗多燃尽率告警 + 每租户 RED 标签 + 新 `monitoring/` 目录里 Prometheus/Grafana/Alertmanager 配置。**先码后 artifact**——追踪脊柱是地基。**Phase 1 与 2 可并行**（独立 crate：aero-server bus 监督 vs aero-bus/aero-common 遥测），二者合起来完成「加固→自愈」与「指标→追踪/SLO」两个命名的层级跃迁。
> **Phase 3（方向四结构件 P1）**：抹除正确性 Phase 0 已修，做 `created_at` 范围分区 + 四张未清扫高频表的留存 sweep + `stream_viewer_samples` 分层 rollup。XL，且受益于 Phase 2 的追踪/SLO 已在位以观测堆/索引膨胀与分区 DROP 行为。读副本拆分是更低优子项 / staging 门。
> **Phase 4（方向三 P1）**：XL 前沿投入。建 Anthropic tool-use 循环 + 规划派发步、文档文本抽取管线（同时喂 agent 检索工具与 FTS）、语言感知（stemmer + unaccent）FTS 迁移、存查定时重跑告警。agentic 质量对真实 AI key 做 staging 门。排在数据地基（Phase 3）之后，因文件内容 RAG 受益于已分区/已索引存储。
> **Phase 5（方向五 P2）**：账户锁定 + 登录 IP/设备历史 schema + 异常钩子、上传魔数/内容类型嗅探（AV 守护进程作部署 seam）、refresh 重用族撤销、行为刷屏评分、JWT `kid`+keyring 轮换。在韧性/可观测/数据地基稳固后的自然续作。

---

## 方向一（P0）：自愈式韧性与金钱路径正确性 —— 把「看起来健康、其实已死」修成真正自愈

### 为什么需要它（本版唯一同时含两类最严重 DEFECT、且单一原语高杠杆的方向）

这是最高价值的方向，因为它装的不是功能空白，而是扫描全程**最严重的两类真实 DEFECT**。**（1）bus 监听器黑洞是一个潜伏的全节点宕机 bug**：全部 9 个长驻消费任务（已核验：golive / webhooks / moderation / ooo / transcribe / unfurl / ws / push / agent_bot 全部携带 `while let Some(sub) = stream.next().await` 模式）会在**任何** NATS 重连 / 滚动升级 / 消费者过期事件后**永久死亡**——此后节点仍在 `/health/ready` 返回 200，却对它连着的客户端**静默地零投递**房间事件、通知、礼物、webhook、审核与推送，直到手动重启。前几版加固了**启动**与**关停**，但从未覆盖**运行期依赖丢失**——这正是「看起来健康、实则已死」的典型失败模式，把平台从「已加固」推到真正的「自愈」（即本版命名的层级跃迁）。修法是一个监督式「重连退避重订阅」包装器，重建消费者消息流（TCP 客户端会自动重连，但**消费者流从不被重建**）。**（2）礼物 / 付费动作路径**每次调用都在服务端 mint 全新 `Ulid` 配裸 `INSERT`、**不接受任何 Idempotency-Key**，于是手机重试或双击会**对用户重复扣费**（重复 `stream_gifts` 行、双倍 goal 进度、双倍 hype-train）——用户明确点名的金钱路径计费正确性 DEFECT；存储层幂等已存在于 join_request / receipts / reactions，金钱路径成了显眼的遗漏。**（3）WS 扇出消费者**把毒消息 `Nak` 进一个无退避的紧致无限重投热旋（默认 pull `Config`，无 `max_deliver`/`ack_wait`/backoff），饿死单个扇出消费者、在引入新 `RoomEvent` 变体的滚动发布期阻塞**全部**投递——一个自伤式宕机放大器；`ai_jobs` 已有 `MAX_ATTEMPTS` 死信，可复刻。高架构杠杆：**单一监督式订阅原语一次修好 9 个监听器的黑洞 + 毒消息 DLQ**。

### 代码证据（已读）

- **bus / bot 监听器在任何 NATS 流中断后永久死亡，无重订阅、无 supervisor 重启（静默扇出黑洞）**：`crates/aero-server/src/ws.rs:969` `while let Some(sub) = stream.next().await` —— JetStream `messages()` 流把任一 per-message 错误映为 `None`（`crates/aero-bus/src/jetstream.rs:236` `Err(err) => { tracing::warn!(?err, "nats message error"); None }`，终止 `filter_map`），NATS 一掉线 / 滚动升级 / 消费者过期，`stream.next()` 即返回 None，`while let` 退出，`run_bus_listener` 返回，spawn 出来的任务只 `tracing::error!` 一行就**死**（`crates/aero-server/src/bin/aero-server.rs:791` `if let Err(e) = ws::run_bus_listener(state_clone).await { tracing::error!(error = ?e, "bus listener exited"); }`——记日志、永不重生）。grep `spawn_supervised`/`with_restart`/`respawn`/`resubscribe` **零结果**；`while let Some(sub) = stream.next` 跨 `crates/aero-server/src/` 命中 9 文件。
- **礼物 / 付费动作路径无客户端幂等令牌，网络重试或双击即重复扣费（重复账本行）**：`crates/aero-storage/src/live.rs:125` `let id = Ulid::new(); … INSERT INTO stream_gifts (id, …) VALUES (…)`——每次调用 mint 全新 id、裸 INSERT、无 client token、无 `ON CONFLICT`；HTTP 礼物路由 `crates/aero-server/src/routes.rs:1742` `.send_gift(auth.participant_id, id, &req.gift_id, qty)` 与 WS 路径 `crates/aero-server/src/ws.rs:794` `state.live.send_gift(pid, stream_id, &gift_id, qty).await?` **均只携 gift_id+qty、无 Idempotency-Key**。`gift_subscription` 同样暴露（每调用一个新 `SubscriptionId`、dedup 仅在 `(creator,subscriber)`）。grep `Idempotency-Key`/`client_token`/`idempotency` 在礼物路由 / handler **零结果**。
- **WS 扇出消费者的毒消息被 nack 进无退避无限重投紧旋**：`crates/aero-server/src/ws.rs:1047` 当 payload 同时通不过 typed `RoomEvent` 解码与 legacy `MessageEnvelope` 兜底时，`warn!(error = ?e, "bad envelope on bus"); let _ = sub.nack().await;`——立即 Nak、无延迟、无放弃；消费者以默认 pull `Config` 创建（`crates/aero-bus/src/jetstream.rs:214`，`consumer::pull::Config { durable_name, filter_subject, ..Default::default() }`）**无 `max_deliver`/`ack_wait`/backoff** → 一条真不可反序列化的消息（schema 漂移、截断、未升级节点解析不了的未来事件变体）被 Nak → 立即重投 → 再失败 → 再 Nak 的热旋，饿死单个扇出消费者、阻塞该节点全部合法投递。与 `ai_jobs` 的 `MAX_ATTEMPTS` 死信不同，此消费者**无毒消息停泊**。
- **AI 任务队列严格 FIFO、无优先级类**：`crates/aero-storage/src/ai_job.rs:138` `claim` 纯 `ORDER BY scheduled_at ASC … FOR UPDATE SKIP LOCKED`、`ai_jobs` 无 `priority` 列；后台扫描器每 5 分钟把 200 条 embed 回填 job 灌进同一 FIFO（`crates/aero-server/src/bin/aero-server.rs:873` `match msgs_bf.list_without_embedding(200).await`），排在用户可见、延迟敏感的 moderation/answer 之前——审核 gate 消息可见性，embed 回填纯尽力催补，LLM 慢时无优先级车道、无 load-shed。grep `priority` 在 ai_job 迁移 / worker **零结果**。
- **出向 webhook 无 per-endpoint 熔断器**：`crates/aero-server/src/webhooks.rs:347` 每个事件都对可能已死的端点 `deliveries.record_attempt(...)` 一次（无 breaker gate），`crates/aero-server/src/webhooks.rs:479` `run_webhook_retry_loop` 只做 per-delivery 退避、无 per-endpoint 连续失败熔断 / half-open 探测、不尊重 HTTP 429 `Retry-After` → 一个 down 一小时的端点被每个新事件重锤数千次、各烧一条 reqwest 连接 + DLQ 行。grep `circuit`/`breaker`/`consecutive`/`Retry-After` 跨 webhooks.rs + storage/webhook.rs **零结果**。

### 「完成」的样子

- 抽出一个**监督式订阅原语**：把每个 `subscribe` 循环包进「重连 → 指数退避 → 重建消费者消息流」的外层 supervisor，铺到全部 9 个 bus 监听器 —— 任一 NATS 流中断后自动重订阅而非永久死亡，节点真正自愈。
- 礼物 / 付费路由接受 `Idempotency-Key`（或 `client_token`），`insert_gift` 以该键 `ON CONFLICT DO NOTHING`/返回既有行 —— 重试 / 双击不再重复扣费、不再双计 goal / hype-train；`gift_subscription` 同治。
- WS 扇出消费者的 pull `Config` 设 `max_deliver`/`ack_wait`/backoff，毒消息超过投递次数后停泊到死信（复刻 `ai_jobs` 的 `MAX_ATTEMPTS` 模式）—— 一条坏消息不再阻塞全节点投递。
- **（P2 子项，同方向后置）** `ai_jobs` 加 `priority`/weight 列，`claim` 先排优先级再排 `scheduled_at`，过载时 shed/降级 embed 回填、不饿死用户可见 job；出向 webhook 加 per-endpoint 连续失败熔断 + half-open 探测 + `Retry-After` 尊重。

**收益/体量**：把「看起来健康、其实已死」修成真正自愈，并堵死金钱路径重复扣费。**体量 M**——监督式订阅原语是一个 M 但一次修好 9 个监听器，是本版最高杠杆项；礼物幂等键随 Phase 0 早发；毒消息 DLQ 是 S 尾；AI 优先级与 webhook 熔断作 P2 子项后置。完全开放且已核验：grep `spawn_supervised`/`with_restart`/`respawn`/`resubscribe` 零结果，9 个监听文件确认携带未监督的 `stream.next()` 循环，`insert_gift` 用 `Ulid::new()` 配裸 INSERT，jetstream pull `Config` 无 `max_deliver`/`ack_wait`。

---

## 方向二（P0）：端到端分布式追踪与 SLO 误差预算层 —— 把割裂的三段轨迹缝成一条、把原始信号变成可寻呼的 SLO

### 为什么需要它（命名的「指标→追踪/SLO」层级跃迁，且每块解锁下一块）

这是本版明确命名的「指标 → 追踪/SLO」层级跃迁，五条发现构成一根紧耦合的脊柱、每块解锁下一块。OTLP 导出、强制优先采样器、per-process `#[instrument]` span **已全部接好**——但 trace 上下文在**每个进程边界被切断**，于是一次逻辑操作（消息发送 → JetStream publish → AI-worker / WS-fanout 消费）产出**三段互不相关的轨迹**，分布式追踪的全部意义荡然无存。已核验：`set_text_map_propagator`/`traceparent`/`HeaderInjector`/`HeaderExtractor` 全树**零命中**。根使能者（OTel 全局 `TextMapPropagator`）**从不安装**，于是入站 HTTP `traceparent` 在网关被丢、出网（webhook、AI provider 调用、S3、跨节点 call-bridge）无一注入 `traceparent`。`EventBus::publish` trait 签名**物理上无法**携带 W3C `traceparent`（`traits.rs:40` 无 headers 参）——一个小而承重的 trait 改，点亮全系统跨 bus 追踪。另一侧，`request_id` 在 header 里回显却**从不录进 tracing span**，且 fmt 层非 JSON / 无 `with_current_span`，于是结构化日志**携带不到** `request_id`/`trace_id`/`span_id` 任何一个——运维凭 `x-request-id` 找到一个慢请求后**无法 pivot 到它的日志或轨迹**（日志↔追踪↔header 关联双向断）。封顶的是：**SLO 机制为零**——无 recording rules、无多窗多燃尽率告警、无误差预算 / 目标配置、无任何随仓发布的 Prometheus/Grafana/Alertmanager 配置（已核验：无 `deploy`/`ops`/`monitoring`/`grafana`/`prometheus` 目录），且唯一带 per-workspace 标签的指标是 AI 成本，per-tenant 看板无 RED 数据可切。高杠杆：propagator + bus-headers 改动是 OTLP 导出一直等待的地基，把原始 RED 信号变成可寻呼的 SLO。

### 代码证据（已读）

- **NATS bus 无 trace 上下文传播，每个异步跳起一段全新断裂轨迹**：`crates/aero-bus/src/traits.rs:40` `async fn publish(&self, subject: &str, payload: bytes::Bytes) -> BusResult<()>`——无 headers/metadata 参，**无法携带 traceparent**；`crates/aero-bus/src/jetstream.rs:176` 的 `publish` 只 `self.js.publish(subject.to_owned(), payload).await`、不注入当前 SpanContext；消费侧 `crates/aero-bus/src/jetstream.rs:230` 的 `filter_map` 把 msg 包成 `JsSubscription` 时**从不从 msg headers 提取父 SpanContext**。grep `traceparent`/`HeaderInjector`/`HeaderExtractor`/`set_parent`/`TraceContextExt` 跨 aero-bus 零命中。
- **OTel 全局 TextMapPropagator 从不安装，入站 traceparent 被丢、出网无一携带**：`crates/aero-common/src/telemetry.rs:17` `init(...)` 构造 tracer + sampler 却**从不** `global::set_text_map_propagator(TraceContextPropagator)`；`crates/aero-server/src/bin/aero-server.rs:1050` `.layer(TraceLayer::new_for_http())` 用默认 `make_span`——不提取 traceparent、不录 request_id、不链远端父；出向 webhook header `crates/aero-storage/src/webhook.rs:92` `pub headers: Vec<(String, String)>` 只带 signature+timestamp、出网不注 traceparent。grep `set_text_map_propagator`/`TraceContextPropagator`/`HeaderExtractor` 全树零命中。
- **request_id 在 header 回显却从不录进 tracing span，日志无追踪 / 请求关联**：`crates/aero-server/src/routes.rs:45` `inject_request_id` 把 id 插进 axum extension 并镜像到响应 header，却**从不** `tracing::Span::current().record("request_id", …)`；`crates/aero-common/src/telemetry.rs:21` `let fmt_layer = tracing_subscriber::fmt::layer().with_target(true).with_level(true);`——**无 `.json()`、无 `with_current_span()`**，日志省掉 request_id/trace_id/span_id 等 span 字段。grep `request_id` 与 `span`/`record`/`instrument`/`field` 交集零命中。
- **无 SLO / 误差预算 / 燃尽率层，无 recording rules、无告警、无目标配置**：`crates/aero-server/src/metrics.rs:1` 只做 RED 信号发射（`metrics_handler`、`http_metrics_layer`），**之上无 SLO/燃尽率/告警层**；`crates/aero-common/src/metrics.rs:121` `DEFAULT_BUCKETS` 固定桶却**无任何 per-route SLO 目标 / objective** 可供评估。grep `recording rule`/`burn_rate`/`error_budget`/`objective`/`apdex`/`PrometheusRule`/`alerting` 跨 crates + README 只命中无关 `slow`/`slot`。
- **无随仓 Prometheus/Grafana/Alertmanager 配置、核心 RED 序列无 per-tenant 标签，per-tenant 看板不可能**：`crates/aero-server/src/metrics.rs:30` 注明 `/metrics` 默认未认证、典型部署由集群内 Prometheus 抓取——**却未在仓内提供 / 配置任何 Prometheus**；`crates/aero-ai/src/metrics.rs:192` per-tenant 成本是**唯一**带 `{kind,workspace}` 的 per-workspace 序列，HTTP RED / 消息吞吐 / WS 连接指标**无 workspace 标签**。`ls deploy/ ops/ monitoring/ grafana/ prometheus/` 均不存在（仅 dev `docker-compose.yml`）。

### 「完成」的样子

- 安装 OTel 全局 `TextMapPropagator(TraceContextPropagator)`；HTTP `TraceLayer` 用自定义 `make_span`，从入站 `traceparent` 提取并链远端父；webhook / AI-provider / S3 / 跨节点 call-bridge 出网注入 `traceparent`。
- 给 `EventBus::publish` 加 headers 参（小 trait 改、大解锁），publish 端注入当前 SpanContext 到 NATS headers、消费端 `extract` 父 SpanContext 并 `set_parent` —— 跨 bus 追踪缝成一条端到端轨迹。
- `inject_request_id` 把 request_id 录进当前 span；fmt 层切 `.json().with_current_span()` —— request_id/trace_id/span_id 进结构化日志，运维可凭 `x-request-id` pivot 到日志与轨迹。
- 在新 `monitoring/` 目录里发 Prometheus scrape + recording rules + 多窗多燃尽率告警 + 误差预算 / objective 配置 + 预置 Grafana 看板 + Alertmanager；给核心 HTTP RED / 消息 / WS 序列加（基数有界的）`workspace` 标签 —— per-tenant 看板有 RED 数据可切、可寻呼降级、可报 SLA。

**收益/体量**：把割裂的三段轨迹缝成一条、把原始 RED 信号变成可寻呼的 SLO，完成「指标→追踪/SLO」层级跃迁。**体量 L**。propagator、bus-header、request-id-span、per-tenant-label 全部可在沙箱构建；**作用域说明**：发 Prometheus/Grafana/Alertmanager 配置与预置看板是落在新 `monitoring/` 目录的 YAML/JSON 部署 artifact——可在仓内构建与评审，但真实抓取 / 告警接线须对真实集群验证，**最后一公里带为 staging 门**。

---

## 方向三（P1）：从问答助手到能动智能体与可检索知识库 —— tool-use 循环 + 文件 RAG + 语言感知 FTS

### 为什么需要它（另一个命名的「问答 AI→能动」跃迁，本版最大 AI 前沿缺口）

这是本版另一个命名的层级跃迁（「Q&A-AI → agentic」），也是单一最大的 AI 前沿缺口。今天每项 AI 能力（summarize/answer/moderate/sentiment/smart-replies）都是单发文本补全：已核验 Anthropic 客户端**刻意跳过**非文本 / `tool_use` 块（`anthropic.rs:127`）、`RequestBody` **无** `tools`/`tool_choice` 字段、worker 派发一个固定 4-kind 枚举且**无规划步**——而数据模型**早已携带**一个 `ToolCall` 块变体（`model.rs:136`）AI 却从不产出。补上它把产品从「答问题」推到「替用户规划并调用 workspace 动作」（找到部署 thread、总结它、建一条跟进任务）——竞品领跑的旗舰能力。agent 只有能**看见** workspace 才有用，这让文件搜索缺口成了天然搭档：文件 / 附件内容**乃至文件名**完全不可搜、缺席 RAG（`File` 块落进 `searchable_text` 的 `_ => None` 兜底，用户连按 PDF 文件名都找不到那条消息、遑论其内容；语音转写**已**被索引，证明这道缝存在——文件是缺的那条腿）。二者是「问你的 workspace / 找那份文档」体验的地基。补全 RRF 融合所依赖路径上的词法质量：FTS 处处硬编码 `'simple'` 配置（已核验），`'deploying'` 匹配不上 `'deploy'`、带音符词不折叠——per-workspace-locale stemmer + unaccent 可实测抬升召回；而存查只有手动 `/run`、无 `last_seen` 追踪，无法浮现「自上次看后有什么**新的**」、无法主动 digest（企业搜索发的存查 / 监控功能）。杠杆：一条文档文本抽取管线**同时**喂 agent 检索工具与 FTS 召回，tool-use 循环是后续每个 AI 动作插入的基底。

### 代码证据（已读）

- **AI 仅请求/响应，无能动 tool-use / function-calling 循环（Anthropic 客户端弃 tool_use、不发 `tools`）**：`crates/aero-ai/src/anthropic.rs:127` 文档「Non-text blocks (e.g. `tool_use`) are skipped — this client deliberately only supports plain text generation for P2.」；`crates/aero-ai/src/anthropic.rs:276` `struct RequestBody<'a> { model, max_tokens, system, messages }`——**无 `tools`/`tool_choice`**；`crates/aero-ai/src/worker.rs:223` `match job.kind { Embed => …, Summarize => …, Moderate => …, Answer => … }`——固定枚举、无 agent/plan 步；`crates/aero-common/src/model.rs:136` `ToolCall { tool: String, args: serde_json::Value, result: Option<…> }`——模型有它、AI 从不 emit。grep `tools`/`tool_choice`/`tool_use` 在 aero-ai 只命中那条跳过注释与一个测试夹具。
- **文件 / 附件内容（乃至文件名）完全不可搜、缺席 RAG，只 `messages.searchable_text` 被索引**：`crates/aero-common/src/model.rs:183` `searchable_text` 把 Text/Code/Voice(transcript)/Button 折入、`File` 落进 `_ => None`；`crates/aero-common/src/model.rs:120` `File { blob_id, kind, name: String, size }`——`name` **从不抵达索引**；`crates/aero-ai/src/service.rs:319` `retrieve_room` 的 `search_vector`/`fts_candidates` 都只跑 `messages` 表。全树无 tika/pdf_extract/ocr/file_content/text_extract，`File` 块从 searchable_text 兜底缺席。
- **FTS 处处只用 `'simple'` 配置，无 stemming / 同义 / 音符折叠**：`crates/aero-storage/src/message.rs:611` `ts_rank(m.search_tsv, {f}('simple', $2)) AS score`（`{f}` ∈ websearch/plain，config 恒 `'simple'`）；`migrations/0002_p2_collab_ai.sql:16` `search_tsv … GENERATED ALWAYS AS (to_tsvector('simple', coalesce(searchable_text, ''))) STORED`；`crates/aero-storage/src/search_query.rs:196` 高级跨房搜索 `websearch_to_tsquery('simple', $2)` 亦 `'simple'`。grep `'english'`/`'chinese'`/`regconfig`/`stemmer`/`unaccent`/`synonym` 在 storage + migrations 零结果。
- **无查询拼写纠错 / 「你是不是想找」/ 搜索自动补全，零结果查询只返回空**：pg_trgm `similarity()` **仅用于模糊排序**（`crates/aero-storage/src/search_query.rs:197`、`message.rs:516`），RRF 的 `fts_candidates` 这一路刻意不做模糊（`message.rs:582` 注「No trigram fallback — fuzzy hits would dilute the lexical signal」）；但 `word_similarity` 驱动的「你是不是想找」/ typeahead / 零结果补全**不存在**——grep `did you mean`/typeahead/word_similarity 只命中频道推荐与文法改写，无搜索建议。
- **存查只有手动 on-demand `/run`，无定时重跑 + 新匹配告警 / digest（无 last-seen 追踪）**：`crates/aero-storage/src/saved_search.rs:8` 注「this repo only owns the saved-query CRUD — it never executes a search itself.」；`crates/aero-server/src/saved_searches.rs:187` `run` 返回当前全部命中、**无 new-since-last-run delta**——无 `last_run`/`last_seen`/cron/digest 列或接线。
- **（低危子项）无搜索点击反馈 / CTR / 相关性分析**：`crates/aero-ai/src/metrics.rs:11` 只跟踪 job 时长/成本/结果/`AI_QUEUE_DEPTH`、**无 relevance/CTR/NDCG**；`crates/aero-storage/src/block_interaction.rs:4` 是唯一的点击日志（交互块按钮）、与搜索无关。
- **（低危子项）AI「记忆」仅 per-(participant,room) 临时会话轮、无持久跨房个性化画像**：`crates/aero-ai/src/service.rs:504` `ctx.get_turns(participant, room, 6)`——per-room 滚动轮、trim；`crates/aero-ai/src/service.rs:477` 注「trimmed … via Redis ZREMRANGEBYRANK」——是 transcript 非学到的画像。
- **（低危子项）搜索返回单一限页、无 faceting/聚合 / 总命中数**：`crates/aero-storage/src/search_query.rs:190`（`let limit = limit.clamp(1, 100)`）+ 同函数 `:215`（`ORDER BY score DESC, m.id DESC LIMIT $3`）——无 OFFSET/cursor、无 COUNT、无 facet；`crates/aero-server/src/saved_searches.rs:182` `RUN_LIMIT` 单一固定 cap。

### 「完成」的样子

- **Anthropic tool-use 循环 + 规划派发**：`RequestBody` 加 `tools`/`tool_choice`，客户端解析 `tool_use` 块，worker 加一个规划步把 Answer 升级为「规划 → 调工具（search / create reminder / post message / fetch file）→ 综合」的多步 agent 循环，落 `ToolCall` 块。
- **文档文本抽取管线**：上传后抽取 PDF/doc/spreadsheet 文本 + 文件名进可索引文本，喂 **agent 检索工具与 FTS/RAG 双侧**——「问你的 workspace / 找那份文档」可用。
- **语言感知 FTS 迁移**：per-workspace-locale → english/chinese stemmer + `unaccent` 扩展，`search_tsv` 与 `*_tsquery` 用语言配置 —— 抬升 RRF 融合所依赖路径的召回。
- **存查定时重跑告警**：`saved_search` 加 `last_run_at`/`last_seen_message_id`，周期重跑整条 operator 查询并 digest 新命中 —— 标准查询 / 监控可用。
- **（P2 子项）** pg_trgm `word_similarity` 驱动「你是不是想找」/ typeahead；搜索点击反馈表 + CTR/NDCG 指标作为学习排序的数据地基；持久跨房 AI 用户画像；搜索 faceting + 总命中数 + keyset 分页。

**收益/体量**：把「答问题」推到「能动智能体」、把「搜消息」扩到「可检索知识库」。**体量 XL、需独立立项**。agent tool-use 循环与语言感知 FTS 迁移可在沙箱构建；**作用域说明**：真实 agentic 质量与真实文档文本 / OCR 抽取器（tika/pdf-extract）依赖外部库 / 二进制与带 tool-calling 的模型，端到端 agent 行为**对真实 AI key 做 staging 门**。CTR 反馈、跨房 AI 记忆、faceting 是低危子项，作 P2 后置——它们是后续学习排序的数据地基、非旗舰。

---

## 方向四（P1）：数据生命周期：分区/归档与 GDPR 抹除正确性收口 —— 先发两个合规 bug，再做范围分区

### 为什么需要它（命名的「无界数据→分区/归档」跃迁，捆两个合规 CRITICAL 抹除 bug）

这个方向把命名的「无界数据 → 分区/归档」跃迁与同属一条数据生命周期写路径的**两个小而合规 CRITICAL 的 GDPR 抹除正确性 bug** 配对。**结构件**：每张高增长 append-only 表（messages / notifications / audit_events / ai_jobs / webhook_delivery_log / stream_viewer_samples）都是单体堆——已核验全仓**零** `PARTITION BY`/pg_partman/`create_hypertable`。唯一删行机制是消息留存 sweep，且它是**软删**（置 `deleted_at`、blank 列），死元组留在堆里、`messages_room_created`/GIN/HNSW 索引继续索引它们，全靠 autovacuum 回收**从无任何结构性 DROP**。按 `created_at` 范围分区把留存 / 抹除变成 O(1) DROP/DETACH-PARTITION 并界住索引大小——无界堆 + 索引膨胀的教科书修法。更糟：四张高频表（notifications / audit_events / ai_jobs / webhook_delivery_log）**根本无任何留存 sweep**（已核验：唯一 `DELETE FROM ai_jobs` 命中是测试夹具）——每次 mention/admin-action/AI-request/webhook-attempt 一行、永久累积，只在父行硬删 FK cascade 时缩；`stream_viewer_samples` 是 30 秒粒度消防水管（4 小时流 ~480 行、规模化每日百万级）、只读方法、零 rollup/cap/降采样——最高基数 append-only 表、最显眼的「按天分区 + DETACH + 分层 rollup」候选。**两个正确性 bug 虽小却是这里最高杠杆**：**（1）** GDPR 账户抹除留下 1024 维消息 `embedding` 向量**完整**——已核验 `participant.rs` 抹除 `UPDATE` 只置 blocks+searchable_text、**从不**置 embedding，与 `message.rs:154` 及 sweep（二者都正确 NULL 它）相左——于是「已删」消息仍**语义可搜**、HNSW 索引仍指向它们（最合规关键写里一个可再识别 PII 的 Art.17 缺口，是明显的不一致而非设计取舍）。**（2）** 同一抹除路径**无 legal-hold 检查**，于是一条删除请求可静默摧毁留存 sweep 的 `NOT EXISTS` 守卫专为保全的取证证据——两条生命周期写路径对「hold 是否权威」**意见相左**。杠杆：一套 `created_at` 分区方案是让**每张热表**的留存与抹除同时变便宜变正确的基底。

### 代码证据（已读）

- **GDPR 账户抹除（delete_participant）留下消息 embedding 向量完整，可再识别 PII 挺过匿名化**：`crates/aero-storage/src/participant.rs:181-190` 的「Anonymise message content (GDPR right-to-erasure)」`UPDATE messages SET blocks = '…[deleted]…'::jsonb, searchable_text = '' WHERE sender_id = $1 AND deleted_at IS NULL`——**只置 blocks + searchable_text**；对照软删 `crates/aero-storage/src/message.rs:154` `SET deleted_at = NOW(), blocks = '[]'::jsonb, searchable_text = '', embedding = NULL` 与留存 sweep `crates/aero-storage/src/workspace.rs:806`（`deleted_at`、`blocks='[]'`、`searchable_text=''`、`embedding = NULL`）都正确清空 embedding。grep `embedding` 在 participant.rs 抹除 UPDATE 内**无命中**——是不一致非设计。
- **GDPR 账户抹除无视 active legal hold，eDiscovery 保全可被静默击穿**：`crates/aero-storage/src/legal_hold.rs:5-9` 注「active hold 覆盖期间留存 sweep MUST NOT 软删该消息……sweep 自身 SQL 携等价 `NOT EXISTS` 排除」；而 `crates/aero-storage/src/participant.rs:182-190` 的抹除 `UPDATE … WHERE sender_id = $1 AND deleted_at IS NULL` **无 `is_held`/`NOT EXISTS` 守卫**。grep `legal_hold`/`is_held`/`NOT EXISTS` 在 participant.rs 无命中——抹除无 hold 排除，而 `sweep_expired_messages` 有。
- **任何热 append-only 表均无时间/范围 PARTITIONING**：`migrations/0001_init.sql:64` `CREATE TABLE IF NOT EXISTS messages (…)` 是纯堆、无 `PARTITION BY`；grep `PARTITION BY`/`pg_partman`/`timescale`/`create_hypertable` 跨 migrations + crates **零命中**（唯一 `partition` 命中是 aero-live-webrtc 的 VP8 视频重组）。
- **无任何留存/sweep 的无界增长表：notifications / audit_events / ai_jobs / webhook_delivery_log**：`crates/aero-storage/src/notification.rs`（全文件）无 `DELETE FROM notifications`/sweep/retention/`created_at <`、只 inserts + read-state；`migrations/0085_webhook_delivery_log.sql:19` 的 `status` ∈ `'delivered'|'dead'` 终态行从不被回收；`crates/aero-storage/src/ai_job.rs`（全文件）无 DELETE/sweep/完成清理。grep 生产 `DELETE FROM notifications`/`DELETE FROM webhook_delivery_log`/`DELETE FROM ai_jobs`（排除测试清理）**零命中**。
- **stream_viewer_samples 是 append-only 分析消防水管（每流每 30s 一行）、零留存**：`crates/aero-server/src/bin/aero-server.rs:843-844` 注「Every 30s, sample each live stream's … viewer count into stream_viewer_samples」；`crates/aero-storage/src/stream_viewer_sample.rs:66` `INSERT INTO stream_viewer_samples (stream_id, viewers) VALUES ($1, $2)`。grep `sweep`/`prune`/`retention`/`DELETE`/`older` 在该 repo 只命中 `retention_curve`/`RetentionPoint` 分析命名、无删行；repo 方法仅 record/stats/retention_curve。
- **（低优子项）无读副本 / 读写分离，全部读写共用一个主库池**：`crates/aero-storage/src/db.rs:23-30` `connect_pg(...) { PgPoolOptions::new().max_connections(max_conns)…connect(url).await }`——单池单 URL；grep `replica`/`readonly`/`standby`/`read-write split` 跨 crates 只命中 notification.rs 无关的 `unread_only`。

### 「完成」的样子

- **（Phase 0 先发，S+M）** `delete_participant` 的抹除 `UPDATE` 加 `embedding = NULL`（S、无迁移）；抹除 `UPDATE` 加 legal-hold `NOT EXISTS` 守卫——要么阻断受 hold 内容的抹除、要么 defer/绕匿，并把冲突上抛（M）。二者是合规 DEFECT 非功能，立即可合。
- **`created_at` 范围分区**：messages 及 append-only 表按 `created_at` 范围分区，留存 / 抹除变 O(1) DROP/DETACH-PARTITION、界住索引大小。
- **四张未清扫高频表留存 sweep**：notifications / audit_events / ai_jobs / webhook_delivery_log 各按策略（drop 读过的旧通知 / drop 完成的 ai_jobs / drop delivered+dead 的 webhook 行 / 旧审计冷转）加 sweep。
- **stream_viewer_samples 分层 rollup**：按天分区 + DETACH，老样本降采样为分层 rollup 后丢原始 30s 粒度。
- **（P2 子项 / staging 门）** 构造只读副本池，把有界陈旧读（分析、导出、搜索）路由到副本，让大导出 / 分析扫描不饿死消息摄入。

**收益/体量**：把无界单体堆变成可 O(1) 老化的分区表、把抹除与留存修成正确且便宜，并堵两个合规 CRITICAL 抹除 bug。**体量 XL、需独立立项**。**排序**：两个 GDPR 正确性修复**先发**（S+M、立即可合、无 schema 迁移风险），作 P1 方向内的快 P0-风味切片；分区是 XL 结构件。读副本拆分是真开放但需真实副本验证路由 / 陈旧度的 infra seam，**作低优子项 / staging 门、非旗舰**。

---

## 方向五（P2）：认证与滥用防御纵深 —— 锁定 + 上传嗅探 + refresh 族撤销 + 行为刷屏 + JWT 轮换

### 为什么需要它（把「有基本控件」推到「纵深防御」，每项 grep 实测仍开放）

一个连贯的安全**纵深**方向，把平台从「有基本控件」推到「纵深防御」——每项都是 grep 返回零的真开放缺口。两个最高危项锚定它。**（1）凭证填充 / 暴破防御**仅有 per-IP 令牌桶（5/min），靠 IP 轮换可轻易绕过（限流器甚至 sweep 满桶空闲条目，轮换喷射者永不累积状态），对单账户的慢速分布式猜测毫无作用——已核验**无** per-account 失败计数、无锁定、无 captcha/PoW、无新设备 / 新 IP 告警，schema 只有单一 `last_login` 时间戳（**无登录 IP/设备历史表**），不可能在现 schema 上建 impossible-travel / 地理速度异常检测；数千次猜测后的成功接管与正常登录**无从区分、零信号**。**（2）上传 blob 信任客户端声称的 MIME**、只对该字符串做前缀 allowlist 校验（`application/octet-stream` 被显式 allowlist）、**无魔数嗅探**——已核验 zero clamav/magic-byte/mime-sniff/infer:: 命中——于是标成 `image/png` 的可执行 / HTML payload 被存储并跨租户分发：IM + 直播平台的典型恶意软件分发 + 存储型 XSS 通道。**中危项**：refresh 轮换正确 401 重放 token、却**不**把已轮换 token 重放当成它本是的盗用信号（`revoke_all_for_participant` 存在却从不从 refresh-reuse 路径调用——盗 token 的攻击者持有有效新会话，受害者重放只 401）；auto-mod 只是内容分类器、**无**行为刷屏 / 洪水 / 重复 / 速度 / 突发检测（刷屏者 10 秒内向 50 房发干净措辞链接什么都触不到）；本地 JWT 签名是单一静态 RSA 密钥、**无 `kid` header、无轮换路径**（轮换被盗密钥是硬切换、全员被强制登出，`kid`+keyring 让它零停机）。发送路径 PII 检测收尾。排 P2 不因项低价值，而因方向一/二带更高危的宕机 / 合规 DEFECT 且本版优先命名的层级跃迁——这是自然的安全续作。

### 代码证据（已读）

- **无登录失败追踪 / 账户锁定 / 可疑认证 step-up（暴破与凭证填充纵深）**：`crates/aero-auth/src/service.rs:149` `login` 找到凭证即 `password::verify(...)` 通过就 `issue_pair(participant.id)`——**无失败计数、无 locked_until**；`crates/aero-server/src/rate_limit.rs:140` `sweep_idle` 让轮换源地址每请求 mint 一个永久桶；`migrations/0001_init.sql:34` `last_login TIMESTAMPTZ` 是唯一字段——**无登录 IP/设备历史、无 failed_attempts/locked_until 列**。grep `failed.?login`/`account.?lockout`/`locked_until`/`impossible.?travel`/`new.?device` 只命中 RTP/SRT `consecutive` 噪声与 2FA workspace gate。
- **无上传 blob 的恶意软件 / AV 扫描或内容类型验证（信任客户端声称 MIME）**：`crates/aero-server/src/routes.rs:1279` `let mime = field.content_type().unwrap_or("application/octet-stream").to_owned(); … if !is_allowed_mime(&mime) { … }`；`crates/aero-server/src/routes.rs:1260` `is_allowed_mime` 只对声称字符串做 `ALLOWED_MIME_PREFIXES.starts_with` + `ALLOWED_MIME_EXACT.contains`（含 `application/octet-stream`）——**无魔数嗅探**。grep `clamav`/`virus`/`malware`/`magic.?byte`/`mime.?sniff`/`infer::`/`safe.?browsing` **零命中**。
- **refresh 轮换无重用检测族撤销，重用只 401 单 token**：`crates/aero-server/src/session.rs:66` `if revoked_repo.is_revoked(&old_hash).await? { return Err(Unauthorized("refresh token revoked")); }`——**只 401、无族撤销 / 告警 / 审计**；`crates/aero-storage/src/auth_session.rs:247` `revoke_all_for_participant(...)` **存在却只被密码改 / 管理流调用、从不从 refresh-reuse 检测调用**。
- **无行为刷屏 / 消息洪水 / 异常检测（auto-mod 仅静态模式）**：`crates/aero-im-core/src/service.rs:1021` `for rule in &rules { if rule.action == "block" && rule.matches(&text) { return Err(…) } }`——单消息静态模式匹配、无速度/重复/突发信号；`crates/aero-server/src/moderation_bot.rs:340` `let verdict = ai.moderate(&job.text).await;`——分类单消息内容、无跨消息行为关联。grep `spam.?wave`/`flood.?detect`/`duplicate.?message`/`velocity`/`burst.?detect`/`posting.?rate` **零命中**。
- **本地 JWT 签名密钥是单一静态 RSA、无 kid header 或轮换路径**：`crates/aero-auth/src/jwt.rs:125` `encode(&Header::new(Algorithm::RS256), &claims, &self.inner.encoding)`——**无 `.kid`**；`crates/aero-auth/src/jwt.rs:62` `struct Inner { encoding, decoding, issuer, … }`——恰一签一验、无 keyring/版本。grep `kid`/`key_id`/`jwks`/`rotat` 在 jwt.rs 无命中（`kid`/JWKS 只在 oidc.rs 验外部 IdP）。
- **（低危子项）消息或导出无 PII 检测/脱敏（GDPR 抹除已有，入站 PII 泄露无守）**：`crates/aero-storage/src/participant.rs:181` 抹除存在；`crates/aero-server/src/moderation_bot.rs:1` 只分类 block-words/abuse、**无 PII/SSN/信用卡检测**。grep `pii`/`ssn`/`credit.?card`/`redact` 只命中 GDPR 抹除/导出、发送路径无 PII 分类。

### 「完成」的样子

- 加 `failed_login_attempts`/`locked_until` 与登录 IP/设备历史表，失败计数 + 临时锁定 + 新设备 / 新 IP 告警 + impossible-travel 异常钩子。
- 上传魔数 / 内容类型嗅探（`infer` crate）核验声称 MIME；真实 AV 扫描（ClamAV 守护进程 / 托管服务）作部署 seam。
- refresh-reuse 路径在「已撤销但签名有效」时调 `revoke_all_for_participant` 杀整族会话 + emit 安全事件 + 审计。
- 在现有 per-sender 历史上建速度 / 重复 / 突发评分，给 auto-mod 加行为刷屏检测。
- JWT 加 `kid` header + 小 keyring（当前签名者 + 接受验证者），签名密钥轮换变零停机例行。
- 发送路径复用现有 moderation 管线做 PII 检测 / 脱敏 / 隔离，防 PII 跨租户进导出 / AI embedding。

**收益/体量**：把「有基本控件」推到「纵深防御」。**体量 L**。全部可在沙箱构建，**除**真实 AV 扫描（需 ClamAV 守护进程 / 托管服务）——魔数 / 内容类型嗅探在沙箱内实现，AV 守护进程接入**作部署 seam**。PII 检测复用现有 moderation 管线故可构建；行为刷屏评分建在现有 per-sender 历史上。

---

## 附：方法与边界

- 本版由若干**只读**子代理并行扫描（自愈韧性与金钱路径 / 可观测性与 SLO 深度 / 搜索相关性与 AI 能动前沿 / 存储增长与数据生命周期 / 安全深度与滥用），每条结论附 `file:line`，跨维度聚类去重、并逐项与第四版交付物核对后成此 5 方向。**未写任何业务代码。**
- 体量记号：S<1d、M≈数日、L≈1–2 周、XL≈需独立立项。
- 测试数与迁移数以 [`README.md`](README.md) / `migrations/` 为准，本文不硬编码。
- **不变的非目标**（承袭 `AGENTS.md`，本版不据此扩展）：E2E 端到端加密客户端、联邦、移动端原生 SDK。
- 标注为 seam 的真实链路本沙箱不可端到端验证——相关项以 staging 联调为交付边界，勿在 CI 内标「done」：① 第四版媒体面携往的编码后 REMB/PLI 背压写到发布者出站 RTCP，需真实浏览器 / 第二节点 ICE/DTLS/SRTP 握手（`forward.rs:46-51`）；② 本版方向二的 Prometheus/Grafana/Alertmanager 抓取 / 告警接线需对真实集群验证；③ 本版方向三的真实 agentic 质量与文档 OCR 抽取需真实 AI key + 外部抽取库 / 二进制；④ 本版方向四的读副本路由 / 陈旧度需真实只读副本；⑤ 本版方向五的真实 AV 扫描需 ClamAV 守护进程 / 托管服务。
