# Design — Single-UDP-loop isolation for aero-live-srt（per-datagram 路径零 audit/outbox I/O；B5-4 fail-closed 配给永不进入媒体面）

- **Module**: `crates/aero-live-srt`（纯媒体面叶子：SRT HSv5 + AES-CTR + reliability/pacing + MPEG-TS→HLS）
- **Source**: `docs/requirements/2026-08-07-aero-live-srt-b5-udp-loop-isolation.req.md`（R1–R6 / T1–T4 / B1 / AC1–AC3）
- **Campaign**: `aero-im-b5-outbox-relay`；gate **G6 (B5)** = "37/37、T-11、moderation 优先级"（`docs/campaigns/implementation-gate.md:78`）；contract anchor `docs/proposals/audit-contract-batch-aero-im.md:9/:11/:13`
- **Design principle**: 隔离不变量 **today-true，缺的是钉死它的测试**——本设计 = 生产代码零改动 + 1 个新测试文件 + 1 条成文契约（B5 enqueue 落点约束）

## 0. Evidence verification（untrusted claims 逐条对仓库复核，2026-08-07）

| # | Evidence claim | Verdict | Independent evidence |
|---|---|---|---|
| E1 | `run_listener` 单 UDP select 循环 + `peers: HashMap<SocketAddr, PeerState>` | ✅ 符号与语义全对（行号微漂） | `async fn run_listener` lib.rs:345；单 `UdpSocket::bind(listen)` :347；`let mut peers: HashMap<SocketAddr, PeerState> = HashMap::new()` :366；`tokio::select! { biased; cancel.cancelled() / control_tick.tick() / sock.recv_from }` :375-383；循环内 `backend.finalize` :397（错误路径 drop 前 best-effort）+ :419（关停 drain）。**修正**：循环内 `handle_datagram(...).await` 调用在 **:388**（:406 是 recv Err 臂，证据行号漂移）；`PeerState` enum :293-309（Handshaking / Streaming） |
| E2 | `feed_packet` async in-loop（:952），await 图仅 LocalFs HLS push | ✅ | `pub async fn feed_packet` :952-1018；streaming 臂 `session.feed_packet(datagram).await?` :589；await 图 = `feed_ts_bytes` :1026 → `flush_segment` :1044-1053 → `self.hls.push_segment(bytes, SEGMENT_DURATION_SECS_F32).await` :1050（LocalFs）。`SrtSession` 字段（:686-712）= segmenter/hls/has_open_segment/crypto/key_rotation/reliability/reorder/receive_buffer_capacity/pending_actions/pacer——**无 repo/backend 句柄，结构性成立** |
| E3 | `feed`/`finish` + pump 循环内 | ✅（细节修正见 C3） | `feed` :1021、`finish` :1058-1067、`flush_segment` :1044-1053；`pump` 是**同步函数** `pub fn pump(&mut self, sink: &mut impl SrtSink, now, peer_socket_id) -> usize`（pump.rs:206，无 await）；循环内 await = `sock.send_to`（握手应答 / ACK-NAK 回送）与 HLS push |
| E4 | `LiveIngest::run` → `run_until_cancelled`（无 provisioning 参数） | ✅ | `async fn run(&self, repo, cfg)` :311-314 委托 `self.run_until_cancelled(repo, cfg, CancellationToken::new())`；`run_until_cancelled` :243-249 签名 = `(repo: StreamRepo, cfg: Arc<LiveStreamConfig>, cancel: CancellationToken)`——无 provisioning、无 audit 配置面 |
| E5 | Cargo.toml 零 audit/relay/http 依赖 | ✅ | `[dependencies]` = aero-common / aero-live-core / aero-storage / aero-live-hls + tokio/tokio-util/async-trait/bytes/tracing/thiserror/anyhow/serde/serde_json/time/ulid + hmac/sha1/base64/pbkdf2/aes（TURN/SRT 密码学栈）；dev-deps = tokio/tempfile/sqlx。全 crate `grep -ri audit` = 0 命中 |
| E6 | boot/ingest.rs SrtIngest spawn（:77-94） | ✅ | `SrtIngest::new()` :80；唯一 SRT env `AERO_SRT_PASSPHRASE` :81（crate 内另有 `AERO_SRT_MAX_BANDWIDTH_BYTES_PER_SEC` 运行期读取，`max_bandwidth_from_env`）；`tracker.spawn(srt.run_until_cancelled(repo, srt_cfg, cancel))` :88。无 audit 配置 |
| E7 | moderation_bot bounded mpsc 512 模式 | ✅ | `crates/aero-server/src/moderation_bot.rs`：`queue_capacity` :72，默认 512 :91（:500 有断言），env `AERO_AI_MODERATION_QUEUE` :108-110；`mpsc::channel::<ModerationJob>(cfg.queue_capacity.max(1))` :272；bus 侧只 `try_send` :169（满队列即 skip 不背压）。**注意路径是 `src/moderation_bot.rs` 非 `src/bots/`** |
| E8 | 37/37 套件 + 无 DB seam | ✅ | tests.rs 恰 **37** 个 `#[test]`/`#[tokio::test]` + rotation_tests.rs **5**；`ListenerTestBackend` tests.rs:369-394（内存 SessionBackend）；`listener_datagram_path_negotiates_and_decrypts_encrypted_media` :459-565（私有 `handle_datagram` 直驱 + 真实 loopback UDP，零 DB）；`idle_ingest_listener_stops_promptly_when_cancelled` :909-945（`run_until_cancelled` boot 模板）；helpers：`ts_packet`/`video_pes`/`pat`/`pmt` :241-284、`make_data_packet` :355、`handshake_datagram` :396、`listener_test_induction` :412 |
| E9 | B5-2/B5-4 语义锚点（connector/配给侧） | ✅ | `docs/proposals/audit-contract-batch-aero-im.md` :9（B5-2：lease>2×timeout、退避 cap 300s、422/409→dead≤1、403→dead=T-11 fail-closed）、:11（B5-4：provision-check seam + fail-closed）、:13（v2 契约仓外 [PROPOSED]）；`docs/campaigns/implementation-gate.md` :64（T-11）、:78（G6）。语义全在 connector/配给侧，本 crate 的 relay-down 模拟 = SessionBackend seam 停滞/报错 |
| E10 | 依赖守卫先例 | ✅ | `scripts/dependency-check.sh:59`：`check_deps "aero-live-srt" "aero-common,aero-live-core,aero-live-hls,aero-storage"`（check_deps :22-45 语义 = allowlist 外 aero-* 依赖即红）；file-size-check.sh HARD=1200 / WARN=800（:7），tests.rs = **1137 行**（余 63） |
| E11 | DB await 只在会话 start/end | ✅ | start：established 臂 `backend.resolve(&stream_id).await` :537 → `resolve_stream` :1085-1124（`repo.get_by_key` :1090 + `repo.mark_live` :1098，`MarkLiveOutcome::Started/AlreadyLive/NotFound`）；end：`finalize_session` :651-666（`session.finish()` + `repo.mark_ended` :661，双 warn-swallow），调用点 = SHUTDOWN 臂 :501、错误路径 :397、关停 drain :419 |

