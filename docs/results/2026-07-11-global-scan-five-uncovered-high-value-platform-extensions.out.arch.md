我仔细阅读了你提供的文档，以及项目现有的设计 Spec、AGENTS.md 和近期分析文档。以下是我的架构师分析。

---

# 架构师评审：五个扩展方向的深层分析与补充

> 评审对象：[文档] 逐方向评审与补充建议（以下简称「输入」）
> 基线：`2026-05-22-aero-im-design.md` + `AGENTS.md` + `2026-07-12` 系列分析文档
> 视角：架构评估 — 关注**跨方向模式**、**隐含架构债务**、**被低估的风险**、**执行级设计决策**

---

## 一、架构评估：对「输入」的元分析

### 1.1 输入的最大价值

输入最深刻的洞察不在于某个具体方向的技术细节，而在于它识别出了**一组跨方向的共性架构缺口**——这些缺口是五个方向都要面对的，且任一方向的实施都会因为它们的缺失而走弯路。

| 跨方向缺口 | 输入提及的位置 | 影响的方向 |
|---|---|---|
| **媒体编解码管线缺失** | 方向二：Safari `MediaRecorder` 回退需要服务端编码 | 方向一（ASR 多轨混流），方向二（视频/语音编码） |
| **幂等与补偿机制不统一** | 方向三：扣款需两阶段模式（reserve+settle） | 方向一（Bot crash rejoin），方向三（金融级），方向五（证据保全） |
| **状态恢复能力不足** | 方向一：Bot 崩溃后不知道自己掉线 | 方向一（会议 Bot），方向四（Bridge sidecar 状态恢复） |
| **可观测性基础设施未与功能绑线** | 跨方向遗漏章节 | 全部五个方向 |
| **合规/法律前置成本与工程成本分离** | 方向三：法律评审 + 审批在工程之外 | 方向三（货币化），方向五（CSAM 报告义务） |

### 1.2 输入的一个系统性盲区

输入在每个方向评审中独立分析了技术风险，但**缺少一阶系统性风险的识别：跨方向之间的资源竞争和架构冲突**。

最显著的例子是**方向一（会议 AI）** 和 **方向二（语音消息）** 对同一基础设施的竞争：

```
方向一要求：
· 每路通话 → 每 peer 独立音频轨 → ASR N 路并行
· 服务端实时转写 → 低延迟（<5s）

方向二要求：
· 语音消息上传 → 单轨 Opus/WebM → 服务端转写
· 非实时（可接受 10-30s 延迟）

冲突点：
· 方向一需要 SFU 提供 per-peer 音频导出（当前 SfuMediaSession 有 SfuPeer.on_rtp，
  但未被会议 bot 订阅）
· 方向二需要独立的编码管线（opus crate / ffmpeg）
· 两者共用 Whisper 模型加载 → 显存/内存竞争
```

如果在两个方向并行开发时不主动管理这种资源竞争，Phase A 结束后会出现「两套独立的 ASR 基础设施」——每套都在加载自己的 Whisper 模型、各自维护编码器池、各自处理 diarization。**架构债务在 Phase A 就埋下，Phase B 重建统一管线时成本翻倍。**

### 1.3 输入对架构债务的评估是否完整？

输入聚焦于新方向的技术风险，但对**既有架构债务如何影响五个方向的实施成本**着墨较少。结合我看到的 `2026-07-12-architectural-analysis-operational-maturity.md` 中已识别的债务：

| 既有架构债务 | 受影响最严重的扩展方向 | 具体影响 |
|---|---|---|
| 80+ 仓储共享单一 PgPool（无隔离） | 方向一（AI 会议） | ASR 结果写入 + 纪要持久化 + 实时搜索 + 动作项创建 — 四个不同优先级在同一池争抢 |
| 错误处理三层混合模式（`anyhow` 逐层丢失类型） | 方向三（支付） | 扣款失败需要区分 "余额不足" vs "系统错误" vs "交易已存在" — `anyhow` 无法表达 |
| `routes.rs` 单个函数 100+ `.merge` 调用 | 方向四（Bridge） | 新 bridge 的鉴权策略（Bridge secret vs JWT）需要在同一个函数里配置，增加误配置概率 |
| `down.sql` 全缺失 | 全部方向 | 每个方向都会引入新迁移；无法回滚意味着方向三（支付表）的 schema 错误可能导致数据不可恢复 |
| CI 不跑集成测试 | 方向一（会议） | WebRTC + ASR + 纪要管线端到端测试无法自动化，仅能手动验证 |

