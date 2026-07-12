验证完成。以下是简明摘要：

---

## 5 方向验证结果一览

| 方向 | 核心判断 | 代码修正 | Overlap 声明 |
|------|---------|---------|-------------|
| 一：消息生命周期 P0 | ✅ 有效 | ⚠️ 2 处修正：traceparent 已存在、NATS ack 已等待但 error 不 propagate | ✅ 新方向 |
| 二：出站 HTTP SSRF P0 | ✅ **有效且紧急** — unfurl + webhook 均零保护 | ✅ 锚点全部准确 | ✅ 新方向 |
| 三：消息撤回 P1 | ✅ 分析有效 | ✅ 代码验证准确 | ❌ **零重叠声明不成立**——已在 `round-18` 和 `five-critical-gaps-final-scan` 中以核心方向覆盖 |
| 四：多级限流 P1 | ✅ 有效 | ⚠️ 1 处：层 4 归因偏（per-connection 中间件覆盖所有请求，非仅 AI） | ✅ 新方向 |
| 五：NATS 生产化 P2 | ✅ 有效 | ⚠️ 2 处：`-js` 已启用、流非隐式创建而是声明式但不完整 | ✅ 新方向 |

**关键发现（P0）**：方向二的 SSRF 是真实漏洞——`ReqwestUnfurler` 连 `.redirect()` 策略都没设，攻击者可通过 redirect 链从公网 URL 打到 `169.254.169.254`（云元数据 API）。这是需立即修复的 P0。

**事实修正 5 处**已记录在 `2026-07-11-global-scan-five-cross-system-blindspots.out.md` 中，全部不影响方向核心价值。
