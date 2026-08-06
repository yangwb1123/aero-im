# Aero IM

AI-Native 即时通讯 + 直播平台,Rust 实现。

- 设计:[`docs/specs/2026-05-22-aero-im-design.md`](docs/specs/2026-05-22-aero-im-design.md)
- 状态：功能矩阵中的服务端、Web SPA、协议栈与可靠性链路均已落地，并由 workspace 单测、PG 门控集成测试和静态门禁持续校验。持久 call-leg generation 已在一次性数据库由当前二进制应用；真实 Chrome 双客户端在两个本机独立 gateway 间连续通过 call-bridge 双向 RTP、late-subscriber、跨 gateway 重连和旧连接延迟清理验收，真实旧 v3 / 当前 v4 二进制混跑也通过双向音视频与协议降级验收。Chrome WHEP 播放、Firefox SFU 双向音视频、本机 coturn 强制 relay-only、真实 OBS RTMP 和 ffmpeg RTMP / WHIP / 加密 SRT（含 post-handshake SEK 轮换）均已通过。本机 MinIO S3、Mailpit SMTP、mock OIDC（RS256 / JWKS）、Jaeger OTLP trace、ClamAV、OTel collector metrics 同样已通过。仍待跨主机 / 公网 NAT / 防火墙 / 公网 TURN、物理设备、Safari，以及真实外部 S3/KMS、FCM/APNs、SMTP/OIDC/OTLP 等供应商凭据往返；本机或 mock 结果不得外推为生产服务商验收。SAML 是单独的 fail-closed 安全 seam：默认 ACS 不接受 assertion，不能归入“仅缺真实凭据”。

## 功能矩阵

