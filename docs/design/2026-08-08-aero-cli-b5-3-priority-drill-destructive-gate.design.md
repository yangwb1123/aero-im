# Design — `audit-provision-check --priority` 破坏性门禁 + B5-3 出站 action 词汇表钉入

- **Status**: Design（证据逐条独立复核完毕，两处勘误见 §0.1；三轮复核 security/database/testing 发现全部纳入或显式拒绝，见 §7 纳入矩阵）
- **Source spec**: `docs/requirements/2026-08-08-aero-cli-b5-3-priority-drill-destructive-gate.req.md`
- **Landing design being revised**: `docs/design/2026-08-08-aero-cli-b5-3-moderation-priority-drill.design.md`（D8「不做硬门禁」→ 本设计 D8′ 条件门禁）
- **Sibling**: `docs/requirements/2026-08-08-aero-ai-b5-3-token-parity-harness.req.md`（token 裁决：叶子锁 `admin.content.flag`，不翻转）
- **Files touched**: `crates/aero-eng/src/audit_provision.rs` · `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs` · `scripts/test-integration.sh` · `docs/requirements/2026-08-08-aero-cli-b5-3-priority-drill-destructive-gate.req.md`（14→25 勘误注记）· 两设计文档修订注记 ·（可选）`crates/aero-cli/src/main.rs` help 一句

## §0 证据复核（untrusted claims → 独立实读）

| Evidence claim | Verdict | 依据（实读锚点） |
|---|---|---|
| `run_priority` :640-718；base 先行；列门；WARNING；`AERO_PRIORITY_DRILL_BIN` :675；120s timeout；**无 outbox 守卫** | ✅ 精确 | `audit_provision.rs` 恰 718 行。fn :640；base :644-651（`base.is_error()` 即返）；列门 :652-667；警告 :672-676；env override :675（`AERO_PRIORITY_DRILL_BIN` 读取行）；timeout :699-700；exit 透传 :705-712。②③之间零行探测 |
| Drill TRUNCATE :105-113 | ✅ 精确 | `aero-audit-priority-drill.rs`：注释 :105-108、`TRUNCATE` :109-111、println :113；能力门（表 :92-103、列 :115-133） |
| CLI `AuditProvisionCheck_` :488-506 响亮 usage error、无破坏性门 | ✅ 精确 | `main.rs` c! :488；usage error :501；exit 1 经 main.rs :42-45 `exit(r.exit_code())` |
| Harness 腿 :443-484 全新 throwaway DB、无 opt-in env | ✅ 精确 | `test-integration.sh`：注释 :443-447、腿 :448-484；`:37 PRIORITY_DRILL_DB="aero_priority_drill_$$"`；`assert_disposable_db_name` :40；env 仅 `DATABASE_URL`/`AERO__DATABASE__URL` :465-466 |
| 唯一出站 token；`admin.moderation.action` 无处出现 | ⚠️→✅ 一处勘误 | 叶子 `aero-common/src/model/audit.rs:150` 常量 + pin :269；`aero-ai/src/governance.rs:41` re-export 链（`GOVERNANCE_PRIORITY_MODERATION=100` :31、`BACKLOG=10` :33）。勘误：第二拼写出现在**恰一处 doc 注释**（audit.rs:147）；作为 token/DDL/断言零表示 |
| 存储断言只钉单 token | ✅ | `audit_governance.rs:320-321`、`:884`：均 `assert_eq!(…["action"], MODERATION_OUTBOUND_ACTION, …)` |
| 契约 item (3) 来源 | ✅ | `docs/proposals/audit-contract-batch-aero-im.md:10` 与 `docs/campaigns/implementation-gate.md:65`：两拼写并列 |
| 设计 D8「不做硬门禁」 | ✅ | `…moderation-priority-drill.design.md:32` |
| `verdict()` 把 0239 pending/claimed 计入 undelivered（F2） | ✅ | `audit_provision.rs:133-145`：`undelivered = v1.pending + v1.claimed + g0239.pending + g0239.claimed`；relay 关 + undelivered>0 → fail-closed。status 0/1 行 base 即红；守卫边际保护 = status 2 历史行 |
| drill 常量 :55-73（500+1=501 / batch 100 / prio 10·100 / MAX_ROUNDS=10） | ✅ | 实读逐常量吻合 |
| F4：B5 交付物未提交工作树 | ✅ | `git status --short`：B5 文件 M/??；另有非 B5 untracked（`.pi-batch.lock`、`crates/aero-live-srt/src/isolation_tests.rs` 等） |
| F5：truth-check 无 TOKEN 类别 | ✅ | `grep -c TOKEN scripts/truth-check.sh` = 0 |
| **canonical copy「identical」** | ❌ **不成立** | `diff` 非空：artifact `requirements-10762e10/requirements.md` = 21 行摘要（自述性头 + 复核表 + findings）；canonical `docs/requirements/2026-08-08-…-destructive-gate.req.md` = 156 行完整 spec。同方向同锚点，但**非同一文件**。后续引用一律以 canonical 为准 |
| **「14 个纯函数测试」** | ⚠️ **少报** | `crates/aero-eng/tests/audit_provision.rs` 实际 **25 个 `#[test]`**（8×verdict_*、3×report_*、2×parse_priority_probe_*、1×report_priority_and_class、1×parse_psql_bool_line、1×parse_relay_line、1×parse_probe_line、1×parse_buckets、1×parse_v1_line、1×parse_dead_rows、4×parse_db_url_*、1×run_without_url）。「14」只枚举了 verdict/report/priority-probe 子集；**零改动回归面 = 25** |

