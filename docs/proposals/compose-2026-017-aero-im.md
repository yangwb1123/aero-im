完成。交付物为 `docs/proposals/compose-2026-017-aero-im.md`（277 行，唯一变更，未改任何代码），已通过组合变更校验（`check-no-refusal.py` → OK）。

## 提案要点

**事件消费契约**（§2）
- **传输**：subject `vault.file.notify` → 新 stream `VAULT_EVENTS`（`AERO_BLOB_BACKEND=vault` gate，7d limits/file，仿 `IM_MESSAGES`）→ **durable queue-group 消费者 `aero-vault-file-notify`**（同 `aero-golive` 模式，集群单节点扇出，`DeliverPolicy::All`），接线在 `bin/boot/background.rs` 仿 `golive_bot`。
- **Schema @1.1**：自包含载荷草案（`schema`/`version`/`event_id`/`occurred_at`/`action`/`reason`/`file{blob_id, owner, name...}`）；两阶段解码（先 lift `event_id` 再 typed decode，容忍未知字段）；major≠1 直接死信；1.x 只允许 additive 演进。
- **幂等三层**：broker `Nats-Msg-Id`（有界）→ `ConsumerEventReceiptRepo` claim/fencing/lease（主防线，5min lease > 120s ack_wait）→ 业务键（delivery 行 `event_id` PK + activity 部分唯一索引兜底）。
- **重试/死信**：失败 release + 不 ack 由 broker 重投，无独立 backoff worker；`max_deliver=16` 后毒信停投、abandoned receipt 由既有 sweep 收敛；契约门禁失败/收件人不可解析记 `dead`/`skipped` 直接 ack，不浪费投递。
- **delivery-status**：新增迁移 `0238` 的 `vault_file_notify_deliveries` 表（`delivered/failed/dead/skipped` + attempts/last_error），对齐 `webhook_delivery_log` 观测形态但不带其重试状态机。

**通知模板**（§3）：选用 **`activity_feed`**（非 `notifications` inbox——后者 room/message 均 NOT NULL）；`kind="vault_file"`、`subject_id=blob_id`、模板按 `action×reason` 映射中文 summary；收件人从 `blobs.owner_id` 主路径解析、`(issuer,subject)` 兜底；已擦除/Bot 所有者 skip；无 WS 帧（v1 走既有 `/api/activity` REST）。

**与既有链路的关系**（§4）：附件链路**只读消费**——不写 `blobs`、不入 `blob_gc_queue`、不改安装 ledger、**通知失败绝不回滚删除**；审核链路不产生消息/`Deleted` 广播/不经审核（模板是常量，不承载用户内容）；明确不做清单（回调 Vault、WS 扇出、推送接入、独立重试 worker 等）。
