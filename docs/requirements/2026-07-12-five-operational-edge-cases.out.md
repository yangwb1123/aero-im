已完成对文档 **2026-07-12-five-operational-edge-cases.md** 的全量交叉验证，并将响应写至 `.out.md` 文件。

## 验证结果摘要

| 方向 | 文档声明 | 实测 | 状态 |
|------|---------|------|------|
| 一：WS 优雅排干 | 5 项代码证据 | 行号偏移 ≤2 | ✅ 全部确认 |
| 二：Webhook 并发边界 | 串行消费 + 串行投递 | `for target` + `dispatch_event().await` 确认 | ✅ 全部确认 |
| 三：Blob 缓存 | 仅 3 个响应头，无流式 | ETag/Cache-Control/Range 全部缺失 | ✅ 全部确认 |
| 四：客户端回退 | sendMessage 返回值被忽略 | `app.js:906` + `media.js:81` + `ws.js:174` 确认 | ✅ 全部确认 |
| 五：DB 监控 | INDEX_SIZE_BYTES 存在但缺少 pg_stat_* | `metrics_tasks.rs` 全量确认 | ✅ 已修正 |

**关键补充发现**：
- `findPendingMatch` 按文本相似度匹配而非 `temp_id` → 即使发送成功，审核修改内容也会导致 pending 僵尸（方向四的影响范围需扩展）
- `sample_index_sizes()` 已有但只查 `pg_relation_size`，不涉及死元组/扫描效率（方向五核心缺口确认）
- 方向三（Blob 缓存）的 `ETag` 可直接复用 `meta.sha256` 字段，L1 实施仅 ~15 行 Rust

**优先级修正建议**：方向三（Blob 缓存 L1）和方向四（L0 止损）实施成本极低，建议从 P2 升级为 P1，与方向一/五并行实施。
