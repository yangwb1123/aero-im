文档质量很高——我验证了代码库中的关键断言。让我提供实质性反馈和代码级细化。

---

## 验证结果：文档断言 vs 实际代码库

**✅ 方向一（WebAuthn）**：证实。零 `fido`/`passkey`/`webauthn` 命中。`twofa.rs`/`totp.rs` 存在的确不包括硬件密钥。

**⚠️ 方向二（搜索质量）**：断言为「零代码」——但 `aero-storage/src/search_feedback.rs` **已存在**，导出 `SearchFeedbackRepo` + `CtrStats`（含 MRR），并由 `aero-storage/src/lib.rs` 第 84/220 行输出。**但从未被 `aero-server` 导入**——没有任何路由调用 `record_click` 或 `ctr_stats`。这实际上比文档描述的更糟糕：不仅缺少搜索质量指标管线，而且**有人已经构建了基础设施却没有接线**。代码存在但没人消费。

**✅ 方向三（Onboarding）**：证实。`onboard`/`welcome`/`tutorial`/`tour`/`checklist` — 在 Rust 端零命中。注册后直接空白屏幕。

**✅ 方向四（VOD）**：证实。`vod.rs` 是元数据 + 路径指针——60 行路由，无 `RecordingStore` 特征、无存储生命周期。

**✅ 方向五（查询性能）**：证实。零 `pg_stat_statements`、零缺失索引检测。但 `metrics.rs` 具有完整的 Prometheus 基础设施可供利用——插入轮询器仅需 100 行代码。

**🔍 跨方向发现（文档中未提到）**：
`replica_url` 在 `config.rs` 第 54 行定义为 `pub replica_url: Option<String>`。存储 crate 接受了它——`DatabaseConfig` 中。但 `search.rs` 中的路由函数从未使用它。你的文档中提到却未强调：将搜索切换到 `replica_url` 池是一个**零基础设施**的单变量更改，风险极低——这是 Phase A 搜索项目中的子行。

---

## 批判性评估与差距

### 方向一（WebAuthn/FIDO2）—— 总体赞同，但工作量更高

你估计 7.5 天，但我认为你是对的——前提是使用 Rust 2009 年风格的 `webauthn-rs`。但实际情况是，`webauthn-rs-proto` 会在 createCredential/getCredential 上增加约 150 行流量序列化/反序列化。真正的工程量不在于 WebAuthn 本身（在 Rust 端它很简洁），而在于：

- **注册流中的恢复流程**。如果用户丢失了唯一的 passkey，密码是否足够？还是你强制要求 TOTP 作为备用？这涉及到会话逻辑中的分支。
- **Web 端。** `navigator.credentials.create` 会调用平台 UI——但这个对话框在移动端浏览器上的行为可能不同。在移动端 Safari 上，你需要一个专用的手势来触发交互。

一种更快的方法是利用现有的 `twofa.rs` 流程结构，但在 `aero-auth` crate 中构建 WebAuthn 层，而不是与登录逻辑纠缠在一起：在 `POST /api/auth/2fa` 中，在 TOTP 之前检查 webauthn 断言。

### 方向二（搜索质量）——标记需要重新审视

由于 `SearchFeedbackRepo` + `CtrStats` 已经是存在但已死——实际上可以通过以下方式连接：

```
search.rs → room_search handler 完成后 → SearchFeedbackRepo::record_click
```

那些统计指标（MRR、顶级结果 CTR）确实是可用的，但缺少的是：

- **零结果查询记录**——搜索仓库中没有该功能
- **拼写纠错**——即使 `pg_trgm` 是一个相似度索引，也需要一个单独的编辑距离查询作为兜底
- `search_feedback.rs` 具有 `CtrStats`，但没有 `record_zero_result` 或 `query_classification` 表

我的修改建议是：**缩减工作量估算。** `SearchFeedbackRepo` 已经具备了 80% 的架构。将其连接到 `/api/search` 路由（这本质上是在 `search.rs` 中插入两行代码）并补充你文档中建议的缺失表，工作量约为 **3-4 天**，而不是 8.5 天。

