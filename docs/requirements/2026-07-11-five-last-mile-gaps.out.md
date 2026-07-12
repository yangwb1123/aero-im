所有 5 个方向的代码验证已完成。以下是我的逐项评估：

---

# 代码验证报告：5 个「最后一公里」生产级缺口

> 已检查：`drafts.rs` `/` `draft.rs` `/` `frame.rs` `/` `reads.rs` `/` `persistence.rs` `/` `search.rs` `/` `analytics.rs` `/` `ai_usage.rs` `/` `routes.rs`(blob_download/blob_upload) `/` `push_bot.rs` `/` `messages.rs(crud/edit)` `/` `worker/mod.rs` `/` `query.rs(list_without_embedding)` `/` `notification_bundle.rs` `/` `background.rs`(ticker) `/` 迁移脚本 `0128_fts_english_stemmer.sql` `/` `0157_message_version.sql`

---

## ✅ 方向一（Draft 冲突与 Composer 状态收敛）— **全部验证通过**

| 子项 | 代码证据 | 验证结果 |
|------|---------|---------|
| Draft 无版本列 | `draft.rs:43` — `ON CONFLICT (participant_id, room_id) DO UPDATE`，无 `version` 列，无乐观锁 | ✅ 确认 |
| 跨设备覆盖即丢失 | upsert 逻辑无条件覆盖，无 409 冲突返回 | ✅ 确认 |
| Typing 无超时/无限流 | `reads.rs:43-54` — 仅检查 membership → `publish_room_event`，无任何 rate limit / timeout / per-participant bucket | ✅ 确认 |
| Typing 在大频道中扇出放大 | 每个 `on:true`/`on:false` 帧广播到房间所有在线成员，无聚合退化为 "N 人正在输入" | ✅ 确认 |
| 发送与草稿竞态 | 无 `draft_version` 字段关联到 `SendMessage` 帧 | ✅ 确认 |

---

## ✅ 方向二（附件平台）— **全部验证通过**

| 子项 | 代码证据 | 验证结果 |
|------|---------|---------|
| 无配额检查 | `routes.rs:1623-1720` — upload handler 只检查 `MAX_BLOB_BYTES=10MB`(单文件)，无 `SUM(size) WHERE owner_id` 检查 | ✅ 确认 |
| 无缩略图/预览管线 | 全局 grep `thumbnail`/`resize` 零命中 | ✅ 确认 |
| 无视频/音频转码 | 无 VOD pipeline 复用 `aero-live-hls` 的切片管线 | ✅ 确认 |
| 无 Cache-Control/ETag | `blob_download`(routes.rs:1745+) — 返回头中只有 `Content-Type` + `Content-Disposition`，无 `Cache-Control`/`ETag`/`Last-Modified` | ✅ 确认 |
| 无 presigned URL | S3 `BlobStore` 无 `get_presigned_url()` 方法，`blob_download` 透传代理字节 | ✅ 确认 |
| 无过期分享链接 | 全局 grep `share_links`/`/s/:token` 零命中 | ✅ 确认 |

---

## ✅ 方向三（Read-Your-Writes 一致性）— **全部验证通过**

| 子项 | 代码证据 | 验证结果 |
|------|---------|---------|
| pg_read 使用范围 | `search.rs:79` / `analytics.rs:66` / `ai_usage.rs:120` — 全部使用 `s.pg_read.clone()` | ✅ 确认 |
| 副本连接方式 | `persistence.rs:28-33` — `REPLICA_URL` → `connect_pool`，否则 `pg.clone()` | ✅ 确认 |
| 无 RYW 保护层 | 全局 grep `RecentWriterCache`/`recent.*writer` 零命中 | ✅ 确认 |
| 写操作后无等待 | `send_message` / `mark_read` / reaction 等写路径无任何 replica-lag 等待或 check | ✅ 确认 |
| 无 replica_lag 指标暴露 | `/ready` 探针(routes.rs:658) — 仅检查 pg/redis/nats/blob | ✅ 确认 |

**风险级别 P1** — 这三个路径（搜索/分析/AI用量）的 handler 当部署真副本后会静默返回过期数据。

---

## ⚠️ 方向四（Embedding/FTS 索引生命周期）— **部分错误/部分有效**

