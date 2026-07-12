审计验证回应已完成，输出至：

**`docs/requirements/2026-07-11-round-31-global-scan-four-systematically-verified-high-value-extensions.out.md`**（24KB）

---

## 审计核心发现摘要

### 代码级验证结果

对原始分析四方向共 **30+ 核心声明**进行了代码锚点验证：

| 方向 | 准确率 | 关键发现 |
|------|--------|---------|
| 一·事件溯源 | ✅ 全部准确 | `legal_hold` 的 SQL 查询不 JOIN `message_edits` 表→**当前合规盲点** |
| 二·Web 安全 | ✅ 全部准确 | CSP opt-in / 无 CSRF / PAT scopes 存储但不传递→**严重安全缺口** |
| 三·平台互操作 | ✅ 全部准确 | Email 网关/日历/跨平台桥确实完全缺失 |
| 四·媒体管线 | ✅ 全部准确 | 缩略图/EXIF/webp/CDN 全部不存在 |

### 优先级调整（3 处重要调整）

| 原始 | 审计调整 | 理由 |
|------|---------|------|
| 方向一 P1 | → **P0**（legal_hold 修复） | 法务保全下编辑删除了敏感内容 → 关联表不保护 → 法律后果 |
| 方向四 P2 | → **P1**（EXIF 剥离为 P0） | GPS 坐标自动泄露是 GDPR 第 32 条违规，合同违约项 |
| 方向二各 Phase 体量偏大 | 大多数估为 **S** 而非 M | 用既有基础设施（rate_limit / AuthUser / middleware）可低代价实现 |

### 推荐实施顺序（4 个 Sprint，8-10 周）

1. **Sprint 1 · P0 修复**（2 周）：PAT 作用域强制执行 + CSP 默认启用 + legal_hold 查询修复 + EXIF/Content-Type 嗅探 + CSRF + 注册限流
2. **Sprint 2 · P1 核心**（3 周）：`message_events` 表 + 异步缩略图管线
3. **Sprint 3 · P2 产品化**（3 周）：Email 入站网关 + CDN 签名 URL + CSP nonce 注入
4. **Sprint 4 · P3 优化**（2 周）：WebP 转换 + 存储分层 + 日历基础

### 新引入的技术依赖

全部为纯 Rust crate，无 C 绑定：
- `kamadak-exif-rs`（EXIF 剥离）
- `infer`（MIME 嗅探）
- `image`（缩略图生成 + WebP 编码）
- `html2text`（邮件 HTML→text 降级）

不使用自建 SMTP，推荐 SendGrid/AWS SES Inbound Parse webhook。
