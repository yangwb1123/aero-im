# 消息撤回时间窗 (Recall Window)

消息撤回（`POST /api/messages/:id/recall` 与 WS `recall_message` 帧）的**作者限时窗口**：
消息发出后，仅作者可在 `AERO_RECALL_WINDOW_SECS` 秒内撤回；超窗后作者撤回返回
`409 Conflict`，消息内容保持原样。

## 语义

- **窗口锚点**：`messages.created_at`（发送时间）。编辑不重置窗口。
- **边界（含端点）**：`now − created_at <= window` 时允许撤回；严格大于窗口即过期。
  即 `t = window` 仍可撤回，`t = window + 1s` 返回 409。
- **仅作者受限**：房主/管理员撤回是审核路径，**不受窗口约束**（任何年龄都可撤回）。
  **作者身份优先**：若房主/管理员同时是消息作者，以作者身份撤回仍受窗口约束
  （实现按 `sender_id == actor` 判定；`recall_window_expired_author_rejected_admin_override`
  固化该契约——作者以 owner 身份入房仍收到窗口 409，独立管理员才豁免）。
- **配置**：`AERO_RECALL_WINDOW_SECS`（plain env，非 `AERO__SERVER__*`）。
  - 默认 `86400`（24h）；
  - `0` = 不限（等价于引入本功能前的行为）；
  - 未设置 / 非法值（负数、非数字、溢出）回落默认 86400。
  - 启动时在日志中输出一次生效值（`recall_window_secs` / `recall_window_unlimited`），
    便于区分「静默回落默认」与「0 = 关闭窗口」两种部署状态。

## 错误契约（稳定）

- REST：`HTTP 409` + `{"code":"conflict","msg":"conflict: recall window expired"}`。
- WS：`{"type":"error","code":"conflict","msg":"conflict: recall window expired"}`。
- `msg` 携带 thiserror Display 前缀 `"conflict: "`（与所有 `Conflict` 一致）；
  客户端判别须前缀宽容（`web/recall_errors.js` 的 `recallErrorToast` 用正则匹配）。
- **判定顺序（不可变）**：404 未知消息 → 403 无房访问 → 409 已删除 → 409 已撤回 →
  403 非作者/非管理员 → **409 窗口过期**。状态检查先于窗口检查：过期且已删除的消息
  仍报「已删除」；普通成员探测过期消息只得到 403，绝不泄露窗口状态。
- 409 从**不做自动重试**；「已撤回/已删除」的 409 可视为目标已达成（静默收敛），
  「窗口过期」的 409 是拒绝而非成功——前端必须向用户展示（info toast）。

## 实现位置（双保险）

1. **服务预检** `ImService::assert_message_recall_preflight`（REST 与 WS 共用，
   在扣减工作区限流预算**之前**返回 409——注定失败的请求不消耗共享预算）。
2. **事务围栏** `MessageRepo::recall_outboxed_authorized`：在
   `lock_message_in_tx`（`FOR UPDATE` 行锁）拿到快照后、`UPDATE` 之前对锁定行
   求值窗口——原子，无并发穿透；`UPDATE` 的 `WHERE` 围栏
   （`recalled_at IS NULL AND deleted_at IS NULL`）保持不变。
   纯谓词 `aero_common::recall_window_expired` 两处共用，两侧都用
   `aero_common::time::now_utc()`（app 时钟，与 `created_at` 写入同源）。

## 指标

- `aero_messages_recall_expired_total`：预检拒绝的窗口过期撤回次数（REST+WS
  唯一收口点，每拒绝计一次）。与 `aero_messages_recalled_total`（成功撤回）
  对照可观察窗口调参效果。
- 边界竞态下由事务围栏拒绝的极小概率事件不计数（storage 层不发指标）。
- 限流前的作者探测可使该计数器增长：REST 侧受每客户端 HTTP 限流（默认 20 rps）约束；
  WS 侧在升级后无逐帧限流，已连接作者可持续发送过期消息的 recall 帧推高该计数
  （与既有 edit preflight 同类，非新漏洞类）——告警阈值需容忍，逐连接帧预算属后续加固。

## 非目标

- 不支持「撤销撤回」（recalled 是一次性终态）。
- 不支持按消息/房间/工作区覆盖窗口（仅全局 env）。
- 删除（`delete_message`）不受窗口约束（作者永久删除路径，既有行为）。
- 无撤回倒计时 UI、无接收方通知。
- **撤回 ≠ 内容彻底移除**：原文快照（文本/转写，字节引用已剔除）写入
  `message_edits` 并保留在历史路由（房内成员可见，证据链设计）；live 行、
  搜索/向量与附件字节会被清理。
- 窗口边界精度受实例间时钟偏差约束：窗口应远大于配置的时钟偏差上界
  （默认 86400s 下 NTP 秒级偏差可忽略）。
