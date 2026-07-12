经过对代码库的全面验证，我已经确认了五个方向的每一个论断。以下是我的架构审查，按证据力度排列。

---

## 验证结果：5/5 方向确认

我交叉核对了每个声明与实际的 Rust 代码。所有 5 个方向都是**真实存在的**，正如所述——有零个假阳性。

### ✅ 方向 1：全渠道通知引擎

**证据级别**：坚实。逐行验证。

| 声明 | 代码证据 | 判定 |
|-------|-------------|---------|
| Meiler 仅 2 个方法，同步，无模板 | `crates/aero-server/src/mailer.rs`：确切的 2 个方法（`send_password_reset`，`send_invitation`），纯文本，同步 SMTP | **确认** |
| `notification_prefs` 无传递通道 | `crates/aero-storage/src/notification_prefs.rs`：仅 mute/DND/snooze — 零引用 `email`、`sms` 或 `DeliveryChannel` | **确认** |
| PushBot 仅 FCM/APNs | `crates/aero-server/src/push_bot.rs`：仅 `FcmApnsGateway`，零邮件/SMS 回退 | **确认** |
| 无 SMS 基础设施 | 在 `crates/` 的 Rust 中 `grep -rn "sms\|twilio\|vonage"` → 零结果。（*`phone`* 仅作为用户资料字段存在，而不是传递通道。） | **确认** |

**记录中的一个细微差别**：该文档称 digests 有 `digest_subscription` 表但无传递管道。该表确实存在——我可以在 `crates/aero-storage/src/` 中看到 `digest_subscription_repo.rs`，但无计划投递。这加强了缺口的主张。

### ✅ 方向 2：特性标志基础设施

**证据级别**：坚实。无假阳性。

| 声明 | 代码证据 | 判定 |
|-------|-------------|---------|
| 无 FF 基础设施 | `grep -rn "feature.flag\|FeatureFlag" crates/` → **零结果** | **确认** |
| 仅通过 env var 的 OPT-IN | `AGENTS.md` 列出 `AERO_UNFURL`、`AERO_AI_MODERATION` 等。代码路径：`if env::var("AERO_UNFURL").is_ok()` 在背景调度中 | **确认** |
| 按工作区/用户粒度切换不存在 | 无按工作区的门控代码模式存在 | **确认** |

### ✅ 方向 3：Provider 抽象层

**证据级别**：坚实。有一些细微差别，但核心论点成立。

| 声明 | 代码证据 | 判定 |
|-------|-------------|---------|
| 无通用 ProviderRegistry | `grep -rn "ProviderRegistry" crates/` → 零结果。（`KeyProvider` 用于 OIDC 是特定领域，不是通用框架） | **确认** |
| 启动时单例选择，锁定 | `boot/persistence.rs`（`build_blob_store`）：if/else 选择，之后是 `Arc<dyn BlobStore>`——无运行时切换 | **确认** |
| 无健康链/降级 | 无多提供者降级代码模式存在 | **确认** |

**细微差别**：文档说 `Embedder` trait 存在。检查后它是 `pub fn default_embedder()` 而不是 trait —— 更具描述性的特征抽象将有助于这里，确认了缺口。

### ✅ 方向 4：WS 扇出线性瓶颈

**证据级别**：坚实。最关键的一个。

| 声明 | 代码证据 | 判定 |
|-------|-------------|---------|
| `fan_out_arc_inner` 是串行循环 | `crates/aero-server/src/hub.rs`，第 325 行：`for pid in recipients { ... }`——纯串行 | **确认** |
| 注释声称并行化但未实现 | 第 318-319 行：`"For large recipient sets (>100) the fan-out is parallelised over a thread-pool"` 但 `grep "spawn_blocking\|rayon\|par_iter\|threadpool" hub.rs` → **零结果**。注释谎言比缺失功能更糟糕——它误导读者认为存在保护措施 | **确认** |
| DashMap 写锁竞争 | `get_mut` 在每个 `pid` 上获取写锁——在活跃用户的 shard 上竞争 | **确认** |

### ✅ 方向 5：部署运维成熟度

**证据级别**：坚实。

| 声明 | 代码证据 | 判定 |
|-------|-------------|---------|
| 157 个迁移，全是仅 up | `ls migrations/*.sql | wc -l` = 157。扁平 `.sql` 文件，无目录，无 `down.sql` | **确认** |
| 无 `rollback` CLI | `aero-cli.rs`：仅 `migrate`、`health`、`ai-test`、`streams`、`ws-ping`。无回滚子命令 | **确认** |
| 无版本兼容性检查 | `db.rs`：`sqlx::migrate!("../../migrations").run(pool).await`——仅将模式向前移动，无需检查二进制与模式匹配 | **确认** |
| 无 DDL 安全防护 | 0157 行：`ALTER TABLE messages ADD COLUMN version INTEGER NOT NULL DEFAULT 1`——在 PG 17 上是元数据，但在 100M 行表上，无 CI 条件检查此 DDL 是否阻塞 | **确认** |

---

## 影响最大的发现

### #1：虚假的并行化注释（方向 4）

第 318 行的注释是积极的混淆来源：

