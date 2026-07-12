Good — I've validated the codebase against the document's claims. Here's my architectural review:

---

## 代码验证结果

### ✅ 已验证为真的核心缺口

| 方向 | 验证结论 |
|------|---------|
| **备份/灾备** | ✅ 无 `pg_dump`/WAL 归档/PITR/备份脚本。三组件的数据目录（`./data/pg`、`./data/redis`、`./data/nats`）是 host-mounted bind volume，但无任何备份复制到外部存储 |
| **媒体资产管线** | ✅ 零图像处理依赖（`rg "image\|libvips\|sharp\|webp\|avif" crates/*/Cargo.toml` → 无结果）。`BlobStore` trait 的 `S3BlobStore` 实现存在但无 CDN URL 生成 |
| **CDN 集成** | ✅ `public_base_url` 是自引用 server URL，无 CDN 域名注入点。无 CDN 缓存头策略 |
| **支付管线** | ✅ 无 Stripe/PayPal 集成代码。`subscription_tier.rs` 的 `price_cents` 仅存储从不用来扣款 |
| **移动端 (PWA)** | ✅ 无 `service-worker.js`、`manifest.json`、`@media` 查询、CSS 固定宽度布局 |
| **负载测试** | ✅ Makefile/CI/脚本中无 k6/oha/wrk/任何基准工具 |

### ❌ 需修正的事实性错误（共 7 处）

1. **「无 `<meta name="viewport">`」** → 错误。`web/index.html` 第 5 行明确有 `<meta name="viewport" content="width=device-width, initial-scale=1" />`。

2. **「Redis RDB/AOF 均未配置，docker-compose 无持久化卷声明」** → 双重错误。
   - AOF 已配置：`command: ["redis-server", "--appendonly", "yes", "--dir", "/data"]`
   - 持久化卷已声明：`volumes: - ./data/redis:/data`

3. **「Postgres `pgdata` 卷是匿名卷」** → 不准确。`volumes: - ./data/pg:/var/lib/postgresql/data` 是 **host-mounted bind volume**，`docker-compose down` 不丢失数据（只有 `docker-compose down -v` 会）。文档对此的警告方向正确但技术描述有误。

4. **「`blob_base_url`（state.rs 行~47）格式为 `http://localhost:3030/api/blobs/`」** → 不存在。`state.rs` 只有 `public_base_url: String`，无 `blob_base_url` 字段。`AppState` 中没有 CDN 注入点这个结论是对的，但引用的证据不存在。

5. **「`content_sniff.rs` 做 MIME 猜测...无 SVG 安全清洗」** → 过时。实际 `content_sniff.rs` 已实现 magic-byte 检测 + SVG 拒收（`Sniffed::Markup` 包含 SVG 并 explicitly comment "SVG is a stored-XSS vector"）。文档描述的 SVG XSS 风险在已有代码中被完全解决。

6. **「`push_bot.rs` 的 `emit_push_notification` 方法（行~98-160）」** → 方法名错误。实际是 `push_to_participant` + `handle` + `run`，功能逻辑描述方向正确但行号和方法名不匹配。

7. **「这段逻辑完全浪费，因为没有客户端注册 token」** → 夸大。服务端 pipeline 完整（`push_bot.rs` 监听总线 → 查 `PushTokenRepo` → FCM/APNs 网关 → 死 token 回收）。缺少的是**移动客户端注册 token**的能力，但服务端 90%+ 已完成。可用于 Web Push API 的 Service Worker 推送同样可行。

---

## 架构强度评价

文档对 5 个方向的战略价值判断是合理的。以下是更精确的风险评级：

### 方向一（备份/灾备）

RPO/RTO 风险确实真实——但需澄清：`docker-compose down -v` 才丢数据，常规 `docker-compose down` 保留。更直接的威胁是：
- **容器重建时若 `./data/` 被 cleanup（`docker system prune --volumes`）**
- **生产环境无备份意味着数据丢失是时间问题**（磁盘故障、操作失误、安全事件）
- **JWT 私钥丢失的恢复性灾难**确实是所有方向中最严重的——影响签名验证能力，且无自动化流程

建议优先级提到 **P0** 是合理的。

### 方向二（媒体管线）

CI/CD 验证确认了 5 个缺口：
- 无 `image-rs`/`libvips` 依赖 → 上传时图像只能原样存储
- 无缩略图生成 → 房间列表、搜索结果的 blob URL 直接返回原图（可能 10MB+）
- EXIF 监听：`content_sniff.rs` 做安全检查但**不清除 EXIF**（GPS/设备数据泄露）
- 无 CDN URL 生成路径
- HLS 片段由 Axum `ServeDir` 分发，无边缘缓存

