# Requirements Spec — aero-cli B5-3：on-demand moderation-priority verification surface（`audit-provision-check --priority` = 列探测 + 500+1 drill，落家 = `crates/aero-cli` aero-eng 二进制）

- **Module (analysis root)**: `crates/aero-cli` — aero-eng 工程 CLI（`[[bin]] name="aero-eng"`，`Cargo.toml:13`；零 DB 依赖设计，文件头 "engineering commands only (no DB dependencies)"）；本 direction 交付物 = `--priority` 模式（probe + drill）+ `priority: landed/absent` 判定行 + harness 接线
- **Direction**: "Add an on-demand moderation-priority verification surface to the CLI (drill + priority-contract probe) decoupled from the 1800s `gate b5`"（value 7 / risk_reduction 7 / effort 6 / confidence 6）
- **Source analysis**: `docs/auto/analyses/crates-aero-cli-e99ec77a.json`（direction #3）
- **Campaign**: `aero-im-b5-outbox-relay`；gate anchor `docs/campaigns/implementation-gate.md`（:78 G6 "37/37、T-11、moderation 优先级"）
- **Sibling specs（同模块/同批次，命令面协调）**: `2026-08-07-aero-cli-b5-2-relay-probe.req.md`（`network relay-probe` = spawn 样板 + 命名 env override 惯例）、`2026-08-07-aero-cli-b5-4-audit-provision-check.req.md`（`audit-provision-check` 命令本体 = 本 direction 的扩展宿主）、`2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md`（BR1-BR6/A1-A5）、`2026-08-08-aero-audit-connector-b5-3-claim-lane-carriage.design.md`（D7 anti-starvation 界）
- **Status**: Requirements（下述证据全部经源码 grep 核对；**direction 的核心前提「0239/priority 未落地」已过时**——§1 逐条更正，交付物按当前仓态 re-ground，acceptance 原句保留）
- **Verification date**: 2026-08-08。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md` :15「`event_outbox.rs` COLUMNS 无 priority — proposed」 | ⚠️→✅ **半过时**。IM `event_outbox.rs` COLUMNS（`crates/aero-storage/src/event_outbox.rs:21`：id…created_at，无 priority）确实无该列且按设计 C3/R-5 **不应有**；但「no repo evidence of the column」整体已不成立：**`migrations/0239_audit_governance_outbox.sql` 已落地**（`ls migrations/*.sql | wc -l` = 241），含 `priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)`、`class TEXT NOT NULL DEFAULT 'message' CHECK (class IN ('admin','message','room'))`、`delivery_mode TEXT NOT NULL DEFAULT 'push'`，0239 trigger 对 `message.moderated` 打 `class='admin'`/`priority=100`/payload action `'admin.content.flag'`，与 `crates/aero-ai/src/governance.rs` 常量交叉钉死（`GOVERNANCE_PRIORITY_MODERATION=100` :31、`GOVERNANCE_PRIORITY_BACKLOG=10` :33、`GOVERNANCE_CLASS_ADMIN='admin'` :37、`MODERATION_OUTBOUND_ACTION='admin.content.flag'` :56） |
| E2 | `scripts/test-integration.sh:441-460`（moderation-priority drill，0239-gated SKIP） | ✅ 位置漂移到 :446-461，语义保留：`[ -f migrations/0239_audit_governance_outbox.sql ]` 门控（:447）→ 建 throwaway 库 + `cargo run --bin aero-cli -- migrate`（:452-454，aero-server 的 DB CLI）→ `cargo run --quiet -p aero-audit-connector --bin aero-audit-priority-drill`（:456-457）→ `b5_check "moderation-priority-drill" "PASS"`（:459）；文件缺席 → `SKIP (0239 not landed)`（:461）。**direction 的「SKIP-exit-2 when 0239/priority absent」已过时**：0239 文件存在 → 腿现 ACTIVE，且 drill 实跑 exit 0（sibling `2026-08-08-aero-audit-connector-b5-1-0239-landing.req.md` E3 取证：`moderation-in-first-batch: PASS` + `drain-501: PASS` + `parity-501: PASS`）。`scripts/b5-pin.sh:41` 已把 `moderation-priority-drill` 钉入 37-slot |
| E3 | `crates/aero-eng/src/audit_provision.rs` Q2_SQL/Q3_SQL（probe 先例） | ✅ Q2_SQL :42（`to_regclass` 候选探测）、Q3_SQL :48（status buckets）、G0239_CANDIDATES :26、parse_probe_line :215、PsqlRunner :359（psql `-At` 单查询契约 + 本地/容器双模 + per-query timeout）、`run()` :462、判定行前缀 `audit-provision-check: …`（:168/:183）。priority/class 列探测复用同款形态——drill bin 已有 `information_schema.columns` EXISTS 探测实现可镜像（`aero-audit-priority-drill.rs:105-130`） |
| E4 | `crates/aero-cli/src/main.rs` Gate_ 'b5'（:100, 1800s — 唯一 drill 入口） | ✅ `"b5" => b(f("test-integration.sh"), 1800).await` 实际 :190（行漂移）。**当前唯一 drill 入口 = `gate b5` → test-integration.sh**，无按需 CLI 面（全仓 grep `priority` 零命中 main.rs）——本 direction 的缺口本体。扩展样板齐备：`network relay-probe` 臂 :417-476（直 spawn connector bin：`AERO_RELAY_PROBE_BIN` env override + `cargo run` fallback + 退出码契约 0/1/2 + 120s timeout + stdout/stderr inherit）；`AuditProvisionCheck_` :488-506（`_args` 未用，加 flag 需解 `args[2]`）；`Outcome::warning(code)`（outcome.rs:47）经 registry Err 路径 → `main.rs exit(r.exit_code())`（distinct 码穿透；`run_cmd` 会拍平码，必须直 spawn）；`Completion_` cmds 串 :316 |
| E5 | `docs/requirements/2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md`（BR1-BR6 / A1-A5） | ✅ BR1-BR6 :61-82、A1-A5 :101-126。A3 = 「500 backlog + 1 moderation → moderation claimed first」drill；A4 = anti-starvation（PG 侧 K-floor 两臂 cap——现状见 §1.1） |
| E6 | （补充核对）direction 前提「aero-bus/aero-storage have no priority sort」 | ⚠️→✅ **过时**。priority 排序已落地于 `crates/aero-audit-connector/src/pg.rs` `PgOutboxRepo::claim_due`（:93-，`ORDER BY candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id FOR UPDATE SKIP LOCKED LIMIT` :111，B5-3 fold-in），配套 `migrations/0240_audit_governance_due_prio_idx.sql`（`(priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)` 部分索引，与 claim ORDER BY 精确匹配）+ `migrations/0241_governance_reconcile.sql`。aero-storage 的 `audit_governance.rs`（933 行）钉 DDL 契约（priority>0 CHECK :502-511、DEFAULT 10 :449、due index 列序 :558-570）+ 0239 trigger 契约（:871-882 断言 priority 100 / action admin.content.flag） |

### 1.1 Anti-starvation cap 现状（direction acceptance 第三项 re-ground 依据）

- **K-floor 两臂 cap（design §5：`K = limit / 5`，tier1 `priority DESC` + tier2 FIFO）未实现**，且是**已接受+文档化的已知界**：`docs/design/2026-08-08-aero-audit-connector-b5-3-claim-lane-carriage.design.md` F9（:273-277）+ D7（:345）——strict-priority 无 aging/promotion 项时，若 claimable priority-100 due set 每 tick ≥ batch，priority-10 行永不被选；**latent 非 live**（高 lane 单一预算受限 producer ≤~60 finalizes/min vs ~1200 rows/min 名义 claim 容量；低 lane 现零 producer——0239 trigger 只映射 `message.moderated`）。修复 = sibling record 的 D-CAP（与 L1 aggregation 同批落地），**本 direction 不实现 cap**（§3 out-of-scope）。
- **drill 上下文的反饥饿 oracle = `drain-501` + `MAX_ROUNDS=10`**（`aero-audit-priority-drill.rs:250-276`）：501/501 全 settle（status=2），stuck 行（status∈{0,1,3}）计数 0，event_id set-parity 精确。direction acceptance 的「anti-starvation cap 仍 drain backlog」在本仓的测试形式 = drill 全 drain 断言；D-CAP 落地后该断言继续覆盖。

## 2. Verified current state

```
B5-3 已落地（E1/E6）：
  0239 DDL（priority/class/delivery_mode + 值 CHECK + 状态机 0-3 + token-keyed trigger）
  → 0240 priority-DESC 部分索引 → 0241 reconcile（241 迁移）
  claim_due ORDER BY priority DESC（connector pg.rs:111）+ mixed_priority_claim 单测（:530）
  aero_ai::governance 常量（100/10/'admin'/'admin.content.flag'）
  aero-audit-priority-drill bin（301 行）：500 backlog(priority 10, 先入) + 1 moderation
    (priority 100, class admin, action admin.content.flag, 后入 → 排序证据只能来自 priority)
    → batch 100 / concurrency 1 串行 drain → moderation-in-first-batch（top-100 批成员 = D3
    heap-order 契约，非 delivered_at 严格 first）/ drain-501 / parity-501；
    表探测 to_regclass（:87-100）+ 列探测 information_schema.columns（:105-130）缺 → exit 2 SKIP；
    排序缺失 → FAIL 红（诚实 G6 信号）
  harness 腿 ACTIVE（test-integration.sh:446-461）+ b5-pin.sh:41 钉入 37-slot（PASS 态）

缺口（本 direction 关闭，全部 verified）：
  a) 无 aero-eng 命令可在 1800s `gate b5` 之外按需跑 drill —— 唯一入口 = gate b5 → test-integration.sh
  b) 无 `priority: landed/absent` 判定行 —— drill 只印 "seeded…"/"drill: …: PASS"，
     无列存在性 verdict 行（harness 无可 grep 的落地证据）
  c) gate 无法区分「未落地」（合法 SKIP，phase-1 window）与「已落地但排序坏」（drill FAIL）——
     只能整跑 1800s 门
```

**Gaps this direction closes**（all verified）：① 按需 drill 命令面——`audit-provision-check --priority`（R1）；② 列探测 + 判定行——`priority: landed/absent`（R2/R3）；③ distinct 退出码 SKIP(2)/FAIL(1)/PASS(0) 在 gate 之外可用（R1.3）；④ harness 腿换 CLI 面 + 非 vacuous grep（R4）。

## 3. Scope

**In scope**：
- `audit-provision-check --priority` 模式（R1）：probe 步 + drill spawn 步 + distinct 退出码
- 探测 SQL 落 `crates/aero-eng/src/audit_provision.rs`（新常量 + 解析，Q2 同款形态；R2）
- 判定行契约（R3）；harness 接线（R4）；help 文本同步（R5）
- aero-cli `Cargo.toml` **零改动**（aero-eng 无 DB 依赖约束：probe 走 PsqlRunner 子进程、drill 走 spawn，均不链接 connector/aero-ai）

**Out of scope**：
- 0239/0240/0241 DDL、claim SQL、`mixed_priority_claim` 等 storage/connector 已落地物——零改动
- **D-CAP K-floor 反饥饿 cap**（D7 已接受界，sibling record 排期；drill 全 drain 断言已覆盖 drill 语境）
- `aero-audit-priority-drill.rs` bin 本体（已存在，零改动；CLI 只 spawn）
- `network priority-drill` 备选命令（direction 给的二选一，**否决**：`network` 是 DB-free ping/dns/port/relay-probe 命名空间，DB drill 放这里语义误导；`audit-provision-check` 已要求 DATABASE_URL 且其 psql probe 面正是列探测的宿主——§6 记录）
- 新 gate 门（不加 `gate priority-drill`——drill 面由 `gate b5` 覆盖，避免 37-slot 之外的新门面；§R5）

## 4. Requirements

### R1 — `audit-provision-check --priority`（probe + drill 模式）

`AuditProvisionCheck_`（main.rs :488-506）解 `args[2]`：

1. **无 flag**：现状不变（B5-4 语义，`aero_eng::audit_provision::run` 原样）。
2. **`--priority`** 顺序执行：
   - **probe 步**（R2 的 SQL，PsqlRunner 直用）：表探测（`to_regclass`，Q2 同款）+ `priority`/`class` 列探测。打印判定行 `audit-provision-check: priority: landed|absent`（+ `class:` 同款行）。
   - 列/表缺席 → 打印 `audit-provision-check: priority-drill: SKIP (<reason>)` + **`Outcome::warning(2, …)` → 进程 exit 2**——**永不 FAIL**，phase-1 window 保留（direction AC4）。
   - 列在 → **drill 步**：`tokio::process::Command` **直 spawn**（`run_cmd` 拍平码，relay-probe :417-476 同款）：env override `AERO_PRIORITY_DRILL_BIN`（设则直接执行该 bin，对齐 `AERO_RELAY_PROBE_BIN` 惯例），否则 `cargo run --quiet -p aero-audit-connector --bin aero-audit-priority-drill`；stdout/stderr **inherit**（drill 的 `drill: …: PASS` 行须过 harness grep）；120s timeout（relay-probe 同款，超时 kill + `Outcome::error`）；退出码 passthrough：`0 → Outcome::ok`、`1 → Outcome::error`（排序坏/断言败 = FAIL）、`2 → Outcome::warning(2)`（drill 内部探测兜底 SKIP）、其他 → error（异常退出）。
3. DATABASE_URL 缺失 → 现有 error 路径（:501-505）原样。

### R2 — probe SQL（`crates/aero-eng/src/audit_provision.rs`）

Q2 同款形态（固定 literal，非用户输入）：

```rust
/// P — priority/class column probe (B5-3 CLI surface; mirrors the drill
/// bin's runtime capability gate, aero-audit-priority-drill.rs:105-130).
const P_SQL: &str = r"SELECT EXISTS (
    SELECT 1 FROM information_schema.columns
     WHERE table_name = 'audit_governance_outbox' AND column_name = $1
)";
```

- PsqlRunner.query 是单串 psql `-At` 契约（无参数化通道）——**两个固定 literal 探测**（`'priority'`、`'class'`）即可，禁止用户输入进 SQL；解析复用 `parse_psql_bool_line`（:188，t/f cell）。
- 表缺席：Q2 的 `to_regclass` 已覆盖 → `priority: absent` 与现有 `outbox-0239: not migrated` 行并存不冲突（probe 步在 Q2 结果之上叠加）。
- 新函数（如 `probe_priority_contract(runner) -> (bool, bool)`）+ `format_report` 追加两行——**truth-check 不允许零调用**：函数必须被 `run()` 的 `--priority` 路径调用。

### R3 — 判定行契约（greppable）

- `audit-provision-check: priority: landed` / `audit-provision-check: priority: absent`（stdout，harness grep 锚点——direction AC5 原词 `priority: landed/absent`）
- `audit-provision-check: class: landed|absent`（同款）
- SKIP 行：`audit-provision-check: priority-drill: SKIP (<reason>)`（沿用现有 `audit-provision-check: …` 前缀惯例 :168/:183；harness 只 grep `priority: ` 判定行 + 退出码，SKIP 行是诊断）
- 不新增其他前缀（防 harness grep 歧义）。

### R4 — harness 接线（`scripts/test-integration.sh:446-461`）

腿保留：`create_throwaway_database` / migrate（`cargo run --bin aero-cli -- migrate`，aero-server DB CLI）/ `drop_created_database` / `b5_check` verdict 协议 / 0239 文件门控。改动两处：

1. drill run 行（:456-457）：`cargo run --quiet -p aero-audit-connector --bin aero-audit-priority-drill` → `cargo run --quiet --bin aero-eng -- audit-provision-check --priority`（DATABASE_URL env 原样传；migrate 已先行——CLI 不负责迁移，与 audit-provision-check 同契约）。
2. 加非 vacuous 证据：`grep -q "priority: landed"` 于 CLI 输出（缺 → 腿 FAIL，防 CLI 面空转/退化）；drill 输出 `drill: moderation-in-first-batch: PASS` 等 PASS 行继续可见（inherit）。
3. SKIP 分支语义不变：0239 文件缺席 → `b5_check "moderation-priority-drill" "SKIP (0239 not landed)"`（:461 原样）；CLI exit 2 → 同 SKIP 带 reason（R1 已保证 exit 2 永不 FAIL）。

### R5 — 文本同步

- `audit-provision-check` help 文本（main.rs :489-490）追加 `--priority` 说明（"…; --priority = moderation-priority drill (probe + 500-backlog drill)"）。
- `Completion_` cmds 串（:316）：**不动**（flag 不是子命令；cmds 串只列子命令）。
- `Gate_`：**不加新臂**、`gate list` 串不动（drill 面由 `gate b5` 覆盖；本 direction 交付按需面，不新增门面）。

## 5. Testable acceptance mapping（direction acceptance 原句保留，re-ground 到当前仓态）

| AC（原句） | 可测断言（测试形式） | 位置 |
|---|---|---|
| **AC1** 新 CLI 子命令跑 500-backlog+1-moderation drill against throwaway migrated DB | `cargo run --bin aero-eng -- audit-provision-check --priority`（DATABASE_URL=已迁移一次性库）exit 0，stdout 含 `audit-provision-check: priority: landed` + `drill: moderation-in-first-batch: PASS` + `drill: drain-501: PASS` + `drill: parity-501: PASS` | harness 腿 R4 + 手工冒烟（throwaway 库纪律 AGENTS §4.3） |
| **AC2** 0239+priority 落地后 admin.content.flag 行 claimed first | drill bin 断言即 oracle（**零改动**）：moderation 行（class admin / priority 100 / action `admin.content.flag`，后入）∈ 首轮 top-100 claimed 批（D3：批成员 = `priority DESC` ORDER BY 契约；delivered 严格 first 是 executor artifact 不断言）+ 全 drain + set-parity；CLI 层断言 = 退出码 0 透传 | bin :192-240（已存在）；CLI 层 = R1 退出码契约 |
| **AC3** anti-starvation cap 仍 drain backlog | `drain-501` + `MAX_ROUNDS=10`：501/501 status=2、stuck=0、event_id parity（bin :250-276）。K-floor D-CAP 未实现 = D7 已接受界（§1.1）——drill 全 drain 即本仓测试形式；D-CAP 落地后同断言继续覆盖（不扩大 scope 实现 cap） | bin（已存在） |
| **AC4** priority 列缺席 → clear SKIP (exit 2)，never FAIL，phase-1 window 保留 | 缺列库（0239 未迁或列被 drop）跑 `--priority` → exit 2 + `audit-provision-check: priority: absent` + SKIP 行；**断言非 exit 1**。harness 侧 0239 文件缺席 → `SKIP (0239 not landed)` 路径保持（:461） | CLI probe 分支（R1.2）+ harness 门控 |
| **AC5** report line greppable（`priority: landed/absent`） | `grep 'priority: landed'` 命中（R4 非 vacuous 证据）；`priority: absent` 在 SKIP 路径命中 | harness R4.2 |
| **AC6** harness `b5_check 'moderation-priority-drill'` 与 sibling B5-3 slice 锁步 | slot 已钉入 `scripts/b5-pin.sh:41`；CLI 面接入后腿保持 PASS（不回归 SKIP）；`scripts/test-b5-pin-guard.sh` 37/37 绿（direction 原句「flips from SKIP to PASS in lockstep」——该翻转已随 0239+排序落地完成，本 direction 的保真形式 = 接入 CLI 面后 PASS 不回归 + pin guard 绿） | test-integration.sh + b5-pin.sh + test-b5-pin-guard.sh |

## 6. Coordination & hard rules（AGENTS §4）

- **命令面互斥**：`audit-provision-check` 是 B5-4 的命令（sibling req 文档 R2/R3）；本 direction 只加 **`--priority` flag**（追加模式，无 flag 行为不变），不新开 `network priority-drill` 子命令（§3 否决记录）——与 B5-2 的 `network relay-probe` 命名面不重叠。
- **bin 名别混**：`cargo run --bin aero-eng`（crates/aero-cli 包）是工程 CLI；harness migrate 用的 `cargo run --bin aero-cli -- migrate` 是 aero-server 的 DB CLI——两个不同 bin。
- **spawn 必须直 spawn**（`tokio::process::Command`）：`run::run_cmd` 把非零码拍平为 1（B5-4 spec E8 取证），distinct 退出码（SKIP=2 / FAIL=1）依赖直 spawn + `Outcome::warning(code)` 穿透（registry Err 路径 → `main.rs exit(r.exit_code())`）。
- **env override 命名**：`AERO_PRIORITY_DRILL_BIN`（对齐 `AERO_RELAY_PROBE_BIN` 惯例）；cargo-run fallback 只在 override 未设时用。
- **aero-eng 零 DB 依赖**：probe 走 PsqlRunner（psql 子进程，现有先例 :359），drill 走 spawn——不链接 aero-audit-connector/aero-ai/aero-storage。
- **提交前必过**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（不新增警告）· `scripts/truth-check.sh`（新 probe 函数必须有调用，不加 allowlist 条目）· `scripts/{file-size-check,web-check}.sh`。
- **活验证**：全新一次性库（`CREATE DATABASE` 再 `aero-cli migrate`，用完 `DROP DATABASE`；AGENTS §4.3）——CLI 面冒烟与 harness 腿共用同纪律。
- **迁移纪律**：本 direction 不加迁移；0239/0240/0241 已落地，勿改。
