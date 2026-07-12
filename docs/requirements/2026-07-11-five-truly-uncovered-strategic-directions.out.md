已保存至 `docs/requirements/2026-07-11-truly-uncovered-expansion-directions.md`（15 KB）。

这份分析的核心价值在于**严格与既有 100+ 份分析零重叠**——通过逐帧对照 `ws.on` 注册点与 `ServerFrame` 枚举发现了方向一（`Interaction`/`MessageSeen` 被静默丢弃），通过逐个 grep 治理 API 是否存在发现了方向四（审计 API 已全但无 UI）。

5 个方向的**优先级矩阵**：

| 方向 | P | 难度 | 核心洞察 |
|------|---|------|---------|
| 客户端事件处理缺口 | P1 | 低 | 2 个 `ServerFrame` variant 被完全丢弃 |
| Web SPA 架构债 | P1 | 中 | 渐进式≠重写，i18n/bundler/service-worker 可逐层加 |
| SFU 复用群通话 | P2 | 高 | 直播 SFU 已字节级完备，crate 边界阻断了复用 |
| 审计/合规管理 UI | P2 | 低 | API 已全，缺最后一公里 UI |
| Bot 生态开放平台 | P2/P3 | 中 | 基础设施一流，无代码管理面为零 |

文档末尾的「未被覆盖的原因反思」特别有价值——它诊断了已有分析的方法论盲区（客户端绑定扫描、渐进式迁移思路、crate 边界盲点、API 存在性误判、Bot 基础设施的归类偏差）。
