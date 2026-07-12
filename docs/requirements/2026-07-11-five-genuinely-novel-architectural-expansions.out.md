文件已保存至 `docs/requirements/2026-07-11-post-full-code-scan-five-missed-architectural-extensions.md`（384 行）。

验证了文档中的关键代码证据：

| 证据 | 状态 |
|------|------|
| `Block` 枚举在 `block.rs:66`，10 variants，无版本号 | ✅ |
| `Message` 结构体无 `blocks_version` | ✅ |
| `search_query.rs` 搜索范围仅限 `FROM messages` | ✅（10 处） |
| `define_id!` × 53 次调用在 `ids.rs` | ✅ |
| `fan_out_raw` 发送完整 JSON 文本无压缩 | ✅ |
| 客户端无 `IndexedDB`/`ServiceWorker` 使用 | ✅ |
| `state` 为全局可变内存对象（`context.js`） | ✅ |
| `ClientFrame`/`ServerFrame` 无版本/能力字段 | ✅（位于 `ws/ws_impl/mod.rs:68/206`） |
| `optimisticAdd` 仅操作内存 DOM（`app.js:937`） | ✅ |