| 模块 | 能力 | 状态 |
|---|---|---|
| **IM 文本** | 注册 / 登录 / 房间 / 历史 / WS 实时；REST/WS 发送共用校验与幂等语义 | ✅ |
| **AI-Ready 消息块** | text / mention / code / file / voice / card / tool_call / thought | ✅ |
| **协作** | 编辑 / 删除 / 反应 / 已读回执 / typing | ✅ |
| **附件** | 房间范围上传绑定不可变 workspace/region；LocalFs、S3/MinIO（SigV4/SSE-KMS）与 Snaplink `client_credentials` 鉴权的 Aero Vault `BlobStore`；引用校验、保留/完成和带意图的 GC | ✅ |
| **RAG 搜索** | FTS / 向量(pgvector) / hybrid 模式 | ✅ |
| **AI** | Anthropic Messages(`claude-sonnet-4-6`)、Voyage 嵌入、AI 摘要、RAG 问答、`ai_jobs` 工作队列 | ✅ |
| **Agent in channel** | `@bot` 自动回复(基于 RAG)；`/api/agents` 必须显式绑定房间，仅当前 room manager 可创建，participant + workspace/room 成员边同事务提交后发送 `MemberAdded`；direct/标记 group DM 零写拒绝 | ✅ |
| **Bot 开放平台** | `/api/bots` 的 workspace-scoped 创建把 participant、workspace membership、Bot registry 与 token hash 原子提交；无 `workspace_id` 时仍可创建个人/system Bot。`migrations/*_bot_workspace_membership_backfill.sql` 将存量 live scoped Bot 修为普通 member，且不降级已有更高角色 | ✅ |
| **内容审核** | `AERO_BLOCKED_WORDS` 关键词预审 + AI 异步审核(`AERO_AI_MODERATION`) + 工作区 AutoMod（统一覆盖客户端可见结构化字段、每租户 100 条原子配额、发送/编辑 fail-closed） | ✅ |
| **1:1 通话** | WebRTC P2P(浏览器原生),信令走 NATS | ✅ |
| **群通话编排** | 通话 roster、邀请/接听/结束、漏接检测与 legacy mesh 信令兼容；持久 call-leg generation 为每次重连分配单调代际，PG 与 Redis route/roster 的 generation CAS、WS/SFU 事件和精确清理共同阻止旧 socket/心跳/leave 污染新腿；`call_legacy_caller_reconnect_compat` 迁移在不放宽伪造 caller 的前提下兼容旧 v3 initiator reconnect upsert；浏览器群通话媒体使用下方 `call_sfu_v2` 路径 | ✅ |
| **实时字幕翻译** | 浏览器语音识别 → 字幕,最终行经 Anthropic 翻译 | ✅ |
| **直播 RTMP→HLS** | rml_rtmp 摄入,真 MPEG-TS muxing(SPS/PPS/ADTS)；真实 ffmpeg RTMP 摄入已通过 | ✅ |
| **直播弹幕 + 礼物** | 弹幕轨道 + 礼物目录/飘屏/榜单 + 实时观看人数(NATS `live.stream.*`) | ✅ |
| **直播治理 / 申诉** | 主播/当前 moderator 的封禁、timeout 与 raid 操作在 stream 事务围栏下复检权限；申诉绑定具体 ban revision，旧申诉不能解除后续重新建立的封禁 | ✅ |
| **WHIP/WHEP** | WHIP:str0m SDP 应答 + 事件循环 + H.264 解包(RFC 6184)+ 重排序缓冲 **→ 真 HLS**(合成 RTP→.ts/.m3u8 字节级集成测试)，真实 ffmpeg WHIP 摄入已通过。WHEP:`WhepSession` sendonly SDP 应答 + H.264→RTP 打包(FU-A/STAP-A)，按远端 media MID 选择协商后的 H.264 PT；真实 Chrome 已完成 ICE/DTLS/SRTP、解码帧持续增长和资源 DELETE 验收 | ✅ 媒体面 |
| **SFU** | 浏览器 `call_sfu_v2` + 服务端 str0m media session；多发布者 MID 隔离、协商 revision 与 durable call-leg generation、选择性转发、seq/ts 重映射、Simulcast、PLI/FIR/REMB。首个 offer 预留 7 对 recvonly 槽，覆盖服务端默认 8 人群规模并避开 Firefox 动态追加 bundled m-line 陷阱；超池仍有 ICE-restart 扩容回退。真实 Chrome 双 gateway 已通过 late-subscriber、跨 gateway 重连和旧连接清理；真实 Firefox 已通过双向音视频；本机 coturn 已通过强制 relay-only；真实 v3/v4 二进制混跑已通过双向媒体和 wire-version 降级。跨主机/公网 NAT、防火墙、物理设备、Safari 与超出默认群规模的 Firefox 实机扩容仍待 | ✅ 媒体面 |
| **SRT 摄入** | HSv5 握手 + 包编解码 + StreamID 解码 + **AES-128-CTR（KMREQ/KMRSP、RFC 3394 密钥包裹、even/odd 双槽 SEK 与 post-handshake 周期轮换、enforced-encryption）+ ACK/NAK 可靠性 + 控制包序列化** → `MpegTsSegmenter`(TS→HLS,关键帧切片)+ 时限 TURN 凭据；真实 libsrt/ffmpeg 加密推流已观测多次服务端轮换且 HLS 连续，真实 OBS RTMP 摄入亦已通过 | ✅ |
| **MLS E2E** | KeyPackage + 群状态服务端透传 | ✅ scaffold |
| **协作核心** | 线程回复 / @提及通知 / 广播提及(@channel·@here·@everyone) / 线程订阅 / 未读计数 / 标记全部已读 / 置顶 / 反应 / 已读 / typing / 草稿 / 转发 / 斜杠命令（含可选真实 GIPHY 搜索）/ 文件标签 | ✅ |
| **关注 / 社交图** | 关注创作者(following·followers, Wave 11) | ✅ |
| **私聊 / DM** | 1:1 私聊找回或新建 / 多人群聊 group DM(find-or-create, Wave 12-13) | ✅ |
| **频道加入** | 私有频道加入申请 → 创建者/管理员批准·拒绝(Wave 13) | ✅ |
| **合规导出** | 工作区导出/删除 / 单会话消息导出(Wave 13) | ✅ |
| **消息调度** | 定时发送(一次性) / 周期消息(hourly·daily·weekly, Wave 12) / 提醒 | ✅ |
| **AI 协作** | 房间摘要 / RAG 问答 / @bot / 翻译 / **"帮我补课"未读摘要(catch-up, Wave 12)** | ✅ |
| **互动细节** | 谁点了表情(reaction detail, Wave 12) / 默认频道自动加入(Wave 12) | ✅ |
| **频道治理** | 公开·私有 / 加入·退出 / 归档 / topic / 发言策略(公告频道) / 侧边栏分组 / 收藏 | ✅ |
| **用户组 @-usergroups** | 工作区命名成员集，`@handle` 提及扇出到全组(Wave 10) | ✅ |
| **个性化** | 自定义状态+presence / 资料字段(title·pronouns·tz·phone, Wave 10) / 自定义表情 / 收藏 / 通知偏好(频道静音+DND) / 关键词提醒(Wave 10) | ✅ |
| **消息生命周期** | 定时发送 / 提醒(消息锚定) / 编辑历史(Wave 10) / 留存策略 / 链接预览(unfurl) | ✅ |
| **消息可靠性** | 创建/编辑/删除按消息聚合版本写入事务 outbox，严格顺序发布；通知/AI 等后置工作走 durable side-effect jobs；外发 consumer 以 `(consumer,event_id)` receipt 防重；回复及定时/草稿回复由数据库约束固定在同一房间 | ✅ |
| **事务 / 资源围栏** | 定时与周期消息、MLS 不透明中继、关键词提醒、预测、置顶、直播治理、目标和预约直播均在数据库提交点复检 actor、租户/房间/stream 归属、生命周期与配额，raw SQL 不能绕过关键边界 | ✅ |
| **离线投递** | 每房事务分配 `delivery_ordinal`；服务端按 `(participant,room)` 持久游标，完整分页回放后发 `delivery_ready` barrier，客户端应用成功后再单调 ACK | ✅ |
| **搜索** | 房间内 + 跨房间成员边界 + 高级操作符；高级搜索签发短期 impression 作为点击反馈证明；保存搜索支持有界后台监控、首次启用基线、复合游标与幂等通知 | ✅ |
| **会话** | JWT + 稳定 `sid` 会话行 + 原位刷新令牌轮换 + 即时单会话/全局吊销；无 `sid` access JWT fail-closed | ✅ |
| **频道角色** | 查看成员角色 / 改角色 / 转让频道所有权(owner-only, Wave 15) | ✅ |
| **投票 / 公告** | 房间内投票(实时计票) / 工作区公告横幅(Wave 10) | ✅ |
| **企业接入** | 多租户·RBAC / 审计 / SSO(OIDC) / SCIM 2.0（仅存 token hash、每租户 100 条并发安全总记录配额、有界 inventory）/ 外部身份别名迁移与退役 tombstone / Snaplink 机器安装和幂等通知 / PAT / **2FA·TOTP(Wave 14)** / Webhook(入·出) / 邀请 / 数据导出·删除 / 访客账号 / **成员停用(Wave 14)** | ✅ |
| **企业数据治理** | workspace 区域 Blob 路由；S3 SSE-KMS；审计追加接口、legal-hold retention 豁免与 HMAC 签名导出 | ✅（本机 MinIO S3 已验；真实外部 S3/KMS 未验） |
| **企业管理前台** | Web SPA 治理、安全、合规中心覆盖审计/Bot、会话/2FA/区域/IP allowlist/SCIM/AutoMod/IdP、留存/保全/隔离墙/成员生命周期/邀请/Webhook；后端始终复检当前角色与资源归属 | ✅ |
| **生产力** | 消息模板/常用语(canned responses, Wave 14) | ✅ |
| **可观测 / 治理** | Prometheus 指标 / OTLP 链路 / liveness·readiness / 限流 / 按租户 AI 预算 / 死信 | ✅ |
| **频道画布 / 文档** | 每频道多文档 Canvas；不可变有序 op log、快照 `snapshot_op_seq` 基线、`client_op_id` 幂等重试、断线补齐与 Web 编辑器 | ✅ |
| **频道书签** | 频道头部固定链接/资源(标题·URL·emoji·排序,区别于消息置顶与个人收藏, Wave 16) | ✅ |
| **直播分类 / 发现** | 直播分类(Gaming/Music/…)+ 标签 + 按分类浏览在播流(创作者归类, Wave 16) | ✅ |
| **创作者订阅** | 会员档位(名称·月费·权益)+ 订阅/退订 + 双向列表(补足一次性礼物的周期性支持, Wave 16) | ✅ |
| **工作区分析** | 管理员聚合统计:总消息/近 7 日/房间数/成员数/活跃成员 + 热门频道 + 每日时间线(Wave 16) | ✅ |
| **成员目录** | 工作区可搜索成员目录(姓名/职衔过滤,带 title·pronouns·timezone, Wave 16) | ✅ |
| **缺勤自动回复** | 离开办公室状态(OOO)+ 后台 bot 在 1:1 私聊里对每个发送者自动回复一次(Wave 17) | ✅ |
| **组织架构** | 汇报关系(经理/直属下属/汇报链,含环路保护, Wave 17) | ✅ |
| **法务保全** | 法律保全/留存豁免:管理员对房间或工作区下保全令,留存清扫跳过被保全消息(eDiscovery, Wave 17) | ✅ |
| **任务 / 待办** | 房间内可指派、带截止日与状态的任务；AI 行动项可仅提取，也可在 `persist=true` + `Idempotency-Key` 下原子持久化批次，重放返回原任务 ID，空结果也有 durable receipt；账户擦除保留团队任务，仅解绑批次元数据并删除私有 receipt/digest | ✅ |
| **工作区文件浏览** | 跨成员所在全部房间聚合附件浏览(成员边界,Wave 17) | ✅ |
| **审批流** | 工作区审批(Lark 审批-lite):请求人 → 单一审批人 批准/拒绝 + 备注(Wave 17) | ✅ |
| **通话记录** | 每会话通话日志(read-only,复用已持久化的 call_sessions, Wave 18) | ✅ |
| **推流密钥轮换** | 主播一键重置泄露的 stream key,无需重建直播(Wave 18) | ✅ |
| **AI 写作助手** | 改写/调语气/精简/扩写(rewrite,复用翻译后端 seam, Wave 18) | ✅ |
| **直播切片 clips** | 观众标记 [start,end] 时间段分享(复用 HLS 播放列表客户端 seek, Wave 18) | ✅ |
| **主播数据面板** | 单场聚合:礼物数/收入·弹幕数·独立发言人·时长(Wave 18) | ✅ |
| **标记未读** | 把已读游标回拨,使房间重新标红(mark-as-unread 三方常见三连之一, Wave 18) | ✅ |
| **按频道留存** | 每频道留存覆盖:房间 retention_days 优先于工作区默认(清扫用 COALESCE,与法务保全叠加, Wave 19) | ✅ |
| **信息隔离墙** | 受限用户组对(Purview ethical walls)：新建 DM/群 DM 前校验，并在消息创建/编辑事务内按当前房间成员实时复检；策略或用户组成员变更后，既有私聊/共享房间也立即阻断跨屏障通信 | ✅ |
| **通知暂停** | 一次性 snooze 到指定时刻(区别于周期 DND, Wave 19) | ✅ |
| **工作区级 RAG 问答** | 跨全部所在频道提问(成员边界向量检索 + LLM,旗舰企业 AI, Wave 19) | ✅ |
| **表情反应通知** | 他人给你的消息加表情 → 收件箱持久通知(不自我通知、撤销不通知,沿用静音/DND/snooze 门控, Wave 20) | ✅ |
| **会话/设备管理** | 活跃登录会话清单 + 撤销单个 / "登出其它所有设备"(与刷新令牌吊销联动, Wave 21) | ✅ |
| **动态 feed / 开播通知** | 通用每用户动态 feed；关注的创作者开播 → 持久通知；`golive_bot` 按 `(follower,stream)` 幂等，NATS 重投不重复插入 | ✅ |
| **未接来电通知** | 通话无人接听即结束 → 被叫的动态 feed 收到"未接来电"(已接听则不记, Wave 22) | ✅ |
| **通话纪要 / AI 复盘** | 持久化最终字幕行,通话结束自动 AI 复盘(无 key 退化为启发式摘要);`/api/calls/:id/{transcript,recap}` 房间内可读(Wave 23) | ✅ |
| **工作区强制 2FA** | 管理员开启后,未启用 TOTP 的成员被 `assert_room_access` 挡在该工作区房间数据外,直至完成 enroll(`/api/me/2fa` 不受此门控, Wave 24)；提交期数据库围栏同时保证每个活跃工作区始终保有至少一名未停用、非访客且满足强制 2FA 的有效 Owner，跨行降级/TOTP 撤销也不能绕过 | ✅ |
| **测试基线** | `cargo test --workspace --lib`；PG 门控测试使用全新已迁移数据库；另有 truth/file-size/web/authz 门禁与运行时 smoke | ✅ |

