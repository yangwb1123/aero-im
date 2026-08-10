# Requirements Spec — aero-live-srt：单 UDP 循环隔离（per-datagram 路径零 audit/outbox I/O；B5-4 fail-closed 配给只作用于 audit 管线，永不作用于媒体面）

- **Module (analysis root)**: `crates/aero-live-srt` — 纯媒体面叶子（SRT HSv5 wire protocol + AES-CTR + reliability/pacing + MPEG-TS→HLS）；本 direction = **隔离不变量 + 测试钉死**，不新增任何 audit 功能
- **Direction**: "Single-UDP-loop isolation: zero audit/outbox I/O on the per-datagram path (media-plane fail-open under B5-4 fail-closed provisioning)"（value 7 / risk_reduction 8 / effort 3 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-live-srt-e06b4c8e.json`（direction #2 of 3）
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml`）；contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（:9 B5-2 语义、:11 B5-4 配给 seam、[PROPOSED] :13）；gate anchor `docs/campaigns/implementation-gate.md`（:64 "T-11（无 relay 配给被拒）"、:78 G6）
- **Sibling specs（并行切片，命名/触点互斥）**: `docs/requirements/2026-08-07-aero-live-rtmp-b5-2-audit-connector.req.md`（B5-2 connector 本体，落家 = 新 crate `aero-audit-connector`）、`docs/requirements/2026-08-07-aero-cli-b5-4-audit-provision-check.req.md`（配给预检，落家 = aero-cli）——**本 direction 零共享文件触点**（见 §3/§7）
- **Status**: Requirements（下述证据全部经源码 grep 核对；行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-07

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-live-srt/src/lib.rs:340-410` — `run_listener` select loop + `peers: HashMap<SocketAddr, PeerState>` 单 socket 多路复用 | ✅ **Verified（行号微漂：实际 :345-431）**。`async fn run_listener` :345；单 `UdpSocket::bind(listen)` :347（SRT 所有发布者共用一个 socket）；`let mut peers: HashMap<SocketAddr, PeerState>` :366；`tokio::select! { biased; cancel / control_tick / sock.recv_from }` :375-383；每 datagram → `handle_datagram(...).await` :406；错误路径在循环内 `backend.finalize` :397（drop 该 peer 前）；关停 drain `backend.finalize` :419 逐个收尾。`PeerState` enum :293-308（Handshaking / Streaming） |
| E2 | lib.rs:952 — `feed_packet` async、in-loop | ✅ **Verified**。`pub async fn feed_packet(&mut self, datagram: &[u8]) -> LiveResult<()>` 精确 :952-1018。由 `handle_datagram` streaming 臂（:486-626，`session.feed_packet(datagram).await?` :589）在循环内调用。await 图 = `feed_ts_bytes` → `flush_segment` → `self.hls.push_segment`（:1050，**LocalFs**，keyframe 切段，`SEGMENT_DURATION_SECS=2` :136）——**feed 路径无任何 DB/audit await**（SrtSession :686 字段集只有 segmenter/hls/reliability/reorder/crypto/pacing，**无 repo/backend 句柄**，结构性成立） |
| E3 | lib.rs:1021-1069 — `feed`/`finish` + pump 循环内 drain | ✅ **Verified**。`feed` :1021（→ `feed_ts_bytes` :1026）；`flush_segment` :1044-1053；`finish` :1058-1067。**pump 是同步函数**（`pub fn pump(&mut self, sink: &mut impl SrtSink, now, peer_socket_id)` pump.rs:206，无 await，doc pump.rs:9-11 "talks only to the supplied sink"）；循环内的 await 是 `sock.send_to`（ACK/NAK 回送，pump :608 + send_to :610-618）与 HLS push——"pump draining all run in-loop" 成立（drain 动作在循环内完成，pump 本身 sync） |
| E4 | lib.rs:311-327 — `LiveIngest::run` → `run_until_cancelled` | ✅ **Verified（行号微漂：实际 :311-314）**。`async fn run(&self, repo, cfg)` :311-314 委托 `self.run_until_cancelled(repo, cfg, CancellationToken::new())`；`run_until_cancelled` :243-247 签名 = `(repo: StreamRepo, cfg: Arc<LiveStreamConfig>, cancel: CancellationToken)`——**无 provisioning 参数**，无任何 audit 配置面 |
| E5 | `crates/aero-live-srt/Cargo.toml` — deps 无 audit/relay/http | ✅ **Verified**。`[dependencies]` = aero-common / aero-live-core / aero-storage / aero-live-hls + tokio/tokio-util/async-trait/bytes/tracing/thiserror/anyhow/serde/serde_json/time/ulid/hmac/sha1/base64/pbkdf2/aes（后六个是 TURN/SRT 密码学栈）。**零 reqwest/hyper/http/audit/relay crate**；dev-deps = tokio/tempfile/sqlx。全 crate `grep -ri audit` = **0 命中**（9 个 src 文件全零） |
| E6 | `crates/aero-server/src/bin/boot/ingest.rs` — SrtIngest boot spawn | ✅ **Verified**。SRT 臂 :77-94：`aero_live_srt::SrtIngest::new()` + 可选 `AERO_SRT_PASSPHRASE`（:82-87）→ `tracker.spawn(srt.run_until_cancelled(repo, srt_cfg, cancel))`。**唯一 env = AERO_SRT_PASSPHRASE**；无 audit 配置、无 provisioning 门。audit 管线落家在 server 侧（工作树在途：`crates/aero-server/src/bin/main.rs:251-259` spawn AuditRelay——与 SRT crate 零连接） |
| E7 | moderation_bot bounded mpsc 512 模式（AGENTS.md §2） | ✅ **Verified**。`crates/aero-server/src/moderation_bot.rs`：`queue_capacity` :72，默认 512 :91，env `AERO_AI_MODERATION_QUEUE` :108-110；`:272 mpsc::channel::<ModerationJob>(cfg.queue_capacity.max(1))`；doc :22/:26 "bus consumer 只 try_send，满队列即 skip 不背压"——**B5 enqueue 落点的绑定模式**（见 R1） |
| E8 | tests.rs 37/37 套件 + 无 DB 测试 seam | ✅ **Verified**。`tests.rs` 恰 **37** 个 `#[tokio::test]`/`#[test]`（+ `rotation_tests.rs` 5 个 = 全 crate 42）；`ListenerTestBackend` :369-394（内存 `SessionBackend` 实现，无 DB）；`listener_datagram_path_negotiates_and_decrypts_encrypted_media` :459-565（**驱动私有 `handle_datagram` + 真实 loopback UDP** 走完整握手+加密媒体，零 DB/audit——R2 隔离测试的直接样板）；`idle_ingest_listener_stops_promptly_when_cancelled` :909-945（`run_until_cancelled` boot 模板，`connect_lazy` 不触 DB——R3 样板）；握手/喂包 helper：`make_data_packet` :355、`handshake_datagram` :396、`listener_test_induction` :412、`ts_packet`/`video_pes`/`pat`/`pmt` :241-284 |
| E9 | B5-2/B5-4 语义锚点（relay-down 模拟的语义基准） | ✅ **Verified**。`docs/proposals/audit-contract-batch-aero-im.md` :9（"B5-2：新 crate … §1.2 语义（lease > 2×timeout、退避 cap 300s、422/409/回执错 → dead ≤1 次、403 → dead = T-11 fail-closed）"）、:11（"B5-4：… 配给验证 seam + fail-closed（boot 门已验证存在；readyz 不翻转）"）、:13（v2 契约全文仓外，[PROPOSED]）；`docs/campaigns/implementation-gate.md` :64（T-11：无 relay 配给被拒）、:78（G6：B5-1..4 = 37/37、T-11、moderation 优先级）。**这些语义是 connector/配给侧的；本 crate 的 relay-down 模拟 = SessionBackend seam 的错误/停滞**（R2） |
| E10 | 依赖守卫先例（静态测试载体） | ✅ **Verified**。`scripts/dependency-check.sh` :19-45 `check_deps`（断言 crate 只依赖 allowlist 内 aero-* crate）；**:59 `check_deps "aero-live-srt" "aero-common,aero-live-core,aero-live-hls,aero-storage"` 已存在**——未来给 aero-live-srt 加 `aero-audit-*`/relay 依赖 = 门红。aero-eng `ALLOWED_DEPS`（checks.rs）同覆盖（工作树在途 B5-2 已修 `gate deps-native` 存量红，本 direction 不碰） |
| E11 | DB 生命周期锚点（start/end 是仅有的 DB await 位置） | ✅ **Verified**。会话 start：`handle_datagram` 握手-established 臂 :537 `backend.resolve(&stream_id).await`（→ `resolve_stream` :1085-1124：`repo.get_by_key` :1090 + `repo.mark_live` :1098，**in-tx outbox CTE 落点在 aero-storage/stream.rs，不在此 crate**；`MarkLiveOutcome` :1101-1111）；会话 end：`finalize_session` :651-666（`session.finish()` + `repo.mark_ended` :661，warn-swallow），在 SHUTDOWN 臂 :501、错误路径 :397、关停 drain :419 三处循环内调用。**per-datagram 路径 DB await = 0；DB await 只在 start/end 各一次（摊薄每会话）** |

