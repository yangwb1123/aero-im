## 验证结论

已验证的 39 条声明中，39 条方向性准确，1 处次要技术措辞需要修正：

### 修正：`cache_answer_invalidate_room` 使用 `SMEMBERS+DEL` 而非 `SCAN+DEL`

文档声称 "AI 答案缓存失效路径使用非原子 `SCAN+DEL`"——实际代码（`ai_context.rs:115-122`）：

```rust
let keys: Vec<String> = self.client.smembers(&idx).await?;  // SMEMBERS, 非 SCAN
if keys.is_empty() { return Ok(0); }
let removed: i64 = self.client.del(keys).await?;
let _: () = self.client.del(&idx).await?;                    // DEL 索引
```

非原子的判断成立（SMEMBERS 和 DEL 之间有一个窗口期），但用的是 `SMEMBERS` 而不是 `SCAN`。方向性不影响。

---

## 逐方向审查

### 方向一（P1）——跨节点缓存一致性 ✅ 质量最高

这是文档中**最深、最有价值的方向**。源码交叉验证确认：

- `participant_cache.rs:24-27` 的文档块**自己承认了跨节点 stale 窗口**："if another node writes a profile change, this node may serve the stale value for up to `CACHE_TTL` seconds. For display names this is acceptable — a 60s stale name is visually imperceptible."
- `room_member_cache.rs:20-25` 同样承认："if membership changes on another node, this node may serve the stale list for up to `TTL` seconds."

但文档对这个 gap 的评估比现有注释更深入——现有注释认为 "display names 60s stale is acceptable"，但文档正确地指出**权限变更和 2FA 门控的 60s 窗口是安全缺口**，不仅仅是 UX 问题。

**一个补充建议**：方向一的 Phase 0（先于缓存抽象）应该是**立即可做、零成本的一致性模型文档化**——写一份 `docs/architecture/consistency-model.md`，明确声明：
- 最终一致性（默认）
- Read-Your-Writes（当前节点保证）
- 哪些操作要求强一致性（权限变更、2FA、角色变更）
- 跨节点默认最终一致

这不需要 NATS subject，不需要代码改动，只需要**花 2 小时写文档**。它是后面所有缓存工作的前提契约。

**风险补充**：NATS 本身是 at-least-once 投递，`cache.invalidate.*` 如果使用 ephemeral consumer，要注意节点重启期间产生的失效消息会丢失。建议使用带有 `max_deliver=1` 的 durable consumer（如果消息丢失意味着节点启动后的一段时间内可能有 stale 数据——其实 TTL 兜底了，所以可以接受）。

---

### 方向二（P1）——滥用检测 ⚠️ 体量估计准确，但缺少多节点竞态分析

**体量 L 估计正确**。这是一个跨 `trust/` 模块 + 修改 3+ 现有模块的方向。

**补充边界情况**：

1. **Redis 基干的故障谱系**：文档正确提到 `ip_reputation` Redis 不可达时 fail-open + 日志降级。但还需要考虑**部分故障**——Redis 集群中 `ip_reputation` 分片不可达但其他 Redis key 正常。代码需要区分 "Redis 全部不可达"（全局跳过）和 "特定 key 不可达"（per-key skip）。

2. **设备指纹的持久性与隐私窗口**：Canvas fingerprint + WebGL + Audio 在无痕模式下可能返回空值/噪音。建议 Phase A 使用 `navigator.userAgent` + `screen` 特征量的轻量级指纹（无需用户授权），Phase B 才引入 canvas/WebGL 指纹（需要 consent banner）。

3. **kill-switch 的级联效应**：`kill_switch:{action}` 写入 Redis 后，如果 Redis 集群自身脑裂（split-brain），不同的节点可能看到不同的 kill-switch 状态。解法：kill-switch 也写入 PG（作为权威来源），Redis 作为缓存 + 心跳检测。或者使用 NATS KV store（内置 RAFT 共识，适合这种低吞吐但高可靠的管理操作）。