## 架构

```
┌─ 客户端(Web ES2020 SPA)── HTTP REST + WebSocket + WebRTC ─────┐
│                                                                │
└──┬─────────────────────────────────────────────────────────────┘
   │
┌──▼ aero-server(Axum 0.7)
│  ├ HTTP API           /api/* (auth/me/rooms/messages/reactions/...)
│  ├ /ws                WS protocol(消息 + 通话信令 + typing 等)
│  ├ /whip/* /whep/*    SDP HTTP 信令
│  ├ /hls/*             HLS 静态分发
│  ├ /api/blobs         个人/迁移兼容上传（不可用于新消息附件）
│  ├ /api/rooms/:id/blobs  房间鉴权 + workspace 区域附件上传
│  ├ /api/ai/{summarize,ask}  AI Gateway
│  ├ /api/mls/*         MLS 不透明字节中继
│  └ RTMP :1935 (TCP)   推流入口
│
├─ Postgres 17 + pgvector + pg_trgm     (primary + 可选显式 eventual read replica)
├─ Redis 7                              (会话 / presence·viewer 256 路分片 / 缓存)
├─ NATS JetStream                       (房间/直播事件与跨实例实时投递)
├─ MinIO / S3 或 LocalFs                (附件 BlobStore)
└─ Jaeger / OTLP collector              (可观测链路)
```

