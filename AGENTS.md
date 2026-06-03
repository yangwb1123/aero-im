# AGENTS.md — Aero IM

> 给编码 agent 的操作手册：**怎么在这个仓库干活、别踩哪些坑、什么在范围内**。
> 功能清单 / API 速查 → `README.md`；架构设计与路线图 → `docs/specs/2026-05-22-aero-im-design.md`。

AI-Native 即时通讯 + 直播平台，纯 Rust。定位 To-B 协作（类 Slack），AI 走云 API 优先，互动直播为主。

## 架构主轴（先读这一段）

事件驱动 + 进程内扇出，是整个系统的复用骨架：

- **房间实时** = `RoomEvent`（tagged enum, `tag="kind"`）→ NATS `im.room.{id}` → 进程内 `Hub` → WebSocket。
- **直播实时** = `StreamEvent` → NATS `live.stream.{id}` → `Hub` → WebSocket。
- **加一个房间实时功能** = 加一个 `RoomEvent` variant（`aero-common`）→ `ImService` 产出（`aero-im-core`）→ `Hub` 转发 → WS 帧（`aero-server`）→ web 客户端处理。直播功能同理走 `StreamEvent`。
- ⚠️ tagged enum 的标签是 `kind`；**variant 内不要再有名为 `kind` 的字段**——会撞成反序列化 `duplicate field kind`（曾让 `CallEvent` 1:1 邀请挂掉，已用 `#[serde(rename = "call_kind")]` 修，客户端读 `call_kind || kind`）。

## crate 地图（15 个，依赖自下而上，勿成环）

| 层 | crate | 职责 |
|---|---|---|
| 基础 | `aero-common` | **叶子**：所有共享类型 / ID / Block / RoomEvent / StreamEvent / Error / Config / telemetry |
| 基础 | `aero-bus` | NATS JetStream EventBus trait + 实现 |
| 基础 | `aero-storage` | sqlx 仓储（Message/Room/Receipt/Reaction/Blob/Call/AiJob/Stream/Presence/MLS）+ BlobStore |
| 基础 | `aero-auth` | Argon2id + RS256 JWT + Axum extractor |
| 基础 | `aero-signaling` | RtcConfig / IceServer / SDP·ICE 校验 / CallRoster |
| IM | `aero-im-core` | `ImService` 编排（消息/编辑/删除/反应/已读/通话）+ `KeywordModerator` |
| IM | `aero-im-call` | P3+ 占位 |
| IM | `aero-ai` | Anthropic / Voyage / HashEmbedder / `AiService` / `AiWorker` |
| 直播 | `aero-live-core` | `LiveIngest` trait + `IngestEvent` |
| 直播 | `aero-live-rtmp` `-hls` | rml_rtmp 摄入 · `HlsWriter` + `FlvToTsConverter`（真 MPEG-TS mux） |
| 直播 | `aero-live-whip` | str0m WHIP/WHEP：SDP 应答 + 事件循环 + RFC 6184 H.264 解包 + RTP→HLS |
| 直播 | `aero-live-webrtc` | str0m SFU：选择性转发 + 每订阅者 RTP seq/ts 重映射 |
| 直播 | `aero-live-srt` | 手写 SRT HSv5 握手 + 包编解码 + `MpegTsSegmenter`（TS→HLS）+ 时限 TURN 凭据 |
| 组合 | `aero-server` | Axum gateway：HTTP + WS + WHIP/WHEP + HLS + RTMP `:1935` + `ai_adapter` + `agent_bot` + `Hub` |
| 组合 | `web/` | 零依赖 ES2020 SPA（hls.js / RTCPeerConnection / SpeechRecognition CDN） |

**放东西的位置**：共享类型 → `common`；表/仓储 → `storage`(+ `migrations/`)；房间功能 → `RoomEvent`；流功能 → `StreamEvent`；AI 能力 → `ai`；HTTP 路由 → `server`；媒体协议 → `live-*`。

## 技术栈基线

Rust 2021 / MSRV 1.80 · tokio · axum 0.7 · sqlx 0.8 · fred 9(Redis)· async-nats 0.36(JetStream)· thiserror · **str0m 0.19（`rust-crypto` 后端，纯 Rust DTLS-SRTP）**。
存储：Postgres 17 + pgvector + pg_trgm · Redis 7 · JetStream（`IM_MESSAGES`/`IM_EVENTS`/`AI_QUEUE`；直播走 `live.stream.*` subject）· 附件默认本地 FS（S3/MinIO 接口已留）。
AI：Anthropic Messages `claude-sonnet-4-6`、Voyage 1024 维嵌入；无 key 时退化到确定性 `HashEmbedder` / 启发式，逻辑路径不变。

## 开发循环

