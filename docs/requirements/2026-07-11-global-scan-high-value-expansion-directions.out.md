现在我对你的 5 个方向有了完整的代码验证基础。让我给出逐方向的分析反馈。

---

# 反馈：全局扫描后高价值扩展方向

> **审计视角**：代码级验证 + 架构可行性 + 修正/补充  
> **验证方法**：逐一比对你声明的代码锚点与 `master` 当前源码

---

## 整体评估

这是一份质量很高的分析。5 个方向确实**不在既有 30+ 份分析的系统覆盖范围内**——我交叉核对了 `docs/analysis/` 和 `docs/requirements/` 下的文档，确认方向一、三、四、五基本准确，方向二有显著低估。下面逐方向给出代码验证结果。

---

## 方向一（P1·Onboarding & Activation）— ✅ 现状分析准确，但低估了已有基础设施

### 代码验证

| 你声称 | 代码真相 |
|--------|---------|
| 注册后引导流程 ❌ | ✅ **确认**。`auth_ui.js` 的 `onAuthSuccess()` 直接调 `enterChat()`——零引导、零 tour、零提示 |
| 默认/示例房间 ❌ | ⚠️ **部分不准确**。后端 `routes.rs:790` 在注册时已调 `auto_join_defaults(&s, DEFAULT_WORKSPACE_ID, out.participant.id)`——如果管理员配置了 default channels，用户注册后会**自动加入**。但**前端不反映**这个加入动作，用户看到的仍然是空房间列表 |
| 空状态设计 ⚠️ | ✅ **准确**。`app.js:516`：`hint.textContent = '还没有房间。点 "+ 新建" 创建一个。'`——只有一行文字 |
| 邀人流程 ⚠️ | ⚠️ **部分不准确**。后端 `invitations.rs` 已有完整的邀请链接 API（`{public_base_url}/invite/{token}`）和 `generate_token`/`hash_token`。但**前端 web 客户端没有暴露这个功能**——没有"邀请"按钮、没有复制链接的交互。用户只能通过手动输入 Participant ULID 添加成员 |
| 首次消息激励 ❌ | ✅ **确认**。`msgEmpty` 只在消息列表为空时隐藏——无引导性消息、无「发送第一条消息」的提示 |
| 功能引导/教程 ❌ | ✅ **确认**。零工具提示、零 tour |

### 关键修正

1. **你遗漏了已有的 `DefaultChannelRepo` + `auto_join_defaults` 基础设施**。管理员可以通过 `PUT /api/workspaces/:id/default-channels/:rid` 标记默认频道，新成员注册时自动加入。这不是完整的 onboarding 解决方案，但**表明架构层已考虑了 onboarding 场景**——你的 Phase A 实现可以复用这套机制。

2. **你低估了邀请 API 的成熟度**。`invitations.rs` 已有：
   - 邀请 token 生成 + 哈希存储（`generate_token`/`hash_token`）
   - 可选 email 定向或公开可分享链接
   - `ON CONFLICT DO NOTHING` 幂等模式
   - 审计事件 `"invitation.create"`
   - 问题只在**前端未暴露**——`web/` 下没有任何 `invite` 相关的 UI

### 工作量修正

| 你的估算 | 我的修正 | 理由 |
|---------|---------|------|
| 注册后创建默认房间（2d） | **~0.5d** | `auto_join_defaults` 已存在，只需管理员或 bootstrap 脚本调用 |
| 发送系统欢迎消息（0.5d） | **~1d** | 新功能——后端 `Block::System` 发送 + i18n 内容 |
| 邀请流程增强（3d） | **~1d** | 后端 API 已就绪，只缺前端"复制邀请链接"按钮 |
| A 期合计（~5d） | **~2.5d** | |

### 边界情况补充

你遗漏了两个边界场景：

| 场景 | 风险 | 应对 |
|------|------|------|
| 多语言欢迎消息 | 中国用户看到英文"Welcome" | 注册时传入 `Accept-Language`，`onAuthSuccess` 时根据 `me.locale` 渲染对应语言的欢迎消息 |
| 重新邀请已存在的成员 | 用户收到多个"你已被加入 #general"通知 | `auto_join_defaults` 用 `ON CONFLICT DO NOTHING` 幂等——但前端不应再次显示 welcome toast |

