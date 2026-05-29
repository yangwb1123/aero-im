# Aero IM — 扩展路线图(ROADMAP)

> 资深架构师 / 产品经理视角的下一步建议。基于 2026-05-29 对当前代码库的一次全局扫描(本轮逐条复核了下文每个 file:line 证据),
> 面向「To-B 协作 + 互动直播 + AI-Native」的产品定位。本文只做规划与论证,不含任何代码。
> 现有交付状态见 [`README.md`](README.md),工程约定见 [`AGENTS.md`](AGENTS.md)。

## 0. 当前地基(已经扎实的部分 —— 路线图是「增量」而非「补课」)

扫描确认这些已做对,后续工作建立在其之上:

- **IM 消息面已可横向扩展**:`crates/aero-server/src/hub.rs` 注释明确「NATS 是跨实例投递的事实源,Hub 仅在本进程内扇出」,每个实例对 `im.room.*` 建独立 durable consumer。多开实例即可水平扩消息吞吐。
- **Presence 跨节点正确**:`crates/aero-storage/src/presence.rs` 是 Redis 支撑的在线态,不依赖单进程内存。
- **语义搜索是生产级**:`migrations/0001_init.sql` 已建 `hnsw (embedding vector_cosine_ops)` ANN 索引(非暴力扫描)+ pg_trgm/FTS。
- **消息历史是 keyset 分页**:`message.rs::list_recent(room, before, limit)` 用游标向前翻,不是 OFFSET。
- **媒体协议栈深度单测**:WHIP/WHEP、SFU(Simulcast/RTCP)、SRT(AES/ACK-NAK)等 394 个单测通过。

→ 真正的高价值缺口集中在 **多租户、媒体面横向扩展、成本/限流治理、可观测性、可靠性硬化** 五处,详述如下。

---

## 方向一:Workspace/Org 多租户 + RBAC 〔核心功能 · To-B 地基〕

**现状/证据**:全仓库 `workspace|tenant|org` 零业务命中;数据模型是「扁平的 `participants` + `rooms`」,迁移里没有租户表。当前任意两个客户的数据生活在同一个无边界的房间空间里。

**为什么需要**:这是「To-B、卖给组织」定位的**前置地基**,缺它则:

- 无法安全接入第二家客户 —— 没有租户隔离,数据、成员、配置全混在一起;
- 无法做企业级必需品:组织管理员、角色权限(owner/admin/member/guest)、邀请流、SSO/SCIM、按组织计费、按组织的留存/合规策略;
- `tenant_id` 必须**尽早**贯穿 schema 与查询(行级隔离)。数据量和并发越大,事后回填租户维度的代价越高 —— 现在做最便宜。

**建议范围**:`Workspace`(=租户)实体 → `Channel`(房间归属 workspace)→ 成员与角色表;所有业务查询按 `workspace_id` 收口(行级隔离 / RLS);邀请与加入流;为后续 SSO/SCIM、按租户配额(见方向三)、按租户留存与数据导出/删除(合规)留接口。**无 E2E 仍是非目标**,但「服务端可见明文」意味着 at-rest 加密 + 严格 authz 是 To-B 合规的替代承诺。

---

## 方向二:集群「正确」的横向扩展 —— 重点是媒体面〔性能 · 规模 · 去 SPOF〕

**现状/证据**:消息面/Presence 已跨节点(见 §0),但**媒体面被钉死在单进程**:

- WHIP 发布者注册表 `s.whip`(`routes.rs`)与 SFU `SfuForwarder` 都是进程内对象;
- `hub.rs` 的 `stream_watchers`、`call_rosters` 是本进程 `DashMap` —— 直播观看人数、群通话 roster 在多节点下是「每节点各算各的」;
- 结果:在节点 A 摄入的流,落到节点 B 的观众/WHEP 拉流者**看不到**;SFU 转发能力被单节点 CPU/出网带宽封顶,且是单点故障。

**为什么需要**:互动直播 + 群通话是产品**差异化卖点**,也是最吃资源的部分。单节点媒体面是硬天花板和 SPOF —— 这恰恰是最该先扩的地方,而不是已经能扩的消息面。

**建议范围**:引入**媒体会话路由层**(哪个节点持有 stream X 的注册中心,Redis/NATS KV)+ 粘性路由或 **SFU 级联/中继**(节点间转发);把 `stream_watchers`/`call_rosters` 的计数与 roster 迁到 Redis/NATS 做集群级聚合;为 WHEP 拉流做「按 stream 定位源节点」的反向代理或中继。

---

## 方向三:成本治理、限流与滥用防护〔边界情况 · 业务风险〕

**现状/证据**:

- `crates/aero-common/src/error.rs` 定义了 `Error::RateLimited`(HTTP 429),但**全仓库没有任何地方实施限流** —— 是个「定义了却没接」的占位;
- 网关(`crates/aero-server/src/bin/aero-server.rs:253-254`)**只挂了 `TraceLayer` + `CorsLayer::permissive()`** —— 缺**全局超时、并发上限、限流、请求体上限**这几层;且 `permissive()` 的 CORS 对 To-B 生产本身就该按域收紧;
- AI worker(`crates/aero-ai/src/worker.rs`)是**串行**处理(`for job in jobs` 逐个 `await`),且 `MAX_ATTEMPTS = 5` 重试,**没有成本预算 / 并发上限 / 按租户配额**;AI 调用的是**付费**的 Anthropic / Voyage API。