### 1.1 补充证据（方向外事实，决定设计形态）

| # | Supplementary evidence | Verification result |
|---|---|---|
| S1 | **tests.rs 逼近 file-size HARD 线**——新测试必须进新文件 | ✅ `tests.rs` = 1137 行，`scripts/file-size-check.sh` HARD=1200 / WARN=800（:7）——**tests.rs 只剩 63 行余量，本 direction 不编辑 tests.rs**；新测试放新文件 `#[cfg(test)] mod isolation_tests;`（先例：`rotation_tests.rs` 243 行，lib.rs:1137-1138 双 `#[cfg(test)] mod`） |
| S2 | 测试可直接驱动私有面（child module + `use super::*`） | ✅ `tests.rs` 首行 `use super::*` 且已直接调用私有 `handle_datagram`（:485-502）——R2 测试无需改生产可见性 |
| S3 | SrtSession 结构上不可能做 DB/audit I/O | ✅ `pub struct SrtSession` :686 字段 = segmenter / hls(HlsWriter) / reliability / reorder / crypto / pacing / key_rotation——无 `StreamRepo`、无 backend、无任何 audit 句柄；`pump` sync（pump.rs:206）。**"by construction" 依据** |
| S4 | 工作树在途 B5 切片（协调面） | ✅ 未提交：`crates/aero-audit-connector/`（B5-2 本体，24/24 in-crate 测试）、aero-server boot wiring（main.rs:251-259）、aero-ai（B5-1）、aero-cli gate 修复。**本 direction 零共享文件触点**（不碰 boot/ingest.rs、dependency-check.sh、test-integration.sh、aero-eng） |
| S5 | 同 analysis 的另两个 direction **未选中**（scope 红线） | ✅ analysis JSON direction #1（事务化 session-end + L1 summary seam）、#3（boot 配给门验证）均未入选本切片——**不建**（§3） |

