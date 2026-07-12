以下是**代码实体验证结果**：

| 方向 | 文档核心断言 | 验证结果 |
|------|------------|---------|
| ① App Platform | Block Kit 已建但无 App 注册概念 | ✅ `block_interaction.rs` 仅有 `"approve"` 等内置 action_id；`app_registry` 表/模块不存在 |
| ② Product Analytics | analytics + usage_report API 有但 Web UI 为 0 | ✅ routes.rs:306/428 有路由挂载，但 web SPA 搜索 `analytics`/`usage.report` 零匹配 |
| ③ Canvas Client | 服务端 3 个文件完整，Web SPA 零引用 | ✅ `canvas.rs` 13KB, `canvas_op.rs` 9.5KB, `storage/canvas.rs` 12.9KB；web/ 中 `canvas` 仅出现在 `node_modules/globals` 中 |
| ④ Message Lifecycle Automation | 无 `rule\|trigger\|automation\|workflow.*config\|if.*then` | ✅ 实际代码只有 OOO auto-responder；无规则引擎模型 |
| ⑤ Cross-Workspace Federation | `assert_room_access` 隐含同 workspace 假设 | ✅ 代码确认：同时校验 workspace 成员 + room 成员，跨 workspace 的 participant 不可能通过 |

所有代码断言**与当前 master 代码一致**。这份分析的证据链是可靠的。

---

我现在可以为你做什么？几个方向：

**A. 选择一个方向，我出详细技术实施方案**（增量迁移、仓储、路由、Web UI 组件树、边界条件处理）——以符合 `AGENTS.md` §4.1 的「加功能配方」。

**B. 将这份分析保存为 `docs/requirements/` 中的正式分析文档**（与既有的 225+ 篇分析并存）。

**C. 对某个方向的边界条件/工程细节做深入钻取**（例如 Canvas CRDT 的 Op 类型设计与服务端 `canvas_ops` 表的对位验证、App Platform 的 `app_manifest` JSON schema 定义、Workflow 规则引擎的并发执行模型）。

**D. 从某个方向开始实现**——根据你的优先级矩阵，我推荐先做 **方向② Product Analytics**（工程量最小、对现有业务零侵入、复用 analytics repo + REST API + Chart.js），或 **方向① App Platform 的 App 注册表部分**（独立增量，可并行推进）。

请告诉我你的意图。
