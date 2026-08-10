# Design — aero-cli B5-3：on-demand moderation-priority verification surface（`audit-provision-check --priority`）

- **Requirements**: `docs/requirements/2026-08-08-aero-cli-b5-3-moderation-priority-drill.req.md`（R1–R5 / AC1–AC6）
- **Landing owner**: `crates/aero-cli`（bin `aero-eng`）+ `crates/aero-eng` lib + `scripts/test-integration.sh`
- **Verification date**: 2026-08-08（全部锚点源码核对；行号会漂移，符号为准）

## 0. Evidence disposition（untrusted → verified）

| 证据主张 | 核对结果 | 锚点 |
|---|---|---|
| 0239 已落地（`priority SMALLINT NOT NULL DEFAULT 10` + `class` + `delivery_mode` + 值 CHECK；241 迁移） | ✅ 属实 | `migrations/0239_audit_governance_outbox.sql`（priority CHECK `> 0`；trigger 对 `message.moderated` 打 class='admin'/priority=100/action='admin.content.flag'）；`ls migrations/*.sql | wc -l` = 241 |
| `PgOutboxRepo::claim_due` 有 priority 排序 | ✅ 属实 | `crates/aero-audit-connector/src/pg.rs`：`ORDER BY candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id … FOR UPDATE SKIP LOCKED LIMIT`（:111） |
| 0240 priority-DESC 索引 + 0241 reconcile 落地 | ✅ 属实 | `migrations/0240_audit_governance_due_prio_idx.sql`（`(priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)`）、`0241_governance_reconcile.sql` |
| harness 腿 ACTIVE、drill bin 存在（301 行、exit-2 能力门、3 条 PASS 断言）、`b5-pin.sh:41` 钉位 | ✅ 属实 | `scripts/test-integration.sh:446-461`（0239 文件门控 → throwaway 库 → migrate → 直跑 drill → `b5_check "moderation-priority-drill" "PASS"`）；`crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（to_regclass :87-100 + information_schema 列探测 :105-130 缺 → `exit(2)`；`moderation-in-first-batch` / `drain-501` / `parity-501` 三断言）；`scripts/b5-pin.sh:41` = `moderation-priority-drill`（37-slot） |
| anti-starvation K-floor cap 未实现 = 已接受界 | ✅ 属实 | `docs/design/2026-08-08-aero-audit-connector-b5-3-claim-lane-carriage.design.md` F9（:273）+ D7（:345）：strict-priority 无 aging 时高 lane 常满会饿死低 lane，**latent 非 live**（高 lane 单一预算受限 producer vs ~1200 rows/min 名义容量）；修复 = sibling D-CAP，本 slice 不实现 |
| 缺口：无按需 CLI 面 + 无 `priority: landed/absent` 判定行 | ✅ 属实 | `crates/aero-cli/src/main.rs` 全文件 `priority` 零命中；drill 唯一入口 = `gate b5` → `test-integration.sh`（main.rs `"b5" => b(f("test-integration.sh"), 1800)`） |
| `network relay-probe` spawn 样板（env override + cargo fallback + 0/1/2 透传 + 120s + inherit） | ✅ 属实 | main.rs `Network_` relay-probe 臂：`AERO_RELAY_PROBE_BIN` override → `cargo run --quiet -p aero-audit-connector --bin aero-audit-relay-probe` fallback；`Some(0)→ok / Some(1)→error / Some(2)→Outcome::warning(2,…) / other→error`；120s timeout kill |
| Q2_SQL psql probe 样板（`audit_provision.rs`） | ✅ 属实 | `crates/aero-eng/src/audit_provision.rs`：Q2_SQL :42、Q3_SQL :48、G0239_CANDIDATES :26、`parse_probe_line`、`parse_psql_bool_line`、`PsqlRunner`（local/container 双模 + 30s per-query timeout）、`run()` :462、判定行前缀 `audit-provision-check: ` |
| 退出码穿透机制 | ✅ 属实 | `Outcome::warning(code)`（outcome.rs:47）；registry `execute_with_ctx` 仅 `Severity::Ok` 走 `Ok`，其余走 `Err(DispatchResult)` → main.rs `exit(r.exit_code())`；`run_cmd` 非零一律拍平成 `Outcome::error`（run.rs:15-41）——distinct 码必须直 spawn |
| drill 只读 `DATABASE_URL`（无 `AERO__DATABASE__URL` fallback） | ✅ 属实（设计要点） | drill bin：`std::env::var("DATABASE_URL").context("DATABASE_URL is required")?` —— CLI 面必须显式把已解析 URL 注入子进程 env |
| drill 对 outbox 做 TRUNCATE（自隔离） | ✅ 属实（安全要点） | drill bin「Self-isolating start (db-reviewer finding 3)」：TRUNCATE `audit_governance_outbox` —— **CLI 面须醒目警告只能对 throwaway 库跑** |

## 1. Design decisions

- **D1 — probe 无条件进 `run()`（base 模式输出 +2 行，additive）**：`--priority` 只是 drill 门，列探测属于 provisioning 契约本身，每次 `audit-provision-check` 都该报。base 模式退出码/判定行语义零变化（见 §3）。
- **D2 — 探测结果进 `AuditSnapshot`**：新增 `priority_landed: bool` + `class_landed: bool` 两个 pub 字段；`format_report` 无条件追加 `audit-provision-check: priority: landed|absent` / `class: landed|absent` 两行。结构体字面量仅存在于 `run()` 与 `crates/aero-eng/tests/audit_provision.rs`（6 处字面量 + `snapshot()` helper，机械更新）。
- **D3 — `--priority` 编排进 lib**：`pub async fn run_priority(db_url: &str) -> Outcome` 放 `audit_provision.rs`（spawn 机制照抄 relay-probe 臂：直 spawn / inherit / 120s / 码透传），main.rs 臂只做 flag 分发。理由：audit_provision.rs 是扩展宿主（562 行，加到 ~690 仍 < 800 WARN）；main.rs 749 行贴近 800 WARN，再塞 50 行 spawn 会越线；lib 已有 psql 子进程先例，`run.rs` 也已在 lib 层 spawn bash/cargo。
- **D4 — 单条探测 SQL**：`P_SQL` 一次返回两格 `t|f`（`-At` 竖线分隔），比两条查询少一次 psql 往返；固定 literal，无参数化通道，禁止用户输入进 SQL。
- **D5 — 纯解析函数**：`pub fn parse_priority_probe(line) -> Result<(bool, bool), String>`，可单测；`run()` 与 `run_priority()` 共用。
- **D6 — 子进程 env 显式注入**：`cmd.env("DATABASE_URL", db_url)`——CLI 臂解析出的 URL 无论来自 `DATABASE_URL` 还是 `AERO__DATABASE__URL` 都必须显式传给 drill（drill 只认 `DATABASE_URL`）。
- **D7 — 退出码契约**：drill 子进程 `0→Outcome::ok`、`1→Outcome::error`（排序坏/断言败 = FAIL）、`2→Outcome::warning(2,…)`（drill 内部能力门兜底 SKIP）、其他→error（异常退出）；120s 超时 kill + error（relay-probe 同款）。
- **D8 — 破坏性警告**：drill TRUNCATE outbox——spawn 前打印 `audit-provision-check: priority-drill: WARNING — drill TRUNCATEs audit_governance_outbox; run against a throwaway DB only`，help 文本同步。不做硬门禁（harness 与历史契约都依赖无 gate 直跑）。
  - **D8′ 修订（2026-08-08，destructive-gate direction：`docs/design/2026-08-08-aero-cli-b5-3-priority-drill-destructive-gate.design.md`）**：警告升级为条件硬门禁——`run_priority` 在列门与 spawn 之间探测 outbox 行数，非空且无 `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1` 则 REFUSED exit 1（`Outcome::error`）；drill 侧把 TRUNCATE-at-start 替换为 in-tx `LOCK ACCESS EXCLUSIVE + COUNT + TRUNCATE` 权威门（直接调用 drill bin 同样被拒）；harness 以 opt-in 承接「无 gate 直跑」。base 判定/exit 码/37 槽/`audit-provision-check` 无 flag 面零变化。
- **D9 — harness 腿换 CLI 面 + 非 vacuous grep**：run 行换成 `cargo run --quiet -p aero-cli -- audit-provision-check --priority`；输出落临时文件；rc==0 → grep `priority: landed` + `b5_check PASS`；rc==2 → `b5_check SKIP`；其余 → 红。0239 文件门控、throwaway 库生命周期、`cargo run --bin aero-cli -- migrate` 原样。
- **D10 — 未知 flag 报 usage error（exit 1）**：`args[2]` 既非 `--priority` 也非空 → `Outcome::error("usage: audit-provision-check [--priority]")`。今日 `_args` 被忽略（任意多余参数静默跑 base）——这是对未文档化行为的故意、响亮破坏：拼错 `--priorty` 若静默跑 base 会跳过 drill（silent-wrong-result 比 loud error 危险）。
- **D11 — 零新依赖/零迁移/零外部改动**：aero-eng 不链接 connector/aero-ai/aero-storage（probe 走 psql 子进程、drill 走 spawn）；drill bin、connector、0239/0240/0241 全不动；`Completion_` cmds 串与 `Gate_` 不动（flag 非子命令、不新增门面）。

## 2. API changes

### 2.1 `crates/aero-eng/src/audit_provision.rs`（新增/修改）

```rust
/// P — priority/class column probe (B5-3 CLI surface; mirrors the drill
/// bin's runtime capability gate, aero-audit-priority-drill.rs:105-130).
/// Two fixed literals; the runner is a single `psql -At -c` — no
/// parameter-binding channel, no user input reaches the SQL.
const P_SQL: &str = r"SELECT EXISTS (
    SELECT 1 FROM information_schema.columns
     WHERE table_name = 'audit_governance_outbox' AND column_name = 'priority'
), EXISTS (
    SELECT 1 FROM information_schema.columns
     WHERE table_name = 'audit_governance_outbox' AND column_name = 'class'
)";