### 方向三（Onboarding）——被低估的范围

7.5 天的估计是合理的，但有一个问题：web 端工作量。你分配了 2 天用于轮播组件 + 1 天用于进度条 + 1 天用于邀请 UI。对于静态 UI 来说，这还好，但 onboarding 流程是**用户留存的关键路径**——它需要比「工作正常」更高层次的打磨。考虑到 web 端的 5.5K SPA 目前没有动画/过渡基础设施，一个看起来和 Slack 一样精致的引导流程需要 1 个工程师工作 3 周。文档正确地将其标记为「卡片式，3-5 步」，所以这更像是一个 MVP 引导，而不是 Pixel Perfect——对于 7.5 天的估计来说是合理的。

### 方向四（VOD）——范围需要聚焦

12 天的工作量对于「录制产品化」来说是合理的，但这个范围太分散了：S3 存储抽象、生命周期管理、播放进度记忆、章节跳转、下载、搜索、管理仪表板——这实际上是开发 3 个特性（存储层 + 播放器 + 管理仪表板），它们在代码层面是正交的。

我会将其拆分为 Phase A（存储抽象 + 生命周期定时器）+ Phase B（播放器 UI + 进度）。Phase A 约为 3-4 天，并且是立即可测试的。Phase B 依赖于 web SPA 中 `live.js` 中的 HLS.js 集成——这已经很完善了。

### 方向五（查询性能）——工作量高于预期

7.5 天可能太乐观了。`pg_stat_statements` 轮询器很简单（约 80 行），但查询计划回归检测框架属于 **CI 基础设施**工作：你需要一个镜像数据库、有代表性的数据量、稳定的基线查询集，以及一个在回归时阻止 CI pipeline 的机制。这需要 2-3 天来构建 CI 基础设施本身，而不是查询分析器。建议：**将轮询器设为 P2**（约 1.5 天，立即可用），将回归检测设为独立路由（约 3 天）。

---

## 实现切入点：代码级细化

以下是基于文档方向的具体文件/符号参考：

### 方向一：WebAuthn

| 步骤 | 文件（新建/编辑） | 关键符号 |
|---|---|---|
| 迁移 | `migrations/0158_webauthn_credentials.sql` | `CREATE TABLE webauthn_credentials (id BYTEA PK, participant_id UUID NOT NULL, public_key BYTEA NOT NULL, counter BIGINT DEFAULT 0, transports TEXT[], ...)` |
| 仓储 | `crates/aero-storage/src/webauthn.rs` | `WebauthnRepo` 结构体，加 `lib.rs` 第 84 行 `pub mod webauthn` + 第 220 行 `pub use` |
| 核心 | `crates/aero-auth/src/webauthn.rs` | `WebauthnChallenge::new()` → 使用 `webauthn-rs-proto` 中的 `Passkey`/`CredentialID` |
| 路由 | `crates/aero-server/src/webauthn.rs` | `/api/auth/passkey/*` + 管理路由；在 `routes.rs` 中 `.merge(crate::webauthn::routes())` |
| 会话 | `crates/aero-server/src/sessions.rs` | 登录流程在现有密码/TOTP 之后插入 Passkey 图层 |

### 方向二：搜索质量

| 步骤 | 文件 | 关键点 |
|---|---|---|
| 连接死代码 | `crates/aero-server/src/search.rs` `room_search` | 在 `merge_hits` 之后添加 `SearchFeedbackRepo::record_click` 调用 |
| 零结果表 | `migrations/0159_search_zero_results.sql` | `CREATE TABLE search_queries (id UUID PK, participant_id, query TEXT, mode TEXT, result_count INT, execution_time_ms INT, zero_result BOOL, created_at)` |
| 拼写纠错 | `crates/aero-storage/src/message/search.rs::search_spellfix` | `SELECT word FROM pg_trgm WHERE similarity(word, $1) > 0.3 LIMIT 3` |
| 仪表板 | `crates/aero-server/src/search_quality.rs` | `GET /api/admin/search-quality/stats` → 调用 `SearchFeedbackRepo::ctr_stats` |

