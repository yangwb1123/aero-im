# Requirements Spec — aero-cli B5-4：`aero-eng audit-provision-check` + fail-closed gate wiring（落家 = `crates/aero-cli` aero-eng 二进制）

- **Module (analysis root)**: `crates/aero-cli` — aero-eng 工程 CLI（`[[bin]] name="aero-eng"`，无 DB 依赖设计）；B5-4 交付物 = 配给验证 seam `audit-provision-check` + `gate audit` 门接线
- **Direction**: "B5-4: add `audit-provision-check` + fail-closed gate wiring to aero-cli (the B5 deliverable that lands in this crate)"（value 9 / risk_reduction 9 / effort 5 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-cli-e99ec77a.json`（direction #1）
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml:35-37`："(4) scope provisioning (grant audit:event:write only after relay works; fail-closed without relay)"）；contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（B5-4 行 :11、[PROPOSED] 标注 :13、门禁 :15）；gate anchor `docs/campaigns/implementation-gate.md`（:64 aero-im 行 "T-11（无 relay 配给被拒）"、:78 G6 "37/37、T-11、moderation 优先级"）
- **Status**: Requirements（下述证据全部经源码 grep 核对）
- **Verification date**: 2026-08-07。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-cli/src/main.rs` — CommandRegistry + `a!()` 宏注册；`Gate_`（跑 scripts/*.sh：filesize/truth/web/deps/all）与 `Network_`（ping/dns/port TCP 探测）是扩展样板 | ✅ **Verified**。`a!()` 宏 :28-33 注册 `Check_`/`Gate_`/`Test_`/`Skill_`/`Doctor_`/`Completion_`/`Network_`/`Bench_`/`Dashboard_`；`Gate_` 经 `aero_eng::run::run_cmd("bash", &[path], timeout)` 跑 `scripts/*.sh`（`gate list` = "filesize truth web deps complexity … all"，未知子命令 → `Outcome::error("unknown")`）；`Network_` 用 `tokio::net::TcpStream::connect` 做 ping/dns/port 探测。`crates/aero-cli/Cargo.toml` deps 仅 aero-eng/tokio/serde_json/async-trait——**零 DB、零 aero-auth 依赖**（文件头注释 "engineering commands only (no DB dependencies)"）。 |
| E2 | `crates/aero-server/src/routes/health.rs` — `probe_commercial` 返回 "disabled"/"not_ready"，刻意不把 central-sink 可达性纳入 readiness（fail-closed = projection 式，readyz 永不因 relay 缺席翻转 ok→fail） | ✅ **Verified**。`probe_commercial` :69-79：runtime 缺席 → `"disabled"`；否则 `runtime.ready()`（**projection 式**，2s 超时）→ `"ok"/"not_ready"/"fail"/"timeout"`。注释 :67-70 原文："A reachable central service is intentionally not a probe dependency"。`health_ready` :166-199：`deps_ok` 对 commercial 用 `matches!(commercial, "ok" \| "disabled")`——**relay 缺席（disabled）不翻 readyz；sink 不可达根本不进 probe_commercial**（无可达性探针）。`readiness_decision` :148-160 是纯函数（既有单测 :240-256）。 |
| E3 | `crates/aero-server/src/snaplink_commercial/http.rs:23` — `SCOPE_AUDIT = "audit:event:write"`；`validate_audit_receipt`（:410）定义 claim/receipt 契约 | ✅ **Verified**。`SCOPE_AUDIT` :23 = `"audit:event:write"`（v1 内嵌 relay cc scope 先例，B5-2 connector `client.rs` 同款 :31）。`validate_audit_receipt` :410-423：`AuditReceiptEnvelope` 解码 + `receipt.event_id == claim.payload.event_id` + `tenant_id` 匹配 + `accepted_at` 存在 + `!conflict` + status ∈ {ledgered, indexed, archived}——provisioning check 可经 env/config + HTTP health 探测该契约面，**保持 aero-cli DB-free**。 |
| E4 | `crates/aero-auth/src/oidc.rs` — `validate_client_credentials_token` + `ClientCredentialsClaims::granted_scopes`（RFC 9068 at+jwt，iss/aud/sub/client_id/scope 校验） | ✅ **Verified**。`ClientCredentialsClaims` :372-385（`scopes: Vec<String>` + `scope: Option<String>`）；`granted_scopes()` :387-394 = scopes 数组 ∪ scope 空格串的**并集**；`validate_client_credentials_token` :406-471：typ ∈ {at+jwt, application/at+jwt}、RS256/EdDSA、iss/aud/exp/nbf/iat/jti/sub/client_id 全必填、`sub == client_id`、jti 约束、`required_scopes ⊆ granted_scopes` 否则 `OidcError::Invalid`。**注意**：aero-auth 依赖 sqlx（`Cargo.toml:47`，DB 可达）——aero-cli 不能依赖它（E1 无 DB 约束）；本 seam 以结构性 JWT payload 解码**镜像**该校验语义（§R2），完整签名验证 = [PROPOSED] seam（§7）。 |

### 1.1 补充证据（方向外事实，影响设计）

| # | Supplementary evidence | Verification result |
|---|---|---|
| E5 | B5-2 connector crate 已在 workspace | ✅ **Verified**。`crates/aero-audit-connector/` 已存在（src/{client,config,outbox,pg,relay,stub,fake,lib}.rs + bin/ + tests/），root `Cargo.toml:29` members、:77 `[workspace.dependencies]`。**AC1 的「connector crate missing」是可检测状态，非当前状态**——check 必须能检出缺席（用临时 workspace 测试），当前仓库态 = 存在（通过）。 |
| E6 | v2 relay boot 门 = `RelayConfig::from_env` presence-gate | ✅ **Verified**。`crates/aero-audit-connector/src/config.rs:3-4,49-63`：`AERO_AUDIT_TOKEN_ENDPOINT` 缺席且无其他 `AERO_AUDIT_*` → `Ok(None)`（feature off）；有 stray `AERO_AUDIT_*` 而无 endpoint → **boot Err（fail-loud）**；`AERO_AUDIT_EVENTS_URL` 必填。接线：`crates/aero-server/src/bin/main.rs:245-269`（presence-gated → PgOutboxRepo + AuditClient + AuditRelay）。 |
| E7 | v1 boot 门 = `SnaplinkCommercialRuntime::from_env` 一致性门 | ✅ **Verified**。`runtime.rs:42-66`：`CommercialMode::from_env()`（`config.rs:150-158`，`AERO_SNAPLINK_COMMERCIAL_ENABLED` explicit_bool）→ Unspecified→`require_disabled`、Disabled→`configure_disabled`、Enabled→`configure_enabled`——proposal :11 的「boot 门已验证存在」即此。 |
| E8 | aero-eng 退出码机制 | ✅ **Verified**。`outcome.rs`：`ok`=0 / `error`=1 / `warning(exit_code, msg)` 任意码 / `skip`=2；`merge` 取 worst severity（Error 3 > Skip 2 > Warning 1 > Ok 0）并带出该 outcome 的 exit_code。`registry.rs::execute_with_ctx` :94-124：**非 ok outcome 走 `Err` 分支 → `main.rs` `std::process::exit(r.exit_code())`** —— `Outcome::warning(不同码)` 可穿透到进程退出码。**但 `run::run_cmd` :15-41 把脚本任意非零码拍平为 `Outcome::error`（exit 1）**——要保留 distinct codes，`audit-provision-check` 命令必须直接用 `tokio::process::Command` 捕获脚本退出码（`Network_` 同款直接使用 tokio 的先例）。 |
| E9 | 37/37 钉入 seam 已占位 | ✅ **Verified**。`scripts/test-integration.sh` 已有 "A3 part 1 (pin)" 占位注释（:266-270 附近）："when the out-of-repo v2 contract's '37/37' test list lands … its in-repo-verifiable subset is pinned here as an explicit named list"——本 direction 把它变成**真实具名清单**（§R5）。 |
| E10 | 门禁/契约锚点 | ✅ **Verified**。`docs/campaigns/implementation-gate.md:64`（aero-im 行 "T-11（无 relay 配给被拒）"）、:78（G6 = "37/37、T-11、moderation 优先级"）；proposal :11（B5-4 定义）、:13（8 处不可验证/[PROPOSED] 含 "37/37" 清单）、:15（"37/37 需先把契约测试清单钉入 test-integration.sh"）。 |
| E11 | 落家冲突：sibling spec 选了另一个 aero-cli | ✅ **Verified**。`docs/requirements/2026-08-07-aero-auth-b5-4-relay-provisioning-gate.req.md` E6 判定 `audit-provision-check` 落家 = `crates/aero-server/src/bin/aero-cli.rs`（DB 能力 CLI，15 命令）。**本 direction 明确定死 = `crates/aero-cli`（aero-eng）**——acceptance 原词 "`aero-eng audit-provision-check`"。冲突记录进 §7（本 spec 的 acceptance 为准）。 |

## 2. Verified current state

```
现状（aero-eng 对 audit/relay 零感知）：
a) aero-eng（E1）  9 命令（check/gate/test/skill/doctor/completion/network/bench/dashboard）
                   └─ gate list 无 audit；completion cmds 无 audit-provision-check；零 audit 符号
b) 配给验证面      全仓 grep audit-provision-check 仅命中 docs（proposal + req 文档），零代码
c) relay 事实源（E5/E6） aero-audit-connector 已在 workspace（B5-2 已落地，403→dead=T-11 已实现）
                   └─ boot 门 = RelayConfig::from_env presence-gate（AERO_AUDIT_TOKEN_ENDPOINT）
d) readyz 语义（E2） probe_commercial 无 sink 可达性探针；deps_ok 接受 "ok"|"disabled"
                   └─ relay 缺席/不可达永不翻 readyz（约束，本 direction 只钉测试，不动代码）
e) 契约清单（E9）   test-integration.sh 的 A3 pin 占位已存在（37/37 内容在仓外，[PROPOSED]）

aero-eng 扩展样板（本 direction 复制的两条线）：
  Gate_   → run_cmd("bash", scripts/*.sh) → Outcome（拍平码，门=pass/fail）
  Network_ → tokio::net::TcpStream::connect 直接探测（可保留 distinct 退出码）
  Outcome::warning(code) → registry Err 分支 → main.rs exit(code)（E8，distinct codes 可穿透）
```

**Gaps this direction closes**（all verified）：① `audit-provision-check` 无家（E1b/E11）——新增 aero-eng 命令 + `scripts/audit-provision-check.sh`；② 无 gate 入口——`Gate_` 增 `audit` 子命令；③ 无 37/37 具名清单——test-integration.sh 占位变真实钉入（E9）；④ readyz 不翻转只有注释无测试——health.rs tests 增回归单测（E2，**零生产代码改动**）。

## 3. Scope

**In scope（B5-4，effort 5 的完整切片）**：
- `scripts/audit-provision-check.sh`（新脚本，bash + python3，**零新第三方依赖**）：配给验证单一事实源，distinct 退出码矩阵（§R1）。
- `crates/aero-cli/src/main.rs`：新 `AuditProvisionCheck_` 命令（`a!()` 注册）+ `Gate_` 增 `audit` 子命令 + `gate list`/`Completion_` cmds 串同步。**零新 crate 依赖**（E1 保持：aero-eng/tokio/serde_json/async-trait）。
- `scripts/test-integration.sh`：37/37 具名清单钉入（替换 A3 pin 占位，E9）+ 退出码矩阵 drill + （可选）readyz 不翻转 drill。
- `crates/aero-server/src/routes/health.rs`：**仅 tests 模块新增** readyz 不翻转回归单测（§R4）——**零生产代码改动**（probe_commercial/readiness_decision/health_ready 一行不动）。

**Out of scope（并行切片 / 其他模块——勿在本 direction 建造）**：
- 配给本体（grant 动作 / IdP scope registry）→ 仓外（proposal :11 "registry 本体在 IdP 仓"）；本 seam 只做**验证 + 非零拒绝**，不签发任何东西。
- aero-auth 侧 `assert_audit_scope_provisioned` 拒绝点 / durable 心跳 / 0240 迁移 → **sibling spec**（`2026-08-07-aero-auth-b5-4-relay-provisioning-gate.req.md`）；本 direction 不建 gate 状态，只消费 env/config/文件系统/网络可达性。
- 0239 治理 outbox DDL / priority 排序 / moderation 优先 → **B5-1/B5-3**；403→dead 状态机 → **B5-2 已实现**（E5），本 direction 只把 connector 存在性当检查项。
- 完整 JWT 签名验证（RS256/JWKS）→ [PROPOSED] seam（§7）；本 direction 只做结构性 claim/scope 校验（acceptance 只要求检出 scope 缺席）。
- v1 `snaplink_commercial/` 运行时、0236 触发器 → 原地不动（只把 v1 boot 门 env 当检查项，E7）。

## 4. Requirements

### R1 — `scripts/audit-provision-check.sh`（单一事实源，distinct 退出码）
新脚本（bash 壳 + python3 辅助；`bash scripts/audit-provision-check.sh [--gate]`，repo root 经 `$(dirname "$0")/..` 自解析）。**短路顺序执行，首个失败即退出对应码**：

| code | 类 | 条件（按序） | 对应 acceptance 条件 | 仓内锚点 |
|---|---|---|---|---|
| 0 | ok | 全部通过 | — | — |
| 1 | usage/config | workspace root 不可读；或 `AERO_AUDIT_*` stray 而无 `AERO_AUDIT_TOKEN_ENDPOINT`（boot 配置错） | — | `RelayConfig::from_env` Err 镜像（E6） |
| 2 | connector crate missing | `crates/aero-audit-connector/` 目录缺席，或不在 root `Cargo.toml` `[workspace] members`，或不在 `[workspace.dependencies]` | AC1-① | E5（当前仓态=存在，测试用临时 workspace 造缺席） |
| 3 | boot gate disabled | `AERO_AUDIT_TOKEN_ENDPOINT` 缺席（且无 stray）→ relay 不 boot，无可配给对象（T-11 fail-closed） | AC1-② | `RelayConfig::from_env` Ok(None) 镜像（E6） |
| 4 | scope absent | 未提供 token；或 token 结构性无效（非 at+jwt / iss·aud·sub·client_id 缺失 / sub≠client_id）；或 granted_scopes（scopes ∪ scope 并集，镜像 `granted_scopes()` :387）不含 `audit:event:write` | AC1-③ | oidc.rs E4；`SCOPE_AUDIT`（E3） |
| 5 | relay unreachable | `AERO_AUDIT_TOKEN_ENDPOINT` / `AERO_AUDIT_EVENTS_URL` 的 host:port TCP dial 失败（`Network_` port 探针同款） | AC1-④ | `Network_`（E1） |
| 6 | pinning missing（仅 `--gate` 模式） | `scripts/test-integration.sh` 无 `B5_CONTRACT_TEST_LIST` 具名清单标记或清单为空 | AC4 | E9 |

- **token 供给**：`AERO_AUDIT_PROVISION_CHECK_TOKEN` env（缺失 = fail-closed 拒绝，code 4，消息 "no token supplied"——配给无法验证即拒绝）。
- **结构校验语义**（python3 base64url 解码 header/payload，**镜像** E4 而非链接 aero-auth——E1 无 DB 约束）：header `typ` ∈ {at+jwt, application/at+jwt}；payload 必填 iss/aud/exp/nbf/iat/jti/sub/client_id；`sub == client_id`；`granted = scopes[] ∪ scope.split_whitespace()`；要求 `audit:event:write` ∈ granted。签名不验证（§7 seam）。
- **可达性**：python3 socket 对两 URL 的 host:port 建连（默认 443/80 按 scheme），2s 超时（镜像 probe_commercial 2s）。
- 每失败打印一行机器可读原因（`audit-provision-check: <class>: <detail>`），供 shell 断言 grep。

### R2 — `aero-eng audit-provision-check` 命令（distinct codes 穿透）
`crates/aero-cli/src/main.rs` 新增 `AuditProvisionCheck_`（`c!` 宏，name `"audit-provision-check"`，`a!(AuditProvisionCheck_)` 注册，`Completion_` cmds 串同步加）：
- 用 **`tokio::process::Command` 直接 spawn** `scripts/audit-provision-check.sh`（E8：`run_cmd` 会拍平退出码），透传 `--gate`（由 `args[2]`）与当前 env；
- 脚本退出码映射：0 → `Outcome::ok`；1 → `Outcome::error`（exit 1）；2..=6 → `Outcome::warning(code, message)`——`execute_with_ctx` Err 分支 → `main.rs exit(code)`（E8 已验证穿透）；
- 超时 120s（`Gate_` 其他脚本同款）；stdout/stderr 透传。

### R3 — `aero-eng gate audit`（Gate_ bash-script 模式接线）
`Gate_` match 增 `"audit" => b(f("audit-provision-check.sh"), 120).await`（**现有 `b()` helper 原样**——门 = pass/fail，非零拍平为 exit 1 即可）；`gate list` 串加 `audit`。**CI 接线**：`scripts/test-integration.sh` B5 段以配给 env（AERO_AUDIT_* + mock token + loopback stub）调用 `aero-eng gate audit`，非零即 fail（脚本已 `set -euo pipefail`）——满足 "fails CI when any of the above holds"。**不并入 `gate all`**（dev CI 无 AERO_AUDIT_* env，并入会让无条件门全红——语义记录 §7）。

### R4 — readyz 不翻转回归（acceptance 3，零生产代码改动）
`crates/aero-server/src/routes/health.rs` tests 模块新增单测 `readyz_never_flips_on_relay_unreachability`：
- 参数化断言 `deps_ok` 语义表：commercial ∈ {"ok", "disabled"} → `readiness_decision(false, true)` = (200, "ready")；{"not_ready", "fail", "timeout"} → (503, "not_ready")；
- 不变量注释 + 断言：**sink 不可达无法产出 "not_ready"/"fail"/"timeout"**——`probe_commercial` 的输入只有 runtime 存在性 + projection `ready()`（:67-70 注释），无 sink 可达性探针；relay 缺席 = "disabled" ∈ 就绪集。即 readyz 永不因 relay 缺席/不可达从 ok 翻 fail（镜像 "not_ready"/"disabled" 语义）。
- 既有 `readiness_decision_*` 三单测保持绿（:240-256 不动）。

### R5 — 37/37 契约清单钉入（acceptance 4）
`scripts/test-integration.sh` 把 A3 pin 占位（E9）替换为**具名清单常量**：
```bash
# B5 contract test list pinning (proposal line 15). 37 slots; entries whose
# contract text has not landed are [PROPOSED] and must stay explicitly listed
# (never silently dropped); in-repo entries must resolve to executable tests.
B5_CONTRACT_TEST_LIST=( ... 37 entries ... )
```
- 每项 = 命名测试/drill 标识符；**仓内可执行项**（B5-1 parity db_tests、B5-2 connector relay 测试含 403→dead/422/409→dead/lease>2×timeout/fencing、B5-3 moderation 优先 drill、T-11 provision-check 退出码 drill 等既有命名条目）必须能在本脚本内解析执行；**仓外契约项**显式标 `[PROPOSED]`（内容不臆造——proposal :13）；
- 脚本断言：清单非空、恰 37 槽、每非-[PROPOSED] 项有对应执行段（缺失即 fail）；
- `audit-provision-check.sh --gate` 的 code 6 守卫：grep 到 `B5_CONTRACT_TEST_LIST` 标记且非空才通过。

### R6 — 约束
- **aero-cli 零新依赖**（E1 保持；不链接 aero-auth——sqlx 可达，E4 注）。
- **aero-server 生产代码零改动**（R4 只加测试；probe_commercial/readiness_decision/health_ready 一行不动）。
- **connector crate 零改动**（E5：check 只读 workspace 成员关系）。
- **不触碰**：migrations/、`aero-auth`、`aero-audit-connector/src/`、`snaplink_commercial/` 生产代码。
- env 命名：`AERO_AUDIT_*` 单下划线家族（E6 先例）；新 `AERO_AUDIT_PROVISION_CHECK_TOKEN`。
- 提交前门禁：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规；新脚本小体量，不触 file-size 阈值）。

## 5. Acceptance checks（direction 原样保留，逐条 testable）

> direction acceptance 原文四条，逐条保留并钉死测试面。shell 断言走 `scripts/test-integration.sh` 命名条目（throwaway 环境、`assert_disposable_db_name` 先例、`set -euo pipefail`）；aero-eng 侧断言走 `aero-eng` 实际二进制（`cargo build -p aero-cli` 后直接调用）。

### AC1 — `aero-eng audit-provision-check` exits non-zero (with distinct codes)：connector crate missing / boot gate disabled / scope absent / relay unreachable —— T-11 fail-closed
**测试 = `test-integration.sh` 命名条目 "audit-provision-check exit-code matrix"**（纯 shell/python3，无 DB、无 server boot）：
- ① 临时 workspace（`crates/` 无 aero-audit-connector + Cargo.toml 无成员项）→ **exit 2**，输出含 "connector"；
- ② 清空 AERO_AUDIT_* → **exit 3**，输出含 "boot gate"/"not configured"；
- ③ `AERO_AUDIT_PROVISION_CHECK_TOKEN` 缺 scope（scopes=["billing:entitlement:read"]）→ **exit 4**，输出含 "audit:event:write"；含 scope 的 token → 通过该步；非 at+jwt / sub≠client_id / 缺 iss → 同样 exit 4；
- ④ `AERO_AUDIT_TOKEN_ENDPOINT`/`AERO_AUDIT_EVENTS_URL` 指向关闭的 loopback 端口 → **exit 5**，输出含 "unreachable"；
- ⑤ 全绿组合（crate 在仓、endpoint 设 loopback 起 `python3 -m http.server`、token 含 scope）→ **exit 0**；
- ⑥ stray `AERO_AUDIT_*` 无 endpoint → **exit 1**（boot 配置错镜像）。
- 每条用 `bash scripts/audit-provision-check.sh …; code=$?; assert $code == N` + `grep` 消息断言。**退出码穿透**（`aero-eng audit-provision-check; echo $?` 同码）由同条目对编译后的 aero-eng 二进制复跑 ②③④ 各一次断言（E8 机制）。

### AC2 — `aero-eng gate audit` runs the check via the existing `Gate_` bash-script pattern and fails CI when any of the above holds
- **测试 = 同条目 + CI 接线**：`aero-eng gate audit`（无 AERO_AUDIT_* env）→ **exit 1**（Gate_ 拍平码，门语义）且 stderr 含失败原因；配给 env 全绿 → **exit 0**。
- CI 接线：`test-integration.sh` B5 段（或 G6 gate 上下文）以配给 env 调用 `aero-eng gate audit`，非零中止（`set -euo pipefail`）——「任何上述条件成立时 CI 失败」的仓内钉死读法；`gate list` 输出含 `audit`（断言）。

### AC3 — Test asserts readyz never flips ("ok"→"fail") when the relay/sink is unreachable —— mirrors probe_commercial semantics
- **测试 = `crates/aero-server/src/routes/health.rs` 新单测 `readyz_never_flips_on_relay_unreachability`**（R4）：commercial "ok"/"disabled" → ready（200）；并断言 probe_commercial 无 sink 可达性探针的语义（sink 不可达 ⇒ probe ∈ {"ok","disabled"} ⇒ readyz 不变）。零生产代码改动。
- 可选 drill（不进 AC1 必过面）：起 server（unreachable relay env）→ `GET /health/ready` 恒 200——因需完整 boot，标 optional（§7）。

### AC4 — Pins the 37/37 contract test list into gate scripts（proposal line 15）so the suite is executable in-repo
- **测试 = `test-integration.sh` 的 `B5_CONTRACT_TEST_LIST` 钉入断言**（R5）：清单存在、非空、恰 37 槽；每非-[PROPOSED] 项在脚本内有可执行解析段（脚本自身迭代执行，缺项 fail）；[PROPOSED] 项显式标注且**内容不臆造**（proposal :13）。
- `audit-provision-check.sh --gate`（= `aero-eng audit-provision-check --gate` / `gate audit` 内嵌）：清单标记缺失/为空 → **exit 6**（同条目断言）。
- 清单内容超出 proposal 文本的部分 = [PROPOSED]（标注即可，不虚构契约项）。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| AC1 退出码矩阵 ①②③④⑤⑥ + AC2 gate audit 双态 + AC4 exit 6 | `scripts/test-integration.sh` 命名条目（`run_migrated_integration` 同级 shell 段，无 DB） | bash + python3 + 编译后 aero-eng 二进制 |
| AC2 `gate list` 含 audit / AC1 退出码穿透 | 同上（对 `cargo build -p aero-cli` 产物断言） | shell |
| AC3 readyz 不翻转参数化单测 | `crates/aero-server/src/routes/health.rs` tests 模块（新测试，既有 `readiness_decision_*` 不动） | `cargo test -p aero-server routes::health`（无 DB） |
| AC4 37/37 清单钉入自检 | `scripts/test-integration.sh` 头部 `B5_CONTRACT_TEST_LIST` + 断言段 | shell |
| 回归（aero-eng 命令注册/help/completion 不破坏） | `cargo test --workspace --lib` + `aero-eng help` 冒烟 | cargo |

## 7. Risks / [PROPOSED] / 决策点

- **落家冲突（E11）**：sibling spec（aero-auth B5-4）把 `audit-provision-check` 放 `crates/aero-server/src/bin/aero-cli.rs`（DB 能力 CLI），本 direction 定死 `crates/aero-cli`（aero-eng）。**本 spec 的 acceptance（"`aero-eng audit-provision-check`"）为准**；两个 CLI 并存不冲突（aero-server bin 的 16 命令面不受影响），但 campaign 级需在集成时确认 sibling spec 的 R4 CLI 段落与本 direction 合并（其 R4 改为消费本 seam 的脚本/退出码，或删去该段）——**集成决策，非本 direction 执行**。
- **`gate audit` 不并入 `gate all`**：acceptance 2 的 "fails CI when any of the above holds" 由 B5/G6 门上下文（test-integration.sh B5 段）调用实现；并入 `gate all` 会让无 AERO_AUDIT_* env 的普通 dev CI 全红（boot gate disabled = exit 3 是 fail-closed 的**正确答案**，不是 bug）。语义：feature-off 部署跑 `gate audit` 得 exit 3 = "配给被拒"（T-11），CI 只在配给验证上下文跑它。
- **token 签名验证 = [PROPOSED] seam**：脚本只做结构性校验（at+jwt/iss/aud/sub=client_id/scope 并集），不做 RS256 签名验证（需 IdP JWKS，仓外）。acceptance 只要求检出 scope 缺席，结构校验足够；完整验证 = 未来 seam（可换 aero-auth `validate_client_credentials_token` 的 KeyProvider 面，但 aero-cli 保持零新依赖）。
- **37/37 清单内容仓外（proposal :13）**：本 direction 钉「37 槽具名清单 + 仓内项可执行 + 仓外项显式 [PROPOSED]」，**不臆造契约项**；契约文本落仓后仅需把 [PROPOSED] 槽替换为真实项（seam 已就位）。"37" 这个计数本身也是契约来源（[PROPOSED]），钉为常量便于契约落地时核对。
- **distinct codes 的边界**：`aero-eng audit-provision-check` 经 `Outcome::warning(code)` 穿透（E8 已验证）；`gate audit` 经 `run_cmd` 拍平为 1——**门语义只要求非零**，distinct codes 属检查命令/脚本层契约（AC1 断言对象），不要求 gate 层保留。
- **脚本零新依赖**：bash + python3（仓库既有脚本栈，`smoke_*.py` 先例）；不用 curl（不必备）、不用 jq（python3 json 内联）。
- **reachability 检查的假阳性**：TCP dial 只验传输层（镜像 `Network_` port 探针与 probe_commercial 的 2s 超时语义），不验 TLS/HTTP——"relay works" 的完整判定（202 receipt / settle）是 sibling spec（aero-auth B5-4 心跳）的领域，本 seam 定位 = **pre-flight 配给预检**；两 seam 互补不重叠。

## 8. Sequencing

1. **脚本**：`scripts/audit-provision-check.sh`（R1 退出码矩阵 + python3 校验段）——纯 shell，可独立 `bash scripts/audit-provision-check.sh; echo $?` 验证。
2. **aero-eng 命令**：`AuditProvisionCheck_`（R2，直接 spawn 保码）+ `a!()` 注册 + `Gate_` `audit` 子命令 + `gate list`/`Completion_` cmds 串（R3）。
3. **readyz 回归**：health.rs tests 增 `readyz_never_flips_on_relay_unreachability`（R4，零生产代码改动）。
4. **钉入 + drills**：test-integration.sh 的 `B5_CONTRACT_TEST_LIST`（R5）+ AC1/AC2/AC4 命名条目。
5. **CI 接线**：B5/G6 门上下文调用 `aero-eng gate audit`（配给 env）；确认 sibling spec 集成面（§7 落家冲突）。
6. **门禁**：`cargo check --workspace`（干净）· `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· `scripts/test-integration.sh`（AC1/AC2/AC4 全绿，AC3 随 `cargo test -p aero-server`）· no-touch 守卫（`git diff --stat` 不含 migrations/、aero-auth、aero-audit-connector/src/、snaplink_commercial/ 生产代码）。