结论：证据主体成立；两处修正（canonical 非 identical、测试数 14→25）纳入本设计验收措辞。

## §1 API changes

### 1.1 `crates/aero-eng/src/audit_provision.rs`（R1 守卫，insert 于 :667 列门之后、:672 警告之前）

新增：

```rust
/// Fixed literal — same pattern as P_SQL; no user input reaches the SQL.
const COUNT_SQL: &str = "SELECT COUNT(*)::bigint FROM audit_governance_outbox";

/// Mirror of parse_priority_probe (:226): psql -At single-value contract.
/// Garbage in → Err (fail-closed).
pub fn parse_outbox_count(out: &str) -> Result<i64, String>;

/// count > 0 && !allow_truncate → block. Pure, unit-testable.
pub fn priority_drill_blocked(count: i64, allow_truncate: bool) -> bool;
```

`run_priority` 插入分支（复用 :654 已建 `PsqlRunner`）：

```rust
// 2.5 — D8′ destructive gate (before the D8 warning + spawn):
let count_raw = runner.query(COUNT_SQL).await.map_err(|e| {
    Outcome::error(format!("audit-provision-check: {e}"))   // 探测失败 → fail-closed，绝不带病 spawn
})?;
let count = match parse_outbox_count(&count_raw) {
    Ok(c) => c,
    Err(e) => return Outcome::error(format!("audit-provision-check: {e}")),
};
let allow = std::env::var("AERO_PRIORITY_DRILL_ALLOW_TRUNCATE").as_deref() == Ok("1");
if priority_drill_blocked(count, allow) {
    let mut msg = format!(
        "audit-provision-check: priority-drill: REFUSED — audit_governance_outbox \
         has {count} row(s); the drill TRUNCATEs the table. Re-run with \
         AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 (throwaway DB only)"
    );
    // security F4: 双下划线 figment 拼写（代码库主导约定）被静默忽略 → REFUSED；
    // 检测到即主动 hint，消除「设了却没生效」的困惑（fail-closed 不变）。
    if std::env::var("AERO__PRIORITY__DRILL__ALLOW_TRUNCATE").is_ok() {
        msg.push_str(
            " (note: AERO__PRIORITY__DRILL__ALLOW_TRUNCATE with double underscores \
             is ignored — single underscore only)",
        );
    }
    println!("{msg}");
    return Outcome::error(msg);   // exit 1，经 main.rs :42-45
}

// security F4: :697 spawn 失败路径追加 BIN hint（仅当 BIN 已设置时）：
//   Err(e) => {
//       let hint = if std::env::var("AERO_PRIORITY_DRILL_BIN").is_ok() {
//           " (AERO_PRIORITY_DRILL_BIN must name an executable path — if you meant \
//            to allow the TRUNCATE, set AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1)"
//       } else { "" };
//       Outcome::error(format!("cannot launch priority drill: {e}{hint}"))
//   }
```

