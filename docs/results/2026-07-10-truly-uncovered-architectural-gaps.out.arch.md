# 架构分析：基于「五个未覆盖方向」审查的深度评估

> **分析范围**：以审查文档《Five Uncovered Directions Review》中确认的事实（正确和错误均标注）为基础，结合对 Aero IM 代码库结构的理解，从架构师视角提出系统性的评估与方向建议。

---

## 1. 架构评估

### 1.1 当前架构的核心优势

| 优势 | 代码证据 | 架构价值 |
|------|---------|---------|
| **事件驱动 + 进程内扇出分离** | NATS JetStream durable consumer → `Hub::fan_out_raw` → bounded mpsc → WS | 水平扩展的瓶颈在 NATS，每实例的 Hub 是无状态扇出，天然可扩 |
| **Crate 单向依赖** | 自下而上：common → bus → storage → auth → signaling → im-core → server | 编译边界清晰，按功能裁剪 crate 可行；`aero-common` 是真正的叶子 |
| **Canvas op log 采用 gap-free seq 追加** | `canvas_op.rs` 行锁 append + `ops_since` delta | 已为 CRDT/OT 准备好基础设施——冲突检测可基于 seq + expected_version |
| **SFU 纯 Rust 实现** | str0m 驱动的 `SfuMediaSession` + `SfuForwarder` | 无 GPL/AGPL 依赖问题（对比 mediasoup/libwebrtc），可嵌入同一进程 |
| **Block enum tagged by serde** | `#[serde(tag = "type")]` 在 `block.rs:65` | 虽无 Unknown 兜底，但 tag 模式是正确起点，修复方向明确 |

### 1.2 关键设计缺陷与架构债务

#### P0 — 阻塞性缺陷

| 缺陷 | 位置 | 后果 |
|------|------|------|
| **`Block` enum 无 `Unknown` 变体** | `common/src/model/block.rs` | 旧 sever 遇到新 client 发的未知 block type → serde 抛错 → 本轮房间事件解码失败 → 静默丢事件。本质是一个**前向兼容性炸弹** |
| **WS 发送失败路径断裂** | `web/ws.js:send()` 返回 `false`，`app.js:sendMessage` 不检查返回值 | 消息"发送成功"后永远消失在乐观 UI 里——比报错更糟。这是**可靠性契约断裂** |
| **无存储配额** | 全库无 `storage_bytes_limit` 引用 | 任何注册用户都能通过 1MB/s 脚本填满磁盘，且无限流。这是**无认证的 DoS 向量** |

#### P1 — 需要重视的债务

| 债务 | 说明 |
|------|------|
| **Canvas 后端骨架未接线** | `canvas.rs` + `canvas_op.rs` 已建，但 `ws/ws_impl/` 无 canvas 帧类型，`web/` 无 UI。状态：**服务器端 RPC 而无客户端调用方**，既是债务也是机会（分阶段交付可行） |
| **CI integration test 全注释** | `.github/workflows/ci.yml` 所有集成步骤以 `#` 开头 | 新鲜部署验证只能靠手动 `make migrate-smoke`。合 PR 前不知道 schema 迁移是否损数据 |
| **通话录制完全空白** | `SfuForwarder::on_rtp` 仅是转发 + RTCP，无 `RecordingSink` 钩子。对于合规场景（FINRA/HIPAA）是 blocker |
| **Cargo workspace 未用 `--package` 隔离 CI** | 当前 CI 跑全部 workspace，单 crate 失败全红。应并行 matrix |

### 1.3 技术债量化评估

| 类别 | 估计消除工时 | 严重程度 |
|------|------------|---------|
| Block Unknown 变体 | 8h（Rust 侧 + serde 自定义反序列化 + JS default 分支升级） | **致命** |
| WS 发送错误处理 | 4h（JS 重试队列 + 错误提示） | **高** |
| 存储配额 | 24h（计数 + 拦截 + 迁移 + `recalc` 定时器） | **高** |
| 通话录制 MVP | 40h（raw RTP dump + API + CLI 查看） | **中** |
| CI integration test 激活 | 4h（YAML 取消注释 + 清理数据库 setup） | **高** |
| 属性/模糊测试 | 40h（proptest harness + fuzz target） | **中** |

