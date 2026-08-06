Plan written to `docs/pi-batch/message-recall-plan.md` (v2, supersedes the original). No implementation code written.

## Summary

**Assessment**: `pi-batch.py assess` → 处方档 demo(S), 工作流 L3_platform, 产品化 L2_platform_capability, 多租户信号 (0 hard rules). 缺失的 main_flow/data_source/error_path/tech_stack 由计划 §4 补全。

**Tree state verified** (I inspected every fix claim against source, not just reviews):
- 初版实现 59 文件未提交；门禁 REJECTED 的 5 缺陷**已全部修复在树**：
  1. P1 系统编辑复活 → `events.rs:87/192` 行锁读后 `recalled_at.is_some() → Ok(None)` 双围栏（覆盖 unfurl/transcribe/webhook 全部调用方，同时消除 DS-1 的 relay 静默丢弃）
  2. P1/P2 replay 不送达 → `query.rs:180-181` 三列 `GREATEST(edited_at, deleted_at, recalled_at)` + 0238 重发 `idx_messages_room_mutated`
  3. P2 客户端丢占位 → `app.js:219` held-id 行走 `applyChange` 守卫漏斗
  4. P2 SPA 无入口 → `ws.js:533`/`api.js:376`/`render.js:809`/`app.js:666`（含 409→success 映射）
  5. HIGH backfill 缺列 → 0238 重发 `backfill_messages_partition`，INSERT/SELECT 双投影含 recall 列
- 每项都对应树内回归测试（`recall_tests.rs` ×3 门禁回归 + 既有 8 用例 + im-core 3 用例 + frame 契约测试）

**计划发现并列入的 3 个残留测试缺口（必须 failing-test-first）**：
- ① `materialize_outbox_payload` Recalled 臂无单测（delivered vs suppressed 两分支）
- ② 并发双撤回竞态无测试（`tokio::join!` 恰一成功一 409、恰一条 outbox）
- ③ **web 测试零 recall 覆盖**（`web/*.test.js` 全库 grep 无 recall）——ws 帧发送/409→success/按钮可见性矩阵/held-id 漏斗/`_lastSeen` 卫生

**计划结构**：§0 评估+树状态+门禁裁定表 · §1 模块边界/数据所有权（storage 唯一写者、im-core 编排+纯函数、server 薄壳）· §2 Persistence Design §12 全模板（聚合状态机、0238 单迁移、身份、9 步单事务一致性边界、快照字段、并发、查询+索引、历史、删除、Expand–Migrate–Contract + DS-4 部署纪律）· §4 API 契约+稳定错误表（404→403→409→409→403 固定顺序防 oracle）+ 幂等（原子单迁移、409 即成功约定）· §5 五层测试计划（含门禁回归表）· §6 变更半径 · §7 门禁缺陷 failing-test-first 清单（5 修复已核查 + 3 缺口先写先红）· §8 DoD。