- **env 语义**：plain 单下划线 `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE`，值 `== "1"` 才放行（与 `AERO_PRIORITY_DRILL_BIN` :675、`AERO_SAML_EXPERIMENTAL_VERIFY=1` 先例一致）。`"true"`/`"yes"`/空 → 仍拒绝（fail-closed env）。
- **不新增 CLI flag**：`AuditProvisionCheck_` args 面字节级不变（AC4）。
- **顺序不变量**：base `run()`（:644-651）与列门 SKIP exit-2（:662-667）原样保留——守卫只在 base 判定 Consistent/Healthy 后生效（F2）。
- **双层门禁**（security F1/F2 裁决，理由链见 §7）：本守卫 = **快速失败 UX 层**（避免冷 `cargo run` 编译数十秒后才知 REFUSED）；**权威门禁在 drill 侧 in-tx 门（§1.2a）**——drill 可被直接调用（其 usage 头 drill.rs :4-9 文档化该路径，直接调用命令 :7-8），wrapper 守卫不覆盖直接调用。
- **env 继承**（security cross-cutting）：`run_priority` spawn 不 `env_clear()`——子进程继承同一 `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE`（与 `AERO_PRIORITY_DRILL_BIN` 同理），wrapper→drill 两层一致；**未来若加 `env_clear()` 必须保留 drill 侧显式 env，否则 F1 复开**——明示。
- **exit-1 碰撞**（security F3）：REFUSED 与 base-fail / FM1 / FM2 / drill-FAIL 同码 1——fail-closed 正确读法（列已落 → 非 SKIP；出错 → 非 0）；按消息前缀 `priority-drill: REFUSED` 区分（harness 已 grep `REFUSED`）。
- **双打印**（security F3 / db-reviewer nit）：`println!` + `Outcome::error` 双打印（stdout + main.rs :42-45 stderr）——同 SKIP 先例（:661-665），harness `2>&1` 双流入日志，无害；消费方不得假设消息单次出现。
- **COUNT_SQL 第三处硬编码表名**（security F5）：漂移响亮失败（psql 错误 → FM1 exit 1），绝不静默；列门 SKIP（:662-667）先于计数保证 `COUNT_SQL` 永不在缺表时执行。

### 1.2 `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（R3 词汇表钉入）

```rust
/// Contract item 3 outbound vocabulary (proposal :10, implementation-gate.md:65;
/// leaf audit.rs:146-150 locks ONE constant — either spelling is contract-legal).
/// Hardcoded, NOT derived from the leaf: a leaf flip must not auto-follow and
/// silently kill the pin.
const MODERATION_OUTBOUND_ACTIONS: [&str; 2] = ["admin.content.flag", "admin.moderation.action"];
```

- **种子期检查**（`MODERATION_ACTION` 定义处/seed 前）：`MODERATION_OUTBOUND_ACTIONS.contains(&MODERATION_ACTION)` 否则 `anyhow::bail!(…)` → exit 1 红。
- **投递后读回**（置于 :227-238 `delivered_at` 读回之后、`moderation-in-first-batch: PASS` 之前或之后）：`SELECT payload->>'action' FROM audit_governance_outbox WHERE event_id = $1`——`fetch_one` → `Option<String>`；**event_id 缺失或 `action` 为 NULL → 红 FAIL**（security F7：不得 no-op 放行）→ 断言 ∈ 词汇表 → `println!("drill: moderation-action-vocabulary: PASS")`。
- **读回边界（db-reviewer 精确化）**：这是**种子行重读，非 wire 级投递证据**——relay deliver 原样转发 `claim.payload` 且从不回写 payload（`settle()` 只 UPDATE status/delivered_at/claim_token/lease/last_error），stub 也只解析 `event_id`；读回值按构造 = 种子值。其价值：(a) 可 grep 的 `drill: moderation-action-vocabulary: PASS` 验收行；(b) 对未来 relay/stub payload 改写路径的纵深防御（届时才成为真实投递证据）；(c) 顺带传递验证 relay claim/deliver 路径不改写 payload。与种子期检查今日严格冗余，双保险无害。
- **残余威胁（security F7）**：协调性源码篡改（drill 种子字面量 + 检查 + bin 单测一起改）可绕过 pin——pin 防意外漂移，不防恶意编辑；出威胁模型，明示。
- **bin 单测** `#[cfg(test)]`：逐字量断言两拼写 + `contains(MODERATION_ACTION)`（镜像叶子 :269 形态）。
- **不变面**：`moderation-in-first-batch` / `drain-501` / `parity-501` 三 PASS 行、TRUNCATE-at-start、能力门 exit-2、500+1=501、batch_size=100、`MODERATION_ROWS=1`。