---

## 2. 扩展方向（架构层建议）

### 方向 A：Block 版本墙系统 — 分布式数据类型的版本演进框架

**为什么需要**：当前 Block enum 的 serde tag 模式在处理未知变体时脆弱——**这是 IM 平台的阿喀琉斯之踵**。一个 Web SPA 可能比服务器版本更新（CDN 缓存）/更旧（Service Worker），Block 变体每新增一个就可能造成一段时间的解码故障。

**核心挑战**：
1. serde 的 `#[serde(tag = "type")]` 遇到未注册 tag 直接抛 `unknown variant`，没有降级路径
2. 客户端与服务器的版本部署不同步，尤其 CDN 缓存 + Service Worker 使 JS 发布滞后
3. Web 端已有 `default` 分支处理未知 type，但 Rust 端没有对应——`Block` 作为 `RoomEvent` 的一部分，服务器解码失败意味着整条消息丢失

**建议方案**——三层兼容策略：

```
层1（立即）：Block::Unknown { type_name: String, payload: serde_json::Value }
  ├── 自定义 Deserialize 引擎：遇到未知 tag 兜底到 Unknown
  ├── 未知 block 写入 DB 存储原始 payload
  └── 扇出时原样传递（客户端已有 default 分支渲染占位符）

层2（短期）：添加 #[serde(deny_unknown_fields)] 到已知变体 → 迁移期手动剥离
  └── 防止旧变体悄悄吞掉新字段

层3（中期）：BlockVersionPolicy trait
  ├── ImService 可配置 version_gate: AcceptAll | RejectBelow(ver)
  └── 在 publish_room_event 入口检测是否接受此 block type
```

**架构影响**：
- 仅影响 `aero-common`（无上游依赖），变更局部化
- `Unknown` 变体在 match 中需要处理，但可以 panic/error，不影响其他分支
- DB 中 `blocks` 列（JSONB）天然存储额外字段，无需迁移

**可裁减方案**：
- 选项 A（激进）：用 `#[serde(untagged)]` 在 `Block` 级包裹 `#[serde(tag)]` 内部枚举。但 untagged 确定性差，不推荐。
- 选项 B（推荐）：`#[serde(tag = "type")]` + `#[serde(other)]` 在 `BlockKind` 而非 `Block`。保持 `Block` 结构体不变，内部 `kind: BlockKind` 枚举兜底。这是最小侵入方案。

---

### 方向 B：存储配额与资源核算子系统

**为什么需要**：当前无限制的存储是架构级风险——不仅是 DoS，也是**经济模型缺失**。一个 IM 平台按存储计费（Slack $8/user/month 含 10GB）需要配额核算。

**核心挑战**：
1. `BlobStore` trait（LocalFs / S3BlobStore）是纯存储抽象——不知道用户、工作区、房间的配额上下文
2. 配额检查横跨多个入口点：消息附件、头像、直播录制，各自独立
3. 精确计数需要事务一致性（上传 + `storage_bytes_used` 递增在一个事务），但在 S3 场景下无法本地事务

**建议方案**——分层配额架构：

```
┌─────────────────────────────┐
│    ResourceGuard 中间件      │ ← Axum 层：检查限额，失败回 413
│  ┌───────────────────────┐  │
│  │ QuotaStore            │  │ ← Redis pipelined incr + multi-key
│  │  - per-ws bucket      │  │    最终一致：不阻塞写路径
│  │  - per-user bucket    │  │
│  │  - global bucket      │  │
│  └───────────────────────┘  │
│  ┌───────────────────────┐  │
│  │ Recalibrator (timer)  │  │ ← 每 15min SUM(blob_size) 重新校准
│  │  - 比精确事务更稳健    │  │    处理反序列化/孤儿 blob
│  └───────────────────────┘  │
└─────────────────────────────┘
         │
         ▼
┌─────────────────────────────┐
│    BlobStore trait          │
│  ┌───────────────────────┐  │
│  │ LocalFs / S3BlobStore │  │ ← 不加配额逻辑，保持纯存储
│  └───────────────────────┘  │
└─────────────────────────────┘
```

