Okay，我已经做了全面的交叉验证——通读了你的分析文件、对照代码库验证了关键断言、检查了 60+ 份既有需求分析。现在给一个**逐方向的实体验证反馈**。

---

## 验证结论总览

### 方向一（搜索质量与相关性）—— ⚠️ **严重高估"零覆盖"，且代码事实有误**

**"零系统性论证"的判断不成立。** 2026-07-09 的 `search-quality-privacy-resilience.md`（594 行整份分析）已经系统性地覆盖了这个方向：

| 你的子方向 | 已有覆盖情况 |
|-----------|------------|
| 搜索建议/自动补全 | ✅ 完整分析 + 架构建议 |
| 拼写纠错 / 「你是不是要找…」| ✅ 完整分析 + 实现方案 |
| 结果多样化（按作者去重、按房间分散） | ✅ 完整分析 |
| 个性化排序（历史交互调权） | ✅ 完整分析 |
| 搜索点击反馈回路 | ✅ 迁移 0133 + `search_feedback.rs` 都已实现 |
| 搜索结果高亮 | ❌ 但你断言"无"——**实际代码已有 `ts_headline`** |

**代码验证发现的 3 个事实错误**：

1. **"ts_headline/snippet 无" → 实际已存在**：`crates/aero-storage/src/search_query.rs` 和 `crates/aero-storage/src/message/search.rs` 都使用了 `ts_headline`（`'StartSel=<b>, StopSel=</b>'`）——basic `search_all_rooms` 没有，但 `AdvancedSearchRepo` 有。

2. **"search_feedback 从未被使用" → 已接线**：`crates/aero-server/src/search_advanced.rs` 中 `record_click` 已被调用。`search_feedback.rs` 文档明确写着 `ROADMAP5 方向三 P2`——收集已就绪，但反馈回排序的闭环确实还没做。

3. **"merge_hits 简单 30/70 比例融合" → 实际是取最大值**：`routes/helpers.rs` 的 `merge_hits` 并非加权融合——只是对每条消息取 FTS 和 Vector 两种 score 中的最大值，无预设比例。

✅ **你的核心 insight（BM25、时间衰减、拼写纠正、排序优化）依然成立**——但这份分析与既有分析的**重叠度 > 70%**。

---

### 方向二（富媒体管线）—— ✅ **最高价值的新缺口，但部分子方向已有覆盖**

"thumbnail" 在 22+ 文件中被提及（多为带过），但**确实没有一份分析系统性地分析了完整的富媒体管线**（缩略图生成 → EXIF 剥离 → WebP 转码 → 文档预览 → 视频转码 → CDN 签名 → 断点续传）。这是最好的一个方向。

⚠️ 但需注意：**HLS 转码、ABR 多码率**已有分析覆盖（`expansion-analysis.md` 方向三）。

---

### 方向三（生产运维基础设施）—— ⚠️ **已有覆盖，非"零"**

| 你的子项目 | 已有覆盖情况 |
|-----------|------------|
| 无 Dockerfile | ✅ `production-operability-expansion-directions.md` 已列出为方向一的零代码证据 |
| 无 K8s 清单 | ✅ 同上，已列出 |
| 无 Helm Chart | ✅ 同上 |
| 无 CI/CD pipeline | ✅ 同上 (`.github/workflows/ci.yml` 标注"当前无 CI runner") |
| 无生产备份策略 | ✅ `production-maturity-and-strategic-gaps.md` 已提及 |
| 无 Terraform IaC | ✅ 已提及 |

`production-operability-expansion-directions.md` 的方向一和三直接覆盖了你的方向三的 **80% 内容**。这不是零覆盖——这是**重复覆盖**。

✅ 你的**新增价值**在于：Phase A/B/C 的工程实施步骤分解、NATS durable consumer 再平衡的边界条件、Secret 轮换的双密钥支持——这些都是既有分析没展开的细节。

---

### 方向四（负载测试）—— ⚠️ **已有提及但未展开**

`production-operability-expansion-directions.md` 的核心发现表中有：**"负载测试 / 基准线：无任何基准或压力测试脚本"**（明确列出作为零代码证据）。但这份分析没有展开成独立方向。

