# Round 3 — 后端新功能：消息撤回（message recall）

流水线：backend-feature-pipeline（plan → implement → meta 对抗审查 → VERDICT 裁决门）
会话：shared（aero-r3-recall / aero-r3-recall-fix）· 决策日志：docs/DECISIONS.md · 归档：docs/archive/

## 特性内容

- `POST /api/messages/:id/recall` + WS `recall_message` 帧（同服务方法单一不变量）
- 权限：作者或房间 owner/admin（纯函数 `recall_authorized`，表驱动矩阵单测）
- 稳定失败路径：404 未知 → 403 无访问（无存在性 oracle）→ 409 已删除 → 409 已撤回 → 403 无权
- 持久化：迁移 0238（recalled_at/recalled_by）；行锁下存储层重查 `recalled_at IS NULL` 围栏
- 事件：`Recalled` room event 经 outbox 广播，客户端 render.js 渲染占位
- 指标：`aero_messages_recalled_total` + 耗时直方图（op=recall）

## 对抗门禁的价值（核心验证）

meta 审查 + 独立裁决门**两次 FAIL 阻断**，抓到 5 个真实缺陷（全部复现）：

| # | 缺陷 | 级别 | 修复 |
|---|---|---|---|
| 1 | `edit_locked_outboxed_in_tx` 无 recalled_at 围栏 → unfurl_bot 慢网络后系统编辑复活原文 | P1/HIGH | 存储 UPDATE `AND recalled_at IS NULL`（authorization.rs:319）+ 单测 |
| 2 | `changes_since` 用 GREATEST(edited_at, deleted_at) 遗漏 recalled_at → 离线重连永不收到撤回 | P1/P2 | 改为三列 GREATEST + 表达式索引（迁移 0238） |
| 3 | 客户端 held-id 去重 `arr.some → return` 丢弃重放占位 | P2 | 重放行走变更漏斗 applyChange |
| 4 | SPA 无 recallMessage/UI 入口 | P2 | api.js/ws.js + 悬浮菜单，409→成功映射 |
| 5 | 迁移 0238 未重发 `backfill_messages_partition` → 分区切换丢撤回列 | HIGH | 重发含撤回列的回填函数 |

门禁循环：implement → gate FAIL → implement（喂回缺陷清单）→ gate FAIL → implement → **gate PASS**。
首次 implement 完成报告因格式不合规被 completion 门禁拒绝（Definition-of-Done 生效），
由编排者以真实命令证据补写后放行。

## 门禁验证

cargo check / clippy(-D warnings) / test（17 块全 ok + recall 专项）/ web-check /
truth-check / test-integration.sh（585+ 用例含迁移链 0238）全部通过。
提交：5caab9e（含预提交钩子 cargo check）。