### 1.2a `aero-audit-priority-drill.rs` — in-tx 权威门禁（security F1+F2 修复，关闭 db-reviewer 竞态）

drill 开跑即 TRUNCATE（:109-111）且**可被直接调用**（usage 头 :4-9 文档化）——wrapper 守卫不覆盖直接调用；且 wrapper psql COUNT 与 drill TRUNCATE 是两条独立连接（中间含冷 `cargo run` 编译窗口，可达数十秒）。**裁决：采纳 in-drill in-tx 门（security F2 形态），拒绝「仅文档化残留」**——直接调用是受支持路径而非误配，文档化无法关闭它（理由链见 §7）。替换 :105-113：

```rust
// Gate 1.5 — D8′ destructive gate (authoritative): the wrapper's psql COUNT
// is fast-fail UX; THIS lock+count+TRUNCATE is the gate that cannot be
// skipped — the drill's own usage header documents direct invocation.
// ACCESS EXCLUSIVE blocks concurrent writers (incl. the 0239 enqueue
// trigger's INSERT, which blocks inside its own audit_events tx) so the
// count and the TRUNCATE are atomic; TRUNCATE is transactional in PG.
let mut tx = pool.begin().await.context("begin drill gate tx")?;
sqlx::query("LOCK TABLE audit_governance_outbox IN ACCESS EXCLUSIVE MODE")
    .execute(&mut *tx).await.context("lock outbox for the drill gate")?;
let n: i64 = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM audit_governance_outbox")
    .fetch_one(&mut *tx).await.context("count outbox rows in the drill gate")?;
if n > 0 && std::env::var("AERO_PRIORITY_DRILL_ALLOW_TRUNCATE").as_deref() != Ok("1") {
    tx.rollback().await?;
    eprintln!(
        "aero-audit-priority-drill: REFUSED — audit_governance_outbox has {n} row(s); \
         the drill TRUNCATEs the table. Re-run with AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 \
         (throwaway DB only)"
    );
    std::process::exit(1);
}
sqlx::query("TRUNCATE audit_governance_outbox").execute(&mut *tx).await
    .context("reset the governance outbox (TRUNCATE-at-start guard)")?;
tx.commit().await?;
println!("reset audit_governance_outbox (TRUNCATE-at-start guard)");
```

- **放置**：表能力门（:92-103）之后、原 TRUNCATE 块（:105-113）原位——列能力门（:115-133）仍在后（顺序不变量不变）。列缺失 + 非空 outbox 的共享库：REFUSED exit 1（此前行为 = TRUNCATE 后 SKIP——fail-closed 改进）。
- **语义与 wrapper 完全一致**：env 值 `== "1"` 才放行；REFUSED = exit 1（消息含 `REFUSED` 供 grep）。**跨 crate 重复**（aero-eng + aero-audit-connector 各一份）：可接受——漂移方向全部 fail-closed（drill 侧更紧 → wrapper 放行的运行被 drill 拒红，harness 自捉；drill 侧更松 → wrapper 先拒不可达，直接调用下不劣于今日零门禁）。
- **TOCTOU 关闭**：wrapper COUNT=0 → 窗口期 relay 落行 → drill in-tx count>0 → REFUSED（行存活，run 红）。wrapper 侧把子进程 exit 1 报为「one or more assertions FAILED」（:707）——消息措辞为 FAIL 而语义为 REFUSED，可经 drill stderr 的 `REFUSED` 区分；fail-closed 不变。
- **lock_timeout 不加**（db-reviewer 观察）：既有无 `lock_timeout`（TRUNCATE 本身即 ACCESS EXCLUSIVE 且无限等待）——in-tx 门把锁持有延长一个 COUNT 查询（微秒级），不扩大爆炸半径；加锁超时属共享库行为变更，出范围（§7 显式拒绝）。

### 1.3 `scripts/test-integration.sh`（R2）

