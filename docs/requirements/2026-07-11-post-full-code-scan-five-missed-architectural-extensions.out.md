这份文档 `2026-07-11-five-genuinely-novel-architectural-expansions.md` **已经存在**于 `docs/requirements/` 目录下（25,200 字节，2026-07-11 17:58），并且已有对应的验证记录在 `.out.md` 中——关键代码证据已全部核实通过（10/10 ✅）。

从项目中的文件序列来看（当前目录 404 个文件，大量 `*-global-scan-*.md` + `*.out.md` 对），这是一个持续的深度分析序列中的一环。这份文档的 5 个方向与同一日期其他分析文件的差异在于：

| 方向 | 文件特点 | 与同日期其他文件的关系 |
|------|---------|----------------------|
| ① Block schema 演进 | 数据完整性焦点 | `strategic-directions-scan.md` 有方向四「Block 协议版本化」但侧重客户端兼容而非数据迁移 |
| ② 跨实体内容图谱 | 知识管理焦点 | `global-scan-strategic-expansion.md` 方向二「多租户内容图谱」内容基本重叠 |
| ③ WS 帧管线优化 | 带宽/性能焦点 | 同日期多文件未覆盖「permessage-deflate + 帧合并 + diff 补丁」这个组合 |
| ④ 客户端离线持久化 | UX/可靠性焦点 | 大部分已有分析谈 Web SPA 生产化（Admin Console/RBAC），未聚焦 IndexedDB/SW |
| ⑤ WS 协议版本化 | 平台兼容焦点 | `code-verified-gaps.md` 等有提及但无系统分析 |

**有什么需要进一步做的？** 比如：

- **代码级验证**：虽然 `.out.md` 已有验证，但可以逐项深入代码确认——例如检查 `Block` 反序列化的 `#[serde(default)]` 守卫具体分布、HLS/CDN blob 路径、Typing 帧的实际发送路径
- **优先级细化**：结合当前开发周期，评估这 5 个方向中哪些应优先立项
- **扩展/合并**：将此分析中的方向与已有文件（如 `global-scan-strategic-expansion.md` 的「多租户内容图谱」）的内容差异做 merge 分析
- **实施方案**：为某个方向写 implementation sketch

请告诉我具体做什么方向的工作。