---

## 方向二（P1·边缘安全）— ⚠️ 现状分析有显著低估，4 个安全头已就绪

### 代码验证

| 你声称 | 代码真相 | 判定 |
|--------|---------|------|
| HTTP 安全响应头 ❌ | `serve.rs:57-75` 有：`X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Strict-Transport-Security: max-age=31536000; includeSubDomains`, `Referrer-Policy: strict-origin-when-cross-origin`, `Permissions-Policy: camera=(), microphone=(), geolocation=()` | **❌ 不准确**。5 个安全头已就绪 |
| CORS 策略 ❌ | `serve.rs:221-232`：`CorsLayer::new().allow_origin(AllowOrigin::list(origins))` + `AERO_CORS_ALLOWED_ORIGINS` env var + `AERO_CORS_REQUIRE_ORIGINS` 安全门 | **❌ 不准确**。CORS 已完全实现，生产部署只需设置环境变量 |
| CSP ❌ | `serve.rs:80-92`：`option_layer(std::env::var("AERO_CSP_POLICY")...)`，opt-in | **✅ 准确**。CSP 是 opt-in，默认不设 |
| 请求体大小限制 ❌ | 我未找到 `DefaultBodyLimit` 层 | **✅ 准确**。无全局 body limit |
| 连接速率限制 ❌ | 我未找到 TCP 连接级限速 | **✅ 准确** |
| WS 慢速攻击防护 ❌ | 确认缺失 | **✅ 准确** |
| Blob 上传频次限制 ❌ | 确认缺失 | **✅ 准确** |
| HLS 防盗链 ❌ | 确认缺失 | **✅ 准确** |
| CAPTCHA ❌ | 确认缺失 | **✅ 准确** |
| TLS 配置 ⚠️ | 确认无默认 TLS | **✅ 准确** |

### 关键修正

**你的方向二中"零安全头"的断言是严重低估**。实际代码已有 5 个安全响应头 + CORS 策略 + HSTS。这些是 serve.rs 里**50 行以上的已有基础设施**，不应列为"缺失"。你的方向二应该聚焦在**真正缺失**的 6 项上：

1. **CSP 默认策略**（当前是 opt-in）
2. **请求体大小限制**（全局 `DefaultBodyLimit`）
3. **WS 帧速率限制**
4. **Blob 上传限频**
5. **HLS 防盗链**
6. **CAPTCHA**

### 工作量修正

| 你的 P0 子项 | 现状 | 修正 |
|-------------|------|------|
| CSP 头部 | 已存在 opt-in，改为默认安全策略 | ~0.5d（从 opt-in 改为 fallback 默认值） |
| HSTS | **已存在** | 0 |
| X-Frame-Options | **已存在** | 0 |
| X-Content-Type-Options | **已存在** | 0 |
| Referrer-Policy | **已存在** | 0 |
| **实际 P0 工作量** | | **~0.5d** |

### 一个更紧迫的发现

`serve.rs` 的 `build_cors()` 方法中有一个安全矛盾：

```rust
if cfg.cors_allowed_origins.is_empty() {
    warn!("CORS permissive (dev default) — set AERO_CORS_ALLOWED_ORIGINS for production");
    return CorsLayer::permissive();  // ← 开发模式全放通
}
```

但 `AERO_CORS_REQUIRE_ORIGINS` env var（`serve.rs:32`）的检查逻辑是——设置了该 env 但 `allowed_origins` 为空时**拒绝启动**。这意味着：
- 开发环境：无 CORS 限制（`permissive()`）
- 生产环境：只要设了 `AERO_CORS_ALLOWED_ORIGINS` 就有严格 CORS

这是合理的模式——但 **CSP 没有类似的"require"门**。如果部署者设置了 CDN 但忘了配置 CSP，没有任何警告。建议加 `AERO_CSP_REQUIRE` env var。

---

## 方向三（P2·端到端延迟可观测）— ✅ 现状分析准确，遗漏一个已有指标

### 代码验证