- 正式 drill 的 env 块（:465-467）加 `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 \`（**单下划线拼写**——拼成双下划线会 REFUSED 红，腿自捉）。
- migrate 后、正式 drill 前插入负例子检查块（**全部位于 `if [ -f "migrations/0239_audit_governance_outbox.sql" ]` 门内**——未迁移库应 SKIP 而非报错，db-reviewer 建议）：
  1. `INSERT` 1 行 **status=2**（payload `{"event_id": <uuid>, "source_system": "aero-im.source"}`）——F2：status 0/1 会让 base 先 fail-closed，测不到守卫本身；status=2 是唯一不先触发 base 的 fixture（security F6）；
  2. **无** opt-in 跑 wrapper `--priority` → 断言 **`RC == 1`**（钉死 error 语义，非 `!= 0`——testing F1）+ 日志含 `REFUSED` + `SELECT COUNT(*)` 仍为 1（TRUNCATE 未发生）；
  3. **无** opt-in **直接调用 drill bin**（`DATABASE_URL=… cargo run --quiet -p aero-audit-connector --bin aero-audit-priority-drill`，usage 头同款）→ 断言 **`RC == 1`** + 日志含 `REFUSED` + COUNT 仍为 1——**钉住 FM11（in-tx 门覆盖直接调用）**，顺带预热 connector bin 供正式 drill 用；
  4. `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=true`（值拼错）跑 wrapper → **`RC == 1`** + `REFUSED`——FM5 端到端（testing F4，采纳为必做：~1s 成本钉住 `== Ok("1")` 严格性）；
  5. `DELETE` 预置行（**必须**：否则 drill 轮 1 `COUNT(status=2)==100` 断言被预置 delivered 行破坏，drill :214-223；TRUNCATE-at-start 是兜底，DELETE 仍必做——腿自包含 + drill SKIP 路径不留脏）；
  6. 正式 drill（带 opt-in，:465-467）原流程不变：RC 0 + grep `priority: landed` + `b5_check` PASS；RC 2 → SKIP；其余 → 红。
- 负例断言只 echo 痕迹行（如 `✓ priority-drill guard: REFUSED on non-empty outbox`），**不调用 `b5_check`**——37 槽一字不动。
- 零改动面：B1/B2 腿（:256-305）、D 腿（:396-421）、t11-fail-closed 段（:343-447）、`b5-pin.sh` 37 槽。

### 1.4 其他

- `docs/design/2026-08-08-aero-cli-b5-3-moderation-priority-drill.design.md` 记 D8′ 修订注记（警告→条件门禁，理由：共享 dev DB 静默销毁 status-2 历史行；harness 以 opt-in 承接「无 gate 直跑」）。
- `docs/requirements/2026-08-08-aero-cli-b5-3-priority-drill-destructive-gate.req.md` 记 14→25 勘误注记（E10/R1/R4/§4(d)，testing F5）——canonical 是引用基准（§0），「25」不得漂回「14」。
- `main.rs` help 描述加一句（additive，可选）：「refuses on a non-empty outbox unless `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1`」。

## §2 Compatibility constraints

| 面 | 约束 |
|---|---|
| CLI args | `audit-provision-check [--priority]` 零改动；未知 flag 仍响亮 usage error（:501） |
| exit 码契约 | 0 PASS / 1 FAIL / 2 SKIP 透传不变；REFUSED = exit 1（error 语义，非 warning）。**REFUSED 与 FAIL 码值不可分**（security F3）——只能靠消息前缀 `priority-drill: REFUSED` 区分（harness 已 grep `REFUSED`）；这是 fail-closed 正确读法（列已落 → 非 SKIP；出错 → 非 0） |
| base 行为 | `run()`/`verdict`/`format_report`/`parse_buckets`/`parse_priority_probe`/`probe_priority_columns` 零触碰；判词行字节级不变 |
| 既有测试 | `tests/audit_provision.rs` **25** 个测试（修正 canonical 的「14」）零改动全绿；新增纯函数单测 |
| 37-pin | `scripts/b5-pin.sh` 37 槽一字不动；`moderation-priority-drill` 槽判词来源不变（仍为正式 drill 的 PASS） |
| 空 outbox 语义 | COUNT==0 → 无 opt-in 也放行——既有 throwaway 用法与历史契约不破；harness 仍显式设 opt-in（意图声明）。**TOCTOU 关闭（db-reviewer 竞态）**：wrapper psql COUNT 只是快速失败探针；权威门禁 = drill 侧 in-tx `LOCK+COUNT+TRUNCATE`（§1.2a）——窗口期落库的行要么被 in-tx count 看到 → REFUSED exit 1（fail-closed，行存活），要么不存在（无可失）。唯一残留破坏路径 = 显式 opt-in（FM10） |
| env 拼写 | 单下划线 plain env，与 `AERO_PRIORITY_DRILL_BIN`/`AERO_RATE_LIMIT_PER_SEC` 同类；`AERO__` 前缀双下划线体系**不用**（本地进程 env，非 figment 配置）；双下划线拼写 `AERO__PRIORITY__DRILL__ALLOW_TRUNCATE` 被静默忽略 → REFUSED（fail-closed），REFUSED 消息检测到该变量存在时追加「single underscore only」hint（security F4） |
| token 裁决 | 叶子仍锁 `admin.content.flag`（sibling R1）；本设计只钉「两拼写皆契约合法」断言面，不翻转、不加第二叶子常量 |
| 提交门 | 显式路径提交（F4），勿 `git add -A` |

## §3 Failure modes

| # | 场景 | 行为 | 备注 |
|---|---|---|---|
| FM1 | 守卫探测 psql 失败 | `Outcome::error` exit 1，绝不 spawn | fail-closed；文档化手工复验步骤见 §5 |
| FM2 | 计数输出垃圾（非数字） | `parse_outbox_count` Err → error exit 1 | 单测覆盖 |
| FM3 | 共享 dev DB 残留 status-2 历史行 | REFUSED exit 1 + 显式消息；行存活 | **本设计要堵的主洞**。覆盖矩阵（security F6）：status 0/1 → base fail-closed；**2 → 门禁（唯一新增保护面）**；3 → base fail-closed。harness 负例种 status=2 = 唯一不先触发 base 的 fixture |
| FM4 | outbox 有 status 0/1 行 | base 先 fail-closed（exit 1），守卫不达 | 既有行为，文档化非新增 |
| FM5 | env 值拼错（`true`/`yes`） | ≠ `"1"` → 仍拒绝 | fail-closed env 语义；harness 负例 4（`=true` 显式跑）端到端钉住（testing F4） |
| FM6 | 叶子翻出契约对（如 `"mod.flag"`） | drill 种子期 bail → exit 1 红 | 词汇表 pin 生效 |
| FM7 | 未来裁决翻叶子到 `"admin.moderation.action"` | 断言仍 PASS（两拼写合法） | 与 sibling 翻转协议兼容 |
| FM8 | harness 负例后忘 DELETE 预置行 | drill TRUNCATE-at-start 会清掉，轮 1 不破；但 DELETE 仍必做（自包含 + drill SKIP 路径下不留脏） | 腿内顺序强制 |
| FM9 | drill 120s 超时 | kill + error（既有） | 不变；文档化手工复验步骤见 §5 |
| FM10 | 有人在真生产库上设 opt-in | 仍会 TRUNCATE——env 是意图声明非安全装置；spawn 前 D8 警告行仍在 | 文档化纪律（AGENTS §4.3 throwaway 库）。**in-drill 门落地后这是唯一残留破坏路径**：需显式 opt-in + 非空 outbox 同时成立（wrapper 与 drill 双层同读同一 env） |
| FM11 | **直接调用 drill bin**（跳过 wrapper——drill usage 头 :4-9 文档化该路径） | drill 侧 in-tx 门 REFUSED exit 1（eprintln + 行存活）；带 opt-in 才 TRUNCATE | **security F1 修复**：门禁在破坏动作所在叶子；harness 负例 3 自动覆盖 |

## §4 Migration steps

无 DB 迁移（纯 CLI/harness/drill 代码）。实施顺序：

1. `crates/aero-eng/src/audit_provision.rs`：`COUNT_SQL` + `parse_outbox_count` + `priority_drill_blocked` + 守卫分支（含双下划线探测 hint、BIN spawn 失败 hint）+ 两个纯函数单测（`tests/audit_provision.rs`，加进既有 25 个旁）。
2. `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`：**in-tx 门（§1.2a，替换 :109-111 TRUNCATE 块）** + 词汇表常量 + 种子期检查 + 投递后读回 + bin `#[cfg(test)]`。
3. `scripts/test-integration.sh`：opt-in env + 负例子检查 1/2/3/4 + DELETE + 正式 drill。
4. `docs/requirements/2026-08-08-…-destructive-gate.req.md` 14→25 勘误注记（E10/R1/R4/§4(d)，testing F5）；`docs/design/2026-08-08-aero-cli-b5-3-moderation-priority-drill.design.md` D8′ 修订注记；（可选）main.rs help 一句。
5. `cargo check --workspace` → `cargo test -p aero-eng --lib --test audit_provision` → `cargo test -p aero-audit-connector --bin aero-audit-priority-drill` → `bash scripts/test-b5-pin-guard.sh` → 带 DB 的 B5 段（throwaway 库，`make migrate-smoke` 流程）。
6. 显式路径提交（P0）。