**结论**：如果先在 2-3 周内清理关键架构债务（PgPool 拆分、`down.sql` 补齐、CI 集成测试管线），五个方向各自的实施成本可以降低 15-30%。这是典型「先还债再扩张」vs 「边欠债边扩张」的 trade-off。

---

## 二、扩展方向：六个更深层的架构问题

### 2.1 方向一：会议 AI 智能管线——被低估的数据模型分歧

输入提到 diarization 的复杂性可以被简化——如果 SFU 能按 peer 分轨，diarization 变成路由问题。这个判断是正确的，但需要加上一条关键约束：**当前 `SfuMediaSession` 的数据流向决定了会议 bot 加入后会获得一份混流的 RTP 包序列，而非按 speak 时段分段的结构化输出**。

```
SfuPeerA 的 RTP ─┐
                  ├── SfuForwarder.on_rtp → 混流 → 写入 call_transcripts.raw_text
SfuPeerB 的 RTP ─┘
```

要变成：

```
SfuPeerA 的 RTP ──→ SfuForwarder 的分轨 Sink A ──→ ASR Worker A ──→ call_transcripts.speaker_id = A
SfuPeerB 的 RTP ──→ SfuForwarder 的分轨 Sink B ──→ ASR Worker B ──→ call_transcripts.speaker_id = B
```

这**不是 SFU 架构的改动**（SFU 已经是 per-peer 分轨的），而是**会议 bot 订阅 SFU 输出的方式**——当前 `SfuMediaSession.run()` 把所有 peer 的包送给 `forwarder.on_rtp`，而会议 bot 需要的是 `forwarder.for_peer(participant_a).on_rtp` 这种粒度的订阅接口。

**数据模型层面的分歧**：如果走前者（混流 ASR），`call_transcripts.speaker_id` 字段的含义是 ASR 模型推断的，不可靠；如果走后者的分轨方案，`speaker_id` 是系统已知的可靠信息。这两种方案的数据模型不同——前端消费 transcript 的方式也不同（混流方案需要前端做 diarization 着色）。

**推荐**：在 Phase A 就选择分轨方案，即使 Phase A 只支持 1:1 通话（两个 peer）。这样 `call_transcripts` 表在 Phase A 就使用可靠的 `speaker_id`，Phase B 扩展到群通话时架构不变。

### 2.2 方向二：语音/视频消息——存储模型的长期影响

输入提到了 MSL E2E 加密对语音消息的影响，并建议两个存储路径。但这个决策的影响比输入估计的更深远：

**挑战 1：Blob 生命周期管理**

当前 `Block::Voice { blob_id, ... }` 引用的 blob 在 `blob_gc_drain` 定时器中通过引用计数释放。语音/视频消息引入后，`blob_gc_drain` 的逻辑需要区分：

| Blob 类型 | GC 条件 | 备注 |
|---|---|---|
| 文件附件 | 无消息引用 → GC | 已有逻辑 |
| 语音消息（明文） | 无消息引用 → GC | 同上 |
| 语音消息（加密） | 无消息引用且 transcript 已写入 → GC | 加密后服务端无法转写，transcript 需客户端上传后写入 |
| 视频消息 | 需要先确认缩略图已生成 → GC | 新增依赖 |

这意味着 `blob_gc_drain` 需要一个依赖感知的 GC 策略，而不是简单的引用计数。

**挑战 2：缩略图生成的异步管线**

视频消息需要缩略图。当前系统没有视频帧提取能力。如果要服务端生成缩略图：

```
上传 → BlobStore 存储 → 消息发送 → 异步任务提取首帧 → 缩略图作为另 Blob 存储 → 消息结构体更新 thumbnail_blob_id → 广播 Edited 事件通知客户端更新
```

这个管线**与现存 `block_gc_drain` 共享 BlobStore 但互相不知情**——缩略图生成失败不算 Blob 泄漏（因为视频 blob 本身有引用），但重试逻辑缺失会导致一部分视频消息永远没有缩略图。需要引入 `video_thumbnail_queue` 表 + 定时重试。

