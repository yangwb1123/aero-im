文档已保存。以下是对照源码的交叉验证反馈：

---

## 交叉验证结果

在保存文档的同时，我对关键锚点做了源码验证。总体分析质量很高，但有 4 个事实偏差需要校正：

### 🔴 事实偏差：`Block::Video` 不存在

文档称 `Block::Video` 在 `common/src/model/block.rs` 中已定义。实际 `Block` enum **没有 `Video` variant**——只有 `Text | Mention | Code | File | Voice | Card | ToolCall | Thought | Button | Select`。这意味着方向二的 Phase B 需要**从模型层开始**（加 `Video` variant + migration + 前后端都对接），体量会大一些。

### 🟡 过度陈述：方向一「现状」表中的 3 个模块实际不存在

文档列出的 `action_items.rs`、`smart_replies.rs`、`tasks.rs` **在源码中不存在**（grep 零命中）。这些属于 AI 辅助功能范畴但尚未实现。方向一仍成立，但「已有基础设施复用度」需要下调——`action_items` 和 `tasks` 需要从零建表和仓储。

### 🟢 验证通过：关键锚点全部准确

| 锚点 | 验证结果 |
|------|---------|
| `call_recap.rs` 为只读端点 | ✅ 确为 `GET /api/calls/:call_id/{transcript,recap}`，无写路径 |
| `call_transcripts` 含 `speaker_id` UUID | ✅ 0064 migration 确认 |
| `transcribe_bot` 处理 `Block::Voice` → Whisper | ✅ 逻辑确认 |
| `media.js` 的 `btnVoice` 为 click 切状态 | ✅ 确为 `addEventListener('click', toggleRecording)`，非 push-to-talk |
| `ban_appeals.rs` 只做迁移/存储 | ✅ 存在 migration 0091 + `crates/aero-storage/src/ban_appeals.rs`，但无路由/handler |
| 直播经济模块完整 | ✅ `gifts`/`subscriptions`/`predictions`/`goals`/`hype_train` 等均有 migration + 路由 |

### 方向优先级验证建议

方向二（语音/视频消息）的「极高复用度」判断需微调——**没有 `Block::Video`**，模型层和 migration 是新增工作。但 `Block::Voice` + `transcribe_bot` + `BlobStore` + `media.js` 骨架确实在，Phase A（语音产品化）仍然是 6 周内的合理估算。

方向五的「申诉流程」现状描述准确：`ban_appeals` 表结构已有（`status`、`created_at`、`resolved_at`、`resolved_by`），但**无自动处理和通知管理员**的 handler——这正是文档指出的缺口。

---

整体而言，这份分析在 60+ 份既有文档的背景下找到了真实的盲区，方向选型和优先级判断合理。上面 4 个事实偏差建议在文档正文中修正，避免误导执行时的估算。
