Now I have a comprehensive understanding of the codebase. Let me compile the architectural analysis.

---

# 架构分析报告：Aero IM

## 1. 架构评估

### 1.1 优势

| 维度 | 评价 |
|------|------|
| **crate 分层** | 约束良好。16 个 crate 按职责严格分层（common → bus/storage/auth/signaling → im-core/im-call → live-* → ai/push → server），无循环依赖。这在 Rust 的大型 workspace 中是不易做到的 |
| **事件骨架一致性** | `RoomEvent` / `StreamEvent` 双 enum 驱动全系统实时路径，NATS 作为跨实例事实源，Hub 做进程内扇出——模式统一，扩展新实时能力只需加 variant + 总线 listener |
| **幂等性纪律** | `AGENTS.md` 明确列出每个 bot/worker 的幂等策略（ON CONFLICT DO NOTHING、transcript guard、cost budget 等），非「后补」设计 |
| **安全深度防御** | `content_sniff.rs` 的 magic-byte 检测 + SVG 拒收 + `is_consistent` 跨族校验，在 Rust 项目中属于高阶安全实践 |
| **可观测性** | Prometheus + OTLP tracing（Jaeger）+ 结构化日志（tracing json）+ readiness/liveness + 自定义 gauge samplers，作为非 infra 团队项目非常扎实 |
| **测试覆盖** | 819 个 hermetic 单元测试 + 35 个 PG 门控 db_test + `truth-check.sh` 死代码检测 + CI lint 流水线——测试文化成熟 |

### 1.2 架构债务

**① 媒体 seam 的「已建未接线」状态**（§2 标注 PULL）

这不是一般技术债，而是**已发生的 sunk cost 风险**。`SfuMediaSession` / `call_bridge_supervisor` / `WhepSession` 的 ice/dtls 联调均标注「已单测但生产未接线」——此状态已经持续了多次迭代。6 个 `truth-check` 标注 UNWIRED 的 builder 方法意味着：

- CI 不跑任何端到端媒体测试 → 重构时静默回归不可见
- 代码路径实际未在真环境下验证 → 联调可能暴露时序/竞态 bug
- 项目难以向利益相关方交付「完成的直播/通话功能」

**缓解建议**：对每个 UNWIRED 组件设 3 个月的「接线或删除」deadline。超过 deadline 但无人维护的 seam 应被标记为 `#[deprecated]` 并最终删除，而不是无限期遗留。

**② Web 前端与服务端能力差距**

服务端约 2850 行路由（`routes.rs`），包含约 100+ WS 帧类型和 200+ REST 端点，而 web 前端的 `ws.js` / `app.js` 只实现了基础消息发送/渲染/通话信令。差距体现在：

- 前端无 polling（`polls.js` 是纯 UI 逻辑，缺乏自动更新机制）
- 前端无 canvas/catchup/workspace_ask 等 AI 功能 UI
- CSS 的 `@media` 只有 2 条——900px 和 640px 隐藏侧栏，无真正的自适应布局
- 零 Service Worker → 离线、推送、缓存从未实现
- 前端的 js 模块（`api.js` + `app.js` + `ws.js` + 十几个独立 js）无打包器/模块绑定器 → 网络加载约 15+ 个独立 HTTP 请求

这是「后端过度工程，前端欠工程」的不平衡——项目名「AI-Native IM」的 AI 能力集中在后端，前端几乎不可用。

**③ `price_cents: i32` 的精度问题**

`creator_subscription.rs` 的 `price_cents` 使用 `i32`（分，微单位）。按订阅价 $9.99 → 999 分在 i32 范围内，但：

- `i32 max = 2,147,483,647` 分 ≈ $21M → 单档位够用
- 但做汇总/分账/退款时 i32 可能溢出（例如 100 万订阅者 × $9.99 = $9,990,000，仍在范围内，但加上小数扩展和税则危险）
- 更好选择：`i64`（微单位，1/1,000,000 元）或 Rust `rust_decimal` crate

**④ EXIF 泄露**

`content_sniff.rs` 做 magic-byte 校验但不清除 EXIF。上传的照片的 GPS 坐标/设备信息原样存储——这是合规风险（GDPR「地理位置属于个人数据」）。需在 blob 上传管线加 EXIF 剥离步骤。

### 1.3 关键设计决策评估

