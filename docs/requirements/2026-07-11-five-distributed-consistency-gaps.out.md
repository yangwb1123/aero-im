已完成为 `2026-07-11-five-distributed-consistency-gaps.md` 的全量代码级交叉验证，结果写入 `docs/requirements/2026-07-11-five-distributed-consistency-gaps.out.md`。

### 验证结论摘要

| 方向 | 评级 | 关键修正 |
|------|------|---------|
| **方向一** P0 | ⚠️ 85% | `handleEdited` 实际 **append** 到 DOM（而非静默跳过），比文档描述的更严重；遗漏了客户端 `SeqGate` 去重机制（ws.js:18-39） |
| **方向二** P1 | ✅ 95% | 精确；追加发现：纯媒体消息编辑 caption 不触发 re-embed（`searchable_text` 为空时跳过） |
| **方向三** P1 | ⚠️ 80% | `leave_group_call` 的顺序是 **SFU→Redis**（而非文档说的 Redis→SFU），实际风险是「Redis 僵尸路由」而非「SFU 僵尸 peer」 |
| **方向四** P2 | ✅ 98% | 精确；**建议升级到 P1**——cache 注释声称"下游 auth 过滤"但 `fan_out_raw` 路径无下游 auth gate，被踢成员 60s 过扇出是真实安全漏洞 |
| **方向五** P2 | ✅ 95% | 精确；`collapse_key` 已部分缓解但不足够 |

### 最重要的修正

1. **`handleEdited` 非静默跳过**（方向一、app.js:800-804）：原始消息未到时编辑帧会 `appendChild` 到 DOM，导致「幽灵消息」——原消息到达后有两条重复。增加 `idx >= 0` 守卫即可修复。

2. **`room_member_cache` 注释矛盾**（方向四）：注释说`下游 auth gate 过滤`，但 `hub.fan_out_raw` 到 WebSocket 广播没有任何权限重校验。这个 60s 过扇出窗口是真实安全越权——被踢成员继续收到消息。建议从 P2 提升至 P1。
