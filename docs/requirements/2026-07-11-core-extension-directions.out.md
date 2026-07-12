已将分析保存至 `docs/requirements/2026-07-12-core-expansion-direction-analysis.md`。

这是一份扎实的架构级分析。几个观察：

**方向三（重连 TOCTOU）** 的 TOCTOU 窗口分析特别精准——`backfill_since` 和 live Hub 扇出之间存在真实的竞态窗口，`delivery_cursors` 基础设施已就位但 WS 协议层未接入，确实是"基础设施完备但最后一百米未通"的典型。建议跟进时同步检查 `backfill_room_ids` 在 `delivery_cursor` 接入后的去重逻辑——如果游标回填和 live 扇出都走通，双路径可能重复投递，需要 `SeqGate` 在客户端做最终去重。

**方向四（通知风暴）** 与 AGENTS.md §4.1 的限流约束有直接映射关系——`broadcast` API 已有 `targets.len() <= 100`，但 `@everyone` 绕过了这条路径。一个务实的首期防护是复用已有的 `rate_limiter` 基础设施（`AERO_RATE_LIMIT_PER_SEC`），在 `dispatch_notifications` 入口加 per-room `@everyone` 冷却期（如 60s），与迁移 0030 的 `post_policy` 配合使用。

**方向五（弹幕时序）** 的 PCR 回绕问题（~26.5h）是真实生产隐患——如果直播持续超 24h，`media_timestamp` 的存储表示需要设计为 `(base_wall_clock, rtp_timestamp)` 元组而非单一单调计数器，否则回绕后弹幕定位会错位到视频开头。
