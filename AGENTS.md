# AGENTS.md — Aero IM

> 编码 agent 操作手册：**怎么在这仓库干活、别踩哪些坑、什么在范围内**——不是功能清单。
> ⚠️ 会漂移的数字（迁移序号、轮次、功能数、测试数、**代码行号**）一律以 `README.md`+`migrations/`+`scripts/`+源码为准，**本文件只给文件名/符号名当 grep 锚点**。架构/路线图 → `docs/specs/2026-05-22-aero-im-design.md`。

纯 Rust 的 AI-Native IM + 互动直播平台，To-B 协作（类 Slack/Lark），AI 云 API 优先。

## 架构主轴（先读这段）

事件驱动 + 进程内扇出，是全系统的复用骨架：

- **房间实时** = `RoomEvent`（tagged enum，`tag="kind"`）→ NATS `im.room.{id}` → 进程内 `Hub` → WebSocket；**直播实时** = `StreamEvent` → `live.stream.{id}` → 同链路。
- **NATS = 跨实例投递的事实源**（subject 上的 durable consumer，seq key 区分 `im.room`/`live.stream` 两命名空间，在 `service.rs` 拼接）；`Hub` 只在本进程内扇出（bounded mpsc）。多开实例即水平扩消息吞吐。
- **集群级状态走 Redis sorted set**，绝不靠单进程内存：房间 presence、直播观看数（`StreamViewerStore`）、通话 roster（`CallRosterStore`），心跳 + `zremrangebyscore` 驱逐（`live_presence.rs`），多节点一致。**唯一例外**：`aero-live-webrtc` SFU 的 peer roster 是进程内 `Arc<RwLock<HashMap<CallId, CallState>>>`（**非** DashMap、**非** Redis）——单实例设计、无跨节点媒体路由（见范围红线）。
- ⚠️ tagged enum 标签是 `kind`，**variant 内别再有名为 `kind` 的字段**（会撞 `duplicate field kind` panic）。已用 `#[serde(rename=...)]`（均在 `model.rs`）：`CallEvent`→`call_kind`、`RoomEvent::Notify`→`notify_kind`；web 客户端一律 `event.call_kind || event.kind` 兜底旧服务端。

## crate 地图（依赖自下而上，勿成环）

| 层 | crate | 职责 |
|---|---|---|
| 基础 | `aero-common` | **叶子**：共享类型/ID/`Block`/`RoomEvent`/`StreamEvent`/`Error`/Config/telemetry；MLS 仅不透明字节 scaffold（`src/mls.rs`，无状态机） |
| 基础 | `aero-bus` | NATS JetStream `EventBus` trait + 实现 |
| 基础 | `aero-storage` | sqlx 仓储（每功能一个 `XRepo`）+ `BlobStore`（本地 FS，S3/MinIO 接口已留）+ Redis presence/roster |
| 基础 | `aero-auth` | Argon2id + RS256 JWT + `AuthUser`（JWT 失败回落 `aero_pat_*` PAT）+ OIDC + TOTP |
| 基础 | `aero-signaling` | `RtcConfig`/`IceServer`/SDP·ICE 校验/`CallRoster` |
| IM | `aero-im-core` | `ImService` 编排（消息/编辑/删除/反应/已读/通知/通话）+ `KeywordModerator` |
| IM | `aero-im-call` | 1:1/群通话编排（invite/ring/answer/hangup）：`CallOrchestrator` 落库 + SFU peer 簿记 + 未接检测 |
| IM | `aero-ai` | Anthropic / Voyage / `HashEmbedder` / `AiService` / `AiWorker` |
| IM | `aero-push` | 移动推送网关（FCM + APNs），token-provider seam + `FakeGateway` |
| 直播 | `aero-live-core` | `LiveIngest` trait + `IngestEvent` |
| 直播 | `aero-live-rtmp`·`-hls` | rml_rtmp 摄入 · `HlsWriter` + `FlvToTsConverter`（真 MPEG-TS mux） |
| 直播 | `aero-live-whip` | str0m WHIP/WHEP：SDP 应答 + RFC 6184 H.264 解包 + RTP→HLS + NAL 中继 |
| 直播 | `aero-live-webrtc` | str0m SFU：选择性转发 + seq/ts 重映射 + Simulcast + RTCP(PLI/FIR)（**单实例**） |
| 直播 | `aero-live-srt` | 手写 SRT HSv5 握手 + AES-CTR + Key Wrap(RFC 3394) + ACK/NAK + `MpegTsSegmenter` + 时限 TURN 凭据 |
| 组合 | `aero-server` | Axum gateway：HTTP + WS + WHIP/WHEP + HLS + RTMP `:1935` + bots + `Hub`；每功能一个 `pub fn routes()` 模块 |
| 组合 | `web/` | 零依赖 ES2020 SPA（hls.js / RTCPeerConnection / SpeechRecognition CDN） |