4. **跨工作区关联的 GDPR 红线**：文档提到了 GDPR consent，但需要进一步：**欧盟工作区默认禁止跨工作区关联**，除非用户主动 opt-in。这个需要在 `workspaces` 表加 `cross_workspace_trust: boolean` 字段。

---

### 方向三（P2）——Schema 治理 ✅ 框架正确，但 CI 门禁设计需要细化

**一个关键的技术选择**：Rust 的 serde 反射能力有限——没有原生方式从 enum 生成 JSON Schema。文档提到 `typify`/`schemars`，这两个 crate 对简单结构有效，但对 `#[serde(tag = "kind")]` 的 `RoomEvent` 这样的一级标签联合体，生成的 schema 质量可能不够好。建议：

- **Phase A**：手写 JSON Schema 存储在 `schemas/room_event/v1.json` + CI 检查 enum 的 variant 数量是否变化（简单启发式：`rg "#[derive.*Deserialize.*Serialize\]"` 枚举的 variant count diff）
- **Phase B**：用 `schemars` + `#[derive(JsonSchema)]` 逐步自动化
- 不要一开始就追求自动化生成——手写 schema + CI diff 可以在 1-2 天内上线，而 `schemars` 对 tag=kind 的完美支持可能需要 1-2 周调试

**另一个被文档忽略的边界**：webhook 的 payload 版本协商。文档正确识别了 webhook payload 无版本字段，但 webhook 消费者如何声明它们支持的版本？建议使用 `webhook_subscriptions` 表的 `accept_version` 字段（缺省=latest 语义）。webhook dispatcher 在构造 payload 时根据 `accept_version` 选择字段集合。

---

### 方向四（P2）——编辑体验 ✅ 验证通过，但 Phase 排序需要调整

文档的 5 个 phase 方案很好，但排序有一个问题：

**Phase A（消息动作菜单）和 Phase B（斜杠命令面板）可以并行做**——因为它们涉及完全不重叠的代码路径。Phase A 改 `render.js`（消息渲染层），Phase B 改 `app.js`（输入框层）+ `commands.rs`（后端）。如果串行做会浪费并行机会，建议改为：

```
Week 1   Phase A (action menu) + Phase B (slash commands) — 并行
Week 2   Phase C (formatting toolbar) — 依赖 Phase B 的输入框改造
Week 3   Phase D (draft save) — 独立
Week 4   Phase E (inline preview) — 依赖 unfurl_cache
```

**补充一个缺失的场景**：消息编辑模式（`edit_message`）。当前用户发送消息后不能编辑。但文档只提到「右键菜单可以编辑」——编辑模式的输入框需要预填充原有消息块（`Block[]` → HTML/Markdown reverse conversion）。这个 reverse 转换（`render.js` 的 `renderMessage` 已有的正向转换的逆操作）目前不存在，是一个被低估的工程体量。

---

### 方向五（P2）——BFF 层 ⚠️ 方向正确，但提案偏重

文档的 4 个提案中，**第一项（`?fields=` 投影参数）可以单独实现且已经解决 80% 的 N+1 问题**。BFF 聚合端点（第二项）和 WS 轻量协议（第四项）是更重的工程。

建议的切入顺序：

1. **Phase A**（独占 S 体量）：`?fields=id,display_name,avatar` 投影——在现有 REST 路由上加 `#[serde(skip_serializing_if)]` + `Option<T>` 字段 + 全量默认值（向后兼容）。只需要改序列化层，不需要新路由。

2. **Phase B**（S-M 体量）：`Accept: application/vnd.aero.mobile+json` 内容协商——加一个 Axum middleware 读取 Accept header → 选择序列化策略。兼容现有客户端。

3. **Phase C**（M 体量）：BFF 聚合端点 `GET /api/bff/room-view/:id`——需要一个新路由，但复用现有 repo 查询。

