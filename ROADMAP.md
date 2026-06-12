# Aero IM — 扩展路线图（ROADMAP · 第四版）

> **资深架构师 / 产品经理视角。** 基于 2026-06-12 一次性全局代码扫描（16 crates / 122 迁移 / 119 个导出 `routes()`/`router()` 的 server 路由模块 / web SPA）。
> 本文只做规划与论证，**不含任何代码**。工程约定见 [`AGENTS.md`](AGENTS.md)，功能矩阵见 [`README.md`](README.md)。
> 每条结论附代码证据（`file:line`），可逐条核验。

---

## 缘起：第三版已交付，应用层 CRUD 已挖尽，本版攻「收口」

应用层与前三版 ROADMAP 已落地，**本版不再堆 CRUD**——后续若干轮竞品对标 / AI 前沿 / 可运维性专项已把可在沙箱内构建的功能逐条挖尽（见 README 功能矩阵与迁移 0001–0122）。本版只攻一类事：**已交付能力的「收口」**——那些在前几版里**做了一半、或在后续浪潮里被重新引入**的写放大、投递空洞、运维硬伤、成本黑洞与媒体面未通电的实体。

**第三版五方向均已交付，逐一退休（retired）：**

- **第三版方向一（实时投递完整性）已交付**：客户端 `?since=` 重连回填（`web/ws.js:96`）、按 subject 单调 `seq` 去重（`aero-bus/src/seq.rs`）、`mark_read` 只增不减 + 多端 `RoomEvent::Read` 广播收敛（`service.rs:1456`、`receipt.rs:33`）、弹幕尾部追赶（`ws.rs` WatchStream 带 `since`）。**「新消息」前向投递这一半已完成、退休。** 仅剩**变更回放**（edit/delete/ephemeral/retention）与多端订阅 / 流重订阅缺口 —— 升格为新**方向一**。
- **第三版方向三（AI 成本真实化与 RAG 完整性）已交付**：真实 Anthropic `usage` 解析（`complete_with_usage`）、embedding 回填循环、RRF 重排、预算门控的审核**队列**、worker 并发环境可配。成本**计量**（`aero_ai_cost_micros_total`）已上线、退休。仅剩成本**enforcement**（准入）、真实审核计费、voyage query-embedding 非对称 —— 升格为新**方向四**。
- **第三版方向四（数据面写放大，prefs 那一半）已交付**：房间静音（`muted_set`）与免打扰 / 暂停（`dnd_snooze_many`）抑制层均已批量化为单条 `= ANY($)` 查询；未读计数覆盖复合索引、HNSW `m=16/ef_construction=128` 重建、DB 连接池环境可配均已落地、退休。仅剩 workspace-mute 循环、thread-level 循环、按收件人逐条 Notify publish、查询期 `ef_search` —— 这些是 doc 之后才引入的残留，升格为新**方向二**。
- **第三版方向五（企业级加固下一层）已交付**：S3 启动期 fail-loud + `/health` 后端标签、per-workspace 限流档位（Standard/Premium/Unlimited）、有界启动连接重试 / 退避、CORS 对放行型 origin fail-closed、消息删除 + 审计事务化均已落地、退休。新**方向三**里的可运维性缺口（就绪排空、worker 关停、限流器淘汰、探针旁路、blob 就绪、S3 重试）是**另一组、doc 之后才暴露**的实体。

> **本版与第三版的关系**：第三版回答了「真实流量进来时最先崩的、最伤信任的、最花钱的是什么」并系统性交付。第四版收口它的**残留与残影**——计量做了但没准入、前向投递通了但变更回放没通、加固做了一批但零停机部署仍是断的、媒体面逻辑全建好了却从未通电。下面 5 个方向均为本次扫描中**实测仍存在**、且**前几版从未覆盖**的缺口，已逐一核对，不重复已完成项。

---

## 总览与优先级

三大产品支柱：**协作可信赖**、**互动直播是差异化**、**AI-native 可持续**，外加 **To-B 信任**的运维底座。