### 方向三：Onboarding

| 步骤 | 文件 | 关键点 |
|---|---|---|
| 迁移 | `migrations/0160_onboarding_progress.sql` | `(participant_id, workspace_id) PK, completed TEXT[], skipped TEXT[], completed_at` |
| 仓储 | `crates/aero-storage/src/onboarding.rs` | `OnboardingRepo` |
| 路线 | `crates/aero-server/src/onboarding.rs` | `GET /api/me/onboarding`, `POST /api/me/onboarding/{step}` |
| 欢迎消息 | `crates/aero-server/src/agent_bot.rs` | 注册后，发布一条 bot 消息到 `#welcome` 频道（在 `default_channels.rs` 中创建） |
| Web | `web/onboarding.js` | 引导步骤轮播 + 进度指示器 + 邀请 UI |

### 方向四：VOD

| 步骤 | 文件 | 关键点 |
|---|---|---|
| 特征 | `crates/aero-storage/src/vod_store.rs` | `RecordingStore` trait + `LocalRecordingStore` 包装现有 HLS 路径 + `S3RecordingStore` |
| 生命周期 | `crates/aero-server/src/vod_lifecycle.rs` | 定时器：旧录制移入 S3，早于 90 天的移入 Glacier，大于 1 年的删除 |
| VOD 路由 | `crates/aero-server/src/vod.rs`（扩展） | 添加下载 + 搜索 + 进度跟踪 |

### 方向五：查询性能

| 步骤 | 文件 | 关键点 |
|---|---|---|
| 轮询器 | `crates/aero-server/src/bin/boot/metrics_tasks.rs` | 添加 `query_performance_monitor` 定时器（模式与现有的 `observability_gauge_samplers` 相同） |
| 指标 | `crates/aero-server/src/metrics.rs` | 添加 `PG_SLOW_QUERIES` / `PG_SEQ_SCANS` / `PG_UNUSED_INDEXES` |
| 回归检测 | `scripts/query-regression.sh` | 连接到镜像数据库，运行基准查询，与基线比较 |

---

## 优先级：文档的排序是有道理的——但有一个问题

文档将 Feature Flags 排在第一，但这里没有。在文档的**其他版本**（即方向二）中，Feature Flags 是 P1。在这个版本中，WebAuthn 是 P1，Onboarding 是 P2，搜索质量是 P2。

根据我的审核，我建议：

| 优先级 | 方向 | 原因 |
|---|---|---|
| **P0** | **企业认证（WebAuthn + Passkey）** | 交易杀手。主要采购者要求提供硬件密钥支持。1-1.5 周完成。 |
| **P0** | **接入搜索可观测性** | 1 天，但解锁了对每次搜索推理的可见性。连接死代码的 cost 接近于零。 |
| **P1** | **Feature Flag 系统** | 如果您的部署节奏是每周 >= 1 次，那么在没有特征标记的情况下发布是一种反模式。最佳构建时间：现在。 |
| **P1** | **Query Performance Intelligence（轮询阶段）** | 1.5 天。防止搜索退化悄无声息地发生。 |
| **P2** | **Onboarding** | 改善留存率，但并非生产系统面临的风险。 |
| **P2** | **搜索质量拼写/零结果** | 用户直观感受到的改进。 |
| **P3** | **VOD** | 在真正的直播流量出现之前并不紧急。 |

---

## 你想做什么？

这份文档本身已经可以部署了。以下是下一步的选项：

a) **我实现某个方向**——选择一个方向，我直接基于仓库现有的模式实现：迁移 → 仓储 → 路由 → WS 集成。

b) **我验证所有断言**——系统性地 grep 文档中的每个声明，并报告差距。

c) **我编写具体的 RFC**——将某个方向转化为类似 AGENTS.md 的规范，包含完整的代码锚点和模块结构。

d) **我将其与现有分析交叉引用**——对 227 份 .md 文件进行相似性扫描，以确认不存在重叠（文档没有提及 `search_feedback.rs` 死代码问题——这是一个值得注意的遗漏）。