### 1.2 Corrections to the direction's claims（证据核对修正）

| # | Direction 原文 | 修正（证据） |
|---|---|---|
| C1 | "run_listener, lib.rs:340+" | 实际 :345（doc 注释自 :339 起）——行号漂移，符号 `run_listener` 无误 |
| C2 | "lib.rs:311-327 (LiveIngest::run -> run_until_cancelled)" | `run` 实际 :311-314；`run_until_cancelled` :243-247。语义成立 |
| C3 | "feed_packet/feed (lib.rs:952, 1021) plus pump draining all run in-loop" | `feed_packet` :952 ✓、`feed` :1021 ✓；**修正细节**：pump 本身是同步函数（pump.rs:206 无 await），"run in-loop" 的是 pump 之后的 `sock.send_to` 回送与 HLS push——风险陈述不变（全部在循环内） |
| C4 | "assert no storage/relay await inside feed_packet/pump" | **语义澄清**：`feed_packet` 有 await = `hls.push_segment`（**LocalFs 媒体面**，keyframe 才切段）；"storage" 指 **DB/audit 存储**（StreamRepo/outbox）——feed 路径 DB await = 0（E2/S3）。验收 (a) 的断言面 = 无 DB/relay await，非无任何 await |
| C5 | "Cargo.toml: no relay/audit crates" | ✓ 全对（E5）；另补：crate 内 `grep -i audit` = 0 命中、无 `AERO_AUDIT_*` env 读取面——"fail-open for media 是今天的事实" 由 E5/E6 双重钉死 |

## 2. Verified current state