### 2.3 方向三：货币化——「分期」建议本身的架构含义

输入建议将合规/审批时间与工程时间分离标注。我同意这个建议，但想强调一个更根本的问题：**合规前置时间不是串行阻塞工程进度的，它可以与工程 Phase A 并行**。

```
更好的模型：

Week 1-6 (并行)
├── 工程： Phase A（虚拟货币 + 礼物/订阅/打赏）
├── 法务： ToS 修改 + 虚拟货币条款
├── 业务： Stripe Connect 申请 + 税务表单模板

Week 7-10 (工程停顿等待合规交会)
├── 工程： Phase B（提现 → 真实货币）—— 不能提前开工，因为依赖 Stripe Connect 商户 ID
├── 法务： 最终 ToS 定稿
├── 业务： W-9/W-8BEN 税务收集 UI 定稿

Week 11-16 (合规就绪后)
├── 工程： Phase C（高级变现 → 付费订阅/直播打赏分成）
├── 法务： 分成协议条款
├── 运营： 创作者入驻流程
```

关键架构决策是：**Phase A 的虚拟经济数据模型必须与 Phase B/C 的真实货币体系兼容**。具体来说：
- `coins_transactions` 表的外键指向 `(participant_id, transaction_type)` — Phase B 需要扩展为 `(participant_id, transaction_type, currency, amount_usd_cents, settlement_id)`
- 礼物记录需要 `(giver_id, receiver_id, gift_type, coins_amount, ...)` — Phase C 分账时需要 `receiver_share_usd_cents` 字段

如果 Phase A 的数据模型不考虑这些扩展字段，Phase C 需要做数据迁移——在金融系统中做数据迁移是高风险操作。

**推荐做法**：Phase A 建表时预留可空字段：

```sql
CREATE TABLE coins_transactions (
    id UUID PRIMARY KEY,
    participant_id UUID NOT NULL REFERENCES participants(id),
    transaction_type TEXT NOT NULL, -- 'gift_send' | 'gift_receive' | 'subscription' | ... | 'payout' | 'refund'
    coins_amount BIGINT NOT NULL CHECK (coins_amount != 0),
    balance_after BIGINT NOT NULL,
    
    -- Phase B/C 预留（当前为 NULL）
    currency TEXT,                    -- 'usd' | 'eur' | ...
    amount_usd_cents BIGINT,          -- 等值美元（用于税务报告）
    settlement_id UUID,               -- Stripe Payout ID
    receiver_share_usd_cents BIGINT,  -- 创作者分账金额
    
    created_at TIMESTAMPTZ DEFAULT now(),
    metadata JSONB
);
```

这避免了金融数据的 schema 迁移。

### 2.4 方向四：Bridge 架构——Sidecar 的进程边界是「物理」还是「逻辑」？

输入建议 bridge 作为 sidecar 独立进程，通过 NATS 通信。这个方向是对的，但需要细化**进程边界的选择标准**：

| 维度 | 同一进程（逻辑边界） | 独立进程（物理边界） |
|---|---|---|
| 故障隔离 | ❌ Bridge 崩溃拖垮主服务 | ✅ Bridge 崩溃 → 仅该协议不可达 |
| 资源隔离（内存/CPU） | ❌ 共享 | ✅ 独立 |
| 通信延迟 | ✅ 直接函数调用 (<1ms) | ❌ NATS 往返 (~1-5ms) |
| 运维复杂度 | ✅ 单进程部署 | ❌ N 进程 + N 监控 + N 日志 |
| 模块热更新 | ❌ 重启整个 service | ✅ 可单独重启 bridge |

对于 Email Bridge（Phase A），**我建议用逻辑边界（同一进程中独立的 mpsc task）而非物理边界**，理由是：

1. Email 是 SMTP 协议——是短暂的 TCP 连接后断开的，不是长连接，不会持续占用内存
2. Email 发件失败不需要隔离——它不会导致主进程崩溃
3. 运维 6 个独立 bridge 进程（Email + Matrix + Discord + SMS + Webhook + WhatsApp）在项目早期太重

对于 Matrix/Discord 这类长连接、高内存占用的 Bridge（Phase B 起），再用物理边界。

