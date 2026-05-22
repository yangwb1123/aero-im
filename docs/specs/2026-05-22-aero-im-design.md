# Aero — AI-Native 即时通讯 + 直播平台 设计 Spec

- **日期**:2026-05-22
- **状态**:Approved (默认参数)
- **范围**:整体架构 + P0/P1 (AI-Ready IM MVP) 详细设计
- **不可逆决策(默认采纳)**:
  - 目标场景:**To-B 协作**(类 Slack/Lark)
  - AI 推理:Gateway 双通,**默认走云 API**
  - 直播形态:**互动直播主线**,RTMP→HLS 作为兼容路径
  - **不做** E2E 加密(数据模型预留,后期可加)

---

## 1. 目标与非目标

### 目标
- 构建以 **Agent 为一等公民** 的通讯协作平台
- 用 Rust 实现高性能、低资源占用的实时服务
- IM + 直播 共享 WebRTC 信令与 SFU,降低运维复杂度
- 渐进式交付,每阶段都可独立演示

### 非目标(本 spec 范围)
- 不做 E2E 加密的密钥协商(数据结构预留)
- 不做联邦(Matrix/ActivityPub)
- 不做移动端原生 SDK(Web 优先,移动端后续)
- 不做多租户/SaaS 化(单租户起步)

---

## 2. 系统架构

### 2.1 高层架构

```
┌──────────────────────────────────────────────────────────┐
│  客户端 (Web / 移动端)                                     │
│  ├ HTTP REST (控制面)                                     │
│  ├ WebSocket (IM 实时 + 通话信令)                          │
│  └ WebRTC (媒体面:通话/直播)                              │
└──────────────────────────────────────────────────────────┘
                          │
┌──────────────────────────────────────────────────────────┐
│  aero-server (Axum 网关)                                  │
│  ├ /api/auth/*    →  aero-auth                            │
│  ├ /api/im/*      →  aero-im-core                         │
│  ├ /api/live/*    →  aero-live-core                       │
│  ├ /api/ai/*      →  aero-ai                              │
│  ├ /ws            →  WS 处理 (im+signaling 复用)           │
│  ├ :1935  (TCP)   →  aero-live-rtmp                       │
│  ├ :3478  (UDP)   →  aero-turn (后期)                     │
└──────────────────────────────────────────────────────────┘
        │                │                │              │
   ┌────▼────┐      ┌───▼────┐       ┌───▼───┐     ┌────▼─────┐
   │ Postgres│      │ Redis  │       │ NATS  │     │ S3/MinIO │
   │+pgvector│      │会话/缓存│       │事件总线│     │  附件     │
   └─────────┘      └────────┘       └───────┘     └──────────┘
```

### 2.2 Cargo Workspace 布局

```
aero-im/
├── Cargo.toml                  # workspace 根
├── docker-compose.yml          # 开发环境 (pg/redis/nats/jaeger/minio)
├── migrations/                 # sqlx migrations
├── crates/
│   ├── aero-common/            # 共享类型 (ID/Block/Error/Config/Logging)
│   ├── aero-bus/               # NATS JetStream 抽象
│   ├── aero-storage/           # PG (sqlx) + Redis 仓储
│   ├── aero-auth/              # JWT (RS256) + Argon2 + Axum 提取器
│   ├── aero-signaling/         # WebRTC 信令(P3+)
│   ├── aero-im-core/           # IM 业务:消息、房间、好友、Block 校验
│   ├── aero-im-call/           # IM 1对1 通话编排(P3+)
│   ├── aero-live-core/         # 直播 Stream/Track 抽象(P4+)
│   ├── aero-live-rtmp/         # RTMP 摄入(P4)
│   ├── aero-live-hls/          # HLS 分发(P4)
│   ├── aero-live-whip/         # WHIP/WHEP(P5)
│   ├── aero-live-webrtc/       # SFU,基于 str0m(P6)
│   ├── aero-live-srt/          # SRT(P7)
│   ├── aero-ai/                # AI Gateway/RAG/Agent/MCP(P2 起逐步实现)
│   └── aero-server/            # 二进制入口
└── web/                        # 联调用 HTML/JS 客户端
```

---

## 3. 数据模型(P1 核心)

### 3.1 Participant(参与者抽象)
```rust
pub enum ParticipantKind { Human, Agent, Bot }
pub struct Participant {
    pub id: ParticipantId,        // 内部统一 Ulid
    pub kind: ParticipantKind,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub created_by: Option<ParticipantId>, // Agent/Bot 的所有者
}
```

### 3.2 Block(消息块)
```rust
pub enum Block {
    Text { content: String, spans: Vec<Span> },
    Mention(ParticipantId),
    Code { lang: String, content: String },
    File { blob_id: BlobId, kind: FileKind, size: u64 },
    Voice { blob_id: BlobId, transcript: Option<String>, duration_ms: u32 },
    Card { schema: String, payload: serde_json::Value },
    ToolCall { tool: String, args: Value, result: Option<Value> },
    Thought { content: String, hidden: bool },
}
```

### 3.3 PG 表结构(P1)
```sql
-- 参与者
participants(id PK, kind, display_name, avatar_url, created_by FK, created_at);

-- 凭据(只针对 Human)
credentials(participant_id PK FK, email UNIQUE, password_hash, created_at, last_login);

-- 房间
rooms(id PK, kind ENUM('direct','group','channel'), name, created_by FK, created_at);

-- 房间成员关系(简版 ReBAC)
room_members(room_id FK, participant_id FK, role ENUM('owner','member'), joined_at, PRIMARY KEY(room_id, participant_id));

-- 消息(blocks 存 JSONB,embedding 异步填充)
messages(
  id PK,                           -- Ulid 自带时序
  room_id FK,
  sender_id FK,
  blocks JSONB NOT NULL,           -- Block[] 序列化
  reply_to UUID,                   -- 引用消息
  metadata JSONB,
  embedding vector(1024),          -- pgvector;P1 留空
  created_at TIMESTAMPTZ DEFAULT now(),
  edited_at TIMESTAMPTZ,
  deleted_at TIMESTAMPTZ
);
CREATE INDEX ON messages (room_id, created_at DESC);
CREATE INDEX ON messages USING gin (blocks);
CREATE INDEX ON messages USING hnsw (embedding vector_cosine_ops); -- P2 启用
```

