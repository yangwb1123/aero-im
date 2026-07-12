以下是对你这份全量扫描分析的系统性回应。我已经交叉比对代码库验证了你的四大缺口的全部主张，以下是逐一的深入评估与补充。

---

## 总体评价

这份分析的质量非常高。所有核心主张都得到代码验证，既有覆盖表的诚实度值得称道。以下是我在验证过程中的发现和补充。

---

## 方向一（P1 · 账户恢复 UX 真空）

**验证结果：主张完全成立。后端投入远超前端。**

我阅读了 `crates/aero-server/src/sessions.rs` 和 `crates/aero-server/src/mailer.rs`，确认后端实现非常完整：

- `POST /api/auth/forgot-password` — token 生成 + 哈希存储 + SMTP 发送（无 SMTP 则日志降级）
- `POST /api/auth/reset-password` — token 验证 + 过期检查 + 密码策略 + 历史复用禁止 + 全部会话吊销
- `POST /api/auth/change-password` — 已有 API，用于「设置页修改密码」
- `migrations/0066_password_reset.sql` — `password_reset_tokens` 表
- `migrations/0074_password_history.sql` — 密码历史表
- `aero-storage/src/password_reset.rs` — `PasswordResetRepo`（create/consume/cleanup）
- `aero-storage/src/password_history.rs` — 历史记录禁止最近 N 个密码复用

前端验证确认为零：`web/index.html:25-35` 登录表单无「忘记密码」链接，`web/auth_ui.js` 无任何重置表单处理。

**补充一个你未提及但重要的细节：**

`sessions.rs` 中的 `forgot_password` handler 有一个防枚举的安全设计——无论邮箱是否存在都返回同样的消息体。这是一个正确但脆弱的实现：

```rust
// 即使参与者查找失败也返回 200，而非 404
Ok(Json(serde_json::json!({
    "message": "If that address is registered, a reset link has been sent."
})))
```

这意味着前端无需区分「邮箱不存在」和「邮件已发送」，只需显示统一的成功状态。前端实现可以利用这一点简化 UX 逻辑。

**另一个你未提及的路径**：`POST /api/auth/change-password`（`sessions.rs` 中已实现）可用于「设置页修改密码」场景，你的建议方向提及了 `PATCH /api/auth/password`，但实际上 `change-password` 已存在且功能完整（含会话吊销、密码历史校验）。前端只需对接。

### 切入路径微调

1. **登录表单**：在 `</form>` 前加一行 `<a href="#" id="link-forgot-password" class="auth-link">忘记密码？</a>` → `auth_ui.js` 监听 click → 显示邮箱输入表单
2. **重置页面**：需要一个「重置密码」视图，解析 URL hash 中的 `?token=xxx` → 显示新密码 + 确认密码表单 → POST `/api/auth/reset-password`
3. **设置页**：利用已有的 `change-password` API 添加「修改密码」界面

**不安全的边界**：`?token=` 通过 URL 参数传递存在日志泄露风险（反向代理日志、浏览历史）。生产部署应考虑 `POST` body 传递 token（先请求获取验证码/链接，再在新页面手动输入），或使用 `#token=`（hash 不被服务器日志记录）。

---

## 方向二（P1 · 事务性邮件基础设施）

**验证结果：主张完全成立。当前实现是最小可用产品，到生产级有 2-3 周差距。**

我完整阅读了 `mailer.rs`，确认：

- **仅 2 种邮件场景**：`send_password_reset` + `send_invitation`
- **纯文本**：`format!()` 构造 body，无 HTML 模板
- **同步发送**：HTTP handler 内 `.await` SMTP 往返
- **失败静默**：`warn!` 日志记录，无重试、无死信、无告警
- **单全局配置**：`build_mailer` 从 `AppConfig.email`（Option）构造

```rust
// 邮件失败仅记录 warn，HTTP handler 继续返回 200
Err(e) => warn!(to = to_addr, error = ?e, "failed to send password reset email"),
```

**一个你未提及但重要的代码发现**：`AppState.mailer` 是 `Option<crate::mailer::Mailer>`，没有 SMTP 配置时完全静默降级到日志输出。这意味着邮件发送是**可选的附加通道**，而非核心送达机制。这个设计决定有其道理（开发环境不需要 SMTP），但延伸到生产环境意味着：

- 即使配置了 SMTP，邮箱 MX 临时故障 → 邮件静默丢失
- 没有任何告警机制通知运维「邮件通道已断」

