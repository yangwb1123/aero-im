3. **重要：当前没有客户端幂等键**——`send_message` 不接受 `client_message_id` 或类似 `txn_id`。这意味着移动端离线发消息排队重送时，服务器无法去重。这是移动 SDK 的先决条件缺口，比 API 适配更深层。

4. **你说「web SPA 在桌面端较为成熟」这话有点重了**——`web/app.js` 是 1009 行单文件 SPA（零依赖 ES2020），评测它是否「成熟」要看骨架而非行数。作为参考，一个中等复杂度的 React 聊天 SPA 通常在 15k-30k 行。1009 行的 monolith 说明它极简但**缺乏组件抽象**，移动端适配时可能发现很多逻辑散在 DOM 事件回调里，难以跨平台复用。

---

## 方向二：OAuth2 授权服务器

**现状评估准确度：✅ 高，但可以更精确**

你说「完全没有 OAuth2 授权服务器能力」——**完全正确**。`rg -rl "oauth" crates/` 返回空（不含 `aero-push`），而 `aero-push` 里的 oauth2 只是 FCM HTTP v1 的**OAuth2 客户端凭据授权**，不是授权服务器。

**需要补充的架构约束：**

1. **JWT 签发资产已有但 scope 编码策略需设计**：`aero-auth` crate 的 `jwt.rs`（RS256，签发/验证 AuthUser）可以复用，但 scope 编码进 `aud` 或 custom claims 需要定一个模式。我建议借鉴 Matrix 的 `m.` prefix scopes + Matrix 的 token introspection 端点设计。

2. **assert_room_access 加 scope 校验的复杂度被低估**：当前 `assert_room_access` 是一个巨大的守卫函数（participant + room → workspace 解析 → 成员 → 停用 → 2FA），scope 需要作为**叠加层**而非修改内部逻辑。架构上应该是一个 `scope_check(method, path) -> bool` 中间件/guard，在 `assert_room_access` **之后**调用。

3. **你漏了一个重要资产**：`aero-auth` crate 有 `pat.rs`（Personal Access Token）——这是一个**已有的 bearer token 机制**，正好可以扩展为 OAuth2 access_token 的存储模型和验证管道。PAT 当前的验证路径 `aero_pat_*` 回落（AGENTS.md §1）可以作为 OAuth2 token 验证的起始模板。

4. **PKCE 是移动端 public client 必须的**：你提到了 PKCE 但没深入。如果移动端（方向一）也要 OAuth，`S256` PKCE 是强制要求（RFC 7636），纯授权码流程不够。

---

## 方向三：可配置工作流引擎

**现状评估准确度：🟡 有细微偏差**

你说的 `workflow` 出现在 `routes.rs` 是因为 **`approvals.rs` 模块注释里写了一句** `"Approvals workflow"`——这是一个单级审批的简写，不是任何工作流引擎的命名空间。`rg -rl "workflow"` 只在 `approvals.rs` 和 `ban_appeals.rs` 的注释里出现。**没有工作流引擎的 scaffold**。

**更深层的架构问题：**

1. **Rust 类型系统的静态性与工作流动态性不匹配**：工作流引擎本质上是**运行时状态机**（steps/transitions/conditions 可动态配置），而当前所有原子能力（`tasks.rs`, `approvals.rs`, `scheduled.rs`, `recurring.rs`, `agent_bot.rs`）都是**编译时硬编码的 Rust handler**。要让客户无代码配置流程，需要：
   - 一个稳定的 JSON schema 定义流程 DAG
   - 一个动态 action dispatcher（根据 step type 调用对应 Rust handler）
   - 持久化的 `WorkflowInstance` 状态机（当前进度 + 入参 + deadline）
   - 背景定时器推进 ready transitions + 超时处理

2. **建议你评估这个缺口是 P2 还是 P3**：Slack Workflow Builder 用了 5 年才成熟（2019 GA），而 Aero 当前连第一方 webhook 的生态都没形成（方向二）。我倾向于这个排 P3 而非 P2——先完成 OAuth2 平台，再在上面搭 workflow，而不是先做 workflow 再搭平台。

3. **「可视化流程编辑器」的难度被低估了**：这是一个完整的 UI 框架级别的工作（节点拖拽/连线/配置面板/序列化/撤销重做），在 1009 行 SPA 上加这个不是「扩展一个画布视图」那么简单。我建议把 workflow definition 定义为**YAML 配置文件**（类似 `docker-compose.yml` 风格的流程 DSL），而不是先做可视化编辑器。SLAs/流程专家更习惯写 YAML 而非拖画布。

---

## 方向四：高级合规

**现状评估准确度：✅ 高**

`legal_hold.rs` + `legal_holds.rs` 确实存在且功能完整（留存清扫跳过）。但「审计报表」这个缺口比你说的更窄——`audit.rs` 有**分区表结构**（migration 0154），数据是可以查询的，差的只是汇总查询 + 报表 UI。

**需要补充的事实：**

1. **GDPR 导出不是「当前已覆盖」那么简单**：`me_export.rs` + `workspace/export.rs` + `participant.rs` 的删除级联列表确实存在，但**导出格式只是 JSON**，不是行业标准的 PST/MBOX。你提到的 eDiscovery 导出需求是对的。

2. **有一个架构约束你没提到：WORM 存储**。FINRA 17a-4 要求记录在保留期内**不可修改/不可删除**。当前的 `message_history` 只能在 audit 层面记录变更，不能阻止硬删。合规归档需要写一个**WORM 存储层**（append-only blob store + retention lock），这和 `BlobStore` trait 可以整合但需要新实现。

