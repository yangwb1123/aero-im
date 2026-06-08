# AGENTS.md — Aero IM

> 编码 agent 操作手册：**怎么在这仓库干活、别踩哪些坑、什么在范围内**。功能矩阵/测试数/轮次 → `README.md`；架构与路线图 → `docs/specs/2026-05-22-aero-im-design.md`。
> ⚠️ 计数类事实（迁移序号、轮次、功能点、测试数）以 `migrations/` `scripts/` README 为准——**本文件不硬写计数**（会过期）。

AI-Native 即时通讯 + 直播平台，纯 Rust。定位 To-B 协作（类 Slack/Lark）+ 互动直播，AI 走云 API 优先。

## 架构主轴（先读这段）

事件驱动 + 进程内扇出，是全系统的复用骨架：

- **房间实时** = `RoomEvent`（tagged enum，`tag="kind"`）→ NATS `im.room.{id}` → 进程内 `Hub` → WebSocket。
- **直播实时** = `StreamEvent`（同 `tag="kind"`）→ NATS `live.stream.{id}` → `Hub` → WebSocket。
- **NATS = 跨实例投递的事实源**；`Hub` 只在本进程内扇出（bounded mpsc）。多开实例即水平扩消息吞吐。
- **集群级状态走 Redis sorted set**，不靠单进程内存：房间 presence、直播观看数（`StreamViewerStore`）、通话 roster（`CallRosterStore`）——多节点一致。
- ⚠️ tagged enum 标签是 `kind`，**variant 内不要再有名为 `kind` 的字段**（会撞 `duplicate field kind`）。已用 `#[serde(rename=...)]` 处理：`CallEvent`→`call_kind`、`RoomEvent::Notify`→`notify_kind`；web 客户端一律 `event.call_kind || event.kind` 兜底旧服务端。

## crate 地图（依赖自下而上，勿成环）

| 层 | crate | 职责 |
|---|---|---|
| 基础 | `aero-common` | **叶子**：共享类型/ID/`Block`/`RoomEvent`/`StreamEvent`/`Error`/Config/telemetry |
| 基础 | `aero-bus` | NATS JetStream `EventBus` trait + 实现 |
| 基础 | `aero-storage` | sqlx 仓储（每功能一个 `XRepo`）+ `BlobStore`（本地 FS，S3/MinIO 接口已留）+ Redis presence/roster |
| 基础 | `aero-auth` | Argon2id + RS256 JWT + `AuthUser`（JWT 失败回落 `aero_pat_*` PAT）+ OIDC + TOTP |
| 基础 | `aero-signaling` | `RtcConfig` / `IceServer` / SDP·ICE 校验 / `CallRoster` |
| IM | `aero-im-core` | `ImService` 编排（消息/编辑/删除/反应/已读/通知/通话）+ `KeywordModerator` |
| IM | `aero-im-call` | 1:1 与群通话编排（invite/ring/answer/hangup）：`CallOrchestrator` 落库 + SFU peer 簿记 + 未接检测 |
| IM | `aero-ai` | Anthropic / Voyage / `HashEmbedder` / `AiService` / `AiWorker` |
| IM | `aero-push` | 移动推送网关（FCM Android + APNs iOS），token-provider seam + `FakeGateway` |
| 直播 | `aero-live-core` | `LiveIngest` trait + `IngestEvent` |
| 直播 | `aero-live-rtmp` · `-hls` | rml_rtmp 摄入 · `HlsWriter` + `FlvToTsConverter`（真 MPEG-TS mux） |
| 直播 | `aero-live-whip` | str0m WHIP/WHEP：SDP 应答 + 事件循环 + RFC 6184 H.264 解包 + RTP→HLS + NAL 中继 |
| 直播 | `aero-live-webrtc` | str0m SFU：选择性转发 + seq/ts 重映射 + Simulcast + RTCP(PLI/FIR)（**单实例**，见范围红线） |
| 直播 | `aero-live-srt` | 手写 SRT HSv5 握手 + AES-CTR + ACK/NAK + `MpegTsSegmenter` + 时限 TURN 凭据 |
| 组合 | `aero-server` | Axum gateway：HTTP + WS + WHIP/WHEP + HLS + RTMP `:1935` + bots + `Hub`；每功能一个 `pub fn routes()` 模块 |
| 组合 | `web/` | 零依赖 ES2020 SPA（hls.js / RTCPeerConnection / SpeechRecognition CDN） |