**关于你的场景扩展列表，我补充一个在你的「合规」象限但未列出的场景：**

| 场景 | 合规要求 | 当前状态 |
|------|---------|---------|
| GDPR 数据导出完成通知 | Art. 15 right of access | 无邮件通知 |
| 账户删除确认 | Art. 17 right to erasure | 无邮件通知 |
| 安全事件通知（新设备登录） | SOC2 CC7 | 无 |
| 密码变更确认（非请求者） | SOC2 CC6 | 无（当前仅会话吊销） |

**架构建议的优先级重排：**

你的建议方向是：
1. 作业队列
2. HTML 模板
3. 通知偏好
4. 退订机制
5. 批量通道

我会建议：

1. **通知偏好**（优先于队列）——没有偏好门控就发邮件是垃圾邮件。`notif_prefs.rs` 已支持 `channel` 维度扩展，只需加 `email` channel。此步也可解决后续所有场景不需要发邮件的问题。

2. **作业队列**——`mpsc` + 后台 drain loop（复用 `ai_usage_ledger_drain` 模式）。这一步也是后续重试/死信的基础。

3. **退订机制**（合规必做）——`List-Unsubscribe` 头 + 退订页面，不能等。

4. **HTML 模板**——可使用 `sqlx` 编译期模板的思路，创建 `templates/email/` 目录，用 `minijinja` 渲染。

5. **批量通道**——可以放最后，digests 和公告优先级低。

---

## 方向三（P0 · 前端测试黑洞）

**验证结果：主张完全成立。5,939 行 JS，零测试。**

`web/package.json` 确认无测试框架依赖、无 `test` script、无 `test/` 或 `spec/` 目录。`node_modules/` 下的 `test/` 目录全部是第三方包的测试，与应用无关。

**我想对你风险矩阵中 5 个 Bug 场景做一些补充和修正：**

### 场景 1: SeqGate 溢出（`ws.js:13`）

```
SEQ_RECENT_CAP = 256
直播弹幕 50 msgs/s → 5.12s 填满窗口
```

这个场景的关键漏洞不在于「重复帧漏过」——SeqGate 的设计目的是去重 **at-least-once 重投**导致的重复，而非高吞吐下的顺序保证。直播弹幕的 ephemeral consumer 本质允许丢帧（文档 §1 写 "丢几条弹幕无碍"）。真正的风险在于：

- 重连回溯时 `_lastSeen` 在 SeqGate 窗口外的 seq 会被误判为重复
- 混合场景：IM 消息 + 直播弹幕共用同一 WS 连接时的 seq 空间竞争

测试应覆盖：SeqGate 满窗口后，seq 跳跃回窗口内的行为。

### 场景 2: 重连回溯全局光标不均匀

这个场景比你说的更严重——不仅是丢失房间 B 的 2 条消息。`ws.js` 的重连逻辑会问 `?since=<seq>`，但这个 `seq` 是**全局单调 seq**（来自 `bus/seq.rs`），而不同房间的 seq 空间是独立的。重连时用跨房间的全局 seq 会导致：

- 房间 A seq 500，房间 B seq 2
- 全局 `_lastSeen` = 500
- 重连时 `?since=500` → 房间 B 的所有新消息都丢了

这是**架构级 bug**（如果我的理解正确的话），应该按房间存储 `_lastSeen`。

### 场景 5: 多 tab 同步

这其实是前端测试无法覆盖的跨窗口问题——`BroadcastChannel` API 的天然用例。当前代码无多 tab 同步机制，E2E 测试（Playwright 多页面）是唯一能验证的方式。

### 测试分层优先级

我建议微调你的优先级：

1. **L1 纯函数**：`render.js`（685 行）和 `context.js`（208 行）的工具函数——最易覆盖，ROI 最高
2. **L2 API 客户端**：`api.js`（336 行）的令牌生命周期、并发队列——无 DOM 依赖，纯 async 测试
3. **L3 WS 核心**：`ws.js`（260 行）的连接/重连/回溯/SeqGate——需要 mock WebSocket
4. **L4 E2E**：全链路消息收发 + 通话——Playwright
5. **L5 通话**：`calls.js`（639 行）——需要 mock RTCPeerConnection，特殊处理

**工具链选型同意 `vitest`**——与 `eslint` 兼容、原生 ESM、高性能。`@testing-library/dom` 对于无框架的纯 DOM 操作是正确选择。