### 2.5 方向五：信任与安全——输入最重要的补充是「内部威胁」

输入建议增加 Phase E（内部风险检测），这个建议非常正确。我更进一步：**内部威胁检测在 Phase A 就应该有数据采集**，而不是等到 Phase E 再做。

原因：内部威胁检测需要**行为基线**。正常行为数据只能在「安全阶段」采集，不能在「事件发生后」回溯。如果 Phase A 不采集：
- 员工正常搜索关键词的频率分布
- 员工正常下载文件的大小和频率
- 各工作区正常的消息量基线

Phase E 时这些数据不存在，无法建立异常检测的阈值。

**建议**：Phase A 增加一个低成本的 `audit_log` 表，记录：
```
audit_log (pid, action_type, resource_type, resource_id, ip, user_agent, created_at)
```
只 Append，不读（不影响性能），为 Phase E 提供基线数据。这个表在 Phase A 几乎零成本（PG 的 Append-only 写入极快），但为后续行为分析提供了无法回溯的数据基础。

### 2.6 输入遗漏的一个方向：多 Region / 数据驻留架构

输入对方向四（Bridge）的多协议支持覆盖了外部集成，但没有覆盖一个跟「Aero IM 作为 To-B 产品」高度相关的架构需求：**多 Region 部署与数据驻留**。

对于 To-B 协作工具，数据驻留（Data Residency）是企业签约的硬性条件（GDPR 要求 EU 数据不离 EU，中国等地的类似要求）。当前架构中：
- NATS 的 consumer 是全局的（`im.room.*`）
- Redis 的 presence/roster 是全局的
- PG 是单实例

要支持多 Region，需要引入**区域边界的概念**：

```
Region EU
├── PG EU (rooms, messages, participants)
├── Redis EU
└── NATS EU (im.room.* — 仅 EU room)
       │
       │ NATS Gateway（跨 region 只同步跨区房间的事件，不同步全量）
       │
Region US
├── PG US
├── Redis US
└── NATS US
```

这是一个大的架构变更，但值得在路线图中标识为「当项目进入企业签约阶段时的必要前置」。它的存在会影响方向三（支付/货币化在不同 Region 的法律合规不同）和方向四（Bridge 的邮箱地址跨 Region 路由）。

---

## 三、接口设计建议

### 3.1 需要一个「可插拔媒体处理器」抽象

方向一（会议 bot ASR）和方向二（语音/视频转写）都需要类似的功能：输入媒体流 → 处理 → 输出结构化结果。当前没有统一的接口：

```rust
// 当前（无抽象，各做各的）
impl transcribe_bot {
    async fn transcribe_voice(blob_id: BlobId) -> Result<String> // whisper
}

impl meeting_bot {
    async fn transcribe_peer(rtp_stream: RtpStream) -> Result<Vec<Segment>> // whisper diarize
}
```

建议引入：

```rust
#[async_trait]
pub trait MediaProcessor: Send + Sync {
    /// 支持的媒体格式
    fn input_formats(&self) -> Vec<MediaFormat>;
    
    /// 处理媒体并返回结构化输出
    async fn process(&self, input: MediaInput) -> Result<ProcessedOutput>;
    
    /// 估计处理成本（用于预算分配）
    fn estimated_cost(&self, input: &MediaInput) -> CostEstimate;
}

pub enum MediaInput {
    Blob(BlobId),           // 已存储的媒体文件
    Stream(RtpStreamId),    // 实时 RTP 流
    RawBytes(Vec<u8>),      // 内存中的媒体数据
}

pub struct ProcessedOutput {
    pub transcript: Option<String>,
    pub segments: Vec<Segment>,       // diarization 后的段落
    pub speaker_map: HashMap<SpeakerId, ParticipantId>,
    pub duration_ms: u32,
    pub metadata: serde_json::Value,  // 格式相关
}
```

这个抽象的收益：
1. `transcribe_bot` 和会议 bot 共享 Whisper 模型池（显存预算可统一管理）
2. 新增处理器（如翻译、情感分析）不需要改调用方
3. 媒体格式转换器（如 Safari PCM → Opus）作为 `MediaProcessor` 实现链

### 3.2 需要一个统一的状态恢复契约

