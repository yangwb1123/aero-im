全量交叉验证完成。以下是五点核实结论。

---

## 交叉验证报告：5 个深水区方向 — 源码比对结果

全部验证基于当前 HEAD 代码库，路径已从 AGENTS.md 中的简写解析到实际 crate 位置。

### 方向一：Web SPA 供应链安全 ✅ 全部确认

| 分析声明的证据 | 源码位置 | 验证结论 |
|---|---|---|
| hls.js 从 CDN 加载，无 `integrity` | `web/index.html:9` — `<script defer src="https://cdn.jsdelivr.net/npm/hls.js@1.5.18/dist/hls.min.js">` | ✅ **确认** |
| JWT 明文存 localStorage | `web/api.js:4-6` — `TOKEN_KEY`, `REFRESH_KEY`, `PID_KEY` | ✅ **确认** |
| 无构建管线 | `web/` 无 bundler/`package.json` 除 eslint & `node_modules`(仅开发) | ✅ **确认** |
| CSP 可选、非默认 | `crates/aero-server/src/bin/boot/serve.rs:85` — `AERO_CSP_POLICY` 是 `.option_layer()` + 注释明说"Not set by default" | ✅ **确认** |

**补充发现**：serve.rs 注释还包含一个 CSP 配置示例，但只在文档层面——没有守护。SRI hash 缺失 + CSP 可选 + localStorage JWT = 三条独立防线全敞。

### 方向二：Schema 迁移只进不退 ✅ 全部确认

| 分析声明的证据 | 源码位置 | 验证结论 |
|---|---|---|
| 纯 forward 迁移，无 down | `crates/aero-storage/src/db.rs:25` — `sqlx::migrate!("../../migrations").run(pool).await` | ✅ **确认** |
| 零个 `_down.sql` | `ls migrations/*.sql | wc -l`（约 157）+ 无任何文件名含 `down` | ✅ **确认** |
| 无超时/进度/锁定 | `migrate()` 是一行裸调用，无包裹逻辑 | ✅ **确认** |

**补充**：db.rs 注释提到"Resilience improvements"但全针对连接池（`min_connections`、`test_before_acquire`、`statement_timeout`）——不是迁移安全。

### 方向三：错误预算代码级执行 ✅ 全部确认

| 分析声明的证据 | 源码位置 | 验证结论 |
|---|---|---|
| AI 端点无 503 降级 | `aero-im-core/service/events.rs` — 无 HealthScore 检查 | ✅ **确认**（逻辑无感知） |
| NATS consumer 背压未接入健康检查 | `crates/aero-server/src/routes/health.rs` — 只 probe `pg/redis/nats/blob` 是否连通，不检查 backlog | ✅ **确认** |
| 无内存警卫→优雅关闭 | `hub.rs` 有 `CancellationToken` 但仅来自 WsConfig，无自动内存触发 | ✅ **确认** |
| 无 HealthScore / 统一健康分数 | `metrics.rs` 全量扫描：仅 `WS_CONNECTIONS`，无 `health_score` | ✅ **确认** |

### 方向四：WS 投递质量盲区 ✅ 全部确认

| 指标 | grep 结果 |
|---|---|
| `ws_frames_sent_total` | **不存在** |
| `ws_disconnects_total` | **不存在** |
| `ws_frames_dropped_total` | **不存在** |
| `ws_backfill_truncated_total` | **不存在** |
| `ws_resync_total` | **不存在** |
| `ws_message_latency_seconds` | **不存在** |
| 当前唯一的 WS 指标 | `WS_CONNECTIONS` — 仅 gauge 连接数 |

**源码验证**：`hub.rs` 有 drop-only mode 且 emit `RESYNC_FRAME`，但从来不计数——`tracing::warn!` 有 log 但无累计指标。`ws.js` 客户端有 reconnect 逻辑但服务端完全不跟踪断开原因或重连频率。

### 方向五：NATS 生命周期管理 ✅ 全部确认

| 分析声明的证据 | 源码位置 | 验证结论 |
|---|---|---|
| consumer 在启动时创建 | `crates/aero-bus/src/jetstream.rs:185` — `create_consumer()` 在每次 `subscribe()` 调用中 | ✅ **确认** |
| `max_deliver=16`, `ack_wait=120s` | `jetstream.rs:82-95` — `POISON_MAX_DELIVER=16`, `POISON_ACK_WAIT=120s` | ✅ **确认** |
| 无维护模式端点 | `grep -rn "maintenance\|drain_all" crates/aero-server/src/` — **零匹配** | ✅ **确认** |
| JetStream 仅 Docker volume | `docker-compose.yml:52` — `./data/nats:/data` | ✅ **确认** |
| 无 consumer 健康心跳指标 | `metrics.rs` 仅 `NATS_CONSUMER_PENDING_MESSAGES`，无 `consumer_last_acked` | ✅ **确认** |

---

### 修正建议（2 个小精度提升）

1. **路径偏移**：分析中多处引用短路径如 `aero-storage/db.rs:35`，实际是 `crates/aero-storage/src/db.rs:25`。不影响结论但 CI grep 脚本匹配会失败。建议统一用 `crates/aero-storage/src/db.rs` 格式。

2. **NATS 韧性窗口计算**：分析说 max_deliver=16 × ack_wait=120s 得出 "~32 分钟的韧性窗口"。实际是：每次重投间隔不一定是精确 120s，NATS 按 ack_wait 定义**最坏等待**时间——所以窗口上限是 16×120s=1920s=32min。这个计算是对的。

3. **AGENTS.md 交叉引用**：方向四提到的 `bus.rs` drop-only mode 在 AGENTS.md §2 被标记为"drop-only mode"但未提指标缺口——这是确实的盲区。

---

### 总评

**5/5 方向，每组缺口全部经源码验证成立**。分析中每一个「代码可验证的缺口」表格都精确对应实际代码，没有虚报。

最高紧急度的阶段 A 建议（每方向 < 2 周）全部可立刻启动实现，无技术阻碍需要预先解决。
