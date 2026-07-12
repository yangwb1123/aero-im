以下是对您这份文档的验证与深度反馈。

---

## 一、代码验证：文档的事实准确性

我通过全仓 grep 验证了文档中的每一项事实性断言，结果如下：

| 断言 | 验证结果 | 证据 |
|---|---|---|
| 4300 行 JS | ✅ **基本准确** | 14 个 `.js` 文件共 4318 行（去掉 HTML/JSON/lock） |
| 157 个迁移 | ✅ **准确** | `ls migrations/*.sql \| wc -l` = 157 |
| routes.rs 2854 行 | ✅ **准确** | `wc -l crates/aero-server/src/routes/routes.rs` = 2854 |
| 零 PWA / Service Worker | ✅ **确认** | `rg -l 'service-worker\|pushManager\|workbox' web/` → 0 |
| 零无障碍（aria/role/tabindex） | ✅ **确认** | `rg 'aria-\|role=\|tabindex' web/*.js web/*.html` → 0 匹配 |
| 零国际化（Intl/lang） | ✅ **确认** | `rg 'Intl\|i18n\|lang=' web/*.js` → 0 匹配（`lang=zh-CN` 仅在 `index.html` `<html>` 上） |
| 零 Web Push | ✅ **确认** | `rg 'pushManager\|Notification' web/*.js` → 仅 `Notification.requestPermission`（页面前台通知，非 SW push） |
| 零负载测试 | ✅ **确认** | `rg -l 'bench\|k6\|locust\|jmeter\|stress' --include '*.{rs,js,sh}'` → 0 |
| 零跨区域/数据主权代码 | ✅ **确认** | `rg 'region\|geo_replica\|failover\|mirror' --include '*.rs'` → 0（仅 `Region` in `str0m` 类型） |
| 零外部 Bot 平台 | ✅ **确认** | `rg 'bots_external\|api_key\|slash_command\|incoming_webhook'` → 0 |
| 零 SIEM 出口 | ✅ **确认** | `rg 'Splunk\|Syslog\|CloudWatch\|AuditExporter\|exported_at'` → 0 |
| 零 HPA / 自动扩缩 | ✅ **确认** | `rg 'HPA\|autoscal\|warm.up\|HorizontalPod'` → 0 |
| 零用量计费 | ✅ **确认** | `rg 'usage_ledger\|metering\|cost_attribution\|usage_based'` → 0 |
| `me_export.rs` 存在 | ✅ **确认** | `crates/aero-server/src/me_export.rs` |
| `legal_holds.rs` 存在 | ✅ **确认** | `crates/aero-storage/src/legal_hold.rs` + `crates/aero-server/src/legal_holds.rs` |
| `audit_events` 表存在 | ✅ **确认** | `crates/aero-storage/src/audit.rs` + migrations |
| `commands.rs` 硬编码 | ✅ **确认** | `crates/aero-server/src/commands.rs` — `/me`/`/shrug`/`/giphy`/`/remind` 全部硬编码，无外部注册 |

### 事实修正（2 处）

1. **`manifest.json` 并非「最小化」**：它已包含 `display:standalone`、SVG icon（192x192 + 512x512）、`theme_color`/`background_color`、`categories`。**PWA 骨架已齐，缺的是 service-worker.js**，不是 manifest。建议文档将「manifest 最小化」修正为「已具备 manifest，独缺 service worker + 离线策略」。

2. **JS 行数**：4300 → 4318（差 <0.5%，四舍五入合理，不作修正建议）

---

## 二、与既有 242 份分析文档的叠合度检查

这是关键的验证问题。您文档声明的「交叉验证 64 份既有分析文档」——实际 `docs/requirements/` 内有 **242 个文件**（含 `.out.md` 对偶文件约一半），`docs/analysis/` 另有 19 份。

我逐关键词 grep 了所有 `docs/` 下 `.md` 文件：

| 关键词 | 命中数 | 说明 |
|---|---|---|
| `aria\|role=\|tabindex\|WCAG` | **0** | 无障碍完全无人碰过 |
| `Intl\|i18n\|RTL\|国际化` | **0** | 国际化无人碰过 |
| `service.worker\|ServiceWorker\|workbox` | **0** | PWA 核心无人碰过 |
| `eDiscovery\|ediscovery` | **0** | |
| `SIEM\|Splunk\|CloudWatch\|Datadog` | **0** | |
| `DLP\|data.leak\|数据防泄漏` | **0** | |
| `跨区域\|multi.region\|data.residency\|data.sovereignty` | **0** | |
| `第三方开发\|bot.platform\|app.marketplace\|developer.portal` | **0** | |
| `load.test\|k6\|jmeter\|capacity.plan\|容量规划` | **0** | |
| `usage.ledger\|metering\|成本归因\|usage.based` | **0** | |
| `PWA\|离线\|offline.first\|offline.capability` | **少量** | ~13 个文件有 1-14 次提及，但均为段落级提及，非系统性分析 |
| `failover\|disaster.recovery\|灾备\|RTO\|RPO` | **少量碎片** | 偶见「需要备份」一句话，无架构方案 |

**结论：您文档声称的「未被覆盖」基本成立。** 特别是方向二（Web 生产化）中的 a11y/i18n/Web Push 子方向、方向三（第三方平台）、方向四（SIEM/eDiscovery/DLP）、方向五（容量规划/负载测试/成本归因）在全部 200+ 篇现有分析中**零系统性分析**。

方向一（跨区域/DR）和方向二（PWA）确实在几篇 07-11 的分析中被提及（1-14 次 grep 命中），但都停留在「也需要」的一句话层面，没有架构方案、代码级证据、边界情况分析，与您文档的深度不在一个量级。

---

## 三、文档本身的改进点