| 子项 | 代码证据 | 验证结果 |
|------|---------|---------|
| **编辑后 embedding 不会更新** | **❌ 被最新代码证伪**。`crud.rs:99` — `SET ... embedding = NULL` 在 edit SQL 中；`messages.rs:203-207` — 编辑后 `enqueue(AiJobKind::Embed)` 入队列；worker 的 `has_embedding` 因 null 返回 false → 继续 re-embed | ❌ **文档错误** |
| 无 REINDEX 机制 | 全局 grep `REINDEX`/`reindex` 零命中 | ✅ 确认有效 |
| 无 CLI backfill 命令 | `aero-cli` 无 `index backfill-fts` 或 `backfill-embeddings` | ✅ 确认有效 |
| 无 `embedding_model` 列 | 全局 grep `embedding_model` 零命中 | ✅ 确认有效 |
| FTS 分析器变更后数据需回填 | `0128_fts_english_stemmer.sql` — `search_tsv` 是 STORED generated column，变更分析器后的确需要 `UPDATE messages SET searchable_text = searchable_text` 触发全量重新计算 | ✅ **有效**（但文档误称这条 SQL 是 `0131_fts_unaccent.sql`） |
| HNSW 索引无声损坏风险 | 无监控/修复逻辑，仅存在 `INDEX_SIZE_BYTES` gauge | ✅ 确认有效 |

**关键修正**：文档中「编辑后 embedding 不变」是中**严重错误**——`edit` 的 UPDATE 语句已设 `embedding = NULL`（`crud.rs:99`），且 `edit_message` 同步入队列 `AiJobKind::Embed`（`messages.rs:203-207`）。这一条当时可能基于旧版本代码。

---

## ⚠️ 方向五（多通道通知去重与 SLA）— **有效但有部分过时**

| 子项 | 代码证据 | 验证结果 |
|------|---------|---------|
| 无跨通道去重 | `push_bot.rs` — 无 `channel_delivered` JSONB 列，无 `source_path` 标签；`notification.rs` schema 无此列 | ✅ 确认有效 |
| 无 unified read-receipt | 无跨通道同步机制 | ✅ 确认有效 |
| inbox 无 TTL 清理 | ✅ 但**文档漏提已存在的清理**：`notification.rs` 有 `sweep_read_before` 方法**已实现**，`background.rs` 的 retention sweep 会调用它 | ⚠️ 已在 retention sweep 中覆盖（文档低估了） |
| push_bot 不在 /ready | `routes.rs:658-659` — 仅 pg/redis/nats/blob 探针 | ✅ 确认有效 |
| **bundle 逃生** | `notification_bundle.rs:40-46` — 已有 `AERO_BUNDLE_DEADLINE_SECS` 环境变量（默认 30s，Min 10s），再加 `max_bundle_delay` 冗余 | ⚠️ **文档高估了**——已实现可配置 deadline |

---

## 汇总表：文档正确性评级

| # | 方向 | 核心 thesis | 正确性 | 修正幅度 |
|---|------|------------|--------|---------|
| 1 | Draft/Composer | 无版本/冲突/typing管控 | ✅ **完全正确** | 无需修正 |
| 2 | 附件平台 | 配额/缩略图/CDN/分享均为零 | ✅ **完全正确** | 无需修正 |
| 3 | Read-Your-Writes | 三路径无一致性保护 | ✅ **完全正确** | 无需修正 |
| 4 | Embedding/FTS 索引 | 生命周期真空 | ⚠️ **编辑后 embedding 不更新 = 错**；其他子项有效 | 需删除「编辑后 embedding 不会更新」，替换为「HNSW 无声退化风险 + 无模型标识列」 |
| 5 | 多通道通知去重 | 跨通道无去重/无统一回执 | ⚠️ **核心 thesis 正确**；inbox 清理 和 bundle 逃生被低估 | 需补充 retention sweep 和 bundle deadline 已有实现 |

**综合准确度评估**：~80%（5 方向中 3 个完全正确，2 个有局部错误/低估，无全错方向）。文档的工程建议（Draft 版本冲突、Typing 超时限流、RecentWriterCache、缩略图管线等）方向正确，值得采纳。