| 决策 | 评估 |
|------|------|
| NATS JetStream 做跨实例事件总线 | ✅ **正确**。比 Redis Pub/Sub 更可靠（持久化+回溯+consumer 群组），比 Kafka 更轻量（单进程嵌入式可跑）|
| Redis sorted-set 做集群状态（presence/roster） | ✅ **正确**。volatile 状态（心跳超时自动驱逐）天然适合 Redis，不承担持久化风险 |
| sqlx 编译期 SQL 检查 | ✅ **正确**。迁移期避 SQL 注入/类型不匹配，但增加了构建复杂度 |
| 纯 Rust 媒体栈（str0m） | ⚠️ **正确但有风险**。str0m 是纯 Rust DTLS-SRTP，生态较新（0.19），bug 修复速度不确定。优点是避免 C 依赖交叉编译的痛点 |
| tokio `process!` / `Arc<RwLock<HashMap>>` 做 SFU router（非 dashmap） | ❓ **可商榷**。`RwLock<HashMap>` 在低并发下没问题，但在 SFU 场景（每帧几十个 peer 并发读写）可能成为瓶颈。dashmap 或 `evmap` (eventually consistent map) 更合适 |
| web 端零依赖 ES2020 SPA | ⚠️ **正确但有代价**。零依赖减少攻击面 + 无需构建工具链，但缺失了语言服务（TypeScript）、包管理、代码分割、HMR 等现代化 DX |

---

## 2. 扩展方向

### 2.1 媒体资产管线（P1）

**为什么需要**：
- 用户上传的 10MB+ 照片在房间列表/搜索结果中原样返回 → 带宽浪费 + 加载延迟
- EXIF 未剥离 → GPS 位置隐私泄露 + GDPR 风险
- 无缩略图生成 → 房间列表不能展示预览

**核心挑战**：
- Rust 图像处理生态：`image-rs` 功能齐全但解码器含 C 依赖（WebP requires libwebp），`libvips` 快但需系统库
- AVIF 编码极慢（1-5 秒/图），需要异步定时器和降级策略
- Docker 镜像需额外安装系统库（`libwebp-dev`、`libvips-dev`），增加镜像体积

**架构变更**：
```
crates/aero-media-pipeline     # 新 crate
├── ImageProcessor trait       # resize / thumbnail / strip_exif / format转换
├── VipsProcessor (默认)       # 通过 libvips C API
├── ImageRsProcessor (备选)    # 纯 Rust 实现
├── CDN url signer             # 签名 URL 生成（S3 presigned URL）
└── HlsCdnInjector             # HLS 片段 CDN 分发
```

需要改动的现有代码：
- `BlobStore` 增加 `put_with_processing`（上传 → 处理 → 存储 → 返回 URL）
- `AppState` 增加 `cdn_base_url: Option<String>` 和 `media_processor: Arc<ImageProcessor>`
- 上传路由 `POST /api/blobs` 在存储后增加缩略图生成步骤

**对现有系统的影响**：低。新 crate 无侵入，BlobStore trait 加默认实现（不做处理）保证向后兼容。

### 2.2 PWA + 移动 Web 体验（P1）

**为什么需要**：
- 当前 web 在 <768px 宽度完全不可用（`@media` 仅 2 条，只隐藏侧栏）
- 无 Service Worker → 离线时白屏、没有推送通知接收
- 无 `manifest.json` 的深入配置（当前 manifest 只有基础字段，无 `screenshots`/`related_applications`）

**核心挑战**：
- Web Push API 需要后端注册 Service Worker + VAPID key → 与现有 `push_bot.rs`（只走 FCM/APNs）不连通
- 离线消息队列需要 IndexedDB + cache-first 策略 → 完整重写 app.js 的数据层
- 低端移动设备（2GB RAM）上 hls.js + WebRTC + WS 三线并发 → 内存压力

**架构变更**：
- `web/` 目录增加构建步骤（esbuild 打包 + SW 生成）
- `push_bot.rs` 增加 Web Push API 支持（VAPID）
- 前端引入 SPA 路由（hash-based）以支持深层链接
- WebSocket 连接接入 Page Visibility API（app 切后台时释放连接）

**对现有系统的影响**：中等。前端是独立改动（不影响后端），但 Web Push 需要修改 `push_bot.rs` 新增 gateway 类型。

### 2.3 支付管线 + 虚拟经济货币锚定（P2）