```
现状（aero-live-srt 对 audit/relay 零感知；隔离是今天的事实，缺的是钉死它的测试）：
a) 单 UDP 循环（E1）   一个 socket + 一个 select 循环多路复用所有 SRT 发布者；
                       任何在循环内新增的同步 I/O 都会拖垮全部发布者（risk 成立）
b) per-datagram 路径（E2/E3/S3） feed_packet/feed/pump 无 DB/audit await（结构性）；
                       DB await 只在会话 start（resolve :537）与 end（finalize :501/:397/:419）
c) 依赖面（E5/E10）    Cargo.toml 无 audit/relay/http；dependency-check.sh:59 allowlist 已钉
d) boot（E6）          boot/ingest.rs 只读 AERO_SRT_PASSPHRASE；run_until_cancelled 无
                       provisioning 参数——零配给下媒体面 fail-open 是今天的行为
e) 测试面（E8/S1）     tests.rs 37 测试 + rotation_tests.rs 5；无 DB 的 ListenerTestBackend
                       seam 与完整握手 helper 齐备；tests.rs 距 HARD 线仅 63 行
f) B5 语义（E9）       B5-2 422/403/退避/lease、B5-4 fail-closed 配给全在 connector/配给侧
                       （aero-audit-connector / aero-cli provision-check / aero-server boot），
                       本 crate 永远不接触——relay-down 的仓内模拟 = SessionBackend seam
g) 在途切片（S4）      connector + boot wiring + aero-ai 未提交；本 direction 零共享触点
```

**Gaps this direction closes**（all verified）：① 无测试钉死「streaming 臂零 backend 调用」（结构性事实无行为断言）；② 无 relay-down 模拟（backend 停滞/报错时已建立会话必须继续出段）；③ 无 boot fail-open 测试（`run_until_cancelled` 零配给全握手+喂包，DB-gated）；④ 无仓内 Cargo.toml 静态守卫（依赖 allowlist 在 scripts/，crate 内无自证）；⑤ B5 落点约束未成文（enqueue boundary = session start/end only + bounded channel，防 B5 误插 feed/pump）。

## 3. Scope

**In scope（isolation 不变量 + 测试钉死，effort 3 的最小切片）**：
- `crates/aero-live-srt/src/isolation_tests.rs`（**新测试文件**，`#[cfg(test)] mod isolation_tests;` 挂进 lib.rs，先例 rotation_tests.rs）+ 必要的最小 lib.rs 改动（仅加一行 `#[cfg(test)] mod isolation_tests;`——**生产代码零改动**）。
- R2 隔离测试 ×3（hermetic，无 DB）：streaming 臂零 backend 调用 + 停滞/报错 backend 下会话继续 + start 失败只掉单 peer。
- R3 boot fail-open 测试 ×1（DB-gated，`#[ignore]` + `DATABASE_URL` + throwaway 库，AGENTS.md §4.3）：`run_until_cancelled` 零审计配给全握手 + TS 喂包 + mark_live/HLS 产物 + cancel → mark_ended。
- R4 静态守卫 ×1（crate 内自证，`include_str!("../Cargo.toml")` 断言依赖 allowlist）——**不碰** scripts/dependency-check.sh（:59 已钉）与 aero-eng（在途切片）。
- R1 成文契约：enqueue boundary = session start/end only，bounded channel try_send（moderation_bot 512 模式），never in feed/pump——写进 spec + 测试钉（T1/T4），**本 direction 不建 worker**（零生产者 = 死代码，AGENTS.md §4.4）。

**Out of scope（并行切片 / 其他 direction / 产品边界——勿在本 direction 建造）**：
- **同 analysis direction #1（事务化 session-end + L1 summary）**：`finalize_session` 的 outbox 化、`SrtSession::finish()` 返回 summary、per-segment 计数——**不建**。
- **同 analysis direction #3（boot 配给门验证）**：`run_until_cancelled` 加 provisioning 参数、audit env 读取、配给 fail-closed 门——**不建**（T-11 fail-closed 的落家 = B5-4 的 aero-cli provision-check + connector，见 sibling spec）。
- **B5-1 outbox / 0239 DDL / aero-storage 任何改动**：`mark_live` in-tx CTE 已存在（E11），B5-1 落点在 aero-ai/storage 侧。
- **B5-2 connector / B5-4 provision-check / B5-3 moderation priority**：全部 sibling 切片，本 direction 只消费其语义做模拟（E9）。
- **真实 relay/IdP 联调**：仓外（proposal :13）。
- **生产代码改动**：`run_listener`/`handle_datagram`/`feed_packet`/`pump`/`finalize_session`/`resolve_stream` 一行不动；Cargo.toml `[dependencies]` 冻结；boot/ingest.rs 不动；无新 env；无迁移。