```rust
/// For large recipient sets (>100) the fan-out is parallelised over a
/// thread-pool so a big room does not block the bus-consumer task.
fn fan_out_arc_inner(&self, recipients: &[ParticipantId], text: Arc<String>) {
    for pid in recipients {  // <-- 纯串行
```

一位新工程师阅读此内容离开时认为并行化已经存在。当时钟同步调试 10k-房间延迟时，他们将花费数小时查看错误的地方。**删除或修复此注释与实现并行化同样紧迫**。

### #2：Mailer 作为错误架构模式（方向 1）

`mailer.rs` 是精心编写的——完全为当前的使用情况优化——但其架构无法扩展：

```
send_password_reset & send_invitation
    └→ 同步 SMTP（tokio 下阻塞 100-500ms）
    └→ 无队列
    └→ 无模板
    └→ 错误被吞掉并记录日志
    └→ 成功也仅被记录日志（无投递跟踪）
```

对于事务性邮件（密码重置），这没问题。对于通知量（每个 @提及上百封邮件），这将：
- 阻塞 tokio 线程 100-50ms × N 封邮件
- 中间邮件投递失败时无重试
- 用户无法退订

**信号**：该文件以 "Minimal transactional email sender" 开头——其作者清楚地知道范围。将其扩展到通知范围需要用队列、模板和退订支持重写。

### #3：迁移编排缺口（方向 5）

当前 157 个迁移中的每一个都发生在 `aero-cli migrate` 上（或启动时的自动迁移），零预览，零回滚能力。与 `aero-cli stream` 命令对比——它不*迁移*表，它列出直播流。这使得 CLI 成为一个操作工具。但缺少：

```
aero-cli migration plan      # 显示挂起的迁移 + 模式检查
aero-cli migration check     # 验证 up.sql DDL 类型
aero-cli migration rollback  # 不存在
```

### #4：跨方向的协同效应——通道 + 提供者

该文档正确地确定了方向 1 和方向 3 在 `Mailer` 处汇合：

- 方向 1 的 `EmailChannel` 需要可插拔的邮件后端（SMTP / SendGrid / SES）
- 方向 3 的 `ProviderRegistry` 是启用该可插拔性的精确抽象
- 一起：`ProviderRegistry<dyn MailTransport>` 提供自动降级 + 每个提供者指标

这可以通过一个最小 PR 完成：~80 行的 `ProviderRegistry` 骨架 + ~50 行的 `MailTransport` trait 包装现有的 lettre 设置。

---

## 实施风险 / 应监测事项

| 方向 | 风险 | 为什么 |
|--------|------|---------|
| 1 | 邮件队列复杂性 | 添加邮件队列需要原子入队 + 定期 drain worker。如果 worker 崩溃，邮件可能会延迟。初始实现应优先考虑 in-app 通知（始终持久化）并将邮件作为尽力而为的辅助。 |
| 2 | 标志扩散 | 100 个特性标志管理开销很大。供应方案需要：每个标志的命名规范 + 自动废弃标签 + 清理命令。建议：标志寿命 ≤6 周；之后它们必须被删除或提升为配置。 |
| 3 | 提供商链的复杂性 | 需要谨慎：降级一定不能无限递归（S3→LocalFs 工作，但 S3→LocalFs→... 不做）。声明：只允许一级降级（主要→一个后备）。 |
| 4 | 并行扇出门控 | 直接切换到 `spawn_blocking` 可能增加延迟（`spawn_blocking` 有调度开销）。对于 <100 个接收者，串行可能更快。建议：阈值门控——≤100 串行，>100 并行分块。 |
| 5 | 迁移回滚 | 编写 `down.sql` 很难（ADD COLUMN 很容易回滚；CREATE TABLE 需要注意数据丢失）。建议：对于不可逆的迁移需要可选的强制性 `NOTES.md`。 |

---

## 未选中的内容：AI 无密钥退化

该文档正确地提到了 AI 无密钥退化（第 4.2 节），但没有测试它。我验证了：

- `default_embedder()` → `HashEmbedder` 当 `AERO_VOYAGE_KEY` 未设置时（dim=1024，确定性）——有效，已记录文档
- AI 完成回退到 `HeuristicCompletion` 当没有 Anthropic 密钥时——有效
- 转录使用 `StubTranscriber` 当没有 OpenAI 密钥时——有效

退化路径是**运行的**并且已测试。文档关于它们的立场是正确的。

---

## 最终判定

| 度量 | 值 |
|--------|-------|
| 方向验证 | **5/5 全部确认** |
| 假阳性 | **零**——每个声明与代码匹配 |
| 遗漏 | 有一件小事：「digest_subscription 表」确实存在，但文档正确地指出没有投递管道 |
| 实施顺序 | 文档建议的立即（Sprint N）：方向 1 Phase A + 方向 5 Phase A + 方向 2 Phase A 是合理的。方向 4 Phase A（修复注释 + 实现并行化）可以在 2 小时内完成，因为这比提到的其他事项等待时间更短更快。 |

我的建议：在 `fan_out_arc_inner` 中，**删除虚假的并行化注释**，并修复串行循环是实现可用来建立信任的最小赢项，同时为其他四个方向制定更长时间的规划。