建议保留 P1。但描述中「无 image processing」比「无任何像素操作」更精确——安全相关的 magic-byte sniffing 已实现。

### 方向三（支付管线）

`subscription_tier.rs` 的 `price_cents` seam 是真实的。但缺口不只是在 Stripe 集成层面——更深层的是：
- **`price_cents` 被译为 `i32`**（微单位 × 分），但无任何扣款/授权/订阅周期逻辑引用它
- 虚拟经济（gifts/points/predictions）**无真实货币锚定**——整个经济层是封闭的
- `subscription_tiers` 和 `channel_points` 两套独立经济系统**互不连通**

这是正确的 P1。值得补充一条分析：先做「购买 coins」的最小闭环（Stripe → 点数量）再扩展，而不是一次性做完整 Stripe Connect 分账。

### 方向四（移动端）

`<meta name="viewport">` 存在说明有基本的移动友好意图，但：
- CSS 零 `@media` 查询 → 布局在 <768px 宽度完全不可用
- 无 PWA shell → 无离线能力、无添加到主屏幕
- `ws.js` 无 Page Visibility API → 后台连接保持（电池/流量浪费）
- 推送管线服务端完整但无客户端 token 注册 → 无法接收

评价：**客户端的缺口比文档描述的更深**——不仅是移动 App 缺失，而是连 "mobile web that works on a phone" 也不成立。建议方向正确，可分阶段（PWA Shell → Push API → React Native）。

### 方向五（负载测试）

纯缺口——完全无基准。但需注意约束：
- **str0m SFU 基准需要真实 SDP 握手**（两个真正的 str0m 端点），不是纯工具可以模拟的
- 直播 ingest 测试需要 ffmpeg 作为推流端
- CI runner（2 核 7GB RAM）跑不了高并发 → 大场景需专用测试环境

建议先从 REST API 和 DB 查询基准做起（用 `oha` 即可），再扩展到 WS 并发。

---

## 边缘情况覆盖评估

文档在各方向都给出了 Edge Cases 小节——以下是补充的自认为关键的遗漏：

**方向一**：
- 备份加密密钥与 JWT 私钥的分离存储未被提及（同一故障域 = 单一攻击面）
- `_sqlx_migrations` 账本：恢复时迁移版本可能已迁移但被回滚过（迁移是不可逆的），恢复会报错误的 `migration version mismatch`

**方向二**：
- WebP/AVIF 转码需要 `libvips` 或 `image` crate 的 `webp` feature——最小 Docker 镜像不包含这些，需额外安装系统库
- AVIF 编码极慢（CPU-bound），需设定合理的超时和降级逻辑

**方向三**：
- 不讨论**退款后已消耗的虚拟商品的撤销**（coin 已打赏给主播 → 退款后要不要从主播扣回？）
- 不讨论**未成年打赏限额和家长控制**（中国/欧盟法规）
- 不讨论**税务合规**（美国 1099、欧盟 VAT、中国个税代扣代缴）

**方向四**：
- 多条消息推送需要 `collapse_key` 以合并通知行——代码已实现（`push_bot.rs` 的 `collapse_key_for_room`）
- 但这是服务端逻辑——移动端也需正确处理 `notification.data` 中的 deep-link

**方向五**：
- 数据污染池的自动 cleanup 只提到 DROP DATABASE——但迁移 157 个文件 replay 耗时可能 >30 秒，每周 load test 需要长期保持测试数据库

---

## 执行顺序建议

整体执行路线合理，但以下精炼：

| 阶段 | 方向 | 原因 |
|------|------|------|
| **Phase 0（2周）** | 备份灾备 + 密钥管理 | 风险最低、ROI 最高、不影响用户面 |
| **Phase 1（4周）** | 负载测试框架 | 提供其他方向的性能基线，指导架构决策 |
| **Phase 2（6周）** | 媒体管线（PWA 前置） | 先做 PWA 缓存的缩略图；图像管线作为独立 crate 逐步替换 |
| **Phase 3（8周）** | 移动端 PWA | 在 React Native 之前先完成可用的 mobile web |
| **Phase 4（12周+）** | 支付管线 + 创作者提现 | 需要 Phase 1 的负载数据评估 Stripe webhook 对主路径的影响 |

文档整体是高质量的架构分析。修正上述 7 处事实错误后，可以作为正式的技术战略文档。我的修改建议见下。