**东西放哪**：共享类型→`common`；表/仓储→`storage`(+`migrations/`)；房间功能→`RoomEvent`；流功能→`StreamEvent`；AI→`ai`；HTTP 路由→`server`；推送→`push`；媒体协议→`live-*`（外部真实链路绑 socket/浏览器的，做成 trait seam，逻辑单测、真实接线另算）。

**技术栈**：Rust 2021 / MSRV 1.80 · tokio · axum 0.7 · sqlx 0.8 · fred 9(Redis 7) · async-nats 0.36(JetStream) · str0m 0.19（纯 Rust DTLS-SRTP）· Postgres 17 + pgvector + pg_trgm。AI：Anthropic `claude-sonnet-4-6` + Voyage 1024 维；**无 key 时退化到确定性 `HashEmbedder`/启发式，逻辑路径不变**（这是为什么沙箱里 AI 路由也能 200）。

## 加一个功能：标准配方

绝大多数新功能是**应用层**，照抄 `storage/src/saved_search.rs` + `server/src/saved_searches.rs`：

1. **迁移** `migrations/NNNN_x.sql`（下一序号，`CREATE TABLE IF NOT EXISTS`、uuid 主键、幂等）。⚠️ 迁移被 `sqlx::migrate!` **编译期嵌入 bin**——**加迁移后必须先 `cargo build` 再 `aero-cli migrate`**，否则新迁移静默不生效。
2. **新 ID**（如需，且先于仓储——仓储模型会引用它）：`common/src/ids.rs` 的 `define_id!`。
3. **仓储** `storage/src/x.rs`：`XRepo` 包 `PgPool`，方法 owner/room-scoped；db_tests 用 `#[ignore]`+`DATABASE_URL` 门控；`lib.rs` 里 `pub mod`+`pub use`。
4. **HTTP** `server/src/x.rs`：`pub fn routes() -> Router<AppState>`，用 `XRepo::new(state.pg.clone())` **内联建仓储**（不动 AppState/bin），`.merge` 进 `routes::build`；`lib.rs` 里 `pub mod`。
5. **鉴权**：`AuthUser` extractor；房间数据路由**一律先** `ImService::assert_room_access(participant, room)`（**participant 在前**；成员资格 + 停用统一收口）；工作区管理端用 `WorkspaceRepo::member_role`（Owner/Admin）。
6. **实时**（如需）：加 `RoomEvent`/`StreamEvent` variant（当心 `kind` tag）→ `Hub` 扇出 → WS 帧 → web 处理。
7. **后台任务**（如需）：在 `bin/aero-server.rs` 里 `tokio::spawn`。既有：AI worker、留存清扫、scheduled/recurring/webhook 投递、agent/ooo/golive/transcribe bot（moderation/unfurl bot 按开关）。

**多 agent 并行集成（既定打法）**：一 agent 管一个**不相交单元**，git worktree 隔离，**先 `git reset --hard master` 校准基线**；依赖只加到自身 crate 的 `Cargo.toml`（别动 workspace root，`Cargo.lock` 由 git 自动合）。集成时**拉新文件**（`git checkout <sha> -- <paths>`）+ **手接共享文件**（`routes::build` 的 `.merge` 链、`lib.rs` re-export、`ids.rs`、`RoomEvent`/match 臂）——避免 N 路 append 冲突；合并后 `cargo check --workspace` 兜底。

## 开发循环

```bash
make up                                          # 起 PG/Redis/NATS/Jaeger/MinIO
make jwt-keys env                                # 生成 RS256 PEM + 写配置
cargo build --bin aero-cli --bin aero-server     # 迁移编译期嵌入，改了迁移先 build
cargo run --bin aero-cli -- migrate              # 幂等，按 migrations/ 序号执行
AERO__SERVER__BLOB_DIR=/tmp/aero/blobs AERO__SERVER__HLS_DIR=/tmp/aero/hls \
  cargo run --bin aero-server                    # HTTP/WS = [server].port，RTMP = :1935
```