## 4. Requirements

### R1 — 隔离硬不变量（契约条款，本 direction 的规范核心）

成文并测试钉死以下不变量（全部 today-true，E2/E3/E5/S3 已验证）：

1. **per-datagram 路径零 audit/outbox/relay I/O**：`run_listener` select 循环 → `handle_datagram` streaming 臂 → `feed_packet`/`feed_ts_bytes`/`flush_segment`/`pump` 不得出现任何 audit/outbox/relay 写、读、await。feed 路径的唯一 await = `hls.push_segment`（LocalFs 媒体面）；DB await 只允许存在于会话 start（`resolve_stream`，每会话一次）与会话 end（`finalize_session`，每会话一次）——摊薄成本，非 per-datagram。
2. **B5 enqueue 落点约束（when B5-1/B5-4 land）**：本 crate 任何 audit enqueue 只允许挂在会话 start（handshake-established，`resolve_stream` 边界）与会话 end（`finalize_session` 边界），且必须是**非阻塞 `try_send` 进 bounded mpsc**（capacity 默认 512、`AERO_*_QUEUE` env 可调——moderation_bot :91/:108-110/:272 模式，AGENTS.md §2），由 **`run_listener` 之外 spawn 的 worker** 消费；循环永不 await worker、永不 await connector/relay/scope 状态。connector 403/422/backoff（B5-2 §1.2）与配给 fail-closed（B5-4）语义全部留在 audit 管线侧（aero-audit-connector / aero-server boot / aero-cli provision-check），**永不进入本 crate 媒体面**。
3. **依赖冻结**：`Cargo.toml [dependencies]` 不得新增 audit/relay/http crate（R4 静态守卫 + scripts/dependency-check.sh:59 双保险）。

### R2 — 隔离测试（hermetic，无 DB，新文件 `src/isolation_tests.rs`）

`#[cfg(test)] mod isolation_tests;`（lib.rs 追加一行；**tests.rs 与生产代码零改动**——S1）。复用 `ListenerTestBackend` 模式（tests.rs:369-394）+ `handle_datagram` 直驱（:459 样板）+ helper（:355/:396/:412/:241-284）。

- **T1 `streaming_peer_never_awaits_backend`**（= 验收 (a) 主钉）：instrumented backend（`resolve`/`finalize` 各带调用计数器 + `tokio::time::sleep(500ms)` 停滞，模拟 relay-down 的最坏情况）。完整握手 → Streaming；随后喂 N=32 个 TS 数据包（`make_data_packet` + `ts_packet`/`video_pes`/`pat`/`pmt` 真实 TS，含 keyframe 触发切段）。断言：
  - `resolve_calls == 1`（仅会话 start）、`finalize_calls == 0`（streaming 期间零 teardown 调用）；
  - 每个数据包经 `tokio::time::timeout(100ms, handle_datagram(...))` **必不超时**（latency bound ≪ 500ms 停滞）——per-datagram 处理时间与 backend 停滞解耦；
  - HLS 段持续产出（段计数增长，`hls.push_segment` 生效）——segments keep flowing。
- **T2 `relay_down_at_session_end_keeps_streaming`**（= 验收 (a) 的 422/403/backoff 模拟）：`finalize` 先停滞 500ms 再返回 Err（模拟 B5-2 connector 422/403 → dead 的终态失败，E9）；已建立会话继续喂包 → 全部在 100ms 界内完成、段继续产出；随后 SHUTDOWN → 该 peer 被清出 `peers`，循环存活。
- **T3 `relay_down_at_session_start_drops_only_that_peer`**（media-plane fail-open 的启动面）：`resolve` 返回 Err（模拟配给/DB 拒绝）；第一个 peer 握手后即被 drop（run_listener 错误路径 :397 同款），但**第二个 peer 的完整握手 + 喂包成功**——监听器不因单 peer 的 start 失败而倒下。
- **T4 `cargo_toml_stays_audit_and_relay_free`**（= 静态守卫，R4）：`const CARGO_TOML: &str = include_str!("../Cargo.toml");`——断言 `[dependencies]` 段内 aero-* 依赖 ⊆ {aero-common, aero-live-core, aero-storage, aero-live-hls}，且不含 `reqwest`/`hyper`/`audit`/`relay` token。仓内自证，随 `cargo test -p aero-live-srt` 跑（37/37 harness 内）。