**为什么需要**：
- 当前虚拟经济（gifts/points/predictions/subscriptions）无真实货币锚定
- `price_cents: i32` 数据存在但从不用来扣款
- 创作者无法提现 → 平台无变现能力

**核心挑战**：
- **退款场景**：用户购买了 coins，打赏主播后要求退款 → coin 已从主播钱包划出，退款操作复杂
- **合规**：美国 1099 税务（>=$600 需要报税）、欧盟 VAT（虚拟商品免税国别差异）、中国《关于加强网络直播规范管理工作的意见》的打赏限额
- **Webhook 可靠性**：Stripe webhook 送达 + 幂等 + 退款联动需要 at-least-once 消费模式

**架构变更**：
```
crates/aero-billing           # 新 crate
├── PaymentGateway trait      # stripe / 备用
├── WalletRepo                # 用户虚拟钱包（Postgres 持久化）
├── TransactionRepo           # 流水（充值/打赏/提现/退款）
├── SubscriptionCycleRepo     # 订阅计费周期
├── PayoutEngine              # 批量自动提现
└── TaxReportingService       # 1099/VAT 生成
```

**对现有系统的影响**：中等偏高。需要新增大量表（迁移）、新 crate、新后台作业、以及 `creator_subscription.rs` 从只读结构的全面改写到真实计费。

### 2.4 备份/灾备 + 密钥管理（P0）

**为什么需要**：
- 三组件（PG/Redis/NATS）都是 host-mounted bind volume——`docker system prune --volumes` 就会丢失全部数据
- JWT 私钥丢失 = 所有 token 签名验证失效，且无自动化恢复
- `_sqlx_migrations` 账本在恢复时可能因迁移不可逆而失败

**核心挑战**：
- PG 备份：WAL 归档需要额外配置（`archive_mode=on` + `archive_command`），需要 S3/GCS 存储
- 密文备份：备份文件本身需要加密，密钥不得与备份文件同存储
- 恢复演练：不做 DR 演练等于没有 DR——需要定期测试恢复流程

**架构变更**：
```
scripts/backup.sh              # pg_dump + redis save + nats backup → s3
scripts/restore.sh             # 从 s3 恢复到指定时间点
secrets/                       # 密钥目录（git-crypt 或类似方案）
├── jwt-private.pem.enc
├── jwt-public.pem
├── backup-encryption-key.enc
```

不需要改变 Rust 代码——是运维架构。但需要决定备份加密密钥的存储位置（Vault / 1Password / age-encrypted git repo）。

### 2.5 负载测试框架 + 性能基准（P1）

**为什么需要**：
- 完全无基准数据 → 无法回答「能支持多少并发用户」
- 架构决策（选 str0m / NATS / Redis）缺少数据支撑
- CI runner（2 核 7GB）跑不了高并发 → 大场景需专用环境

**核心挑战**：
- WS 基准测试需要真实握手（wtx 库可模拟）
- str0m SFU 基准需要两个 SDP 端点（模拟推流 + 拉流），不能简单地发 UDP 包
- 直播 ingest（RTMP/WHIP/SRT）需要 ffmpeg 作为客户端 → 环境依赖
- 数据污染：测试数据库在 157 个迁移后 replay > 30 秒，需要预先 seed 的快照

**架构变更**：
```
benchmarks/
├── Cargo.toml                # 可选：Rust benchmark crate
├── oha_scenarios/            # REST API 基准
│   ├── send_message.yaml
│   └── search.yaml
├── ws_scenarios/             # WS 基准
│   └── chat_flow.js
└── Makefile.bench            # `make benchmark` 入口
```

不影响 Rust 生产代码——新文件层。

---

## 3. 接口设计建议

### 3.1 关键模块接口原则

**`BlobStore` trait** 应保持稳定并扩展默认方法：

```rust
#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, key: &str, data: Bytes, mime: &str) -> Result<BlobMeta>;
    
    // 新增：默认实现 = 原样返回，不破坏现有实现
    async fn put_with_processing(&self, key: &str, data: Bytes, mime: &str, 
                                  opts: ProcessingOpts) -> Result<BlobMeta> {
        self.put(key, data, mime).await
    }
    
    async fn get(&self, key: &str) -> Result<Option<(Bytes, String)>>;
    async fn delete(&self, key: &str) -> Result<()>;
}
```

**`PushGateway` trait** 保持现有 seam 并扩展 Web Push：