## §5 Testable acceptance mapping

| Acceptance（canonical (a)–(d)） | Requirements | Executable check |
|---|---|---|
| (a) 非空 outbox 上 `--priority` 非零退出 + 显式消息，除非 opt-in；harness 腿 opt-in 保持绿 | R1 + R2 | 负例：INSERT status=2 → 无 opt-in 跑 → **`RC == 1`**（钉死 error 语义，非 `!= 0`——testing F1）+ 日志 grep `REFUSED` + `SELECT COUNT(*)`==1；直接调用 drill bin（无 opt-in）→ **`RC == 1`** + `REFUSED` + COUNT==1（FM11）；`AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=true` → **`RC == 1`** + `REFUSED`（FM5 端到端）；放行：带 opt-in → drill exit 0 + `priority: landed`；空 outbox 无 opt-in 也放行（COUNT==0） |
| (b) B1/B2/D 与 t11 腿不变；37-pin 不变 | R2 | `bash scripts/test-b5-pin-guard.sh` 绿 + `B5 contract pin: 37/37`；B1/B2/D 腿零改动即证 |
| (c) drill 断言出站 action ∈ {两拼写}；class='admin' + priority=100 首批成员资格不变 | R3 | 日志含 `drill: moderation-action-vocabulary: PASS` + 既有三 PASS 行 + exit 0；负例（临时改 `MODERATION_ACTION` 出词汇表）→ exit 1；双拼写兼容（R3.4） |
| (d) 无 flag 行为字节级不变；aero-eng 测试无回归 | R4 | `cargo test -p aero-eng --lib --test audit_provision`：**25 既有 + 新增**全绿（修正 canonical「14」；requirements doc 勘误见 §1.4）；变更前后同库 `audit-provision-check` 输出 `diff` 为空；B1/B2/D grep 判词原样 |