### 0.1 Corrections（证据核对修正，影响设计形态）

| # | Claim 原文 | 修正 |
|---|---|---|
| C1 | "in-loop handle_datagram :406" | 实际调用点 :388；:406 是 `recv_from` Err 臂。语义不变（每 datagram 循环内 await） |
| C2 | "finalize 先停滞 500ms 再返回 Err"（T2 措辞） | **`SessionBackend::finalize` 返回 `()`**（lib.rs:326，非 `LiveResult`）——"返回 Err" 字面不可能。误差模拟改为：instrumented finalize 停滞 500ms 后对**已损坏的 HLS writer** 执行 `session.finish()`（内部 Err 被吞，镜像生产 `finalize_session` 的 warn-swallow 语义 :656-665）。隔离断言不变：finalize 不在 streaming 路径上 |
| C3 | "pump draining all run in-loop" | pump 本身 sync（pump.rs:206）；"in-loop" 的是 pump 后的 `sock.send_to` 回送与 HLS push。风险陈述不变 |
| C4 | "feed 路径无任何 await" | 精确化：feed 路径唯一 await = `hls.push_segment`（LocalFs 媒体面，keyframe 切段）；DB/audit await = 0 |
| C5 | "仅 env AERO_SRT_PASSPHRASE" | crate 运行期还读 `AERO_SRT_MAX_BANDWIDTH_BYTES_PER_SEC`（`max_bandwidth_from_env`）——与 audit 无关，不影响本设计 |

### 0.2 三轮评审修正合并（2026-08-07，R1–R9 全部落地）

| # | 来源 | 修正 | 落点 |
|---|---|---|---|
| R1 | tokio_async (a) + testing_rigor (a) | C8/F1 "tokio 虚拟时钟" 主张**删除** → 真实时钟（plain `#[tokio::test]`，house convention——tests.rs 全部 16 个 `#[tokio::test]` 皆默认 real-time）+ **构造性确定性**（停滞永不进入有界区域）+ 余量 ≥100×（streaming 100ms vs LocalFs µs–ms flush）/ 4×（T2 2s vs 500ms 停滞） | §3 C8、§5 F1 |
| R2 | tokio_async (d) | T2 SHUTDOWN 界 **500ms → `timeout(2s)`**（真实时钟下 500ms 停滞 + broken-writer finish 可超 500ms，精确边界即 flake；2s = 4× 余量）+ 可选 `std::time::Instant` elapsed **≥400ms** 证明停滞确实被支付 | §4 T2 |
| R3 | tokio_async (c) | B1 teardown 序列：join 超时后 `handle.abort()` + `let _ = handle.await`（残留任务持池连接 → `DROP DATABASE` 报 "being accessed by other users"）；成功/失败**两路径** `pool.close().await` 后再 DROP（测试持 `pool.clone()`）；探 **SRT** 端口 P、`rtmp_listen = P-1`（tests.rs:909-916 模板；`listen_addr` = rtmp+1，lib.rs:254-260——探 P 用作 rtmp_listen 会让实际绑定的 P+1 未探测）；join 断言镜像 tests.rs:940-944 | §4 B1 |
| R4 | testing_rigor (c) | T4 dotted-key 规范化（scripts/dependency-check.sh:29 同款：`=` 前段 → `.` 前段）+ `package=` 改名扫描 + **死 banned-token 循环删除**（deps 预过滤为 aero-*，任何新 aero-* 先被相等断言红、非 aero-* 被过滤不可见——循环在一切状态不可达；且 reqwest/hyper 经允许的 aero-storage（S3BlobStore，AGENTS.md §4.5）已是传递依赖，禁 token 是剧场） | §4 T4 |
| R5 | testing_rigor bonus 2 + tokio_async (d) | T3 peer2 = **全新 `CountingBackend`**（单 `Mutex<Option<SrtSession>>` 槽只服务一次 resolve）；"或计数至第二会话" 选项**删除**（第二次 resolve 必 Err "test session already consumed"，不可实现） | §4 T3 |
| R6 | testing_rigor bonus 3 | T1 断言 1–4 在第二 peer 阶段**前**快照（`resolve_calls == 1` 先钉；phase 5 后跨两 backend 总计 = 2） | §4 T1 |
| R7 | testing_rigor (b) | T2 **显式 `fs::remove_dir_all(hls_dir)` 注入**（16 包后、SHUTDOWN 前，测试持 tempdir）——否则 `finalize_broken_writer` 对健康 writer 的 finish 返回 Ok，`let _` 吞的是 Ok，Err 分支从未执行（测试空转通过） | §4 T2 |
| R8 | srt_protocol (c) + testing_rigor bonus 1 | AC1 限界措辞 **"bounded stall then full recovery"**（错误不传播/不入 per-datagram 路径；他 peer 仅见 = finalize 时长的有界停滞，随后全保真恢复）+ **F11 行**（循环内联 await 的跨 peer 有界黑障 + UDP 缓冲 drop 窗口 + R1.2 为防无界增长守卫）；T3 断言 2 修正（直驱下 resolve Err 后 peer 仍留 `Handshaking`——移除在 run_listener 错误臂 :396-402，循环级覆盖 = B1） | §7 AC1、§5 F11、§4 T3 |
| R9 | srt_protocol (b) | helper 复制清单补 **shutdown-header builder**（tests.rs:194-210 样板：`SrtHeader{Control::Shutdown}` + `to_bytes()`，裸 16 字节控制头即真实 SHUTDOWN 无 body）；clear-conclusion builder 已含（去 KM 版，negotiate_crypto :1058-1092 已验通过） | §2.2 |