| 优先级 | 方向 | 解决什么 | 支柱 | 体量 |
|---|---|---|---|---|
| **P0** | 三、零停机部署与多租户公平性加固 | 就绪排空 + 后台任务有界退出 + 限流器有界化 + 探针旁路 + blob/S3 韧性 | To-B 信任 | M |
| **P0** | 一、投递一致性闭环 | reconnect/sweep 必须重放 edit/delete/ephemeral + 多设备订阅 + 流重订阅 | 协作 / To-B 信任 | L |
| **P1** | 二、广播提醒扇出的数据面与索引收口 | 批量 workspace-mute / thread-level + 单次多接收人 Notify + HNSW `ef_search` | 协作 | M |
| **P1** | 四、AI 成本治理从计数到真实美元 | 按 token/micros 计费的准入 + 真实 moderation 计费 + voyage query/document 非对称 | AI | L |
| **P2** | 五、媒体面真实跨节点化与多编解码正确性 | SFU UDP 驱动 + 节点间 RTP 拉流 + REMB/PLI 出网 + AV1/H.265 关键帧 + SRT 重排序 | 直播 | XL |

> 建议落地顺序：**三 → 一 → 二 → 四 → 五**。三体量最小、相互独立、且消除的是**正在生效的生产隐患**（零停机部署是断的、限流器被 IP 喷射可 OOM、探针被打挂会杀健康 Pod），每小时风险削减最高、为后续一切安全上线兜底；一信任 / 合规影响最大且直接建在已完成的 `?since` 管道上；二是纯后端规模工，复刻仓内已有模式；四体量 L、触及预算 / 准入核心，成本超支是经济风险非可用性风险故靠后；五体量 XL、需真实多节点 + ICE/DTLS/SRTP staging，前序路线图已明令「勿在 CI 内标 done」，正确排在最后。

---

## 方向三（P0）：零停机部署与多租户公平性加固 —— 把「能演示的服务」变成「企业敢运行的服务」

### 为什么需要它（最高可运维性 ROI，且几乎全是铲子活）

这是**正在生效的生产隐患**集群，且绝大多数是 S 体量、可独立测试。它们是把「demo 级服务」变成「企业愿意跑的服务」的那些经典项：零停机部署当前是**断的**，限流器是 **DoS 放大器**，k8s 探针会被自身限流**误杀健康 Pod**。每一项都是小而可独立验证的加固，合在一起构成可信的 SRE 故事。与第三版交付的**启动期** fail-loud 是不同的、doc 之后才暴露的一组运行期缺口。

### 代码证据（已读）

- **优雅关停从不翻转就绪态**：`crates/aero-server/src/bin/aero-server.rs:1043` `shutdown_signal` 收到 SIGTERM 后只 `ai_shutdown.cancel()`（`aero-server.rs:1067`）并让 axum 排空在途 HTTP；但 `AppState` 上**没有任何 shutdown 标志**供探针读取，`crates/aero-server/src/routes.rs:562` `health_ready` 全程只看 PG/Redis/NATS（`routes.rs:564`）→ 滚动发布时 LB 的端点摘除滞后于 SIGTERM，新 HTTP/WS 连接落到正在拆 worker 的 Pod → 连接重置 + 5xx。这是经典零停机部署缺口。
- **后台任务无有界排空：部分派发器不收令牌、且无统一 join**：`shutdown_signal` 收到 SIGTERM 只做 `ai_shutdown.cancel()`（`aero-server.rs:1067`）。令牌确实克隆进了约 8 个任务（`aero-server.rs:232/427/512/562/737/768/792/820`，含留存 sweep 循环），但有两类漏洞：① 部分长驻派发器**签名根本不收** `CancellationToken`、永不参与协作式取消 —— webhook DLQ 重试循环（`webhooks.rs:478` `run_webhook_retry_loop(state, interval_secs)`）、push_bot（`push_bot.rs:39` `run(state)`）—— 进程退出时被运行时直接 drop，一个 mid-HTTP-POST 的 webhook 投递或 mid-DB-write 被切断 → **重启后 webhook 重发、GC/导出态半写孤儿**；② 即便持令牌的任务也**从不被 await**：`main()` 在 `axum::serve().await?` 后立即返回（`aero-server.rs:1037`），全仓**无 `JoinSet`/`TaskTracker`/`join_all`** → `cancel()` 之后没有任何有界排空，令牌只是「请尽快停」而非「等你停完」。
- **per-client 限流器 DashMap 无淘汰、无界内存增长**：`crates/aero-server/src/rate_limit.rs:46` 每个不同 `ClientKey`（participant id **或** source IP）存一个 Bucket，`rate_limit.rs:98` `or_insert` 后**从无任何移除**（无 retain/sweep/TTL/cap，全文件仅 `rate_limit.rs:122` 的 `buckets.len()`）→ 攻击者从轮换源 IP（IPv6 或僵尸网络下轻而易举）喷射即为每个 IP 永久建桶，撑到 OOM。**限流器——一个 DoS 防御——反成 DoS 放大器。**
- **k8s 探针与 `/metrics` 共享 IP 限流桶**：`rate_limit::layer` 包住整个 router（`aero-server.rs:1010`），`/health/live`、`/health/ready`、`/metrics` 均未认证、按 source IP 计（`rate_limit.rs:184` `ClientKey::Ip(...)`），`rate_limit.rs:159` 的 path match 把它们一并归到 baseline 限流器 → 过载时 NAT/ingress 出口共享 IP 耗尽桶：liveness 被 429 → k8s 重启**本健康** Pod（放大过载）；readiness 被 429 → Pod 被摘出轮换；`/metrics` 被 429 → 仪表盘恰在事故中**致盲**。
- **就绪探针与优雅失败忽略 blob 存储**：`crates/aero-server/src/routes.rs:475` `probe_deps` 只查 PG/Redis/NATS（`routes.rs:519` 返回 `(pg, redis, nats)`）；`blob_backend` 仅出现在 `health()`（`routes.rs:541`），**不在 `probe_deps`、也不在 `health_ready`** → `AERO_BLOB_BACKEND=s3` 时若 S3 启动后失联（轮换凭据 / 桶策略变更 / 区域故障），Pod **保持 ready** 并持续接受全部 5xx 的上传 / 下载，而非被摘出轮换。
- **S3 无瞬时重试、构建失败静默丢超时**：`crates/aero-storage/src/s3_blob_store.rs:165` 每次 put/get/delete 只 `req.send()` 一次，瞬时 5xx/超时（S3 正常行为、AWS 明确建议重试）即以 `BlobStoreError::Io` 失败终端用户的附件（`s3_blob_store.rs:169` 直接报错，全文件无 retry/backoff）；且 `s3_blob_store.rs:102` 构造 reqwest client 的 30s 超时经 `unwrap_or_default()` 兜底 —— 一旦 builder 失败，default `Client` **无超时**，挂死的 S3 端点可无限期钉住一个请求。
- **承密配置结构体 derive(Debug)**：`crates/aero-common/src/config.rs:65` `AuthConfig`（`config.rs:68` `jwt_private_key_pem`）与 `s3_blob_store.rs:50` `S3Config`（`s3_blob_store.rs:56` `secret_key`）均 `#[derive(Debug)]`、`AppConfig` 透传 → 今日无活跃泄露，但任何人将来加一行 `info!(?cfg)` 即把 RSA 签名私钥 / S3 密钥明文落进日志 / 遥测。