✅ 这是合理的新角度——但 claim "零系统性论证" 不准确，应该是"有提及但未展开"。

**新增价值**: 你的 `oha`/`drill` 工具选择、WS 扇出基准设计、NATS `max_ack_pending` 的测试方案、str0m 配对测试——这些细节是既有分析没有的。

---

### 方向五（国际化 i18n）—— ⚠️ **已有覆盖，非"零"**

`2026-07-10-five-strategic-product-directions.md` 的 web SPA 现代化清单中已明确列出：
> "**无 i18n 框架**——中文硬编码，无切换机制 | 无法进入英文/多语言市场"

`2026-07-11-global-scan-extension-directions.md` 的方向一（Web SPA 现代化）也列出了 i18n。

虽然这些分析没有像你的分析那样展开到 Phase A/B/C 的详细实施计划，但**方向本身已被识别和论证**，并非"零系统性覆盖"。

---

## 关键事实纠正汇总

| 你的声明 | 实际代码/文档状态 | 严重程度 |
|---------|-----------------|---------|
| "零系统性论证"（方向一） | `2026-07-09-search-quality-privacy-resilience.md` 已系统性覆盖 | 🔴 严重 |
| "零系统性论证"（方向三） | 至少 2 份分析已系统性覆盖 | 🔴 严重 |
| "零系统性论证"（方向五） | 至少 2 份分析已列出 | 🟡 中等 |
| "ts_headline 不存在" | `search_query.rs` 中已使用 `ts_headline` | 🔴 事实错误 |
| "search_feedback 从未被使用" | `record_click` 已接线到 `search_advanced.rs` 路由 | 🟡 不准确 |
| "merge_hits 30/70 比例" | 实际是取 `max(FTS_score, vector_score)` | 🟡 不准确 |
| "仅按 created_at + rank 排序" | 实际使用 `ts_rank` + `similarity` 的 `GREATEST()` 相关性排序 | 🟡 不太准确 |

---

## 这份分析的真正价值

尽管有事实错误和"零覆盖"声明的问题，这份分析中的**核心建议质量很高**，特别是：

1. **方向二（富媒体管线）** — 这是唯一真正未被系统性分析覆盖的方向。缩略图管线、EXIF 剥离、文档预览、CDN 签名 URL、断点续传——这些打包成一个完整方向，既有分析都没有做到。应该保留并推为 P0。

2. **方向一的时间衰减建议** — `score * exp(-days * decay_lambda)` 的实现思路是现有分析没有涉及的具体优化。

3. **方向四的 WS 扇出基准设计** — `fan_out_latency(N)` 的曲线、NATS `max_ack_pending` 测试——这些具体的测试设计是新增价值。

4. **方向三的边界条件** — NATS durable consumer 在 K8s 滚动更新时的再平衡、Secret 轮换的双密钥策略——这些是其他分析没有涉及的部署细节。

5. **方向五的 Phase A/B/C 实施分解** — 从 `function t(key)` 切入、逐步替换的渐进式策略，比现有分析的"需要 i18n"声明要具体得多。

---

## 建议修改方向

如果把"零覆盖"的框架调成**"已有覆盖但未深入到实施层面"的分析补丁**，这份文档的 credibility 会大幅提升。建议：

1. **前置分析加上**：`交叉验证说明` → 改为标记每个方向与既有分析的关系（"方向一已由分析 X 覆盖了 A/B/C，本文补充 D/E/F"）

2. **方向一缩为"搜索相关性补丁"**（而非"搜索引擎做了一半"）— 去掉已被覆盖的 autocomplete/spelling/个性化，聚焦**时间衰减 + BM25 + click-to-rank 闭环**这些既有分析没涉及的子方向

3. **方向三和五** 改为"实施计划补充"而非"零覆盖新方向"

4. **修正代码事实错误**：删除 ts_headline/search_feedback/merge_hits 的错误表述

5. **方向二保持完整**并升级为 P0 — 这个方向确实没有系统性分析

这份分析在**实施细节和边界条件**上有独立价值——如果修正"零覆盖"声明和代码事实错误，会是一份有建设性的补充分析。
