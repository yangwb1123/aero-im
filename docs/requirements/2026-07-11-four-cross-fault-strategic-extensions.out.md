仔细核对了代码中的引用锚点。文档分析框架和方向选择极其扎实，但有几处具体代码引用与实际源码有偏差，我在下方列出——你可以选择修正后再存盘，或直接按当前版本保存。

### 交叉验证发现的偏差

| 文档中的声明 | 实际代码状态 | 偏差程度 |
|---|---|---|
| `metrics.rs` names 常量中 **0 个媒体层面** | 存在 **`LIVE_WHIP_SESSIONS`**（line 118），但确实仅此一个 | 轻微：核心论点「媒体指标极度匮乏」仍成立 |
| SRT/WHIP crate 的 **lib.rs 无 metrics 模块引用** | **`aero-live-srt`** 有 `mod metrics;`（lib.rs:52）、**`aero-live-whip`** 有 `pub(crate) mod metrics;`（lib.rs:51） | 中等：SRT 已有 `ACTIVE_SESSIONS / PACKETS_RECEIVED_TOTAL / BYTES_RECEIVED_TOTAL / PACKETS_LOST_TOTAL`；WHIP 有 `RTP_PACKETS_RECEIVED_TOTAL / DEPACKETIZE_FAILURES_TOTAL` |
| **SFU / HLS** 零指标 | ✅ **完全正确**——`aero-live-webrtc` 和 `aero-live-hls` 中无任何 metrics 调用 | 无偏差 |
| 用户说 **`appendBlock` switch 缺失多个类型** | 实际覆盖 `text / code / mention / file / voice / card / tool_call / thought / button / select` **全部 10 个变体**，且有 `default` fallback 渲染 `[${b.type}]` | 中等：不是「静默空白」，但 fallback 确实简陋（仅显示类型名） |
| **`searchableText` 函数在 render.js ~350 行** | 不存在。`Block::searchable_text()` 是 **Rust side** 方法（`block.rs:142`） | 中等：函数名/位置/语言均错。但核心论点「switch 覆盖不完全」仍成立——`searchable_text` 确实覆盖了 Text/Code/Voice-transcript/Thought/Button，但未覆盖 Card/ToolCall/Select |
| **`Poll` 列为 Block 变体** | Poll **不是** `Block` 的 variant——Poll 是独立 REST API + `PollEvent` RoomEvent | 中等：文档中的 Block 列表应去掉 Poll |
| WS 帧 **`Hello` / `Welcome`** 中无 `block_caps` | ✅ **完全正确**——`mod.rs:207` 的 `Welcome` 仅有 `{ participant }` 字段 | 无偏差 |
| 后端 `Block` 无 **`deny_unknown_fields`** | ✅ **完全正确**——`#[serde(tag = "type", rename_all = "snake_case")]` 无 `deny_unknown_fields` | 无偏差 |

### 核心论点的真实度评估

尽管有几处具体代码引用需要修正，**四个交叉断层的选择和三层的分析逻辑全部经得起代码验证**：

1. **Bot 生态生产化** ✅ 现状评估准确，配额/审计/令牌吊销完全缺失
2. **媒体管道可观测** ✅ 核心论点成立（SFU/HLS 零指标），只是 SRT/WHIP 有零星指标而非完全空白
3. **数据生命周期治理** ✅ 现状评估准确，`data_class` 和统一框架完全不存在
4. **Block 版本化与兼容** ✅ 核心论点成立（无版本协商），Web 端 `default` fallback 存在但过于简陋

要我现在保存这份文档（附带脚注标注偏差），还是你先修正上述引用问题后再保存？
