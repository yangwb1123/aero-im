# 十轮维护总结 — Aero IM × ai-batch-runner 全特性

日期：2026-08-06 · 目标仓库：/home/u1/aero-im · 工具：/home/u1/ai-batch-runner
（配置 /home/u1/aero-im-batch/pi-batch.yaml，经 PBATCH_SCRIPT_DIR 注入）

## 十轮执行记录

| 轮 | 使用的特性 | 产出 |
|---|---|---|
| R1 | check（quality+registry+eval 27）、assess、rules+LLM 双向校验、classify、context、memory ingest、eval | 基线门禁体检；3 处工具缺陷修复；选定 recall/草稿方向 |
| R2 | backend-fix-pipeline（plan→implement→meta 审查→VERDICT 门）、cargo-clippy/check/test 门禁、--session-mode shared、决策日志 | **clippy -D warnings 全绿**（173 文件，16+29+66 处错误） |
| R3 | backend-feature-pipeline、backend-specs、completion/backendquality 门禁、meta 对抗审查（5 角色）、gate 循环 | **消息撤回特性**：REST+WS、权限矩阵、迁移 0238；gate 2 轮 FAIL 抓到 5 个真实缺陷（unfurl 复活/重放遗漏/去重丢占位/无入口/回填缺列）后 PASS |
| R4 | campaign（发现→评分→实施）、docs/auto 状态、--reuse/--retry-failed | 通知扇出集成测试（40 用例+runbook+CI 接线）、**SSE UTF-8 流解析修复**（CJK 损坏 bug，6 新测试） |
| R5 | classify 前端路由、frontend-pipeline、web-check/uiquality 门禁、gate 循环 | **草稿持久化前端接线**（258 行模块、防抖/串行/镜像/双路径清发送）；gate 2 轮 FAIL（Enter 路径复活/keepalive 竞态/粘性 403）后 PASS |
| R6 | ai/run-review.py 十阶段评审（02/04/06）、backend-fix-pipeline | 安全评审发现 F1（索引写入复活→SQL 围栏+竞态测试）F3（限流缺失→补齐）；**测试隔离修复**（--skip 子串过宽根因定位，worktree 复现） |
| R7 | advance 自推进（P0/P1/P2 分批、state.jsonl） | 5 处硬编码颜色/间距 → CSS token；N+1 误报甄别（3 处均为刻意的串行设计） |
| R8 | full-SDLC 闭环（需求→设计→对抗→设计门→实现→验收门→归档） | **撤回时间窗**（AERO_RECALL_WINDOW_SECS，24h 默认，admin 豁免，行锁原子评估，15+ 测试）；双 gate PASS |
| R9 | memory ingest/find/recent、learn（Evolution Engineering） | 2 份规则草案（--skip 子串事故、completion schema 事故）；流水线提示词内嵌 schema（进化闭环） |
| R10 | eval（37 用例）、rules --check、check | 回归全绿；工具修复提交；本汇总 |

## 特性覆盖清单（AGENTS.md §1）

✅ 全部使用：check/assess/rules/classify/context/memory/learn/eval/advance/campaign/
pipeline/meta/gate/decision_log/git_commit/archive/validators（cargo-*、web-check、
truth-check、integration、backendquality、uiquality、completion）/sessions（shared、
per-stage）/retries/max-rounds/reuse/ratelimit 配置/evidence 有界注入/fail-closed schema。

## 修复与新功能

**新功能（3）**：消息撤回（含撤回时间窗）、草稿持久化前端、SSE UTF-8 流解析修复、通知扇出集成测试。

**修复（4 类）**：clippy 全量门禁、撤回安全围栏（F1/F3）、集成测试隔离（3 个顺序依赖测试）、5 处 UI token 违规。

**工具修复（5 类，dogfooding）**：backend-quality DDL 崩溃、TS 专属规则误伤 Rust、
vendor/.claude/target 排除、campaign 资源边界适配、completion 报告 schema 反馈闭环。

## 质量证据

- 门禁：cargo check/clippy(-D warnings)/test（17 块全 ok）、web-check 0 违规、
  truth-check 0 orphan、test-integration.sh 全部通过（585+ 用例、迁移链至 0238）
- 工具自检：check 全绿（quality+registry+37 eval）、tests/ + checks/ 全绿
- 提交：aero-im 72 个提交（af1fa89..HEAD，339 文件 +25337/-1533）；工具 6 个提交
- 决策日志：docs/DECISIONS.md（追加式，含被否决策）；归档：docs/archive/ 5 个时间戳目录

## 对抗门禁战绩（交叉验证价值）

10 次 VERDICT 裁决中 **5 次 FAIL 后被修复**：recall（2 轮）、drafts（2 轮）、
安全修复（1 轮）。每轮 FAIL 都带可复现缺陷清单（复活路径/竞态/去重/围栏/预算），
修复后门禁 PASS。completion Definition-of-Done 门禁 5 次拒绝不合规完成报告。

## 遗留项（诚实边界）

- 撤回 403 vs 404 契约决策（F4/F5 安全评审，Low）、历史保留契约（F2，产品决策）
- 撤回限流/迁移并发安全/双客户端 E2E（stage 06 H2/H3/M3，运营验收项）
- aero-im 内存在已提交的 pbatch/ 工具副本（先前 agent 遗留，未清理）
- docs/campaigns/（compose-017 并行进程产物，未纳入提交）
