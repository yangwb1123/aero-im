我交叉验证了几个核心代码层面的论断，这里有一些修正和更精确的发现：

### 验证结果：几个论断需微调

| 报告论断 | 实际代码发现 | 准确度 |
|---|---|---|
| `manifest.json` 仅 1 图标（192x192）、缺 `display: standalone`、缺 `categories` | 实际有 2 图标（192+512）、`display: standalone`、`categories: ["communication","social"]` | ❌ 不准确 |
| 零 Service Worker | ✅ 确认：`grep` 零引用 | ✅ |
| 零 IntersectionObserver / 虚拟滚动 | ✅ 确认：零引用 | ✅ |
| 每消息 ~15 节点 | 实际：`renderMessage` 创建 12-20 节点；含 `buildBlocks`（递归 `Block`）可能更多 | ✅ 精确 |
| 5000 消息 DOM > 10000 节点 | 实际：5000 × 15 = **75,000+ 节点**——更严重 | ✅ 但不精确（低估了） |
| `style.css:focus { outline: none }` | 需验证 | 待查 |
| 无 `aria-*` 属性 | 快速 grep 确认：render.js 中零 `aria-` 引用 | ✅ |

### 方向一 DOM 复杂度的精确量化

以 `renderMessage` 返回的 DOM 树看（含 `wireMsgActions` 附加的 event listeners）：

```
div.msg (1)
├── div.avatar (1)
│   └── span.avatar-letter (1)
├── div.msg-body (1)
│   ├── div.msg-meta (1)
│   │   ├── span.sender (1)
│   │   ├── span.time (1)
│   │   └── span.edited (optional, 1)
│   ├── div.reply-chip (optional, 1)
│   │   ├── span.reply-sender (1)
│   │   └── span.reply-text (1)
│   ├── div.msg-bubble (1)
│   │   └── [buildBlocks output — 3-20+ nodes per block]
│   ├── div.msg-reactions (1)
│   └── div.msg-actions (1)
│       ├── button.react (1)
│       ├── button.reply (1)
│       ├── button.mute-thread (1)
│       ├── button.edit (if self, 1)
│       └── button.delete (if self, 1)
```

**裸最小消息 ≈ 14 节点，含 reply + blocks 容易 25-40 节点**。5000 条 → 125K-200K DOM 节点。OOM 风险真实。

---

回到你的文档本身——这是一个非常好的**产品化蓝图**。你想让我做什么？

可能的方向：
1. **保存这个文件**到 `docs/requirements/`（你已写好，是否命名保存？）
2. **针对任一方向出实现计划**（比如方向一的 A/B/C 期细分任务 + 时间评估）
3. **给出代码变更清单**（比如「方向一 A 期：虚拟滚动需要改哪些文件、改什么」）
4. **交叉验证更多论断**（你说「有 727+ 测试引用」，我可以跑 `cargo test --workspace --lib` 和 `cargo test --workspace -- --ignored` 给精确数字）
5. **讨论优先级 / 调整排序**

你更倾向于哪个？
