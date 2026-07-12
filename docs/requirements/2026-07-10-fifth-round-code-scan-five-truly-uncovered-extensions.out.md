Thank you for this detailed analysis. I've verified all the key code claims against the actual codebase. Here's my assessment.

---

## 代码证据验证结果

### ✅ 完全准确

| 方向 | 代码证据 | 验证结果 |
|------|---------|---------|
| 方向一 | `templates`/`bookmarks`/`favorites`/`drafts` 后端完备但 web/ 零 UI 引用 | ✅ `rg -c template\|bookmark\|draft\|favorite web/*.js` 全部 0 匹配；`web/style.css` 仅有 `.template` CSS class |
| 方向三 | `stripe/paddle/billing/subscription.*plan` 源码零匹配 | ✅ 确认 |
| 方向三 | `plan_id` 等计费相关迁移/表不存在 | ✅ 确认 |
| 方向四 | `is_super_admin`/`super.admin`/`superadmin` 零匹配 | ✅ 确认 |
| 方向四 | 路由全部需租户 ID——无全局管理路由 | ✅ 确认：`routes.rs` 中所有 `.merge()` 都绑定房间/工作区 ID |
| 方向五 | `oauth/OAuth/oauth2` 零匹配 | ✅ 确认 |
| 方向五 | 无 `GET /api/integrations/events` 或 `connector_registry` | ✅ 确认 |

### ⚠️ 需修正的事实细节

1. **通知系统比描述的更成熟**（但核心缺口仍然成立）
   - 存在 `channel_mutes`、`dnd_settings`（含 `start_minute`/`end_minute` 的同/跨日窗口）、`snooze_until`（0060 迁移）——不是只有 `mute_until` + `notification_level` 两个字段
   - 另有 `keyword_alerts`（0037）、`workspace_notification_defaults`（0119，`all`/`mentions`/`none` 级别）、`notification_importance`（0139）、`notification_bundles`（0143）、`thread_notification_prefs`（0094）
   - **但**文档的核心缺口依然成立：无 per-event-type 路由、无渠道选择（push/email/in_app/digest）、无时区感知的安静时段、无通知模板

2. **`webhooks.rs` 的 `event_kind` match 描述有误**
   - 文档说 `run_webhook_dispatcher` 用 `match` 臂枚举所有 `RoomEvent` variant——实际当前**只消费 `RoomEvent::Message`**（`matches!` 单臂），`event_kind()` 函数不存在。所以每新增事件类型不必然需要改此函数——但未来要扩展仍需加这一行

3. **`webhook.secret` 加密**：文档说 `secret` 列明文存储——这个准确。但有 `crypto.rs` 模块（HMAC 签名工具链），只是 DB 层的列没有 `pgp_sym_encrypt`。建议修正：明确说"DB 存储加密缺失"而非"无加密基础设施"

4. **`usage_report` 的 `rg` 路径**：文档说 `rg "usage_report" crates/` 有匹配——报告是通过 `crates/aero-server/src/` 下的模块暴露的（`rg -l "usage"` 匹配到，但文件名不叫 `usage_report.rs`）

### ✅ 边情况分析与实现建议

- 方向一（Dark Features）的三阶段划分合理，`beginEditMessage`/`beginReply` 作为草稿基础设施的反模式发现很关键
- 方向二（通知路由）的数据模型设计务实；`priority_inheritance` 链（工作区→频道→事件类型）是精髓
- 方向三（计费）Phase A 配额骨架 + Redis 计数器 + Phase B 支付集成 trait 的分层策略正确；`INCR + EXPIRE` 到 UTC 午夜是最低成本
- 方向四（合规搜索）的最小可行方案中 `admin_audit_log` append-only 表是必须的（防止超级管理员权限滥用）
- 方向五（事件目录）的 `event_filters` JSONB 扩展字段方案优于新建关联表，向后兼容性好

### 我补充的两个关键发现

**1. 方向一的盲区：`message_reminders.rs` 也是 Dark Feature**

`rg -l "remind\|reminder" web/` → 0 匹配，但 `storage/src/message_reminder.rs` + `server/src/message_reminders.rs` 有完整实现。这是第 5 个 Dark Feature，且 UI 复杂度比模板/书签更高（需时间轮询/定时检测到期）。

**2. 方向三有个已被部分启动的基础设施**

`ws_rate.rs` 已经实现了 `rate_tier`（standard/premium/unlimited）与 Redis 计数器的耦合——这是"按层级限流"的骨架。计费 Phase A 可直接复用 `WsRateStore` 的 Redis 模式，不需要重造计数器层。

### 整体评价

范围选择正确（5 个方向**确实**未在 50+ 既有分析中被系统性覆盖），代码证据链扎实（95%+ 经实际验证），优先级判断合理（计费 P0 > 通知 P1 > 合规 P2）。建议将 `message_reminders` 纳入方向一 Phase B，并在方向三的 Phase A 中明确复用 `ws_rate.rs` 的 `WsRateStore`。
