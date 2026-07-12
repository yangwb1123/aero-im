文档已保存。494 行，完整无截断。

**文件**：`docs/requirements/2026-07-11-codebase-scan-five-production-gaps-email-load-test-deeplink-verify-notify.md`

**既有相似文件**检查——与以下文件**内容不同**，本文是独立的代码扫描分析：

| 既有文件 | 与本文关系 |
|---------|-----------|
| `2026-07-12-five-production-gaps-missed-by-130-analyses.md` | ≈ 同期但不同分析（方向重叠但方法不同；本文基于代码扫描 + grep 证据链，前者基于文档标题比对） |
| `2026-07-11-round-8-global-scan-five-strategic-expansion-directions.md` | 第 8 轮全局扫描，方向不同（覆盖度 60 份 vs 本文 130+ 份） |
| `2026-07-11-five-zero-coverage-systemic-gaps.md` | 不同方向集合 |

**5 个缺口总结**：

| 方向 | 严重度 | 核心证据 |
|------|--------|---------|
| ① 邮件基础设施成熟度 | **P1** | mailer.rs 仅纯文本 + 同步发送 + 零 DKIM，6 道缺口 |
| ② 负载与性能测试真空 | **P1** | 819 UT 全绿，但零 load test / WS fuzz / API contract / bench |
| ③ Web SPA 深度链接缺失 | **P2** | `rg "location.hash\|history.pushState" web/` → 0 命中，纯柱塞式视图切换 |
| ④ 注册身份验证真空 | **P1** | 注册 6 步流程零验证，`rg "email_verified" migrations/` → 0 |
| ⑤ 邮件通知渠道空白 | **P2** | 有 FCM/APNs 但零邮件偏好/调度/回退，`notif_prefs.rs` 无 email 字段 |
