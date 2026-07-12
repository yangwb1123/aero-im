文档已保存 + 代码验证完成。下面是综合反馈：

---

## 结果摘要

### 保存
`docs/requirements/2026-07-11-round-four-architecture-scan-three-high-value-directions.md`
`docs/requirements/2026-07-11-round-four-architecture-scan-three-high-value-directions.out.md`

### 验证评分：95% 准确

全部方向确认**有效且未被覆盖**。3 处小修正：

| 位置 | 原断言 | 修正 |
|------|--------|------|
| **方向一**·增量 DOM | ❌ 完全不存在 | ⚠️ `app.js:800` 已有 `replaceNodeForMsg`（编辑）+ `refreshReactionsFor`（反应）做原地更新——但**主线路径仍全量重建**，不影响方向有效性 |
| **方向三**·Message 帧带 reactions | ❌ Message 帧携带完整 reactions | ⚠️ `Message` 结构体**无 `reactions` 字段**，反应通过独立 `ServerFrame::Reaction` variant 传输。不破坏核心论点（"每帧独立 JSON 无压缩"仍成立） |
| **方向三**·`fan_out_raw` 无优化 | ❌ 逐个接收者 | ⚠️ `hub.rs:320-325` 已有 `fan_out_arc`（Arc 共享 payload）+ 大房间并行化。阶段 A 方案可在此基础上增量迭代 |

### 值得注意的补充发现

**方向二**的后端管线比分析描述的更深——`POST /api/rooms/:id/tasks` 已经接受 `source_message_id` 字段（`tasks.rs:105`），且 `forward.rs:130` 广播时也设置了该字段。缺口完全在前端（消息 context menu 缺"创建任务"入口），**阶段 A 的工程代价可能比预估更低**。
