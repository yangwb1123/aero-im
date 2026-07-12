已保存至 `docs/requirements/2026-07-12-five-zero-coverage-systemic-gaps.md`（~18KB）。

**总结 5 个方向与零覆盖验证：**

| # | 方向 | 既有覆盖 | 优先级 | 核心证据链 |
|---|------|---------|--------|-----------|
| 1 | **跨房间因果一致性** | **0** hits (causal/happens-before) | P1 | `bus/seq.rs` per-subject seq 不跨 subject；`forward.rs` 跨房间发布不携带前因；WS 客户端 ULID 排序依赖时钟 |
| 2 | **AI budget defer 活锁** | **0** hits (livelock/defer loop) | P1 | `worker/mod.rs` defer 不消耗 retry、不递增计数器；持续过载下 job 无限期 claim→defer→claim |
| 3 | **级联静默降级面** | **0** hits (silent degradation) | P1 | 6+ fail-open 路径（RateLimit/AI/Push/Moderation/IP/Login）；Redis 故障可触发 PG 级联过载+全组件降级，无聚合告警 |
| 4 | **不可逆迁移/零停机** | **≤1** (down migration/rollback) | P2 | 157 个迁移全部 up-only；`sqlx::migrate!` 编译期嵌入；消息分区标记为停机窗口操作 |
| 5 | **数据完整性巡视器** | **≤1** (scrubber=0, checksum=5 全在安全语境) | P2 | `blob.rs` 无 checksum 列；`BlobStore` trait 无 verify 方法；orphan reaction/notification 无级联清扫 |

每个方向均附完整代码锚点 (文件路径+行级引用)、级联影响场景、修复建议及边界情况。三个 P1 方向的商业影响门槛较低（消息可靠性 + AI 合规 + 故障 MTTR），值得优先跟进。