---

## 4. P0/P1 详细设计

### 4.1 P0 — 基础设施
- Cargo workspace,所有 crate 编译通过(空实现)
- `docker-compose.yml`:Postgres 17、Redis 7、NATS JetStream、Jaeger、MinIO
- sqlx 迁移脚本运行通过
- `aero-common`:ID 类型、Block、Error、Config(figment)、tracing+OTLP
- 启动 `aero-server` 健康检查 `/health` 返回 200

### 4.2 P1 — AI-Ready IM MVP

**功能验收**:
1. 用户注册 / 登录 → 拿到 JWT
2. 创建房间(group)、邀请其他用户
3. 浏览器 WebSocket 连接,带 JWT 鉴权
4. 在房间内发送 Text Block 消息
5. 同房间其他客户端实时收到消息
6. 刷新页面,历史消息按时序加载(分页)
7. 所有消息走 NATS JetStream 发布,可重放
8. `tracing` 链路在 Jaeger 可见

**非功能**:
- 单消息端到端延迟 < 100ms(本地)
- WS 断线自动重连,消息不丢(NATS 持久化)
- 全部 SQL 走 sqlx 编译期校验

**API(REST + WS)**:
```
POST /api/auth/register {email, password, display_name}
POST /api/auth/login {email, password} → {access_token, refresh_token}
GET  /api/me                                 → Participant
POST /api/rooms {kind, name}                 → Room
POST /api/rooms/:id/members {participant_id}
GET  /api/rooms/:id/messages?before=&limit=  → [Message]
WS   /ws (Bearer in query/header)
  ↓ Client → Server:
  { type:"join_room", room_id }
  { type:"send_message", room_id, blocks:[...], reply_to? }
  { type:"ping" }
  ↑ Server → Client:
  { type:"message", message:{...} }
  { type:"presence", room_id, online:[...] }
  { type:"error", code, msg }
  { type:"pong" }
```

**NATS 流**:
```
im.room.{room_id}     消息广播,持久化 7 天,消费组 = ws-server-{instance}
im.events             高层事件(房间创建/成员变更),持久化 30 天
ai.queue.embed        embedding 工作队列(P2 启用)
```

---

## 5. 技术栈

| 层 | 选型 | 版本 |
|---|---|---|
| HTTP/WS | axum | 0.8 |
| 异步运行时 | tokio (multi-thread) | 1.40+ |
| 数据库 | Postgres + pgvector | 17 / 0.7 |
| SQL | sqlx | 0.8 |
| 缓存 | Redis | 7 |
| 事件总线 | NATS JetStream / async-nats | 0.36 |
| WebRTC | str0m | 最新 |
| RTMP | rml_rtmp | 最新 |
| AI 本地 | candle-core | 最新 |
| AI 远程 | async-openai | 最新 |
| MCP | rmcp | 最新 |
| 序列化 | serde + prost | — |
| 鉴权 | jsonwebtoken (RS256) + argon2 | — |
| 可观测 | tracing + opentelemetry-otlp | — |
| 错误 | thiserror (lib) + anyhow (bin) | — |

---

## 6. 渐进交付路线

| 阶段 | 内容 | 备注 |
|---|---|---|
| **P0** | Workspace + 基础(本 spec) | 本会话 |
| **P1** | AI-Ready IM MVP(本 spec) | 本会话 |
| P2 | 多房间/私聊/历史/已读/附件 + RAG 语义搜索 + 摘要 | |
| P3 | IM 1对1 音视频通话(WebRTC P2P)+ 实时字幕翻译 | |
| P4 | 直播 RTMP→HLS + 弹幕 + 礼物 | |
| P5 | 直播 WHIP/WHEP + AI 内容审核 | |
| P6 | WebRTC SFU(群通话 + 互动直播) + AI Agent 进频道 | |
| P7 | SRT + 自建 TURN + 多 Region | |
| P8 | E2E 加密(MLS)+ 联邦化(可选) | |

---

## 7. 风险与开放问题

| 风险 | 缓解 |
|---|---|
| str0m 在 SFU 场景下的成熟度 | P6 前用 webrtc-rs 做 PoC 对照 |
| pgvector HNSW 在大规模(>1000万)的性能 | P2 评测;必要时切 Qdrant |
| NATS 单点 → 多副本运维 | 早期单实例,P5 起部署 cluster |
| Rust 招聘难度 | 模块边界清晰,部分外围(web/AI 调用)可用 TS/Python |

---

## 8. 验收清单(P1 完成定义)

- [ ] `cargo check --workspace` 通过
- [ ] `cargo test --workspace` 通过
- [ ] `docker compose up -d` 起服务
- [ ] `cargo run --bin aero-server` 启动健康
- [ ] 浏览器双标签:登录 → 进同一房间 → 互发消息 → 实时显示
- [ ] 刷新页面 → 历史可见
- [ ] Jaeger 看到 trace
- [ ] NATS 看到消息持久化(`nats stream ls`)

---

**附**:本 spec 与代码同源演进,有任何冲突以代码为准,但 schema 变更必须先改 spec。