**关键设计决策**：
- **最终一致计数器**（Redis INCRBY）而非事务 COUNT。理由：写路径不等待 COUNT 回写 → 上传延迟可控；`recalibrator` 兜底漂移。
- **配额档位**：按 `workspace.plan_tier`（free/pro/enterprise）而非硬编码。`plan_tier` 存 `workspaces` 表一个 TINYINT。
- **拦截点**：`BlobStore::store` 不拦（保持 trait 纯净），`ImService::send_message` / `RoomService::update_avatar` 等入口拦。

**技术选型**：Redis 作为配额计数器。理由：已有 Redis presence 基础设施（`fred 9`），重用连接池。不引入新依赖。

---

### 方向 C：Canvas 实时协作层（CRDT-Aware Op Sync）

**为什么需要**：Canvas 当前是服务器权威模型（`expected_version` 乐观锁 + 409 冲突），这对协作画布是零和博弈 ——「谁先保存谁赢」。真正的实时画布需要 OT/CRDT。审查确认 op log 基础设施已齐备（gap-free seq、delta catch-up），缺少的是 CRDT 合并 + WS 帧类型。

**核心挑战**：
1. 当前 op log 是 append-only 且按 **room scope** 排序，而 CRDT 需要 **per-document** 因果序
2. Rust 生态没有成熟的 CRDT 库如 yjs（JS Native）、automerge（Rust 有 but 不成熟）
3. yjs 集成需要 WebSocket aware 协议——当前 WS 无 Canvas 帧类型，需扩展 `ClientFrame`/`ServerFrame`

**建议方案**——分阶段实施：

```
阶段1（基础设施，2周）：WS 帧协议定义
  ├── ClientFrame::CanvasOp { doc_id, op_payload }
  ├── ServerFrame::CanvasOp { doc_id, seq, op_payload }
  └── server: canvas.rs → Hub::fan_out_raw 扇出

阶段2（CRDT 集成，3周）：yjs 作为 backend sync protocol
  ├── yrs（Rust yjs 端口）作为 lib
  ├── 每个 canvas document 一个 yrs Doc
  ├── 同步协议：yrs sync step 1/2（state vector + update）
  └── 持久化：定期 snapshot + op log 增量 replay

阶段3（UI，2周）：最小画布编辑器
  ├── tldraw / excalidraw 嵌入（已用 CDN 模式）
  ├── 退化到没有 CRDT 时的 expected_version 冲突模式
  └── 在线光标（√ 复用已有 presence）
```

**为什么选 yjs（yrs）而非 automerge**：
- `yrs`（Rust port of yjs）代码成熟度高于 automerge-rs，API 对齐 JS yjs 生态
- yjs 的 sync protocol 是 WebSocket 友好二进制（无需 JSON 序列化，Base64 编码后走 WS text frame 也可）
- 社区大：Obsidian、Notion、Linear 都用 yjs（JS 端），Rust 端 yrs 可与其互换 update

**架构影响**：
- `aero-common` 加 `yrs` 依赖（纯 Rust，无 C 依赖）
- 每个 canvas 文档在服务器端维护一个 `yrs::Doc` 实例——内存开销：空文档约 50KB，含 1000 op 约 200KB
- 内存风险可控：只有活跃画布（最后访问 < 1h）保留 Doc，冷文档从 DB snapshot 还原

---

### 方向 D：可录制 SFU 管道（Recordable Sink Adapter）

**为什么需要**：合规（FINRA 17a-4 记录保存、HIPAA 审计 trail、MiFID II 通话记录）是采购前置条件。代码库已有完整 SFU pipeline（`SfuMediaSession → SfuForwarder → local_subs + CallEgress`），缺的只是一个 `RecordingSink` 适配器。

**核心挑战**：
1. RTP 流是 per-participant 的，每个参与者单独 Opus/VP8/H264 编码——不是混合音频
2. 可回放录制需要多路复用容器（WebM 或 mkv），而不仅仅是 RTP dump
3. 混合音频（MixMinus）涉及 DSP 层——完全不同的技术栈

