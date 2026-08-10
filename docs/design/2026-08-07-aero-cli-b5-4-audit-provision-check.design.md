# Design — aero-cli B5-4：`aero-eng audit-provision-check` + fail-closed gate 接线（落家 = `crates/aero-cli` aero-eng 二进制）

- **Direction**: "B5-4: add `audit-provision-check` + fail-closed gate wiring to aero-cli"（value 9 / risk_reduction 9 / effort 5 / confidence 9）
- **Requirements**: `docs/requirements/2026-08-07-aero-cli-b5-4-audit-provision-check.req.md`（R1–R6, AC1–AC4）
- **Status**: Design（证据全部逐条复核；11 处实现偏差见 §7：D1 gate 直 spawn 带 `--gate`、D2 code-1 扩展为完整 `from_env` Err 面镜像、D3 脚本不用 `set -e`、D4 code-4 时间窗 **strict-no-leeway**（刻意严于 connector leeway 60）、D5 **白名单整体删除**（token+无 endpoint → 1）、D8 fake-tree 四臂 + 四格模式隔离、D11 P6 双层镜像（运行时门 + aero-auth 结构超集）、D12 token 泄漏 pin、D13 P4/P3 镜像机制 pin（env 扫描/awk/URL 规则 + R1-R5 假红文档）、D14 AC1 负向断言与确定性边界、D15 清单成员断言 + 文本级解析 + hoist 放置）
- **Verification date**: 2026-08-07。行号为复核锚点，会漂移——**文件/符号**为准（AGENTS.md §0）

## 0. Evidence adjudication（untrusted claims → verified anchors）

