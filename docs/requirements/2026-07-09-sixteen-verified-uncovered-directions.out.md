现在我已对文档各主张完成代码交叉验证。以下是审核意见：

---

# 审核：第 16 轮全局扫描文档

已保存至 `docs/requirements/2026-07-11-round-16-global-scan-five-uncovered-high-value-directions.md`

## 总体评价

文档的**方向选择结构精良**，5 个方向确实代表了既有 35+ 份分析的盲区。但具体**主张（claims）的精确度参差不齐**——部分被代码证据显著弱化，部分则经确认成立。以下按方向逐条审校。

---

## 方向一 · 混沌工程 ✅ 最坚实的方向

**结论：全中。这是 5 个方向中代码证据最无争议的。**

验证结果：

| 检查点 | 文档主张 | 代码核实 |
|-------|---------|---------|
| Toxiproxy / Chaos Mesh 集成 | ❌ 不存在 | ✅ 确认，`grep -r` 返回空 |
| 故障注入框架 | ❌ 不存在 | ✅ 确认，代码库无任何故障模拟设施 |
| 网络分区/进程崩溃/延迟注入/资源耗尽测试 | ❌ 全部不存在 | ✅ 全部确认不存在 |

**仅有细化建议**：阶段 A 的 `storage/db.rs` 原子布尔开关方法可行但需要留意——`AtomicBool` 故障注入在生产代码里引入条件分支，应通过 `#[cfg(test)]` 或 feature flag 条件编译隔离，避免生产路径增加分支预测开销。建议在 `aero-common` 中加一个 `FaultInjector` trait，测试时注入，生产时为空操作（no-op）。

---

## 方向二 · 身份全生命周期 ⚠️ 准确度需要大幅修正

**此方向方向正确，但具体主张有多处与代码事实不符。**

### 密码重设：文档 claim 明显过低

文档称：
> "代码有 `password_reset` 表但**无完整的前到后工作流**——没有邮件发送管线，令牌过期/轮换不完整"

**事实相反。** `crates/aero-server/src/sessions.rs:332-376` 显示：

| 能力 | 状态 |
|------|------|
| `POST /api/auth/forgot-password` | ✅ 完整实现，含防枚举（始终返回相同响应体） |
| 邮件发送 | ✅ `mailer.send_password_reset(&email, &token).await` 调用（mailer 未配置时 fallback log-only） |
| `POST /api/auth/reset-password` | ✅ 完整实现 |
| 令牌 hash + 过期 | ✅ `hash_reset_token` + `RESET_TOKEN_TTL` + 一次性使用 |
| 密码历史检查 | ✅ `password_was_recently_used()` 防重用 |
| 会话吊销 | ✅ `revoke_all_for_participant` 强制所有设备重新登录 |

**真正缺失的不是后端工作流，而是 web SPA 端的 UI**——`grep -r "forgot\|reset" web/*.js web/*.html` 显示前端**没有任何密码重设表单**。正确的表述应为：「后端管线已经完备，但 web SPA 中缺少 forgot-password / reset-password 的用户交互页面。」

### 邮箱变更验证：文档 claim 需修正

文档称：
> "当前 `update_me` 直接接受新邮箱（通过 `participants::update_profile`），没有任何验证步骤"

**`update_me` 确实不处理邮箱**——它只更新 `display_name` 和 `avatar_url`（routes.rs:933-954）。邮箱变更走专门的 `POST /api/auth/change-email`（sessions.rs:278-322），其中：

```rust
// Verify the caller's current password before allowing a silent email swap.
auth_password::verify(&req.current_password, &creds.password_hash)
```

**密码验证在**，防止劫持会话静默改邮箱。但确实**没有邮箱所有权验证**（向新邮箱发确认链接）。所以文档的核心担忧（无邮箱所有权证明）成立，但具体"`update_me` 直接改"的说法需要修正。

### 恢复代码：API 存在，UI 缺失

文档称：
> "恢复代码 migration 0122 存在，但无实际恢复流程 UI"