### 「完成」的样子

- `AppState` 加 `shutting_down: AtomicBool`，`shutdown_signal` 在 `cancel()` **之前**置位，`health_ready` 置位时返回 503，随后 sleep 一段排空宽限期再放行 `serve()` 完成。
- 把关停令牌穿进每个派发器的 `select!` 循环，退出前 await 一个 `TaskTracker` —— 实现有界排空，消除 webhook 重发 / 半写孤儿。
- 限流器加周期性 `retain()`，丢弃空闲（`last < now - window`）且满桶的条目，或以 LRU 给 map 设上限。
- `/health` 与 `/metrics` 前缀在限流检查前短路 `Ok(next.run)`；`probe_deps` 在 backend==s3 时加一次廉价 blob HEAD/list 探测，`health_ready` body 带上 `blob_backend`。
- S3 `send()` 包一层对 5xx/超时的有界退避重试；`unwrap_or_default()` 改为传播 error（工厂已是 `Result`）。
- 给承密字段手写 Debug，密钥字段打印 `<redacted>`。

**收益/体量**：把「能跑」升级为「企业敢交付」，且几乎全是 S 体量、可拆分独立测试。整体 **M**；先发 readiness-drain + worker-token + 限流器淘汰 + 探针旁路一波快浪，再补 blob/S3/Debug 尾。

---

## 方向一（P0）：投递一致性闭环 —— reconnect/sweep 必须重放 edit/delete/ephemeral

### 为什么需要它（最高信任 / 合规影响，是已完成 `?since` 的天然续作）

旧方向一只解决了**新消息**追赶（`?since`），却留下一个巨大的**变更回放空洞**：edit、delete、reaction、retention 软删、ephemeral 硬删**从不**到达「断连后重连」的客户端（`list_since` 只返回 `id>cursor AND deleted_at IS NULL`），周期性 sweep 是裸 DELETE/UPDATE、**根本不发 Deleted 事件**。净效应：一条因审核 / 合规被移除的消息，或一条「阅后即焚」的 ephemeral 消息，**在每个重连和每个在线客户端上保持可见**直到手动刷新 —— 这是隐私 + 合规缺陷，不是装饰性的，它直接击穿两个已交付功能（ephemeral 消息、retention）。同根（state 键太粗）捆绑的还有两个多设备正确性 bug 与一个客户端复活竞态。修好它，实时层才真正收敛 —— 这是企业 IM / 直播产品的入场资格。

