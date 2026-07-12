已写入 `docs/requirements/2026-07-10-architecture-gaps-analysis.md`（370 行 / 13.6 KB）。

5 个方向的简要概述：

| 方向 | P | 核心缺口 | 代码证据亮点 |
|------|---|---------|-------------|
| **SPA 路由/深度链接** | P1 | 纯 `hidden` 属性驱动视图切换，零 `hashchange`/`pushState`，刷新丢全部状态 | `rg 'history.pushState'` → 0 匹配 |
| **API 版本治理** | P2 | 126 个模块全部挂 `/api/*` 裸路径，无版本前缀/无 deprecation 协议 | 42 行手写 OpenAPI 示意 vs 126 个真实模块 |
| **多实例缓存一致性** | P2 | `participant_cache`/`room_member_cache` 的 `invalidate` 仅清本地，无跨实例广播 | 源码中 `inner.remove(&pid)` 后无 Redis pub 或 NATS pub |
| **性能基准体系** | P2 | 817 个单元测试通过但 `cargo bench` 零 target，CI 无性能回归检测 | `rg 'criterion|iai|divan' Cargo.toml` → 0 |
| **降级/熔断** | P2 | 依赖不可用时错误直扑用户，无降级层级矩阵、无断路器、无用户面降级提示 | `rg 'circuit.breaker|Degradation'` → 0 |

所有 5 个方向均已在附录中交叉验证：在 76 份既有 `docs/requirements/` + 22 份 `docs/analysis/` 中未被系统性深入分析（最多表格一行提及）。
