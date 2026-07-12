现在我已经建立了充分的代码基础来评估你的文档。这是一份整体质量很高的分析——让我给出结构性反馈。

---

## 整体评估

这是一份扎实的「扫尾」分析。五条方向中的四条（#1、#2、#3、#5）在已有文档中确实没有论证过，#4（跨工作区协作）在 `expansion-2026-07-09-v2.md` 的企业相关讨论中有轻微触及但未系统化。方法论的透明性（逐一核对代码锚点、以 `❌` 标记空白）值得肯定。

以下是我逐条发现的补充/修正：

---

### 方向一（直播实时字幕/ASR）— 代码验证基本准确，但有一个关键修正

你写 "Whisper 仅出现在 `transcribe_bot.rs`（对已发送的 `Block::Voice` 做离线转写），无实时流 ASR"——✅ 正确。但有一个潜在捷径：

**你遗漏了 `CallEvent::Caption` 作为实现模板**。`crates/aero-common/src/model/media.rs:178` 已有的 `Caption` variant（含 `text`/`lang`/`translated`/`is_final`）是通话字幕的线协议。直播字幕可以**复用几乎相同的线格式**——只需把 `call_id`/`room_id` 换成 `stream_id`，而非从头设计一个新 event struct。

这意味着 `StreamEvent::Caption` 的实现工作量从 ~0.5d 降低到 ~0.2d，因为序列化模式（`is_final` 增量更新、可选翻译字段）已在生产中验证。

**边界情况补充**——你遗漏了一个需要处理的极端情况：

| 场景 | 风险 | 应对建议 |
|---|---|---|
| ASR 返回空字符串（静音段、背景音乐无语音） | 产生大量无意义的空字幕 event | 过滤 `text.trim().is_empty()` → skip（仍发 `is_final=false` 的心跳维持同步）|

---

### 方向二（ABR 多码率 HLS）— 技术可行性评估偏乐观

这个方向你的分析是对的，但**我认为工作量应该比 7d 更高**。原因：

**转码引擎不能是 `std::process::Command` spawn ffmpeg**。底层代码结构对此不友好：

```
ingest(depacketize) → raw H.264 NAL units → FlvToTsConverter → HlsWriter
```

ffmpeg 管道要求输入要么是：
- 文件路径（延迟极高）
- pipe stdin（需要完整 elementary stream，当前架构不维护）

实际需要**解耦**：在 `depacketize` 后插入一个可选的转码旁路。H.264 bitstream 解析 + 重编码的时间线对齐远比 spawn ffmpeg 复杂。

**建议修正的执行计划**：

| 阶段 | 内容 | 估算 |
|---|---|---|
| Phase 1 | 重构 `HlsWriter` → `HlsVariantWriter` + `MasterPlaylistWriter`（不变编解码逻辑） | ~2d |
| Phase 2 | 插入转码旁路：`depacketize → optional transcode pipeline → FlvToTsConverter per variant` | ~4d |
| Phase 3 | ffmpeg 子进程管理（启动/保活/优雅关停/超时 kill/健康检测） | ~2d |
| Phase 4 | 关键帧对齐逻辑（`force_keyframe` 在所有 variant 同一时间戳打 IDR） | ~2d |
| Phase 5 | 延迟优化 + Simulcast 直通路径（str0m 已有 `SubscribedTrack::select` 可以复用） | ~2d |
| **合计** | | **~12d** |

这是一个 **P1 但不应并行启动**的方向——因为它依赖方向三的 segment 基础设施。建议放在方向三之后。

**还有一个被忽略的约束**：当前 `FlvToTsConverter` 假定输入是 H.264 + AAC。如果推流端使用 H.265（HEVC），转码管线的编解码器协商需要加一个 fallback。这在生产环境（OBS 默认 H.264）中很少见，但边界应在架构图里标注。

---

### 方向三（DVR / Time-Shifted Viewing）— 最稳的方向，但有一个架构问题

这是五个方向中最干净的一个——侵入最小，切面清晰。

**但是 DVR manifest 和 live manifest 的同步问题值得认真对待**：

你写 "两个 manifest 写同一 segment 文件，只是 playlist 长度不同 — 文件一致性问题不存在"。这不是完全正确的。考虑时序：

1. `push_segment` 写入 seg_100.ts + 更新 live manifest（6 段 window）
2. 同一 tick，DVR manifest 更新（追加 seg_100.ts）
3. 在第 1 步和第 2 步之间，一个 DVR 读者请求 `dvr/index.m3u8` → 该 manifest 没有 seg_100.ts 但文件系统中 seg_100.ts 已存在——这不一致但无害（manifest 只是不引用）
4. **反向问题更严重**：DVR manifest 引用了一个 seg，但该 seg 文件已被 GC 删除（如果 `DVR_WINDOW_HOURS` 启用）