**一个被文档遗漏的约束**：`?fields` 投影需要在响应序列化时动态跳过字段。当前的 `Participant` 结构体有 `#[derive(Serialize)]` 自动推导，不支持动态字段选择。实现方式有两种：
- 运行时：`#[serde(skip_serializing_if)]` + `Option<T>`（在序列化前把不需要的字段设为 `None`）
- 编译时：每个投影组合一个独立结构体

前者对现有代码侵入小，但意味着所有字段必须是 `Option<T>`（或 `#[serde(default)]`），会增加内存占用。建议 Phase A 只对**高频大字段**（`Participant.tz`、`Participant.pronouns`、`Participant.phone`）做投影——这些字段在当前 SPA 中本来也用不到。

---

## 跨方向依赖图（文档未分析的发现）

文档的「跨方向共识工程基线」正确提到不要同时进行方向一（缓存抽象）和方向四/五。但有一个**隐藏的依赖链条**：

```
方向三（Schema 注册表） → 方向五（BFF/投影）
```

为什么？方向二的 `Accept` 头内容协商和 `?fields` 投影本质上是一种"响应 schema 版本化"——服务端根据客户端的 `Accept` 头选择不同的响应 schema。如果方向三先建立了 schema 注册表（`GET /api/dev/schemas`），方向五可以直接引用这些 schema 来声明支持的投影视图。两者可以共享 `schema/` 目录。

建议的全局顺序：

```
Phase 1 (可并行):
└── 方向一 Phase 0（一致性文档，2h）
└── 方向三 Phase A（手写 schema + CI diff，2天）
└── 方向四 Phase A+B（消息动作菜单 + 斜杠命令，3天并行）

Phase 2:
└── 方向一 Phase 1（NATS 失效总线，1周）
└── 方向二 Phase A（IP 信誉 + CAPTCHA，1周）
└── 方向五 Phase A（fields 投影，2天）

Phase 3:
└── 方向二 Phase B（设备指纹 + 跨工作区关联，2周）
└── 方向四 Phase C+E（格式化工具栏 + 内联预览，1.5周）

Phase 4:
└── 方向一 Phase 2（统一缓存抽象，1.5周）
└── 方向五 Phase B（Accept 头内容协商，3天）
└── 方向三 Phase B（schemars 自动化，1周）
└── 方向四 Phase D（草稿持久化，2天）
```

---

## 最终优先级评分（修订版）

| 方向 | 优先级 | 与原文差异 | 原因 |
|------|--------|-----------|------|
| 一·缓存一致性 | **P1** | 一致 | 架构承诺的可交付性缺口；但 Phase 0（文档化）可 2h 完成，Phase 1（NATS 失效）是真正的 P1 |
| 二·滥用检测 | **P1** | 一致 | 同意 P1。但建议分 Phase A（IP 信誉 + CAPTCHA，纯防护）和 Phase B（设备指纹 + 跨工作区，风控） |
| 三·Schema 治理 | **P1→P2** | **微调** | 原文 P2，建议**从 P2 升为 P1**——不是因为当前的紧迫性，而是因为方向三如果不在第一批 bot 正式 launch 之前就位，后续升级成本是指数级的。破坏性变更是停服级别的事故 |
| 四·编辑体验 | **P2** | 一致 | P2 合理。Debug Client 对外测试前完成 Phase A+B |
| 五·BFF 层 | **P2** | 一致 | P2 合理。Phase A（`fields` 投影）可以提前到 P1.5——几乎没有风险，解决了 80% 问题 |

**修订后建议**：方向三升 P1，方向五 Phase A 单独列为 P1.5。

---

这个文档是现在为止**技术上最扎实的一轮扫描**——每个方向的边界情况分析都比前几轮更细致（失效风暴的去重、因果版本戳、设备指纹隐私授权、BFF 延迟计量等）。源码验证通过率 39/39 说明作者对代码库的理解已经非常深入。强烈建议优先为方向一写一致性模型文档（2h 零成本收益），然后进入 Phase 1 实现计划。