**延迟界纪律**：停滞 500ms vs 界 100ms 是 5× 余量，用 `tokio::time::timeout` 断言（不测绝对延迟，测「不随 backend 停滞增长」），无 wall-clock flake。

### R3 — Boot fail-open 测试（零审计配给，DB-gated）

- **B1 `run_until_cancelled_boot_fails_open_without_audit_provisioning`**（`#[ignore]` + `DATABASE_URL` 门控，throwaway 库纪律，AGENTS.md §4.3；模板 = tests.rs:909-945 的 `run_until_cancelled` + 握手 helper）：
  1. throwaway DB 建库迁移（`make migrate-smoke` 式），`StreamRepo::create(NewStream)` 预插 stream 行；
  2. `SrtIngest::new().run_until_cancelled(repo, cfg, cancel)` spawn（临时端口，rtmp_listen = 探得端口 −1，:909-916 模式）；
  3. UDP 直驱完整 HSv5 握手（`listener_test_induction` :412 + `handshake_datagram` :396）+ TS 喂包（含 keyframe）；
  4. 断言：`mark_live` 生效（`get_by_key` 后状态 live / `MarkLiveOutcome::Started`）、HLS `.ts`/`.m3u8` 出现在 `hls_dir`；
  5. `cancel.cancel()` → listener 优雅返回，`mark_ended` 落库；
  6. **零审计配给面**：全程无 `AERO_AUDIT_*` env、无 audit 配置——crate 无 audit 符号（E5：grep = 0）即为构造性证明；测试 doc 注释 + T4 静态守卫双钉。
- **既有锚点保绿**（fail-open-for-media 的现有证据，不改不删）：`listener_datagram_path_negotiates_and_decrypts_encrypted_media` tests.rs:459（完整握手 + 加密媒体，零 DB/audit）、`idle_ingest_listener_stops_promptly_when_cancelled` :909（boot + cancel，零配给）。

### R4 — 静态依赖守卫（双保险，零共享文件改动）

- **crate 内**：T4（R2）——`include_str!("../Cargo.toml")` 断言，随测试套件跑。
- **仓级（既有，保持绿，不编辑）**：`scripts/dependency-check.sh:59` `check_deps "aero-live-srt" "aero-common,aero-live-core,aero-live-hls,aero-storage"`（check_deps :19-45 语义 = 出现 allowlist 外 aero-* 依赖即红——未来加 `aero-audit-*` 必红）；`aero-eng gate deps`/`deps-native`（ALLOWED_DEPS 已含 aero-live-srt）。
- **断言面**：`bash scripts/dependency-check.sh` exit 0；`aero-eng gate deps` exit 0。

### R5 — 37/37 套件保全（验收 (c)）