### 0.3 评审冲突裁定（Reconciliation）

| 冲突 | 裁定 | 理由 |
|---|---|---|
| testing_rigor #1（paused-clock + test-util 手动 poll harness，可捕亚界 await）vs tokio_async（真实时钟唯一） | **真实时钟**（tokio_async） | (i) house convention：tests.rs 全部 16 个 `#[tokio::test]` 皆 plain real-time；(ii) `start_paused=true` 的 auto-advance 在 runtime 空闲时把虚拟时间跳到**最早 pending timer**——`push_segment`（tokio::fs）pending 时 100ms 定时器必先触发 → T1 断言 3 恒败；T2 任何 timeout 包装在 sleep 前进后恒败；B1 5s 在 `mark_ended`（PG）pending 时触发——paused clock 与真实 IO await 不相容；(iii) test-util 不在 dev-deps（C3 冻结）。testing_rigor #1 的**可移植组件并入**：全 seam 总 `calls` 计数器（per-datagram `calls==0`，强于双计数器——带默认体的新 seam 方法也可见）+ 聚合 wall bound 兜底（32 包 < 150ms） |
| testing_rigor #3（`run_listener_with(ingest, backend, cancel)` seam，~5 行行为保持重构）vs 零生产改动（C1/R6） | **零生产改动** | R6 是 requirement 的硬约束条款，seam 触碰 `run_listener` 签名与 no-touch 清单。诚实处理 = F11 行 + AC1 限界措辞（"bounded stall then full recovery"）+ B1 跑真实循环（快 PG）；强主张（循环级并发 fail-open）列为 follow-up（§8），不假装 T1 覆盖 |
| T1 断言 4 段计数 vs `LIVE_WINDOW_SEGMENTS=6` 驱逐（aero-live-hls/src/lib.rs:24，驱逐 :148） | 工作量不变式 + 单调指标兜底 | 32 包/4 IDR（8/16/24/32 切段，CutBeforeKeyframe 语义 :1029-1037）→ 至多 5 段 < 6 → 驱逐不触发、`.ts` 文件数 == segment_index（单调）；若未来工作量增长越过窗口，改用清单 `EXT-X-MEDIA-SEQUENCE`（每段重写 :152）+ `next_segment_index()`（:126）作单调指标 |

## 1. Design overview

```
SRT 发布者 datagrams ──► run_listener（单 UDP socket + 单 select 循环，多路复用全部 peer）
  ├─ handshake 臂 ──► backend.resolve（会话 start，每会话一次 DB/backend await）   [B5 enqueue 落点 #1]
  ├─ streaming 臂 ──► feed_packet → feed_ts_bytes → flush_segment → hls.push_segment（LocalFs）
  │                    + pump（sync）→ sock.send_to（ACK/NAK 回送）                 [ZERO DB/audit]
  └─ SHUTDOWN/错误/关停 ──► backend.finalize（会话 end，每会话一次）                [B5 enqueue 落点 #2]

隔离不变量（today-true，本设计用测试钉死）：
  per-datagram 路径零 audit/outbox/relay I/O；DB await 只允许在会话 start/end 各一次（摊薄每会话）。
  结构性证明：SrtSession 无 repo/backend 句柄（E2）+ pump sync（E3）。
```

**本设计交付物**（effort 3 的最小切片，全部可独立验收）：

1. **`crates/aero-live-srt/src/isolation_tests.rs`**（新文件，`#[cfg(test)] mod isolation_tests;` 挂进 lib.rs）——T1–T4（hermetic）+ B1（DB-gated）。
2. **lib.rs 生产代码零改动**（仅追加一行 `#[cfg(test)] mod isolation_tests;`）。
3. **R1 成文契约**（§2.3）：B5 enqueue 落点 = 会话 start/end 边界 + bounded mpsc `try_send`（512 模式）+ `run_listener` 外 worker；**本 direction 不建 worker**（零生产者 = 死代码，truth-check 红，AGENTS.md §4.4）。

## 2. API changes

### 2.1 生产 API：零改动（no-touch 清单）

以下符号**一行不动、不改可见性**（child module 经 `use super::*` 可驱私有面，tests.rs:459 先例）：

| 符号 | 位置 | 为什么不动 |
|---|---|---|
| `run_listener` / `send_due_keepalives` | lib.rs:345 / :431 | 循环结构 = 隔离的载体 |
| `handle_datagram` / `PeerState` | lib.rs:486 / :293 | 会话边界路由 |
| `SrtSession::{feed_packet, feed, finish, feed_ts_bytes, flush_segment, new}` | lib.rs:952/1021/1058/1026/1044/732 | 媒体面本体 |
| `finalize_session` / `resolve_stream` | lib.rs:651 / :1085 | 唯二 DB await 点 |
| `SrtIngest::{new, with_identity, with_passphrase, run_until_cancelled, run}` | lib.rs:182/…/243/311 | boot 面 |
| `SessionBackend` trait | lib.rs:323-327 | 测试 seam（新增 `CountingBackend` 实现它，不改 trait） |
| `crates/aero-live-srt/Cargo.toml` | — | `[dependencies]` 冻结（T4 静态钉） |

### 2.2 测试面 API（新，`#[cfg(test)]` only）

**lib.rs 追加一行**（文件末尾，先例 rotation_tests.rs）：

```rust
#[cfg(test)]
mod isolation_tests;
```

**`src/isolation_tests.rs` 内容**（`use super::*;`，直接复用私有面）：

