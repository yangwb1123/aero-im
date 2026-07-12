两份文件均已保存：

**主文档**：`docs/requirements/2026-07-12-post-full-scan-five-uncovered-high-value-extensions.md`（23.8KB）
**反馈审稿**：`docs/requirements/2026-07-12-post-full-scan-five-uncovered-high-value-extensions.out.md`（11.3KB）

---

## 反馈要点摘要

代码核实验证了你的 5 条方向均确实未在现有文档中覆盖。以下是关键发现问题：

### 🔴 需要修正的 1 个文件引用

- 方向二：你写 `aero-bus/src/nats.rs`，实际文件名是 `jetstream.rs`

### 🟡 低估已有基础设施（2 处）

1. **`bootstrap()` 已存在**：你说「流在第一次 publish 时隐式创建」——不准确。`jetstream.rs` 已有显式 `get_or_create_stream` 调用创建 4 条 JetStream，但确实是 `..Default::default()`（replicas=1，max_msg_size 未显式设）。
2. **`participant_cache.invalidate()` 已实现**：你说「无写透失效」——participant 写路径已有 cache invalidate，但仅覆盖 participant 不覆盖 room 数据。

### 🟢 工作量修正

- **方向二**（NATS 集群）：你说 ~2w，我估算 **~6d**，因为现有 `bootstrap()` + `consumer_pending()` 方法已就绪
- **方向一** Phase 1（room metadata L1 缓存）：你说 3-4w，我估算 Phase 1 仅需 **~1w**（模板 `participant_cache` 已存在，可复用 pattern）
- **方向四**（语音波形）：你低估了波形采集的工作量——它是纯**前端**工作（`AnalyserNode` + Canvas），无服务端捷径

### 🔵 优先级调整建议

将方向二（NATS 基础设施）从 P1 **提至 P0**——PG 和 Redis 各有 HA 配置，NATS 单节点是整个事件主干的单点故障，是当前最紧迫的基础设施风险。
