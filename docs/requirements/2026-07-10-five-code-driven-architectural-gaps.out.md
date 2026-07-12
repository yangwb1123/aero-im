分析已保存。以下是文档定位摘要：

**文件**: `docs/requirements/2026-07-11-five-source-driven-architectural-gaps.md`（3,448 字）

该文档在 70+ 份既有分析的饱和覆盖面上找到 5 个正交缺口，每个均以源码行级证据锚定：

| # | 方向 | 优先级 | 核心发现 |
|---|------|--------|---------|
| 1 | Webhook 事件面裁减 | **P1** | `webhooks.rs:644` 硬编码只放行 `RoomEvent::Message`，其余 14+ variant 被静默丢弃，`outgoing_webhooks.events` 过滤条件形同虚设 |
| 2 | 事件因果序缺失 | **P1** | 跨类型事件（Edit/Reaction/Delete）无时序保证；SPA 侧 `handleEdited` 中 `idx < 0` 静默丢弃编辑、reaction 在 delete 后仍渲染——防御代码证实开发者已知但未系统性解决 |
| 3 | Bot 轮询 1s 延迟 | **P2** | 10 个长循环全部使用 `sleep(1s) + try_recv` 模式，引入 1s 延迟地板；应改用 `tokio::select! { biased; cmd = rx.recv() => ... }` |
| 4 | 乐观发送无恢复 | **P1** | `submitComposer` → `optimisticAdd` → `ws.send` 不检查返回值——连接断开时用户看到永久幻影消息；无重发队列 |
| 5 | 错误码目录缺失 | **P2** | `Error.code()` 返回 ad-hoc `&'static str`，非枚举；无 `retryable` 标志、`details` 字段、`request_id` 内联；WS 错误帧无标准结构 |

与既有 70+ 分析不重叠的关键区别：方向一来自**阅读 webhook 调度逻辑源码而非功能文档**；方向二来自**追踪 SPA 侧乱序防御代码而非 seq 机制**；方向四来自**追踪发送路径中返回值从不被检查的事实**。
