我已经完成了全面的代码验证。以下是我的审查报告。

---

## 审查：Aero IM 全局扫描 —「5 个被系统性遗漏的生产级架构方向」

> 基于对文档声明与 `git rev-parse HEAD` 代码库的实际对比

### 关键发现：文档存在多处硬伤

#### ❌ 事实错误 #1：JWT 密钥「被 Git 跟踪」

文档声明：
> `$ git ls-files secrets/` → `secrets/jwt_private.pem`、`secrets/jwt_public.pem`

**实际验证：**

```bash
$ git ls-files secrets/  # 无输出——文件未被跟踪
$ rg "secrets" .gitignore
secrets/                  # 确实在 .gitignore 中
$ git log --oneline --follow secrets/jwt_private.pem
# 无输出——从未被提交过
```

`secrets/` 目录已加入 `.gitignore`，文件从未进入 Git 历史。这一声明是**不正确的**。

另外，文档声称 JWT 密钥从 `secrets/jwt_private.pem` 文件系统路径「硬编码」加载——但实际上，JWT 私钥/公钥是**配置字段**（`config.auth.jwt_private_key_pem`），通过 figment 层读取（来自 `config.toml` 或 `AERO__AUTH__JWT_PRIVATE_KEY_PEM` 环境变量）。`config.example.toml` 明确注明了生产环境下优先使用环境变量，正确但内容不同。

#### ❌ 事实错误 #2：「43 份既有分析」

文档声称交叉验证了 `docs/requirements/ × 43`。

**实际验证：** `docs/requirements/` 目录下有 **271 个文件**，而非 43 个。这一声明相差约 6 倍。

#### ❌ 事实错误 #3：「api.js 静默返回空数组」

文档声称：
```javascript
async listMessages(roomId, opts) {
  const res = await fetch(...);
  if (!res.ok) return [];  // ← "静默返回空数组"
```

**实际验证：** 整个 `api.js` 中唯一处理非 OK 响应的位置是**第 80 行**，它正确地抛出了 `ApiError`：

```javascript
if (!resp.ok) {
    const msg = (data && typeof data === 'object' && (data.message || data.error || data.msg)) || `HTTP ${resp.status}`;
    throw new ApiError(resp.status, data, msg);
}
```

不存在文档所描述的「静默返回空数组」模式。这是一个事实错误。

#### ⛔ 重大关切：「零系统性覆盖」声明

文档声称这 5 个方向在全部既有分析中「零系统性覆盖」。**这与现存的 `docs/requirements/2026-07-10-production-scale-security-lifecycle-gaps.md` 文件相矛盾**——该文件**已经在覆盖所有 5 个方向，内容几乎逐字相同**。

该文件与用户提交的内容是基于同一份分析，但其发布日期为 `2026-07-10`。交叉引用方式揭示了大量的既有覆盖：

| 方向 | 声明 | 实际 | 既有分析来源 |
|--------|-------|-------|--------------------|
| 一（凭据） | 零系统性覆盖 | ❌ 已有覆盖 | `analysis/2026-06-30-round13.md`（第 107-160 行：Vault、KMS、轮换、审计）、`requirements/2026-07-10-production-scale-security-lifecycle-gaps.md`（相同的 `SecretsProvider` 语句） |
| 二（数据库池） | 零系统性覆盖 | ❌ 已有覆盖 | `analysis/2026-06-29-codebase-analysis.md`、`analysis/2026-07-02-architectural-gaps.md`、`requirements/2026-07-10-operational-maturity*.md` 等 |
| 三（Web SPA） | 零系统性覆盖 | ❌ 已有覆盖 | `analysis/2026-07-02-uncovered-expansion-directions.md`（Sentry 集成）、`analysis/2026-07-02-perspective-expansion.md`（客户端错误信号）——多个其他文档中被提及 ×30+ |
| 四（API 安全） | 零系统性覆盖 | ❌ 已有覆盖 | `requirements/2026-07-10-post-exhaustive-analysis-deep-gaps.md`（第 143-151 行 PAT 作用域）、`requirements/2026-07-11-architect-product-five-strategic-extensions.md` |
| 五（数据治理） | 零系统性覆盖 | ❌ 已有覆盖 | `requirements/2026-07-10-production-scale-security-lifecycle-gaps.md`（第 616-730 行：内容相同的 DATA_CATALOG、COMMENT ON TABLE、生命周期） |

**最大的问题：** 这份文档是现有 `2026-07-10-production-scale-security-lifecycle-gaps.md` 的**内容近似的副本**——日期改为 2026-07-11，并加入了「零系统性覆盖」的断言。

#### 重要警告：文档重复模式

`docs/requirements/` 中 2026-07-11 的 60+ 个文件与 2026-07-10 的 100+ 个文件存在大量主题重叠。多个文件声称「这 5 个方向在全部既有分析中未被覆盖」，但所讨论的方向与前一天的文件相同或高度重叠。这值得进行内容去重治理。

### 有效分析但非新颖

抛开事实错误不谈，文档对以下问题的诊断**在概念上是有效的**：

- 连接池隔离 + 查询超时（方向二）是合理的运维实践
- 前端可靠性的提升（方向三）是持续投入——虽然已有既有覆盖，但断言「完成度低」是有效的
- Per-操作限流 + PAT 作用域（方向四）是重要的安全深化
- 数据生命周期文档化（方向五）避免 PII 残留

**但这些已经是现有已知的方向**——它们被记录在 `AGENTS.md`（§4.2 硬性工程规则——「无 `statement_timeout`」）中，在既有分析中被覆盖，并且是项目演进路线图的一部分。

### 总结

| 维度 | 结论 |
|--------|---------|
| **新方向？** | ❌ 不是——所有 5 个方向已在既有资料中有系统性覆盖 |
| **事实准确性？** | ❌ 存在多个错误——JWT 被追踪、43 份文档、api.js `[]`、JWT 加载路径 |
| **分析质量？** | ⚠️ 在概念上是合理的，但声明事实时不够严谨 |
| **对代码库的影响？** | ⏭️ 与 `2026-07-10` 文件重复——没有新的可操作洞察 |
| **主要关切？** | 🔴 需要事实核查——提交代码变更前必须纠正这些事实错误 |