媒体协议外部真实链路（绑 socket/浏览器）一律**做 trait seam**：逻辑单测，真实接线另算。

**技术栈**：Rust 2021 / MSRV 1.80 · tokio · axum 0.7 · sqlx 0.8 · fred 9(Redis 7) · async-nats 0.36(JetStream) · str0m 0.19（纯 Rust DTLS-SRTP，仅 `aero-live-webrtc`/`-whip` 各自 `Cargo.toml` 声明，**不在 root**）· Postgres 17 + pgvector + pg_trgm。**AI 无 key 退化**：退到确定性 `HashEmbedder`（`EMBED_DIM=1024`，对齐 voyage-3）+ 启发式 completion，**逻辑路径不变**——故沙箱 AI 路由也能 200。

## 加一个功能：标准配方

绝大多数新功能是**应用层**，照抄模板 `storage/src/saved_search.rs` + `server/src/saved_searches.rs`：

1. **迁移** `migrations/NNNN_x.sql`（下一序号，`CREATE TABLE IF NOT EXISTS`、uuid 主键、幂等）。⚠️ 迁移被 `sqlx::migrate!`（`db.rs` 唯一调用点）**编译期嵌入 bin**——**加迁移后必须先 `cargo build` 再 `aero-cli migrate`**，否则新迁移静默 no-op。
2. **新 ID**（如需，且先于仓储）：`common/src/ids.rs` 的 `define_id!`。
3. **仓储** `storage/src/x.rs`：`XRepo` 包 `PgPool`，方法 owner/room-scoped；db_tests 用 `#[ignore]`+`DATABASE_URL` 门控；`lib.rs` 里 `pub mod`+`pub use`。
4. **HTTP** `server/src/x.rs`：主流写法 `pub fn routes() -> Router<AppState>` 内 `XRepo::new(s.pg.clone())` **内联建仓储**（不动 AppState/bin），`.merge` 进 `routes::build`；少数模块（如 `user_blocks`）改用 `pub fn router()` + 预接好的 AppState store（`s.blocks`）。
5. **鉴权**：`AuthUser` extractor；房间数据路由**一律先** `ImService::assert_room_access(participant, room)`（**participant 在前**；唯一获批的房间数据守卫，成员资格 + 停用统一收口）；工作区管理端用 `WorkspaceRepo::member_role(workspace, participant)`（Owner/Admin）。CI `authz_lint` 兜底——拿了 room/workspace id 却没调守卫会红。
6. **实时**（如需）：加 `RoomEvent`/`StreamEvent` variant（当心 `kind` tag）→ `Hub` 扇出 → WS 帧 → web 处理。
7. **后台任务**（如需）：`bin/aero-server.rs` 里 `tokio::spawn`。既有：AI worker、留存清扫、scheduled/recurring/webhook 投递、agent/ooo/golive/transcribe bot（moderation/unfurl bot 按开关）。

**多 agent 并行集成（既定打法）**：一 agent 管一个**不相交单元**，git worktree 隔离，**先 `git reset --hard master` 校准基线**；依赖只加到自身 crate 的 `Cargo.toml`（别动 root，`Cargo.lock` 由 git 自动合）。集成时**拉新文件**（`git checkout <sha> -- <paths>`）+ **手接共享文件**（`routes::build` 的 `.merge` 链、`lib.rs` re-export、`ids.rs`、`RoomEvent`/match 臂），避免 N 路 append 冲突；合并后 `cargo check --workspace` 兜底。

## 开发循环

```bash
make up                                          # 起 PG/Redis/NATS/Jaeger/MinIO
make jwt-keys env                                # 生成 RS256 PEM + 写配置
cargo build --bin aero-cli --bin aero-server     # 改了迁移先 build，否则 migrate 静默 no-op
cargo run --bin aero-cli -- migrate              # 幂等，按 migrations/ 序号执行
AERO__SERVER__BLOB_DIR=/tmp/aero/blobs AERO__SERVER__HLS_DIR=/tmp/aero/hls \
  cargo run --bin aero-server                    # HTTP/WS = [server].port，RTMP = :1935
```