| Evidence claim | Verdict | Verified anchor（实际） |
|---|---|---|
| `crates/aero-cli/src/main.rs` — `a!()` 注册表 + `Gate_`（`run_cmd` 跑 scripts/*.sh）+ `Network_`（tokio TCP dial）；Cargo.toml deps 仅 aero-eng/tokio/serde_json/async-trait | ✅ 精确命中 | `main.rs`：`macro_rules! a` :18、注册 :23-31（check/gate/test/skill/doctor/completion/network/bench/dashboard 9 命令）；`c!(Gate_, "gate", …)` :104-112，`b()` helper :106-108 = `run_cmd("bash", &[path], timeout)`；match 臂 filesize/truth/web/deps/complexity/all :113-119；`Network_` :358/:408 `tokio::net::TcpStream::connect`；`Completion_` cmds 串 :307 = "check gate test skill doctor network completion help"；`c!` 宏 :48-55；main 退出路径 :32-43（Err → `std::process::exit(r.exit_code())`）。`crates/aero-cli/Cargo.toml` deps = aero-eng/tokio/serde_json/async-trait，文件头注释 "no DB dependencies"。**补充**：root `Cargo.toml:80` tokio = `features = ["full"]`（含 process）——`tokio::process::Command` 可直接用，**零 Cargo.toml 改动** |
| `routes/health.rs` — `probe_commercial` "disabled"/"not_ready"，无 sink 可达性探针；`health_ready` `matches!("ok"\|"disabled")`；`readiness_decision` 纯函数 | ✅ 精确命中 | `probe_commercial` :69-79，注释 :67-68 "A reachable central service is intentionally not a probe dependency"；`health_ready` :166-199，deps_ok 对 commercial 用 `matches!(commercial, "ok" \| "disabled")` :186；`readiness_decision` :151 纯函数；既有单测 :235-253（draining/healthy/degraded 三例） |
| `snaplink_commercial/http.rs:23` `SCOPE_AUDIT`、:410 `validate_audit_receipt` | ✅ 精确命中 | `SCOPE_AUDIT: &str = "audit:event:write"` :23（同文件 `SCOPE_ENTITLEMENT`/`SCOPE_USAGE` :21-22）；`validate_audit_receipt` :410-423（event_id 匹配 + tenant_id + accepted_at + !conflict + status ∈ {ledgered,indexed,archived}） |
| `aero-auth/oidc.rs` — `validate_client_credentials_token` :406、`granted_scopes` :387（RFC 9068 at+jwt） | ✅ 精确命中 | `ClientCredentialsClaims` :372-385（`scopes`/`scope` 均 `#[serde(default)]` :377-380）；`granted_scopes()` :387-394 = scopes 数组 ∪ scope 空格串并集；`validate_client_credentials_token` :406-471（typ 必填且 `eq_ignore_ascii_case` ∈ {at+jwt, application/at+jwt} :419-423、alg ∈ {RS256, EdDSA} :425-431、iss/aud/exp/nbf/iat/jti/sub/client_id 全必填、`sub == client_id` + `valid_identity_component`（非空/trim/无控制字符）:445-447、`iat > now+60` 拒 :453、jti 非空/≤1024 字节/无控制字符 :458、`required_scopes ⊆ granted_scopes`） |
| **复核 1（B5-4 硬化）**：connector 运行时 token 门 = `client.rs::validate_token_claims`，**aero-auth 不是 connector 依赖** | ✅ 确认（设计修正） | `client.rs:197-259`：iss==`AERO_AUDIT_EXPECTED_ISS`、aud string/数组含 expected_aud、**`scope` claim 单词或数组含 expected_scope——不读 `scopes[]`**、sub==`AERO_AUDIT_EXPECTED_SUB`、形状非空/≤16KB/无控制字符 :199-202；`client.rs:415-431` decode = 恰 3 段 + URL_SAFE_NO_PAD 优先/URL_SAFE 容错 padding + JSON；`client.rs:126` 每次 POST 前调用；connector src/ + Cargo.toml 全仓 grep `aero-auth` = 0 命中 |
| **复核 2（B5-4 硬化）**：`LEEWAY_SECS=60`（oidc.rs:293；jsonwebtoken `Validation::new` 默认 leeway=60、`reject_tokens_expiring_in_less_than=0`）——exp 拒 iff `exp < now-60`、nbf 拒 iff `nbf > now+60`（validation.rs:272-280） | ✅ 精确命中 | 设计 D4 原 "exp<=now 或 nbf>now" = leeway 内假红，且缺 iat 未来窗 = 假绿 → §7 D4 修正 |
| **复核 3（B5-4 硬化）**：stray 扫描 = `vars_os().starts_with("AERO_AUDIT_")` 前缀计数（config.rs:54-56），**无白名单**——`AERO_AUDIT_ALLOW_INSECURE_LOOPBACK` 是真实 connector 变量（config.rs:65） | ✅ 确认（设计修正） | 原设计 P4 白名单含 allow-insecure = 镜像失配；进一步（G5）白名单**整体删除**——{token, 无 endpoint} 预检报 1 而非 3（boot 必 Err）→ §2.1 P4 + §7 D5；`from_env` 其余 Err 类（bool/duration×5/不变量×2/整数×2/identity×6/secret）→ §7 D2 |
| **复核 4（shell_script 硬化）**：url 2.5.4（lockfile :5528-5530 钉死）离线探针实证——`parse_port` 仅拒 >u16::MAX（99999/abc → "invalid port number"）、port 0 → `Some(0)`、空端口 `host:` 丢弃（parser.rs:1104-1135）；`host_str()` 取序列化切片（lib.rs:1153-1159）→ IPv6 带括号 `[::1]` → `IpAddr` 解析失败 → 永不 loopback；`[127.0.0.1]` → "invalid IPv6 address"；host 空白 → "invalid international domain name"；`user:@host` 保留 username（pass=None）→ 拒；`@host` 过；`http:///path` → host="path"；path 空白接受（percent-encode）；`http://host/?`/`#` → query/fragment `Some("")` | ✅ 精确命中 | → §2.1 P4 URL 规则显式化（G1-G5）+ R1-R5 假红文档（§7 D13）；drill 只用 canonical 127.0.0.1（R2） |
| B5-2 connector 已在 workspace（"crate missing" 是可检测状态非当前态） | ✅ 精确命中 | `crates/aero-audit-connector/` 存在：src/{client,config,outbox,pg,relay,stub,fake,lib}.rs + bin/ + tests/{state_machine,claim_validation}.rs；root `Cargo.toml:29` members、:77 `[workspace.dependencies]`。全仓 grep `audit-provision-check` 仅命中 docs（proposal + req 文档），零代码 |
| 退出码机制：`Outcome::warning(code)` 穿透到进程退出码；`run_cmd` 拍平脚本码为 1 | ✅ 精确命中 | `crates/aero-eng/src/outcome.rs`：`ok` :35 / `warning(exit_code, …)` :47 / `error` :59（exit 1）/ `skip` :71（exit 2）；`exit_code()` :103；`merge` :133-160。`registry.rs::execute_with_ctx` :94-124 非 ok 走 `Err` 分支。`run.rs::run_cmd` :15-41：非零 → `Outcome::error`（exit 1）——**distinct codes 必须直 spawn** |
| v2 relay boot 门 = `RelayConfig::from_env` presence-gate | ✅ 精确命中 | `config.rs:49-63`：无 `AERO_AUDIT_TOKEN_ENDPOINT` 且无其他 `AERO_AUDIT_*` → `Ok(None)`；有 stray 而无 endpoint → `bail!`（fail-loud）；有 endpoint → `required_env("AERO_AUDIT_EVENTS_URL")` :68（**必填**）；`service_url` :135-151（https 或 http 仅 loopback 且 `AERO_AUDIT_ALLOW_INSECURE_LOOPBACK`，拒凭据/query/fragment/超 2048 字节）；`expected_scope` :89-91 默认 `"audit:event:write"`。接线 `crates/aero-server/src/bin/main.rs:245-269` |
| v1 boot 门 = `CommercialMode::from_env`（`AERO_SNAPLINK_COMMERCIAL_ENABLED`） | ✅ 精确命中 | `snaplink_commercial/config.rs:151-158`（Unspecified→`require_disabled` / Disabled→`configure_disabled` / Enabled→`configure_enabled`）；`runtime.rs:42-66` |
| 37/37 pin seam 已占位 | ✅ 精确命中 | `scripts/test-integration.sh:265-269` "A3 part 1 (pin)" 注释（"its in-repo-verifiable subset is pinned here as an explicit named list"） |
| 门禁/契约锚点（implementation-gate :64/:78；proposal :11/:13/:15） | ✅ 精确命中 | `docs/campaigns/implementation-gate.md` aero-im 行 :63-64（"T-11（无 relay 配给被拒）"、B5-1 "30 个忽略测试 CI 全绿（37/37）"）、:78（G6 = "37/37、T-11、moderation 优先级"）；`docs/proposals/audit-contract-batch-aero-im.md:11`（B5-4 seam [PROPOSED]）、:13（[PROPOSED] 标注） |
| 落家冲突：sibling spec 选了 `aero-server/src/bin/aero-cli.rs` | ✅ 精确命中 | `docs/requirements/2026-08-07-aero-auth-b5-4-relay-provisioning-gate.req.md` R4 :104 = "`aero-cli audit-provision-check` 落家（`crates/aero-server/src/bin/aero-cli.rs`）"（702 行 15 命令 DB CLI）。本 direction acceptance 原词 "`aero-eng audit-provision-check`" 为准（§7 D0） |
| **发现（req 未覆盖，本设计补）**：`b()` helper 只收 (path, timeout)，**无法传 `--gate`** | ✅ 确认 | `main.rs:106-108` `async fn b(p: String, t: u64)`——spec R3 字面 `b(f("audit-provision-check.sh"), 120)` 跑不出 code-6 钉入守卫 → §7 D1 |
| **发现（req 未覆盖，本设计补）**：`service_url` 的 boot-Err 类（events-url 必填、http 仅 loopback 且 opt-in）在 req code-1 行未列 | ✅ 确认 | `config.rs:68,135-151`——preflight 报 0 而 boot fail = 假绿，违反 fail-closed 核心不变量 → §7 D2 |
| **发现（req 未覆盖，本设计补）**：code-4 结构校验缺 exp/nbf 时间窗（oidc 标准校验含之） | ✅ 确认 | `oidc.rs:406-471` StandardClaims 校验——过期 token 结构合法会假绿 → §7 D4 |

**勘误/细化汇总**：① 行号与 req 记录基本一致（仅 probe_commercial 注释 :67-68、health_ready matches :186 微漂）；② **关键新事实**：tokio workspace features=["full"] → 直 spawn 零依赖改动；`AERO_AUDIT_EXPECTED_SCOPE` 默认即 `audit:event:write`（code-4 需求 scope 有配置镜像面）；`AERO_AUDIT_PROVISION_CHECK_TOKEN` 落入 `AERO_AUDIT_` 前缀 → server 进程 stray 扫描会把它当 stray（§3 C5）；③ `Completion_` cmds 串本已缺 bench/dashboard（存量过期，非本 direction 修）；④ migrations 尾号 0238（0239 未落库——B5-1 段按文件门跳过的现状保持）；⑤ **硬化复核新增事实**：connector 运行时 token 门 = `client.rs::validate_token_claims`（aero-auth 非依赖，见复核 1）；`LEEWAY_SECS=60`（复核 2）；stray 扫描无白名单（复核 3）→ P4 白名单删除、P4 补 from_env 值类 Err 面、P6 双层镜像（§7 D2/D4/D5/D11/D12）；url 2.5.4 实证（复核 4）→ URL 规则显式化 + R1-R5 假红文档（§7 D13）。

## 1. Design overview

```
┌─ 单进程预检（无 DB、无 server boot、无新依赖：bash + python3）─────────────┐
│ scripts/audit-provision-check.sh [--gate]                                │
│   P1 repo root 可读 ──✗→ 1                                               │
│   P2 python3 存在 ──✗→ 1                                                 │
│   P3 connector 在 workspace（dir + members + workspace.dependencies）─✗→ 2│
│   P4 boot 配置错镜像（from_env 全 Err 面：stray / 必填 / URL / bool /    │
│      数值范围 / 不变量 / identity / secret）──✗→ 1                       │
│   P5 boot gate disabled（endpoint 缺席且无 stray）──✗→ 3                  │
│   P6 token 结构校验（双层：运行时门 validate_token_claims 接受判定 +     │
│      aero-auth 结构超集：typ 大小写不敏感 / alg / 8 claim / jti /         │
│      sub==client_id / exp≤now·nbf>now·iat>now（no-leeway，严于 boot）──✗→ 4│
│   P7 双 URL host:port TCP dial（2s）──✗→ 5                               │
│   P8 [--gate] test-integration.sh 含 B5_CONTRACT_TEST_LIST 且非空 ──✗→ 6 │
│   ──✓→ 0                                                                 │
└──────────────────────────────────────────────────────────────────────────┘
        │ 直 spawn（tokio::process::Command，保退出码）
        ▼
crates/aero-cli/src/main.rs
  AuditProvisionCheck_（新命令，a!() 注册）      Gate_ 增 "audit" 臂（直 spawn --gate，
    0→ok / 1→error / 2..=6→warning(code)          非零拍平 Outcome::error = exit 1）
    └─ Err 分支 → main.rs exit(code)（已验穿透）   gate list 串 += "audit"
  Completion_ cmds 串 += "audit-provision-check"
        │
        ▼
scripts/test-integration.sh
  B5_CONTRACT_TEST_LIST（37 槽具名清单：仓内项可执行 + 仓外项 [PROPOSED-NN]）
  命名条目 "audit-provision-check exit-code matrix"（AC1/AC2/AC4 drills）
  B5 段 CI 接线：配给 env 下调用 aero-eng gate audit（set -euo pipefail 中止）
        │
        ▼
crates/aero-server/src/routes/health.rs —— tests 模块新增
  readyz_never_flips_on_relay_unreachability（零生产代码改动）
```

**定位**：pre-flight 配给预检（"relay 会不会工作" 的 fail-closed 前置判定），非投递健康（202 receipt / settle = sibling spec aero-auth B5-4 心跳的领域）；两 seam 互补不重叠。**核心不变量：preflight 永不比 boot 更绿**——凡 `RelayConfig::from_env` 会 Err 的 env 面，脚本必须报 1；凡 relay 不会 boot 的 env 面（endpoint 缺席），脚本必须报 3。

## 2. API changes（逐文件签名）

### 2.1 `scripts/audit-provision-check.sh`（新文件，~230 行 bash + 内联 python3 heredoc）

```
用法：bash scripts/audit-provision-check.sh [--gate]
退出码：0 ok / 1 usage·config / 2 connector missing / 3 boot gate disabled /
       4 scope absent / 5 relay unreachable / 6 pinning missing（仅 --gate）
输出：每失败一行 stderr "audit-provision-check: <class>: <detail>"（class ∈ {usage, config, connector, boot gate, scope, unreachable, pinning, internal}；python 阶段同前缀写 sys.stderr，成功零 stderr；shell grep 断言面）
```

- **P1 root 自解析**：先绝对化再取 root——`src="${BASH_SOURCE[0]}"; case "$src" in /*) ;; *) src="$PWD/$src";; esac; ROOT="$(cd "$(dirname "$src")/.." && pwd)"`（相对路径调用 `bash scripts/audit-provision-check.sh` 在非 root cwd 下裸 `dirname` 是相对路径 → 错误 root，F5；aero-eng 传绝对路径，生产安全）；`${BASH_SOURCE[0]:-}` 守卫：sh 直调无 BASH_SOURCE → 前缀 usage 行 + `exit 1`。root 不可读/不存在 → `exit 1`（"usage: repo root not readable"）。
- **P2**：`command -v python3` 缺席 → `exit 1`（"usage: python3 required"）。
- **P3 connector 存在性**：`[ -d "$ROOT/crates/aero-audit-connector" ]` 且 **awk 段作用域 FSM**（F2——整文件 `grep -q` 会假过注释掉的条目、无法归段、且 path 串出现在错误 key 下也假过）：`^\[` 行切 section（任何 `[workspace.package]`/`[workspace.dependencies]` 等头都结束上一段作用域）；首个非空白字符为 `#` 的行跳过（注释安全；行内注释无碍——锚点仍命中）；`[workspace]` 段内匹配成员串 `"crates/aero-audit-connector"`（**带闭引号**——`"crates/aero-audit-connector-x"` 不得别名）；`[workspace.dependencies]` 段内匹配 **key** `^aero-audit-connector[[:space:]]*=`（错误 key 下的 path 串必须 fail）→ 两臂独立 detail（"connector: not in workspace members" / "connector: not in workspace.dependencies"），均 `exit 2`。真仓 :29/:77 双命中 → 基线通过。
- **P4 boot 配置错镜像**（`RelayConfig::from_env` **全部** Err 类——§7 D2；7 个必填身份/密钥变量 + 5 duration + 2 integer + 2 不变量 + URL/bool/stray 全镜像，G6）：python3 统一实现（与 P6 同一解释器），任一命中 → `exit 1`。**前置：所有 env 读取先镜像 `optional_env`（trim + drop-empty，F3d）**——endpoint/events/required 判定、URL 2048 输入、bool 解析、token 读取同一语义；`AERO_AUDIT_EVENTS_URL=" "` → Err（必填判定用 trim 后值）：
  - **stray 前缀扫描**：`AERO_AUDIT_TOKEN_ENDPOINT` 缺席（trim 后为空 = 缺席）时，对**全部 env 名**做 `starts_with("AERO_AUDIT_")` 前缀计数（config.rs:54-56 同语义）。**无白名单**（§7 D5——G5 决策）：`AERO_AUDIT_PROVISION_CHECK_TOKEN`、`AERO_AUDIT_ALLOW_INSECURE_LOOPBACK`（真实 connector 变量，config.rs:65）均计入 stray——{token, 无 endpoint} 在 boot 必 Err（stray bail），预检必须报 1 而非 3。扫描实现 = python3 `os.environ` 键集（C environ 镜像 `vars_os()`，等同 `env` 扫描）；**禁 `${!AERO_AUDIT_*}`**（F1c：bash 只持有合法标识符名，`AERO_AUDIT_FOO-BAR` 这类 `vars_os()` 会计数的名字 `${!…}` 静默漏计 → 假绿）。计数 >0 → `exit 1`（"config: AERO_AUDIT_TOKEN_ENDPOINT unset while N AERO_AUDIT_* var(s) are present"）。脚本内读取 allow-insecure 做 URL 判定不冲突（endpoint 在时不进 stray 分支）；
  - `AERO_AUDIT_TOKEN_ENDPOINT` 有而 `AERO_AUDIT_EVENTS_URL` 无（trim 语义）→ `exit 1`（"config: AERO_AUDIT_EVENTS_URL required"——`required_env` :67 镜像）；
  - `AERO_AUDIT_ALLOW_INSECURE_LOOPBACK` 值（trim + 小写）∉ {true, 1, false, 0} → `exit 1`（`bool_or_default` :65/:117-129 镜像）；
  - 两 URL 任一 → `exit 1`（"config: <var-name> must use HTTPS (HTTP is opt-in and loopback-only)"——`service_url` :135-151 镜像；**url 2.5.4 实证规则，§7 D13**）：**G1** raw（trim 后）串含 `?` 或 `#` 即拒（Rust `query()/fragment()` 对空串也是 `Some("")`，urlsplit 无法区分）；**G2** netloc 含 `:port` 时 digits-only 且 1..=65535（99999/abc → Rust InvalidPort；空端口 `host:` Rust 丢弃 → 通过；**端口 0 Rust 接受 `Some(0)`** → 脚本拒 0 = 假红面 R 文档）；**G3** hostname 含 `:`（IPv6）**永不 loopback**（Rust `host_str()` 返回带括号 `[::1]` → `IpAddr` 解析失败 → is_loopback 恒 false——http IPv6 loopback 当前 boot 不可能）；**G4** netloc 以 `[` 开头 → hostname 必须为合法 IPv6（含 `:`）否则拒（`[127.0.0.1]` → Rust "invalid IPv6 address"）；**G5** host 段含空白/控制字符 → 拒（"invalid international domain name"）；path 段空白/控制字符**接受**（R3——Rust percent-encode，不整串拒）；含凭据（拒 iff username∉{None,''} 或 password∉{None,''}——`user:@host` Rust 丢空密码但保留 username → 仍拒；`@host` 过，R1）；含 query/fragment（G1 raw 检查已覆盖）；字节长 ≥2048 拒（R5：Rust 检查**归一化后** `as_str()` 长度，可涨可缩——脚本保守镜像 raw 字节 ≥2048，raw 恰 2048 而归一化收缩 = 假红安全）；scheme ∉ {https} ∪ {http ∧ allow-insecure ∧ loopback}；loopback = host 大小写不敏感 "localhost" 或 IP 解析 `is_loopback`（127.0.0.0/8），**不做 DNS 解析**（`http://localhost.evil.com` 非 loopback）；R2 假红文档：IPv4 简写（127.1/0x7f.0.0.1/2130706433/127.0.0.1.）Rust 归一化为 127.0.0.1 → loopback，python 无法复刻——**drill 只用 canonical 127.0.0.1/localhost**；R4 `http:///path` Rust host="path" → 脚本 hostname-None 拒 = 假红；python3 `urllib.parse` + `ipaddress`；
  - 数值类（解析失败或越界 → 1）：`AERO_AUDIT_REQUEST_TIMEOUT_SECS` 1..=120、`AERO_AUDIT_DELIVERY_LEASE_SECS` 5..=300、`AERO_AUDIT_SHUTDOWN_DRAIN_SECS` 1..=60、`AERO_AUDIT_TASK_DRAIN_SECS` 1..=600、`AERO_AUDIT_POLL_INTERVAL_SECS` 1..=300；lease 不变量 `delivery_lease > 2×request_timeout+2`、drain 不变量 `task_drain > 2×request_timeout+shutdown_drain+2`（:155-171 镜像）；`AERO_AUDIT_BATCH_SIZE` 1..=500、`AERO_AUDIT_CONCURRENCY` 1..=32；
  - 身份类 7 个必填变量（`identity_env` :131-139 / `secret_env` :141-148 镜像：必填 = trim 后非空、长度 ≤ 上限、无控制字符；Rust 的 "trim 后不变" 防御检查经 optional_env 先行 trim 后恒真，无需镜像）：`AERO_AUDIT_RESOURCE`≤512、`AERO_AUDIT_CLIENT_ID`≤512、`AERO_AUDIT_EXPECTED_ISS/AUD/SUB/SOURCE_SYSTEM`≤256；`AERO_AUDIT_CLIENT_SECRET` ≥32 字节、≤16384 字节、无控制字符。
- **P5 boot gate**：`AERO_AUDIT_TOKEN_ENDPOINT` 缺席且**零个** `AERO_AUDIT_*` 变量（白名单已删——任何 stray 都在 P4 先报 1，§7 D5）→ `exit 3`（"boot gate: AERO_AUDIT_TOKEN_ENDPOINT not set (relay not provisioned; fail-closed)"）——T-11 的仓内钉死读法。
- **P6 token 结构校验**（python3，**双层镜像**——§7 D11：接受判定 = 运行时门 `client.rs::validate_token_claims`（connector 真实拒收语义，aero-auth 非依赖），额外拒绝条件 = aero-auth `validate_client_credentials_token` 结构超集（假红安全、文档化）；aero-cli 零 DB 依赖约束不变）：`AERO_AUDIT_PROVISION_CHECK_TOKEN` 缺席 → `exit 4`（"scope: no token supplied"）。
  - **层 R（运行时门，接受判定——失败即 4，缺一不可）**：token 读取走 `optional_env` trim 语义（trim 后空 = 未提供 → 4，F3d）；非空、≤16,384 字节（按 trim 后字节计）、无控制字符（client.rs:199-202）；恰 3 个 JWT 段（client.rs:415-431）；payload base64url 解码（URL_SAFE_NO_PAD 优先、URL_SAFE 容错 padding）且 JSON 可解析；`iss` 为 string 且 == `AERO_AUDIT_EXPECTED_ISS`；`aud` 为 string == `AERO_AUDIT_EXPECTED_AUD` 或 string 数组含之；`scope` claim 为 string（`split()` 单词含期望 scope）或 string 数组含期望 scope——**`scopes[]` 数组不参与判定**（union 语义接受仅 scopes[] 的 token = 假绿，§7 D11）；`sub` 为 string 且 == `AERO_AUDIT_EXPECTED_SUB`。期望 scope = `AERO_AUDIT_EXPECTED_SCOPE` env 或默认 `"audit:event:write"`（config.rs:89-91，§7 D7）；期望 iss/aud/sub 由 P4 保证（endpoint 在时必填）；scope/iss/aud/sub 比对**大小写敏感**（运行时 `==` 语义，与 typ 的大小写不敏感相反）。
  - **层 A（aero-auth 结构超集，额外拒绝条件）**：header 可解码；`typ` 必填且**大小写不敏感** ∈ {at+jwt, application/at+jwt}（oidc.rs:419-423——`eq_ignore_ascii_case`，"AT+JWT" 必须通过）；`alg` ∈ {RS256, EdDSA}（:425-431）；payload 缺 iss/aud/exp/nbf/iat/jti/sub/client_id 任一 → 4；类型约束：exp/nbf/iat 为 JSON 整数（bool/float/string → 4，u64 语义）、sub/jti/client_id 为 string、iss/aud 为 string 或 string 数组；`sub == client_id` 且二者满足 identity-component（非空、trim、无控制字符，:445-447）；`exp ≤ now` / `nbf > now` / `iat > now` → 4（**strict-no-leeway**——connector jsonwebtoken leeway=60 接受 exp∈[now−60,now]、nbf∈[now,now+60]，脚本刻意更红 ≤60s 窗口（假红安全，文档化）；aero-auth 的 `iat>now+60` 收紧为 `iat>now`；§7 D4）；`jti` 非空、≤1024 字节、无控制字符（:458）。
  - 签名不验证（[PROPOSED] seam，§7 D9）；失败消息 = 静态文本 + 变量名 + 期望 scope 字面量，token 值永不回显（§7 D12）。
- **P7 可达性**：python3 `socket.create_connection((host, port), timeout=2)` 对两 URL host:port（scheme 默认 443/80）各一次，任一失败 → `exit 5`（"unreachable: <var-name> <host>:<port>"——**不回显 URL 路径/查询**，仓库惯例：oidc.rs `JwksUriError`、client.rs `transport_error` 用 `without_url()`、connector `service_url` 错误只点名变量）——2s 超时镜像 `probe_commercial` :72。
- **P8 钉入守卫（仅 `--gate`）**：awk 锚定**赋值行** `^B5_CONTRACT_TEST_LIST=\(`（裸 `grep -q 'B5_CONTRACT_TEST_LIST'` 会假过注释掉的清单 `# B5_CONTRACT_TEST_LIST=(…)`，F6）且赋值行括号内条目计数 ≥1（非空）→ 否则 `exit 6`（"pinning: B5_CONTRACT_TEST_LIST missing or empty in scripts/test-integration.sh"）。非 `--gate` 模式跳过。
- **控制流**：**不用 `set -euo pipefail`**（§7 D3）——显式 if/else + `exit N` 短路，首个失败即退出对应码；**P4 / P6 / P7 为三个独立 python3 阶段**（各自 `python3 - <<'PY' … PY || exit N`，映射 1 / 4 / 5——单 blob 混阶段会把 distinct codes 拍平，FM8 同族）；**参数校验最先**：恰接受 `[]` 或 `["--gate"]`，其余 → 前缀 usage + `exit 1`（aero-eng 透传 `args.skip(2)`，`--bogus` 不得静默吞掉，F6）；脚本零副作用（纯读 env/文件/网络），幂等。**退出码矩阵表写入脚本头注释**（防未来编辑重排阶段序，F6）。
- **token 泄漏 pin（§7 D12）**：token 只经 env 进入（脚本与 python 均 `os.environ` 读取；heredoc 用**单引号定界符**——bash 零展开，**禁止 bash 插值 token 进 python 正文**，否则 traceback 源码行打印会泄漏字面量）；argv 契约 = 仅接受 `--gate`，其余任何参数 → `exit 1` "usage: unexpected argument" 且不回显参数值；失败消息只含静态文本 + 变量名 + 计数（期望 scope 字面量除外）；python 全程 `try/except` → `audit-provision-check: internal: <静态消息>` + `sys.exit(1)`（异常文本可能携带 env 派生值，如 int() 的 ValueError 含原串——**无 traceback**，`internal` 入 class 词表，F5）；禁 `set -x`/`set -v`/`PS4`（展开命令行会回显 env 赋值）；零临时文件、零 `export`/re-export。

### 2.2 `crates/aero-cli/src/main.rs`（仅增不改现有臂）

```rust
c!(
    AuditProvisionCheck_, "audit-provision-check",
    "Verify audit relay provisioning (fail-closed; distinct exit codes)",
    |ctx, args| {
        let sd = ctx.root.join("scripts");
        let script = sd.join("audit-provision-check.sh");
        let extra: Vec<&str> = args.iter().skip(2).map(String::as_str).collect(); // 透传 --gate
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg(&script).args(&extra);                 // 继承 env + stdout/stderr
        match tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output()).await {
            Ok(Ok(out)) => match out.status.code() {
                Some(0) => Outcome::ok("audit provision check passed"),
                Some(1) => Outcome::error(format!("audit provision check failed: {}",
                                                  String::from_utf8_lossy(&out.stderr).trim())),
                Some(code @ 2..=6) => Outcome::warning(code, format!(
                    "audit provision check: exit {code}: {}",
                    String::from_utf8_lossy(&out.stderr).trim())),
                _ => Outcome::error("audit provision check: terminated by signal"),
            },
            Ok(Err(e)) => Outcome::error(format!("cannot launch audit-provision-check.sh: {e}")),
            Err(_) => Outcome::error("audit provision check timed out after 120s"),
        }
    }
);
```

- 注册：`a!(AuditProvisionCheck_);`（`Network_` 之后）；**直接 `tokio::process::Command` spawn**（§7 D6：`run_cmd` 拍平码）；tokio workspace `features=["full"]` 已含 process（§0 补充），**aero-cli Cargo.toml 一行不动**。
- 退出码映射经已验证机制穿透：warning(2..=6) → `execute_with_ctx` Err → `main.rs:43` `exit(code)`；1 → `Outcome::error`（exit 1）；0 → ok（exit 0）。
- `Completion_` cmds 串 :307 增 `audit-provision-check`（§7 D10 注：bench/dashboard 存量缺失不修）。
- `Gate_`（§7 D1）：match 增 `"audit"` 臂，**直 spawn** `bash scripts/audit-provision-check.sh --gate`（`b()` 无法传参），非零 → `Outcome::error`（门语义 = pass/fail，exit 1）；`gate list` ok 串 :113 增 `audit`。

### 2.3 `crates/aero-server/src/routes/health.rs`（tests 模块新增，生产代码零改动）

```rust
#[test]
fn readyz_never_flips_on_relay_unreachability() {
    // probe_commercial 的输入只有 runtime 存在性 + projection ready()（:67-70 注释）——
    // 无 sink 可达性探针；relay 缺席 = "disabled" ∈ 就绪集。sink 不可达 ⇒ probe ∈
    // {"ok","disabled"} ⇒ deps_ok 恒真 ⇒ readyz 永不 ok→fail 翻转。
    for commercial in ["ok", "disabled"] {
        let (status, state) = readiness_decision(false, matches!(commercial, "ok" | "disabled"));
        assert_eq!(status, StatusCode::OK);
        assert_eq!(state, "ready");
    }
    for commercial in ["not_ready", "fail", "timeout"] {
        let (status, state) = readiness_decision(false, matches!(commercial, "ok" | "disabled"));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(state, "not_ready");
    }
}
```

（`deps_ok` 语义 :186 的表格化断言 + 不变量注释；既有 `readiness_decision_*` 三单测不动。）

### 2.4 `scripts/test-integration.sh`（B5 段扩展）

- **`B5_CONTRACT_TEST_LIST` 具名清单**（替换 :265-269 A3 pin 占位，**整体 hoist 出 `SKIP_DB_CREATE` 分支与 0239 文件门**——顶层新段，自检与 drill 无 DB 无条件执行，G15）：37 槽 bash 数组；仓内可执行项实名（下表 §6），仓外契约项 `[PROPOSED-01]…[PROPOSED-NN]`（内容不臆造，proposal :13）；自检段断言（**在清单定义块之外执行**，防自指）：清单非空、恰 37 槽、`"audit-provision-check exit-code matrix"` **显式成员断言**（G14——防删条目补 [PROPOSED] 逃逸）、每非-[PROPOSED] 项有**文本级**可执行解析锚（清单块外 grep 到对应 `run_migrated_integration` 调用/段函数名；**运行通过语义弃用**——B5-1 槽在 0239 缺席时 SKIP 是段自身职责，当前阶段 run-pass 必红，G15）、[PROPOSED] 槽只出现在具名列表尾部。**step 4 三件套（清单替换 + 命名条目 + B5 CI 接线）单 commit 落**（G14）。
- **命名条目 "audit-provision-check exit-code matrix"**（§6 AC1/AC2/AC4 drills，无 DB 无 server boot）。
- **B5 段 CI 接线**：以**命令级 inline env**（`env AERO_AUDIT_* ... "$AERO_ENG" gate audit`，**禁止 `export`**——C5：export 会污染同 shell 后续任何 aero-server boot，触发 stray fail-loud）调用 `"$AERO_ENG" gate audit`，非零即中止（脚本 :21 `set -euo pipefail`）——"fails CI when any of the above holds" 的仓内钉死读法（implementation-gate.md:78 G6 上下文）。mock token 经 `env AERO_AUDIT_PROVISION_CHECK_TOKEN=...` 传入，不得 echo/写文件（§7 D12）。

## 3. Compatibility constraints

- **C1 aero-cli 零新依赖**：Cargo.toml 不动（tokio full 已含 process）；不链接 aero-auth/aero-audit-connector（sqlx 可达 = DB 依赖，违反 crate 头注释 "no DB dependencies"）。
- **C2 aero-server 生产代码零改动**：health.rs 仅 tests 模块加一测试；`probe_commercial`/`readiness_decision`/`health_ready` 一行不动（AGENTS.md §4.2 无新警告约束适用）。
- **C3 connector crate 零改动**：check 只读 workspace 成员关系与 env，不 import 任何 connector 符号。
- **C4 零迁移**：本 direction 不新增任何 `migrations/NNNN_*.sql`（配给本体在 IdP 仓；gate 状态 = sibling spec 的 0240 域）。迁移计数不变（当前尾号 0238）。
- **C5 env 命名冲突（关键）**：新 `AERO_AUDIT_PROVISION_CHECK_TOKEN` 落入 `AERO_AUDIT_` 前缀——`RelayConfig::from_env` 的 stray 扫描（config.rs:54-56）会把 server 进程 env 里任何 `AERO_AUDIT_*` 判为 boot Err。**已核实 fail-loud 方向**：stray 扫描只在 **endpoint 缺席分支**运行（config.rs:53-64）——token 变量 + 无 endpoint → boot `bail!`（main.rs:245-269 接线 → boot Err，消息点名 stray 计数"unset while N ... present"，不点名变量名）；endpoint 在时该变量对 `from_env` 不可见（无副作用）。**约束：该变量只允许出现在 aero-eng 检查进程的 env（CI/冒烟按命令级 inline env 传入，§2.4），不得 shell 级 export 进 aero-server 启动环境**。违反的两种表现均被 fail-closed 覆盖：server boot 对 {token, 无 endpoint} fail-loud（stray bail）；**预检对同一 env 直接报 1**（config 类，点名 stray 计数——被检出而非被掩盖，§7 D5 白名单删除决策；AC1-⑥e 回归钉）。若未来需要全局 export，改前缀为 `AERO_PROVISION_CHECK_TOKEN`（[PROPOSED]，§7 D5）。
- **C6 既有 aero-eng 命令面不变**：新命令名无冲突；`help`/`completion` 串更新；`gate all` 不含 audit（feature-off dev CI 无 AERO_AUDIT_* env → exit 3 是 fail-closed 正确答案，并入会无条件全红——spec §7 语义保留）。
- **C7 退出码契约分层**：distinct codes（0-6）是脚本 + `audit-provision-check` 命令层契约（AC1 断言对象）；`gate audit` 层只承诺非零（拍平为 1，AC2 断言对象）。两层分离，互不渗透。
- **C8 与 sibling spec（aero-auth B5-4）并存**：两 CLI 不冲突（aero-server bin 15 命令面不受影响）；`audit-provision-check` 落家冲突按本 direction acceptance 原词 "`aero-eng audit-provision-check`" 为准（§7 D0），sibling 的 R4 段在集成时消费本 seam 或删去（campaign 级决策，非本 direction 执行）。

## 4. Failure modes（含处置）

| # | 失效面 | 表现 | 处置 |
|---|---|---|---|
| FM1 | 脚本缺席/不可执行 | `Command::new("bash")` 启动 Err | `Outcome::error`（exit 1）消息点名路径；CI B5 段立即红 |
| FM2 | 脚本卡死 | 120s 超时 | `Outcome::error` "timed out after 120s"（`run_cmd` 同款语义） |
| FM3 | TCP dial 挂起 | 每 URL 2s socket 超时，双 URL ≤4s | 恒有界；总脚本执行上界 ≈ 10s + 校验开销 |
| FM4 | token 畸形（坏 base64 / 缺 claim / sub≠client_id / 过期 / scope 缺 / iss·aud·sub 不匹配 expected / jti 空 / iat 未来） | code 4 + 原因行 | 消息只含静态文本 + 变量名 + 计数 + 期望 scope 字面量（AC1-③ grep 面）；token/URL 值永不回显；无 traceback（§2.1 泄漏 pin / §7 D12） |
| FM5 | 错误 cwd / root 不可读 | root 自解析失败 | code 1；脚本不依赖调用方 cwd（与 `Gate_` 同语义） |
| FM6 | **假绿**：preflight 0 而 boot Err | 未镜像的 `from_env` Err 类（bool/duration×5/不变量×2/整数×2/identity×6/secret/URL 解析面）——如 client_secret 过短、timeout 越界、lease 不变量破坏、allow-insecure 非法值、{token, 无 endpoint}（白名单时代） | D2：code-1 全量镜像 `from_env` 全部 Err 类（§2.1 P4）；残余 = URL 解析器边缘差异（G1-G5 已显式规则镜像）+ R1-R5 假红面（文档化，§7 D13）；白名单删除（D5）；AC1 增 drill ⑥b/⑥c/⑥e（http 非 loopback、值类 Err 面、token+无 endpoint → 1） |
| FM7 | `AERO_AUDIT_PROVISION_CHECK_TOKEN` 泄漏进 server env | server boot Err（stray）**且预检报 1（检出，非掩盖）** | C5 约束 + fail-loud 消息兜底 + 预检检出（AC1-⑥e）；CI 按命令级 env 传入 |
| FM8 | 退出码被拍平（误用 `run_cmd`） | 全变 1，AC1 distinct 断言红 | D6 直 spawn；AC1 穿透 drill（对编译后 aero-eng 复跑 ②③④）常驻回归 |
| FM9 | python3 缺席 | P2 拦截 | code 1（usage 类）——bash 壳可独立报错，不依赖 python3 先行 |
| FM10 | 假阴：TLS/HTTP 层问题（5xx、TLS 握手失败、403） | dial 通过 → 0 或 4 不覆盖 | 已知边界（spec §7）：传输层预检 ≠ "relay works"（202 receipt/settle = sibling 心跳域）；文档化，不扩 scope |
| FM11 | 重复执行 | 纯读、零副作用 | 幂等 by construction；无状态机、无 receipt、无锁 |
| FM12 | **假绿**：preflight 0 而运行时 token 被拒（P6 接受集 ⊃ 运行时接受集） | union scope 接受仅 `scopes[]` 的 token、缺 iss/aud/sub expected 比对、jti 空、iat 未来 | D11：P6 接受判定 = 运行时门 `validate_token_claims` 镜像，aero-auth 结构为额外拒绝条件（层 A 已含 iat>now / jti 非空 / identity-component——G4 钉）；AC1-③ 变体"仅 scopes[] 无 scope claim → 4"与 iat=now+3600/jti="" 常驻回归钉 |

## 5. Migration steps & sequencing（零 SQL 迁移；代码落地序）

> 本 direction **无数据库迁移**。"migration" 指功能落地顺序与回滚面：每步独立可验证，前序失败不阻塞后续（除 4 依赖 1-3 的产物）。

1. **脚本**（可独立交付）：`scripts/audit-provision-check.sh`（P1-P8 + 退出码矩阵）。验证：`bash scripts/audit-provision-check.sh; echo $?`（本仓当前态 = 有 connector、无 AERO_AUDIT_* → **exit 3** 是正确基线）；`--gate` 无清单 → 先 exit 6（清单未落）→ 第 4 步后转 exit 0。
2. **aero-eng 命令**：`AuditProvisionCheck_` + `a!(…)` + `Gate_` audit 臂 + `gate list`/`Completion_` 串。验证：`cargo build -p aero-cli && ./target/debug/aero-eng audit-provision-check; echo $?`（同码穿透）；`aero-eng help` / `aero-eng completion bash` 冒烟。
3. **readyz 回归**：health.rs tests 新单测。验证：`cargo test -p aero-server routes::health`（无 DB，全绿含既有 3 例）。
4. **钉入 + drills（单 commit，G14）**：`B5_CONTRACT_TEST_LIST` 37 槽替换占位（:265-269）+ "audit-provision-check exit-code matrix" 条目（清单/自检/drill **hoist 出 `SKIP_DB_CREATE` 分支与 0239 文件门**——顶层新段，G15）+ B5 段 `gate audit` CI 调用。验证：`bash scripts/test-integration.sh`（A3 段沿用 0239 文件门现状——0239 未落库时 B5-1 段跳过，本条目无 DB 不受影响）。
5. **CI/门上下文接线**：implementation-gate.md G6（:78）语境下以配给 env 调 `aero-eng gate audit`；sibling spec 集成面确认（§7 D0）。
6. **提交前门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规；新脚本 ~230 行 < 800 WARN 阈值）· `scripts/test-integration.sh` · no-touch 守卫：`git diff --stat` 不含 migrations/、aero-auth、aero-audit-connector/src/、snaplink_commercial/ 生产代码。

**回滚面**：删除脚本 + 摘 `a!()`/`Gate_` 臂/串改动 = 完全回退（aero-eng 9 命令原状）；health.rs 测试删除 = 回退；`B5_CONTRACT_TEST_LIST` 移除 = 回到 A3 pin 占位（清单是新增段，不触碰既有 A3 drill 行）。

## 6. Testable acceptance mapping

### AC1 — distinct exit codes（crate missing / boot gate disabled / scope absent / relay unreachable）

**测试 = test-integration.sh 命名条目 "audit-provision-check exit-code matrix"**（无 DB、无 server boot；`set +e` 捕获码后 `set -e` 恢复）。Harness 约定（G1/G3/G9-G12）：
- **`GREEN_ENV`（命名 boot-faithful 夹具，F3c）**：endpoint/events 双 URL（canonical `http://127.0.0.1:<port>`——**仅 canonical 127.0.0.1/localhost**，R2 禁 127.1/0x7f 等 Rust 归一化变体）+ `AERO_AUDIT_ALLOW_INSECURE_LOOPBACK=true` + 7 个必填身份/密钥变量（`RESOURCE`/`CLIENT_ID`/`CLIENT_SECRET`≥32B/`EXPECTED_ISS`/`EXPECTED_AUD`/`EXPECTED_SUB`/`SOURCE_SYSTEM`）+ 合法数值（timeout 10/lease 30/shutdown 5/task 30/poll 5/batch 100/concurrency 4）+ 含 `scope` claim 的 mock token。③/④/⑤/⑥b/⑥c/exit-6 树均以 GREEN_ENV 为基底（按场景变异）；②/⑥/⑥d/⑥e 从零 env 构造。
- **每场景独立构造 env**（`env -i` 或 unset 循环），禁共享 export（F1b——`AERO_AUDIT_ALLOW_INSECURE_LOOPBACK=true` 若全局 export 会把 ② 从 3 翻成 1）。
- **负向断言（G1）**：每个失败场景合并 stdout+stderr 后断言 `! grep -Fq "$MOCK_TOKEN"`；`MOCK_TOKEN` 由 python3 现场 mint，每次内嵌唯一随机 nonce（jti=UUID）防 `grep -F` 巧合命中。
- **确定性边界（G3）**：只造 T1≥T0 恒成立用例（`exp==now→4`、`nbf==now→0`、`exp=now+3600→0`、`nbf=now+3600→4`、`iat=now+3600→4`）；**禁造 `exp=now+1`/`nbf=now+1`**（±1s 竞态，§7 D4）。
- **活 listener harness（G9/G10/G12）**：`python3 -u -m http.server 0 --bind 127.0.0.1`（**`-u` 防块缓冲吞端口行**；**port 0** 防固定端口占用——本仓环境 8123 实证 EADDRINUSE）；从输出行解析 `port (\d+)`；wait-for-port 有界轮询（≤10s，python connect 探测）后才跑主脚本；`trap 'kill $SRV_PID' EXIT` 防断言失败孤儿进程。

| # | 场景构造 | 期望码 | 消息 grep | 负向断言 | 断言点 |
|---|---|---|---|---|---|
| ①a | fake tree：`crates/aero-audit-connector/` 缺席；fake Cargo.toml **同时含 `[workspace]` members 与 `[workspace.dependencies]` 两段**（真仓 :29/:77 形态）但无 connector 条目；脚本拷入 tree 自解析 root | 2 | `connector` | —（无 token） | 目录臂（G7——设计早期单臂在目录检查即短路，deps 臂永不执行） |
| ①b | 目录在；members 段含 `"crates/aero-audit-connector",`；deps 段缺 `aero-audit-connector =` key | 2 | `workspace.dependencies` | — | **deps 臂**（F2 段作用域 awk——整文件 grep 会假过） |
| ①c | 目录在；members 段缺条目；deps 段含 `aero-audit-connector = { path = "crates/aero-audit-connector" }` | 2 | `members` | — | **members 臂** |
| ①d | 双段均含（= 真仓形态） | 0（该步） | — | — | 绿树基底（复用为 exit-6 树） |
| P2 | PATH 剔除 python3 所在目录（`PDIR=$(dirname "$(command -v python3)")` 从 PATH 逐段剔除，勿假设 /usr/bin）跑真仓脚本 | 1 | `python3` | — | P2 先于 P3-P5 触发（G2） |
| ② | 零个 AERO_AUDIT_*（env -i） | 3 | `boot gate` | — | P5 仅当零 stray（D5） |
| ⑥ | 仅 `AERO_AUDIT_CLIENT_ID`（无 endpoint） | 1 | `config` | — | stray 臂 |
| ⑥d | 仅 `AERO_AUDIT_ALLOW_INSECURE_LOOPBACK`=true（无 endpoint） | 1 | `config` | — | 真实 connector 变量计入 stray（F1b） |
| ⑥e | 仅 `AERO_AUDIT_PROVISION_CHECK_TOKEN`（无 endpoint） | 1 | `config` | token 字面量不出现 | **G5/D5 决策钉：白名单删除** |
| ⑥b | GREEN_ENV 变异：endpoint=http://example.com（非 loopback） | 1 | `HTTPS` | token 字面量不出现 | URL scheme 臂 |
| ⑥c | GREEN_ENV 变异其一：`CLIENT_SECRET`="short"（<32B）/ 缺 `CLIENT_ID` / `REQUEST_TIMEOUT_SECS`=5000 / `CONCURRENCY`=99 / `ALLOW_INSECURE_LOOPBACK`="yes" / `DELIVERY_LEASE_SECS`=10（破 lease 不变量） | 1 | `config` | token 字面量不出现 | from_env 值类全镜像（G6） |
| ③ | GREEN_ENV + token `scope`="billing:entitlement:read"（mock at+jwt：iss/aud/sub 对齐 GREEN_ENV、sub==client_id，python3 生成） | 4 | `audit:event:write` | token 字面量不出现 | scope 拒 |
| ③ 变体（各 → 4，grep `audit:event:write`，负向断言同） | `typ`="JWT"；`alg`="HS256"；sub≠client_id；sub≠`EXPECTED_SUB`；iss≠`EXPECTED_ISS`；aud≠`EXPECTED_AUD`；缺 iss；`jti`=""；jti 含控制字符；sub 首尾空白；exp="abc"（非整数）；**exp==now**（边界，恒 4）；exp=now-30（恒 4——no-leeway 假红面实证）；**nbf=now+3600**（恒 4）；**iat=now+3600**（恒 4，G4 钉）；**仅 `scopes[]` 无 `scope` claim**（D11 钉） | 4 | `audit:event:write` | token 字面量不出现 | 层 A/R 各臂 |
| ③b（各 → 0 该步） | `scope` string="audit:event:write"；`typ`="AT+JWT"（大小写变体，oidc.rs:420 镜像）；`scope`=" audit:event:write billing:x "（多空白）；`scope` 数组；`scopes`+`scope` 双 claim；**nbf==now**（边界，恒 0）；**exp=now+3600**（恒 0） | 0 | — | — | 步骤级通过 |
| ④ | GREEN_ENV 双 URL 指向 bind-without-listen 端口（bind 后**不 listen 不 close**——dial 必得 connection refused，G11） | 5 | `unreachable` | token 字面量不出现 | P7 臂 |
| ⑤ | GREEN_ENV 双 URL 指向活 listener（`python3 -u -m http.server 0 --bind 127.0.0.1`，port 0 + wait-for-port + trap） | 0 | — | — | 全绿 |
| 穿透 | 对 `cargo build -p aero-cli` 产物复跑 ②③④（inline env 传入） | 同码 | 同 grep | token 字面量不出现（`eprintln!` 回显面，G1） | `./target/debug/aero-eng audit-provision-check; assert $? == N`（E8 机制回归） |

### AC2 — `aero-eng gate audit` via Gate_ bash-script pattern；CI fails on any condition

同条目追加：
- 无 AERO_AUDIT_* env → `aero-eng gate audit` **exit 1** 且 stderr 含失败原因（拍平语义）；配给 env 全绿 → **exit 0**；坏 token（GREEN_ENV + 缺 scope）→ exit 1 + stderr 含 `audit:event:write` **且 token 字面量不出现**（eprintln! 回显面，G1）。
- `aero-eng gate list` 输出含 `audit`（断言）。
- **CI 接线**：test-integration.sh B5 段（implementation-gate.md:78 G6 语境）以配给 env 调 `aero-eng gate audit`，非零经 :21 `set -euo pipefail` 中止 = "fails CI when any of the above holds" 的仓内钉死读法。

### AC3 — readyz never flips on relay/sink unreachability

**测试 = `crates/aero-server/src/routes/health.rs` tests 模块 `readyz_never_flips_on_relay_unreachability`**（§2.3，参数化表格 + 不变量注释：probe_commercial 无 sink 探针 ⇒ sink 不可达 ⇒ probe ∈ {"ok","disabled"} ⇒ readyz 恒 200）。零生产代码改动；既有 `readiness_decision_*` 三单测保持绿。运行：`cargo test -p aero-server routes::health`（无 DB）。
可选 drill（不进 AC1 必过面）：真实 boot server + 不可达 relay env → `GET /health/ready` 恒 200（需完整 boot，标 optional）。

### AC4 — 37/37 contract test list pinned & executable in-repo

- **`B5_CONTRACT_TEST_LIST` 具名清单**（替换 test-integration.sh:265-269 占位；**清单 + 自检 + drill 整体 hoist 出 `SKIP_DB_CREATE` 分支与 0239 文件门**——顶层新段，无 DB 无条件执行，G15）：37 槽；仓内可执行项实名（锚点 = 既有命名测试/钻）：

| 槽 | 仓内项（实名，可执行） | 解析段（文本级锚点） |
|---|---|---|
| 1-30 | B5-1 governance parity db_tests（"30 个忽略测试 CI 全绿（37/37）"——implementation-gate.md:63） | 既有 B5-1 段（:226-242，0239 文件门；运行期 SKIP 是段自身职责） |
| 31 | B5-2 403→dead（`forbidden_dead_on_first_attempt`，state_machine.rs:216-238） | 既有 A3 relay drill 段 |
| 32 | B5-2 claim/receipt 契约（claim_validation.rs 10 用例） | 同上 |
| 33 | B5-3 moderation 优先级 drill | B5-3 段（落地后接线） |
| 34 | T-11 provision-check 退出码 drill | 本条目（AC1，`run_audit_provision_check_matrix` 段函数） |
| 35 | readyz 不翻转单测（AC3） | `cargo test -p aero-server routes::health` |
| 36-37 | 契约文本未落 → `[PROPOSED-01]`…（仓外，不臆造，proposal :13） | 显式标注 |

- **自检断言（G14/G15）**（在**清单定义块之外**执行，防自指）：清单非空、**恰 37 槽**、**显式成员断言 `"audit-provision-check exit-code matrix" ∈ B5_CONTRACT_TEST_LIST`**（「恰 37 + [PROPOSED] 尾置」查不出「删条目 + 补 [PROPOSED]」逃逸）、每非-[PROPOSED] 项名在清单块外命中**文本级**可执行锚点（`run_migrated_integration` 调用 / 段函数名——**运行通过语义弃用**：B5-1 槽在 0239 缺席时 SKIP 是段自身职责，当前阶段 run-pass 必红）、[PROPOSED] 槽只出现在具名列表尾部。**G13 注记（不改）**：「37」自指（implementation-gate.md:63 的 37/37 在 B5-1 行）——35+2 构成作为本地契约成立，[PROPOSED] 尾是诚实 seam，维持现状。契约文本落仓后仅需把 [PROPOSED] 槽替换为真实项（seam 已就位）。
- **exit-6 守卫四格矩阵（G8，P3-P7 全绿 = GREEN_ENV + 活 listener + fake Cargo.toml 双段 + 目录，①d 树）**：P7 dial 的是**活 socket**，exit-6 树必须起 http.server（复用 ⑤ harness）：

| tree | 模式 | 期望 |
|---|---|---|
| markerless（fake scripts/test-integration.sh 无 `B5_CONTRACT_TEST_LIST`） | `--gate` | 6 |
| markerless | （无参） | 0 |
| marker 在场（`B5_CONTRACT_TEST_LIST=(placeholder)`） | `--gate` | 0 |
| marker 在场 | （无参） | 0 |
| markerless + 坏 token（缺 scope） | `--gate` | 4（P6 先于 P8 短路序钉，G16 补强） |

清单落位后本仓直跑 `--gate` → 0（G16：当前态字面量全仓零命中 → step 1 落地后真仓直出 6，可作 checkpoint）。

## 7. Deviations from requirements spec & decisions

- **D0 落家冲突（spec §7 原样保留）**：sibling spec（aero-auth B5-4）把 seam 放 `aero-server/src/bin/aero-cli.rs`；本 direction acceptance 原词 "`aero-eng audit-provision-check`" 为准。集成时 sibling R4 段消费本 seam 的脚本/退出码或删去——campaign 级决策，非本 direction 执行。
- **D1 `gate audit` 直 spawn 带 `--gate`（spec R3 字面修正）**：`b()` helper 签名 `(path, timeout)` 无法传参，spec R3 字面 `b(f("audit-provision-check.sh"), 120)` 跑不出 code-6 钉入守卫（而 AC4 要求 `gate audit` 内嵌 `--gate` 语义）。设计：Gate_ audit 臂直 spawn `bash <script> --gate`，非零 → `Outcome::error`（exit 1）——门语义（pass/fail）与 AC2 不变，钉入守卫生效。意图保留，机制修正。
- **D2 code-1 扩展为完整 `from_env` Err 面镜像（spec R1 code-1 行补全；硬化复核补值类；F3c 合流）**：spec 只列 stray 情形；设计补全 `RelayConfig::from_env`（config.rs:52-160）+ `service_url` 的**全部 Err 类**：① stray 前缀扫描（:54-56，**白名单无**——含 token、allow-insecure 均计入，§7 D5）；② `optional_env` trim 语义（trim 后为空 = 缺席，endpoint/required 判定同语义）；③ `bool_or_default`（true/1/false/0 大小写不敏感，否则 bail，:117-129）；④ `required_env` events-url（:67）；⑤ `service_url` 全规则（parse/凭据/query/fragment/2048 字节/https 或 http+loopback+opt-in，:135-151；loopback = host 大小写不敏感 "localhost" 或 IP 解析 is_loopback，**不做 DNS**；G1-G5 显式规则 + R1-R5 假红文档 → §7 D13）；⑥ `duration_secs`×5 解析+范围（timeout 1..120、lease 5..300、shutdown 1..60、task 1..600、poll 1..300）；⑦ lease/drain 不变量（:155-171：`delivery_lease > 2×request_timeout+2`、`task_drain > 2×request_timeout+shutdown_drain+2`）；⑧ `integer_env`×2（batch 1..500、concurrency 1..32）；⑨ `identity_env`×6（resource≤512、client_id≤512、expected_iss/aud/sub/source_system≤256：必填+非空+trim+无控制字符）；⑩ `secret_env` client_secret（≥32 字节、≤16384 字节、trim、无控制字符）。理由：**preflight 永不比 boot 更绿**（FM6）——任一 Err 类未镜像即假绿（如 client_secret 过短：boot Err 而 preflight 0）。代价：~40 行 python（P4 与 P6 同一解释器）；AC1 增 ⑥b/⑥c/⑥e drills。
- **D3 脚本不用 `set -euo pipefail`（spec 未明说，repo 常态例外）**：distinct 退出码要求短路显式 `exit N`；`set -e` 会把一切失败拍平为 1。脚本头注释说明偏离理由；AC1 矩阵断言即回归。
- **D4 code-4 时间窗 strict-no-leeway（spec R1 结构校验补全；G3/G4 合流决策）**：connector 侧 jsonwebtoken leeway=60（oidc.rs:293；validation.rs:272-280）——exp 拒 iff `exp < now-60`、nbf 拒 iff `nbf > now+60`，connector **接受 exp∈[now−60,now]、nbf∈[now,now+60]**；aero-auth 另拒 `iat > now+60`（oidc.rs:453）。**决策：脚本不镜像 leeway，统一 strict-no-leeway——`exp ≤ now → 4`、`nbf > now → 4`、`iat > now → 4`**（早期措辞「精确镜像 leeway 60」作废——脚本是 **no-leeway 近似，刻意严于 boot**，非镜像）。理由：① 确定性 drills（G3：`exp==now → 4`、`nbf==now → 0` 在 T1≥T0 下恒成立；leeway 窗口用例如 `nbf=now+30` 时间依赖，检查点漂移出窗口即翻转）；② 假红方向安全——脚本比 connector 更红 ≤60s，fail-closed 方向（AC1-③ `exp=now-30 → 4` 实证该面）。**禁造 `exp=now+1`/`nbf=now+1`**（±1s 内检查完成即竞态）。aero-auth 的 `iat>now+60` 在脚本收紧为 `iat>now`（与 exp/nbf 自洽，假红 ≤60s 同方向）。exp/nbf/iat 必须为 JSON 整数（u64 语义，bool/float/string → 4，python 需排除 `bool`——`isinstance(True, int)` 为真）。python3 epoch 比较，~8 行。
- **D5 `AERO_AUDIT_PROVISION_CHECK_TOKEN` 前缀冲突 + 白名单整体删除（spec 未覆盖，§3 C5；G5/F1a/F1b 合流决策）**：变量名落入 `AERO_AUDIT_` 前缀，server 进程 stray 扫描会把它当 stray。**决策：白名单整体删除**——connector stray 扫描**无任何白名单**（config.rs:54-56），{token, 无 endpoint} 在 boot 必 Err，预检必须报 1（否则违反 §1 核心不变量「凡 `from_env` 会 Err 的 env 面必须报 1」）。早期白名单（排除 token 变量）使该场景报 3（"not provisioned"）——**假绿**（boot Err 而 preflight 3），且把 C5 违规（token 被全局 export）从「被检出（1）」掩盖为「被掩盖（3）」。F1a 的 exact-match 关切随删除一并满足：`AERO_AUDIT_PROVISION_CHECK_TOKEN_EXTRA` 等前缀名自动计入 stray → exit 1（与 `vars_os().starts_with` 一致）。删除后：P4 stray 臂 = **任一** `AERO_AUDIT_*`（含 token、含 `AERO_AUDIT_ALLOW_INSECURE_LOOPBACK`——真实 connector 变量，config.rs:65，F1b）而无 endpoint → `exit 1`（AC1-⑥d/⑥e 回归钉）；P5 exit-3 仅当**零个** `AERO_AUDIT_*` 变量。spec 变量名保留（acceptance 引用之），约束 = CLI 进程级 inline env（§2.4；drill harness 每场景独立构造 env，禁共享 export）；全局 export 场景改前缀 `AERO_PROVISION_CHECK_TOKEN`（[PROPOSED]）。脚本内读取 allow-insecure 做 URL 判定与计入 stray 不冲突（endpoint 在时不进 stray 分支）。
- **D6 直 spawn 保码（spec R2 已定，设计确认机制面）**：`run_cmd` 拍平（run.rs:15-41）→ `tokio::process::Command` + `Outcome::warning(code)` 穿透（outcome.rs:47 + registry.rs Err 分支 + main.rs:43，三层已验）。tokio full features 含 process → Cargo.toml 零改动。
- **D7 期望 scope 镜像 `AERO_AUDIT_EXPECTED_SCOPE`（spec R1 硬编码 audit:event:write 的细化）**：connector 默认即 `audit:event:write`（config.rs:89-91）；脚本读该 env 缺省同默认值——保证 check 与 connector 实际索求一致；AC1-③ 消息 grep 面（"audit:event:write"）在默认配置下不变。
- **D8 exit-6 drill 用 fake tree 而非测试专用 override（G16 验证通过；G7/G8 扩展为四臂 + 四格）**：脚本 root 自解析（`BASH_SOURCE` 定位，§2.1 P1）⇒ 脚本 + 最小 fake tree 拷入 mktemp 目录即可测 2/6 两码；零新增 env seam。**P3 四臂矩阵（G7）**：①a 目录缺席 / ①b members 在 deps 缺 / ①c deps 在 members 缺 / ①d 双段均在（绿树基底）——fake Cargo.toml 必须同时含 `[workspace]` members 与 `[workspace.dependencies]` 两段（真仓 :29/:77），脚本 grep 段作用域（F2 awk）；设计早期「crates/ 无 connector + Cargo.toml 无成员项」单臂在目录检查即短路，deps grep 臂永不执行。**exit-6 四格模式隔离（G8）**：markerless × {--gate=6, 无参=0}；marker 在场 × {--gate=0, 无参=0}——与真仓清单状态解耦；P7 dial 的是**活 socket**，exit-6 树必须起 http.server（复用 ⑤ harness）；可选补强：markerless + 坏 token → 4（钉 P6 先于 P8 短路序）。当前态真仓 `--gate` 直出 6（字面量全仓零命中，G16 实证）。
- **D9 token 签名验证 = [PROPOSED] seam（spec §7 原样保留）**：结构校验足够检出 scope 缺席（acceptance 字面）；完整 RS256/JWKS 验证需 IdP 仓外密钥，未来可换 aero-auth `validate_client_credentials_token` 的 KeyProvider 面（aero-cli 保持零新依赖）。
- **D10 `Completion_` 串只增 audit-provision-check**：串现存缺 bench/dashboard（存量过期，main.rs:307）——非本 direction 修，不扩大 diff。
- **D11 P6 双层镜像：运行时门 + aero-auth 结构超集（硬化复核发现；F4 合流）**：connector **不依赖 aero-auth**（src/ + Cargo.toml 全仓 grep 零命中）——运行时 token 门是 `client.rs::validate_token_claims`（:197-259）：iss==expected_iss、aud string/数组含 expected_aud、**`scope` claim（单词或数组）含 expected_scope、`scopes[]` 数组不参与**、sub==expected_sub、形状非空/≤16KB/无控制字符，decode = 恰 3 段 + base64url padding 容错（:415-431）。aero-auth `validate_client_credentials_token`（:406-471）只是库级语义（更严：typ/alg/exp/nbf/iat/jti/sub==client_id，但 iss/aud 对的是 OIDC cfg 而非 connector env）。**接受集必须 ⊆ 运行时接受集**（否则预检 0 而运行时时拒 = FM12 假绿）：P6 以运行时门为接受判定（层 R），aero-auth 结构检查作额外拒绝条件（层 A，假红安全、文档化）。关键差异：**union 并集 `granted_scopes()`（:387-394）接受仅 `scopes[]` 的 token = 假绿**——原 AC1-③b 绿色样本（仅 scopes 数组）正是运行时拒收的 token；绿色样本必须含 `scope` string claim。另补：iss/aud/sub 与 `AERO_AUDIT_EXPECTED_{ISS,AUD,SUB}` 比对（P4 保证 endpoint 在时必填）、`valid_identity_component`（:445-447）、jti 非空/≤1024 字节/无控制（:458）、alg ∈ {RS256, EdDSA}（:425-431）、typ 必填且大小写不敏感（:419-423）。
- **D12 token 泄漏 pin（硬化复核补充）**：token 只经 env 进入（脚本与 python 均 `os.environ` 读取，heredoc 用**单引号定界符**，**禁止 bash 插值 token 进 python 正文**——python traceback 会打印失败帧的源码行，插值字面量 = 泄漏）；argv 契约 = 仅接受 `--gate`，其余参数 → `exit 1` "usage" 且不回显参数值（防误把 token 当 argv 传入后静默吞掉）；python 全程 `try/except` → 静态消息 + `sys.exit(1)`（`internal` 类），**无 traceback**（异常文本可能携带 env 派生值，如 int() 的 ValueError 含原串）；禁 `set -x`/`set -v`/`PS4` 追踪（展开命令行会回显 env 赋值）；零临时文件、零 `export`/re-export；Rust 侧错误文本 = 脚本 stderr（受上述 pin 约束）；AC1 drills 的 mock token 一律 inline env 传入、不 echo、不写文件；**每个失败 drill 与编译产物穿透复跑均断言 token 字面量不出现在合并 stdout+stderr**（AC1 负向断言列，G1——aero-eng `eprintln!` 回显 stderr 使二进制路径同样被覆盖）。
- **D13 P4/P3 镜像机制 pin 与 R1-R5 假红文档（shell_script 复核 F1c/F2/F3a/F3d 合流）**：① P3 段作用域 awk FSM（`^\[` 切 section、首个非空白字符 `#` 的行跳过——注释安全；`[workspace]` 匹配成员串 `"crates/aero-audit-connector"`（带闭引号防 `-x` 别名）、`[workspace.dependencies]` 匹配 key `^aero-audit-connector[[:space:]]*=`（path 串出现在错误 key 下必须 fail））——整文件 `grep -q` 假过注释条目/跨段命中/错 key 命中（F2）；两臂独立 detail 均 exit 2。② stray 扫描 = python3 `os.environ` 键集（C environ 镜像 `vars_os()`，等同 `env` 扫描）；**禁 `${!AERO_AUDIT_*}`**（F1c：bash 只持有合法标识符名，`AERO_AUDIT_FOO-BAR` 这类 `vars_os()` 会计数的名字 `${!…}` 静默漏计 → 假绿）。③ URL 镜像规则（url 2.5.4 实证，§0 复核 4）：G1 raw 串含 `?`/`#` 即拒（Rust `query()/fragment()` 对空串也是 `Some("")`）；G2 netloc `:port` digits-only + 1..=65535（99999/abc → Rust InvalidPort；空端口 `host:` Rust 丢弃 → 通过；**端口 0 Rust 接受 `Some(0)`——脚本拒 0 = 假红面**）；G3 hostname 含 `:`（IPv6）**永不 loopback**（`host_str()` 带括号 `[::1]` → `IpAddr` 解析失败）；G4 netloc `[` 开头 → hostname 必须合法 IPv6（`[127.0.0.1]` → Rust "invalid IPv6 address"）；G5 host 段空白/控制字符拒（path 段接受——R3）。④ **R1-R5 假红面（redder-than-boot，文档化不收紧）**：R1 userinfo 拒 iff username∉{None,''} 或 password∉{None,''}（`user:@host` Rust 丢空密码但保留 username → 仍拒；`@host` 过）；R2 IPv4 简写（127.1/0x7f.0.0.1/2130706433/127.0.0.1.）Rust 归一化为 127.0.0.1 → loopback，python 无法复刻 → **drill 只用 canonical 127.0.0.1/localhost**；R3 host 内 TAB/CR/LF Rust 剥除（`exa\tmple.com` → `example.com`）——脚本拒 = 假红；R4 `http:///path` Rust host="path"——脚本 hostname-None 拒 = 假红；R5 2048 字节限在**归一化后** URL（可涨可缩）——脚本保守镜像 raw 字节 ≥2048 拒（≥1 字节增长余量），raw 恰 2048 而归一化收缩 = 假红安全。⑤ trim 语义（F3d）：P4 所有 env 读取先镜像 `optional_env`（trim + drop-empty）——endpoint/events/required 判定、URL 2048 输入、bool 解析、token 读取同一语义。
- **D14 AC1 负向断言与确定性边界（test_strategy G1-G3 合流）**：① 每个失败 drill（①a-c、②、③ 全变体、④、⑥、⑥b-e、P2）与编译产物穿透复跑，合并 stdout+stderr 断言 `! grep -Fq "$MOCK_TOKEN"`——aero-eng 经 `eprintln!` 回显脚本 stderr（Outcome 消息），同一断言覆盖二进制泄漏面；mock token 每次 mint 内嵌唯一随机 nonce（jti=UUID）防巧合命中。② P2 PATH drill：PATH 剔除 python3 所在目录（`dirname "$(command -v python3)"` 逐段剔除，勿假设 /usr/bin）→ exit 1 + grep `python3`。③ 边界 drill 只造确定性用例：`exp==now→4`、`nbf==now→0`、`exp=now+3600→0`、`nbf=now+3600→4`、`iat=now+3600→4`（T1≥T0 恒成立）；**禁造 `exp=now+1`/`nbf=now+1`**（±1s 竞态）。
- **D15 B5_CONTRACT_TEST_LIST 成员断言 + 文本级解析 + 放置 hoist（G14/G15 合流）**：① 自检段增显式成员断言 `"audit-provision-check exit-code matrix" ∈ B5_CONTRACT_TEST_LIST`——「恰 37 + [PROPOSED] 尾置」查不出「删条目 + 补 [PROPOSED]」逃逸；step 4 三件套（清单替换 + 命名条目 + B5 CI 接线）**单 commit** 落。② 「可执行解析段」语义钉死为**文本级**：清单定义块之外，每非-[PROPOSED] 项名须命中可执行锚点（`run_migrated_integration` 调用 / 命名段函数）；运行通过语义弃用——B5-1 槽在 0239 缺席时 SKIP 是段自身职责，当前阶段 run-pass 必红。③ 放置：清单 + 自检 + 命名条目 drill **hoist 出 `SKIP_DB_CREATE` 分支（:197-276）与 0239 文件门**——顶层新段无 DB 无条件执行；否则 SKIP_DB_CREATE 下自检与 drill 都不跑。

## 8. Out of scope（勿在本 direction 建造）

- 配给本体（grant 动作 / IdP scope registry）→ 仓外 IdP 仓（proposal :11）；本 seam 只验证 + 非零拒绝，不签发。
- aero-auth `assert_audit_scope_provisioned` 拒绝点 / durable 心跳 / 0240 迁移 → sibling spec（aero-auth B5-4）；本 direction 不建 gate 状态，只消费 env/config/文件/网络。
- 0239 治理 outbox DDL / priority 排序 / moderation 优先 → B5-1/B5-3；403→dead 状态机 → B5-2 已实现，只当检查项。
- 完整 JWT 签名验证 / TLS·HTTP 层可达性 / 202 receipt·settle 判定 → [PROPOSED] / sibling 心跳域。
- v1 `snaplink_commercial/` 运行时、0236 触发器 → 原地不动（只把 v1 boot 门 env 当检查项）。
- migrations/、aero-auth、aero-audit-connector/src/、snaplink_commercial/ 生产代码 → no-touch 守卫。