```rust
/// Instrumented SessionBackend：调用计数器 + 停滞/报错注入（B5 relay-down 的仓内模拟）。
struct CountingBackend {
    stream_id: ulid::Ulid,
    /// 预构造的 SrtSession（HlsWriter 根于 tempdir）；resolve 时取出。
    session: std::sync::Mutex<Option<SrtSession>>,
    resolve_calls: std::sync::atomic::AtomicUsize,
    finalize_calls: std::sync::atomic::AtomicUsize,
    /// 全 seam 总计数器（resolve+finalize+未来任何方法）：per-datagram 断言 `calls == 0`
    /// 比双计数器更强——新 seam 方法若带默认体（如 `async fn audit(&self) {}`），
    /// 双计数器看不见，总计数器看得见（0.3 裁定 1，F10）。
    calls: std::sync::atomic::AtomicUsize,
    /// >0 时 resolve 先停滞该时长（T1 的 500ms；模拟 DB/配给慢）。
    resolve_delay: std::time::Duration,
    /// true 时 resolve 返回 Err（T3：配给/DB 拒绝）。
    resolve_error: bool,
    /// >0 时 finalize 先停滞该时长（T2 的 500ms；模拟 connector 终态失败）。
    finalize_delay: std::time::Duration,
    /// true 时 finalize 对已损坏 writer 执行 finish（内部 Err 吞掉，镜像 finalize_session
    /// :656-665 的 warn-swallow；同时代言 mark_ended 失败分支——同形吞错，doc 注明）。
    /// 损坏由测试在 SHUTDOWN 前显式 `fs::remove_dir_all(hls_dir)` 注入（T2 步骤 3，R7）；
    /// writer 无粘性错误态、文件句柄每次调用即开即弃（aero-live-hls/src/lib.rs:138/:200），
    /// 删目录后下一次 `File::create` 必 NotFound → `finish()` 内部 Err。
    finalize_broken_writer: bool,
}
impl SessionBackend for CountingBackend {
    async fn resolve(&self, stream_key: &str) -> LiveResult<(ulid::Ulid, SrtSession)> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.resolve_calls.fetch_add(1, Ordering::SeqCst);
        if !self.resolve_delay.is_zero() { tokio::time::sleep(self.resolve_delay).await; }
        if self.resolve_error {
            return Err(LiveError::UnknownStreamKey(stream_key.to_string()));
        }
        let session = self.session.lock().unwrap().take()
            .ok_or_else(|| LiveError::Protocol("test session already consumed".into()))?;
        Ok((self.stream_id, session))
    }
    async fn finalize(&self, session: SrtSession, _stream_id: Option<ulid::Ulid>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.finalize_calls.fetch_add(1, Ordering::SeqCst);
        if !self.finalize_delay.is_zero() { tokio::time::sleep(self.finalize_delay).await; }
        if self.finalize_broken_writer {
            // HLS writer 根目录已被删除 → finish() 内部 Err → 吞掉（生产 warn-swallow 同款）。
            let _ = session.finish().await;
        }
    }
}
```

**Helper 复制策略**（**不编辑 tests.rs**——1137/1200 HARD，余 63 行，S1）：从 tests.rs 复制到 isolation_tests.rs 的 helpers = `ts_packet`/`video_pes`/`pat`/`pmt`（:241-284）、`make_data_packet`（:355）、`handshake_datagram`（:396）、`listener_test_induction`（:412）+ 两个新造 builder（R9）：
- **clear-conclusion builder** `listener_test_clear_conclusion`：`listener_test_encrypted_conclusion` :433 的**去 KM 版**——`HS_ENC_CLEAR` + `HsExtension::HsReq` + `StreamId`、**无 `KeyMaterial` 扩展**；无 passphrase 的 `SrtIngest` 经 `negotiate_crypto`（protocol.rs:1058-1092：无 passphrase 时接受 CLEAR 且无 KM，否则 `UnexpectedEncryption`）真实进入 `HsAction::Established`——已验，驱动真实 established 分支；
- **shutdown-header builder** `shutdown_control_datagram`：tests.rs:194-210 `is_shutdown_detects_only_shutdown_control` 样板——`SrtHeader { kind: PacketKind::Control { control_type: ControlType::Shutdown, .. }, .. }` + `to_bytes()`；裸 16 字节控制头即真实 SRT SHUTDOWN（无 body），`is_shutdown`（lib.rs:622-627）只要求可解析控制头。
每份复制顶部注释指向 tests.rs 的 canonical 位置；字节保持逐字一致（防语义漂移，见 F7）。

**会话驱动序列**（tests.rs:459-565 样板，非加密变体）：
1. `SrtIngest::with_identity(0x5254_0001, 0xCAFE)`（确定性 socket id）+ 真实 loopback `UdpSocket` 对；
2. `handle_datagram(ingest, &server, &backend, &mut peers, peer, &listener_test_induction(caller_socket_id))` → 读应答、解析 cookie；
3. conclusion（`listener_test_clear_conclusion`：syn_cookie + `HsExtension::HsReq` + `StreamId(stream_key)`，`HS_ENC_CLEAR` 无 KM）→ established 臂调 `backend.resolve` 恰一次 → Streaming；
4. 数据包：`make_data_packet(seq, KkFlag::Clear, &ts_payload)`，`ts_payload` = `ts_packet(0x0000, true, &pat())` 开头 + 后续 PAT/PMT/video PES（IDR nal_type=5 触发切段）。

### 2.3 B5 enqueue 落点契约（R1.2——本 direction 只成文 + 测试钉边界，不建 worker）

当 B5-1/B5-4 落地时，本 crate 任何 audit enqueue **只允许**以下形状（moderation_bot 512 模式，AGENTS.md §2）：

```rust
// ① 会话 start：handle_datagram established 臂、backend.resolve 成功之后（唯一落点）
// ② 会话 end：finalize_session 边界（唯一落点）
// 两者都必须：
let _ = audit_tx.try_send(AuditEnqueue::SessionBoundary { /* stream_id, peer, phase */ });
//  - bounded mpsc（capacity 默认 512，AERO_*_QUEUE env 可调——moderation_bot :91/:108-110/:272）
//  - 非阻塞 try_send：满队列即 skip，绝不背压媒体面
//  - worker 在 run_listener 之外 tokio::spawn 消费（本 direction 不建：零生产者 = 死代码）
//  - 循环永不 await worker / connector / relay / scope 状态
```

**禁止**：`feed_packet`/`feed_ts_bytes`/`flush_segment`/`pump` 内任何 enqueue/await；`run_until_cancelled` 加 provisioning 参数（B5-4 fail-closed 门在 aero-server boot / aero-cli provision-check，永不进本 crate——T-11 落点，implementation-gate.md:64）。钉死方式 = T1（streaming 臂 `resolve_calls==1 ∧ finalize_calls==0`）+ T4（依赖冻结）+ 本契约条款。

## 3. Compatibility constraints