3. **`keyword_moderator` 做上下文规则引擎的基线比你评估的更薄**：`keyword_moderator.rs` 只是词级过滤（`AERO_BLOCKED_WORDS` env 配置的词表），没有 AST、没有 pattern matching 引擎。要做「股价在交易时段=违规」这种上下文敏感规则，需要完整的规则引擎——我评估至少 600-800 行 Rust，外加规则 DSL 解析。

---

## 方向五：SRE 生产就绪

**现状评估准确度：🟡 有些细节需要校准**

**你低估的地方：**

1. **监控就绪度比你说的好**：`monitoring/` 目录下有完整的 Prometheus recording rules + alert rules + Grafana dashboard 配置，README 标明了 bearer-gated `/metrics`、99.9% SLO burn-rate alerting。这不是「存在样例」——这是**可部署的监控配置**，只差 Prometheus 实例去 scrape。

2. **测试基础设施比你说的多**：不是「无性能测试框架」——`scripts/` 下有 30+ 个 `smoke_*.py` 脚本（ws_smoke.py, smoke_live.py, smoke_enterprise.py 等），覆盖 24 波功能迭代。虽然这些是**功能性冒烟测试**不是**负载测试**，但它们说明项目有自动化的端到端验证资产，不是从零开始。

3. **CI 配置不是「模板」**：`.github/workflows/ci.yml` 是完整的 GitHub Actions 配置（只是被注释掉等待 runner），包含 `cargo check`、`cargo test --lib`、`cargo clippy`、`scripts/truth-check.sh`、`file-size-check.sh`、`web-check.sh` 五个 job。这是**配置已完成但没 runner**，而不是「还没写」。

**你高估的地方：**

1. **迁移回滚的可行性被高估**：`sqlx::migrate!("../../migrations")` 只支持前向迁移（`MigrateError` 里没有 down 逻辑）。`aero-cli migrate down` 的方案需要：
   - 每个 `.sql` 文件嵌入 `-- DOWN:` 注释块（当前 157 个迁移都没有）
   - 一个新的 CLI 子命令解析这些块并逆向执行
   - 处理 `_sqlx_migrations` 表的版本回退
   - **工作量被严重低估**——我建议用 `sqlx migrate revert`（需 `sqlx-cli` 配合运行时检测）而非 DIY，但这需要验证兼容性。

2. **Helm Chart 的路径依赖**：你说的没错——缺 Helm。但需要明确先做**docker-compose 生产化**（健康检查、优雅关闭、日志聚合）还是直接跳 Helm。我建议先做 `docker-compose.prod.yml`（带 backup sidecar 和 log shipper）作为过渡，再写 Helm chart，而不是一步到位。

3. **没有提到**：当前没有 **blue-green readiness probe** 模式。`/health/ready` 探活 PG/Redis/NATS/Blob，但**没有 draining 模式**——新 pod 就绪前旧 pod 仍接收流量，这是零宕机部署的必备条件，你漏了。

---

## 核心纠偏总结

| 报告结论 | 我的验证结果 | 建议调整 |
|---------|-------------|---------|
| 推送基础设施「已就位」 | ✅ 准确，且比文档描述的更成熟（有真 FCM/APNs 网关代码） | 加强措辞，说明只差生产凭据 |
| 「移动端离线消息队列」缺失 | ✅ 准确，且缺少客户端幂等键（`client_message_id`） | 补充：这是移动 SDK 的先决条件 |
| web SPA 「较为成熟」 | 🟡 1009 行单文件 monolith，功能完整但缺乏组件化 | 降级为「功能完整但架构极简」 |
| OAuth2 「完全没有」 | ✅ 准确 | 补充：PAT 机制是可复用资产 |
| 工作流引擎「复用任务/审批/AI」 | 🟡 概念正确，但静态 Rust vs 动态流程的矛盾被低估 | 建议降为 P3，先做 OAuth2 平台 |
| 合规归档「缺失」 | ✅ 准确 | 补充 WORM 存储需求 |
| SRE 「无性能负载测试」 | 🟡 有 30+ smoke 脚本但非负载测试 | 校准措辞：有功能冒烟，无 k6/locust |
| CI 「仅模板」 | 🟡 配置完整仅缺 runner | 校准措辞：CI 就绪仅 runner 未接 |
| 迁移回滚「待做」 | ✅ 准确，但难度低估 | 建议先验证 `sqlx migrate revert` 兼容性 |

---

## 优先矩阵修正建议

| 方向 | 你给的 P | 我建议的 P | 理由 |
|------|---------|-----------|------|
| ⑤ SRE 生产就绪 | P0 | **P0** ✅ | 赞同，但把 Helm 拆为 P0.5，把「CI runner 接入 + 监控投产」作为即时 P0 |
| ② OAuth2 集成平台 | P1 | **P1** ✅ | 建议和 PAT 复用结合，降低启动成本 |
| ① 原生移动端 SDK | P1 | **P2** | 不是不重要，而是 SDK 的客户端幂等键 + sync 端点 + 消息缓存设计需要 OAuth2 先就位做认证层。建议先做 API 面适配（`?since=` 端点 + 精简模型），SDK 层排 P2 |
| ④ 高级合规 | P2 | **P1**（金融客户）✅ | 同意你的条件式升级 |
| ③ 工作流引擎 | P2 | **P3** | 缺乏平台生态（方向二）先行的情况下，工作流引擎客户采用率会很低 |

要不要我展开其中任何一个方向的技术设计草图？比如 OAuth2 scope 在现有 `assert_room_access` 层的叠加方案，或者 `docker-compose.prod.yml` → Helm chart 的过渡路线图？