---

## 方向四（P0 · API 版本化）

**验证结果：主张完全成立。零版本化基础设施。**

我验证了：

- `routes/routes.rs`：38 条 `.route("/api/..."` 全部无版本前缀
- WS `ServerFrame`：无版本字段
- Webhook payload：无 `schema_version` 字段
- 无 `Accept-Version` 头处理、无 `Deprecation`/`Sunset` 响应头

**我想给一个更务实的 Phase 1 方案：**

你建议的 Phase 1 包含 `/api/v1/` 别名路由共享 handler。我认为这**过早引入了 URL 前缀方案**，而 URL 前缀对 WS 和 Webhook 不适用，会造成三种 API 面有三种版本化策略。

更统一的 Phase 1 应该是：

### Phase 1（2 天）：可观测 + 合约测试

1. **添加 `VersioningLayer` 中间件**（而不是改全部路由）：
   - 读取 `Accept: application/vnd.aero.v1+json` 头
   - 当前阶段：只记录日志 + 设置响应头 `X-Api-Version: 1`
   - 无路由分发变化
   - 未来 `v2` 才真正路由到不同 handler

2. **添加弃用响应头**：
   - 对已经稳定的端点：`Deprecation: false`
   - 为未来准备：支持 `Deprecation: true; sunset="..."`

3. **添加兼容性合约测试**（`cargo test --test api_compat`）：
   - 对每个公开端点记录 JSON Schema
   - 添加测试验证响应结构不会意外变化
   - 使用 `serde_json::Value` 泛化反序列化

### 为什么先做可观测和测试

- URL 前缀模式要求**全部路由同时迁移**（因为 WS 不能前缀，Webhook 不能前缀）
- 当前阶段最重要的是**知道哪些端点在变化、不要让它们意外变化**
- 企业客户当前的准入障碍是「我签 6 个月 SLA，你们能不能不做 breaking change」——合约测试直接解决这个问题

### 关于 WS 版本协商

WS 的 serde `#[serde(deny_unknown_fields)]` 是核心风险点——如果某个 `ServerFrame` variant 用了这个属性（而不是默认忽略未知字段），添加新字段就是 breaking change。检查所有 WS 帧的 serde 配置应该纳入 Phase 1。

---

## 额外发现：第 5 个方向（跨领域）

在验证你的分析过程中，我发现一个跨领域的问题——**可观测性盲区**：

### 日志分级与告警缺失

```
// mailer.rs — 邮件失败
warn!(to = to_addr, error = ?e, "failed to send password reset email");
// → 无指标、无告警、无计数器
```

```
// sessions.rs — token 存储失败
tracing::warn!("failed to store reset token for {email}: {e}");
// → 用户得到 "邮件已发送" 但 token 没存进去——永久的死胡同
```

```
// AppState.mailer — 可选
pub mailer: Option<crate::mailer::Mailer>,
// → 无 SMTP 配置时，整个邮件通道静默消失
```

**核心问题**：静默失败（fail-open）模式在所有路径中都有意为之（防枚举、防 DoS），但**对应的运维可观测性为零**。这意味着生产运行时：

- SMTP 挂了 → 用户无法重置密码 → 没有告警触发
- Token 存储失败 → 用户永远收不到重置链接 → 没有错误计数
- 无 SMTP 配置（忘记配置）→ 系统正常运行但所有邮件功能不可用 → 无配置检查

这应该被追踪为 P1 的**基础设施债务**。

---

## 总结：执行计划调整建议

| 你的优先级 | 我的调整 | 理由 |
|-----------|---------|------|
| **P1 方向一**（0.5 天） | ✅ **保留，立即执行** | ROI 极高，一个下午的事 |
| **P1 方向二**（2-3 周） | ⚠️ 拆分为 P1+P3：通知偏好+队列为 P1，模板+退订为 P2，批量通道为 P3 | 前两步解锁所有后续场景 |
| **P0 方向三**（1 周初始） | ✅ **保留，短期执行** | 质量根基，但建议从 `render.js` 纯函数开始（1 天出成果） |
| **P0 方向四**（3 天初始） | ⚠️ 改为可观测优先（2 天），URL 前缀放后期 | 统一策略优于提前选型 |
| **新增**：可观测性盲区 | **P1**，0.5 天 | 邮件失败告警 + token 存储失败计数器 + SMTP 配置检查 |

准备好在任意方向上深入实现细节。接下来你希望走哪个方向？