| # | Constraint | 违反后果 |
|---|---|---|
| C1 | 生产代码零改动（§2.1 no-touch 清单） | truth-check 抓孤儿/行为漂移；隔离断言失去基准 |
| C2 | tests.rs 不编辑（1137/1200，余 63 行）；新测试只进 isolation_tests.rs | file-size-check HARD 红；37/37 基线破坏 |
| C3 | `Cargo.toml [dependencies]` 冻结；dev-deps 不动（tokio/tempfile/sqlx 已够 B1） | T4 红 + `scripts/dependency-check.sh:59` 红 + aero-eng `gate deps` 红 |
| C4 | 无新 env（`AERO_SRT_*` 集合不变：PASSPHRASE + MAX_BANDWIDTH） | 配置面漂移 |
| C5 | 无迁移、无 storage/server/boot/eng/scripts 改动（在途 B5 切片零共享触点） | 与未提交切片（aero-audit-connector、main.rs boot wiring、aero-ai）冲突 |
| C6 | 37/37 + rotation 5 原样保绿；新增 T1-T4+B1 为增量（42+N） | AC3 红 |
| C7 | B1 按 AGENTS.md §4.3：`#[ignore]` + `DATABASE_URL` 门控 + throwaway 库；无 DATABASE_URL 时显式 skip 不红 | CI 无 DB 环境误红 |
| C8 | 延迟界纪律：plain `#[tokio::test]`（**真实时钟**，house convention——tests.rs 全部 16 个 `#[tokio::test]` 均默认 real-time，tokio 1.52 语义）；确定性靠**构造**：停滞只放在会话 start/end（allowed slow points，F6），**永不进入有界区域**；余量 = streaming 100ms vs LocalFs µs–ms flush **≥100×**、T2 2s vs 500ms 停滞 **4×**、B1 5s vs ms 级 drain。**无** tokio 虚拟时钟主张（test-util 未启用，auto-advance 会破坏 T1/B1——0.3 裁定 1） | CI 慢机 flake |
| C9 | 新文件 < 800 行 WARN 线；lib.rs diff = 仅一行 `#[cfg(test)] mod` | file-size-check WARN/truth-check 红 |

## 4. Test design（concrete assertion contracts）

### T1 `streaming_peer_never_awaits_backend`（AC1 主钉）

- `CountingBackend { resolve_delay: 500ms, resolve_error: false, finalize_delay: 0, finalize_broken_writer: false }`。
- 完整握手 → Streaming（established datagram **不包** 100ms 界、承担 500ms——**会话 start 是允许的慢点，每会话一次**，F6）；随后喂 **N=32** 个 TS 数据包（`pat` + `pmt` + 非 IDR PES 交替、第 8/16/24/32 包为含 IDR 的 PES 触发切段）。
- 断言（**1–4 在第二 peer 阶段前快照执行**——phase 5 会合法新增 resolve #2，R6）：
  1. `resolve_calls == 1`（仅会话 start；500ms 停滞恰好被支付一次——每会话摊薄证明）；
  2. `finalize_calls == 0` 且总计数器 **`calls == 1`**（全 seam 单计数器，R4：未来任何带默认体的新 seam 方法也会被计数）；
  3. 32 个数据包**每个**经 `tokio::time::timeout(Duration::from_millis(100), handle_datagram(...))` 必不超时，且每次返回后 `calls` 不增（per-datagram 与 backend 解耦；真实时钟下 100ms vs LocalFs µs–ms flush **≥100× 余量**——C8）；
  4. HLS 段持续产出：喂完第 16 包后与喂完第 32 包后各数一次 `hls_dir` 下 `.ts` 文件数，`count_after > count_before ≥ 1`（segments keep flowing）。**单调性不变式（0.3 裁定 3）**：4 IDR 切段（CutBeforeKeyframe，lib.rs:1029-1037）→ 至多 5 段 < `LIVE_WINDOW_SEGMENTS = 6`（aero-live-hls/src/lib.rs:24，驱逐 :148）→ 窗口驱逐不触发、文件数 == segment_index；若未来工作量增长越过窗口，改用清单 `EXT-X-MEDIA-SEQUENCE`（每段重写 :152）+ `next_segment_index()`（:126）作单调指标；
  5. 循环存活：随后第二个 peer（新 caller socket → 新 SocketAddr + **fresh `CountingBackend`**，R5）完整握手 + 喂包成功；phase 5 后 `backend2.resolve_calls == 1`（跨两 backend 总计 = 2）。

### T2 `relay_down_at_session_end_keeps_streaming`（AC1 的 422/403/backoff 终态模拟）

- `CountingBackend { finalize_delay: 500ms, finalize_broken_writer: true, ... }`（**C2 修正**：finalize 返回 `()`，误差 = 停滞 + 内部 finish 失败被吞，镜像 `finalize_session` warn-swallow :656-665）。
- 流程（真实时钟，plain `#[tokio::test]`）：
  1. 握手 → Streaming；
  2. 喂 16 包（全部 `timeout(100ms)` 必过、段增长、`finalize_calls == 0`）；
  3. **显式注入损坏（R7）**：`fs::remove_dir_all(hls_dir)`——测试持有 tempdir；writer 无粘性错误态、句柄每次调用即开即弃（aero-live-hls/src/lib.rs:138/:200），删目录后下一次 `File::create` 必 `NotFound` → `finish()` 内部 Err。**没有这一步，`finalize_broken_writer` 对健康 writer 的 finish 返回 Ok，`let _` 吞的是 Ok，Err 分支从未执行（测试空转通过）**；
  4. 发送 SHUTDOWN 控制包（`shutdown_control_datagram`，§2.2）→ `tokio::time::timeout(Duration::from_secs(2), handle_datagram(...))` **必不超时**——界 = **2s 而非 500ms**（R2：500ms 停滞 + broken-writer finish 在真实时钟上可超 500ms，精确边界即 flake；2s = 4× 余量）。可选强化：`std::time::Instant::elapsed() ≥ 400ms` 证明停滞确实被支付（防未来误删停滞注入后测试仍绿）；
  5. 断言：
     1. `finalize_calls == 1`（teardown 只在 SHUTDOWN 边界）；
     2. `peers` 中该 peer 已移除（SHUTDOWN 臂在 `handle_datagram` 内自移除，lib.rs:496-500——与 T3 的循环臂不同，直驱可断言）；
     3. 第二个 peer（fresh `CountingBackend`，R5）完整握手 + 喂包成功（监听器存活）；
     4. 全程无 panic/Err 传播（错误 finalize 不咬媒体面）。