### 代码证据（已读）

- **reconnect/REST 回填从不重放 edit/delete/reaction**：`crates/aero-storage/src/message.rs:362` `list_since` 的 `WHERE room_id = $1 AND id > $2 AND deleted_at IS NULL` → WS 回填（`crates/aero-server/src/ws.rs:891` `backfill_since`，`ws.rs:917` 只发 `ServerFrame::Message`）与 REST `?since=`（`crates/aero-server/src/routes.rs:893` 同样走 `list_since`）都**只取 id 大于游标的新消息**。客户端已有的旧消息（`id <= cursor`）若在断连期间被编辑或删除，**永远收不到变更**；`web/ws.js:124-126` 注释自承「edit/delete 携带的是更老的 id，被 `>` 守卫忽略」。grep `edited_since/deleted_since/changes_since/tombstone-replay` **零结果**——无任何回放路径。
- **retention + ephemeral sweep 删 DB 却不发任何 Deleted 事件**：`crates/aero-storage/src/message.rs:1005` `sweep_ephemeral` 是纯 `DELETE ... WHERE expires_at < NOW()` 返回行数、**无 bus/RoomEvent 引用**；sweep 循环 `crates/aero-server/src/bin/aero-server.rs:452`（retention 软删）/ `aero-server.rs:457`（ephemeral 硬删）**只记数日志**，两次 sweep 后均无 `publish_room_event`/`RoomEvent::Deleted`/`hub.fan_out` → 删除从不抵达在线或重连客户端。**「阅后即焚」的消息对在线观众仍可见**——直接背叛该功能本意。
- **多设备 unregister 一刀切清空全部订阅**：`crates/aero-server/src/hub.rs:122` `subs` 仅按 `ParticipantId` 键（`ParticipantSubs` 无 socket 判别），`hub.rs:179` `register`（`hub.rs:180` 向 per-PID `Vec<WsSender>` push，多 socket 受支持）；但 `hub.rs:207` `unregister` 在 `hub.rs:197` 那个只保护 `conns` 移除的 `if entry.is_empty()` 守卫**之外**无条件 `self.subs.remove(&pid)` → 用户任一 socket（手机）断开，会把该 PID 从所有 room/stream/call 路由表清掉，**桌面端这条仍开着的 socket 静默停收**消息 / 弹幕 / 礼物 / presence，直到手动重新加入。
- **重连不重订阅直播流、重订阅无 since 游标**：`web/app.js:252` 唯一的 `ws.on('open')` 只 `joinRoom(currentRoomId)`，**无对 `state.watchedStreams` 的循环**去重发 `watch_stream`；而 server `unregister` 在 `hub.rs:211` 已把 PID 从 `stream_watchers` 移除 → 任何网络抖动后，观众停收弹幕 / 礼物 / 观看人数直到切房或重挂卡片。即便卡片重挂，`app.js:779` `registerLiveCard` 调 `watchStream(streamId)` **不带 since**，而协议本支持（`web/ws.js:230` `watchStream(streamId, since=null)`）→ 间隔期弹幕丢失。
- **客户端可复活已删消息**：`web/app.js:454` `handleDeleted` 置 `deleted_at` 并清 blocks，但 `web/app.js:445` 的乱序守卫只比 edit 时间戳，`web/app.js:438` `handleEdited` 在替换前（`app.js:449`）**不查 `arr[idx].deleted_at`** → 一条后到的乱序 `Edited` 帧（其 `deleted_at:null`）覆盖删除态、**把消息复活在屏上**。需乱序投递触发（per-connection SeqGate 通常排好序），故影响窄，但跨实例 replay 或 REST/WS 交错可触发。

### 「完成」的样子

- 新增按变更游标的回放：reconnect 与 REST `?since=` 除新消息外，重放区间内 edit/delete/reaction（去掉 `deleted_at IS NULL` 过滤、引入 tombstone 回放）。
- 两处 sweep 之后发 `RoomEvent::Deleted` 经 `publish_room_event`/`hub.fan_out` 扇出 —— ephemeral / retention 删除实时抵达在线与重连客户端。
- `unregister` 改按 socket 粒度记订阅（或仅在该 PID 的最后一条 socket 关闭时才移除路由），多设备互不影响。
- 客户端 `on('open')` 增对 `watchedStreams` 的重订阅循环并携带 per-stream `since`；`handleEdited` 替换前检查 `deleted_at`，已删则忽略陈旧 edit。

