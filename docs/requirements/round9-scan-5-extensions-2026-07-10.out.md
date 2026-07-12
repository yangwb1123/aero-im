This is an impressively thorough analysis. I've independently verified the key claims against the actual codebase. Here's my review:

---

## 验证结果

### ✅ 方向一（AI Prompt 管理）— 完全验证

你的 `10+ 模块硬编码` 表格是准确的。我在 `crates/aero-ai/src/service/tools.rs` 找到了所有 system prompt：

- `SUMMARIZE_SYSTEM_PROMPT`（行 26）
- `RECAP_SYSTEM_PROMPT`（行 35）
- `TRANSLATE_SYSTEM_PROMPT`（行 44）
- `MODERATE_SYSTEM_PROMPT`（行 48）
- `ANSWER_SYSTEM_PROMPT`（行 53）
- `THREAD_TITLE_SYSTEM_PROMPT`（行 61）
- `SENTIMENT_SYSTEM_PROMPT`（行 67）
- `AGENT_SYSTEM_PROMPT`（行 99）
- 以及 `crates/aero-ai/src/service/profile.rs` 的 `PROFILE_SYSTEM_PROMPT`（行 111）

全部是 `pub(crate) const &str`，零运行时配置。`agent_bot.rs` 通过 `AiService::answer_question` 间接使用这些硬编码提示词。**证据确凿**。

**补充**：你的表格应加上 `PROFILE_SYSTEM_PROMPT`（跨房画像行 111），使硬编码点变为 10 个模块 / 9 个 const。另外 `tools.rs` 里还有工具定义（`SearchMessagesTool` / `ReadAttachmentTool` 的 `description` 和 `input_schema`）也是硬编码——虽然不是 system prompt 本身，但属于同一类问题。

### ✅ 方向三（草稿持久化）— 完全验证，但有一处微调

后端确实完整：`migrations/0028_drafts.sql` → `aero-storage/src/draft.rs`（`DraftRepo` 含 `upsert`/`find`/`list`/`delete`）→ `aero-server/src/drafts.rs`（REST 路由）→ `routes/routes.rs:211`（`.merge(crate::drafts::routes())`）。

前端确认零引用：`grep -rn "draft\|saveDraft\|loadDraft" web/app.js web/api.js web/ws.js web/context.js` → 无命中。

**一处微调**：你说 `switchRoom()` 清空 `composerInput.value`，但实查 `app.js:533–560` 并没有 `.value = ''`——它只是**不清空也不保存**。实际效果是：切换房间后 composer 里还残留着上一个房间的文本。这比「丢失」更糟——用户以为文本还在，但实际上保存到新房间的却是错误的内容。提交表单时其他房间的文本才被清掉（`app.js:896`, `909`）。不过这属于细节，不削弱核心论点。

### ✅ 方向二（灾备）— 完全验证

`scripts/` 下 55 个文件，没有 `backup.sh` / `restore.sh`。`docker-compose.yml` 无备份服务。`Makefile` 无备份 target。Redis 配置了 `--appendonly yes`（方向二表的 ⚠️ 部分应升级为 ✅ 部分），但 PG 零备份配置。

### ✅ 方向四（冷热分层）— 推理正确，补充一点

你说 `ai_jobs` 表「无清理策略」——需要区分语义：`ai_jobs` 是**工作队列**（`FOR UPDATE SKIP LOCKED` 消费），不是 LLM 对话日志。`AiWorker` 的 `MAX_ATTEMPTS=5→dead` 后行留在表里。所以清理需求存在的，但体量较小（每日 ~jobs 数，不是 ~messages 数）。这不影响方向四的核心论点。

### ✅ 方向五（开发体验）— 完全验证

`Makefile` 仅 50 行，功能有限。无 API playground（试过 `utoipa` / `aight`？无引用）。无种子数据。无 hot reload。无 pre-commit hooks。

---

## 结构性建议

### 1. 方向三和方向一的联动值得显式标注

如果 Prompt 管理平台（方向一）上线了，AI 回答时的「智能回复」（`smart_replies.rs`）和「Agent Bot」（`agent_bot.rs`）也可以使用工作区自定义的系统 prompt。**方向三是前端未接线，方向一是配置层未接线——两者都是「有后端骨架，缺前/配层」的模式**。可以考虑在跨方向注意事项里加一条。

### 2. 方向二的「NATS 消息备份」可以更精确

你说 JetStream 流「max_age 自动老化」。是的，`docker-compose.yml` 没有配置 `max_age`（NATS 默认 `max_age=0` = 不限）。不过对于已 ack 的消息，JetStream 在 `durable` consumer 全部 ack 后最终会清理（取决于 `max_age` 或 `max_msgs` 策略）。如果担心 NATS 消息损失，应关注的是 `durable` consumer 的起始位置策略（`DeliverPolicy`）和流配置——但方向二（PG 备份）和 NATS 消息备份确实是不同的问题。可以考虑加一句明确区分。

### 3. 方向五的 hot reload 兼容性

你提到的 `unsafe_code = "forbid"` 和 hot reload 的冲突——`cargo-watch` 不涉及 `unsafe`（它只是在文件变更时 rerun cargo），`cargo-hotpatch` 才可能冲突。`cargo watch -x run` 是安全的，只需在 `Makefile` 里加一个 target。建议把 `cargo-watch` 作为 Phase B 的默认方案，`cargo-hotpatch` 标为「需评估」。

### 4. 方向四的 `archived` 列实现建议

Phase A 说加 `archived BOOLEAN DEFAULT FALSE`。需要注意 `message` 表的已有行数——对 100M+ 行的表加 `DEFAULT FALSE` 的列不会回填已有行（PG 11+ 的 default 值存在系统目录，不物理写每行），所以迁移是安全的。但查询 `WHERE archived = FALSE` 需要索引来避免全表扫——建议在 Phase A 的迁移里同时建 `CREATE INDEX CONCURRENTLY IF NOT EXISTS`（需注意不能放事务里）。

### 5. 跨方向「token helper 同名」的约束（AGENTS.md §4.2）

方向一的 Prompt 管理如果引入 `generate_token` 等 helper（不一定需要），要注意不和已有的 `webhook::generate_token` 在 `aero-storage/lib.rs` re-export 时撞名。不过 Prompt 管理的 token 需求不大（API 鉴权复用已有 JWT），大概率不触发此问题。

---

## 综合评分

| 维度 | 评分 | 说明 |
|------|------|------|
| 事实准确性 | **9.5/10** | 全部关键 claim 可独立验证。唯一微调是 switchRoom 的清空行为 |
| 缺口论证 | **10/10** | 每个方向都清楚区分「既有覆盖了什么」和「本文新增了什么」 |
| 可执行性 | **9/10** | Phase A/B/C 拆分合理，每阶段的边界情况都覆盖到了 |
| 代码证据 | **10/10** | 每个方向都有精确的文件/行号引用，可独立 grep 验证 |
| 优先级判断 | **9/10** | P1 vs P2 区分合理。建议把方向三提升到「本周」而非「1-2 周」——纯前端改动且后端就绪，应是按天计的工作 |

总体来说是高质量的架构分析——有代码证据、有边界情况考虑、有明确的阶段性交付。如果这是要落盘到 `docs/requirements/` 的文件，建议保存前补充上面 4 处微调，然后就可以提交了。