### T3 `relay_down_at_session_start_drops_only_that_peer`（media-plane fail-open 的启动面）

- 驱动：peer1 用 `CountingBackend { resolve_error: true, ... }` 握手 → conclusion → `handle_datagram` 返回 `Err(LiveError::UnknownStreamKey)`（resolve 拒绝传播；生产同款错误路径 = run_listener :396-402 的 drop 臂）。
- 断言：
  1. peer1 的 `handle_datagram` 返回 Err（`LiveError::UnknownStreamKey`）；
  2. **直驱语义修正（R8）**：resolve Err 时 peer1 在 `peers` 中**仍留 `Handshaking` 条目**（`*entry = PeerState::Streaming` 只在 resolve 成功后发生，lib.rs:537-566；条目移除是 `run_listener` 错误臂的职责 :396-402）——断言条目仍在且状态为 `Handshaking`，**不**断言已移除（直驱无循环，断言移除反而错误）；循环级移除由 B1 覆盖（真实循环 + 快 PG）；
  3. peer2 = **全新 `CountingBackend { resolve_error: false }`**（R5：单 `Mutex<Option<SrtSession>>` 槽只服务一次 resolve；"或计数至第二会话" 不可实现——第二次 resolve 必 Err "test session already consumed"）→ 完整握手 + 喂包成功——**单 peer start 失败不倒下监听器**；
  4. `resolve_calls` 跨两个 backend 总计 = 2（每 peer 恰一次）。

### T4 `cargo_toml_stays_audit_and_relay_free`（静态守卫，R4）

```rust
const CARGO_TOML: &str = include_str!("../Cargo.toml");

/// 依赖键规范化（scripts/dependency-check.sh:29 同款语义）：`aero-common.workspace = true`
/// → 取 `=` 前段、再取首个 `.` 前段 → `aero-common`。整文件扫描（含 [target...dependencies]
/// 段，防 cfg 段绕过 take_while）；dev-deps 无 aero-*（tokio/tempfile/sqlx，已验证）不会误捕；
/// 注释行先行过滤。
fn dep_keys() -> Vec<String> {
    let mut keys = CARGO_TOML
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| {
            let key = l.split('=').next()?.trim();
            let key = key.split('.').next()?.trim();
            key.starts_with("aero-").then(|| key.to_string())
        })
        .collect::<Vec<_>>();
    keys.sort();
    keys
}

#[test]
fn cargo_toml_stays_audit_and_relay_free() {
    // allowlist 集合相等（排序后比较：多一个 aero-* = 红；少一个 = 红；合法重排不误红）。
    assert_eq!(
        dep_keys(),
        ["aero-common", "aero-live-core", "aero-live-hls", "aero-storage"]
    );
    // 改名绕过守卫（R4）：`my-connector.workspace = true` + root `package = "aero-audit-connector"`
    // 的键不是 aero-*——dependency-check.sh 的键-only sed 同样看不见；再扫 [dependencies] 段内
    // 所有 `package = "..."` 值。
    let section = CARGO_TOML
        .lines()
        .skip_while(|l| !l.starts_with("[dependencies]"))
        .take_while(|l| !l.starts_with('[') || l.starts_with("[dependencies]"));
    let renamed: Vec<&str> = section
        .filter_map(|l| {
            let rest = l.split("package = ").nth(1)?;
            let pkg = rest.trim().trim_start_matches('"');
            let pkg = pkg.split(['"', ',', '}']).next().unwrap_or(pkg).trim();
            pkg.starts_with("aero-").then_some(pkg)
        })
        .collect();
    assert!(
        renamed.iter().all(|p| {
            ["aero-common", "aero-live-core", "aero-live-hls", "aero-storage"].contains(p)
        }),
        "renamed aero dep bypass: {renamed:?}"
    );
}
```

（解析不引入 toml 解析依赖；`[dev-dependencies]` 段排除正确性 = 段内无 aero-*（已验）。**banned-token 循环已删除（R4）**：deps 预过滤为 aero-* 键，任何新 aero-* 先被相等断言红、非 aero-*（reqwest/hyper 等）被过滤不可见——循环在一切状态不可达；且 reqwest/hyper 经允许的 aero-storage（S3BlobStore，AGENTS.md §4.5）已是传递依赖，"禁 token" 是剧场。真实钉 = 排序集合相等 + scripts/dependency-check.sh:59。root Cargo.toml 不在扫描范围（改名须在 root `[workspace.dependencies]` 声明——若未来出现，`package =` 扫描已在 [dependencies] 段兜住）。）

### B1 `run_until_cancelled_boot_fails_open_without_audit_provisioning`（AC2，DB-gated）

- `#[ignore]` + 门控：`DATABASE_URL` 缺失 → `eprintln!("skipped: DATABASE_URL unset"); return;`（不红）。
- 步骤（throwaway 库纪律，AGENTS.md §4.3）：
  1. 管理连接 `CREATE DATABASE aero_live_srt_b1_<pid>`（用完 `DROP DATABASE`）；
  2. `aero_storage::db::migrate(&pool)`（唯一 migrate 入口，db.rs:54）+ `StreamRepo::new(pool)`；
  3. `StreamRepo::create(NewStream { owner_id, room_id: None, title, protocol: StreamProtocol::Srt, stream_key: Some("b1/e2e-key") })`（NewStream 字段 aero-storage/stream.rs:15-22；`StreamProtocol::Srt` 在 aero-common/model/media.rs:27）；
  4. **探 SRT 端口（R3）**：`std::net::UdpSocket::bind("127.0.0.1:0")` 探得端口 **P**（即 SRT 实际监听口）→ drop → `rtmp_listen = SocketAddr::new(ip, P - 1)`（tests.rs:909-916 模板逐字语义；`listen_addr` = rtmp+1，lib.rs:254-260——探 P 而把 P 用作 rtmp_listen 会让实际绑定的 P+1 未被探测）；
  5. `let handle = tokio::spawn(SrtIngest::new().run_until_cancelled(repo.clone(), Arc::new(cfg), cancel.clone()))`；测试持 `pool.clone()`（teardown 需用，R3）；
  6. UDP 直驱完整握手（`listener_test_induction` + conclusion + cookie）+ TS 喂包（含 IDR）；
  7. 断言：`repo.get_by_key("b1/e2e-key")` → `status == StreamStatus::Live`（mark_live 生效）；`hls_dir` 出现 `.ts` + `.m3u8`；
  8. `cancel.cancel()` → join 断言镜像 tests.rs:940-944 模板：`tokio::time::timeout(Duration::from_secs(5), handle).await.expect("cancelled listener should stop promptly").expect("listener task should not panic")` + `assert!(result.is_ok())` → `repo.get_by_key("b1/e2e-key")` → `status == StreamStatus::Ended`（mark_ended 落库）；
  9. **teardown 序列（R3，两路径都走）**：a) join 若超时（挂 PG/磁盘），先 `handle.abort()` + `let _ = handle.await` 再动 DB——否则残留任务持 StreamRepo 连接，`DROP DATABASE` 报 "being accessed by other users"（sqlx 空闲连接不释放）；b) **成功与失败两路径**都先 `pool.close().await` 再 `DROP DATABASE`（`status == Ended` 断言在 join 成功后、close 前执行）；
  10. **零 audit 配给面**：全程无 `AERO_AUDIT_*` env、无 audit 配置；crate 零 audit 符号（E5）即构造性证明（doc 注释 + T4 双钉）。