当前系统没有「进程重启后主动恢复进行中任务」的模式。方向一（会议 bot）和方向四（Bridge）都需要这个能力。

建议引入 `ResumableTask` trait：

```rust
#[async_trait]
pub trait ResumableTask: Send + Sync {
    /// 唯一标识（用于 dedup）
    fn task_id(&self) -> TaskId;
    
    /// 当前状态（序列化后存入 state 表）
    fn serialize_state(&self) -> Result<Vec<u8>>;
    
    /// 从序列化状态恢复
    fn deserialize_state(data: &[u8]) -> Result<Self> where Self: Sized;
    
    /// 恢复后的行为（如重新订阅、rejoin 通话）
    async fn on_resume(&self) -> Result<()>;
    
    /// 优雅停止
    async fn on_shutdown(&self) -> Result<()>;
}
```

`background.rs` 在启动时扫描 `active_tasks` 表，对 status = 'running' 的任务执行 `deserialize_state()` → `on_resume()`。这统一了方向一的会议 bot、方向四的 bridge、以及未来所有需要状态恢复的功能。

### 3.3 `routes.rs` 的解耦方式

输入没有直接评论 `routes.rs` 的膨胀问题，但方向四（Bridge）的新路由会加剧这个问题。当前 `routes.rs::build()` 的 `~2854` 行（按 AGENTS.md 接近 3000 行上限）是架构脆弱点。

建议的分解方式——不是简单地 split 到不同文件，而是引入 **插件式路由注册**：

```rust
// 当前模式
pub fn build() -> Router<AppState> {
    Router::new()
        .merge(crate::auth::routes())
        .merge(crate::rooms::routes())
        .merge(crate::messages::routes())
        .merge(crate::bridge::routes())        // 新加
        .merge(crate::payments::routes())       // 新加
        // ... 100+ 行
        .layer(middleware)
}

// 更好的模式
pub trait RouteProvider {
    fn priority(&self) -> u8;          // 路由注册顺序
    fn routes(&self) -> Router<AppState>;
}

// build() 函数改为扫描所有 RouteProvider
pub fn build(providers: Vec<Box<dyn RouteProvider>>) -> Router<AppState> {
    let mut sorted = providers;
    sorted.sort_by_key(|p| p.priority());
    sorted.into_iter().fold(Router::new(), |acc, p| acc.merge(p.routes()))
}
```

这样新方向加路由时，只需实现 `RouteProvider` trait 并在 boot 时注册，不需要修改 `routes.rs`。这也能避免 merge 顺序导致的中间件覆盖问题。

---

## 四、技术选型

### 4.1 需要评估的关键新增依赖

| 功能需求 | 候选依赖 | 风险 | 建议 |
|---|---|---|---|
| 服务端 Opus 编码 | `opus` crate / ffmpeg 子进程 | `opus` crate 是绑定 C libopus，需要系统级 libopus-dev；ffmpeg 可执行文件依赖 | Phase A 先用 ffmpeg 子进程（低代码量，但需额外 Docker 构建），Phase B 如果成为瓶颈再换 `opus` crate |
| 视频缩略图 | `ffmpeg` (thumbnail filter) | 同上 ffmpeg 依赖，但需要 ffmpeg 的 `-vf thumbnail` 和 `-frames:v 1` 参数 | 同上，与 Opus 编码共用 ffmpeg 调用 |
| 设备指纹 | `fingerprintjs` (JS 端) | 无后端依赖，JS 端 MIT 许可 | 直接采用（已在方向五 Phase A 计划中） |
| 金融级扣款 | 无新依赖（PG 事务+FOR UPDATE） | 纯 SQL 方案，风险在 PG 故障处理 | 方向三不引入新依赖 |
| Bridge(Email) | `lettre` / `sendgrid-rs` | lettre 是纯 Rust SMTP 库，sendgrid-rs 依赖外部 API | 建议 lettre（纯 Rust，不依赖外部 API 的可用性） |
| Web Push | `web-push` crate | 成熟度一般，更新频率不高 | 可作为 N3-001 的第一个实现，运行时故障回退到 no-op |
| 行为分析/异常检测 | 无现成 Rust 库 | 业务逻辑太特定 | 自建（约 200 行 SQL + Rust 逻辑），不引入 ML 依赖 |

