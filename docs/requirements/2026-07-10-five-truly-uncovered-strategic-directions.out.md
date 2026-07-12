I've now done extensive codebase verification. Let me compile the assessment.

---

## 准确性审查：5 个扩展方向分析文档

本文档质量很高——论证结构清晰，每个方向都提供了代码证据、缺口分析和扩展提案。以下是对其**代码引用准确性**的逐方向核查。

---

### 方向一 (E2EE) — 基本准确 ✅

| 文档声明 | 代码验证 | 判定 |
|---------|---------|------|
| MLS scaffold 仅限于不透明字节 | ✅ `crates/aero-common/src/mls.rs` — 确实只有 `KeyPackage`, `MlsGroupState`, `MlsCiphertext` 类型定义，无状态机 | 准确 |
| 存储层仅 CRUD 不透明 blob | ✅ `crates/aero-storage/src/mls.rs` — `KeyPackageRepo` 仅 publish/consume_one/pending_count；`MlsGroupRepo` 仅 upsert/get | 准确 |
| REST 路由是中继 | ✅ `crates/aero-server/src/routes/mls.rs` — 四个端点全是 base64 收发不透明 blob | 准确 |
| E2EE 与 AI 功能的根本矛盾 | ✅ 系统确实做了 AI 功能（审核/RAG/摘要/翻译），E2EE 打开后这些功能不可用 | 准确 |

**微瑕**：文档用了路径 `aero-common/src/mls.rs` 而非实际路径 `crates/aero-common/src/mls.rs`。此外 `MlsCiphertext` **已经存在**（line 50-56），有一行注释 `"replaces Message.blocks when the room is E2E"`——表明代码对 E2EE 方向已有初步意识，非完全空白。

---

### 方向二 (统一通知中心) — **有显著误差** ⚠️

**关键错误 1**：文档声称 `activity_feed` 表无 `is_read` 列。
> 「开播/未接来电的已读状态在 `activity_feed` 表无 `is_read` 列——需要添加。」

✅ **但实际上** `activity_feed` 已有 `read_at: Option<time::OffsetDateTime>`（`crates/aero-storage/src/activity_feed.rs:47`），且有 `UPDATE activity_feed SET read_at = now()` 方法（:231）。已读状态已经在逻辑上存在。

**关键错误 2**：文档夸大了通知系统的分散程度。
文档说通知分散在 11+ 个子系统，但代码显示：
- ✅ `crates/aero-common/src/model/notification.rs` 定义了统一 `NotificationKind` enum（`Mention | Reply | AggregateReply | SavedSearch | Reaction`）
- ✅ `crates/aero-storage/src/notification.rs` 是统一的通知表 repo
- ✅ 已有 `notification_bundles` 表（migration 0143）和 `NotificationBundleRepo`（`crates/aero-storage/src/notification_bundle.rs`）——文档把「聚合通知」作为提案，实际已实现

通知确实有多个入径（mention/reply/reaction/go_live/missed_call），但底层存储/读取已经较统一——文档的「11+ 独立子系统」叙述偏严重了。

**部分准确**：审批/任务/频道公告确实没有一致的通知化处理。通知偏好确实分散（`notif_prefs` + `keyword_alerts` + `thread_subs` + `snooze`）。

---

### 方向三 (屏幕共享与通话) — **有严重事实错误** ❌

**致命错误 1**：文档声称 Web 端无 `getDisplayMedia` 调用。
> 「Web端 `calls.js` 无 `getDisplayMedia()` 调用」

❌ **这是完全错误的**。`web/calls.js` 中：
- Line 236: `display = await navigator.mediaDevices.getDisplayMedia({ video: true })` — 屏幕共享采集
- Lines 219-286: 完整的 1:1 屏幕共享流程（`toggleScreenShare` / `stopScreenShare` / `getDisplayMedia` / `replaceTrack` / `addTrack`）
- Lines 545-580: 群组 mesh 通话的屏幕共享（`gcallToggleScreenShare`）
- 屏幕共享开始/停止会触发 renegotiation，完整的信令处理 exist

**致命错误 2**：文档声称通话子系统缺少屏幕共享能力。
> 「缺少的能力：屏幕共享」

❌ 前端已经有 1:1 和 mesh 群组的屏幕共享实现。通过 `getDisplayMedia` 获取屏幕流，通过 `replaceTrack` 或 `addTrack` 替换/添加视频轨道，通过 negotiationneeded → re-offer 通知对端。

**部分准确**：SFU 层面 (aero-live-webrtc) 的多轨道支持确实是缺口。`SfuPeer` 和 `SfuForwarder` 当前以单视频轨道设计，没有显式的 `TrackKind` 或 `stream_id` 多路复用。但文档完全没有认可前端的现有实现。

**文档关于通话录制的说法（缺 MCU/Mixer）是准确的**——当前 SFU 是纯选择性转发，无混音/转码录制能力。

---

### 方向四 (Schema Registry) — 准确 ✅

- ✅ 事件定义确实无 schema version：`RoomEvent`、`StreamEvent`、`CallEvent` 都无 `schema_version` 字段
- ✅ `deny_unknown_fields` 未被使用（代码注释 `src/service/events.rs:76` 明确提到 "no event type uses deny_unknown_fields"）
- ✅ 无向后兼容性检查
- ✅ 无版本协商机制
所有断言都得到代码验证。

---

### 方向五 (分析管道) — 准确 ✅

- ✅ 分析查询走 PG 主库：`crates/aero-server/src/analytics.rs` 使用 `AppState.pg` 直接查询 messages 表做 COUNT/聚合
- ✅ 无独立分析管道（`analytics_events` 表不存在）
- ✅ 无行为事件（无 `AnalyticsEvent` 枚举）
- ✅ 无 ClickHouse 集成
- ✅ `workspace_analytics` 确实做 `COUNT(*)` 类全表扫描操作

所有断言成立。

---

## 总结：准确性问题影响评估

| 方向 | 准确性问题 | 对论点的影响 |
|------|-----------|------------|
| E2EE | 路径名偏移；`MlsCiphertext` 已存在 | **轻微**—不影响核心论点（E2EE 未实现） |
| **通知中心** | `activity_feed` 已有 `read_at`；通知模型较统一；聚合通知已实现 | **中等**—文档提出的「需要添加」的能力部分已存在，修正后方向仍然成立但紧迫性降低 |
| **屏幕共享** | 前端已有完整屏幕共享（1:1 + mesh）；calls.js 有 `getDisplayMedia` | **严重**—核心论据「缺少屏幕共享」与代码事实矛盾。应修正为「SFU 侧缺少多轨道转发支持」，而非「整个系统缺少屏幕共享」 |
| Schema Registry | 无 | 完全准确 |
| 分析管道 | 无 | 完全准确 |

**最重要的修正**：屏幕共享方向应从「从零构建屏幕共享」调整为「将前端已有的 1:1/mesh 屏幕共享扩展到 SFU 架构」。这不是一个新的功能开发，而是现有功能的架构升级。