**建议方案**——三层产物：

```
层1（MVP，2周）：Raw RTP per-participant dump
  ├── Sink trait: trait RecordingSink { fn write_rtp(&self, ssrc, rtp: &[u8]); }
  ├── FileRecordingSink: 写入 /recordings/{call_id}/{ssrc}.rtp
  ├── RTCP SR 跟踪：记下 NTP → RTP timestamp 映射（恢复同步需要）
  └── 无转码、无混音、无容器。但合规 audit 可证明"原始流已保存"

层2（中期，4周）：可回放录制（WebM 容器）
  ├── webm 容器（muxer）遍历 per-ssrc RTP → Cluster 包装
  ├── Opus 头提取（带 Codec Delay / Pre-skip）
  ├── 参考：LiveStreamer 的 muxer 模式（已有 FlvToTsConverter 经验）
  └── 需要 libwebm 或手写 EBML——推荐 crates.io `webm`（纯 Rust，无 C 依赖）

层3（长期）：按需录制 API
  ├── REST POST /api/calls/:id/recording { action: "start" | "stop" }
  ├── 录制由 call lifecycle 驱动（启动时接入 Sink，结束时 finalize）
  └── 在 ImService::publish_room_event 层面判断合规策略自动启停
```

**关键设计模式**：Sink Adapter 而非修改现有 `SfuForwarder`。当前 `on_rtp` 持有 `egress: Option<CallEgress>`，再加上 `recording: Option<Box<dyn RecordingSink>>`。拓展示例：

```rust
// 现状
impl SfuMediaSession {
    fn on_rtp(&mut self, ssrc: u32, rtp: &[u8]) {
        self.forwarder.on_rtp(ssrc, rtp);        // 本地订阅者
        if let Some(egress) = &self.egress {
            egress.publish(ssrc, rtp);           // 跨节点
        }
    }
}

// 扩展后
impl SfuMediaSession {
    fn on_rtp(&mut self, ssrc: u32, rtp: &[u8]) {
        self.forwarder.on_rtp(ssrc, rtp);
        if let Some(egress) = &self.egress {
            egress.publish(ssrc, rtp);
        }
        if let Some(rec) = &self.recording {    // 新增：一行钩子
            rec.write_rtp(ssrc, rtp);
        }
    }
}
```

**技术选型**：
- 容器 muxer：`webm` crate（纯 Rust，MIT）vs `libwebm-sys`（C++ 绑定）。推荐纯 Rust 避免 cross-compile 问题。
- 混音（层3可选）：`audrey`（https://crates.io/crates/audrey）读取 + SPEEX DSP lib 或 `opussrc`。注意混音需要解码 + 重采样 + 编码，是 CPU 密集型——建议标记为专业版功能。

---

### 方向 E：测试基础设施现代化（CI-Integrated Property & Load Testing）

**为什么需要**：审查确认 CI integration test 全注释、无 fuzz、无 proptest。对于一款涉及实时推送、并发编辑、RTP 流处理的系统，测试覆盖现状构成**发布风险**。

**核心挑战**：
1. 集成测试需要 PG + Redis + NATS 实例——CI 环境准备开销大（启动 3 个 service）
2. RTP/media 测试需要虚拟编解码器或 mock——当前 CI 中 `#[cfg(test)]` 代码依赖编译特征而非运行时 gate
3. Hub fan-out / SeqGate 等并发组件需要属性测试验证不变量，而非单一样例

**建议方案**——三层测试策略：

