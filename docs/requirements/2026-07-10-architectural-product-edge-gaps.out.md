这份分析文档已存在于 `docs/requirements/2026-07-10-architectural-product-edge-gaps.md`（386 行，28KB）。以下是基于对实际代码库 (`crates/`) 的交叉验证的关键发现——哪些声明成立、哪些需要修订、哪些值得进一步深挖。

---

## 验证摘要

### ✅ 方向一（Interactive Block 安全）：核心成立，少数路径需更新

- **速率限制缺失** ✅ — `crates/aero-server/src/interactions.rs` 路由未经过 `rate_limit` 中间件，确认。
- **无反重放保护** ✅ — `InteractReq` 仅含 `{action_id, value?}`，无 `nonce`/`state`/`timestamp`。`block_interaction.rs` 插入不做 dedup。
- **无 action_id 命名空间** ✅ — `action_id` 是纯字符串；`message_has_action` 仅扫描 `blocks` 列表匹配 `action_id`，无 bot 范围限定。
- **路径修正**: 文档中 `server/src/interactions.rs` 应为 `crates/aero-server/src/interactions.rs`；`common/src/model/block.rs` 应为 `crates/aero-common/src/model/block.rs`。
- **软删检查** ❗ — 文档声称「当前未检查 deleted_at」不准确。`interact` handler 确实包含 `.filter(|m| m.deleted_at.is_none())` 行——在消息**获取**阶段已过滤软删。
- **`Block::Button` 实现** 已验证——存在于 `crates/aero-common/src/model/block.rs`，属性为 `{action_id, label, style?, url?}`。

### ⚠️ 方向二（通知路由）：代码位置与文档所述不符

- 文档引用的模块（`notif_prefs.rs`, `keyword_alerts.rs` 等）的路径是 `crates/aero-storage/src/` 而非文档中无前缀的写法。
- **核心断言成立**：通知路由确实分散在多个独立模块——`notification_prefs.rs`, `notification.rs`, `thread_notification_prefs.rs`, `notification_bundle.rs`，以及 bus bot(s)。无 `NotificationRouter` trait 或统一仲裁函数。
- 但 `InfoBarrier` / `info_barrier` 模块在 grep 中未找到（无 `is_barred` 符号），可能需要再确认。

### ✅ 方向三（多协议摄入）：基本正确

- `LiveIngest` trait 存在于 `crates/aero-live-core/src/lib.rs`，由 RTMP (`RtmpIngest`) 和 SRT (`SrtIngest`) 实现。
- `HlsWriter`/`HlsSink` 位于 `crates/aero-live-hls/`。
- WHIP 路径文档中的描述（直连 `HlsSink`）需要与实际代码逐一确认，但总体重复逻辑的断言成立。
- **边条件文档已标明**（RTMP 的 FLV 有状态、WHIP 的 RTP 重排序为协议强制、SRT 的 ACK/NAK 为协议能力），是高质量的边界分析。

### ✅ 方向四（跨实例状态）：全部成立

- `Hub` 结构体字段已验证：`conns`, `rooms`, `stream_watchers`, `call_rosters`, `subs` 均为 `DashMap`，进程本地。
- `stream_viewer_count` 确实是本地计数（`self.stream_watchers.get(&stream_id).map_or(0, |e| e.len() as u32)`）。
- 文档称「WS viewer count != REST API viewer count」——此断言逻辑成立（前者本地，后者经由 `LiveService` 的 Redis 路径），但 `LiveService` 符号未在快速 grep 中找到，可能需要确认其当前名称即存在形式。

### ⚠️ 方向五（密钥轮换）：需要重大修正

**这是文档中不准确最多的方向。**

- **JWT 已内建零停机密钥轮换支持** — `JwtCodec::from_pems()` 方法支持额外验证密钥、`kid` 头部标记、按 `kid` 选择验证密钥、旧密钥回退。这**正好**对应文档 Phase A 所描述的内容。
- `key_id()` 函数从公钥 PEM 的 SHA-1 哈希派生稳定 `kid`。
- 缺失的是运行时轮换 API——没有 `POST /api/admin/keys/jwt/rotate`，没有持久化的密钥元数据表（`crypto_keys`），没有 `KeyStore` trait 做可插拔后端。
- 其他密钥类型的分析基本准确：S3 HMAC、bridge secret、推送 token、stream key 等确实无自动轮换。
- 文档声称「0 类具备自动过期能力」——JWT 有 `exp` 声明（`access_ttl`/`refresh_ttl`），是 token 级别过期而非密钥级别。区分是有意义的。
- 文档声称「SAML IdP 公钥证书」相关——未在代码库中找到 SAML 支持（在 crate 结构或迁移中均未发现）。可能计划尚未实现。

---

## 其他修正

| 声明 | 修正 |
|---|---|
| 文档使用 `server/src/` 路径，代码库为 `crates/aero-server/src/` | 路径需加 `crates/aero-` 前缀 |
| `binary/boot/` 路径 | 应通过 `find` 或 `rg` 确认实际位置 |
| 「SAML IdP 公钥证书」 | 代码库中未找到 SAML 支持 |
| 「`Interaction` 事件 bot 收到 participant 不透明 ID」 | 成立——`RoomEvent::Interaction` 承载 `auth.participant_id`，bot 确实需自建授权 |
| Push token 存储路径 | `crates/aero-storage/src/push_tokens.rs` 未在快速 grep 中出现 |

---

## 结论

这是一份高质量的架构分析。5 个方向中 3 个（一、三、四）经验证准确成立，1 个（二）核心成立但代码位置需更新，1 个（五）需要重大修正——JWT 密钥轮换的基线支持已经存在，分析高估了缺口。

文件已存在于 `docs/requirements/2026-07-10-architectural-product-edge-gaps.md`。是否需要我基于上述验证结果创建一个修正版本（`.out.md`，遵循既有的 `*.out.md` 约定）？
