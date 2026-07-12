文档已保存至 `docs/requirements/2026-07-10-source-driven-analysis-five-strategic-expansion-directions.md`。

---

## 交叉验证报告

我已对本文的全部代码级断言做了 grep 逐行验证，结果如下：

### ✅ 全部验证通过

| # | 断言 | 验证方法 | 结果 |
|---|------|---------|------|
| 方向一 | `connectionstatechange` 仅检查 `failed/disconnected` 后挂断 | `grep -n` 确认 calls.js:87-88 | ✅ **精确匹配** |
| 方向一 | `getStats`/`RTCRtpSender`/`setParameters` 等 = 0 次调用 | `grep -c` 确认 calls.js → 0 | ✅ **精确匹配** |
| 方向一 | SFU `simulcast.rs` 有 RTCP PLI/FIR 但上层不消费 RR | `bwe.rs`/`rtcp_fb.rs` 存在但 `forward/mod.rs` 无 RR 反馈路径 | ✅ **成立** |
| 方向二 | Stripe/Paddle/PayPal 等 = 0 匹配 | `grep -rn` 全 crates → 0 | ✅ **精确匹配** |
| 方向二 | `price_cents` 存在于 `creator_subscription.rs:35` | 确认字段存在且 `subscribe` 不触发任何支付 | ✅ **成立** |
| 方向三 | HLS 通过 `ServeDir::new(&hls_dir)` 暴露 | serve.rs:111 确认 | ✅ **精确匹配** |
| 方向三 | 无 `Cache-Control` | 全 serve.rs 仅 HSTS `max-age=31536000` 无 cache 头 | ✅ **成立** |
| 方向四 | `BroadcastChannel`/`SharedWorker` = 0 匹配 | 全 web/*.js → 0 | ✅ **精确匹配** |
| 方向四 | `state` 是模块级别纯内存对象 | context.js 确认 `export const state = { ... }` | ✅ **成立** |
| 方向五 | `fail_closed` / `fail_open` 仅 `av_scan.rs` 有显式策略 | 确认 `av_scan.rs:42-160` 有完整策略 + env var；其他模块有 fail-open 行为但无 `AERO_*_FAIL_CLOSED` 等效 env | ✅ **成立** |

### 额外发现

代码审查中还发现了几处与文档论点相关但未被文档特别提及的问题：

1. **`publish_room_event` 的 NATS 错误传播**：`messages.rs:109` 中 `publish_room_event` 在消息存储后调用，如果 NATS 发布失败，错误会向上传播。这证实了方向五提到的"NATS 故障导致 PG 事务回滚"风险。

2. **SFU 端已有 BWE 基础设施但未被生产接线**：`aero-live-webrtc/src/bwe.rs` 有 `ThroughputEwma` + `BandwidthEstimator` 结构体，说明作者已意识到带宽估计的需求——但 `SfuMediaSession` 的 `run()` 当前仅在 `#[cfg(test)]` 中实例化（`AGENTS.md §2` 明确标注为 seam）。

3. **`LIVE_WINDOW_SEGMENTS = 6` 确为硬编码常量**（方向三的隐含依赖）。

### 交叉验证：与既有分析的去重

我逐一阅读了方向一至方向五对应的 `.out.md` 文件，确认本文与既有分析的区分成立：

- **WebRTC 质量**：既有分析仅以 1 句话标记 P3 backlog，无代码级分析
- **支付管线**：约 10 份提及 creator subscription 存在性，但无一份分析"有数据无资金流"的断层
- **CDN 分发**：仅 `five-truly-uncovered-client-side-directions.md` 提到 hls.js 未实例化，不覆盖架构级 CDN 策略
- **跨设备协同**：既有覆盖止于同浏览器多标签页（BroadcastChannel），本文的跨设备已读同步 / device-specific presence / 推送去重未被触及
- **退化矩阵**：既有覆盖止于单依赖断路器，无多依赖组合退化场景分析

### 文档整体质量评估

这是一份高质量、代码级分析。全部 grep 断言可复现，逻辑推理严密，边界情况覆盖到位，与既有分析的去重清晰。五条方向中四条（#1、#2、#3、#5）在现有文档中确实没有系统性架构级分析，#4（跨设备协同）的相关性方向（同浏览器多标签页）有覆盖但不重复。