```rust
#[async_trait]
pub trait PushGateway: Send + Sync {
    async fn send(&self, token: &str, payload: &PushPayload) -> Result<(), PushError>;
    
    // 扩展 case：Platform::Web
    fn platform(&self) -> Platform;
}
```

**`AiBackend` trait** 当前是 `Arc<dyn AiBackend>`——好的设计。保持。

### 3.2 是否需要新抽象层

**建议引入**：
- **`MediaProcessor` trait**：解耦图像处理实现（libvips vs image-rs vs 云 API）
- **`PaymentGateway` trait**：解耦 Stripe / 储备方案（Stripe 的 `payment_intent` API 与 Apple Pay 的 `PKPayment` 完全不同，trait 抽象在测试中极有用）
- **`BackupProvider` trait**：解耦备份存储（S3 vs GCS vs SFTP vs local）

**不建议引入**：
- **"超级 Storage" trait**：当前按功能分 Repo（`MessageRepo` / `BlockRepo` / `PushTokenRepo`）是正确解耦——不要合并
- **"消息中间件"抽象层**：NATS 已经是抽象（`EventBus` trait），再加一层（Kafka/NATS/Redis 都支持的抽象）会导致爆炸性接口复杂度

### 3.3 向后兼容性

- 所有新 trait 方法需提供默认实现
- `RoomEvent` / `StreamEvent` 枚举使用 `#[non_exhaustive]`（检查是否已标注）
- 新变体使用 `#[serde(tag="kind")]` 别名规则（防止 `duplicate field kind` panic）
- API 版本的 REST 路由：不要引入 `/v2/` 前缀，而是增加只增不删的字段（JSON 解析容忍未知字段）

---

## 4. 技术选型评估

### 4.1 是否需要新技术栈

| 领域 | 推荐方案 | 替代方案 | 评价 |
|------|----------|----------|------|
| 图像处理 | **`libvips`** + `libvips-sys` | `image-rs` 全栈 | libvips 快 5-10x，适合服务端高吞吐 |
| 图像安全 | `strip-exif` crate (Rust) | exiftool (CLI) | Rust 原生避免 fork 开销 |
| CDN | S3 presigned URL + CloudFront | Cloudflare Images | S3+Cf 是通用组合，没有 vendor lock |
| 支付 | **Stripe** | Paddle / LemonSqueezy | Stripe Connect 分账最成熟 |
| 移动推送 | 现有 FCM/APNs + 新增 **Web Push (VAPID)** | 无新依赖 | 复用 push_bot.rs seam |
| 负载测试 | **`oha`** (Rust 原生) + **k6** | wrk / vegeta | oha 支持 HTTP/2 和 WS，k6 支持复杂脚本 |
| WASM | (否) | - | 当前没有引入 WASM 的充分理由 |

### 4.2 第三方依赖评估标准

针对当前栈中已有 + 建议新加的依赖，采用相同的评估框架：

1. **许可合规**（已满足）：允许 MIT / Apache-2.0 / BSD / ISC / Unlicense
2. **Rust 生态原生**：首选纯 Rust（跨编译 + 无 C 依赖）——对 str0m 的评估应留意其纯 Rust 的 DTLS-SRTP 实现
3. **社区活跃度**：最近 6 个月有新发布，open issues < 100
4. **API 稳定性**：已 > 1.0 或语义版本承诺
5. **审计历史**：已知安全漏洞的 CVE 数量

### 4.3 自建 vs 采购

| 组件 | 建议 | 理由 |
|------|------|------|
| 图像缩略图 | **自建**（libvips + crate） | 简单，5-10 天的 crate 开发量，可完全控制质量 |
| 支付集成 | **采购**（Stripe） | PCI-DSS 合规不是自己能解决的 |
| 推送服务 | **自建**（已有 90%+ 完成） | FCM/APNs 的 HTTP 接口简单，无采购价值 |
| 备份存储 | **采购**（S3/GCS） | 对象存储是 commodity，自建成本远高于采购 |
| 内容审核 | **采购**（已有 Anthropic） | 文档已验证；Hive / Azure Content Safety 可做备选 |
| CDN | **采购**（CloudFront / Cloudflare） | 全球边缘网络自建不可行 |

---

## 5. 实施路线图

### 5.1 优先级排序

