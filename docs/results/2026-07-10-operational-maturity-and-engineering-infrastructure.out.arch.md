分析文档已写入 `docs/analysis/2026-07-12-architectural-analysis-operational-maturity.md`。

## 分析总结

### 关键发现

**源文档的准确性修正**（通过源码验证）：

| 文档声称 | 实际 | 影响 |
|----------|------|------|
| `hub.rs:553`、`forward.rs:179` 生产路径 panic | **实测在 `#[test]` 块内** | 高估了生产 panic 数量，但非测试 panic 仍有约 **15 处**（`commands.rs`、`channels.rs:743`、`bin/main.rs:146` 等） |
| `monitoring/` 目录为空 | **实际已有** Grafana RED dashboard JSON、Prometheus SLO burn-rate alert rules、recording rules、Alertmanager 配置 | 不是「没做」而是「未接入 scrape + 告警路由」 |
| 80+ 个 `XRepo` 文件 | **实际 146 个** `.rs` 文件 | 问题比文档描述的更大 |

### 核心架构判断

1. **最大的架构债务 = 迁移生命周期**：157 次单向迁移 + boot 时全量重放 + `CREATE INDEX CONCURRENTLY` 被事务包装 = 部署不可逆 + blue-green 风险
2. **最紧急的修复 = CI 集成测试启封 + 非测试 panic 消除**：前者是最低成本最高收益（取消 `ci.yml` 注释即可），后者是改动 15 处 panic 换来生产稳定性
3. **连接池隔离比全 trait 化仓储重要**：80+（实际 146）仓储共享单池 = 单体故障放大器。建议按域分 5 个池（IM/Live/AI/Auth/System），每个池独立 `max_connections` + `acquire_timeout`——不做全 trait 化（不经济）

### 5 个扩展方向优先级

| ID | 方向 | 优先级 | 工作量 |
|----|------|--------|--------|
| A3 | CI 集成测试启封 | **P0** | 2 天 |
| A4 | 非测试 panic 消除 | **P0** | 3 天 |
| A1 | 连接池隔离 + 查询级可观测 | **P0** | 5 天 |
| A2 | 迁移生命周期 v1（分级 + metric） | **P0** | 3 天 |
| B1-B3 | 错误枚举、fuzz、路由拆分 | **P1** | 各 2-3 天 |

### 技术选型评估

- **不做**：Kafka 替换 NATS、gRPC 替换 HTTP、Service Mesh、全 trait 化仓储
- **引入**：cargo-fuzz（dev-dep）、criterion（bench）、cargo-watch（dev-tool）
- **自建**：错误码枚举、迁移生命周期管理、查询监测（轻量）
- **已有但需维护**：`monitoring/` 下的 Grafana 仪表盘 + 告警规则