**提交前必过**：

```bash
cargo check  --workspace                         # 干净
cargo test   --workspace --lib                   # 全绿是底线（实时数见 README）；PG 门控测试加 `-- --ignored`（需 DATABASE_URL + 已迁移）
cargo clippy --workspace --all-targets           # 别新增警告（见「约定」）
```

冒烟（服务须在跑，`AERO_HOST` 指向它）：`smoke_p2.py`(IM 全功能) · `smoke_collab.py` · `smoke_live.py`(弹幕/礼物/观看数) · `smoke_captions.py`(字幕) · `smoke_enterprise.py` · 各 `smoke_waveN.py`（对应批次）。

## 约定

- workspace lints：`unsafe_code = forbid`、`unreachable_pub = warn`、clippy `all`+`pedantic`=warn（已 `allow` 一组 pedantic，完整名单见 `Cargo.toml [workspace.lints]`）。**别引入新警告**；storage/server 等有存量 pedantic 债，非本批次别背。
- 错误用 `thiserror`，每 crate 一个 `Error`；跨 crate 复用类型只放 `aero-common`。公开项写文档注释（仓库全文档化）。
- ⚠️ `webhook`/`scim`/`invitation` 各有自己的 `generate_token`/`hash_token`（`revoked_token` 只有 `hash_token`）；**只有 `webhook` 的在 crate root re-export**，其余走 `aero_storage::<mod>::` 子路径，别撞名。

## 范围红线（动手前确认）

- ✅ **已实现 → 见 README 功能矩阵**（P0–P11 + 多轮 To-B/企业扩展，应用层功能集已完整）。**别重复造**：动手前 grep 模块名/迁移确认没做过。
- 🚫 **非目标，别做**：MLS 端到端加密**客户端** + 联邦（服务端仅不透明字节透传 scaffold，数据模型预留）；移动端原生 SDK（Web 优先）。"无 E2E" 是产品既定决策。
- ✅ **已实现、但沙箱内无法对真实外部端点验证**（**别当 seam 重造**）：OIDC JWKS 拉取、出站 Webhook 投递（HMAC 签名）、unfurl 抓取——都是真实出站 HTTP，只是这里没有真实对端。
- 🧱 **设计边界，非待办**：SCIM 仅入站（RFC 7644，由 IdP 驱动）；SFU **单实例**（roster 在内存，无跨节点媒体路由设计）；VOD 只管录制元数据，真实切片采集骑既有 HLS writer。
- ⛔ **本沙箱无法端到端验证**：浏览器 ICE/DTLS/SRTP 推/拉流、真实 ffmpeg/OBS 推流。报告这类项写「待真实链路联调」，别写完成。

## 已知坑

- **服务器只能前台跑**：后台长驻网络进程会被 harness 回收（exit 144 / SIGURG），`run_in_background` 不行。活冒烟法：前台跑 server，短命 smoke 放后台 subshell、跑完 `pkill aero-server`，输出重定向到文件再 `cat`。
- **端口对不上**：`config.example.toml` 出厂 `[server].port = 8080`，但 smoke 脚本默认 `AERO_HOST=http://localhost:3030`——二者择一对齐（设 `AERO__SERVER__PORT=3030` 或导出 `AERO_HOST`）。
- **活验证用全新一次性库**：共享 dev 库的 `_sqlx_migrations` 账本可能被并行 agent 弄乱序，导致 boot 时 `migrate()` 拒绝——`docker exec aero-postgres psql -U aero -d postgres -c 'CREATE DATABASE x'` 建新库再迁；**别手改账本**。
- `data/` 由容器以 root 创建 → 默认 `blob_dir`/`hls_dir` 写不进，用上面 env 覆写到 `/tmp`。
- config 环境变量前缀 `AERO__SECTION__KEY`（**双下划线**分段），如 `AERO__DATABASE__URL`。
- 限流默认 20/s（burst 40）；冒烟前设 `AERO_RATE_LIMIT_PER_SEC` / `AERO_RATE_LIMIT_BURST` 高些免误伤。
- zsh 下裸 glob（`--include=*.rs`）会被 shell 抢先展开报错；加引号或用 `rg`。
