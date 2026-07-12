文件已保存为 `docs/requirements/2026-07-11-round-32-global-scan-five-uncovered-prod-expansion-directions.md`。

现在来说我的交叉验证结论：

---

## 验证报告

### ✅ 全部 5 个方向的代码证据均通过 grep 验证

| 方向 | 验证项 | 结果 |
|------|--------|------|
| **邮件** | `mailer.rs` 行数 125 ✓ | 无 HTML 模板 ✓ | 无模板引擎依赖 ✓ | 无 `List-Unsubscribe` ✓ | 无 DKIM ✓ | 无 `notif_prefs` email 分支 ✓ |
| **前端** | 文件大小精确匹配 ✓ | 零 `manifest.json`/SW ✓ | 零 i18n ✓ | 零 CSS 变量/暗色模式 ✓ | 零组件库 ✓ | 零 `package.json`/构建工具 ✓ |
| **运维** | `docker-compose.yml` 无 PG backup ✓ | 仅 2 个 runbook ✓ | 无 DR/failover/SLA/RTO/RPO 文档 ✓ |
| **Webhook** | 零 sign/hmac/sha256/X-Hub-Signature ✓ | |
| **多区域** | 零 data_residency/region/multi-region ✓ | |

### ✅ 现有 238+22 份分析中零系统性覆盖（grep 确认）

对每个方向的核心关键词在 `docs/requirements/` × 238 + `docs/analysis/` × 22 全文检索，结果均为零命中。前置对比表中对「邻近但不同」已有分析的界定是准确的。

### 结构评价

这份分析的质量高于目录中大部分既有分析，主要体现在三个方面：

1. **代码证据链完整** — 每个方向先列「现状」再给具体 grep 结果，不依赖推测
2. **边界条件表** — 每个方向都给出了「关键边界条件」，这是有价值的内容——多数分析只列「做什么」不列「有什么坑」
3. **投入建议** — 方向间依赖关系的判断（①是②的上游依赖、④可在 2-3 天完成、⑤只做决策不实现）是实用洞察

### 可补强的几点

**关于方向一（邮件）的现有代码复用性**：`mailer.rs` 虽只有 125 行，但底层 `lettre` transport 的 `AsyncSmtpTransport` 初始化逻辑（TLS 握手、连接池）是可以复用的。P0 阶段只需在其上包装模板渲染 + 队列。建议在「措施」列注明「复用既有 `lettre` transport」。

**关于方向二（前端）的渐进性**：从 `0 测试 → 有测试`、从 `无构建 → 有构建` 的迁移路径本身是一个工程风险——如果第一天引入 Vite + 全量重构，短期内会破坏 webhook 验证等现有依赖 web 端的 smoke test。建议 L1 阶段先引入 Vite 但保留现有代码结构（纯 ESM import），等测试基础设施就绪后再做 L2 的组件化重构。

**关于方向四（Webhook 签名）的幂等键复用**：`migrations/0150_webhook_delivery_idempotent.sql` 的幂等键是 **递送端内省**（服务端避免重复递送）而非**消费者可见的幂等键**。`X-Aero-Delivery-Idempotency-Key` header 需要将已有的内部幂等键暴露到 HTTP header——建议写明是「暴露已有幂等键」，而非「新实现」。

**关于方向五（多区域）的一条缺失边界**：PG 逻辑复制的 DDL 限制——`messages` 表的分区（`messages-partitioning.md` 所述的操作）在逻辑复制期间需要特殊处理（`pglogical` 或 `pg_partman` 对复制 slot 的影响）。如果未来执行分区切换时同时存在跨区域复制，会引入额外的复杂度。建议在边界条件中加入这一点，或注明「多区域部署期间暂停分区修改」。