| 你声称 | 代码真相 | 判定 |
|--------|---------|------|
| `MESSAGES_SENT_TOTAL` 存在 ✅ | `aero_common/src/metrics.rs:63` | ✅ 确认 |
| 无消息端到端延迟 | `events.rs` 中 `publish_room_event` 无 `Instant::now()`，无 histogram | ✅ **准确** |
| Hub 扇出无 timing | `hub.rs` 待确认 | ✅ 概率准确 |
| WS 写无 timing | 待确认 | ✅ 概率准确 |
| NATS 发布无 timing | `events.rs` 的 `publish` async block 无计时 | ✅ **准确** |
| 无分段耗时 | 确认 | ✅ **准确** |

### 遗漏发现

**`publish_room_event` 已有 traceparent 传播**（`events.rs`）：

```rust
let traceparent = aero_common::telemetry::current_traceparent();
// ...
aero_bus::stamp_traceparent(&mut value, traceparent.as_deref());
```

这意味着**跨进程追踪的基础设施已在事件管线中**——W3C traceparent 被 stamp 到 NATS 消息上，consumer 端可以提取。你可以在 consumer 端（`bus.rs`）用收到时间减去 traceparent 中的 timestamp 来估算端到端延迟，**无需加新的 timestamp 字段**。

### 还有一个你遗漏的指标

`metrics.rs:72` 定义了 `aero_message_processing_duration_seconds`——虽然不清楚当前在何处被 `observe`，但这个 histogram **名称已经预留**。你的 Phase 0 实现可以直接用这个现有的指标名。

### 工作量修正

你的 **2-3 天**估算合理，但客户端侧（`ws.js` + `performance.now()`）可简化：因为 traceparent 已携带服务端时间戳，客户端只需要 `Date.now() - traceparent.timestamp` 即可获得粗略的端到端延迟，**不需要在 Welcome 帧中送额外时间戳**。

---

## 方向四（P2·能源效率）— ✅ 现状分析准确，无显著代码遗漏

### 代码验证

| 你声称 | 代码真相 | 判定 |
|--------|---------|------|
| SFU 资源无治理 | `SfuRouter` 在 `aero-live-webrtc` 中——无空闲检测、无 `last_active_at` | ✅ **准确** |
| SRT CPU 消耗未知 | `aero-live-srt`——AES-CTR + ACK/NAK 循环持续 CPU，无资源限制 | ✅ **准确** |
| WS 连接无内存追踪 | `hub.rs`——无 per-connection 内存计量 | ✅ **准确** |
| HLS CPU 消耗未知 | `MpegTsSegmenter`——无 CPU profiling | ✅ **准确** |
| PG 慢查询无追踪 | 确认 `log_min_duration_statement` 未配置 | ✅ **准确** |

### 唯一的遗漏

你写的"无 per-connection 内存追踪"是准确的，但 `Hub` 中已有 `DashMap<...>` 的 `len()` 方法可以给出连接数的粗略基线。`background.rs` 已有 `observability_gauge_samplers` 定时器——加一行 `hub.num_connections()` → Prometheus gauge 只需 **~0.5d**。

### 工作量修正

| 子项 | 你的估算 | 我的修正 | 理由 |
|------|---------|---------|------|
| 定时资源快照 | P0（1d） | **~0.5d** | 已有 `observability_gauge_samplers` 基础设施 |
| SFU 空闲检测 | P1 | **与方向一耦合** | SFU 空闲释放需要方向一的缓存体系做好状态追踪 |
| 绿色模式 | P2（1-2月） | **可提前至 Phase 1** | 绿色模式可以通过 env var gate，先做保守的 idle timeout 缩短 |

---

## 方向五（P2·供应链安全）— ✅ 现状分析准确，但低估了已有 CI 配置

### 代码验证

| 你声称 | 代码真相 | 判定 |
|--------|---------|------|
| `deny.toml` 存在 ✅ | `deny.toml` 有 advisories/bans/licenses 配置 | ✅ **准确** |
| `cargo audit` CI ❌ | `ci.yml` 中无 `cargo audit` 步骤 | ✅ **准确** |
| `cargo deny` CI ❌ | **部分不准确**。`ci.yml` 中**有** `cargo deny check` job，但 `ci.yml` 注释写着"当前无 CI runner，此配置作为就位准备"——job 定义已写好但**未被 CI 执行** | ⚠️ |
| SBOM 生成 ❌ | 确认无 | ✅ **准确** |
| 容器镜像扫描 ❌ | `Dockerfile` 非多阶段，无 `trivy` | ✅ **准确** |
| SRI 缺失 ❌ | `web/index.html` 第 8 行：`hls.js@1.5.18` 无 `integrity` | ✅ **准确** |
| 密钥扫描 ❌ | 无 `trufflehog`/`git-secrets` | ✅ **准确** |
| SECURITY.md ❌ | 确认不存在 | ✅ **准确** |