**建议**：DVR 的 segment 不应复用 live manifest 的 `remove_file`。需要独立目录：

```
{hls_dir}/{stream_id}/
  ├── live/          # live manifest + 6 segments (sliding)
  │   ├── index.m3u8
  │   └── seg_*.ts
  └── dvr/           # DVR manifest + all segments (append-only)
      ├── index.m3u8
      └── segments/
          └── seg_*.ts
```

这样 DVR 的 segment 永不被 live 的滑动窗口删除，GC 只作用于 `dvr/segments/` 的超期文件。

**另外请注意**：当前 `HlsWriter::push_segment` 的 `write_manifest` 是同步写磁盘。DVR manifest 每次追加 segment 都 write 一遍——对于高频推流（2s segment），这是额外的 O(n) 每段写入（n = 累计段数）。需要改为附加写而不是全量重写。DVR manifest 的 `write_manifest` 应为 `append_segment`：

```
// 不是
buf = full_manifest(); fs::write(manifest, buf)
// 而是
buf = "#EXTINF:2.000,\ndvr/segments/seg_N.ts\n"; fs::append(manifest, buf)
```

---

### 方向四（跨工作区协作）— 业务价值被低估，技术分析被高估

**我不同意 P2 的评级**。这个方向应该提到 P1，原因：

1. **每个企业 PoC 都会问这个问题**。通常是在第一次 demo 时：「我们需要 Agency A 和 Client B 在同一个频道」— 当前架构只能说「不行」，直接丢单。
2. **竞品对标**：Slack Connect 是 Slack 最强企业武器。Teams External Access 是 Teams 采购的必选项。无此 ≈ 不参与企业采购 RFP。
3. **SCIM 出站被你排除，但 Phase 1 共享频道完全不依赖 SCIM**——身份桥可以简单到 `invite_by_email → cross_workspace_membership`。无需身份联邦即可上线。

**技术风险也比我预期高**——你低估了一个核心复杂度：

> "主人工作区的 admin 可管理 shared_with 列表"

问题：客人工作区的成员**不可见主人工作区其他房间**——但 `assert_room_access` 当前接受一个 `room_id` 并检查`workspace_members`。如果客人 Participant B1 在 shared room R 中，B1 尝试访问同一 workpace 中的另一个房间 R2：

```
assert_room_access(B1, R2) →
  1. B1 是 workspace B 的成员（R2 所属 workspace A？R2 也是 A 的？）
  2. 检查 B1 是否在 workspace A 中是成员 → ❌
  3. 检查 B2 是否在 shared_with 中有权 → 只包含 R，不包含 R2
```

但这个逻辑需要扩展——当前 `assert_room_access` 只做 `participant_in_workspace + participant_in_room`。共享关系在 room 级，`assert_room_access` 需要新增一个路径：**如果 room 被 shared_with participant 的 workspace，则允许该 participant 访问该 room，但不应授予对其 workspace 中其他资源的访问权限**。

**你建议的 `shared_with = [B, C]` 在 room 级别，这是正确的**。但 `is_member` 检查需要加一个 fallback 查询 `shared_rooms` 表，这个在 `assert_room_access` 的当前 SQL 中没有直通路径——需要加一个 `LEFT JOIN shared_rooms ON ... AND shared_rooms.guest_workspace_id = $workspace_id`。

这个 SQL 变更集中在 `is_member` 查询中，不需要重构 `assert_room_access`——所以说技术风险是「中」而非「高」，这个判断我认同。

**但数据驻留风险需要更严肃对待**：`workspace.region_code` 作为前置条件我赞成，但如果 workspace A（us-east）邀请 workspace B（eu-west）的成员，被邀请者的数据传输会违反 GDPR。你需要一个在邀请时点就检查 region 匹配的守卫——不能只在创建后扫描。

---

### 方向五（封禁规避检测 + 信任评分）— 分析最完整，但 Phase 1 有现成基础设施被忽略

这个方向的分析在五个中最深入。但我有一个重要的补充：

**当前已有部分 T&S 基础设施可以复用**：

| 已存在 | 位置 | 可复用方式 |
|---|---|---|
| `login_throttle`（per-account lockout） | `crates/aero-auth/src/service.rs:194` | Phase 1 的注册限流可以直接扩展 `login_throttle`（当前叫这名但实际锁的是 account，不限于 login） |
| `ban_appeals.rs` | `crates/aero-server/src/ban_appeals.rs` | Phase 2 的误申诉路径可直接复用现有申诉工作流（仅加 `risk_event_id` 外键关联） |
| `ip_allowlist::enforce_layer` 中间件 | `crates/aero-server/src/ip_allowlist.rs` | 反面的 IP 拒绝可以直接复用同一个 `X-Forwarded-For` 解析 + 中间件模式 |
| `keyword_moderator` | `aero-im-core` | 域名黑名单（`tempmail.txt`）可用相同的关键词匹配模式 |

