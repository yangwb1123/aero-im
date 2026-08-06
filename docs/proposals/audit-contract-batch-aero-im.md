实施提案已完成并通过批次校验（`check-no-refusal.py` → OK）。交付物：**`docs/proposals/audit-contract-batch-aero-im.md`**（294 行，未改任何代码）。

## 核心结论

**现状核实**（B5 起点）：v1 审计投递已存在且工作 —— `audit_events`（L0，分区 0146/legal-hold 0154）→ 0236 **AFTER INSERT 触发器**同事务入 `snaplink_delivery_outbox` → `aero-server/src/snaplink_commercial/` 内嵌 relay（cc + `audit:event:write` + 202 receipt）。v2 缺口 = **无 status 枚举、无 dead 终态、无优先级**，relay 非独立 crate。

**逐项设计**（每项含文件定位/设计/验收映射/测试计划）：
- **B5-1**：新迁移 `0239_audit_governance_outbox.sql` —— 专用审计 outbox 表（status 0/1/2/3 normative、`class` message/room/admin、`priority`、`delivery_mode`），`CREATE OR REPLACE` 重定向 enqueue/reconcile 函数；`audit.rs` 是清单锚点（已验证 1049 行），新仓储放同级 `audit_governance.rs`（防 file-size 1200 HARD）；P2 parity = `event_id` 1:1 断言。
- **B5-2**：新 crate（候选 `crates/aero-audit-connector`，无新第三方依赖）—— §1.2 语义（lease > 2×timeout、退避 cap 300s、**422/409/回执错 → dead ≤1 次**、403 → dead = T-11 fail-closed）、cc + claim 校验（iss/aud/scope/sub）、usage relay 原地不动。
- **B5-3**：`priority` 列 + `claim_due` 按 `priority DESC` 排序；本地 `message.moderated` → 出站 `admin.content.flag`/`admin.moderation.action` 映射表；注入积压 drill（500 积压 + 1 moderation → 先达 sink）+ 反饥饿上限。
- **B5-4**：本仓库交付配给验证 seam（`aero-cli audit-provision-check`，[PROPOSED]）+ fail-closed（boot 门已验证存在；readyz 不翻转）；registry 本体在 IdP 仓。

**关键标注**：8 处不可验证/[PROPOSED] 明确列出 —— v2 契约三文档与 `docs/snaplink-platform/v2/` **不在本仓库**（§1.2 原文、"‡ 类"清单、"37/37" 测试清单、aero-id connector、IdP scope registry、sink 行为全部不可独立核对，已按自洽推定设计并留 seam，不冒充 Verified）。

**门禁核对**：T-11 与 moderation drill 本仓库可全绿；37/37 需先把契约测试清单钉入 `test-integration.sh`；B5 起始条件 = B1（sink G2）+ B4-2（scope registry）就绪，本地骨架可用 mock 先行。
