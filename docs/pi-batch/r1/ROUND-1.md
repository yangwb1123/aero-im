# Round 1 — 基线自检与需求评估（CLI 特性全家桶）

日期：2026-07-12 · 维护框架：ai-batch-runner（pi-batch.py + pbatch/ + 规范资产）
目标仓库：/home/u1/aero-im（Rust workspace + 原生 ES-module SPA）

## 使用的特性

| 特性 | 命令 | 结果 |
|---|---|---|
| `check`（quality + 注册表 schema + eval 27 用例） | `pi-batch check` | ✅ 全部通过 |
| `classify` 任务类型判定（双语关键词） | `classify "消息撤回…接口，数据库迁移与事务"` | backend score 10 |
| `classify` 前端路由 | `classify "web 前端草稿持久化组件"` | frontend_ui score 4 → route_frontend |
| `assess` 需求评估（8 维完整性/规模/克制处方） | `assess "消息撤回与发送确认…"` | demo 档处方 1 条必选 + 刻意未选用清单 |
| `rules` 规则匹配 + LLM 双向校验 | `rules "草稿持久化组件" --llm-json` | tier=standard 3 条适用，business-profile 跳过 |
| `context` 上下文路由 | `context "消息撤回"` | 无匹配文档（aero-im 尚无 agent-context，显式警告） |
| `eval` 规则回归套件 | 内含于 check | 27/27 PASS |
| `memory ingest` 渐进式 memory | `memory ingest` | 导入 1157 条既有 Pi session 元数据 |

## 基线门禁（修复前）

| 门禁 | 状态 |
|---|---|
| `cargo check --workspace --all-targets` | ✅ |
| `cargo test --workspace --lib`（1956+ 用例） | ✅ |
| `bash scripts/test-integration.sh`（585 个 PG 门控用例） | ✅ |
| `bash scripts/web-check.sh`（56 文件/108 import） | ✅ 0 违规 |
| `bash scripts/truth-check.sh` | ✅ 0 orphan（3 个 unwired builder 警告） |
| `cargo clippy --workspace --all-targets -- -D warnings` | ❌ aero-common 16+29 处错误 |

## 工具修复（dogfooding，本次已提交到 ai-batch-runner）

在把验证器适配 aero-im 时发现并修复 3 处工具缺陷：
1. `check-backend-quality.py::_ddl_warnings`：单捕获组交替正则 `findall`
   返回空串时 `""[0]` 抛 IndexError 崩溃（aero-im 的 `ALTER TABLE ... DROP`
   字符串触发）；改为防御性取值。测试 `test_dangerous_ddl_detected` 保持通过。
2. `check-backend-quality.py` TS 专属规则误伤 Rust：`Box<dyn Any>` 被报
   "TS any usage"、`task.status = "x"` 被报 "direct status assignment"、
   `html: &str` 字段被报 "unsafe innerHTML"、测试文件 DDL 字符串被报
   "dangerous DDL" —— 全部按语言语境修正，非 strict 下 aero-im 672 个
   crate 文件 0 违规。
3. `check-frontend-quality.py` DEFAULT_EXCLUDES 缺 `vendor`（与 docstring
   矛盾），vendored 第三方文件被计入上帝文件违规；已补。

## 本轮选定的后续实现方向（证据在 docs/requirements/）

- **Round 6 后端新功能**：消息撤回与发送确认 —— `grep recall/unsend/retract`
  零实现（round-33 方向二，从未独立分析）。
- **Round 7 前端新功能**：草稿持久化前端接线 —— 后端 `/api/drafts`
  （crates/aero-server/src/drafts.rs）已实现，`web/*.js` 零引用（round-9
  验证结论）。
- **Round 2 修复**：clippy 门禁 16+29 处错误。
