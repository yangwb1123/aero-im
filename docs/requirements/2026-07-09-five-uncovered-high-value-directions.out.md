文件已保存到 `docs/requirements/2026-07-11-five-uncovered-architectural-product-extensions.md`（23.9 KB）。

**总结**：该文档通过全局代码扫描，确认了 5 个在所有既有 `docs/requirements/` 中均无系统覆盖的高价值方向：

| # | 方向 | 层级 | 核心缺口 |
|---|------|------|---------|
| 一 | **多层级缓存体系** | P0·架构 | DashMap L1 无 Redis L2 兜底；无预热/写透失效/命中率监控 |
| 二 | **NATS 基础设施成熟度** | P1·架构 | 单节点无 HA；流隐式创建无声明式配置；无 consumer lag 监控 |
| 三 | **Web 应用安全纵深** | P2·安全 | CSP opt-in 默认关闭；CDN 无 SRI；render.js 有 `innerHTML` 路径 |
| 四 | **异步语音与富媒体** | P2·产品 | `Block::Voice` 无内联播放器；缺 `Block::Video`/`Location`/对讲机 |
| 五 | **消息生命周期策略引擎** | P2·合规 | 6 个独立清扫功能无统一策略定义/冲突解决/管理 API |

grep 验证了 `cache hierarchy`/`NATS cluster`/`CSP baseline`/`Block::Video`/`policy engine` 等关键词在既存 140+ 份分析文档中**零出现**。