```
层1（P0，1周）：CI Integration Test 激活
  ├── GitHub Actions service containers（PG 17 + Redis 7 + NATS 2.10）
  ├── 关键路径测试（不再注释）：用户注册 → 创建房间 → 发消息 → 扇出接收
  ├── DB migration 回放验证（落地到临时数据库，然后 DROP）
  └── 维护头不超过 20 行 YAML 变更

层2（P1，2周）：属性测试（proptest）
  ├── SeqGate：生成乱序 seq → 验证 SeqState 始终单调递增
  ├── Hub::fan_out_raw：随机生成 ServerFrame → 验证 bounded mpsc 不丢消息
  ├── bridge_frame 编解码往返：随机 payload → encode → decode → assert_eq
  └── Block serde：随机生成 Block tree → serialize → deserialize → assert

层3（P2，2周）：Load / Fuzz
  ├── cargo-fuzz target：WS ClientFrame 反序列化（fuzz 无效 UTF-8、超大数组）
  ├── cargo-fuzz target：RoomEvent bus 解码（fuzz 损坏的 NATS payload）
  └── k6 脚本：10 并发 WS 发送 30s（验证 Hub fan-out 延迟 P99 < 50ms）
```

**属性测试案例示例**（SeqGate 不变量）：

```rust
proptest! {
    #[test]
    fn seq_gate_dedup_invariant(seqs in prop::collection::vec(0u64..1000, 1..100)) {
        let mut gate = SeqGate::new();
        let mut delivered = Vec::new();
        for seq in seqs {
            if gate.accept(seq) {
                delivered.push(seq);
            }
        }
        // 不变量 1：delivered 严格单调递增
        for w in delivered.windows(2) {
            prop_assert!(w[0] < w[1], "SeqGate delivered non-monotonic: {:?}", w);
        }
        // 不变量 2：delivered 中无重复
        let mut deduped = delivered.clone();
        deduped.dedup();
        prop_assert_eq!(delivered, deduped);
    }
}
```

**技术选型**：
- `proptest` 已足够（无需 `quickcheck`）。proptest 的 `#![proptest_async]` 支持 tokio context，适用 Hub 测试。
- `cargo-fuzz` 需要 Nightly Rust——CI 可加一个 nightly matrix entry 专门跑 fuzz，不影响主 stable 构建。

---

## 3. 接口设计建议

### 3.1 关键 Trait 设计原则

#### `RecordingSink` — 最小接口 vs 全能接口

**选项 A（最小主义）**：
```rust
#[async_trait]
pub trait RecordingSink: Send + Sync + 'static {
    /// 写入 RTP 包（原始，不保证有序）
    async fn write_rtp(&self, ssrc: u32, rtp: &[u8]);
    /// 结束录制，Flush 所有缓冲
    async fn finalize(self: Box<Self>);
}
```
**优点**：容易实现，`FileRecordingSink` 只需 append + close。不影响现有 `SfuForwarder`。
**缺点**：无容器/混音能力，调用者需在外部编排多 SSRC。

**选项 B（富接口）**：
```rust
#[async_trait]
pub trait RecordingSink: Send + Sync + 'static {
    async fn write_rtp(&self, ssrc: u32, rtp: &[u8]);
    async fn write_rtcp(&self, ssrc: u32, rtcp: &[u8]);  // 用于 SR 时间戳
    async fn set_codec(&self, ssrc: u32, codec: CodecConfig); // Opus/VP8/H264 头
    async fn finalize(self: Box<Self>) -> Vec<RecordingSegment>; // 返回分段
}
```
**优点**：可产生标准容器（WebM/MP4），支持分段录制。
**缺点**：所有 sink 实现者必须处理 RTCP 和 CodecConfig。

**推荐**：先最小接口 MVP，再 trait 继承扩展。

#### `QuotaBackend` — 计数与检查分离

```rust
#[async_trait]
pub trait QuotaBackend: Send + Sync + 'static {
    /// 检查是否超出配额（快速路径，不计数）
    async fn check_quota(&self, user_id: &UserId) -> Result<QuotaStatus>;
    
    /// 消费配额（返回新的使用量）
    async fn consume(&self, user_id: &UserId, bytes: u64) -> Result<u64>;
    
    /// 释放配额（失败时回滚）
    async fn release(&self, user_id: &UserId, bytes: u64);
    
    /// 重新校准确认
    async fn recalibrate(&self, user_id: &UserId) -> Result<u64>;
}
```

**设计理由**：`check` 与 `consume` 分离允许乐观写入（先 write 再 consume），`release` 支撑写入回滚。`recalibrate` 定期从 `SUM(blob_size)` 找回真实值。