**一个被忽略的信号**：**注册时的 display_name 相似度检查**。当前注册路径（`register` → `create_human`）对 display_name 没有唯一性约束。同一个被 ban 的用户可以用 `"evil_streamer"`、`"evi1_streamer"`、`"evil_streamer_"` 反复注册。Levenshtein 距离应在注册时计算——不需要 ML，pg_trgm 已经作为扩展安装了（见 `README.md` pgvector + pg_trgm）：

```sql
SELECT similarity('evil_streamer', 'evi1_streamer') AS sim;  -- pg_trgm
```


**优先级我建议调高到 P1**。理由是：这个方向是纯粹的防守性投资，但内容平台的 T&S 失败不是渐进式损失——是**灭顶之灾**。一个反复逃避封禁的恶意用户造成的骚扰事件可以毁掉平台口碑。而且它的实现量（Phase 1 约 3d）远小于其他方向。

---

### 交叉观察：三个方向间隐含的依赖图

你的「执行建议」提到方向②依赖③——对。但还有两个依赖关系没有说：

1. **方向①（ASR）依赖方向③（DVR）**：实时字幕在直播中显示，但 VOD 的字幕必须从 DVR 存储的 segment 时间轴导出。如果没有 DVR 的完整 segment 索引，VOD 的字幕无法对齐。建议**方向① 和 ③ 共享 `segment_index` 命名空间**——ASR 的 `start_ms`/`end_ms` 应该绑定到 DVR segment 编号，而非 wall clock。

2. **方向④（跨工作区）和方向⑤（信任评分）共享身份基础设施**：跨工作区协作意味着一个 participant 的信任分应该跨 workspace 传播。如果 participant A1@ws-a 在 ws-a 中被封禁，ta 不应该能通过跨 ws 频道继续骚扰 ws-b 的人。Phase 2 的信任评分需要能够查询参与者在**所有** workspace 中的风险历史。

---

### 被忽略的方向（第零号）：LIVE_WINDOW_SEGMENTS = 6 硬编码

所有三个直播方向（#1、#2、#3）都依赖 HLS segment 基础设施，但**当前 `LIVE_WINDOW_SEGMENTS = 6` 是 const，不可配**。这个值对应 ~12-30s 的直播窗口，对于 ABR 切换缓冲和 DVR 延迟加入都不够。建议在实现任何直播方向前，先把 `LIVE_WINDOW_SEGMENTS` 改为运行时配置（`AERO__SERVER__LIVE_WINDOW_SEGMENTS`），默认值提高到 30-60（~1-2 分钟窗口）。这是一个 0.1d 的改动，但三个方向都依赖它。

---

### 总结：调整后的优先级

| 调整后优先级 | 方向 | 与原文档差异 | 原因 |
|---|---|---|---|
| **P1** | ③ 直播 DVR | 同意 P1 | 最小侵入、最高投资回报 |
| **P1** | ⑤ 封禁规避 + 信任评分 | ↑ 从 P2→P1 | T&S 失败是存在性风险；Phase 1 体量小可快速上线 |
| **P1** | ① 直播 ASR | 同意 P1 | 合规必需，但建议推迟到③ 之后（共享时间轴） |
| **P1** | ④ 跨工作区协作 | ↑ 从 P2→P1 | 每个企业 PoC 都问到 |
| **P2** | ② 多码率 ABR | 同意 P2（甚至 P3） | 实现量最大、延迟约束最严格；建议在③ 之后启动 Phase 1 的重构 |

**执行路径建议**：

```
Phase 1 (3-4 周):
  ③ DVR (基础: DvrWriter + 独立目录 + 路由) — 2d
  ⑤ 信任评分 Phase 1 (信号收集 + risk_events + banned_ips) — 3d
  LIVE_WINDOW_SEGMENTS 可配化 — 0.1d

Phase 2 (3-4 周):
  ① ASR + WebVTT (复用 DVR segment index) — 5d
  ④ 跨工作区共享频道 (shared_rooms + is_member 扩展) — 4d
  ③ DVR Phase 2 (VOD 转换 + GC) — 2d

Phase 3 (4-6 周):
  ② ABR (HlsVariantWriter + transcode pipeline + keyframe alignment) — 12d
  ⑤ 信任评分 Phase 2 (ML 辅助聚类 + 验证码挑战) — 4d
  ④ 跨工作区 DM (external_contacts) — 3d
```

---

文档整体质量很高。方法论—逐一代码验证—值得做标准。五个方向的切入锚点精确，工作量估算合理（唯方向②偏低约 40%）。没有覆盖已有的分析（"前置" 的去重做得干净）。唯一需要注意的：边界情况的覆盖率可以再深一点（方向③ 的 manifest GC 竞争条件、方向② 的 HEVC 输入假设），但这些是完善而非错误。
