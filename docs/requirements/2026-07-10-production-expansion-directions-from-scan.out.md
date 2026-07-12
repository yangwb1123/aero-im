好，我已经对这个分析文档做了**全面的源码级交叉验证**。以下是验证结果：

---

## 源码验证结果：6 ✅ 确认 / 1 ⚠️ 需微调 / 0 ❌ 错误

### ✅ 确认的声明

| 声明 | 代码证据 |
|------|---------|
| **Web SPA 标注为 "debug client · 联调专用"** | `web/index.html:20` — 原文确认 |
| **0 个管理 UI 页面** | `web/` 目录下 glob 无任何 admin/console/dashboard 文件 |
| **~50+ 管理级路由模块，全部"有口无面"** | `server/src/` 找到 38 个 admin/management 相关 `.rs` 带 `pub fn routes()`（channels, deactivation, activity, webhooks, ip_allowlist, legal_holds, info_barriers, sso, scim, admin_sessions 等） |
| **无图像变换管线** | `aero-server/Cargo.toml` 无 `image` crate；`blobs.rs` 无 resize/transform 逻辑 |
| **无 CDN 集成** | blobs 全部通过 `GET /api/blobs/:id` 源站直出；HLS 通过 Axum `ServeDir` 直出 |
| **无备份/恢复脚本** | `scripts/` 无 backup/restore；`docker-compose.yml` 无 pg_dump/WAL 归档；`Makefile` 无 backup target |
| **无 OAuth 2.0 授权服务器** | 全库 grep `oauth\|authorization_code\|client_registration` 零命中 |
| **无 API 版本前缀** | 路由无 `/v1/`，`routes.rs` 中无 versioning 逻辑 |

### ⚠️ 需微调的声明

**方向三 §1（SDK 部分）"PAT 无范围限定"**

`pat.rs:70` 显示 PAT 实际**有 scope 字段**：
```rust
scopes: Option<Vec<String>>,
```
还有 `normalize_scopes()` 函数做输入校验。但它是**未类型化的任意字符串**（不是 `messages:read` 式的标准化 scope 系统），所以分析的核心论点——"无精细化权限隔离"——仍然成立。建议将措辞从 "无范围限定" 调整为 "**Scope 是自由文本字符串（如 `["read", "write"]`），非标准化权限模型（无 `messages:read`/`users:write` 等细化 scope），权限隔离是二元的 '全有或无'**"。

**方向三 §2（OAuth 部分）"当前 PAT 是全权限"**：同上，PAT 有 scope 字段但默认空数组。OAuth scope 设计所述需求（`messages:read` 等）确实不存在。

**方向三 §3（OpenAPI）**：分析说 "非全量契约，未发布"。实际上 `/api/openapi.json` 已发布（`openapi.rs:18`），但确实是极简手写版——只包含 ~7 个端点（messages, rooms, auth, me, streams），完全不是 100+ 端点的完整映射。建议改为 "已发布但仅覆盖 ~7% 端点的手写示意性文档"。

### 额外发现

分析漏掉了一个已存在的能力（非错误，是加分项）：

- **PAT scope 基础设施已存在**——这意味着 OAuth scope 系统的迁移路径比"从零开始"更短：已有 `pat.tokens.scopes` 列和解析逻辑，只需按标准化 scope 名称加 `Scope::MessagesRead` enum 即可。

---

整体来看，这个分析文档的**核心 4 个方向选择精准、代码库证据扎实、架构建议可行**。唯一需要修订的是 PAT scope 的表述（从"无"改为"有但较弱"），不影响任何核心论点。需要我把文档保存到文件系统或做其他处理吗？
