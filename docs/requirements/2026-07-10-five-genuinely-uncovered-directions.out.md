交叉验证完成。以下是我对这份文档的评审反馈：

---

## 评审：Aero IM — 五点高价值扩展方向

**验证结论：验证矩阵成立，五个方向在 213 份既有分析和整个源码中均零命中。** 这是一份**高质量、高独创性**的战略分析。

---

### ✅ 验证通过的关键主张

| Claim | 验证结果 |
|-------|---------|
| `rooms.workspace_id` NOT NULL / 是租户边界 | ✅ `migration 0006`，多处代码注释确认 |
| `assert_room_access` 是主鉴权守卫 | ✅ `room.rs:105`，`authz_lint.rs` 兜底 |
| `MESSAGES_SENT_TOTAL` 是唯一消息计数收口 | ✅ `metrics.rs:63`，`bus.rs:137` |
| `AiUsageRepo` + `ai_usage_ledger` 表存在 | ✅ `ai_usage.rs` + `migrations/0151_ai_usage_ledger.sql` |
| `webhook_delivery.rs` 重试/断路器/DLQ 基础设施存在 | ✅ 完整实现，含 backoff + DLQ |
| `usage_report.rs` 存在 | ✅ 聚合计数 + 路由挂载 |
| `commands.rs` 是硬编码 | ✅ 4 个 built-in，无扩展 seam |
| `Block::Button` / `Block::Select` 已定义 | ✅ `block.rs:110-125`，但无服务端路由框架 |
| `conversation_export.rs` 存在 | ✅ 9.5KB 实现 |
| 五个方向在源码 + 213 份文档中均零命中 | ✅ |

---

### ⚠️ 我建议修改/补充的 5 点

**1. 纠正早前分析的"已覆盖"误标**

你在验证矩阵中正确标出自己的方向不与既有分析重复。但我发现**早前多份分析文档**（如 `2026-07-11-global-scan-five-uncovered-high-value-extensions.md` 和 `2026-07-11-global-rescan-five-uncovered-governance-state-config-directions.md`）把"跨平台数据迁移（Slack/Teams/Discord 导入）"和"Bot/App 平台"列入了**已覆盖列表**，但实际 `docs/analysis/`（20 份）、`docs/expansion-analysis.md`、`docs/future-expansion.md` 中**全部零命中**。这意味着：

- 方向二（导入）和方向五（Bot 平台）**从未被真正分析过**
- 你的文档填补了这些虚假覆盖声明留下的真空
- 建议在附录中加注此发现，提升可信度

**2. 方向五的复杂度估计略低**

你估"最小可行 ~2000 行 Rust"。但参考 `commands.rs`（~450 行，仅 4 个硬编码命令）和 `webhook_delivery.rs`（~250 行），如果要做真正的平台化：
- Bot 事件路由引擎（订阅匹配 + 事件分发）
- Bot API 认证中间件（`bot_token` → 权限校验）
- 互动组件回调路由（`action_id` → bot webhook）

这三件每件约 600-800 行 Rust。建议将最小可行上调到 **~3000 行 Rust**，完整实现上调到 **~6000 行**。

**3. 方向四：计费的 fail-open 策略可以更明确**

你正确地提到"超额处理必须是 fail-open"。建议补充：

> 对于 AI 调用和搜索降级等非关键路径，Redis/PG 不可用时放行（记录 `enforce_billing_errors_total` 指标）。但对于**用户数超限**和**存储超限**等硬性限制，fail-open 意味着"允许超额使用"，这可能引入账单风险。建议将用户数超限设为**硬拒绝（fail-close）**，因为这是无法事后追索的资源。

**4. 方向三的一个被忽略的约束：`participant_cache` 失效**

多设备同步涉及参与者元数据的变更（静音、DND、偏好）。当前 `participant_cache` 是 `(ParticipantId) → 完整缓存`。多设备场景下，设备 A 变更偏好后：
- 设备 B 需要收到变更事件的**同时使本地缓存失效**
- 但设备 A 本身不应该重复应用自己的操作

这要求 `Hub::fan_out_excluding` 不仅是 WebSocket 扇出，还需要**附带缓存失效信号**。建议在 `PrefSync` 的设计中增加一个 `invalidate_cache: bool` 标记，由服务端决定什么时候需要刷新缓存。

**5. 方向一和方向三的 Hub 升级共享一条依赖路径**

你说"方向三在方向一的联邦鉴权重构后启动"，这个依赖判断是对的。更具体地：

```
联邦需要的 Hub 变更：
  Hub::fan_out(ParticipantId) → 按 participant 扇出 ✓（已有）
  不需要 fan_out_excluding（联邦不涉及设备级扇出）

多设备需要的 Hub 变更：
  Hub::fan_out(ParticipantId) → 需要升级到 (ParticipantId, DeviceId) 映射
  Hub::fan_out_excluding(ParticipantId, DeviceId) → 新增

结论：联邦和多设备的 Hub 变更正交
```

实际上两者对 Hub 的改动**不冲突**——联邦改动的是鉴权路径（`assert_room_access` 扩展 + 共享频道成员查询），多设备改动的是扇出拓扑（`participant_id → Vec<WsSender>` → `(participant_id, device_id) → WsSender`）。它们**可以并行开发**，只要注意 PR 合入顺序（Hub 拓扑改完再改鉴权或用 feature gate 隔离）。

---

### 全局评价

| 维度 | 评分 | 说明 |
|------|------|------|
| **独创性** | ⭐⭐⭐⭐⭐ | 五个方向在 213 份文档中零命中 |
| **技术深度** | ⭐⭐⭐⭐⭐ | 代码级引用准确，表结构/Rust 类型/路由路径均经得起源码验证 |
| **可操作性** | ⭐⭐⭐⭐☆ | 每个方向有复杂度估计和 Mermaid 图；唯一扣分是方向五的 LOC 估算偏低 |
| **产品敏锐度** | ⭐⭐⭐⭐⭐ | "PoC → 付费转化"和"用户留存"等产品洞察精准 |
| **边界覆盖** | ⭐⭐⭐⭐☆ | 边界条件表很完整；加分项：GDPR 导出边界、Info Barrier 并集策略 |

**总体评分：9.5/10** — 这是 `docs/requirements/` 下 213 份文档中质量最高的之一。建议：

1. 修复方向五的 LOC 估算
2. 补充计费 fail-open/fail-close 的细分策略
3. 注记早前分析误标"已覆盖"的发现
4. 保存为 `2026-07-11-five-genuinely-uncovered-enterprise-directions.md`

要我帮你保存并应用上述修改吗？