## 5. Failure modes & mitigations

| # | Failure | Behavior | Mitigation / invariant |
|---|---|---|---|
| F1 | CI 慢机 timeout flake | 真实时钟 + **构造性确定性**（停滞永不进入有界区域，F6）+ 余量（≥100× / 4×，见 C8）。`start_paused=true` 被明确拒绝（0.3 裁定 1）：auto-advance 在 runtime 空闲时跳到最早 pending timer——`push_segment`（tokio::fs）pending 时 100ms 定时器必先触发（T1 断言 3 恒败），T2/B1 同型。断言形式 = "不随 backend 停滞增长"，非绝对延迟测量；超时网只捕 >100ms 新增 await，亚界 await 由总 `calls` 计数器 + 结构性证明兜（F10） | C8；0.3 裁定 1 |
| F2 | B1 无 DATABASE_URL 环境 | `#[ignore]` + skip（不红） | C7；显式 eprintln skip 标记 |
| F3 | B1 污染共享 dev 库 | throwaway `CREATE DATABASE`/`DROP DATABASE` + 唯一迁移入口 `aero_storage::db::migrate` | AGENTS.md §4.3 硬纪律；测试内 try/finally 清理 |
| F4 | B1 端口冲突 | 探空闲端口（bind :0 → drop probe）后使用 | tests.rs:909-916 模板 |
| F5 | B1 失败时 spawn 任务残留 | cancel + `timeout(5s, join)`（镜像 tests.rs:940-944）；join 超时 → `handle.abort()` + `let _ = handle.await`；成功/失败两路径 `pool.close().await` 后再 `DROP DATABASE`（R3） | 防测试进程悬挂 + 防 DROP DATABASE 被残留连接阻塞 |
| F6 | resolve 停滞 500ms 使 established datagram 超时（T1 断言误伤） | 100ms 界**只作用于 streaming 后的数据包**；established datagram 单独处理（允许 500ms+，且断言 `resolve_calls==1` 证明只付一次） | T1 断言范围精确化（§4） |
| F7 | helper 复制与 tests.rs 漂移（语义悄悄变弱） | 复制顶部注释指向 canonical 位置；T1-T3 断言是行为级（计数/超时/段数），helper 漂移会显式红 | §2.2 复制纪律 |
| F8 | T4 误红（Cargo.toml 合法重排/注释/点键） | 键规范化（`=` 前段 → `.` 前段，dependency-check.sh:29 同款）+ 注释先行过滤 + **排序后集合相等**（防合法重排/点键误红）；`package=` 改名扫描只在 `[dependencies]` 段内（dev-deps 排除正确） | §4 T4 实现细则 |
| F9 | 与在途 B5 切片（aero-audit-connector 等未提交）冲突 | 本设计零共享文件（唯一落点 = 本 crate 新测试文件 + lib.rs 一行 cfg 挂载） | §3 C5；`git diff --stat` 守卫 |
| F10 | 未来 B5 误插 feed/pump（enqueue/await 进 per-datagram 路径） | 依赖守卫 T4 + dependency-check.sh:59 先红；**总 `calls` 计数器红**（sync `try_send` / 带默认体的新 seam 方法：无 await、无新依赖、ns 级——超时断言看不见，`calls` 看得见；per-datagram 断言 `calls==0`）；结构性证明（无句柄 + pump sync）；R1.2 契约评审 | R1.2 契约 + T1 断言 2/3 + T4 双钉 |
| F11 | 会话 start/end 停滞期间的**跨 peer 有界黑障**（srt_protocol (c)，R8） | `run_listener` 在循环体内联 await `backend.finalize`（SHUTDOWN 臂 lib.rs:501、错误臂 :397、drain :419）与 `backend.resolve`（established 臂 :537）——停滞期间不 poll `sock.recv_from`（:382）、不发 keepalive、不推 HLS：**所有 peer** 黑障 = 停滞时长；入站 datagram 排 UDP socket 缓冲、溢出即丢。**有界性**：本 crate 无重试/退避（B5-2 的 300s cap 在 connector 侧），今天 = LocalFs finish + 一次 `mark_ended` 往返；R1.2 契约（enqueue 只许 `try_send` + worker 在循环外）禁止更长的循环内 await——**契约是防无界增长的守卫**。零 error/panic 传播（warn-swallow）。**覆盖边界**：T1–T3 直驱 `handle_datagram` 不覆盖循环级并发；B1 跑真实循环（快 PG）但无 per-datagram 延迟断言——AC1 措辞已限界（§7）；强主张需 `run_listener_with` seam（§8 follow-up，本 direction 不建） | R1.2 契约；AC1 限界措辞 |

## 6. Migration steps

**本 direction 无 DB 迁移**（纯测试 + 契约；`cargo build` + 测试即完成装配）。若并行切片先行落地 `0239_audit_governance_outbox.sql`（B5-1），遵守 §4.2 硬规则：**加迁移后必先 `cargo build` 再 `aero-cli migrate`**（`sqlx::migrate!("../../migrations")` 编译期嵌入）——与本 direction 无关，但不互相干扰（本 direction 不碰 migrations/）。