**文档化手工复验步骤**（testing FM1/FM9 gap 关闭）：

- **FM1**：临时把 `COUNT_SQL` 改为非法表名（如 `audit_governance_outbox_broken`）→ 无 opt-in 跑 `--priority` → exit 1 + `audit-provision-check:` 错误 + 日志**无** drill spawn 痕迹（无 `reset audit_governance_outbox (TRUNCATE-at-start guard)` 行）；还原后绿。防御纵深：base `run()` 已先失败于坏 psql/DB，此分支实际不可达，但按审计规则文档化。
- **FM9**：`AERO_PRIORITY_DRILL_BIN` 指向 sleep >120s 的 stub 脚本（`#!/bin/sh\nsleep 130`）→ 跑 `--priority` → 日志含 `priority drill timed out after 120s` + exit 1 + 无 TRUNCATE（行存活）；还原后绿。120s 不现实自动化，故为手工步骤（既有行为，非新增）。

## §6 Scope boundaries（与 sibling 划界）

- **不做**：token 翻转/裁决（sibling token-parity R1）；truth-check TOKEN 类别（sibling R4）；0239 DDL/claim 排序/反饥饿 cap；drill 行数结构（不加第二 seed 行——`drain-501`/`parity-501` 计数与 sibling R3.2 钉死）；新 CLI flag。
- **做**：R1 条件门禁（wrapper 快速失败层 + drill 侧 in-tx 权威门） + R2 harness opt-in/四步负例 + R3 词汇表硬编码断言 + R4 回归零改动 + requirements 14→25 勘误注记 + D8′ 修订注记。

