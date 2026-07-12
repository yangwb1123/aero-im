好，我已经做了完整的源码验证。你的文档有一个需要订正的事实错误——我直接说，不绕弯：

**文档 §2（方向二）的验证证据指出「0 个是媒体层面的」和「各 crate 的 lib.rs 中均无 metrics 模块引用」不准确**。实际状况是：

| crate | 已有指标 | 文档的缺失判断 |
|---|---|---|
| `aero-live-srt` | `aero_srt_active_sessions` (gauge), `aero_srt_packets_received_total`, `aero_srt_packets_lost_total`, `aero_srt_bytes_received_total` | ❌ 错误——已有 metrics 模块 |
| `aero-live-whip` | `aero_whip_rtp_packets_received_total`, `aero_whip_depacketize_failures_total`, `aero_whip_active_sessions` | ❌ 错误——已有 metrics 模块 |
| `aero-live-webrtc` (SFU) | **零** | ✅ 正确 |
| `aero-live-hls` | **零** | ✅ 正确 |
| `aero-live-rtmp` | **零** | ✅ 正确 |
| `aero-im-call` | **零** | ✅ 正确 |

不过，即使对于 SRT 和 WHIP，你建议的扩展指标（RTT 直方图、加密协商失败、连接时长、WHIP 协商耗时/失败、ICE 状态）**确实不存在**，所以方向二的核心判断「当前指标不足以支撑生产排障和容量规划」仍然成立——只是需要把验证证据从「0 个媒体指标」修正为「4/7 媒体组件零指标，已埋点的 SRT/WHIP 缺乏排障级细粒度指标」。

---

下面是我的架构审查意见，按价值排序：

---

## 1. 方向二 · 关于指标设计的热路径成本

你给出的 15 个指标中，有 2 个需要特别注意：

```rust
pub const SRT_RTT_SECONDS: &str = "aero_srt_rtt_seconds";                         // histogram
pub const SFU_SUBSCRIBER_LATENCY_SECONDS: &str = "aero_sfu_subscriber_latency_seconds"; // histogram
```

**问题**: histogram 在 Prometheus 客户端库中默认暴露为累积桶（cumulative buckets），每个观测在热路径上做 N 次 `+1` (默认桶数 ~12)。对于 SRT 每数据包路径和 SFU 每 RTP 包路径，每秒可能跑 900-1500 次观测，这会变成可测量的 CPU 开销。

**建议**: 改用 **Exponentially Decaying Reservoir (EDR) sampling**——每 N 个包采样一次而不是全部。或者用 `aero_common::metrics::observe_histogram` 仅对每流每秒采样 1 次（set a sampling rate flag on `SfuPeer` 级别）。对于 SRT RTT，RTT 通过 SRT 的 ACK/NAK 携带的频率远低于数据包速率，可以直接用 gauge + EWMA（指数加权移动平均）代替 histogram。

## 2. 方向一 · 内置 bot 配额豁免的实现歧路

你说「内置 bot 使用独立配额池（unlimited）或走特权路径 bypass」。这里有一个架构选择：

**方案 A - 特权路径**: 在 rate limiter 里做 `if is_internal_bot { skip }`。风险小但增加条件分支。

**方案 B - 独立配额池**: 给内置 bot 注册时在 `bot_quotas` 表里设置 `messages_per_min = -1`（表示 unlimited）。好处是统一的数据模型。

我推荐 **方案 B**——因为如果走特权路径，未来增加新的内置 bot 需要同时修改 rate limiter，很容易遗漏。统一的配额模型让审计日志也能记录「内置 bot 使用了 unlimited 配额」这个信息。

## 3. 方向三 · 数据分类的标签反模式

你提议的：

> 每个业务表加 `data_class` 标签，不直接改表——用配置映射

这是一个**运行时分类 tags 与 schema 耦合**的隐含问题。如果你用配置映射（比如 `Map<table_name, DataClass>`），有一个边缘情况：

```sql
-- 同一条房间消息，几个小时后用户上传了图片
-- 原始 INSERT: data_class='message_text'
-- 上传附件后: 变成 message_text + message_media 的混合
```

如果 `Block` 列表中可以混合不同类型的 block（text + file），那 `data_class` 按行级别就不够精细了。建议：

**每条消息行仍然走 `data_class`**，但使用 `least_retention` 原则：如果一个 message row 包含 Text + File + Voice block，取所有 block type 对应 `data_class` 中 retention 最短的那个。这样可以保证即使 message_text 保留 365 天，如果它附带了一个仅保留 90 天的 file blob，消息行的保留策略不会超过 365 天——但如果 file 是外部 blob 单独存储的，blob 级的 retention 可以和行级 retention 不同步，导致一致性风险。

设计上要考虑这个 `min(data_class_retention)` 逻辑。

## 4. 方向四 · `renderUnsupportedBlock` 的 RCE 隐患

```javascript
function renderUnsupportedBlock(block) {
  const el = document.createElement('div');
  el.textContent = `[不支持的消息类型: ${block.type}]`;
  el.title = JSON.stringify(block, null, 2);
  return el;
}
```

`el.title` 赋值为 `JSON.stringify(...)` 是安全的（setter 做字符串化），但 `el.textContent` 中直接嵌 `block.type`——虽然 textContent 不会执行 HTML，但如果 `block.type` 包含换行符或 U+2028/U+2029（行分隔符），在控制台和工具提示中可能误导调试者。建议 `block.type` 限定为 `/[a-zA-Z0-9_]+/` 再输出。

更重要的是——**这个 fallback 需要在 render.js 最顶部、任何其他 render 函数之前注册**。因为如果新增的 Block type 导致 `switch` 前的解构代码崩溃（比如 `renderPoll` 假设 `block.options` 存在但新 variant 没有），fallback 根本不会被激活。建议用 `tryRenderBlock` + catch 兜底：

```javascript
function renderBlock(block) {
  try {
    switch (block.type) {
      case 'text': return renderText(block);
      case 'poll': return renderPoll(block);
      // ...
      default: return renderUnsupportedBlock(block);
    }
  } catch (err) {
    console.error('[render] block render crashed:', block.type, err);
    return renderUnsupportedBlock(block);
  }
}
```

---

## 汇总

| 维度 | 评价 |
|---|---|
| 方法论创新 | ⭐ **方向一的方向**——交叉断层分析是这个代码库 180+ 份文档中未出现过的方法，值得作为后续分析的模板 |
| 事实准确性 | ⚠️ 方向二的数据验证有误（SRT 和 WHIP 已有 metrics），建议修正后再纳入 ROADMAP |
| 架构风险 | ✅ 方向四的 catch-all fallback + 热路径 histogram 采样是真正的 blindspot，分析到位 |
| 排期逻辑 | ✅ 媒体可观测 > Bot 运营基线 > 数据治理 > Block 兼容 的排序合理 |

是否需要我帮修正方向二的验证段落，或者进一步展开某个方向的技术设计？