### 3.2 抽象层引入

是否需要新抽象层取决于：

| 场景 | 现有抽象 | 缺什么 | 决策 |
|------|---------|--------|------|
| Canvas CRDT | `CanvasOpRepo` | 缺乏 per-doc 的 CRDT state + sync protocol 抽象 | **推荐引入** `CanvasDocument`，封装 `yrs::Doc`，暴露 `apply_update` / `sync_step1` / `sync_step2` |
| 录制 | 无 | 需要 sink trait | **推荐引入** `RecordingSink` trait（见 §3.1），从 `SfuMediaSession` 中分离录制职责 |
| 配额 | 无 | 需要 `QuotaStore` | **推荐引入** `QuotaBackend` trait，但仅在 `aero-storage` 层实现 Redis 版，server 层用 middleware 调用 |
| Block 前向兼容 | 无 | 需要 `BlockVersionPolicy` | **不引入新 trait**——用 `#[serde(other)]` 或自定义 `Deserialize` 即可，server 层用 config 控制接受策略 |

### 3.3 向后兼容性策略

| 变更 | 兼容策略 | 回滚窗口 |
|------|---------|---------|
| Block 加 Unknown 变体 | serde `#[serde(other)]` 对旧 server 透明（旧二进制无 Unknown 变体则反序列化失败） | **需零停机部署**：先部署 New(Block::Unknown 变体)，稳定后清理老数据 |
| 存储配额拦截 | **先 Audit 模式**（日志配额越界但不拦截）→ **Enforce 模式**（返回 413）| Audit 模式跑 1 个 release cycle |
| WS Canvas 帧 | 新增 `ClientFrame`/`ServerFrame` variant，旧客户端忽略不识别的 server 帧 | 长期共存 |
| `RecordingSink` 集成 | `SfuMediaSession::new` 不传 `recording`（None）→ 行为不变 | 永久兼容 |

---

## 4. 技术选型与依赖评估

### 4.1 引入新依赖的必要性

| 依赖 | 用途 | 替代方案 | 推荐 |
|------|------|---------|------|
| `yrs`（Rust yjs port） | Canvas CRDT 后端 | automerge-rs / 自研 LWW-register | **推荐 yrs**（成熟度更高，与 JS 客户端共享 update 格式） |
| `proptest` | 属性测试 | `quickcheck` / 手写 property | **推荐 proptest**（已有 async 支持，策略组合器更丰富） |
| `webm`（纯 Rust EBML muxer） | 通话录制容器 | `libwebm-sys` C++ 绑定 / raw RTP dump | **推迟决定**——先用 raw RTP dump MVP，上线后再升级到 webm muxer |
| `k6`（CI 中） | 负载测试 | `drill` / `go-wrk` / 自写 tokio tcp | **推荐 k6**（JS 脚本友好，GitHub Actions 有官方 action） |

### 4.2 自建 vs 采购 vs 集成决策

| 功能 | 自建 | 集成 | 采购/SaaS | 推荐 |
|------|------|------|-----------|------|
| Canvas CRDT | 自建 LWW-register 复杂度低但缺乏收敛性保证 | yjs（yrs）集成 | 无合适 SaaS（Figma API 只读） | **集成 yrs** |
| 通话录制 | 原始 RTP dump 很简单，但可回放容器复杂 | webm muxer 集成 | Twilio 录制（但需迁移 SD-FU） | **自建 raw dump + 逐步升级** |
| 存储配额 | Redis 计数器 + recalibrator 约 200 行 | 无需外部集成 | Stripe Metering（费用高） | **自建** |
| 属性测试 | proptest 是 Rust 标准实践 | 无 | 无 SaaS | **集成 proptest** |
| CI 集成测试 | 容器化 service 是行业标准 | GitHub Actions service containers | 无合适 SaaS | **自配 CI service，零外部成本** |

### 4.3 技术栈变更影响

**yrs 依赖影响**：
- 编译：增加约 1000 行 Rust 代码编译（`yrs` 及其依赖 `lib0`、`yyy`）
- 运行时：每个活跃 canvas 文档约 50-200KB 内存（vs 无 canvas 时 0）
- 无 C 依赖，纯 Rust → 不影响 cross-compile 管线