## §7 三轮复核发现纳入矩阵（2026-08-08 复核确认）

| 发现 | 裁决 | 落点 |
|---|---|---|
| security F1：门禁只在 wrapper，drill 直接调用即绕过 | **采纳 in-drill 门**（非仅文档化）——直接调用是 drill usage 头（:4-9）文档化的受支持路径，文档化无法关闭绕过 | §1.2a + FM11 + harness 负例 3 |
| security F2：COUNT→TRUNCATE TOCTOU（含冷编译窗口） | **采纳** in-tx `LOCK+COUNT+TRUNCATE` 原子化——窗口关闭而非收窄 | §1.2a + §2「空 outbox 语义」 |
| security F3：REFUSED≡FAIL 码值碰撞 + 消息双打印 | **采纳**（文档行） | §2 exit 码契约 + §1.1 注 |
| security F4：BIN=1 无 hint / 双下划线拼写静默忽略 | **采纳**：REFUSED 消息双下划线主动探测 hint + spawn 失败路径 BIN hint | §1.1 |
| security F5：COUNT_SQL 第三处硬编码表名 | **采纳**（文档行）：漂移响亮失败（psql 错 → FM1），列门 SKIP 保证计数不先于建表 | §1.1 注 |
| security F6：status-2 是唯一真洞 | **采纳**（文档行）：覆盖矩阵 0/1→base、2→门禁、3→base | FM3 |
| security F7：读回 fetch_one 语义 + 协调编辑残余 + relay 不改写 payload | **采纳** | §1.2 |
| security cross-cutting：env 继承（无 `env_clear`） | **采纳**（文档行）：子进程继承同一 env；未来 `env_clear` 必须保留 drill 侧显式 env | §1.1 注 |
| db-reviewer：TOCTOU 残留「文档化于 §2/FM10」 | **被 in-drill 修复取代**——竞态被关闭，文档化为「已关闭的竞态 + 为何 wrapper 探针不足」；纯文档化方案**显式拒绝**（理由：直接调用是受支持路径，非误配；文档化不改变破坏面） | §2 + FM10 + §1.2a |
| db-reviewer：R3 读回 = 种子行重读非 wire 证据 | **采纳** | §1.2 |
| db-reviewer：负例块置于 0239 门内 | **采纳**（结构性已满足，显式化） | §1.3 |
| db-reviewer：无 `lock_timeout` | **显式拒绝（出范围）**：既有无锁超时（TRUNCATE 本身即 ACCESS EXCLUSIVE 无限等待）；加锁超时改变共享库行为，属 FM10 纪律面 | §1.2a 注 |
| testing F1：`RC != 0` → `RC == 1` | **采纳**（两处） | §1.3 步骤 2 + §5(a) |
| testing F4：`=true` 端到端负例 | **采纳为必做**（~1s 成本钉住 `== Ok("1")` 严格性） | §1.3 步骤 4 |
| testing FM1/FM9：文档化手工步骤 | **采纳** | §5 手工复验段落 |
| testing F5：requirements doc 14→25 | **采纳**：requirements doc 四处勘误（E10/R1/R4/§4(d)）+ 设计 §1.4/§4 记注 | requirements doc + §1.4 + §4 步骤 4 |
| testing F6：drill 锚点漂移（:207-212、:216-227） | **采纳**（修正为 :214-223、:227-238） | §1.2/§1.3 |
| §0 勘误（canonical 非 identical、25 测试） | 已在 §0 纳入 | §0 |

**裁决理由（security-vs-database 张力）**：db-reviewer 建议「文档化残留竞态」以省跨 crate 重复——其前提（FM10/§4.3 纪律覆盖 drill 直接调用）不成立：直接调用是**文档化的受支持路径**（drill usage 头），不是误配；security F1 的绕过与 F2 的窗口是同一位置的同一修复（in-tx 门，约 8 行）。跨 crate 重复的漂移方向全 fail-closed（drill 更紧 → wrapper 放行的运行被 drill 拒红，harness 自捉；更松 → wrapper 先拒不可达 / 直接调用不劣于今日零门禁）。故采纳 in-drill 门，wrapper 门保留为快速失败 UX 层；db-reviewer 的「文档化」以关闭后竞态的记述形式保留（§2/FM10）。
