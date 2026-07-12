文件已保存。以下是基于代码库验证的真实性审查报告。

---

## 真实性审查：4 项实质性事实错误需要修正

我在保存前对跨代码库的关键声明进行了验证。以下是需要修正的问题：

### 🔴 错误 1：安全响应头状态 — 文档说"全被注释掉"，实际已激活

**文档原文**：安全响应头「全被注释掉」（`serve.rs:55-59`）

**实际代码**（`crates/aero-server/src/bin/boot/serve.rs`，第 55-98 行）：

```rust
// Security response headers (ROADMAP 第四次分析·方向三):
//   X-Frame-Options DENY           → prevent clickjacking
//   X-Content-Type-Options nosniff  → prevent MIME-type sniffing
//   Strict-Transport-Security       → force HTTPS (production)
//   Referrer-Policy                 → reasonable privacy default
//   Permissions-Policy              → restrict sensitive browser APIs
.layer(SetResponseHeaderLayer::overriding(
    axum::http::header::X_FRAME_OPTIONS,
    axum::http::HeaderValue::from_static("DENY"),
))
// ... (全部 5 个头均已 active)
```

全部五个基础安全响应头均通过 `SetResponseHeaderLayer::overriding` **已激活**。写在注释旁边的 header 看起来像注释，但实际并非注释——`SetResponseHeaderLayer` 调用紧随其后。CSP 也可通过 `AERO_CSP_POLICY` 环境变量配置（opt-in）。

差距不在于这些头缺失——它们已经存在。真正的差距是 CSP 默认关闭和缺少 CSRF 保护。

**建议修复方向二**：删除 "全被注释掉" 的说法，改为准确描述——"基础五头已激活；CSP 为 opt-in 默认关闭；无 CSRF 保护"。

### 🟡 错误 2：表名 `message_history` 应为 `message_edits`

**文档**：引用了 `message_history` 表（迁移 0036/0157）

**实际**：
- 迁移 0036 创建的是 `message_edits` 表（文档存储编辑历史）
- 迁移 0157 在 `messages` 表添加 `version` 列（乐观锁）
- 表 `message_edits` 由 `aero-storage/src/message_edit.rs` 中的 `MessageEditRepo` 支持

该表确实用于相同目的，但名称有误。仅需简单修正。

### 🔴 错误 3：`av_scan.rs` + `content_sniff.rs` 不存在

**文档**：声称 `av_scan.rs`（ClamAV INSTREAM 协议客户端）和 `content_sniff.rs`（magic bytes + MIME 校验）已存在且标记为 "✅"

**实际**：整个代码库中**完全不存在**——无 rust 文件、无 SQL 迁移、无 Cargo.toml 依赖、无任何位置引用 ClamAV、magic bytes 或 MIME 嗅探。

这意味着：
- 方向四现状表格中的 "✅ 内容嗅探" 应为 "❌ 尚未实现"
- "✅ 病毒扫描（ClamAV INSTREAM）" 应为 "❌ 尚未实现"
- 文档声称这些已存在并可用，但它们**根本不存在**

### 🟡 错误 4：`mailer.rs` 不存在

**文档**：引用 `mailer.rs` 作为方向三的切入锚点

**实际**：代码库中不存在 `mailer.rs`。目前无邮件发送模块。通知流程可能直接基于模板构建，但无专门的 `mailer` 模块。

### ⚪ 次要：`innerHTML` 声明被夸大

文档称 "`app.js` 中 `innerHTML` 在多个地方使用"。Web 代码库整体是**安全的**——`render.js` 第 3 行注释为 "never via innerHTML"，使用 `textContent`。`notifications.js:71` 有一处 innerHTML 引用，但仅为一条注释说明 "builds DOM nodes only (no innerHTML)"。文档若引用的话应更精确。

---

## 已修正版本（我写入到文件中的内容）

我已将修正后的版本写入 `2026-07-11-round-31-global-scan-four-systematically-verified-high-value-extensions.md`，其中：
- 安全头表格将「❌ 全被注释掉」改为「⚠️ 五头已激活；CSP 为 opt-in；无 CSRF」
- 安全问卷将 header 状态更新为 ✅ active，保留 CSP/CSRF/PAT scope 作为真实差距
- 前一个文档误记为 **✅** 的地方已修正为内容嗅探/病毒扫描的 **❌**
- 表格中 `message_edits` 的表名已更正

方向的核心论点（消息事件溯源、CSP 覆盖、PAT scope 强制执行、CSRF 缺失、媒体管线、外部集成）**仍然成立**——基础事实检查中的这些错误削弱了看似覆盖严密的 audit trail 的可信度，但不会使整个分析失效。
