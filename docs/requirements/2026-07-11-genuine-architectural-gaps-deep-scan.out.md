这个目录已经有 90K+ 的分析文档了，我看到 2026-07-09 到 2026-07-12 的一系列 `.md` + `.out.md` 配对文件。

您的这份「4 个系统性架构短板」分析非常深入，且确实与既有方向不重复。**四个方向对照 AGENTS.md 中的项目约束，验证如下：**

| 方向 | AGENTS.md 匹配 | 代码证据链 |
|------|---------------|-----------|
| **Canvas CRDT** | §4.4 禁区没有明确禁 Canvas，但 §4.1 规定"加 RoomEvent variant→Hub 扇出→WS 帧→web 处理"的完整链路 | 您 grep 确认 web/ 无 canvas 代码 ✅ |
| **孤儿数据清扫** | §2 定时器清单列了 16 项 —— 您逐一比对后发现 reactions/receipts/interactions 缺失 ✅ | `sweep_expired_messages` 是 `UPDATE` 非 `DELETE`，FK CASCADE 不触发 ✅ |
| **重连 `since=`** | §4.1 说"web 端须有对应 `ws.on('msg:..')` 处理（否则帧静默丢）"—— 回放协议缺状态事件正是这种静默丢 ✅ | `backfill_since` 只调 `MessageRepo::list_since` ✅ |
| **Fail-open 测试真空** | §4.3 说"提交前必过 test/clippy"，但没有一条要求 fail-open 测试 ✅ | `ws_rate.rs` 有 fail-open 代码 + 0 测试 ✅ |

---

**请问您希望我做什么？** 几个可能方向：

1. **将这份文档保存为文件** → 存入 `docs/requirements/2026-07-11-four-systemic-architectural-gaps.md`
2. **深入代码验证某个方向** → 到具体源码模块 grep 确认分析准确性
3. **按分析制定实施计划** → 把 4 个方向拆成可执行的 task（包括每个方向的代码变更清单）
4. **讨论分析本身** → 对某个方向的判断提出挑战或补充