### 4.2 « 自建 vs 采购 » 的关键决策节点

方向三（货币化）的**支付提供商选择**是项目级的不可逆决策，其影响远超出方向三本身：

| | Stripe Connect | Paddle | Lemon Squeezy | 支付宝/微信支付 |
|---|---|---|---|---|
| 平台模式 | 市场平台 | 商家记录（Merchant of Record） | 商家记录 | 商家记录 |
| 税务处理 | 需自行处理 | 自动处理（VAT/Sales Tax） | 自动处理 | 不处理国际税 |
| 提现周期 | 7 天 | 7-14 天 | 7 天 | T+1 |
| 全球覆盖 | 46+ 国家 | 支持 Paypal + 信用卡 | 仅信用卡 + PayPal | 仅中国用户 |
| 企业支持 | 优秀 | 有 | 较小 | 有 |
| Rust SDK | 无（需 HTTP 封装） | 无 | 无 | 无 |

**架构影响**：选择 Stripe Connect 意味着需要自建税务表单 UI（W-9/W-8BEN），这反过来影响方向四（Bridge）中的出站邮件模板——因为税务表单的 Email 通知和提醒也是 Bridge 的一部分。这不是方向三内部的事。

**推荐**：对于 MVP 阶段，用 Stripe Connect 的标准 onboarding flow（跳转 Stripe 页面），不自建税务表单 UI。这样 Phase A 的工程成本降低约 4 周。等用户量大了再自建定制 onboarding。

### 4.3 关于 Media Server 的隐性依赖

方向一（会议 AI）和方向二（语音/视频消息）都假设服务端有媒体处理能力。当前架构中，媒体处理分散在：

```
aero-live-webrtc (str0m)   →  RTP 选择性转发（不做转码）
aero-live-rtmp (rml_rtmp)  →  RTMP 摄入 → HLS（只走 H.264/AAC）
aero-live-srt               →  SRT 摄入 → TS（同上）
```

**没有一个 crate 提供通用的媒体编解码能力**。如果要服务端做：
- 音频：PCM → Opus 编码 → WebM 封装
- 视频：H.264 解包 → 缩略图提取 → JPEG 编码
- ASR：音频 → Whisper 文本

需要的新能力是媒体编解码管线——这既不是一个 crate 也不是一个依赖能解决的，而是一组能力的组合：

```
ffmpeg CLI（子进程）
├── -f s16le -ar 16000 -ac 1 → PCM 16kHz 单声道（Whisper 标准输入）
├── -vf thumbnail → 缩略图
└── -c:a libopus → Opus 编码

OR

Rust crates
├── opus (C 绑定) → Opus 编码
├── image → JPEG 编码
└── symphonia → 解码（用于缩略图提取）
```

**选择建议**：Phase A 用 ffmpeg 子进程（最快实现路径），Phase B 按性能瓶颈分阶段替换为 Rust crate。ffmpeg 子进程的抽象层应该允许平滑替换：

```rust
trait AudioEncoder {
    async fn encode_pcm_to_opus(&self, pcm: &[u8], sample_rate: u32) -> Result<Vec<u8>>;
}

// Phase A 实现
struct FfmpegAudioEncoder;
#[async_trait]
impl AudioEncoder for FfmpegAudioEncoder {
    async fn encode_pcm_to_opus(&self, pcm: &[u8], _sample_rate: u32) -> Result<Vec<u8>> {
        // ffmpeg -f s16le -ar 16000 -ac 1 -i pipe: -c:a libopus -b:a 16k pipe:
    }
}

// Phase B 实现（可选）
struct OpusCrateEncoder;
#[async_trait]
impl AudioEncoder for OpusCrateEncoder {
    async fn encode_pcm_to_opus(&self, pcm: &[u8], sample_rate: u32) -> Result<Vec<u8>> {
        // 直接用 opus crate 编码
    }
}
```

---

## 五、实施路线图

### 5.1 优先级排序的重新评估

我认同输入对 Q3 并行路线的建议，但需要在一个关键维度上补充：**外部依赖性 vs 内部依赖性的视角**。

