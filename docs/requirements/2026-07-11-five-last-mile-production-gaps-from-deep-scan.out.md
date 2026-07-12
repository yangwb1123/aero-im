现在我已完整验证了所有关键代码引用。以下是分析中各声明的逐项准确性评估：

---

## 验证结果总结

### 方向一（Draft / Composer）— ✅ 全部准确

| 声明 | 结论 | 证据 |
|------|------|------|
| Draft upsert 无版本/乐观锁 | ✅ 准确 | `draft.rs:58-72` — `ON CONFLICT ... DO UPDATE SET`，无 `version` 列 |
| Typing 无 rate limit / 无超时 | ✅ 准确 | `reads.rs:43-58` — `typing()` 直接 `publish_room_event`，无限流无过期 |
| 房间内全部在线成员接收 Typing 广播 | ✅ 准确 | `frame.rs:108` → `state.im.typing(pid, room_id, on)` → `publish_room_event(Typing{...})` → NATS 扇出到全部成员 |

### 方向二（附件平台）— ⚠️ 部分准确，需修正 2 处

| 声明 | 结论 | 证据 |
|------|------|------|
| 无 per-user/per-workspace 配额 | ✅ 准确 | 无 `SUM(size) WHERE owner_id` 检查 |
| 无缩略图/预览管线 | ✅ 准确 | 代码库中无 `thumbnail`/`thumb` 生成代码 |
| 无视频转码→HLS VOD | ✅ 准确 | HLS 管线在 `aero-live-hls` 但仅直播用 |
| 下载无 Cache-Control / ETag | ✅ 准确 | `routes.rs:1750-1810` — 无缓存头 |
| S3 预签名 URL = 直接代理传输 | ✅ 准确 | `s3_blob_store.rs:237-241` — `get()` 下载全字节返回 |
| 无 `/api/me/blobs` 文件管理 | ✅ 准确 | 不存在此类路由 |
| ❌ **无 Content-Disposition 差异化** | ❌ **不准确** | `routes.rs:1774-1795` 有 `inline` vs `attachment` 差异化。图片/视频/音频 = inline，其余 = attachment |
| ❌ **MAX_BLOB_BYTES = 10MB** | ❌ **不准确** | 实际值 `32 * 1024 * 1024` (**32 MiB**)，非 10MB |

### 方向三（Read-Your-Writes）— ✅ 全部准确

| 声明 | 结论 | 证据 |
|------|------|------|
| `pg_read` 用于 `search.rs` | ✅ 准确 | `search.rs:79` — `MessageRepo::new(s.pg_read.clone())` |
| `pg_read` 用于 `ai_usage.rs` | ✅ 准确 | `ai_usage.rs:120` |
| `pg_read` 用于 `analytics.rs` | ✅ 准确 | `analytics.rs:66` |
| 无 read-your-writes 保障 | ✅ 准确 | 三处路径均未检查用户最近写入 |
| persistence.rs 连接逻辑 | ✅ 准确 | `persistence.rs:41-50` 与分析原文完全一致 |

### 方向四（Embedding/FTS）— ⚠️ 1 处不准确，其余准确

| 声明 | 结论 | 证据 |
|------|------|------|
| 无 REINDEX cron | ✅ 准确 | 索引重建迁移(0075/0136)均有但无定时重建 |
| FTS 分析器变更无数据回填 | ✅ 准确 | `messages.search_tsv` 只通过 DML trigger 更新，无回填机制 |
| 跨租户 embedding 命名空间混叠 | ✅ 准确 | `embedding` 列无 `embedding_model` 标记 |
| ❌ **编辑后 embedding 不更新** | ❌ **不准确** | `messages.rs:193-206` 中 `edit_message` 会显式 enqueue `AiJobKind::Embed`；底层 SQL `crud.rs:87-105` 中 `SET embedding = NULL`，worker 会正常重新生成。分析关于「`IS NULL` 条件不会捕获」的推理因为 edit 已 null embedding 而不成立 |
| 迁移前消息 FTS 不对齐 | ✅ 准确 | 迁移(0128/0131)只建分析器，旧消息 DML 前 `search_tsv` 用旧规则 |

### 方向五（多通道通知）— ⚠️ 1 处不准确，其余准确

| 声明 | 结论 | 证据 |
|------|------|------|
| 无跨通道去重 | ✅ 准确 | 无 `source_path` / `channel_delivered` 机制 |
| 无 unified read-receipt | ✅ 准确 | push/email 点击不会同步到 inbox |
| inbox 通知数膨胀 | ✅ 准确 | 无 TTL 清理（只依赖 retention sweep） |
| 降级无通告 | ✅ 准确 | `/ready` 不检查通知通道 |
| ❌ **Bundle 无超时逃逸** | ❌ **不准确** | `notification_bundle.rs:167` — `AERO_BUNDLE_DEADLINE_SECS` (默 30s，最小 10s) + `background.rs:262-269` — 定期 flush (默 10s)。最大延迟 ≈ 30-40s，不会到「小时」级 |

---

## 最终评估

**3 个方向完全准确**：方向一 (Draft/Typing)、方向三 (RYW)、方向四的大部分（除编辑后的 embedding 路径）

**2 个方向需小幅修正**：
- **方向二**：`Content-Disposition` 差异化已实现；`MAX_BLOB_BYTES` 是 32 MiB 非 10MB
- **方向五**：`AERO_BUNDLE_DEADLINE_SECS` 提供 bundle 超时逃逸，最大延迟约 40s

整体分析质量很高。建议将以下修正合并到文档中：
1. 方向二的 `Content-Disposition` 行改为「无 CDN 缓存头 / ETag」（去掉差异化声明）
2. 方向四的编辑→ embedding 段落删除（该路径已正确实现）
3. 方向五的 bundle 超时逃逸段落删除或在分析中注明 `AERO_BUNDLE_DEADLINE_SECS` 的存在