/// Parse the two-cell psql boolean line ("t|f") → (priority_landed,
/// class_landed). Reuses [`parse_psql_bool_line`]; exactly two cells.
pub fn parse_priority_probe(line: &str) -> Result<(bool, bool), String>;

/// Probe both columns via the runner (private; called by `run` and
/// `run_priority`). Table absent → (false, false) from information_schema.
async fn probe_priority_columns(runner: &PsqlRunner) -> Result<(bool, bool), String>;

// AuditSnapshot: +2 pub fields
pub struct AuditSnapshot {
    // …existing…
    pub priority_landed: bool,
    pub class_landed: bool,
}

// format_report: +2 lines (g0239 Some 与 None 两个分支都印，字段驱动)
//   audit-provision-check: priority: landed|absent
//   audit-provision-check: class: landed|absent

/// `--priority` mode: base check (report + verdict lines, incl. the two
/// probe lines) → column gate → drill spawn with code passthrough.
pub async fn run_priority(db_url: &str) -> Outcome;
```

`run_priority` 控制流（直 spawn，**不得**走 `run_cmd`）：

1. `let base = run(db_url).await;`——打印完整 report；`base.is_error()` → 原样返回（DB 不可达/psql 缺失/URL 坏/超时一律先于 drill 失败，绝不让 drill 在坏 DB 上跑出误导性 FAIL）。
2. gate 重探（D2 字段是 run() 内部状态，不外泄；`run_priority` 自建 `PsqlRunner` 再跑 `P_SQL`——多一次单行查询，on-demand 路径可接受）：Err → `Outcome::error`；`(false, _) | (_, false)` → stdout 打印 `audit-provision-check: priority-drill: SKIP (priority/class columns not landed)` + `Outcome::warning(2, 同文案)`——**永不 FAIL**（phase-1 window，AC4）。
3. `(true, true)`：打印 D8 警告行 → spawn：
   - `AERO_PRIORITY_DRILL_BIN` 设了 → 直跑该 bin；否则 `cargo run --quiet -p aero-audit-connector --bin aero-audit-priority-drill`；
   - `cmd.env("DATABASE_URL", db_url)`（D6）；stdout/stderr inherit（drill 的 `drill: …: PASS` 行须原样可见，harness grep 依赖）；
   - 120s timeout：超时 `child.kill()` + `child.wait()` + `Outcome::error("priority drill timed out after 120s")`；
   - 码映射见 D7。

### 2.2 `crates/aero-cli/src/main.rs`

- `AuditProvisionCheck_` 臂：`|_ctx, _args|` → `|_ctx, args|`，`match args.get(2).map(String::as_str)`：
  - `None` → `aero_eng::audit_provision::run(&url).await`（现状原样）；
  - `Some("--priority")` → `aero_eng::audit_provision::run_priority(&url).await`；
  - `Some(_)` → `Outcome::error("usage: audit-provision-check [--priority]")`（D10）；
  - URL 缺失 error 路径不动。
- description 追加：`; --priority = moderation-priority drill (probe + 500-backlog drill; TRUNCATEs audit_governance_outbox — throwaway DB only)`。
- `Completion_` cmds 串 / `Gate_` / `gate list`：零改动（R5）。

### 2.3 `scripts/test-integration.sh`（:446-461 腿内替换）

```
    if [ -f "migrations/0239_audit_governance_outbox.sql" ]; then
        echo "▶ Creating fresh database for moderation-priority drill: ${PRIORITY_DRILL_DB}"
        create_throwaway_database "$PRIORITY_DRILL_DB"
        PRIORITY_DRILL_URL="${BASE_URL}/${PRIORITY_DRILL_DB}"
        echo "▶ Migrating database for moderation-priority drill..."
        DATABASE_URL="$PRIORITY_DRILL_URL" \
            AERO__DATABASE__URL="$PRIORITY_DRILL_URL" \
            cargo run --bin aero-cli -- migrate 2>&1 | tail -1
        echo "▶ Running moderation-priority drill via aero-eng CLI (500 backlog + 1 moderation)..."
        PRIORITY_DRILL_LOG="$(mktemp "${TMPDIR:-/tmp}/aero-priority-drill.XXXXXX")"
        set +e
        DATABASE_URL="$PRIORITY_DRILL_URL" \
            AERO__DATABASE__URL="$PRIORITY_DRILL_URL" \
            cargo run --quiet -p aero-cli -- audit-provision-check --priority \
            >"$PRIORITY_DRILL_LOG" 2>&1
        PRIORITY_DRILL_RC=$?
        set -e
        if [ "$PRIORITY_DRILL_RC" -eq 0 ]; then
            grep -q "priority: landed" "$PRIORITY_DRILL_LOG" || {
                echo "✗ priority drill: missing 'priority: landed' verdict line" >&2
                cat "$PRIORITY_DRILL_LOG" >&2
                exit 1
            }
            cat "$PRIORITY_DRILL_LOG"   # drill 的 PASS 行保持可见
            b5_check "moderation-priority-drill" "PASS"
        elif [ "$PRIORITY_DRILL_RC" -eq 2 ]; then
            cat "$PRIORITY_DRILL_LOG"
            b5_check "moderation-priority-drill" "SKIP (priority/class not landed)"
        else
            echo "✗ moderation-priority drill FAILED (exit $PRIORITY_DRILL_RC)" >&2
            cat "$PRIORITY_DRILL_LOG" >&2
            exit 1
        fi
        rm -f "$PRIORITY_DRILL_LOG"
        drop_created_database "$PRIORITY_DRILL_DB"
    else
        b5_check "moderation-priority-drill" "SKIP (0239 not landed)"
    fi
