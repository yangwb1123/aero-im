所有代码证据已验证完毕。以下是对这份文档的逐条验证和完整反馈。

---

## 验证结果

### 方向一（SSRF）— ✅ 确认，评估准确

| 声明 | 代码证据 | 验证结果 |
|------|---------|---------|
| `assert_webhook_url_safe` 存在 | `webhooks.rs:235`，在创建时 (L302) 调用 | ✅ 通过 |
| `ReqwestUnfurler::fetch` 缺 SSRF 检查 | `unfurl.rs:395` — 纯 `self.client.get(url).send().await.ok()?` | ✅ 通过 |
| Client builder 只有 timeout + UA | `unfurl.rs:366-372` — 无 redirect 限制、无 IP 过滤 | ✅ 通过 |

**补充发现**：`metrics_tasks.rs:91` 的 NATS consumer backlog 监控只追踪 2 个消费者（`IM_MESSAGES`/`aero-server` 和 `AI_QUEUE`/`aero-ai`），但 9 个 bot consumer 的积压完全不可见 —— 这与方向五有交叉。

**微小修正**：「特定场景白名单」建议中的 `AERO_UNFURL_ALLOW_PRIVATE` env 应预置文档说明其安全影响，避免运维在不知情下开启。

### 方向二（CI/CD）— ✅ 确认，评估准确

| 声明 | 代码证据 | 验证结果 |
|------|---------|---------|
| CI 文件是注释模板 | `ci.yml` 头部注释明确说明 | ✅ 通过 |
| 无 runner 接入 | 注释提到「当前无 CI runner」 | ✅ 通过 |
| 50+ 脚本无自动化 | `scripts/` 下有丰富脚本，但无任何自动触发 | ✅ 通过 |

**补充发现**：`ci.yml` 中的 `check`/`test`/`size-check`/`truth-check`/`web-check`/`dependency-check` jobs 代码完整但全部被注释状态。`integration-test` 和 `coverage` 也是模板。这是一个**只需取消注释 + 配置 runner 即可激活**的场景——风险低回报高。

### 方向三（导出截断）— ✅ 确认，评估准确

| 声明 | 代码证据 | 验证结果 |
|------|---------|---------|
| `EXPORT_CAP = 5000` | `conversation_export.rs:42` | ✅ 通过 |
| 响应无 `total_count` | 响应只有 `{room_id, count, truncated, messages}` (L119-126)——`count` = `messages.len()`，不是房间总消息数 | ✅ 通过 |
| 无 cursor 续传 | 单次请求，无 `before` 参数暴露给 API 调用者 | ✅ 通过 |

**微小纠正**：文档说「1821 个 `#[tokio::test]`」单元测试。实际计数：`#[tokio::test]` = 415，`#[test]` = 1406，合计 1821。文档合计数正确但具体标签描述略有偏差——不影响论点。

### 方向四（前端令牌安全）— ✅ 确认，评估准确

| 声明 | 代码证据 | 验证结果 |
|------|---------|---------|
| `localStorage` 存储 token | `web/api.js:10-21` 全部使用 `localStorage.getItem/setItem` | ✅ 通过 |
| CSP 默认不启用 | `serve.rs:88` — `std::env::var("AERO_CSP_POLICY").ok().map(...)` | ✅ 通过 |
| 无 httpOnly cookie 支持 | `grep -rn "Set-Cookie\|HttpOnly" crates/aero-server/src/` 无匹配 | ✅ 通过 |
| hls.js 从 CDN 加载 | `web/index.html:8` — `cdn.jsdelivr.net` | ✅ 通过 |

**重要补充**：`render.js:2-3` 注释声称「never via innerHTML」，但实际代码中仍有 `innerHTML` 用于静态骨架模板——文档关于 XSS 入口的担忧是合理的，因为： (a) 骨架模板的 `innerHTML` 若被注入仍可能绕过， (b) 无 CSP 意味着任何第三方脚本（CDN 被攻陷、浏览器扩展）都可以执行。

**建议补充**：httpOnly cookie 方案的 WebSocket 适配细节值得更多讨论——`ws.js` 的帧认证方式需要调整，当前 WS 连接是通过消息帧认证而非连接时 Cookie。

### 方向五（消费者监督）— ✅ 确认，评估准确

| 声明 | 代码证据 | 验证结果 |
|------|---------|---------|
| 9 个消费者独立 spawn | `background.rs` — 每个 bot/dispatcher 均有独立 `tracker.spawn` | ✅ 通过 |
| 无统一注册中心 | 无 `ConsumerRegistry`、无健康 API | ✅ 通过 |
| NATS backlog 监控不完整 | `metrics_tasks.rs:91` 只追踪 2 个 stream/consumer | ✅ 通过 |
| 无限重试空转 | `bus.rs:37`/`bus.rs:201` 的 `loop { subscribe→err→sleep(1s)→retry }` | ✅ 通过 |

**深化建议**：文档的 watchdog 建议（连续 N 条失败后重启）需谨慎实现——目前 `handle_room_event_sub` 的单条失败已通过 `warn!` 日志记录但不中断消费者循环。真正的「消费者挂死」模式是消费者仍在运行但因逻辑错误（如 payload 格式变更）导致每条消息都不做有效工作。这需要：
- 消息级成功/失败计数独立于连接级健康
- 熔断器模式而非简单重启

---

## 综合评估

| 维度 | 结论 |
|------|------|
| **证据准确性** | 🟢 全部核心声明经代码验证为真。文档的代码引用（行号、函数名、模块路径）完全对齐实际源码。 |
| **影响评估** | 🟢 P0（安全/工程基础设施）和 P1（数据合规/前端安全/可观测）分级合理。SSRF 和 CI/CD 确实是当前最大工程断层。 |
| **建议可行性** | 🟢 建议方向具体、体量估计合理、与既有架构模式一致（如复用 `assert_webhook_url_safe`、对齐 `me_export.rs` 模式、扩展既有 `metrics_tasks.rs` 定时器）。 |
| **遗漏点** | 🟡 方向二（CI/CD）的 Phase C 冒烟测试需注意 `docker-compose` 的依赖镜像拉取时间——CI 分钟消耗可能远超预期。方向四的 httpOnly cookie 方案需要说明退 localStorage 仍用于 PAT/WS——可以更清晰描述混合策略。 |
| **与既有分析的差异化** | 🟢 本文区别于 `docs/requirements/` 中 80+ 份文档（多数是「加功能」方向）——它聚焦代码中**可验证的工程断层**，这是独特的价值定位。 |

**总评**：这份分析质量很高。代码证据链完整、影响评估客观、建议方向与既有架构一致。五个方向的 P0/P1 分级正确，其中方向一（SSRF）和方向二（CI/CD）在**投入产出比**上最值得立即行动——前者约 30 行代码关闭一个安全审计一票否决项，后者只需取消注释 + 配置 runner 即可激活全套自动化管线。唯一的技术性纠正：`#[tokio::test]` 计为 415（非 1821），但合计数 1821 正确（含 `#[test]`）。不要因这个微末错误影响对整体质量的判断。
