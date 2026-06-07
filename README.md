# Aero IM

AI-Native 即时通讯 + 直播平台,Rust 实现。

- 设计:[`docs/specs/2026-05-22-aero-im-design.md`](docs/specs/2026-05-22-aero-im-design.md)
- 状态:**P0–P11 全部就位** + **12 轮 To-B 协作/企业扩展**(48 个功能点,迁移 0001–0042)。媒体面协议栈深化(WHIP/WHEP + SFU 联播/RTCP + SRT 加密/可靠性)已实现并字节级单测(**765 个 hermetic 单元测试通过**,15 个 crate)

## 功能矩阵

| 模块 | 能力 | 状态 |
|---|---|---|
| **IM 文本** | 注册 / 登录 / 房间 / 历史 / WS 实时 | ✅ |
| **AI-Ready 消息块** | text / mention / code / file / voice / card / tool_call / thought | ✅ |
| **协作** | 编辑 / 删除 / 反应 / 已读回执 / typing | ✅ |
| **附件** | 多媒体上传(本地 FS 后端,S3/MinIO 适配口已留) | ✅ |
| **RAG 搜索** | FTS / 向量(pgvector) / hybrid 模式 | ✅ |
| **AI** | Anthropic Messages(`claude-sonnet-4-6`)、Voyage 嵌入、AI 摘要、RAG 问答、`ai_jobs` 工作队列 | ✅ |
| **Agent in channel** | `@bot` 自动回复(基于 RAG) | ✅ |
| **内容审核** | `AERO_BLOCKED_WORDS` 关键词预审 + AI 异步审核(`AERO_AI_MODERATION`) | ✅ |
| **1:1 通话** | WebRTC P2P(浏览器原生),信令走 NATS | ✅ |
| **群通话(mesh)** | 多人全网格 WebRTC,服务端协调 roster + 无眩光配对(小 id 发 offer) | ✅ |
| **实时字幕翻译** | 浏览器语音识别 → 字幕,最终行经 Anthropic 翻译 | ✅ |
| **直播 RTMP→HLS** | rml_rtmp 摄入,真 MPEG-TS muxing(SPS/PPS/ADTS) | ✅ |
| **直播弹幕 + 礼物** | 弹幕轨道 + 礼物目录/飘屏/榜单 + 实时观看人数(NATS `live.stream.*`) | ✅ |
| **WHIP/WHEP** | WHIP:str0m SDP 应答 + 事件循环 + H.264 解包(RFC 6184)+ 重排序缓冲 **→ 真 HLS**(合成 RTP→.ts/.m3u8 字节级集成测试)。WHEP:`WhepSession` sendonly SDP 应答 + H.264→RTP 打包(FU-A/STAP-A);仅余浏览器 ICE/DTLS/SRTP 联调 | ✅ 媒体面 |
| **SFU** | str0m 选择性转发 + 每订阅者 seq/ts 重映射 + **Simulcast 分层选择(关键帧边界切换)+ RTCP PLI/FIR 反馈 + H.264 关键帧检测**;ICE/DTLS 端到端联调待做 | ✅ 媒体面 |
| **SRT 摄入** | HSv5 握手 + 包编解码 + StreamID 解码 + **AES-CTR 加密(KMREQ/KMRSP + RFC 3394 密钥包裹)+ ACK/NAK 可靠性 + 控制包序列化** → `MpegTsSegmenter`(TS→HLS,关键帧切片)+ 时限 TURN 凭据;仅余真实 ffmpeg 推流联调 | ✅ |
| **MLS E2E** | KeyPackage + 群状态服务端透传 | ✅ scaffold |
| **协作核心** | 线程回复 / @提及通知 / 广播提及(@channel·@here·@everyone) / 线程订阅 / 未读计数 / 标记全部已读 / 置顶 / 反应 / 已读 / typing / 草稿 / 转发 / 斜杠命令 / 文件标签 | ✅ |
| **关注 / 社交图** | 关注创作者(following·followers, Wave 11) | ✅ |
| **私聊 / DM** | 1:1 私聊找回或新建(find-or-create, Wave 12) | ✅ |
| **消息调度** | 定时发送(一次性) / 周期消息(hourly·daily·weekly, Wave 12) / 提醒 | ✅ |
| **AI 协作** | 房间摘要 / RAG 问答 / @bot / 翻译 / **"帮我补课"未读摘要(catch-up, Wave 12)** | ✅ |
| **互动细节** | 谁点了表情(reaction detail, Wave 12) / 默认频道自动加入(Wave 12) | ✅ |
| **频道治理** | 公开·私有 / 加入·退出 / 归档 / topic / 发言策略(公告频道) / 侧边栏分组 / 收藏 | ✅ |
| **用户组 @-usergroups** | 工作区命名成员集，`@handle` 提及扇出到全组(Wave 10) | ✅ |
| **个性化** | 自定义状态+presence / 资料字段(title·pronouns·tz·phone, Wave 10) / 自定义表情 / 收藏 / 通知偏好(频道静音+DND) / 关键词提醒(Wave 10) | ✅ |
| **消息生命周期** | 定时发送 / 提醒(消息锚定) / 编辑历史(Wave 10) / 留存策略 / 链接预览(unfurl) | ✅ |
| **搜索** | 房间内 + 跨房间(成员边界) + 保存的搜索 | ✅ |
| **投票 / 公告** | 房间内投票(实时计票) / 工作区公告横幅(Wave 10) | ✅ |
| **企业接入** | 多租户·RBAC / 审计 / SSO(OIDC) / SCIM 2.0 / PAT / Webhook(入·出) / 邀请 / 数据导出·删除 / 访客账号 | ✅ |
| **可观测 / 治理** | Prometheus 指标 / OTLP 链路 / liveness·readiness / 限流 / 按租户 AI 预算 / 死信 | ✅ |

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
│  ├ /api/blobs         附件 multipart
│  ├ /api/ai/{summarize,ask}  AI Gateway
│  ├ /api/mls/*         MLS 不透明字节中继
│  └ RTMP :1935 (TCP)   推流入口
│
├─ Postgres 17 + pgvector + pg_trgm     (业务 + RAG + ai_jobs + MLS state)
├─ Redis 7                              (会话 / presence / 缓存)
├─ NATS JetStream                       (IM_MESSAGES / IM_EVENTS / AI_QUEUE)
├─ MinIO                                (附件 — 已留接口,默认本地 FS)
└─ Jaeger                               (tracing OTLP — 当前 stdout fmt)
```

### Cargo workspace(15 crates)
- **aero-common** — 共享类型(IDs / Block / RoomEvent / CallEvent / Stream / MLS / Error / Config / telemetry)
- **aero-bus** — NATS JetStream EventBus trait + 实现
- **aero-storage** — sqlx 仓储:Participant / Room / Message / Receipt / Reaction / Blob / BlobStore / Call / AiJob / Stream / Cache / Presence / MLS
- **aero-auth** — Argon2id + RS256 JWT + Axum extractor
- **aero-signaling** — WebRTC IceServer / RtcConfig / SDP+ICE 校验 / CallRoster
- **aero-im-core** — ImService(消息/编辑/删除/反应/已读/通话编排)+ KeywordModerator
- **aero-im-call** — (P3+ 占位)
- **aero-ai** — Anthropic / Voyage / HashEmbedder / AiService / AiWorker
- **aero-live-core** — LiveStreamConfig + LiveIngest trait + IngestEvent
- **aero-live-hls** — HlsWriter(滚动 m3u8)+ FlvToTsConverter(真 MPEG-TS)
- **aero-live-rtmp** — rml_rtmp 摄入器
- **aero-live-whip** — WHIP/WHEP:str0m 媒体终结(SDP 应答 + 事件循环 + H.264 解包/打包 + 重排序缓冲 + `WhepSession` egress)
- **aero-live-webrtc** — SFU:str0m 选择性转发 + RTP 重映射 + Simulcast 分层选择 + RTCP PLI/FIR + H.264 关键帧检测(SfuRouter/SfuPeer/SfuForwarder)
- **aero-live-srt** — SRT HSv5 握手 + AES-CTR 加密(KMREQ/KMRSP)+ ACK/NAK 可靠性 + 控制包编码 + MpegTsSegmenter(TS→HLS)+ 时限 TURN 凭据
- **aero-server** — Axum gateway + ai_adapter + agent_bot + hub
- **web/** — 依赖零的 ES2020 SPA

## 快速开始

```bash
# 1. 起开发环境(Postgres / Redis / NATS / Jaeger / MinIO)
make up

# 2. 生成 JWT 密钥(PKCS#1 PEM)+ 配置
make jwt-keys env

# 3. 跑迁移(0001 P1 + 0002 P2 collab + 0003 P8 MLS)
cargo run --bin aero-cli -- migrate

# 4. 启动服务(默认 3030;:1935 RTMP 摄入)
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
| `AERO_AI_MODERATION` | 置位后启用 AI 异步内容审核(每条消息一次 LLM 调用;需 `ANTHROPIC_API_KEY`) |
| `AERO_STUN_URLS` | STUN(默认 stun.l.google.com:19302) |
| `AERO_TURN_URL` / `AERO_TURN_USERNAME` / `AERO_TURN_PASSWORD` | TURN 凭据(浏览器 ICE 兜底) |
| `AERO_TURN_SHARED_SECRET` / `AERO_TURN_REALM` / `AERO_TURN_EXTERNAL_IP` | coturn 配置 |
| `AERO_INGEST_HOST` / `AERO_INGEST_UDP_PORT` | WHIP SDP 候选地址 |
| `AERO_PUBLIC_BASE_URL` | 渲染 RTMP / WHIP ingest URL |

## 体验脚本

```bash
# 启动后:
bash scripts/smoke.sh http://localhost:3030      # P1 happy path
python3 scripts/ws_smoke.py                       # WS fan-out(2 客户端互发)
python3 scripts/smoke_p2.py                       # P2 全功能:edit/react/read/search/blob/AI/stream/MLS
python3 scripts/smoke_live.py                     # P11 弹幕/礼物/观看人数 WS 扇出 + 榜单 + 主播下播
python3 scripts/smoke_captions.py                 # P3 字幕中继:call_caption → 房间 call/op:caption 扇出
```

> 运行时验证(2026-05-24,真实 PG/Redis/NATS):`smoke_p2` / `smoke_live` /
> `smoke_captions` 全部端到端通过。注意 `data/` 由容器以 root 创建,本地起服务需把
> 附件/HLS 目录改到可写路径:`AERO__SERVER__BLOB_DIR=/tmp/aero/blobs AERO__SERVER__HLS_DIR=/tmp/aero/hls`。

## 直播测试

```bash
# 1. 浏览器点 "🔴 开播",取得 ingest URL 和 stream_key
# 2. 用 ffmpeg(或 OBS)推流:
ffmpeg -re -i sample.mp4 -c:v libx264 -c:a aac -f flv rtmp://localhost/live/<stream_key>
# 3. 在另一个 tab 点 "📺 直播大厅",看到 LIVE 卡片,Safari/hls.js 即时播放
```

## API 速查

### 鉴权
- `POST /api/auth/register` `POST /api/auth/login` `GET /api/me`

### 房间 / 消息
- `POST /api/rooms` `GET /api/rooms`
- `POST /api/rooms/:id/members` `GET /api/rooms/:id/members/list`
- `GET /api/rooms/:id/messages` `POST /api/rooms/:id/read` `GET /api/rooms/:id/receipts`
- `POST /api/rooms/:id/search` `body: {query, mode, limit}` — mode: `fts` / `vector` / `hybrid` / `auto`
- `PATCH /api/messages/:id` `DELETE /api/messages/:id` `POST /api/messages/:id/reactions`
- `POST /api/messages/reactions` `body: {message_ids}`

### 附件
- `POST /api/blobs`(multipart `file`)→ `{id, kind, ...}`
- `GET /api/blobs/:id` → 原字节

### Participants / Agents
- `GET /api/participants?q=...` `GET /api/participants/:id`
- `POST /api/agents` `body: {display_name, kind}` — `kind`: `bot` / `agent`

### AI
- `POST /api/ai/summarize` `body: {room_id, last_n?}`
- `POST /api/ai/ask` `body: {room_id, question, k?}`

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

### MLS
- `POST /api/mls/key-packages` `body: {ciphersuite, payload_b64}`
- `GET /api/mls/key-packages/:participant_id`
- `POST /api/mls/groups` `GET /api/mls/groups/:gid`

### WebSocket(`/ws?token=<jwt>`)

客户端帧:`join_room` `send_message` `edit_message` `delete_message` `react` `mark_read` `typing` `call_invite` `call_answer` `call_ice` `call_end` `call_caption` `call_join` `call_leave` `call_offer` `watch_stream` `unwatch_stream` `stream_chat` `stream_gift` `ping`

服务端帧:`welcome` `presence` `message` `edited` `deleted` `reaction` `read` `typing` `call`(`op`: `invite`/`answer`/`ice`/`end`/`caption`/`join`/`leave`/`roster`/`offer`)`stream_event`(`chat`/`gift`/`viewers`/`status`)`error` `pong`

## 验证

```bash
cargo check --workspace          # 干净
cargo test --workspace --lib     # 765 pass / 0 fail / 95 ignored(DB 集成,需 --ignored + DATABASE_URL)
cargo build --bin aero-server    # 二进制成功
cargo clippy -p aero-live-srt -p aero-live-webrtc -p aero-live-whip --all-targets  # 媒体面 crate 零告警
# 注:`cargo clippy --workspace --all-targets` 在 aero-storage/aero-server 等既有 crate 下仍有 pedantic 告警(非本批次引入,待清理)
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
| **P7** | SRT(HSv5 握手 + AES-CTR 加密 + ACK/NAK 可靠性 + TS→HLS 切片)+ 时限 TURN 凭据 | ✅ |
| **P8** | MLS E2E | ✅(scaffold;E2E 为 spec 非目标) |
| **P9** | 真 TS muxing / vector 搜 / mention / read avatars / smoke_p2 | ✅ |
| **P11** | 直播弹幕 + 虚拟礼物 + 观看人数 | ✅ |

媒体面已落地(str0m 0.19,纯 Rust crypto),协议栈深化并尽可能字节级单测;剩余为需真实媒体链路/浏览器的端到端联调或传输回路接线:
- WHIP/WHEP:H.264 解包/打包(FU-A/STAP-A)+ 重排序缓冲 + **RTP→.ts/.m3u8 字节级集成测试** + `WhepSession` egress + **WHIP→WHEP 媒体中继(NAL 扇出)** + `/whep` 路由协商 sendonly SDP 均已实现并单测;**待做** 浏览器 ICE/DTLS/SRTP 推/拉流联调、中继接入实时 run loop
- SFU:选择性转发 + RTP 重映射 + **Simulcast 分层选择 + RTCP PLI/FIR + H.264 关键帧检测** 已实现并单测;**待做** ICE/DTLS/SRTP 端到端、分层目标码率自适应(拥塞控制)
- SRT:HSv5 握手 + 包编解码 + **AES-CTR 加密(KMREQ/KMRSP + RFC 3394)+ ACK/NAK 可靠性 + 控制包序列化 + 收发/重传 pump(`SrtSink` trait)** 已实现并单测;**待做** 绑定真实 UDP socket、拥塞控制、真实 ffmpeg 推流联调
- (E2E/MLS 客户端为 spec 非目标,服务端透传 scaffold 已超出要求)

---

License: MIT OR Apache-2.0