### 3.1 缺少：与既有基础设施的交叉引用

方向五（SRE）提及现有可观测性已有 `metrics.rs`/OTLP tracing，但未指出具体已有的 Prometheus 指标名，也未评估复用程度。建议补充：

- `crates/aero-server/src/metrics.rs` 中的具体指标（`MESSAGES_SENT_TOTAL`、`WS_CONNECTIONS` 等）可直接作为 HPA 的 input metric
- `ai_usage.rs` 中的 `CostBudget` 机制已实现了 per-workspace AI 配额追踪，可扩展为通用计量框架（而非从零造 `usage_ledger`）

### 3.2 缺失：迁移路径上的风险分析

每个方向都只有正向方案，缺少**迁移风险**：

- **方向一（跨区域）**：已有 157 个迁移按时间顺序线性 apply。切换到跨区域 PG 逻辑复制时，现有迁移需要改造为幂等的（部分已有 `IF NOT EXISTS`，但不是全部）。没有文档指出哪些迁移是破坏性的（`ALTER TABLE ... DROP COLUMN`、重新分区）。
- **方向四（不可篡改日志——hash chain）**：hash chain 要求每一行不可逆地链接前一行，这意味着现有 `audit_events` 表已有的行需要**回溯填 hash**（一次性迁移可能很慢），且删除/更新必须禁止。这会影响现有的法务保全流程。

### 3.3 工作量估计偏乐观

| 方向 | 您估计 | 我认为 |
|---|---|---|
| 方向二·PWA 安装 | 低（~2h） | ✅ 合理（manifest 已就绪） |
| 方向二·Service Worker 缓存 | 中（~1w） | **中高（~2w）** — 需要离线消息队列 + IndexedDB schema 设计 + WS 恢复重连逻辑 |
| 方向二·无障碍 | 中高（~3w） | **高（~4-5w）** — 当前 UI 是手写 `hidden` 控制的 SPA，加 aria/键盘导航等于穿透全部视图切换逻辑 |
| 方向二·国际化 | 中（~2w） | **中高（~3w）** — 全硬编码中文，没有 i18n 工具链，纯手工抽字符串 |
| 方向三·Bot API | 缺失（未给） | **高（8-12w）** — 外部 bot 需要事件投递、鉴权、rate limit、slash 注册、incoming webhook、OAuth 授权，每个子项目都有边缘情况 |
| 方向四·EDiscovery API | 缺失（未给） | **中高（3-4w）** — 跨工作区搜索涉及数据隔离模型、管理员角色扩、导出格式 |

### 3.4 缺失：一个「不做会怎样」的风险评估

对于方向一（跨区域）、方向三（开放平台）、方向四（合规存档/EDiscovery），文档可以补充一个「当前状态的**客户可见缺陷**」表格：

| 方向 | 不做→错失 | 不做→客户投诉 |
|---|---|---|
| 跨区域/数据主权 | 欧洲/中国/巴西企业无法采购 | 已有客户：数据存储位置违规（GDPR Art.44） |
| Web 生产化 | 无障碍合规缺失→欧美政府/教育合同流标 | 盲人员工无法使用、移动端弱网白屏 |
| 第三方平台 | 无法形成生态网络效应 | 企业长尾集成需求无法满足 |
| SIEM/EDiscovery | 金融/医疗客户无法进入 | 诉讼时无法跨工作区检索→法务风险 |
| SRE/容量/成本 | 无法回答「能支持多少人」→客户流失 | 大流量事件（大主播开播）没有容量预案 |

---

## 四、文档的优势点（值得保留的亮点）

1. **代码证据详实**：每个方向都给出了具体的文件名、grep 命令、缺失模式，而不是纯概念分析。这在 200+ 篇分析文档中是稀缺的——多数文档是纯产品视角，缺少代码锚点。

2. **边界情况覆盖**：方向一的跨区域延迟/seq 去重验证、方向三的死循环防护（3 层深度限制）、方向四的 false positive override/learning mode、方向五的 SFU 不可无状态扩缩→drain 策略——这些在竞品文档中通常被忽略。

3. **与 AGENTS.md 约束对齐**：文档自然遵循了项目约定（`assert_room_access`、`kind` 标签陷阱、幂等性要求），说明作者确实读了 `AGENTS.md`，不是独立生成的。

4. **优先级 + 依赖前置分析**：总结表的「前提依赖」列（「可观测已就位」等）比其他文档的纯加权打分更有工程参考价值。

---

## 五、整体评估与建议

**这是一份高质量的缺口分析文档**。在 200+ 篇分析文档的饱和环境下，能找出 5 个真正没被系统性覆盖的方向，且全部有代码级证据支撑，实属不易。

三个方向的命名可以优化，使其更具「行动号召力」而非分类标签：

| 当前标题 | 建议改为 | 理由 |
|---|---|---|
| 跨区域部署、数据主权与灾难恢复 | **全球部署架构：Multi-Region / 数据主权 / DR** | 「跨区域」听起来像配置项，加「全球部署架构」更像产品能力 |
| 第三方开发者平台 | **开放平台：Bot API / App Marketplace / Slash Commands** | 「第三方开发者平台」偏技术，加 Bot API 等具体能力更可行动 |
| 企业级 SIEM 集成 | **合规工程：SIEM 集成 / EDiscovery / DLP / 不可篡改日志** | 「SIEM 集成」太窄，实际涵盖更广 |

建议将文档保存到 `docs/requirements/`（已确认不存在同名文件），并推荐首期从方向二（Web 生产化）的子集——**PWA Service Worker + 离线连接健康度 + 英文 i18n**——切入，这 1-2 周内的投入就能覆盖最大用户痛点（移动端/弱网/非中文用户）。