**收益/体量**：把「不丢数据」从前向投递扩到**变更收敛**——ephemeral / 审核删除必须真的消失，这是合规底线。体量 **L**：edit/delete 回放查询 + sweep→Deleted 事件发布是脊柱，hub 多 socket 修复与流重订阅是可并行子任务，客户端复活守卫是 S 尾。

---

## 方向二（P1）：广播提醒扇出的数据面与索引收口 —— 批量抑制 + 单次多接收人 Notify + 召回旋钮

### 为什么需要它（旧方向四的直接续作，复刻仓内已有模式）

旧方向四批量化了房间静音与 DND 抑制层，却漏了**另两层抑制**与**整个发布侧**。对一次 1 万人的 `@everyone`，`dispatch_notifications` **仍**发 1 万次串行 `is_muted` 往返（`service.rs:1778`）+ 每收件人一次 `get_level` 循环（`service.rs:1818`），再 publish **1 万条独立 NATS `RoomEvent::Notify`**（`service.rs:1892`，每条投到每个节点）—— 因为 `RoomEvent::Notify` 只携带单个 `mentioned` id（`model.rs:834`）、**无收件人列表变体**。这是一次广播提及的**集群级主导成本**，随房间线性增长——正是旧方向针对、却只关了一半的写放大类。捆绑的还有 pgvector 召回旋钮：`search_vector*` **从不发** `SET LOCAL hnsw.ef_search`，尽管 `ef_construction=128`，查询游走只探索默认 40 个候选并 POST-filter，**稀疏 / 小房间静默返回近空**。同根（调了构建、没调查询），便宜可落，提升每条 grounded answer 与搜索。

### 代码证据（已读）

- **workspace-mute 抑制是 O(N) 串行往返**：`crates/aero-im-core/src/service.rs:1778` `for id in &ids { ws_mute_repo.is_muted(*id, workspace).await }`，`crates/aero-storage/src/workspace_mutes.rs:72` 只暴露单行 `is_muted`；对照同函数内房间静音 `crates/aero-im-core/src/service.rs:1852` 的 `prefs.muted_set(room, &ids)` **已是批量** `= ANY($)`。grep `is_muted_many/muted_participants` 跨 crates **零结果**。
- **workspace_mutes 主键以 participant_id 打头，批量查无法走索引**：`migrations/0095_workspace_mutes.sql:6` 仅定义 `PRIMARY KEY (participant_id, workspace_id)`，自然批量形 `WHERE workspace_id=$1 AND participant_id = ANY($2)` 因前导列是 participant_id 无法 seek、被迫扫描；跨 `migrations/*.sql` 无任何 `(workspace_id, ...)` 索引。
- **广播提及扇出为 O(N) 独立 Notify publish**：`crates/aero-im-core/src/service.rs:1892` 把每个 notifiable 收件人各 map 成一次 `publish_room_event`（`service.rs:1900`），`buffer_unordered(NOTIFY_PUBLISH_CONCURRENCY)` 只限并发不限总数；`crates/aero-common/src/model.rs:834` `RoomEvent::Notify { mentioned, .. } => vec![*mentioned]` 携单个 id、**无多接收人变体**（对照 `MessageEnvelope.recipients` 一次 publish 在收端扇出）。
- **thread-level 检查逐收件人串行**：`crates/aero-im-core/src/service.rs:1818` `for &recipient in &reply_recipients { tnp_repo.get_level(recipient, root).await }`；现有批量 `crates/aero-storage/src/thread_notification_prefs.rs:84` `level_map` 批的是**错的轴**（一人多 root），dispatch 需要的是「多人一 root」、无对应批量方法 → 退化成 O(R) 串行。被 thread-follower 数量界住故较低危，但同函数同 N+1 反模式。
- **向量检索从不在查询期设 `hnsw.ef_search`**：`crates/aero-storage/src/message.rs:514`（及 `message.rs:520` 的 room/workspace 边界）`ORDER BY m.embedding <=> $2 LIMIT $3` 前**从不** `SET LOCAL hnsw.ef_search`，`migrations/0075_index_tuning.sql:24` 只设了构建期 `ef_construction=128`；房间 / 工作区过滤是对一次默认 ef（40）HNSW 游走的 **POST-filter**，`crates/aero-ai/src/service.rs:940` `RETRIEVAL_POOL=40` 只是 SQL `LIMIT` 不是 `ef_search` → 多租户繁忙库里那 40 个全局最近多属其他房间，小 / 稀疏租户**召回坍塌**。grep `ef_search/SET LOCAL/set_config/probes` 跨迁移与 storage **零结果**。