**proptest 依赖影响**：
- 仅 `[dev-dependencies]`，不影响发布产物
- 编译测试二进制时增加约 3000 行（proptest + proptest-derive + bytes）
- 建议锁定版本（如 `0.10`）避免 API break

---

## 5. 实施路线图

### 5.1 优先级矩阵

```
优先级 = 风险降低价值 × 实施难度（反向）

高影响 / 低难度 ──────────────────────────────── 高影响 / 高难度
        │                                            │
  P0 ───┼── Block Unknown 变体 ──────────────────────│── 不适用
        │     (8h, 致命)                             │
        │                                            │
  P0 ───┼── WS 发送错误处理 ────────────────────────│── 不适用
        │     (4h, 高)                               │
        │                                            │
  P0 ───┼── CI 集成测试激活 ────────────────────────│── 不适用
        │     (4h, 高)                               │
        │                                            │
  P1 ───┼── 存储配额 Audit 模式 ───────────────────│── Canvas CRDT 层2
        │     (12h, 高)                              │     (3周, 中等)
        │                                            │
  P2 ───┼── 通话录制 Raw Dump ─────────────────────│── Canvas CRDT 层3
        │     (2周, 中)                              │     (2周, 低影响)
        │                                            │
  P2 ───┼── proptest 属性测试 ─────────────────────│── 通话录制 WebM muxer
        │     (2周, 中)                              │     (4周, 高)
        │                                            │
  低影响 / 低难度 ──────────────────────────────── 低影响 / 高难度
```

### 5.2 分阶段实施计划

#### 阶段 0 —— 止血（1 周，P0 全部）

| 任务 | 估计 | 风险 |
|------|------|------|
| `Block::Unknown` 变体 + 自定义 Deserialize | 2 天 | **零容量**：Block 是消息正文，反序列化失败 ⇒ 整条消息不可读。这是线上紧急级。 |
| WS `send()` 返回值检查 + 错误提示 | 0.5 天 | 用户体验：乐观 UI 吞消息比报错更糟。 |
| CI integration test 取消注释 | 0.5 天 | 维护：20 行 YAML 变更，gate 合 PR。 |
| `storage_bytes_used` 迁移 + Audit 计数 | 1.5 天 | 安全：从零计数到有计数，但暂不拦截。 |

**阶段 0 交付物**：`cargo clippy` 无新增警告 + CI 全绿 + 消息不会静默丢失 + 存储有计数。

#### 阶段 1 —— 基础设施（2-3 周，P1）

| 任务 | 估计 | 依赖 |
|------|------|------|
| 存储配额 Enforce 模式（413 拦截） | 3 天 | 阶段 0 计数完成 |
| 配额 recalibrator timer（每 15min） | 1 天 | 阶段 0 计数完成 |
| WS Canvas 帧协议定义（ClientFrame + ServerFrame） | 2 天 | 无 |
| Canvas CRDT 层1：yrs 集成 + per-doc state | 5 天 | 帧协议完成 |
| 通话录制层1：`RecordingSink` trait + `FileRecordingSink` | 3 天 | 无 |
| proptest: SeqGate + bridge_frame 属性测试 | 3 天 | 无 |

**阶段 1 交付物**：存储配额强制拦截上线（可配置阈值）→ 磁盘 DoS 向量关闭。Canvas 后端 CRDT 可处理并发操作（仍需 UI）。通话录制可产生 per-ssrc raw dump。

#### 阶段 2 —— 产品化（4-6 周，P2）

| 任务 | 估计 | 风险 |
|------|------|------|
| Canvas 最小 UI（excalidraw 嵌入 + WS 连接） | 2 周 | 客户端 CRDT 集成：yjs JS 端与 yrs 的 sync protocol 兼容性 |
| 通话录制层2：WebM 容器 muxer | 4 周 | 多 SSRC 同步（需 RTCP SR timestamp 映射）：PES 时序复杂 |
| cargo-fuzz: WS + bus 反序列化 | 1 周 | Nightly Rust CI gate |
| k6 负载测试脚本 + CI gate | 1 周 | 假套件维护：mock RTMP/WS 连接 |

