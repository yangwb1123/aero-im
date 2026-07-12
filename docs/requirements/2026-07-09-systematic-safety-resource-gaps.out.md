这份文档已经完整存在于仓库中——`docs/requirements/2026-07-09-systematic-safety-resource-gaps.md`（30,940 bytes，md5 完全一致），是 7 月 9 日的分析。

此外，在对代码库做交叉验证时，发现文档中有少量不精确之处需要留意：

### 需要校正的细节

| 项目 | 文档声称 | 实际验证 | 影响 |
|------|---------|---------|------|
| `panic!()` 数量 | ≈ 20+ | **55**（含 test 文件） | 低估了 2.75x，但定性论据（存在 panic 风险）依然成立 |
| 「5 个方向在所有既有文档中均未被论证」 | **零命中** | 所有 5 个方向在其他 `.out.md` 文件中**有提及**（如 `architect-product-five-underexplored-directions.out.md` 讨论了配额、空闲超时、连接上限；`five-genuinely-uncovered-high-value-directions.out.md` 讨论了草稿） | 不颠覆结论，但「未被论证」的描述不完全准确——更准确的表述是：「此前未被作为独立专题深度分析」 |

### 其余已验证的定量数据

| 检查项 | 文档数据 | 实际验证 | 状态 |
|-------|---------|---------|------|
| `unwrap()` 总数 | 1951 | **1951** | ✅ 精确匹配 |
| `expect()` 总数 | 924 | **924** | ✅ 精确匹配 |
| 含 `unwrap()` 文件数 | 175 | **175** | ✅ 精确匹配 |
| 存储配额相关代码 | 零代码 | **零命中**（`rg -i quota\|storage_limit\|max_blobs` 空） | ✅ |
| WS idle timeout | 零代码 | **零命中**（`run_socket` `select!` 仅 `close.cancelled()` + `receiver.next()`） | ✅ |
| 草稿持久化 | 零代码 | **零命中**（`rg draft\|save_draft\|auto_save` 在 web/server 空） | ✅ |
| 每用户连接上限 | 零代码 | **零命中**（`Hub::register` 无检查，直接 `push`） | ✅ |
| `max_body_bytes` 33 MiB | ✅ | `config.rs` 确认 | ✅ |
| `MAX_BLOB_BYTES` 32 MiB | ✅ | `routes.rs` 确认 | ✅ |

该分析文档已在仓库中，无需重复保存。如需对此分析做下一步操作（如实现提案、精修数据或对齐 AGENTS.md），请告知。