### 「完成」的样子

- 新增 `muted_participants(workspace, &[ids]) -> HashSet`（镜像 `thread_mute::muted_by`）+ `(workspace_id, participant_id)` 复合索引，让批量 workspace-mute 抑制走索引。
- 新增 `levels_for(root, &[participants])` 批量（`participant_id = ANY($) AND root_message_id = $`），消除 thread-level N+1。
- 给 `RoomEvent::Notify` 加携带收件人列表的多接收人变体（如 `MessageEnvelope.recipients`），一次 publish 在收端扇出 —— 把 O(N) NATS publish 收敛为 O(1)。
- 检索热路径协同落地：查询前 `SET LOCAL hnsw.ef_search`（按 `RETRIEVAL_POOL` 调高），让 room/workspace 过滤建立在更宽的候选游走上。

**收益/体量**：消除一次广播提及的集群级写放大，并修复多租户向量召回坍塌——同时改善每条 grounded answer 与搜索。体量 **M**：批量抑制 + 复合索引 + 多接收人 Notify 一并做，`ef_search` 的一行 `SET LOCAL` 作为同热路径的琐碎协同落地项捎带。

---

## 方向四（P1）：AI 成本治理从计数到真实美元 —— 让 AI 面经济可治理，而非仅可观测

### 为什么需要它（旧方向三只交付了一半的「成本真实化」）

旧方向三加了真实 token-usage **解析**与 per-workspace 成本**计量**（已验证：`complete_with_usage`、`aero_ai_cost_micros_total`）—— 但**计量不是 enforcement**。准入仍按 job **条数**每条收 1 单位（`worker.rs:541` `acquire_up_to(len(runnable))`），`CostBudget/KeyedCostBudget` 只暴露按计数的 `acquire_up_to(n:u32)`、**全仓无 `max_micros`/spend 字段**。于是一个 120 调用窗口被 120 个零 token embed 与 120 个跑满 800 token 的 Answer 补全**等量消耗**——约 **250×** 真实成本差，而那个准确的 `token_micros` 只在调用**之后**算给指标、从不回喂准入。租户因此能在不破上限的前提下跑出巨额真实开销，击穿 chargeback / 滥用归因。更糟，最高频付费路径**完全不可见**：server `moderation_bot`（每窗口至多 300 调用）**不记任何成本指标**，worker 的 Moderate 路径按 `moderate_micros` 收费而其默认值是 **0** —— 一个刷洪水的租户生成数千次付费审核补全，账面显示 **\$0**。捆绑 voyage-3 的 query/document 非对称 bug（query 侧也硬编码 `input_type=document`），同 RAG-质量主题、S 体量。这是让 AI 面**经济可治理**而非仅可观测的那个方向。

### 代码证据（已读）

- **AI 预算是「按调用计数」而非「token/美元上限」**：`crates/aero-ai/src/worker.rs:541` `budget.acquire_up_to(runnable.len())` 不分 kind/token 收费；`crates/aero-ai/src/budget.rs:58` `grant = max.saturating_sub(used).min(n); used += grant`——纯计数；`crates/aero-ai/src/metrics.rs:149` `token_micros(input, output)` 算出的准确数只流向 `record_token_cost`（一个指标）、**从不喂给 `CostBudget`**；budget.rs 无任何 `micros`/spend 字段。
- **真实 Anthropic 审核开销记为零成本**：`crates/aero-ai/src/service.rs:868` `service::moderate` 用 `client.complete(...)`（非 `complete_with_usage`，**不捕获 token usage**），`handle_moderate` 从不 `attach_usage`；`crates/aero-ai/src/metrics.rs:108` `CostModel::default` 设 `moderate_micros: 0`；`crates/aero-server/src/moderation_bot.rs:54` 只有 `AI_MODERATION_CALLS_TOTAL`/FLAGGED/SKIPPED 计数器、**无 record_cost / `AI_COST_MICROS_TOTAL` 引用**（对照 `worker.rs:438` Summarize/Answer 才被计为应计费）。每次付费审核都收 0 micros。
- **voyage query embedding 用了 `input_type="document"`**：`crates/aero-ai/src/embed.rs:92` `input_type: Some("document")` 对**所有**调用硬编码；而 `crates/aero-ai/src/service.rs:320` 与 `crates/aero-ai/src/service.rs:342`（retrieve_room/retrieve_workspace/topic search 的 query 侧）都调 `embed_one(q)`。voyage-3 的查询与文档嵌入是非对称的——把用户问题嵌入 document 空间会可测量地降低与语料的余弦匹配，削弱每条 grounded answer / 搜索的向量半。Embedder trait 只暴露 `embed_one(text)`、无 role 变体。