| Phase | 内容 | 验收 |
|---|---|---|
| 1 | lib.rs 追加 `#[cfg(test)] mod isolation_tests;`（生产代码唯一 diff） | `cargo check -p aero-live-srt` 绿 |
| 2 | isolation_tests.rs：helper 复制 + CountingBackend + T1-T3 | `cargo test -p aero-live-srt isolation_tests` 绿（hermetic，无 DB） |
| 3 | T4 静态守卫 | 随 phase 2 绿 |
| 4 | B1 DB-gated（throwaway 库） | `DATABASE_URL=... cargo test -p aero-live-srt -- --ignored isolation_tests::boot_fails_open` 绿 |
| 5 | 门禁全跑（§7） | 全绿 + no-touch 守卫 |

## 7. Testable acceptance mapping

| Acceptance（requirements spec 原文） | 断言（testable form） | Location / harness | Gate |
|---|---|---|---|
| **AC1** simulate relay-down (connector 422/403/backoff per B5-2) while established session feeds → per-datagram latency bound holds + segments keep flowing + no storage/relay await in feed_packet/pump（**限界措辞 R8**：relay-down 在会话 end **不传播 error/panic、不入 per-datagram 路径**；他 peer 仅见 = finalize 时长的**有界停滞**（今天 = 一次 DB 往返；R1.2 契约禁止更长循环内 await），随后**全保真恢复**） | (T1) streaming 期间 `resolve_calls==1 ∧ finalize_calls==0`（快照先于第二 peer 阶段）；32 数据包每包 `timeout(100ms)` 必过；段数增长（16→32 包后 `.ts` 计数递增，≤5 段 < `LIVE_WINDOW_SEGMENTS=6` 无驱逐）；(T2) `remove_dir_all` 注入 + `timeout(2s)` SHUTDOWN 界 + 停滞被付证明（elapsed ≥400ms）、peer 移除、监听器存活；(T3) start 失败只掉该 peer（直驱断言 `Handshaking` 残留；循环级移除 = B1）；(T4) 依赖键排序集合相等 + `package=` 改名扫描；(by construction) SrtSession 无 repo 句柄 + pump sync | `src/isolation_tests.rs`；`cargo test -p aero-live-srt`（hermetic） | G6 |
| **AC2** boot test：`run_until_cancelled` 全握手 + TS feed 零审计配给（媒体 fail-open = 今天行为） | (B1) throwaway DB：`create` 预插 → spawn `run_until_cancelled`（探 SRT 端口 P、`rtmp_listen = P-1`）→ 完整 HSv5 握手 + TS 喂包 → `get_by_key.status == Live` + `.ts`/`.m3u8` 产物 → cancel → join 模板断言（expect×2 + `is_ok`）→ `status == Ended` → 两路径 teardown（join 超时则 `abort()` + await；`pool.close().await` 后 `DROP DATABASE`）；零 audit 配给面（无 `AERO_AUDIT_*`、无 provisioning 参数——E4/E5 构造性 + T4 静态）；既有锚点保绿：tests.rs:459（加密握手+媒体零 DB）、:909（boot+cancel 零配给） | `isolation_tests.rs` `#[ignore]` + `DATABASE_URL` + throwaway 库；`cargo test -p aero-live-srt -- --ignored` | G6 + §4.3 纪律 |
| **AC3** 37/37 套件保绿 + 热路径零新 DB/relay 依赖 | tests.rs 37 + rotation_tests.rs 5 **原样保绿**（零编辑，S1 行数纪律）；新增仅 T1-T4+B1（42+N）；`Cargo.toml [dependencies]` 冻结（T4 + dependency-check.sh:59 + aero-eng `gate deps`/`deps-native` 绿）；per-datagram 路径生产代码零改动（R6） | `cargo test --workspace --lib`（含 `-- --ignored`）· `cargo check --workspace` · `cargo clippy --workspace --all-targets`（无新警告）· `bash scripts/dependency-check.sh` · `aero-eng gate deps` · `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· `git diff --stat` 不含 boot/ingest.rs、dependency-check.sh、test-integration.sh、aero-storage、migrations/、aero-server 生产代码；lib.rs diff = 仅一行 cfg | G6 |

## 8. Out of scope（boundary reminders）

- **同 analysis direction #1（事务化 session-end + L1 summary）**、**#3（boot 配给门验证）**：不建。R1.2 恰为它们划好边界（enqueue 只许 start/end + bounded channel）。
- **B5-1 outbox / 0239 DDL / aero-storage**：`mark_live` in-tx CTE 已存在（E11）；B5-1 落点在 aero-ai/storage 侧。
- **B5-2 connector / B5-4 provision-check / B5-3 moderation priority**：sibling 切片；本设计只消费其语义做 SessionBackend 模拟（E9）。
- **bounded-channel worker**：不预建（零生产者 = 死代码，truth-check 红，AGENTS.md §4.4）；R1.2 是契约，T1/T4 钉边界。
- **`run_listener_with(ingest, backend, cancel)` 后端注入 seam**（0.3 裁定 2）：不建——违背 R6/C1 零生产改动（触碰 `run_listener` 签名与 no-touch 清单）。若未来要求「循环级并发 fail-open」强主张，列为 follow-up；本 direction 以 F11 + AC1 限界措辞诚实覆盖，不假装 T1 覆盖循环级。
- **tests.rs / rotation_tests.rs 编辑、生产代码改动、新 env、新迁移**：全部禁止（§3）。
- **真实 relay/IdP 联调**：仓外（proposal :13，[PROPOSED]）。

## 9. Sequencing

1. lib.rs 挂载一行 `#[cfg(test)] mod isolation_tests;`（生产代码零改动）。
2. `src/isolation_tests.rs`：helper 复制 → CountingBackend → T1 → T2 → T3 → T4；`cargo test -p aero-live-srt isolation_tests`。
3. B1（DB-gated，throwaway 库）：`DATABASE_URL=... cargo test -p aero-live-srt -- --ignored`。
4. 门禁全跑（§7 表格）：workspace test/check/clippy + 脚本族 + no-touch 守卫。
5. 行号漂移纪律：本文件全部行号为 2026-08-07 核对锚点；实现与后续引用以**符号名**为准（AGENTS.md §0）。