```

（`set -euo pipefail` 已开；rc 捕获用 `set +e` 包住。SKIP 分支语义与 pin guard 的 `B5-CHECK …: SKIP` 协议兼容。）

### 2.4 新增 env

- `AERO_PRIORITY_DRILL_BIN`——预编译 drill 路径 override（对齐 `AERO_RELAY_PROBE_BIN` 惯例；冷构建逃生门，见 FM7）。

## 3. Compatibility constraints

1. **base 模式输出 = 现状 + 恰好 2 行**：新行前缀 `priority:` / `class:` 与既有前缀（`relay:`、`v1-outbox:`、`outbox-0239:`、`oldest-pending-age:`、`dead:`、`verdict:`）不冲突；harness t11 C/D 腿的 grep（`verdict: healthy`、`outbox-0239: table=…`、`oldest-pending-age:`、`dead=1`、`verdict: fail-closed`）全部不受影响。
2. **base 模式退出码不变**：仍由 verdict 驱动（fail-closed→1，consistent/healthy→0）。`run()` 签名不变——既有调用方（main.rs 臂、`tests/audit_provision.rs`）零破坏。
3. **`audit-provision-check <任意多余参数>`**：从「静默忽略」变为「usage error exit 1」——对未文档化行为的有意破坏（D10），文档记录。
4. **aero-eng 零 DB crate 依赖**：probe = psql 子进程（既有 PsqlRunner），drill = spawn——不新增 Cargo.toml 依赖；`unsafe_code=forbid`、clippy pedantic 约束照旧。
5. **bin 名别混**：`cargo run -p aero-cli`（aero-eng 工程 CLI，t11 同款）vs `cargo run --bin aero-cli -- migrate`（aero-server 的 DB CLI）——harness 两处保持各自形态。
6. **不动**：drill bin、connector、storage、0239/0240/0241、`Completion_` cmds 串、`Gate_`、`b5-pin.sh` 37-slot 清单（`test-b5-pin-guard.sh` 37/37 必须保持绿）。

## 4. Failure modes

| # | 失败 | 行为 | 为什么安全 |
|---|---|---|---|
| FM1 | P_SQL 查询失败（psql 权限/网络） | 与 Q0–Q5 同款 `outcome_error` → exit 1 | fail-closed：契约行绝不静默缺失 |
| FM2 | 0239 表/列缺席（未迁或列被 drop） | `priority: absent` + `class: absent` + SKIP 行 + **exit 2** | 永不 FAIL，phase-1 window 保留（AC4） |
| FM3 | drill 子进程 exit 2（自己的能力门：probe 与 drill 间竞态/更严检查） | `warning(2)` + SKIP 语义 | 与 FM2 同向，不误报红 |
| FM4 | drill exit 1（排序坏 / drain 卡 / parity 错） | `Outcome::error` → exit 1 | 诚实 G6 红信号：0239 落地但 B5-3 排序未落地 |
| FM5 | drill 异常退出（>2 或信号） | error "exited abnormally: {status}" | 不吞异常 |
| FM6 | spawn 失败（`AERO_PRIORITY_DRILL_BIN` 指向不存在路径 / cargo 缺失） | error "cannot launch priority drill: {e}" | relay-probe 同款 |
| FM7 | 120s 超时（冷 checkout 下 `cargo run -p aero-audit-connector` 编译 sqlx/reqwest 图可能 >120s） | kill + error "timed out after 120s" | 已知界：relay-probe 同款暴露；逃生门 = `AERO_PRIORITY_DRILL_BIN` 指预编译 bin 或先 `cargo build -p aero-audit-connector`；harness（gate b5）场景工作区已编译 |
| FM8 | 用户只设 `AERO__DATABASE__URL` 没设 `DATABASE_URL` | 臂解析 URL 后 `cmd.env("DATABASE_URL", db_url)` 显式注入（D6） | 否则 drill 死在 "DATABASE_URL is required"——困惑性失败 |
| FM9 | 对 live/生产库跑 `--priority` | spawn 前醒目 WARNING（D8）+ help 文案 + 本文档；drill TRUNCATE 会清空 outbox | 无法可靠检测库性质；警告 + 纪律（AGENTS §4.3 throwaway 库） |
| FM10 | 误用 `run_cmd` 包 drill | 设计强制直 spawn（D3/D7） | `run_cmd` 把非零码拍平成 1，SKIP=2 无法穿透（已验证 registry 仅 `Severity::Ok` 走 Ok 路径） |
| FM11 | base 步失败（DB 不可达/psql 缺失/URL 坏/超时） | run_priority 原样返回 error，drill 不 spawn | drill 在坏 DB 上只会产出误导性 FAIL |
| FM12 | flag 拼错（`--priorty`） | usage error exit 1（D10） | 响亮失败，绝不静默跑 base |
| FM13 | drill bin 被改名/删除 | cargo fallback 失败 → error；harness 腿红 | 期望行为：G6 诚实信号 |
| FM14 | 并发跑 drill 于同一库 | drill 自隔离（TRUNCATE + 幂等 seed）；仍只准 throwaway 库 | 与现状 harness 契约一致 |

## 5. Migration steps（rollout；**无 DB 迁移**——0239/0240/0241 零改动）

1. **代码**：`audit_provision.rs`（P_SQL、`parse_priority_probe`、`probe_priority_columns`、AuditSnapshot +2 字段、format_report +2 行、`run_priority`）→ `main.rs` 臂分发 + help → `tests/audit_provision.rs`（新测试 + 6 处结构体字面量/snapshot helper 机械补字段）。
2. **构建门**：`cargo check --workspace` → `cargo test --workspace --lib`（新单测绿）→ `cargo clippy --workspace --all-targets`（零新警告）→ `scripts/truth-check.sh`（`run_priority` 被 main.rs 臂调用、`parse_priority_probe` 被 run()/run_priority 调用、`probe_priority_columns` 被两者调用——零调用即红，**不加 allowlist**）→ `scripts/file-size-check.sh`（audit_provision.rs ~562→~690 < 800 WARN；main.rs 749→~765 < 800 WARN；tests 384→~470 < 800）。
3. **harness**：按 §2.3 换腿；`bash scripts/test-b5-pin-guard.sh` 37/37 绿（slot 数不变）。
4. **活验证（全新一次性库，AGENTS §4.3）**：`CREATE DATABASE` → `cargo run --bin aero-cli -- migrate` →
   - base：`DATABASE_URL=… cargo run -p aero-cli -- audit-provision-check` → exit 0 + 既有行 + `priority: landed`；
   - `--priority`：exit 0 + `drill: moderation-in-first-batch: PASS` / `drain-501: PASS` / `parity-501: PASS`；
   - 未迁 0239 的库（或列 drop）：exit 2 + `priority: absent` + SKIP 行，**断言非 exit 1**；
   - `--priorty`：exit 1 + usage；
   - 用完 `DROP DATABASE`。
5. **gate b5 全量**：1800s 门内腿转 PASS 不回归。

## 6. Testable acceptance mapping

| 验收（req §5 原句保留） | 可测断言 | 位置 |
|---|---|---|
| **AC1** 新 CLI 子命令跑 500-backlog+1-moderation drill against throwaway migrated DB | `DATABASE_URL=<已迁移一次性库> cargo run -p aero-cli -- audit-provision-check --priority` exit 0，stdout 含 `audit-provision-check: priority: landed` + `drill: moderation-in-first-batch: PASS` + `drill: drain-501: PASS` + `drill: parity-501: PASS` | harness 腿 §2.3 + 手工冒烟 §5.4 |
| **AC2** 0239+priority 落地后 admin.content.flag 行 claimed first | drill bin 断言即 oracle（零改动）：moderation 行（class admin / priority 100 / action `admin.content.flag`，后入）∈ 首轮 top-100 claimed 批（D3 heap-order 契约，非 delivered 严格 first）+ 全 drain + set-parity；CLI 层 = exit 0 透传 | drill bin :192-240；CLI = D7 |
| **AC3** anti-starvation cap 仍 drain backlog | `drain-501` + `MAX_ROUNDS=10`：501/501 status=2、stuck=0、event_id parity（drill bin :250-276）。D-CAP 未实现 = D7 已接受界；drill 全 drain 即本仓测试形式，D-CAP 落地后同断言继续覆盖 | drill bin（零改动） |
| **AC4** priority 列缺席 → clear SKIP (exit 2)，never FAIL，phase-1 window 保留 | 未迁 0239 库跑 `--priority` → exit 2 + `priority: absent` + `priority-drill: SKIP`；**断言非 exit 1**；harness 0239 文件缺席 → `SKIP (0239 not landed)` 分支原样 | run_priority gate（D7）+ harness 门控 |
| **AC5** report line greppable（`priority: landed/absent`） | base 与 `--priority` 两模式 stdout 均含 `audit-provision-check: priority: landed\|absent`；harness 腿 `grep -q "priority: landed"`（非 vacuous 证据，缺 → 腿 FAIL） | format_report 两行 + §2.3 |
| **AC6** harness `b5_check 'moderation-priority-drill'` 与 sibling B5-3 slice 锁步 | 换 CLI 面后腿保持 **PASS 不回归 SKIP**；`B5-CHECK moderation-priority-drill: PASS` 出现在 B5 log；`scripts/test-b5-pin-guard.sh` 37/37 绿（slot 已钉 `b5-pin.sh:41`；SKIP→PASS 翻转已随 0239+排序完成，本 direction 的保真形式 = PASS 不回归） | test-integration.sh + b5-pin.sh + test-b5-pin-guard.sh |
| 新增单测 | `parse_priority_probe`：`"t|t"→Ok((true,true))`、`"f|t"→Ok((false,true))`、`"t"`/`"x|t"`/`""`→Err；format_report：`priority_landed=true` → 含 `priority: landed`；false / g0239=None → 含 `priority: absent` | `crates/aero-eng/tests/audit_provision.rs` |
| 负例 | `--priorty` → exit 1 + usage；`AERO_PRIORITY_DRILL_BIN=/bin/false` → exit 1（spawn 通、drill FAIL 透传）；`AERO_PRIORITY_DRILL_BIN=/nonexistent` → exit 1（cannot launch）；只设 `AERO__DATABASE__URL` → drill 仍连上（D6） | 手工冒烟 §5.4 |

## 7. Out of scope（与 req §3 一致）

- 0239/0240/0241 DDL、claim SQL、`mixed_priority_claim`、drill bin——零改动。
- **D-CAP K-floor 反饥饿 cap**（F9/D7 已接受界，sibling record 排期；drill 全 drain 断言覆盖 drill 语境）。
- `network priority-drill` 备选命令（否决：`network` 是 DB-free 命名空间；`audit-provision-check` 已要求 DATABASE_URL 且其 psql probe 面正是列探测宿主）。
- 新 gate 门面（drill 面由 `gate b5` 覆盖）。
