文档已保存至 `docs/requirements/2026-07-12-five-operational-edge-cases.md`。

---

## 实体验证摘要

对五个方向的代码级声明做了逐项 grep + read 验证，结果如下：

### ✅ 完全确认（方向一~四）

| 方向 | 关键确认点 |
|------|-----------|
| **WS 排干** | `mod.rs:301` `close = CancellationToken::new()` 是局部 token，非从全局派生；`shutdown.rs` 只停了 AI 任务，未碰 WS |
| **Webhook 串行** | `dispatch_event` 中 `for target { sender.deliver(&delivery).await }` 是串行阻塞的；`run_webhook_dispatcher` 也是事件级串行 |
| **Blob 无缓存** | `blob_download` 响应只有 `Content-Type` / `Content-Disposition` / `X-Content-Type-Options: nosniff`，**无** ETag / Cache-Control / Range / 内容协商 |
| **客户端无重试** | `ws.sendMessage()` 返回 `boolean`，但 `app.js:906`、`media.js:82,144` 等调用方均不检查返回值。WS 断开时消息静默丢 |

### ⚠️ 方向五需修正一处

初版称 `index.bloat=0`，但实际上 **`metrics_tasks.rs` 已有 `INDEX_SIZE_BYTES` gauge**，通过 `sample_index_sizes()` 采样 `messages` 表热索引的 `pg_relation_size`。这提供了有限索引膨胀视角（索引从 4KB→4GB 可见），但**不覆盖**死元组比率、seq_scan、长事务等关键 PG 内部指标。文档已据此修正正文。
