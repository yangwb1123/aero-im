根据我的验证，我已对这份文档进行了全面审查。以下是详细评估：

---

## ✅ 文档长处

1. **方向一（TURN 凭证）** — 问题真实、代码证据确凿。`rtc_config_payload()` 确实将静态明文凭据嵌入每个 HTTP 响应中。这是生产 WebRTC 的安全阻碍项。

2. **方向二（@everyone 成本）** — 分析到位。`n_token()` 展开缺少门控，O(N) 通知/推送写放大的风险真实存在。

3. **方向三（Interaction/MessageSeen 协议断头）** — `ServerFrame::Interaction` 和 `ServerFrame::MessageSeen` 在后端完整发送，而 `web/app.js:110-128` 的 handler 注册表中确实缺失 `msg:interaction` 和 `msg:message_seen`。代码引用准确。

4. **方向四（Delivery Cursor 客户端缺口）** — `delivery_cursors` 仓储 + REST API + WS `?cursors=` 参数已建，但 Web SPA 的 `ws.markRead()` 只发 `mark_read` 帧，不调用 `PUT /api/rooms/:id/delivery-cursor`，也不在 reconnect 时读取。分析正确。

---

## ❌ 需要修正的问题

### 问题 1：关键声明「未被系统性覆盖」不准确

文档声明*"确认以下 5 个方向未被系统性覆盖"*，但实际验证发现：

| 方向 | 既有覆盖情况 | 示例文件 |
|------|-------------|---------|
| **方向一（TURN）** | 昨天已有**完全相同的分析**——标题一致、代码引用一致、修复方向一致 | `2026-07-11-code-scan-high-value-extensions.md` 方向一 |
| **方向二（@everyone）** | 已有**系统性分析**——包含代码证据、边界情况、修复建议 | `2026-07-11-core-extension-directions.md` 方向四（「通知风暴防护与广播提及治理」） |
| **方向三（Interaction/MessageSeen）** | **已被至少 5 份文档系统覆盖**——包含相同代码引用、分析、甚至修复代码 | `2026-07-09-truly-uncovered-gaps.md`、`2026-07-10-architectural-blindspots-after-comprehensive-scan.md`、`2026-07-11-round-18-global-scan-five-critical-expansion-directions.md` 等 |
| **方向四（Delivery Cursor）** | **已有覆盖**——`DeliveryAck` 协议缺口分析 | `2026-07-11-post-143-analysis-five-novel-directions.md` |
| **方向五（优雅降级）** | 部分覆盖（降级/断路器/韧性已 132 次匹配） | 多个文档 |

**建议**：将"未被系统性覆盖"改为"既有系统性覆盖不足的五个方向"，或为每个方向如实标注既有分析频次。

### 问题 2：事实性不准确——方向五的 Redis→500 断言

文档说*"Redis 宕机→presence 查询 500→拒绝所有消息发送"*，但实测代码表明：

```rust
// crates/aero-server/src/online.rs:55-59
let ids = match s.presence.members(room).await {
    Ok(members) if !members.is_empty() => members,
    _ => s.hub.room_members_online(room),  // ← Redis不可达时回落本地Hub数据
};
```

presence 路径已经有降级 fallback。`assert_room_access()` 也完全基于 PG，不依赖 Redis。建议修正为**仅保留 NATS 扇出静默丢帧和 AI 超时无保护**两个实际成立的子问题。

### 问题 3：过时/不匹配的代码引用

- `crates/aero-server/src/routes/routes.rs:2826-2835` — 引用位置偏移约 2-3 行（实际为 2828-2835）
- `aero-im-core/src/service/orig/tests.rs:106-108` — 函数名是 `n_token()` 而非 `is_all_broadcast_token()`
- `crates/aero-server/src/state.rs:279` 中 `delivery_cursors` 字段 — 无误，但 WS 协议中已加入 `?cursors=` 参数支持多设备收敛（`ws_impl/mod.rs:53-56`），文档未提及此进展

### 问题 4：缺少对既有基础设施进展的认可

方向四（Delivery Cursor）的分析遗漏了一个关键事实：**WS 协议已在 2026-07-11 加入 `?cursors=` 参数**（来自 `WsParams`）：

```rust
/// Opt-in per-room delivery-cursor backfill (ROADMAP 方向三·A).
/// When truthy ... the server resumes each room from this participant's
/// persisted DELIVERY cursor (multi-device-shared) instead of a single
/// global cursor.
#[serde(default)]
cursors: Option<String>,
```

这意味着服务端侧的多设备收敛基础设施已比文档描述的更进一步——文档只提了 REST API，未提 WS 原生支持。

---

## 建议的修正行动

1. **标注来源**：增加脚注承认既有分析的存在（特别是 `2026-07-11-code-scan-high-value-extensions.md` 的方向一完全重叠）
2. **修正方向五的断言**：删除或弱化 Redis 故障级联导致消息被拒的描述
3. **更新方向四**：提及 `?cursors=` WS 参数的存在
4. **补充代码引用版本标注**：注明行号可能因版本漂移
5. **如有意愿作为独立文件保存**：建议命名如 `2026-07-12-five-persistent-production-gaps-revalidated.md` 以反映这是验证式重述而非全新发现

---

## 总体评价

文档的分析质量本身扎实——代码证据链清晰、结构性完整。但**元声明（meta-claim）"未被系统性覆盖"在 5 个方向中有 3-4 个不成立**，这会降低文档在熟悉既有分析上下文的人眼中的可信度。如果修正声明、补充既有覆盖标注，这份文档可以作为一份有价值的**综合验证报告**——确认这些方向经过多次分析后仍未修复，强化其优先级。