代码验证：
- ✅ `POST /api/me/2fa/recovery-codes` — 生成恢复代码（`twofa.rs:214`）
- ✅ `POST /api/auth/2fa/recover` — 消费恢复代码（`twofa.rs:254`）
- ✅ `recovery_codes` 表（migration 0122）+ `RecoveryCodeRepo` 完整
- ❌ web SPA 中无恢复代码 UI 入口（`grep -r "recover" web/*.js web/*.html` 返回空）

同样，**后端 API 就绪，前端 UI 缺失**。

### 修正后方向二的工作量评估

| 子项 | 实际工作量（vs 文档评估） |
|------|------------------------|
| 密码重设前端 UI | 纯前端工作 ~2-3 天（后端已就绪） |
| 邮箱所有权验证 | 后端加确认链接逻辑 + mailer 集成 ~2 天 |
| 恢复代码前端 UI | 纯前端工作 ~1-2 天（后端已就绪） |
| 可信设备模型 | ~3 天（需新增 `device_fingerprints` 表 + 中间件） |
| 不活跃账号回收 | ~3-4 天（定时器 + 通知 + 冻结逻辑） |
| 账号合并/转移 | ~1 周（数据迁移逻辑复杂） |

**方向二的阶段 A 从文档评估的 1 周减为约 4-5 天**，因为密码重设和恢复代码的后端管线已存在。

---

## 方向三 · 数据质量一致性监控 ✅ 成立

**结论：新方向，主张基本准确。**

代码证实：

| 核对目标 | 现状 |
|---------|------|
| 跨系统一致性核对 | ❌ 不存在任何系统性核对 |
| 外键约束 | ❌ 151 张表零外键（与 round-14 分析一致） |
| 数据质量控制台 | ❌ 不存在 |

**补充建议**：
- 阶段 A 的核对定时器应考虑使用 `pg_background` 或 `pg_cron` 扩展在数据库内运行，而非 Rust 定时器拉取——减少网络往返，核对逻辑可用 SQL `INSERT INTO data_quality_log SELECT ...` 直接表达。
- 核对频率 6h 可能过密，建议先跑一次 **full scan** 评估积压量，之后按实体变化率调整（消息表每 24h，房间成员每 6h）。
- 阶段 C 的自动修复「S3 缺失 blob → 从 LocalFs 补传」需注意 LocalFs 可能是 ephemeral 容器存储——如果生产用 S3、开发用 LocalFs，补传方向应是 S3 ← LocalFs 或仅告警。

---

## 方向四 · API 开发者平台 ✅ 方向正确，但已有一些铺垫

**结论：方向正确，文档精准识别了手写 openapi.rs（162 行/11 端点 vs 150+ 实际端点）的核心问题。**

文档准确指出：
- `openapi.rs` 手写 162 行仅覆盖 8 个端点（实际应有 150+）
- 无版本化策略
- 无 SDK
- 无开发者控制台

**但有几个需要校正的点**：

1. **utoipa 集成的实际工作量被低估**。当前路由架构分散在 50+ 子模块（`crate::workspaces::routes()`、`crate::collab::routes()` 等），utoipa 需要为每个 handler 添加 `#[utoipa::path(...)]` 属性宏——这不仅仅是添加依赖，而是**逐一注解 150+ 个 handler**，工作量约 2-3 天而非 1 周。建议逐步加，先覆盖核心消息/房间/认证端点。

2. **WebSocket SDK 难度被低估**。文档说 WebSocket 客户端 SDK 和当前 `api.js` 能力相同。但 `ws.js` 中包含复杂的 `since` 游标补回、连接状态机、重连退避、心跳——将其封装为 npm 包并文档化需要 1 周而非零头。

3. **文档中 `Sandbox 环境` 评估为阶段 B（2 周）** 合理，但需要注意资源隔离的边界：独立的 PG schema + Redis DB 意味着 sandbox 工作区不能和正式工作区共享 NATS subject——否则 sandbox 中的 Bot 会收到正式环境的 `im.room.*` 事件。