```
方向五 Phase A （设备指纹）：外部依赖 = JS 库，无后端变更，2 周

方向二 Phase A （语音 UX）：外部依赖 = opus/ffmpeg，前端工作重，6 周

方向一 Phase A （会议纪要重构）：外部依赖 = Whisper + 新数据模型，4 周

方向三 Phase A （虚拟货币）：外部依赖 = 无（纯 SQL），4-6 周

方向四 Phase A （Email Bridge）：外部依赖 = SMTP 服务器 / lettre，3-4 周
```

仅看工程维度，方向五 Phase A 是**最高 ROI**（2 周，无产品变更，为所有后续方向提供安全基础）。但方向二 Phase A 是**最高产品可见性**（用户能摸到的 UI 变更）。

**修改后的建议执行顺序**：

```
Weeks 1-2           Weeks 3-6            Weeks 7-10            Weeks 11-14
┌──────────┐        ┌────────────┐       ┌─────────────┐       ┌─────────────┐
│ 方向五-A  │        │ 方向一-A    │       │ 方向三-B     │       │ 方向一-B     │
│ 设备指纹  │        │ 会议纪要    │       │ 提现（法务   │       │ 结构化纪要   │
│ JS + 1列  │        │ 数据模型    │       │ 审批到位后） │       │ + 动作项     │
└──────────┘        └────────────┘       └─────────────┘       └─────────────┘
                      方向二-A            方向四-A              方向二-B
                      语音 UX 重做         Email Bridge          视频消息 + 波形
                      6周                  4周                    3周
                                          ┌────────────┐
                                          │ 方向五-B    │
                                          │ 年龄门控 +  │
                                          │ 举报工作流  │
                                          │ 3周         │
                                          └────────────┘
                                          方向三-A（第二阶段）
                                          真实支付集成
                                          4周（法务放行后）
```

**并行约束**：方向一（房间内 ASR 会议 bot）和方向二（语音/视频转写）可能共享 Whisper 模型实例，需要统一模型管理。如果两个团队并行，需要明确边界——方向一的模型面向实时流，方向二的模型面向静态文件。建议用 `aero-ai` crate 内的 `model_pool.rs` 统一管理 Whisper 进程实例。

### 5.2 里程碑定义

| 里程碑 | 时间 | 验收标准 | 依赖前置 |
|---|---|---|---|
| M1: 安全基础就绪 | Week 2 | 设备指纹已在 `sessions` 表采集；`audit_log` Append-only 表已建；会话并发上限已实施 | 方向五 2 周 sprint |
| M2: 语音产品化 | Week 6 | 录音 UX 重做（波形+预览+重录+播放速度）；`transcribe_bot` 覆盖 `Block::Voice` 转写；Safari 回退路径（PCM → ffmpeg → Opus） | M1（方向五已采集设备指纹，不阻塞） |
| M3: 会议基线 | Week 6 | 会议 bot 可以加入 1:1 通话并记录 transcript；`call_transcripts.speaker_id` 可靠；纪要摘要 API 存在 | M1 |
| M4: 虚拟经济上线 | Week 10 | 虚拟货币充值/消费/转账/余额查询；礼物/订阅/打赏；`CHECK (balance >= 0)` 强制执行 | M1 |
| M5: 邮件联通 | Week 10 | Email→Room 收发可用；身份映射已建立；退信/垃圾邮件反馈回路已处理 | M1 |
| M6: 结构化纪要 | Week 14 | 从通话记录提取 action items → `tasks` 表存在；annotations 结构化输出；会议室跨引用 threads | M3 |
| M7: 提现 | Week 14 | 创作者可提现虚拟货币为真实货币（由 Stripe Payout 驱动）；税务表单收集就位 | M4 + 法务放行 |

### 5.3 风险矩阵

