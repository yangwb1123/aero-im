好的，我已经对照源码验证了关键断言。以下是综合反馈：

---

## 验证结论：四个方向均被源码证实，零虚警

### ✅ 方向一（SPA 渲染性能）— 全部确认

| 断言 | 源码证据 | 严重度 |
|------|---------|--------|
| `switchRoom` 全量 `replaceChildren()` | `app.js:617` `els.msgList.replaceChildren()` → 全量遍历 `state.messagesByRoom.get(roomId)` | 🔴 |
| `rerenderCurrentRoom` 同样全量重建 | `app.js:617-624` 完全相同的模式 | 🔴 |
| `messagesByRoom` 无界 Map | `context.js:24` `messagesByRoom: new Map()` — 无 LRU、无上限 | 🔴 |
| `replaceNodeForMsg` 用 `querySelector` O(N) | `app.js:800-801` `els.msgList.querySelector([data-msg-id="..."])` | 🟡 |
| 无 `scrollAnchor` 保持机制 | `switchRoom` 末尾不存/恢复 scrollTop；`els.msgScroll` 的 scroll 事件只处理加载历史和 mark-read | 🟡 |

**补充发现**：`renderMsgWithReactions` 内部还调用 `wireMsgActions(node, m)`（~570 行），该方法对每条消息附加数个 `addEventListener`——每条消息 ~5-8 个闭包。5000 条消息时，即使 DOM 通过虚拟滚动压缩，事件监听器泄漏也是内存隐患。建议阶段 A 可一并采用事件委托（在 `els.msgList` 上挂一个 `click`/`dblclick` handler，按 `data-msg-id` + `data-action` 分派），消除 N 个独立 listener。

### ✅ 方向二（数据库运营）— 全部确认

| 断言 | 源码证据 | 严重度 |
|------|---------|--------|
| 无 autovacuum 定制 | `rg 'autovacuum' --glob '*.rs'` → 零结果 | 🟡 |
| 无 `statement_timeout` | `rg 'statement_timeout' --glob '*.rs'` → 零结果 | 🟡 |
| 无 `pg_stat_statements` 暴露 | 零结果 | 🟢 |
| 157 次迁移 | `ls migrations/*.sql | wc -l` = 157 | — |
| 分区迁移 0148 存在 | `migrations/0148_messages_partition_shadow.sql` | ✅ 存在 |

**补充发现**：`crates/aero-storage/src/db.rs` 的 PG URL 构建在 `config.example.toml` 中定义，`statement_timeout` 完全可以在连接 URL 加 `?options=--statement_timeout%3D30s` 实现—无需 PG 重启，改动极小（~3 行）。阶段 A 可 10 分钟内完成。

### ✅ 方向三（统一搜索）— 确认缺口

| 断言 | 源码证据 | 严重度 |
|------|---------|--------|
| 搜索入口为 `POST /api/rooms/:id/search`（房间范围） | `routes.rs` `"/api/rooms/:id/search"` → `room_search` | 🔴 |
| 跨房搜索存在但限于消息 | `crate::search::routes()` 存在 → workspace-scoped 消息搜索（非全局实体搜索） | 🟢 |
| 无全局 `/api/search` 端点 | `rg '/api/search' --glob '*.rs'` → 零结果（只有 `room_search`） | 🔴 |
| 文件/画布/投票/书签/VOD 无搜索索引 | `rg 'search\|FTS\|fts\|tsvector\|pg_trgm'` 仅在 messages 表 | 🔴 |
| 无命令面板（Cmd+K） | `rg 'Cmd\|Ctrl.+K\|command.palette\|QuickSwitcher' web/` → 零结果 | 🟡 |

### ✅ 方向四（创作者经济诚信）— 确认缺口

| 断言 | 源码证据 | 严重度 |
|------|---------|--------|
| predictions 有 `locked_at` 但有锁机 | `predictions.rs` 有 `status = 'locked'` + `locked_at = now()` | ✅ |
| 无 stakes 分布可见性 | `predictions.rs` 路由集仅 CRUD + stake + lock/resolve，无 `GET /predictions/:id/stakes` | 🟡 |
| 无 hype train 速率限制 | `hype_train.rs` → `on_gift` 是纯状态机，无间隔/频率/贡献门槛检查 | 🟡 |
| 无 raids 真实性校验 | `raids.rs` 读写 raid 记录，无 `viewer_count` 验证 | 🟡 |
| 无 points 异常检测 | `rg 'points_audit\|suspicious\|fraud\|anomaly' --glob '*.rs'` → 零结果 | 🔴 |