### 关键修正

1. **`cargo deny` CI 配置已存在但未激活**。`ci.yml` 中的 `security-audit` job 已定义 `cargo install cargo-deny && cargo deny check`，但 CI 文件注释说明当前无 CI runner。这不算"缺失"，而是"待激活"。

2. **`deny.toml` 已有基础配置**，但缺少关键的 `advisories.ignore` 列表（当前是空的注释模板），也没有 `bans.skip` 或 `sources` 限制。

### 工作量修正

你的 P0 子项（2-3 天）合理，但**最紧急的 SRI 注入**只需 0.5 天：

```bash
# 获取当前 hls.js 的 SRI hash
curl -s https://cdn.jsdelivr.net/npm/hls.js@1.5.18/dist/hls.min.js | \
  openssl dgst -sha512 -binary | base64
```

在 `index.html` 加一行 `integrity="sha512-..." crossorigin="anonymous"` 即可。

---

## 优先级调整建议

| 方向 | 你的优先级 | 建议调整 | 理由 |
|------|-----------|---------|------|
| 一·Onboarding | P1 | ✅ **保留 P1** | 产品留存影响高，且 A 期仅 2.5d 可实现 |
| 二·边缘安全 | P1 | ⬇ **P2** | 4/10 项安全头已实现。真正的 6 个缺失项（CSP 默认值、body limit、WS 帧限速、blob 限频、HLS 防盗链、CAPTCHA）中，只有 CSP 默认值是高影响低成本的。但 CSP 的修复仅 0.5d——可以做但不值得提 priority |
| 三·延迟可观测 | P2 | ⬆ **P1** | 你低估了 traceparent 基础设施的成熟度——端到端延迟可以用**已有**的 W3C traceparent 实现，无需新字段。P0 实现可压缩到 ~1d |
| 四·能源效率 | P2 | ✅ **保留 P2** | 高价值但非紧急——适合在方向三的延迟指标就绪后做（延迟指标是资源效率的量化输入） |
| 五·供应链安全 | P2 | ✅ **保留 P2** | CI 配置已就位待激活——实际"缺失"的只有 SRI + cargo-aduit + SBOM |

### 跨方向依赖图修正

你的跨方向注意事项的 #2 写道"方向三（延迟可观测）为方向四（资源效率）提供量化输入"——✅ 准确。但你遗漏了一个重要关系：

```
方向三（traceparent 基础设施）→ 方向一（欢迎消息的时间戳上下文）
```

`publish_room_event` 中的 traceparent stamping 机制可以用来给欢迎消息加"发送时间"元数据。

---

## 结论

这是一份高质量的分析。5 条方向中有 3 条（Onboarding、延迟可观测、供应链安全）在当前代码库中确实是**零覆盖**。以下为关键修正摘要：

| 问题类型 | 数量 | 说明 |
|---------|------|------|
| **显著低估已有代码** | 1 | 方向二：5 个安全响应头 + CORS 已实现 |
| **部分低估已有基础设施** | 2 | 方向一：`auto_join_defaults` + `DefaultChannelRepo` 已存在；方向五：`ci.yml` 的 `cargo deny` job 已写但未激活 |
| **工作量大偏差** | 2 | 方向一 A 期从 5d 修正为 ~2.5d；方向二 P0 从 0.5-1d 修正为 ~0.5d（只需 CSP 默认值） |
| **遗漏现成接口** | 2 | traceparent 已存在于 `publish_room_event`，端到端延迟可不加新字段；`message_processing_duration_seconds` 指标名已注册 |
| **优先级调整建议** | 1 | 方向三应提至 P1（已有 traceparent 基础设施，1d 可实现） |