- 基线：`cargo test -p aero-live-srt` = **42 测试函数**（tests.rs 37 = direction 所指 "37/37" + rotation_tests.rs 5）；本 direction 后 = 42 + N（T1-T4 + B1），**全部既有测试原样保绿**。
- 新测试只进新文件 `isolation_tests.rs`（tests.rs 1137 行，距 HARD 1200 仅 63 行——S1，file-size-check.sh:7）；lib.rs 生产代码零改动（仅 `#[cfg(test)] mod` 一行）。
- `Cargo.toml [dependencies]` 冻结（R1.3）；dev-deps 不动（tokio/tempfile/sqlx 已够 B1）。
- 门禁：`cargo check --workspace` · `cargo test --workspace --lib`（+ `-- --ignored` 跑 B1，需 DATABASE_URL + throwaway 库）· `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）。

### R6 — 约束条款

- **生产代码零改动**：`run_listener`/`handle_datagram`/`feed_packet`/`feed`/`pump`/`finalize_session`/`resolve_stream`/`SrtSession` 一行不动；不改可见性（child module 可驱私有面，S2）。
- **不建 bounded-channel worker**：零生产者 = 零调用 builder（truth-check 会抓，AGENTS.md §4.4）——R1.2 是**契约**（B5 落地时执行），由 T1/T4 测试钉住边界，不预建死代码。
- **零共享文件触点**：不碰 boot/ingest.rs、scripts/dependency-check.sh、scripts/test-integration.sh、aero-eng、migrations/、aero-storage、aero-server（S4 协调）。
- 新测试文件体量小（< 800 行 WARN 线），file-size-check 不触发。

## 5. Acceptance checks（direction 原样保留，逐条 testable）

> direction acceptance 原文三条，逐条保留并钉死测试面。机器断言面 = isolation_tests.rs 的 T1-T4/B1（R2/R3）+ 仓级依赖门（R4/R5）。

### AC1 — simulate relay-down (connector 422/403/backoff per B5-2 semantics) while an established session feeds packets → per-datagram processing latency bound holds and segments keep flowing (assert no storage/relay await inside feed_packet/pump, by construction or static test)

**测试 = T1 + T2 + T3（hermetic，无 DB）+ T4（静态）**：
- **relay-down 模拟**：connector 语义（E9）在本 crate 的仓内映射 = `SessionBackend::{resolve, finalize}` 停滞 500ms / 返回 Err——这是 B5 enqueue 唯一可挂的两点（R1.2），模拟「配给/relay 失效」的最坏情况；connector 本体（422/403/backoff 状态机）在 B5-2 crate，不在本方向验收面；
- **latency bound**：streaming 期间每数据包 `timeout(100ms)` 必过（T1/T2）——per-datagram 处理与 backend 停滞解耦（5× 余量，非 wall-clock flake）；
- **segments keep flowing**：HLS 段计数在停滞/报错 backend 下持续增长（T1/T2）；
- **no storage/relay await inside feed_packet/pump**：三路证明——(i) **by construction**：SrtSession 无 repo/backend 句柄（S3）+ pump sync（pump.rs:206）+ feed 路径唯一 await = LocalFs HLS push（E2）；(ii) **行为钉死**：T1 断言 streaming 期间 `resolve_calls==1 ∧ finalize_calls==0`（backend 调用只在会话边界）；(iii) **静态**：T4 依赖守卫 + scripts/dependency-check.sh:59（E10）。

### AC2 — boot test: SrtIngest::run_until_cancelled accepts a full handshake + TS feed with zero audit provisioning configured (fails-open for media, matching today's behavior)

**测试 = B1（DB-gated，`#[ignore]` + DATABASE_URL + throwaway 库）+ 既有锚点 :459/:909 保绿**：
- `run_until_cancelled` 全 boot：bind → 完整 HSv5 握手 → TS 喂包 → `mark_live` + HLS 产物 → cancel → `mark_ended`（B1 步骤 1-5）；
- **zero audit provisioning**：crate 零 audit 符号（E5，grep = 0）、`run_until_cancelled` 签名无 provisioning 参数（E4）、boot/ingest.rs 只读 AERO_SRT_PASSPHRASE（E6）——构造性成立，B1 步骤 6 + T4 双钉；
- **fails-open for media, matching today's behavior**：既有 hermetic 锚点 :459（完整握手+加密媒体，零 DB/audit）与 :909（boot+cancel 零配给）保绿即证明「今天的行为」未被改变；B1 证明完整 boot 路径在零配给下成功。

### AC3 — 37/37 integration suite stays green — no new DB/relay dependency introduced into the crate's hot path

- **37/37**：tests.rs 37 测试（+ rotation_tests.rs 5）**原样保绿**（R5）；新增 T1-T4/B1 为增量（42 + N），不进 tests.rs（S1 行数纪律）；
- **no new DB/relay dependency**：`Cargo.toml [dependencies]` 冻结（R1.3）+ T4 静态断言 + `scripts/dependency-check.sh:59` 保持绿（E10）+ `gate deps`/`deps-native` 绿；
- **hot path 无新依赖**：per-datagram 路径（`feed_packet`/`pump`）生产代码零改动（R6）——依赖面不变即路径不变；
- 门禁：`cargo test --workspace --lib`（含 `-- --ignored` B1）· `cargo check --workspace` · `cargo clippy --workspace --all-targets` 无新警告 · `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| T1 streaming 臂零 backend 调用 + latency bound + 段持续产出 | `crates/aero-live-srt/src/isolation_tests.rs`（新，`#[cfg(test)] mod isolation_tests;`） | `cargo test -p aero-live-srt`（hermetic，无 DB，无外部依赖） |
| T2 relay-down @ session end（finalize 停滞+报错）会话继续 | 同上 | 同上 |
| T3 relay-down @ session start 只掉单 peer，监听器存活 | 同上 | 同上 |
| T4 Cargo.toml 静态守卫（include_str! 断言） | 同上 | 同上 |
| B1 boot fail-open 全握手+喂包（DB-gated） | 同上（`#[ignore]` + DATABASE_URL） | `cargo test -p aero-live-srt -- --ignored`（throwaway 库，AGENTS.md §4.3） |
| 既有锚点保绿（不改不删） | `tests.rs` 37 测试（含 :459/:909）、`rotation_tests.rs` 5 | `cargo test -p aero-live-srt` |
| 仓级依赖门（既有，保持绿） | `scripts/dependency-check.sh:59`；aero-eng `gate deps`/`deps-native` | `bash scripts/dependency-check.sh`；`aero-eng gate deps` |