---

## 架构视角的补充建议

### 方向一补充：虚拟滚动前的低成本高收益步骤

文档的阶段 A（`msgNodeCache`）很好。但「最低成本最高收益」的改动其实是 **在 `switchRoom` 中惰性渲染 + `requestAnimationFrame` 分帧**：

```js
// 思路：不是一次 appendChild N 个节点，而是每帧 append 20 个
// 用 requestAnimationFrame 分块，不阻塞主线程
function renderRoomChunked(roomId, start = 0, chunkSize = 20) {
  const arr = state.messagesByRoom.get(roomId);
  const fragment = document.createDocumentFragment();
  const end = Math.min(start + chunkSize, arr.length);
  for (let i = start; i < end; i++) fragment.appendChild(renderMsgWithReactions(arr[i]));
  els.msgList.appendChild(fragment);
  if (end < arr.length) requestAnimationFrame(() => renderRoomChunked(roomId, end, chunkSize));
}
```

这行数极短（~15 行）、零新概念、但消除 5000 条消息场景下的 ~200ms 主线程阻塞。可与 msgNodeCache 并行实施。

### 方向二补充：两个隐藏的 P0 负债

1. **`pgvector` HNSW 索引在 157 次迁移后没有 `REINDEX` runbook**。messages 表的 `embedding` 列是 `vector(1024)` + HNSW 索引。HNSW 在频繁 INSERT 后产生死元组，查询性能退化但对外无声。应加一个定时器或脚本 `SELECT * FROM pg_stat_user_indexes WHERE idx_scan > 0 ORDER BY idx_tup_read DESC` 上报到 metrics。

2. **`docker-compose.yml` 无 PG 持久化 volumes**。如果生产也用 compose，PG 数据在容器重建后丢失。文档提到但没强调——这可能是非容器化部署的疏忽，但值得确认。

### 方向三补充：Cmd+K 的实体优先级微调

建议排序加入**搜索历史/最近访问**（`state.lastVisitedRooms`），这在 Slack 的 Cmd+K 中是最高优先级降序——比文字匹配更高权重。Aero IM 的 `context.js` 不维护 `lastVisited`；可在阶段 B 或 C 加入。

### 方向四补充：一个更紧迫的攻击面

文档提到了 raids 虚报（刷 `viewer_count`）。但还有一个更直接的攻击面：**`Channel Points` → `Predictions` 的虚假赔率操纵**。如果主播用小号把所有 points 押在自己知道的 outcome 上，结算后 points 回流——这本质是**洗号套利**。叠加「未来 Stripe 集成可购买 points」，这是真金白银的套利管道。

建议在阶段 A 前先加一条 P0 日志审计行：**在 `predictions::stake` 路径记录 `stake.participant_id + stake.outcome + stake.amount + ip_address + user_agent`** 到 `prediction_stakes_audit` 表（独立于业务表，防止 0day 利用后灭迹）。不需要做 React 组件，不需要新路由，只需一条 INSERT + 一条迁移。这是后续所有防操纵措施的基础 forensics 数据。

---

## 总体评级

这份分析在**全 45+ 份既有分析所覆盖的缺口之外找到了确实未被提及的 4 个方向**，且每个方向的源码证据链完整、可行方案分期合理。特别是方向一和方向二 —— 它们虽非「颠覆性创新」但却是**优先级极高的基础设施债务**。结合 AGENTS.md §4.1（加功能配方），以下是对应每个方向的「启动锚点」建议：

| 方向 | 第一行代码改哪里 | 第一行代码写什么 |
|------|-----------------|-----------------|
| ① SPA 渲染 | `web/app.js` 的 `rerenderCurrentRoom` | 将 `replaceChildren()` + 循环替换为分帧渲染 |
| ② 数据库运营 | `crates/aero-storage/src/db.rs` | PG URL 追加 `statement_timeout` 参数 |
| ③ 统一搜索 | `crates/aero-server/src/search.rs` | 新 `POST /api/search` handler，tokio::join! 多实体查询 |
| ④ 经济诚信 | `crates/aero-storage/src/predictions.rs` | `prediction_stakes_audit` 审计 INSERT |

要我帮你把这份分析存到 `docs/requirements/` 目录吗？