```bash
make up                                    # 起 PG/Redis/NATS/Jaeger/MinIO
make jwt-keys env                          # 生成 RS256 PEM + 写配置
cargo run --bin aero-cli -- migrate        # 迁移，幂等（0001 IM · 0002 collab · 0003 MLS · 0004 转写 · 0005 直播 · 0006 多租户 · 0007 审计 · 0008 AI-job 租户标记 · 0009 消息留存 · 0010 通知 · 0011 置顶 · 0012 频道 · 0013 webhook · 0014 SSO · 0015 SCIM · 0016 定时消息 · 0017 邀请 · 0018 通知偏好/免打扰）
AERO__SERVER__BLOB_DIR=/tmp/aero/blobs AERO__SERVER__HLS_DIR=/tmp/aero/hls \
  cargo run --bin aero-server              # :3030 HTTP/WS，:1935 RTMP
```

**提交前必过**：

```bash
cargo check  --workspace                   # 干净
cargo test   --workspace --lib             # 646 pass（最近一次绿：2026-06-03）；PG 门控测试用 `-- --ignored`（需 DATABASE_URL + 已迁移）
cargo clippy --workspace --all-targets     # all+pedantic=warn，零新增警告
```

冒烟（需服务在跑，`AERO_HOST=http://localhost:3030`）：`scripts/smoke_p2.py`（IM 全功能）· `smoke_live.py`（弹幕/礼物/观看人数）· `smoke_captions.py`（字幕中继）。

## 约定

- `unsafe_code = forbid`；clippy `all`+`pedantic` 全开为 warn——别引入新警告。
- 错误一律 `thiserror`，每 crate 一个 `Error`；跨 crate 复用的类型只放 `aero-common`。
- 依赖加到**自己 crate 的 `Cargo.toml`**，不要动 workspace root。
- **多 agent 并行**（本仓库的既定打法）：一个 agent 管一个**不相交** crate，用 git worktree 隔离，各自只改本 crate `Cargo.toml` → 合并零冲突（`Cargo.lock` 由 git 自动合）。合并后跑 `cargo check --workspace` 兜底。

## 范围红线（动手前确认）

- ✅ **在范围内、已实现（应用层）**：P0–P7 —— IM / 协作 / RAG / AI / 1:1 与群通话(mesh) / 字幕翻译 / RTMP·WHIP·SRT 直播 / 审核。媒体面核心已落地并尽量字节级单测。
- ✅ **To-B 协作 + 企业接入（已实现，活验证）**：线程回复 + @提及通知 + 未读/提及计数 + 置顶（`collab`）；频道治理（公开/私有 / 加入·退出 / 归档 / topic，`channels`）；入站/出站 Webhook（HMAC 签名，`webhooks`，出站投递 `run_webhook_dispatcher`）；SSO via OIDC（`aero-auth::oidc` + `sso`）；SCIM 2.0（`scim`，RFC 7643/7644）；定时消息/提醒（`scheduled` + 后台投递 `run_scheduled_dispatcher`）；工作区邀请/邀请链接（`invitations`）；跨房间全局搜索（`search`，`MessageRepo::search_all_rooms`，按成员资格 SQL 过滤）；通知偏好——频道免打扰 + 每用户 DND（`notif_prefs`，在 `ImService::dispatch_notifications` 经 `should_notify` 抑制，fail-open）。新功能挂载法：新 `RoomEvent` variant（注意 `kind` tag 冲突）+ 仓储新文件 + `pub fn routes()` 模块 `.merge` 进 `routes::build`，仓储用 `XRepo::new(state.pg.clone())`；定时类后台任务在 bin `tokio::spawn`。⚠️ `webhook`/`scim`/`invitation` 各自有 `generate_token`/`hash_token`——只有 `webhook` 的在 crate root re-export，其余走 `aero_storage::<mod>::` 子路径避免重名冲突。
- 🚫 **明确非目标，别做**：P8 = **MLS 端到端加密 + 联邦**。服务端仅做不透明字节透传 scaffold（数据模型预留），**不要实现客户端 MLS 加密 / openmls-wasm**。"无 E2E 加密" 是产品既定决策。
- 🛠️ **可单测的剩余工作**（媒体协议完整性，多已落地）：SRT AES/KMREQ + ACK/NAK、SFU Simulcast + RTCP(PLI/FIR)、WHEP egress。企业接入的**真实链路**为 seam：OIDC 的 JWKS 拉取（逻辑已单测）、出站 Webhook 的真实 HTTP 投递（签名/构造已单测）、SCIM 的真实 IdP 驱动。
- ⛔ **本沙箱无法端到端验证、别标 "done"**：浏览器 ICE/DTLS/SRTP 推流、真实 ffmpeg/OBS 推流。环境无浏览器 / 真实媒体源，后台长驻进程会被回收（exit 144），服务器**只能前台跑**。报告这类项写 "待真实链路联调"，不要写完成。

## 已知坑

- `data/` 由容器以 root 创建 → 默认 `blob_dir`/`hls_dir` 写不进（Permission denied），用上面的 env 覆写到 `/tmp`。
- config 环境变量前缀 `AERO__SECTION__KEY`（**双下划线**分段）。
- zsh 下 `--include=*.rs` 这类裸 glob 会被 shell 抢先展开报错；加引号或改用 `rg`。