## 7. Risks / [PROPOSED] / 决策点

- **B5-2/B5-4 语义 [PROPOSED]（proposal :13，v2 契约仓外）**：本 direction 不实现任何 connector 语义（422/403/backoff/lease 在 B5-2 crate），只把「relay-down」模拟为 SessionBackend seam 的停滞/报错——语义数值若改判，只影响 connector 侧测试，本 direction 的隔离断言不受影响。
- **latency bound 的 flake 风险**：500ms 停滞 vs 100ms 界 = 5× 余量 + `tokio::time::timeout` 断言（非绝对延迟测量）；CI 慢机下 timeout 只可能更松不会更紧（停滞是相对量）。
- **tests.rs 行数红线**：1137/1200（S1）——本 direction 不编辑 tests.rs；新测试全进新文件；lib.rs 只加一行 `#[cfg(test)] mod`。若实现时发现必须复用 tests.rs 的私有 helper（如 `ts_packet` 族），复制到 isolation_tests.rs（或提取共享 test-util 模块——但**不编辑 tests.rs** 是本 direction 的硬约束，避免 63 行余量内任何意外）。
- **不预建 bounded-channel worker（R6）**：方向 evidence 的 "bounded channel/worker" 是**设计约束**（B5 落地时的绑定模式），非本 direction 交付物；现在建 = 零调用死代码（truth-check 红，AGENTS.md §4.4）。契约由 T1/T4 + R1.2 成文钉死。
- **与在途切片的协调**：aero-audit-connector / aero-server main.rs boot wiring / aero-ai（B5-1）/ aero-cli gate 修复均未提交（S4）——本 direction 零共享文件触点（唯一落点 = 本 crate 新测试文件 + lib.rs 一行 cfg 挂载），无冲突；`dependency-check.sh:59`、`gate deps` 保持现状绿。
- **同 analysis 其他 direction 不并入**：direction #1（事务化 session-end）与 #3（boot 配给门）未选中——B5 落地时若需要，按各自 direction 另开切片；本 direction 的 R1.2 恰好为它们划好边界（enqueue 只许 start/end + bounded channel）。
- **DB-gated B1 的纪律**：按 AGENTS.md §4.3 用一次性 throwaway 库（`CREATE DATABASE` + 迁移 + 用完 `DROP DATABASE`），`#[ignore]` 门控，`DATABASE_URL` 缺失时显式 skip 不红。
- **行号漂移**：本 spec 全部行号为 2026-08-07 核对时锚点；实现与后续引用以**符号名**为准（AGENTS.md §0）。

## 8. Sequencing

1. **lib.rs 挂载**：追加 `#[cfg(test)] mod isolation_tests;`（生产代码零改动）。
2. **T1/T2/T3**：`src/isolation_tests.rs` hermetic 隔离测试（instrumented backend + `handle_datagram` 直驱 + helper 复制）——独立可验：`cargo test -p aero-live-srt isolation_tests`。
3. **T4**：Cargo.toml 静态守卫（`include_str!`）——随 2 一起绿。
4. **B1**：DB-gated boot 测试（throwaway 库 + `StreamRepo::create` 预插 + 全握手/喂包 + mark_live/HLS 断言 + cancel）——`cargo test -p aero-live-srt -- --ignored`。
5. **门禁**：`cargo test -p aero-live-srt`（42+N 全绿）· `cargo test --workspace --lib` · `cargo check --workspace` · `cargo clippy --workspace --all-targets`（无新警告）· `bash scripts/dependency-check.sh` + `aero-eng gate deps`（绿）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· no-touch 守卫（`git diff --stat` 不含 boot/ingest.rs、dependency-check.sh、test-integration.sh、aero-storage、migrations/、aero-server 生产代码；lib.rs 生产代码 diff = 仅 `#[cfg(test)] mod` 一行）。