### 「完成」的样子

- 给 `CostBudget/KeyedCostBudget` 加 `max_micros`/spend 维度；准入按 kind 的预估 micros（embed≈0 / Answer 按 `max_tokens`）扣减，调用后用真实 `token_micros` 对账修正——从「计量」升到「enforcement」。
- `service::moderate` 改走 `complete_with_usage`、`attach_usage`；`moderation_bot` 记 `AI_COST_MICROS_TOTAL`；`moderate_micros` 默认值给真实估算 —— 付费审核可见、可 chargeback。
- Embedder 加 `embed_query`/带 role 变体，三条 query 侧路径用 `input_type="query"`、存储侧仍用 `"document"`。voyage 修复作为快速召回赢早落。

**收益/体量**：让 AI 面经济可治理（可计费、可归因、可设防），而非只可观测。体量 **L**（触及 budget/admission 核心）；voyage query-embedding 修复是其中 S 体量的早收益项。

---

## 方向五（P2）：媒体面真实跨节点化与多编解码正确性 —— SFU 通电、节点间拉流、出网背压、关键帧补全

### 为什么需要它（最大的剩余规模项，旧方向二的真正续作）

SFU 是 ~4–6 人 mesh 通话（每人上传 N-1 份）与**真正群组规模**之间的分水岭。所有难逻辑都已发货且单测覆盖——`SfuForwarder::on_rtp` 扇出、per-subscriber remap、simulcast 选层、REMB/TWCC AIMD、`CallBridgeSupervisor` 幂等 spawn/cancel——但**没有一处被驱动**：`rg -c` 在 server 内对 `on_rtp/poll_*/SfuPeer::/UdpSocket` 返回**零**，唯一接线的 `UpstreamFactory`（`NodeRtpPullerFactory`）是 None 桩，`poll_remb_requests/poll_keyframe_requests` 从不被排空、发布侧编码器拿不到背压。接线 SfuPeer 的 UDP 回路是最高价值媒体项；跨节点 recvonly RTP puller 可镜像已有的 `aero-live-whip` WHEP 级联。折进的两个正确性尾巴只在媒体真通后才咬人。**仅排 P2 因为它是 XL 且基本需真实多节点 + ICE/DTLS/SRTP staging**——旧路线图本身就把这条 scope 为「勿在 CI 内标 done」。

### 代码证据（已读）