---

## 方向五 · Schema 演化安全管线 ✅ 成立但已有前驱分析

**结论：方向完全正确，但在既有分析中已有实质性探讨（round-14 的不可逆迁移体系章节和 genuine-novel-directions 均提及），并非零命中。**

准确主张：
- ✅ 157 个迁移全为 up-only，无 down.sql
- ✅ `aero-cli migration rollback` 不存在
- ✅ 迁移测试仅在空库运行（`make migrate-smoke` 验证）
- ✅ 无零停机 DDL 工具

**补充**：
- 阶段 A 建议「先最后 20 个迁移开始，前向批量补写」——实际建议从**对数据有破坏性的迁移**开始补（DROP COLUMN / ALTER TYPE / ADD NOT NULL），而非简单的最后 N 个。CREATE TABLE 迁移的 down 很简单（`DROP TABLE IF EXISTS`），应优先补。
- 补写 down.sql 的工作量被低估。157 个迁移中约 ~100 个是 `CREATE TABLE`（down 简单），~40 个是 `ALTER TABLE ADD COLUMN`（down 中等），~10 个是数据迁移/复杂变更（down 需谨慎）。阶段 A 的 1 周评估可能需调整为 ~2 周以包含测试。
- 阶段 C 的 `staging 实况回放` 中的 **ANONYMIZE** 步骤非常关键——建议列为 mandatory 而非 optional，因为它涉及 GDPR 合规。

---

## 总结：修正后的评估矩阵

| 方向 | 优先级 | 文档评估准确性 | 实际体量 | 核心发现 |
|------|--------|--------------|---------|---------|
| 一 · 混沌工程与故障注入 | **P1** | ✅ 全准 | 阶段 A 2-3 天 | **最坚实的新方向** |
| 二 · 身份全生命周期 | **P1** | ⚠️ 部分过低 | 阶段 A ~4-5 天（非 1 周） | 密码重设 + 恢复代码后端已就绪，真实缺口在 **web SPA UI** 和邮箱所有权验证 |
| 三 · 数据质量一致性监控 | **P1** | ✅ 全准 | 阶段 A 3 天 | 方向成立，核对频率和自动修复边界需细化 |
| 四 · 开发者 API 平台 | **P2** | ⚠️ 部分低估 | 阶段 A ~2 周（非 1 周） | utoipa 注解工作量和 WS SDK 封装难度均被低估 |
| 五 · Schema 演化安全管线 | **P2** | ⚠️ 非零命中 | 阶段 A ~2 周（非 1 周） | 前驱分析已有讨论（genuine-novel-directions 方向四），下行迁移补写范围需按破坏性优先 |

### 文档的最大贡献

尽管有几个具体主张需要校正，这 5 个方向的**选择是高质量的**。最令人印象深刻的是：

1. **方向一（混沌工程）**在全部既有分析中确实为零——这是真正的盲区，且与生产可靠性直接相关。**建议立即启动阶段 A**（可在一个 PR 内完成 `FaultInjector` trait + 单元级故障模拟）。

2. **方向二的前端缺口**（密码重设 UI / 恢复代码 UI）在所有 35+ 分析文档中确实未被系统触及——所有分析都关注了后端身份逻辑（JWT/PAT/2FA/SCIM），但从未有人问过 web SPA 里 `forgot-password` 表单在哪里。

3. **文档的元分析框架**（「功能+架构+安全+运维」四轴盲区）本身就是一个有价值的贡献——它为未来扫描提供了发现新盲区的方法论。

### 建议的下一步

1. 将方向一（混沌工程）拆为一个 Issue 含阶段 A 的具体实现方案
2. 将方向二拆为两个独立的 Issue：（a）密码重设 + 恢复代码前端 UI；（b）邮箱所有权验证
3. 方向三和方向五可合并为一个「数据平台工程」专题，两者都涉及定时核对 + PG 基础设施
4. 方向四（开发者平台）建议放在 AI 审核/搜索系统生产化之后，作为 P2-P3 处理