读副本由可选 `database.replica_url`（环境变量
`AERO__DATABASE__REPLICA_URL`）启用。`QueryRouter` 只把已在 primary 完成
`assert_room_access` 的单房间陈旧容忍读路由到 replica。历史接口仅在 primary
证明 `before` 后确有更新可见消息时，才把该页视作旧页；未来 ULID 或其它不能
证明为旧页的 cursor 仍走 primary。消息上下文默认 `strong`，只有显式
`consistency=eventual` 才可走 replica。鉴权、安全状态、跨房查询、最新页、
重连追平、变更回放和写后读取始终走 primary；副本未配置、启动连接失败或
一次 eventual 查询失败时均回退 primary。

`messages_partitioned` shadow、增量 backfill 和人工 cutover runbook 已就绪，
但生产表交换会重写复合主键及入站外键，只能在获批维护窗由 DBA 执行；它不是
普通部署迁移，也不能把“预构建完成”写成“生产切换完成”。

### Cargo workspace
- **aero-common** — 共享类型(IDs / Block / RoomEvent / CallEvent / Stream / MLS / Error / Config / telemetry)
- **aero-bus** — NATS JetStream EventBus trait + 实现
- **aero-storage** — sqlx 仓储:Participant / Room / Message / Receipt / Reaction / Blob / BlobStore / Call / AiJob / Stream / Cache / Presence / MLS
- **aero-auth** — Argon2id + RS256 JWT + Axum extractor
- **aero-signaling** — WebRTC IceServer / RtcConfig / SDP+ICE 校验 / CallRoster
- **aero-im-core** — ImService(消息/编辑/删除/反应/已读/通话编排)+ KeywordModerator
- **aero-im-call** — 通话编排:`CallOrchestrator`(start/answer/end/join/leave + `SfuRouter` 对等管理 + 漏接检测)
- **aero-ai** — Anthropic / Voyage / HashEmbedder / AiService / AiWorker
- **aero-live-core** — LiveStreamConfig + LiveIngest trait + IngestEvent
- **aero-live-hls** — HlsWriter(滚动 m3u8)+ FlvToTsConverter(真 MPEG-TS)
- **aero-live-rtmp** — rml_rtmp 摄入器
- **aero-live-whip** — WHIP/WHEP:str0m 媒体终结(SDP 应答 + 事件循环 + H.264 解包/打包 + 重排序缓冲 + `WhepSession` egress)
- **aero-live-webrtc** — SFU:str0m 选择性转发 + RTP 重映射 + Simulcast 分层选择 + RTCP PLI/FIR + H.264 关键帧检测(SfuRouter/SfuPeer/SfuForwarder)
- **aero-live-srt** — SRT HSv5 握手 + AES-128-CTR（KMREQ/KMRSP、even/odd SEK 周期轮换、重放/冲突/错密/明密降级拒绝）+ ACK/NAK 可靠性 + 控制包编码 + MpegTsSegmenter(TS→HLS)+ 时限 TURN 凭据
- **aero-push** — FCM/APNs 服务端推送网关 + token-provider seam
- **aero-server** — Axum gateway + ai_adapter + agent_bot + hub
- **aero-eng / aero-cli** — 工程命令框架与 `aero-eng` CLI
- **web/** — 依赖零的 ES2020 SPA

## 快速开始

```bash
# 1. 起开发环境(Postgres / Redis / NATS / Jaeger / MinIO)
make up

# 2. 生成 JWT 密钥(PKCS#1 PEM)+ 配置
make jwt-keys env

# 3. 先编译再跑全部迁移（迁移在编译期嵌入 aero-cli）
cargo build --bin aero-cli
cargo run --bin aero-cli -- migrate

# 4. 启动服务（端口以 config/env 为准；示例 env=8080，仓库本地 config/smoke=3030）
cargo run --bin aero-server

# 5. 浏览器
open http://localhost:3030
```

### 可选环境变量

| 名字 | 作用 |
|---|---|
| `ANTHROPIC_API_KEY` | 启用真实 LLM 摘要 / 问答 / 字幕翻译 / 内容审核(否则启发式 fallback) |
| `ANTHROPIC_MODEL` | 默认 `claude-sonnet-4-6` |
| `VOYAGE_API_KEY` | 启用 Voyage embeddings(1024 维);否则用确定性 HashEmbedder |
| `AERO_BLOCKED_WORDS` | 逗号分隔,关键词审核(同步预审) |
| `AERO_AI_MODERATION` | 置位后启用 AI 异步内容审核(经有界预算化队列;需 `ANTHROPIC_API_KEY`) |
| `AERO_AI_MODERATION_QUEUE` / `_CONCURRENCY` | 审核队列容量(默认 512)/ 工作者数(默认 2) |
| `AERO_AI_MODERATION_MAX_PER_WINDOW` / `_PER_WS_WINDOW` / `_WINDOW_SECS` | 审核全局/单租户预算窗口(默认 300 / 60 / 60s;超额跳过并计数) |
| `AERO_GIPHY_API_KEY` | 可选；启用服务端 `/giphy <query>` 真实搜索，密钥不会下发浏览器或写入消息 |
| `AERO_GIPHY_RATING` | GIPHY 内容等级：`g` / `pg` / `pg-13`，默认及无效值均收敛到 `pg` |
| `AERO_GIPHY_RATE_PER_MIN` / `_BURST` | 每参与者的集群级 GIPHY provider 分钟配额（默认 10 次）及单节点突发上限（默认 3）；出网前还会消费集群级 workspace 发送预算并原子预留 slow-mode |
| `AERO_WS_RATE_STANDARD_PER_MIN` / `AERO_WS_RATE_PREMIUM_PER_MIN` | 每工作区限流档位上限(默认 1200 / 6000;`unlimited` 档不限);Redis 故障 fail-open 并计数 |
| `AERO_SRT_MAX_BANDWIDTH_BYTES_PER_SEC` | SRT 发送带宽上限(默认 1.5 MB/s;RTT/NAK 自适应降速) |
| `AERO_SRT_PASSPHRASE` | 可选 SRT AES-128-CTR 强制加密口令（libsrt/FFmpeg 兼容长度 10..64 bytes） |
| `AERO_STUN_URLS` | STUN(默认 stun.l.google.com:19302) |
| `AERO_TURN_URL` / `AERO_TURN_USERNAME` / `AERO_TURN_PASSWORD` | TURN 凭据(浏览器 ICE 兜底) |
| `AERO_ICE_TRANSPORT_POLICY` | 浏览器 ICE 策略：`all`（默认）或 `relay`；`relay` 仅应与可达且带凭据的 TURN 一起使用 |
| `AERO_TURN_SHARED_SECRET` / `AERO_TURN_REALM` / `AERO_TURN_EXTERNAL_IP` | coturn 配置 |
| `AERO_INGEST_HOST` / `AERO_INGEST_UDP_PORT` | WHIP SDP 候选地址 |
| `AERO_PUBLIC_BASE_URL` | 渲染 RTMP / WHIP ingest URL |
| `AERO__SERVER__EVENT_OUTBOX_POLL_MS` / `AERO__SERVER__MESSAGE_SIDE_EFFECT_POLL_MS` | 消息聚合事件与 durable 后置工作的轮询间隔；`0` 禁用该实例上的 relay |
| `AERO__SERVER__CONSUMER_RECEIPT_RETENTION_DAYS` | 已完成 consumer receipt 的保留期；仍有 pending producer outbox 时不会清除 |
| `AERO__SERVER__PASSWORD_RESET_RETENTION_DAYS` | 已使用或已过期 password-reset 记录的审计保留期（默认 7 天，`0` 禁用）；尚有效的未使用 token 不会按年龄清除 |
| `AERO_BOT_DELIVERY_RETENTION_DAYS` / `AERO_BOT_DELIVERY_DLQ_RETENTION_DAYS` | Bot webhook 已成功 outbox 保留期（默认 30 天）/ DLQ 与逐次 attempt 历史运维窗口（默认 90 天）；`pending`/`failed` 不参与按龄清扫 |
| `AERO_BOT_DELIVERY_SWEEP_SECS` | Bot durable delivery 留存清扫周期（默认 3600 秒，限制为 60–86400 秒） |
| `AERO_NOTIFICATION_BUNDLES` / `AERO_NOTIFICATION_BUNDLE_FLUSH_SECS` | 可选回复通知聚合及刷新周期 |
| `AERO_WEBHOOK_GLOBAL_CONCURRENCY` / `AERO_WEBHOOK_ENDPOINT_CONCURRENCY` | Webhook 全局与单 endpoint 并发上限 |
| `AERO_TRUSTED_PROXY_CIDRS` | 可信反向代理 CIDR；默认空，直连请求的 `X-Forwarded-For`/`X-Real-IP` 不作为授权或限流依据 |
| `AERO_INSTANCE_ID` | 每个 Aero IM 实例唯一且跨重启稳定的标识；据此派生独立 JetStream 实时扇出 cursor，多实例不得复用同一值 |
| `AERO_SFU_BIND_ADDR` / `AERO_SFU_ADVERTISE_HOST` | SFU UDP 绑定地址与对客户端公布的可达地址 |
| `AERO_BRIDGE_ADVERTISE_HOST` / `AERO_INTERNAL_BRIDGE_SECRET` | 跨节点 call-bridge 可达地址与内部 subscribe/feedback 共享密钥 |
| `AERO_CALL_ROUTE_V2_ONLY` | Call-route Redis 两阶段升级开关，默认 `false`：R1 对旧 hash 双写/兼容读；仅在所有节点都已运行 R1 后才全量设 `true`，R2 将停写并清除旧 hash |
| `AERO_BLOB_BACKEND` / `AERO_S3_*` / `AERO_S3_KMS_KEY_ID` | 显式选择 S3 及完整 SigV4 配置；可选 KMS key 使 PUT 携带并签名 SSE-KMS headers，配置不完整或不安全时启动失败 |
| `AERO_VAULT_*` | `AERO_BLOB_BACKEND=vault` 时将附件字节写入 Aero Vault；支持 Snaplink client_credentials 短期机器令牌、稳定 BlobId 幂等上传、Range 下载与 GC 硬删除，配置不完整时启动失败 |
| `AERO_DEPENDENCY_BIND_HOST` | Docker Compose 中 PostgreSQL/Redis/NATS/Jaeger/MinIO 的宿主机绑定地址，安全默认 `127.0.0.1`；仅在防火墙与认证私网齐备时覆盖 |
| `AERO__INTEGRATIONS__ISSUER` / `AERO__INTEGRATIONS__AUDIENCE` / `AERO__INTEGRATIONS__JWKS_URI` | 外部 ERP/worker 的 Snaplink RFC 9068 access-token 信任边界；issuer/JWKS 可回落 OIDC 配置，API audience 必须显式配置，token 须含 `aero.notify.publish` scope |
| `AERO_SNAPLINK_COMMERCIAL_*` | 可选的 Snaplink Billing Entitlement 投影、月度消息/通知计量和 Audit Governance durable relay；按工作区 client_credentials/source binding，细节见 `docs/snaplink-commercial.md` |
| `AERO_AUDIT_SIGNING_KEY` | 可选 HMAC-SHA256 审计 CSV 签名；响应通过 `x-audit-signature` 暴露防篡改校验值 |
| `AERO_AGENTIC_ANSWERS` | 置位后 AI 问答走「能动」工具循环(模型自驱动房间检索 search→refine→answer);默认关闭(单次检索更省) |
| `AERO_LOGIN_LOCKOUT` | 置位后启用按账户登录失败锁定(默认关闭;与 per-IP 限流 + 2FA 叠加) |
| `AERO_LOGIN_LOCKOUT_MAX_FAILURES` / `_WINDOW_SECS` / `_SECS` | 锁定阈值/窗口/锁定时长(默认 5 次 / 300s 窗口 / 900s 锁定) |
| `AERO_PER_TENANT_METRICS` | 置位后 `aero_messages_sent_total` 额外发一条 `{workspace}` 标签序列(每租户速率);默认关闭(标签基数随租户数增长,需运维自行权衡) |

## 体验脚本

```bash
# 启动后:
bash scripts/smoke.sh http://localhost:3030      # P1 happy path
python3 scripts/ws_smoke.py                       # WS fan-out(2 客户端互发)
python3 scripts/smoke_p2.py                       # P2 全功能:edit/react/read/search/blob/AI/stream/MLS
python3 scripts/smoke_live.py                     # P11 弹幕/礼物/观看人数 WS 扇出 + 榜单 + 主播下播
python3 scripts/smoke_captions.py                 # P3 字幕中继:call_caption → 房间 call/op:caption 扇出
python3 scripts/smoke_roadmap3_wave_c.py          # ROADMAP3:事件 seq/去重、回填截断续拉、多端已读、异步通知、租户限流档位、删除审计事务、重排检索
python3 scripts/media_ingest_smoke.py --help      # 真实 ffmpeg RTMP/WHIP/SRT；可验证加密 SRT 服务端 SEK 轮换
python3 scripts/whep_browser_smoke.py --help      # 真实 Chrome WHEP ICE/DTLS/SRTP/H.264 解码
python3 scripts/sfu_browser_smoke.py --help       # 真实 Chrome SFU/双 gateway/重连/强制 TURN relay
python3 scripts/sfu_firefox_smoke.py --help       # 真实 Firefox SFU 双向媒体
python3 scripts/obs_ingest_smoke.py --help        # Xvfb 下真实 OBS Studio RTMP→HLS
```

> 本地运行时验证（2026-07-29，隔离 disposable PostgreSQL + Redis + NATS +
> LocalFs）已覆盖注册/房间/消息、P2 room-scoped Agent 原子安装、离线投递游标、
> Canvas、企业治理、线程、直播/字幕、Bot SDK 及关键 IDOR/authz 路径。代表脚本
> `smoke.sh`、`smoke_p2.py`、`smoke_delivery_cursor.py`、`smoke_canvas_ops.py`、
> `smoke_enterprise.py`、`smoke_bot_sdk.py`、`smoke_live.py` 和
> `smoke_captions.py` 均通过；专项配置下的 moderation/rate-tier 与真实 loopback
> source IP allowlist smoke 也通过。当前二进制在一次性库应用
> call-leg generation 迁移后，又以真实 Chrome 双客户端通过了两个本机
> 独立 gateway（不同 NATS durable、共享 PostgreSQL / Redis / NATS）经
> call-bridge 的 late-subscriber 双向 RTP，并通过同参与者跨 gateway 重连、
> 旧连接延迟清理与替代媒体持续增长验收；真实旧 v3 / 当前 v4 二进制混跑也
> 通过双向音视频和协议降级验收。真实 Firefox SFU、Chrome WHEP、本机 coturn
> 强制 relay-only、真实 OBS Studio RTMP，以及 ffmpeg RTMP / WHIP / 加密 SRT
> （含服务端观测到的 post-handshake SEK 轮换）均已通过。本机 MinIO S3、
> Mailpit SMTP、mock OIDC（RS256 / JWKS）、Jaeger OTLP trace、ClamAV 和 OTel
> collector metrics 也已通过。服务停止后删除对应一次性数据库。以上不覆盖
> 跨主机 / 公网 NAT / 防火墙 / 公网 TURN、物理摄像头/麦克风、Safari，也不
> 覆盖真实外部 S3/KMS、FCM/APNs、SMTP/OIDC/OTLP 等供应商凭据往返，不得
> 外推为生产服务商验收。`data/` 由容器以 root
> 创建时，本地服务仍须把附件/HLS 目录改到可写路径：
> `AERO__SERVER__BLOB_DIR=/tmp/aero/blobs AERO__SERVER__HLS_DIR=/tmp/aero/hls`。

## 直播测试

```bash
# 1. 浏览器点 "🔴 开播",取得 ingest URL 和 stream_key
# 2. 用 ffmpeg(或 OBS)推流:
ffmpeg -re -i sample.mp4 -c:v libx264 -c:a aac -f flv rtmp://localhost/live/<stream_key>
# 3. 在另一个 tab 点 "📺 直播大厅",用 hls.js 验证播放（Safari 仍待单独 staging 验收）
```

## API 速查

### 鉴权
- `POST /api/auth/register` `POST /api/auth/login` `GET /api/me`

### 房间 / 消息
- `POST /api/rooms` `GET /api/rooms`
- `POST /api/rooms/:id/members` `GET /api/rooms/:id/members/list`
- `GET /api/rooms/:id/messages` `POST /api/rooms/:id/messages` `POST /api/rooms/:id/read` `GET /api/rooms/:id/receipts`
- `POST /api/rooms/:id/messages` 支持 `Idempotency-Key: <uuid>` 或 body `client_message_id`；首次创建返回 201，重放返回 200 + `idempotency-replayed: true`
- `POST /api/rooms/:id/search` `body: {query, mode, limit}` — mode: `fts` / `vector` / `hybrid` / `auto`
- `PATCH /api/messages/:id` `DELETE /api/messages/:id` `POST /api/messages/:id/reactions`
- `POST /api/messages/reactions` `body: {message_ids}`

### 附件
- `POST /api/rooms/:id/blobs`(multipart `file`)→ `{id, kind, workspace_id, storage_region, residency_scoped:true}`；服务端从已鉴权房间解析 workspace，客户端不能自报区域
- `POST /api/blobs`(multipart `file`)→ 仅供个人数据/旧迁移兼容；返回 `residency_scoped:false, message_attachment_eligible:false`，该未限定 workspace 的 blob 不能附加到新消息
- `GET /api/blobs/:id` → 原字节
- `GET/PUT /api/workspaces/:id/storage-region` → 查看/设置未来附件区域（PUT 仅 Owner/Admin；只接受 `default` 或 `[storage_regions]` 已配置键）

### Snaplink 项目集成
- 管理员：`POST/GET /api/workspaces/:workspace_id/integrations`，`PATCH/DELETE /api/workspaces/:workspace_id/integrations/:installation_id`；可原子轮换 Snaplink `client_id`、发送 Bot、目标房间白名单及启停状态
- 账号迁移：`POST /api/workspaces/:workspace_id/identity-migrations`；当前有效成员只能为自己的 immutable participant 提交短期 Snaplink target ID-token 证明，先增加新 issuer/sub 登录别名，验证后再退役并 tombstone 旧别名；管理员只能通过停用/SCIM 管理成员生命周期，不能凭 issuer/sub 字符串改绑他人的全局身份
- 机器调用：`POST /api/integrations/v1/installations/:installation_id/notifications`；要求 Snaplink `client_credentials` Bearer、`aero.notify.publish` scope 和 UUID `Idempotency-Key`，可投递到白名单房间或 `{type:"snaplink_user",subject}` 的规范 1:1 DM
- 机器附件：`POST /api/integrations/v1/installations/:installation_id/blobs` 须带 UUID `Idempotency-Key`，并从互斥的 `X-Aero-Room-Id` / `X-Aero-Snaplink-Subject` 选择一个目标头上传 multipart `file`，再在通知 `blocks` 中引用返回的 `blob_id`；稳定 subject 不进入 URL，配置 `AERO_BLOB_BACKEND=vault` 时字节落 Aero Vault，Aero IM 仅保留授权与元数据
- 完整接入、重试、账号迁移与多实例伸缩说明见 [`docs/snaplink-integrations.md`](docs/snaplink-integrations.md)
- 商业套餐 feature/额度、usage fact、Audit Governance 和灾备说明见 [`docs/snaplink-commercial.md`](docs/snaplink-commercial.md)

### Participants / Agents
- `GET /api/participants?q=...` `GET /api/participants/:id`
- `POST /api/agents` `body: {room_id, display_name, kind, avatar_url?}` — `kind`: `bot` / `agent`；`room_id` 必填，仅当前 room manager 可创建。服务端从房间解析租户，并在同一事务创建 participant + workspace/room 成员边，提交后发送 `MemberAdded`；direct/标记 group DM 在任何写入前拒绝

### Bot 开放平台
- `POST /api/bots` `body: {name, icon_url?, workspace_id?}` — 提供 `workspace_id` 时，participant、普通 workspace membership、Bot registry 与初始 token hash 在同一事务提交；省略时创建不绑定租户的个人/system Bot
- 存量 live workspace-scoped Bot 由 `migrations/*_bot_workspace_membership_backfill.sql` 补齐普通成员边；guest-shaped 旧边转为 member，已有 Admin/Owner 等更高角色保持不变

### AI
- `POST /api/ai/summarize` `body: {room_id, last_n?}`
- `POST /api/ai/ask` `body: {room_id, question, k?}`
- `POST /api/rooms/:id/action-items?k=...` → 提取行动项；加 `persist=true` 时必须提供 `Idempotency-Key`，并原子返回/重放持久任务 ID

### 搜索反馈 / 保存搜索
- `POST /api/search/advanced` → 返回 `impression_id` 与有序结果；`POST /api/search/click` 只接受 `{impression_id,result_id}`，query/rank 由服务端证明推导
- `POST/GET /api/workspaces/:id/saved-searches` `DELETE /api/saved-searches/:sid` `POST /api/saved-searches/:sid/run`
- `POST /api/saved-searches/:sid/monitor` → 启停有界后台监控；首次启用从当前基线开始，不补发历史命中

### 直播 + RTC
- `POST /api/streams` `body: {title, protocol, room_id?}`
- `GET /api/streams` `GET /api/streams/:id` `POST /api/streams/:id/end`(主播下播)
- `GET /api/rtc/config` → `{ice_servers, ice_transport_policy}`
- `POST /whip/:stream_key` `DELETE /whip/resource/:id` `POST /whep/:stream_id`(body: SDP 文本)

### 直播互动(弹幕 + 礼物)
- `GET /api/live/gifts` → 礼物目录
- `GET /api/streams/:id/chat` `POST /api/streams/:id/chat` `body: {body}`
- `GET /api/streams/:id/gifts` `POST /api/streams/:id/gifts` `body: {gift_id, qty?}`
- `GET /api/streams/:id/leaderboard` → 打赏榜
- `POST/GET /api/streams/:id/appeals` → 当前 ban 的申诉/待审队列；`POST /api/appeals/:id/review` → 主播或当前 moderator 审核

### MLS
- `POST /api/mls/key-packages` `body: {ciphersuite, payload_b64}`
- `GET /api/rooms/:room_id/mls/key-packages/:participant_id` — requester 与 target 均须保有当前房间访问权，单次原子领取
- `GET /api/mls/key-packages/:participant_id` — 兼容端点，仅允许 `participant_id` 等于当前用户
- `POST /api/mls/groups` `body: {group_id_b64, room_id, ciphersuite, epoch, state_b64}` · `GET /api/mls/groups/:gid`

服务端只保存和转发 MLS opaque bytes；房间绑定、当前成员权限、ciphersuite 不变和 epoch 单调性由事务/数据库围栏验证，密码学状态机仍完全位于客户端。

### WebSocket (`Authorization: Bearer <jwt>`；浏览器兼容回退 `/ws?token=<jwt>`)

非浏览器客户端优先使用 `Authorization`，服务端只在该头缺失时读取旧的
`token` query 参数；两条路径均限制为 48 KiB，日志中的凭据始终脱敏。
当前 `aero_oidc_*` Cookie 只绑定 OIDC 登录事务，不是认证会话 Cookie。

客户端帧:`join_room` `send_message` `edit_message` `delete_message` `react` `mark_read` `delivery_ack` `typing` `call_invite` `call_answer` `call_ice` `call_end` `call_caption` `call_join` `call_leave` `call_offer` `call_sfu_offer` `call_sfu_ice` `call_sfu_subscribe` `watch_stream` `unwatch_stream` `stream_chat` `stream_gift` `ping`

服务端帧:`welcome`（含 `delivery_cursor_v2`、`call_sfu_v2` capability）`presence` `message`（含 `delivery_ordinal`）`delivery_ready` `edited` `deleted` `reaction` `read` `typing` `call`(`op`: `invite`/`answer`/`ice`/`end`/`caption`/`join`/`leave`/`roster`/`offer`)`call_sfu_answer` `call_sfu_ice_ack` `call_sfu_renegotiate` `call_sfu_subscribed` `stream_event`(`chat`/`gift`/`viewers`/`status`)`error` `pong`

## 验证

```bash
cargo check --workspace          # 干净
cargo test --workspace --lib     # hermetic 单测；忽略的 DB 集成测试需 DATABASE_URL + 已迁移的新库
cargo build --bin aero-server    # 二进制成功
cargo clippy --workspace --all-targets
scripts/truth-check.sh
scripts/file-size-check.sh
scripts/web-check.sh
```

## 路线图

| 阶段 | 内容 | 状态 |
|---|---|---|
| **P0** | Workspace + 基础设施 | ✅ |
| **P1** | AI-Ready IM MVP | ✅ |
| **P2** | 协作 + RAG + Agent | ✅ |
| **P3** | 1:1 通话 + 实时字幕翻译 | ✅ |
| **P4** | RTMP→HLS(真 TS) + 弹幕 + 礼物 | ✅ |
| **P5** | WHIP/WHEP(str0m 媒体面)+ 关键词审核 + AI 审核 | ✅ |
| **P6** | 群通话(mesh,浏览器原生媒体)+ AI Agent 进频道;SFU str0m 转发 + Simulcast/RTCP | ✅ |
| **P7** | SRT(HSv5 握手 + AES-128-CTR KMREQ/KMRSP、even/odd SEK 周期轮换与强制加密 + ACK/NAK 可靠性 + TS→HLS 切片)+ 时限 TURN 凭据 | ✅ |
| **P8** | MLS E2E | ✅(scaffold;E2E 为 spec 非目标) |
| **P9** | 真 TS muxing / vector 搜 / mention / read avatars / smoke_p2 | ✅ |
| **P11** | 直播弹幕 + 虚拟礼物 + 观看人数 | ✅ |

媒体实现与生产启动路径已经接线：WHIP/WHEP/RTMP/SRT → HLS、浏览器 SFU v2、`SfuMediaSession`、call-bridge UDP RTP，以及 secret-gated subscribe/RTCP feedback 控制面在结构/协议/localhost 测试之外，已通过真实 Chrome WHEP、Firefox/Chrome SFU、OBS RTMP 和 ffmpeg RTMP/WHIP/加密 SRT 摄入。当前 generation-fenced 二进制已在一次性库上通过两个本机独立 gateway 之间的 call-bridge late-subscriber 双向 RTP、同参与者跨 gateway 重连与旧连接延迟清理；真实 v3/v4 二进制混跑也通过双向媒体。coturn 在本机 LAN 地址下完成了 Chrome 强制 relay-only 验收；这些结果仍不等于跨主机、公网 NAT/防火墙或公网 TURN 完成。

call-bridge subscriber 使用 60 秒 lease、每 15 秒认证 refresh、`subscription_id` 代际围栏和 best-effort unsubscribe；绑定帧同时校验 call 与订阅代际，过期/撤销项有硬容量上限。当前 v4 puller 会声明 `wire_version`，owner 可按订阅分别发送 v3/v4 绑定帧，新 puller也能解码上一代 v3 绑定帧。真实旧 v3 / 当前 v4 依赖二进制已完成双向音视频兼容验收；无绑定的 v2 仍只保留“新 owner 服务旧 puller”的单向兼容，新 puller 拒绝 v2 owner 的未绑定帧。发布链中若仍含 v2，必须先 drain/重连活跃跨节点通话，不能宣称无损滚动升级。

本机集成验收还通过了 MinIO S3、Mailpit SMTP、mock OIDC RS256/JWKS、Jaeger OTLP trace、ClamAV 与 OTel collector metrics。以下仍只可写为待真实环境验收：

- 物理摄像头/麦克风设备、Safari，以及超出默认群规模或长时弱网下的浏览器行为。
- 跨主机部署下的路由、可达 UDP 地址、NAT/防火墙、公网 TURN 和跨主机 RTP/RTCP；本机 LAN coturn 强制 relay-only 不能替代这些验收。
- 真实外部 S3/KMS、FCM/APNs、SMTP/OIDC/OTLP 等供应商凭据网络往返；上述本机或 mock 集成结果不得外推为生产服务商验收。
- SAML 元数据/AuthnRequest/JIT 骨架已建；实验 ACS 在进入 verifier 前已把输入约束为单一直接 Assertion/Signature、同文档引用、RSA-SHA256/SHA256 与固定 transform，并对 Recipient/Audience/InResponseTo/时窗及一次性请求状态 fail-closed。默认 ACS 仍不接受 assertion；生产启用仍需接入并安全评审经审计的 XML-DSig verifier。`AERO_SAML_EXPERIMENTAL_VERIFY=1` 仅启用未审计的实验 verifier，不代表生产完成。

MLS 客户端密码学、联邦与原生移动 SDK 是既定非目标，不计入上述 staging 缺口。

---

License: MIT OR Apache-2.0
