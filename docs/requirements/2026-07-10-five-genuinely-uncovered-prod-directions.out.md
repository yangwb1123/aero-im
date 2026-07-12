至此代码交叉验证完成。以下是我的验证结论。

---

## 交叉验证报告：逐方向评估声明准确性

### 方向一（客户端离线韧性）—— 基本准确，区分有效

**缺口真实存在**：
- ✅ Service Worker 注册为零：`web/` 无任何 `navigator.serviceWorker.register` 调用
- ✅ `ws.js:169-175` send() 在 `readyState !== OPEN` 时返回 false，调用方静默丢弃
- ✅ 无缓存策略：`serve.rs` 无 `Cache-Control` 头，静态资源每次全量从网络加载
- ✅ `manifest.json` 存在但未被引用
- ✅ 重连后仅恢复消息（`?since=`），房间列表/通话状态/直播订阅全部丢失

**"已有覆盖边界"区分的有效性**：VALID。既有分析中：
- `five-truly-uncovered-client-side-directions.md` 方向一关注**页面刷新的状态持久化**（内存→localStorage/IndexedDB）
- 本文方向一关注**完全无网络时的操作可用性**（SW cache + offline queue）

两者解决不同问题——一个解决 F5 恢复，一个解决断网可用。不重叠。

**修正建议**：离线/Service Worker 在多个既有分析中被**提及**（`five-truly-uncovered-high-value-directions.md:72`、`prod-scale-perspective.md:16` 标为 "✅ 分析覆盖"），但仅是**边缘提及**，不是系统性论证。建议在表注中加一句说明。

---

### 方向二（Web 无障碍 a11y）—— ❌ **已有覆盖被遗漏，区分不成立**

**方向本身有价值，但"无系统性论证"的声明不成立**。

`2026-07-09-truly-uncovered-gaps.md` **已经有一整节 dedicated 方向二**（`## 方向二（P1）· 客户端全局可访问性（a11y）缺口`），篇幅约 40 行，包含：