**为什么需要**:对 To-B SaaS 这是**生存级**风险:

- **付费 LLM 成本失控**:一次重试风暴(每个 job 最多 5× 付费调用)或一个滥用租户,能在零护栏下刷出天价账单;
- **无限流 = 廉价 DoS/刷量**:WS 连接、发消息、上传、AI 端点都没有速率/配额限制。

**建议范围**:统一的 tower `ServiceBuilder` 栈(超时 + 并发上限 + 按 IP/用户/租户限流 + 请求体上限);AI 侧加并发 `Semaphore` + 按租户 token/成本预算 + 毒丸任务的死信队列(见方向五);WS 帧级节流。**高价值、中等工作量,直接护住成本底线**。

---

## 方向四:可观测性与运维就绪(SRE)〔运维 · 可被运营〕

**现状/证据**:`crates/aero-common/src/telemetry.rs` 明说「OTLP export not yet wired」「deferred to P2」;无 Prometheus/metrics;有 `/health`(超时探活 PG/Redis/NATS)但无 liveness/readiness 区分,也无 RED/USE 指标。

**为什么需要**:实时音视频 SaaS **不能盲飞**。没有指标与链路,故障 MTTR 不可控、容量规划靠猜。需要:真正接通 OTLP 链路;Prometheus 指标(消息吞吐、WS 连接数、AI 延迟/成本/队列深度、媒体码率/丢包、DB 连接池饱和度);规范的 k8s liveness/readiness 探针;SLO + 告警。**中等工作量,运维价值极高**。

---

## 方向五:可靠性与正确性硬化〔边界情况 + 性能 Quick-Wins 集合〕

一组有据可查、单点高价值的硬化项 —— 每条都附了证据位置,适合作为持续 backlog:

| 项 | 证据 | 风险 | 类型 | 建议 |
|---|---|---|---|---|
| **WS 发送队列无界 → OOM** | `hub.rs:19` 用 `UnboundedSender` | 慢/卡住的客户端队列无限增长,内存打爆 | 稳定性 | 改有界 channel + 慢消费者丢弃/断连策略 |
| **Blob 越权下载(IDOR)** | `routes.rs::blob_download(_auth, ...)` —— `_auth` 下划线即「鉴权了但没授权」 | 任意登录用户可凭 ID 下载任意 blob,不校验房间归属 | 安全 | 按 blob 所属房间/拥有者做 authz |
| **断线重连无消息补偿保证** | WS `Welcome` 帧只带 participant,不重放断连期间漏掉的消息(仅 danmaku 有小回放 `ws.rs:404`) | 客户端若不主动拉 REST 历史,会静默丢消息 | 正确性 | join 时支持「since 游标」补偿,或协议层显式约定+强制客户端回填 |
| **Hub 断连是 O(房间数)** | `hub.rs:51-60` 每次 unregister 遍历**所有** rooms/stream_watchers | 断连成本随全局房间数线性增长 | 性能 | 维护 participant→rooms 反向索引 |
| **AI 重试成本放大 + 串行吞吐** | `worker.rs` `MAX_ATTEMPTS=5` + 串行 `for job` | 毒丸任务烧 5× 付费调用;单 worker 串行限吞吐 | 成本/性能 | 死信队列 + 幂等 + 有界并发(与方向三联动) |
| **多节点观看人数/roster 失真** | `hub.rs` 本进程 `DashMap` | 观众分散在多节点时计数各算各的 | 正确性 | 计数/roster 迁 Redis 聚合(与方向二联动) |
| **无全局请求超时/体积上限** | 网关仅 `TraceLayer`+`CorsLayer`(`aero-server.rs:253-254`),无 `TimeoutLayer`/`DefaultBodyLimit` | 慢 handler / 超大请求体占住资源不释放 | 稳定性 | tower `TimeoutLayer` + `DefaultBodyLimit`(与方向三联动) |
| **CORS 全开(`permissive`)** | `aero-server.rs:254` `CorsLayer::permissive()` | 任意源可携带请求访问 API,生产应按部署域/租户白名单收紧 | 安全 | 收紧 `allow-origin`(与方向一/三联动) |

---

## 建议排序(为什么是这个顺序)

1. **方向一(多租户)优先** —— 它改 schema,必须趁数据/规模小的时候把 `tenant_id` 织进去;越晚越贵,且是 To-B 一切的地基。
2. **方向三 + 四(护栏 + 可观测)其次** —— 上线前的「安全带与仪表盘」,中等成本、立刻降低成本与运维风险,且方向五的多数 quick-wins 可顺带做掉。
3. **方向二(媒体面横扩)按需** —— 当真实直播/群通话并发逼近单节点上限时再投入;它最重,但前面三步会让它更可控、可观测。

> 落地原则承袭 [`AGENTS.md`](AGENTS.md):新功能优先沿用「`RoomEvent`/`StreamEvent` → `Hub` 扇出」主轴;依赖只加到自身 crate;凡需真实浏览器/媒体源/绑定 socket 才能验证的,不在沙箱里标「done」。
