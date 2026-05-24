# Aero IM

AI-Native 即时通讯 + 直播平台,Rust 实现。

- 设计:[`docs/specs/2026-05-22-aero-im-design.md`](docs/specs/2026-05-22-aero-im-design.md)
- 状态:**P0–P11 全部就位**(110 个单元测试通过,16 个 crate,~18,000 行 Rust + Web)

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
| **WHIP/WHEP** | HTTP 信令(媒体面 P6 接入 str0m) | ✅ scaffold |
| **SFU** | 路由数据模型 (peers/tracks/subscriptions) | ✅ scaffold |
| **SRT 摄入** | UDP 监听占位 + TURN(coturn) 配置渲染 | ✅ scaffold |
| **MLS E2E** | KeyPackage + 群状态服务端透传 | ✅ scaffold |

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

### Cargo workspace(16 crates)
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
- **aero-live-whip** — WHIP/WHEP HTTP 信令
- **aero-live-webrtc** — SFU 路由模型(str0m drop-in 接口)
- **aero-live-srt** — SRT 监听占位 + TurnConfig
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
cargo test --workspace --lib     # 110 pass / 0 fail / 3 ignored(DB 集成)
cargo build --bin aero-server    # 二进制成功
```

## 路线图

| 阶段 | 内容 | 状态 |
|---|---|---|
| **P0** | Workspace + 基础设施 | ✅ |
| **P1** | AI-Ready IM MVP | ✅ |
| **P2** | 协作 + RAG + Agent | ✅ |
| **P3** | 1:1 通话 + 实时字幕翻译 | ✅ |
| **P4** | RTMP→HLS(真 TS) + 弹幕 + 礼物 | ✅ |
| **P5** | WHIP/WHEP 信令 + 关键词审核 + AI 审核 | ✅ |
| **P6** | 群通话(mesh,浏览器原生媒体)+ AI Agent 进频道;SFU 路由模型 | ✅(SFU 媒体面见下) |
| **P7** | SRT + TURN | ✅(占位) |
| **P8** | MLS E2E | ✅(scaffold) |
| **P9** | 真 TS muxing / vector 搜 / mention / read avatars / smoke_p2 | ✅ |
| **P11** | 直播弹幕 + 虚拟礼物 + 观看人数 | ✅ |

后续可继续推进的(都是重力气活,需真实媒体链路联调):
- WHIP/WHEP **媒体面**:str0m DTLS-SRTP 终结 + RTP 注入 HLS muxer
- SFU **媒体面**:str0m 多 peer 转发 + Simulcast/SVC + congestion(mesh 已覆盖小群,SFU 用于大房间扩展)
- SRT **真协议**:srt-tokio 集成
- MLS **客户端**:web 加 openmls-wasm,加密 payload 透传

---

License: MIT OR Apache-2.0