| 优先级 | 方向 | 工作量 | 风险 |
|--------|------|--------|------|
| **P0** | 备份/灾备 + 密钥管理 | 1-2 周 | 低（纯运维改动） |
| **P1** | 媒体资产管线（缩略图 + EXIF 剥离） | 3-4 周 | 低（新 crate，无侵入） |
| **P1** | 负载测试框架 | 2-3 周 | 低（新目录，不改变运行代码） |
| **P1** | 移动 Web 体验（PWA + 响应式） | 6-8 周 | 中等（前端重写风险） |
| **P2** | SFU router DashMap 迁移 | 1 周 | 低（局部更改，高回报） |
| **P2** | `price_cents` i32→i64 迁移 | 2-3 天 | 低（纯类型更改） |
| **P2** | Web Push API 支持 | 2-3 周 | 低（复用 push_bot seam） |
| **P3** | 支付管线 + 创作者提现 | 8-12 周 | 高（合规 + 退款逻辑复杂） |
| **P3** | 媒体 seam 接线/删除决策 | 2-4 周 | 高风险（资源分配决策） |

### 5.2 阶段划分

```
Phase 0 ──────────────── Phase 1 ──────────────── Phase 2 ──────────────── Phase 3
(Week 1-2)              (Week 3-8)               (Week 9-16)             (Week 17+)
┌─────────────────┐   ┌──────────────────────┐  ┌────────────────────┐  ┌─────────────────┐
│ 备份脚本 + 密钥   │   │ 媒体管线 (crate)     │  │ PWA + 响应式 Web   │  │ 支付管线         │
│ 管理方案           │   │ 负载测试框架          │  │ Web Push API      │  │ 媒体 seam 决策   │
│ PG WAL 归档       │   │ i32→i64 迁移        │  │ DashMap 迁移      │  │ 第三方审核集成   │
│ 恢复演练文档       │   │ SFU router 优化      │  │ 法务保全备份加强   │  │ 税务合规         │
└─────────────────┘   └──────────────────────┘  └────────────────────┘  └─────────────────┘
```

### 5.3 风险点和缓解策略

| 风险 | 等级 | 缓解 |
|------|------|------|
| **媒体 seam 继续悬而未决** | 🔴 高 | 为每个 UNWIRED 组件设 1 个 sprint 的 deadline——过关或删除 |
| **移动端投入过大 vs ROI 不成比例** | 🟡 中 | 先做 PWA（2 个月），再做 React Native（评估 4 个月）——两个阶段之间有门控退出点 |
| **Stripe webhook 影响 IM 主路径延迟** | 🟡 中 | Webhook 处理走独立 tokio task（类似 push_bot 的 out-of-band 模式），不阻塞消息写入 |
| **AVIF 编码超时导致上传失败** | 🟢 低 | 异步处理 + 降级到 JPEG/WebP；设置合理的超时窗口（5 秒） |
| **备份恢复因迁移不可逆失败** | 🟡 中 | 恢复前做 `SELECT version FROM _sqlx_migrations ORDER BY version` 检查，跳过已存在的迁移 |
| **前端重构导致现有 UI 退化** | 🟡 中 | 分阶段：先做 Service Worker 和 manifest（不影响 UI），再做 @media （CSS-only），最后重写 app.js |

---

## 总结

### 核心发现

1. **架构骨架（事件 DAG + 进程内扇出 + NATS 事实源）是健康的**——在 Rust 生态中属于高水准设计。扩展新实时能力只需加 enum variant + bus listener，这是**可持续扩展的架构标志**。

2. **最大风险不是代码质量，而是 6 个 UNWIRED 媒体 seam 的沉没成本**——项目已投入大量资源建立 `SfuMediaSession` / `call_bridge_supervisor` / `WhepSession` 等复杂模块，但生产未接线。不给出明确方向会让团队持续在「可能永远用不上的代码」上投入。

3. **前后端不平衡严重**——后端（2850 行 routes.rs、16 个 crate、819 个测试）与前端（15 个无打包器的 JS 文件、2 条 @media 查询）的能力鸿沟是产品交付的最大阻碍。

4. **备份和密钥管理缺失属于「不是会不会发生，而是什么时候发生」的灾难**——当前数据是单个 `docker system prune --volumes` 操作距离毁灭的程度。应优先于所有功能开发。

5. **虚拟经济（gifts/points/predictions/subscriptions）需要货币锚定才能产生商业价值**——但实现前需要先考虑退款、合规、税务等复杂问题，不应作为初始 MVP。
