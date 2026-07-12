已完整阅读。这份分析的质量很高——每个方向都有明确的源码证据链、边界情况分析和扩展路径。以下是我的反馈：

---

## 总体评价

这份文档的价值在于它精准地定位了五个**跨越多个 crate 的系统性断层**，而不是孤立的 feature gap。每一条证据都能在源码中 grep 验证，这是它区别于 87 份需求分析的关键差异。

## 按方向的具体观察

### 方向一（媒体处理管线）— 最尖锐的观察

EXIF 泄露 + SVG XSS 的边界分析比「需要缩略图」这个结论更重要。当前 `content_sniff.rs` 只做了魔数验证，而**媒体文件的攻击面远不止 SVG**——Polyglot 文件（GIFAR 风格）、PDF 中的 JavaScript、带有嵌入对象的 Office 文档，这些都在当前管线中畅通无阻。

**一个可追加的边界情况**：`Block::Voice` 的原始 PCM/Opus 字节可能包含说话人声纹特征。如果用作 AI 语音分析的训练数据（`transcribe_bot` 的上游），需要区分「转写文本」和「声纹特征」的 GDPR 敏感度差异——前者是内容数据，后者是生物特征数据（GDPR Art.9）。

### 方向二（审核人类兜底）— 预算攻击是最务实的威胁

预算窗口耗尽攻击（60s 内 61 条消息耗光 per-ws 预算 → 第 62 条绕过审核）是**可在生产中验证的 DoS 向量**，不需要复杂的攻击链。当前 `moderation_bot.rs` 的 budget check 路径（`KeyedCostBudget` → `CostBudget`）耗尽后直接 `continue` 跳过——这是纯自动化系统特有的盲区，有人类兜底的审核队列可以在预算耗尽后降级为「延迟审核」而非「跳过审核」。

**与现有代码的汇合点**：`message_reports.rs`（举报）和 `user_reports.rs`（用户举报）已经存在表结构。如果审核队列的路由设计能直接对接这两个已有的举报表，方向二的扩展路径可以复用现有数据模型，而不需要新建审核表——`message_reports` 中 `status` 字段（pending/resolved/dismissed）已经部分覆盖了审核状态机。

### 方向三（消息/线程生命周期）— retention 策略冲突是暗坑

当前 `message/sweep.rs` 的清扫逻辑使用 `COALESCE` 做 retention 优先级（频道级 > 工作区级），但 `legal_hold` 和 `expires_at`（ephemeral）的交互是**空隙**：

- `legal_hold` 是 boolean 跳过，但 `expires_at IS NOT NULL` 的 ephemeral 消息如果同时被 legal hold 覆盖——当前行为是 legal hold 胜出，但用户感知上是「我设了阅后即焚为什么还在」。
- 方向三提出的策略冲突优先级（法务保全 > 标记重要 > expires_at > 频道 retention > 工作区默认）应该写在代码注释里作为不可违反的规范。

**追加观察**：`forward.rs` 和 `broadcast.rs` 的转发链精度丢失（原文已提）在搜索 RAG 场景中会被放大——如果 AI 问答的上下文来自一个已删除消息的转发副本，它会引用一个不存在的消息。

### 方向四（Bot 平台）— 凭证明文存储是最直接的隐患

`bot.config` JSONB 明文存凭证，任何有 DB 只读权限的人可读取所有 bot 的 API keys。考虑到 `aero-server` 的 DB 连接池有完整的表访问权限，一个 SQL 注入漏洞（即使 read-only）就能泄露所有 bot 的凭证。

**与方向五的交叉依赖**：方向四提出用工作区级别密钥加密 bot config。方向五的 `workspaces` 表还未添加 `encryption_key` 字段。如果两个方向独立推进，需要在 `workspaces` 上协商一个共享的密钥派生方案——建议方向五先行添加 `workspaces.encryption_key_salt` 列，方向四在此基础上派生工作区密钥。

### 方向五（多租户数据运维）— 与现有 export 管线的汇合

`migration 0070` 的 export_job 管线已经有了工作区数据导出的基础设施。方向五的阶段一（导出工作区数据为结构化归档）可以直接复用这套机制——不需要重新造轮子，只需要在 export_job 产出物上增加一条验证路径（导出 → 导入新实例 → 校验一致性）。

**一个务实的切入路径**：存储配额（`SELECT SUM(size) FROM blobs WHERE workspace_id = $1`）是方向五中最轻量的项，可以独立于备份/恢复/RPO 等工作提前落地。它在 `blob_upload` 路径上增加一个查询和一个 429 响应，不涉及架构变更。

---

## 从 git status 看到的关联性

注意到当前工作区有大量更改：

```
 M crates/aero-server/src/routes/routes.rs
 M crates/aero-server/src/hub.rs
 M crates/aero-server/src/bin/boot/serve.rs
 M crates/aero-server/src/bin/boot/state_builder.rs
 M crates/aero-ai/src/anthropic.rs
 M crates/aero-ai/src/worker/mod.rs
 M crates/aero-im-core/src/service/messages.rs
 M crates/aero-server/src/approvals.rs
 M crates/aero-server/src/polls.rs
 M crates/aero-server/src/search.rs
 M crates/aero-server/src/rate_limit.rs
 M crates/aero-server/src/invitations.rs
 M ... etc
```

你对 `messages.rs`、`routes.rs`、`hub.rs`、`ai` worker、approvals、polls、search、rate_limit 等模块的修改，涉及了消息发送主路径、AI 异步预算、限流、搜索等核心管线。这些改动与本文分析的 5 个方向有交集——特别是方向一（blob_upload 上传路径）、方向二（审核预算攻击面）、方向三（消息生命周期状态）。

---

## 一句话总结

这份分析最有价值的部分不是它提出的 5 个方向本身，而是**它为每个方向提供了「如果不管这个断层，什么具体场景会出问题」的边界条件**。这比大多数架构文档（只写目标不写失效模式）多了一层验证锚点。

要我做什么？比如：
1. **将这份分析导入 ROADMAP.md**（映射为 ROADMAP 中的 Phase 条目，与当前 git status 的改动对齐）
2. **对某个方向做更深入的代码级 drill-down**（比如方向二的审核预算攻击面的具体 exploit 路径，或方向四的凭证加密方案）
3. **起草与当前 git 改动对应的实现 commit**（基于你看完分析后想落地的方向）
4. **对方向间的依赖关系做交叉验证**（比如方向三的 retention 冲突优先级是否需要改动 `message/sweep.rs`）