| 风险 | 概率 | 影响 | 触发条件 | 缓解措施 |
|---|---|---|---|---|
| **Whisper 模型显存不足**（ASR 与转写共享 GPU 时） | 高 | 中 | 方向一、二并行开发，同一 GPU 上两个模型实例争显存 | Phase B 统一 `model_pool.rs` 管理模型生命周期，Phase A 各自独立部署（不同 GPU/节点） |
| **支付合规审批延迟**（Stripe Connect 平台审核 > 4 周） | 中 | 高 | Stripe 将 Aero IM 评为「高风险」（社交平台类别） | Phase A 先上虚拟货币（不涉真实货币），合规审批期间 Phase B 只用少量测试用户联调 Stripe sandbox |
| **Safari MediaRecorder 回退延迟不可控** | 中 | 中 | 用户上传的 PCM 文件很大（>10MB），ffmpeg 编码 + 网络上传叠加超出预期 | 限制录音长度（最多 5min），PCM 文件分片上传，服务端边收边编码 |
| **设备指纹在微信/支付宝内嵌浏览器失效** | 高 | 低（中国以外用户无影响） | 微信浏览器禁用 canvas/WebGL fingerprint | 对低熵指纹不信任但也不丢弃，结合 IP + 注册时间 + 行为模式做综合评分 |
| **方向一、二 ASR 管线回到方向三支付事务的隔离冲突** | 低 | 高 | 会议 bot 的回执写入 `call_transcripts` 与支付事务在同一个 PG 事务内混跑 | 确保支付事务使用独立的 PgPool（见 2.3 节），和实时 ASR 写入不共用连接 |
| **方向四 Email Bridge 被攻击者利用做邮件注入** | 中 | 高 | 攻击者通过 Bridge 发送伪造邮件（spoofing） | DMARC 校验 + SPF 记录 + 退信处理 + 内容扫描（复用方向五的内容安全审核） |

### 5.4 输入未讨论但关键的一个风险

**方向一到五全部增加了对 PG 和 Redis 的写入负载**，但当前架构中两者都缺乏明确的容量规划。

| 方向 | 新增 PG 写入 | 新增 Redis 写入 |
|---|---|---|
| 方向一 | 每段 transcript INSERT + action item INSERT | 通话状态（增加 entry） |
| 方向二 | 每段转写 UPDATE `messages.blocks` + blob 引用 | 语音上传进度 |
| 方向三 | 每笔 coins_transaction INSERT + balance UPDATE | 每小时热点礼物排行 |
| 方向四 | 每条桥接消息 INSERT + 身份映射 | 速率限制 + 退信计数器 |
| 方向五 | 每次举报 INSERT + 审核 UPDATE + jail 操作 | IP/设备指纹缓存 |

粗算：如果 Aero IM 有 10 万 DAU：
- 方向三：日交易量 5000-20000 笔（取决于货币化深度）→ PG 事务对 `coins.balance` 的 UPDATE 是行锁热点→ 需要 `balance` 切分到多个物理行（分片）或引入 Redis 临时余额
- 方向五：每用户每天 ~1 次举报审核 → 日新行 2-3 万 → PG 表无压力，但审核队列长度需要监控

**建议**：在发布任一方向前，先对 PG/Redis 做一次压力测试（pgbench + redis-benchmark + 模拟各方向写入模式）。目标是在开发环境中找到「哪个方向是第一个遇到 PG 写入瓶颈的」。

---

## 总结

| 维度 | 输入覆盖完整度 | 我的补充核心 |
|---|---|---|
| 架构评估 | ⭐⭐⭐⭐ 跨方向缺口识别准确 | 补充：既有债务对扩张成本的影响量化 |
| 扩展方向 | ⭐⭐⭐⭐ 五个方向深挖深入 | 补充：方向间资源竞争、对话 ASR 的数据模型分歧、多 Region 缺失 |
| 接口设计 | ⭐⭐⭐ 部分提及 | 补充：MediaProcessor 抽象、ResumableTask 契约、RouteProvider 解耦 |
| 技术选型 | ⭐⭐⭐ 方向一时提及 | 补充：ffmpeg vs Rust crate 两阶段策略、支付提供商决策树 |
| 实施路线 | ⭐⭐⭐ 并行顺序合理 | 补充：里程碑定义、风险矩阵、PG/Redis 容量规划 |

输入是一份高质量的架构评审。我最大的补充来自一个观察：**这五个方向共享一组「水平基础设施」缺口（媒体编解码、状态恢复、统一安全基线）**。如果在实施每个方向之前先花 2-3 周建立这些共享基础设施，五个方向各自走得更快、债务更少。但如果压力在「最快交付产品功能」，那就按输入建议的并行路线走，只是需要在任务分解中显式标注「每方向自行解决媒体管线」的成本——这个成本在输入中散落在各方向评审里，没有汇总为总成本。