**提交前必过**：`cargo check --workspace`（干净）· `cargo test --workspace --lib`（全绿是底线，数见 README；PG 门控测试加 `-- --ignored`，需 `DATABASE_URL`+已迁移）· `cargo clippy --workspace --all-targets`（别新增警告）。冒烟脚本 `smoke_*.py`/`smoke_waveN.py`（服务须在跑、`AERO_HOST` 指向它），全清单见 `scripts/`、说明见 README。

## 约定

- workspace lints（`Cargo.toml [workspace.lints]`）：`unsafe_code=forbid`、`unreachable_pub=warn`、clippy `all`+`pedantic`=warn（已 `allow` 一组）。**别引入新警告**；storage/server 有存量 pedantic 债，非本批次别背。
- 错误用 `thiserror`，每 crate 一个 `Error`；跨 crate 复用类型只放 `aero-common`。公开项写文档注释（仓库全文档化）。
- ⚠️ **token 命名撞名**（load-bearing）：`webhook`/`scim`/`invitation` 各有自己的 `generate_token`+`hash_token`（`revoked_token` 只有 `hash_token`）；**只有 `webhook` 的在 crate root re-export**，其余走 `aero_storage::<mod>::` 子路径（`lib.rs` 有显式「别 re-export」注释），别撞名。

## 范围红线（动手前确认）

| 类别 | 项 | 怎么办 |
|---|---|---|
| ✅ 已实现 | 应用层功能集，见 README 功能矩阵 | **别重复造**：grep 模块名/迁移确认 |
| ✅ 真实出站 HTTP、沙箱无真对端 | OIDC JWKS 拉取、出站 Webhook（`webhook.rs` HMAC-SHA256，非 `webhook_delivery.rs`）、unfurl 抓取 | **别当 seam 重造** |
| 🚫 非目标 | MLS E2E **客户端**（仅 `mls.rs` 不透明字节 scaffold，无 openmls）；**联邦**（零代码零 scaffold，明确出范围）；移动端原生 SDK（Web 优先） | 产品既定决策 |
| 🧱 设计边界、非待办 | SCIM 仅入站（RFC 7644，IdP 驱动）；SFU **单实例**（roster 进程内，无跨节点媒体路由）；VOD 只管录制元数据/生命周期，切片骑既有 HLS writer | 当边界，别当 TODO |
| ⛔ 沙箱无法端到端验证 | 浏览器 ICE/DTLS/SRTP 推/拉流、真实 ffmpeg/OBS 推流；`call_bridge_supervisor.rs` 的 `TODO(real-transport)`（节点间 recvonly RTP 未接线） | 写「待真实链路联调」，别写完成 |

## 已知坑

- **服务器只能前台跑**：后台长驻网络进程会被 harness 回收（exit 144 / SIGURG），`run_in_background` 不行。活法：前台跑 server，短命 smoke 放后台 subshell、跑完 `pkill aero-server`，输出重定向到文件再 `cat`。
- **本地起服务 env 两件套**：① 端口——出厂 `[server].port=8080`，smoke 默认 `AERO_HOST=http://localhost:3030`，二者择一对齐（个别脚本端口不同看脚本）；② `data/` 由容器以 root 创建、默认 `blob_dir`/`hls_dir` 写不进 → `AERO__SERVER__BLOB_DIR`/`HLS_DIR` 覆写到 `/tmp`。
- **活验证用全新一次性库**：共享 dev 库的 `_sqlx_migrations` 账本可能被并行 agent 弄乱序，导致 boot 时 `migrate()` 拒绝——`CREATE DATABASE` 建新库再迁，**别手改账本**。
- config 前缀 `AERO__SECTION__KEY`（**双下划线**分段，figment），如 `AERO__DATABASE__URL`。**例外**：限流用 `AERO_RATE_LIMIT_PER_SEC`/`AERO_RATE_LIMIT_BURST`（**单下划线** plain env）；默认 20/s（burst 40），冒烟前调高免 429。
- zsh 下裸 glob（`--include=*.rs`）会被 shell 抢先展开报 `no matches found`；加引号或用 `rg --glob '*.rs'`。