- **群通话仍是全 mesh P2P，str0m SFU 媒体路径全建好却从未通电**：`crates/aero-live-webrtc/src/peer.rs:10` 文档示例 `socket.send_to / forwarder.on_rtp(peer.id(), rtp)` 即应有的 UDP 回路；`crates/aero-server/src/bin/aero-server.rs:118` 只构造了 `SfuForwarder` 并用其 legacy `MediaForwarder::forward_rtp`（自承「真实 RTP 扇出请用 `SfuForwarder::on_rtp`」、实际不转发），WS CallJoin 路径 `crates/aero-server/src/ws.rs:706` 只在客户端间中继 `CallEvent::Offer/Answer/Ice`；`crates/aero-live-webrtc/src/lib.rs:30` 明示「UDP 回路的 server 接线刻意 out of scope」。每个参与者跑全 mesh、上传 N-1 份编码副本，实测封顶 ~4–6 人，BWE/REMB/simulcast 机器对真实通话毫无作用。
- **跨节点桥拉不到远端 RTP**：`crates/aero-server/src/call_bridge_supervisor.rs:114` `NodeRtpPullerFactory::connect` 体内是 `// TODO(real-transport)` 并在 `call_bridge_supervisor.rs:120` 返回 `None`，它是 `aero-server.rs:353` 唯一接线的 `UpstreamFactory`；其周边编排（`ws.rs:677` `ensure_bridges`、cancel_call）在 WS 路径里活着，但生产中**从不拿到非 None 的 upstream** → 跨节点群通话从不真正桥接媒体，node-B 的参与者对 node-A 不可见。剩余 seam 是 recvonly 节点间 RTP puller（SDP 交换 + 对端 bridge 端点的 ICE/DTLS/SRTP），`aero-live-whip/src/upstream.rs` 的 WHEP 级联已实现、可镜像。
- **聚合 REMB 与合并关键帧请求算了却从不上线**：`crates/aero-live-webrtc/src/forward.rs:48` 注明 `PendingRemb` 仍须写到发布者出站 RTCP（真实 DTLS-SRTP）才能抵达 OBS/浏览器编码器——这是 infra seam；`crates/aero-live-webrtc/src/forward.rs:425` `poll_remb_requests()` 有定义与 intra-crate 测试，但**无 server 调用方排空队列** → `encode_remb` 输出从不抵达发布者，编码器拿不到背压、对拥塞 SFU 持续全码率轰炸。
- **SFU 关键帧检测仅覆盖 H.264/VP8/VP9**：`crates/aero-live-webrtc/src/peer.rs:75` 明示「任何其他 codec（AV1、H.265、音频…）yields false」，`crates/aero-live-webrtc/src/codec.rs:16` 同述 → `InboundRtp.is_keyframe` 驱动 simulcast SwitchAndForward 与 PLI 满足，一旦媒体通电（上一发现），AV1/H.265 发布者的订阅者可能永远卡在 bootstrap 层（up-switch 等一个永不被检测的目标层关键帧）、新订阅者的 PLI 永不被识别为已满足。AV1 日益成为现代 WebRTC 默认，这是前瞻性正确性缺口。无 av1.rs/h265.rs 描述符解析器。
- **SRT 接收路径按到达序喂 MPEG-TS segmenter**：`crates/aero-live-srt/src/lib.rs:47` 与 `crates/aero-live-srt/src/lib.rs:292` 注明「接收侧重排序仍未接、v1 in-order best-effort」；`feed_packet`（含 `crates/aero-live-srt/src/lib.rs:697` `feed_ts_bytes(payload_slice)`）跑 seq 过 `ReliabilityState` 做丢包记账后**直接把 payload 按到达序推进 segmenter**、无 per-seq 重排 / 抖动缓冲 → 重排序路径（公网常见）上 TS 连续性破裂、损坏本可由重传恢复的 HLS 段。SRT 低 RTT / 低重排场景常见故较低危，但这是 SRT 数据面唯一真未接的一块。

### 「完成」的样子

- **SFU UDP 回路（脊柱）**：server 构造 `SfuPeer`、绑定媒体 UDP socket，驱动 `poll()` → `on_rtp` 扇出、排空 `poll_remb_requests/poll_keyframe_requests` 并经真实 DTLS-SRTP 把 `encode_remb`/PLI 写回发布者出站。
- **跨节点 recvonly RTP puller**：实现 `NodeRtpPullerFactory::connect`（镜像 WHEP 级联），让同一 `CallId` 跨实例真正桥接媒体。
- **多编解码正确性（后续）**：补 AV1/H.265 payload-descriptor 关键帧检测；SRT 接收侧在 reliability 与 segmenter 之间加 per-seq 重排缓冲。

**收益/体量**：把差异化从「能演示」推到「能扛群组量」。体量 **XL、需独立立项**。**边界**：浏览器 / 节点间 ICE/DTLS/SRTP、真实 OBS/ffmpeg 推流本沙箱不可端到端验证（`AGENTS.md` 已注明、旧路线图明令「勿在 CI 内标 done」）——以 staging milestone 为交付门，SFU UDP 回路为脊柱，AV1/H.265 关键帧 + SRT 重排序为媒体真流后的后续正确性任务。

---

## 附：方法与边界

- 本版由若干**只读**子代理并行扫描（投递一致性 / 数据面扇出 / 媒体面 / AI 经济 / 可运维性与多租户公平），每条结论附 `file:line`，跨维度聚类去重、并逐项与第三版交付物核对后成此 5 方向。**未写任何业务代码。**
- 体量记号：S<1d、M≈数日、L≈1–2 周、XL≈需独立立项。
- 测试数与迁移数以 [`README.md`](README.md) / `migrations/` 为准，本文不硬编码。
- **不变的非目标**（承袭 `AGENTS.md`，本版不据此扩展）：E2E 端到端加密客户端、联邦、移动端原生 SDK。
- 标注为 seam 的真实链路（浏览器 / 节点间 ICE/DTLS/SRTP、真实 OBS/ffmpeg、真实 S3、第二节点）本沙箱不可端到端验证——相关项以 staging 联调为交付边界，勿在 CI 内标「done」。