| 检查点 | 现有分析覆盖 | 您的文档覆盖 |
|--------|-------------|-------------|
| ARIA 属性覆盖率 | ✅ 零 ARIA (grep count=0) | ✅ 3 处（含 `render.js`） |
| aria-live / aria-label | ✅ 动态区域缺 `aria-live="polite"` | ✅ 同 |
| 焦点管理（模态框 trap） | ✅ | ✅ 同 |
| 键盘导航 | ✅ | ✅ 同 |
| 色彩对比度 (.muted #888) | ✅ WCAG AA 4.5:1 未达标 | ✅ 同 |
| 合规要求（ADA/508/EN 301 549） | ✅ | ✅ 同 |
| 竞品对标（Slack/Teams/Discord） | ✅ | ❌ 未提 |
| P0/P1/P2 分层修复方案 | ✅ | ❌ 无分层方案 |

**您的"已有覆盖边界"表将方向二与「Web 安全头硬化」对比**，但最相邻的既有分析是 a11y 分析本身——而不是安全头。这个遗漏使得"无系统性论证"的核心声明不成立。

**建议**：
1. 承认已有 a11y 分析的存在
2. 将分界从"安全头 vs a11y"改为"您的方向二作为已有 a11y 分析的增强/不同视角"
3. 具体差异点可以是：您的分析聚焦于**具体代码行**和**声明的精确度**（如 `aria-live` 的 `aria-relevant` 属性），而既有分析更偏**分层修复体量评估**

---

### 方向三（前端供应链安全）—— ❌ **核心声明错误，高度重叠**

**`production-engineering-directions.md` 方向一已系统性地覆盖了全部要项**：

| 您的方向三点 | 既有分析覆盖 | 状态 |
|------------|-------------|------|
| CDN 加载无 `integrity` (SRI) | ✅ SRI 缺失识别，给出修复方案（P1 级别，~20 行 HTML + CI 检查） | **完全重叠** |
| CSP 默认未启用 | ✅ CSP 环境变量驱动、默认不发送、修复建议（P0 默认基线策略） | **完全重叠** |
| npm audit 未运行 | ✅ CI 依赖漏洞扫描被注释，修复建议（P1 取消 `cargo deny` 注释 + 增加 npm audit） | **完全重叠** |
| CDN jsdelivr 中文可达性 | ❌ 未提及 | **您的独特贡献** |
| hls.js 自托管策略 | ❌ 未提及 | **您的独特贡献** |

**您的"已有覆盖边界"表将方向三与「Web 安全头硬化」对比，但 `production-engineering-directions.md` 方向一不限于安全头——它就是一个完整的 Web 安全硬化分析，包含 CSP、SRI、CORS、SAML、依赖扫描、OpenAPI。** 方向三的 4 个要点中，3 个已被直接覆盖。

**建议**：
1. 承认该方向已被系统性覆盖
2. 如要保留方向三，应聚焦于**您的独特贡献**：CDN 中文可达性分析和 hls.js 自托管策略
3. 或将这两个独特点作为已有分析的补充章节，而非独立的 5 方向之一

---

### 方向四（数据库 Schema 演进运营手册）—— 基本准确，验证成立

| 检查点 | 代码验证 | 您的声明 |
|--------|---------|---------|
| ADD COLUMN 频率 | ✅ 61 次，>10 对 `messages` | 准确 |
| CREATE INDEX CONCURRENTLY 使用 | ✅ **零次**（所有 `CREATE INDEX` 未用 `CONCURRENTLY`） | 准确 |
| 0 停机策略/DOWN 迁移 | ✅ 无 `DOWN`，回滚注释仅 10 次 | 准确 |
| 迁移命名一致性 | ✅ 有 `_p10_`/`_p2_` 等前缀但无文档化规范 | 准确 |
| 大表操作性能基准 | ✅ 无 EXPLAIN ANALYZE 覆盖 | 准确 |
| 迁移粒度（一件事/迁移） | ✅ 148 单做分区影子表，145 单做 partial index | 部分准确 — 多数迁移是单目的，但有混用 |

**与既有分析的区分**：VALID。`deep-architecture-chasm.md` 和 `runbooks/messages-partitioning.md` 关注**分区这个特定项目**的步骤顺序，而您的方向四关注**全生命周期治理规范**（回滚、零停机、命名规范、backfill 策略）。

**修正建议**：`CREATE INDEX CONCURRENTLY` 对小型应用（测试/开发环境）确实非必需。建议区分环境——对小表普通 `CREATE INDEX` 足够，只对预期千万级以上的表用 `CONCURRENTLY`。

---

### 方向五（可观测驱动的性能工程管线）—— 部分准确，量化

| 您的声明 | 代码验证 | 评估 |
|---------|---------|------|
| "无端到端延迟观测" | ✅ `bus.rs:48-52` 有 `traceparent` 注入和 span 继承——**基础已建**，但未聚合为端到端 P50/P95/P99 指标 | **部分正确**：追踪骨架有，但 SLO/bookkeeping 缺失 |
| "无 per-endpoint SQL 延迟" | ✅ `metrics.rs:300` 有 `HTTP_REQUEST_DURATION_SECONDS` per-route 直方图——但标记的是请求级别，不是 SQL 查询级别 | **部分正确**：HTTP 延迟有，SQL 查询延迟无 |
| "WS 丢帧不可观测" | ✅ `hub.rs:341-375` `lossy` AtomicBool 状态存在但**未暴露为 Prometheus 指标**（仅 tracing::debug） | **准确** |
| "无客户端 RUM" | ✅ `app.js`/`render.js` 零 `performance` API 调用 | **准确** |
| "无 CI 基准测试" | ✅ `Makefile` 无 `criterion`/`bench` 目标 | **准确** |

**"已有覆盖边界"表的区分**：VALID。已有分析覆盖**基础设施监控**（OTLP/Prometheus/告警），您的方向五覆盖**从指标到行动的闭环**（SLO burn-rate、per-query 回归、RUM）。

**修正建议**：`bus.rs` 的 W3C traceparent 传播已经建好——不应说"完全无"。应改为"追踪骨架已建，但无端到端延迟聚合和 SLO bookkeeping"。

---

## 综合评估

| 方向 | 声明验证 |
|------|---------|
| **① 客户端离线韧性** | ✅ **准确**。缺口真实，与既有分析区分成立 |
| **② Web 无障碍合规** | ❌ **声明不成立**。既有 `truly-uncovered-gaps.md` 已系统性覆盖，遗漏在"已有覆盖边界"表中 |
| **③ 前端供应链安全** | ❌ **声明不成立**。`production-engineering-directions.md` 方向一已覆盖 CSP/SRI/npm audit，仅中文可达性和自托管策略为独特贡献 |
| **④ 数据库演进运营** | ✅ **基本准确**。仅 CREATE INDEX CONCURRENTLY 的部分需加环境区分 |
| **⑤ 可观测性能工程** | ⚠️ **准确性中等**。低估了已有 traceparent 和 per-route 指标的存在；但 RUM/CI 基准/SQL 延迟/SLO 确实缺失 |

**整体文档质量**：论证扎实，代码依据充分，边界条件有思考。但"已有覆盖边界"表的准确性不足——方向②和③有直接重叠的既有分析，**交叉验证应发现并引用这些文档**，而不是只对比不相关的安全头方向。

**整改方向**：
- 方向②：引用 `truly-uncovered-gaps.md` 方向二作为已有覆盖，说明您的分析的增量价值
- 方向③：承认已有覆盖，聚焦"CDN 中文可达性 + 自托管策略"两个独特贡献点