**阶段 2 交付物**：Canvas 可编辑和实时协作。通话录制可回放（VLC/浏览器原生）。CI 包含 fuzz + load test gate。

### 5.3 风险矩阵与缓解策略

| 风险 | 可能性 | 影响 | 缓解 |
|------|--------|------|------|
| `yrs` vs `yjs` sync protocol 不兼容 | 中 | 高（Canvas 无法同步） | **层1 纯后端 CRDT 验证**：先单进程自测 yrs 文档双向合并，再用 Node.js yjs 客户端连接验证 |
| `webm` crate 维护不活跃 | 低 | 中（录制容器需换库） | **用 trait 抽象**：`ContainerMuxer` 接口可在 `webm` → `mp4` 间切换 |
| 存储配额 Redis 计数与 DB 差异过大 | 中 | 低（超额或不足） | **recalibrator** 每 15min 从 `SUM(blob_size)` 重置 + `recalibrate()` 手动触发 API |
| CI service container 资源不足（PG+Redis+NATS 同跑） | 中 | 高（CI 不稳定） | GitHub Runner 默认 7GB RAM，极限约 300MB/每个服务 → 够用。如不够：`docker compose` 而非 service container |
| Canvas UI 开发量超出预估 | 中 | 中 | **分段交付**：层1（纯 API 测试）→ 层2（tldraw/excalidraw CDN 嵌入）→ 层3（自定义编辑器） |

---

## 6. 总结

### 核心主张

1. **Block 前向兼容性是架构级紧急事项**。代码证据确认致命路径：新变体 → serde 失败 → 整条消息丢。这是 IM 系统的数据完整性风险，优先级最高。

2. **存储配额是安全架构 baseline**（非可选的 feature）。当前"任何注册用户都能无限制填满磁盘"是 DoS 向量而非存储策略问题。解决方案极低工程成本（Redis COUNTER + recalibrator，~200 行）。

3. **Canvas CRDT 是风险适中、价值高的中期扩展**。op log 基础设施的已到位意味着路径的 50% 已完成——缺的是 WS 协议集成和前端。`yrs`（Rust）与 yjs（JS）共享 update 格式，没有协议定义成本。

4. **通话录制从 raw RTP dump 起步，不要一步到位到可回放容器**。Raw dump（最小 `RecordingSink` trait + `FileRecordingSink`，~150 行）即可满足 FINRA 合规要求（只要能证明"原始流已保存"）。WebM muxer 可以在合规 audit 触发需求后再做。

5. **测试现代化是低投入、高回报**。20 行 YAML（CI 取消注释）即可修复最痛的缺口。proptest 为 SeqGate、Hub、bridge_frame 等核心组件提供不变量保证，与现有单元测试互补而非替代。

### 与现有架构的关系

```
            ┌────────────────────┐
            │    现有核心架构     │   ← 事件驱动、NATS、Hub 扇出、Crate 单向
            │ （功能完整，已交付） │
            └────────┬───────────┘
                     │
     ┌───────────────┼───────────────┐
     │               │               │
     ▼               ▼               ▼
┌──────────┐  ┌──────────┐  ┌──────────┐
│ P0 止血  │  │ P1 基建   │  │ P2 产品化 │
│ ──────── │  │ ──────── │  │ ──────── │
│ Block    │  │ 配额拦截  │  │ CanvasUI │
│ WS err   │  │ CanvasBD  │  │ WebM mux│
│ CI act.  │  │ 录制 raw  │  │ Fuzz    │
│ 配额计数  │  │ proptest  │  │ Load tst│
└──────────┘  └──────────┘  └──────────┘
```

所有五个方向都与现有架构正交——可以在不破坏现有功能的前提下并行推进。Block 修复只影响 `aero-common`；配额拦截只影响 `aero-storage` + server middleware；Canvas CRDT 作为新模块而非重构；录制 sink 作为 `SfuMediaSession` 的可选依赖注入；测试是 CI + dev-dependencies，零运行时影响。
