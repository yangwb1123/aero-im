# Requirements Spec — aero-cli AC4 deps-audit gap：aero-cli + aero-audit-connector 入 ALLOWED_DEPS 与 dependency-check.sh（落家 = `crates/aero-cli` aero-eng 二进制）

- **Module (analysis root)**: `crates/aero-cli` — aero-eng 工程 CLI（`[[bin]] name="aero-eng"`，无 DB 依赖设计）；本 direction 交付物 = 两行 `ALLOWED_DEPS` + 两行 `dependency-check.sh`，把 `gate deps-native` 从红转绿并把 aero-audit-connector 纳入依赖审计面
- **Direction**: "Close the AC4 deps-audit gap: admit aero-cli and aero-audit-connector into ALLOWED_DEPS + dependency-check.sh"（value 7 / risk_reduction 7 / effort 1 / confidence 10）
- **Source analysis**: `docs/auto/analyses/crates-aero-cli-e99ec77a.json`（direction #2）
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml`）
- **Sibling spec（同模块并行切片，本 direction 只做 R4 子集）**: `docs/requirements/2026-08-07-aero-cli-b5-2-relay-probe.req.md`（§R4 的 deps 修复面 = 本 direction 的**全部**；B5-2 的 probe/relay-mock/CI 条目不属于本 direction）
- **Status**: Requirements（下述证据全部经源码 grep + **实跑**核对；`gate deps-native` 现状 = 3 violations，非 direction 所述的 1）
- **Verification date**: 2026-08-08。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-eng/src/checks.rs:195` `ALLOWED_DEPS` 不含 aero-cli / aero-audit-connector | ✅ **Verified**。const 起于 :195，共 16 条目（末条 `("aero-eng", &["aero-common", "serde", "tokio"])`）；`grep aero-cli\|aero-audit-connector` 零命中 |
| E2 | `checks.rs:326` "unknown crate" 分支 | ✅ **Verified**。:326 `let allowed = ALLOWED_DEPS.iter().find(|(n, _)| *n == member);` → :328-330 `None` 分支 push `"{member}: unknown crate (not in architecture rules)"` 并 `continue`。**关键机制**：该分支按「成员不在 ALLOWED_DEPS 且**有 ≥1 个 workspace 内部依赖**」触发，且对每个命中 dep **各 push 一条**（无 per-member 早退）——`workspace_crates`（:286）= ALLOWED_DEPS 键集合，故成员自身的每个 `aero-*` dep 都单独撞一次 |
| E3 | `crates/aero-cli/Cargo.toml` 依赖 aero-eng | ✅ **Verified**。`[dependencies]`：`aero-eng.workspace = true`（+ tokio/serde_json/async-trait 外部依赖，不入审计面）；`[[bin]] name = "aero-eng"` |
| E4 | `scripts/dependency-check.sh` 无 aero-cli / aero-audit-connector 条目 | ✅ **Verified**。全文件 82 行：`check_deps` 序列覆盖 common 检查 + bus/storage/auth/signaling/live-*/im-core/im-call/ai/push 14 行（:49-62）+ 反向依赖段；`grep aero-cli\|aero-audit-connector` 零命中（aero-eng、aero-server 同样无正向条目——存量现状，不在本 direction 修）。**现状 exit 0**（实跑：`结果: 0 依赖违规`）——两 crate 目前整面漏检 |
| E5 | root `Cargo.toml:29,32` 两 crate 已是 workspace members | ✅ **Verified**。members :29 `"crates/aero-audit-connector"`、:32 `"crates/aero-cli"`（共 19 成员）；`[workspace.dependencies]` :77 `aero-audit-connector = { path = ... }`。**工作树状态**：`git status` = root `Cargo.toml` `M`（+成员 +workspace dep 两行）、`crates/aero-audit-connector/` `??`（未提交）——connector 入 workspace 是**在途切片**，本 direction 的修复须与该成员态一并提交 |
| E6 | `docs/requirements/2026-08-07-aero-cli-b5-2-relay-probe.req.md` §R4（:108-111）与 E9（:28，含记录的 gate 输出） | ✅ **Verified**。E9 记录 2026-08-07 实跑 `{"checked": 19, "violations": 1, "details": ["aero-cli: unknown crate …"]}`；§R4 提议 `("aero-cli", &["aero-eng"])` + `("aero-audit-connector", &[])` 与 `check_deps "aero-cli" "aero-eng"` + `check_deps "aero-audit-connector" ""`。**该记录已过期**——当时 connector 尚无内部依赖；见 E7/E8 |
| E7 | **实跑取证（本 direction 的关键更正）**：`gate deps-native` 当前 = **3 violations，非 1** | ✅ **实跑**（2026-08-08，工作树现状）：`cargo run --quiet -p aero-cli -- gate deps-native` → `{"checked": 19, "violations": 3, "details": ["aero-audit-connector: unknown crate (not in architecture rules)", "aero-audit-connector: unknown crate (not in architecture rules)", "aero-cli: unknown crate (not in architecture rules)"]}`。connector ×2 = 其两个内部依赖（E8）各撞一次 E2 分支；cli ×1 = aero-eng。**方向陈述 "(currently 1)" 与「connector 零内部依赖、静默漏检」均过期** |
| E8 | **connector 实际内部依赖 = `aero-common` + `aero-auth`（非零）** | ✅ **Verified**。`crates/aero-audit-connector/Cargo.toml` `[dependencies]`：`aero-common.workspace = true`、`aero-auth.workspace = true`，且源码真实使用：`src/fake.rs:22`/`src/outbox.rs:26`/`src/pg.rs:24` `use aero_common::{AuditId, OutboxStatus}`；`src/client.rs:16` `use aero_auth::{JwksKeyProvider, KeyProvider}`。**因此正确白名单 = `["aero-common", "aero-auth"]`，不是 §R4 的 `&[]`**；`check_deps "aero-audit-connector" ""` 空名单在 shell 面也会当场失败（脚本提取到 aero-common/aero-auth 不在排除集 → 违规） |
| E9 | `gate deps`（dependency-check.sh）现状 | ✅ **实跑**：exit 0 / 0 违规——因为两 crate 整面漏检（E4）。修复后须**仍** exit 0（两条新 `check_deps` 行各自 `✓ OK`） |
| E10 | checks.rs 单测不钉 ALLOWED_DEPS 内容 | ✅ **Verified**。`#[cfg(test)]` 全部用 tempdir fixture workspace（`parse_workspace_members_*`/`check_deps_ok_on_leaf_crate`/`check_deps_rejects_illegal_upward_dep` 等），无一断言 ALLOWED_DEPS 常量本身 → 加条目零测试 churn |
| E11 | AC4 引文出处 | ✅ **Verified（citation 更正）**。"Cargo.toml audit via `Gate_` deps check" 出自 **B5-2** 文档 §AC4（:158）与 §R4（:108）——不是 direction 所引的 B5-4 文档（`2026-08-07-aero-cli-b5-4-audit-provision-check.req.md` 的 AC4 是「37/37 契约清单钉入 gate 脚本」，:144，另一回事） |
| E12 | `gate b5` deps leg | ✅ **Verified（现状为空）**。`Gate_` `"b5" => b(f("test-integration.sh"), 1800)`（main.rs:190）；`scripts/test-integration.sh` 全文件 `grep deps\|dependency` **零命中**——目前**无** deps leg。B5-2 §R5 命名条目步骤 6（`gate deps-native` 与 `gate deps` 断言 exit 0）是 deps leg 的**计划落点**（sibling direction，未落地）；本 direction 不加 leg（见 §7） |
| E13 | `skills/clean-architecture.md`（checks.rs 注释自称 ALLOWED_DEPS 的生成源） | ✅ **Verified（no-touch）**。92 行，无 aero-cli/aero-audit-connector 条目，且本身已漂移（无 aero-eng、无 aero-server 组合行、im-call 依赖列不全）——B5-2 §R4 明示「只加两行 ALLOWED_DEPS + 两行 dependency-check.sh，不碰其他条目」，文档同步明确出范围 |

### 1.1 修复面机制核对（改哪、改完什么状态）

| 面 | 现状（实跑） | 修复后预期 | 验证命令 |
|---|---|---|---|
| `aero_eng::checks::check_deps`（checks.rs） | 3 violations（connector×2 + cli×1），`checked: 19` | 0 violations，`checked: 19`（成员数不变，仅白名单补齐） | `cargo run -p aero-cli -- gate deps-native`（JSON `"violations": 0` + exit 0） |
| `scripts/dependency-check.sh` | exit 0，但两 crate 漏检 | exit 0 且新增两行各自 `✓ OK` | `bash scripts/dependency-check.sh; echo $?` + `cargo run -p aero-cli -- gate deps` |
| `gate b5` deps 断言（B5-2 §R5 step 6，未落地） | 无 leg | 落地后 `gate deps-native`/`gate deps` 双绿即通过——本 direction 保证其前置条件 | 本 direction 不建 leg（§7） |

## 2. Verified current state

```
a) ALLOWED_DEPS（checks.rs:195） 16 条目，无 aero-cli / aero-audit-connector
b) check_deps 机制（:326）       成员不在白名单 + 有 ≥1 个 workspace 内部 dep → 每 dep 一条
                                 "unknown crate" 违规；workspace_crates = 白名单键集（:286）
c) 实跑 gate deps-native         3 violations：aero-audit-connector ×2（aero-common、aero-auth
                                 各一）+ aero-cli ×1（aero-eng）；checked 19
d) connector 内部依赖            aero-common + aero-auth（源码真实 use；E8）——非「零内部依赖」
e) dependency-check.sh           14 条 check_deps（:49-62）+ 反向依赖段；两 crate 漏检；现状 exit 0
f) 工作树                        connector 未提交（??）+ root Cargo.toml 已 M——修复与成员态同批提交
g) gate b5 / test-integration.sh 无 deps leg（E12）；B5-2 §R5 step 6 为计划落点
```

**Gap this direction closes**（all verified）：`gate deps-native` 红（AC4 "Cargo.toml audit via `Gate_` deps check" 要求的门）→ 两行白名单修复；connector 从「漏检」变「显式入审计」（其真实依赖 aero-common/aero-auth 进白名单）；shell 依赖检查面同步两行。

## 3. Scope

**In scope（effort 1 的最小切片，与 B5-2 §R4 完全一致）**：
- `crates/aero-eng/src/checks.rs`：`ALLOWED_DEPS` += **`("aero-cli", &["aero-eng"])`** 与 **`("aero-audit-connector", &["aero-common", "aero-auth"])`**（§R1）。
- `scripts/dependency-check.sh`：+= **`check_deps "aero-cli" "aero-eng"`** 与 **`check_deps "aero-audit-connector" "aero-common,aero-auth"`** 行（§R2）。

**Out of scope（并行切片 / 其他模块——勿在本 direction 建造）**：
- **B5-2 的 probe bin / `network relay-probe` / `scripts/relay-mock.sh` / `gate relay-mock` / test-integration 命名条目**：sibling spec 的 R1/R2/R3/R5 主体，本 direction 只消费其 R4 面。
- **`scripts/test-integration.sh` 加 deps leg**：B5-2 §R5 step 6 已计划（grep 断言 `gate deps-native`/`gate deps` exit 0）；本 direction 不加 leg、不加命名条目（§7 决策）。
- **connector `Cargo.toml` / `src/`、aero-cli `Cargo.toml` / `main.rs`、root `Cargo.toml`**：零改动（成员态与 workspace dep 是既有在途修改，不归本 direction）。
- **`skills/clean-architecture.md` / 任何文档同步**：存量已漂移（E13），B5-2 §R4 明示不碰；如需同步属独立 doc 切片。
- **dependency-check.sh 既有条目修正**（如 aero-im-call 白名单缺 aero-bus、aero-live-webrtc 缺 aero-live-hls，与 ALLOWED_DEPS 的存量漂移）：非本 direction，碰了即扩大 scope。

## 4. Requirements

### R1 — `ALLOWED_DEPS` 补两条目（checks.rs:195 常量，修 `gate deps-native` 红）

`crates/aero-eng/src/checks.rs` `const ALLOWED_DEPS` 追加（位置不限，建议跟在 `("aero-eng", …)` 后）：

```rust
("aero-cli", &["aero-eng"]),            // engineering CLI framework consumer
("aero-audit-connector", &["aero-common", "aero-auth"]), // audit relay connector
```

- `aero-cli` 的 workspace 内部依赖仅 `aero-eng`（E3）；`aero-audit-connector` 的仅 `aero-common` + `aero-auth`（E8）——**白名单必须与各 crate 当时的实际 `aero-*` 依赖逐一相等**（实现时若 connector Cargo.toml 已变，以实跑 `gate deps-native` 的 violations 明细反推补全，见 §7）。
- 不碰其他 16 条目；`ROOT_CRATE`/`workspace_crates` 机制不动。
- 效果（E2 机制）：connector 与 cli 均命中白名单 → 3 条 violations 全部消失；`checked` 保持 19。

### R2 — `dependency-check.sh` 补两行（shell 依赖方向面同步）

`scripts/dependency-check.sh` 的 `check_deps` 序列（aero-ai 行之后、反向依赖段之前）追加：

```bash
check_deps "aero-cli" "aero-eng" || violations=$((violations + 1))
check_deps "aero-audit-connector" "aero-common,aero-auth" || violations=$((violations + 1))
```

- **不得用空名单** `check_deps "aero-audit-connector" ""`：脚本 `sed -nE` 提取规则（:38-42）会提取 `aero-common`/`aero-auth` 且不在排除集 → 当场违规（B5-2 §R4 的 `""` 提议基于「零内部依赖」的过期前提，E6/E8）。
- 两行各自 `✓ OK` 且脚本整体 exit 0（现状 0 违规保持，E9）。

### R3 — 约束

- **零 Cargo.toml / Cargo.lock 改动**（含 connector、aero-cli、root）：本 direction 只改一个 `.rs` 常量 + 一个 `.sh`；「connector 零新增依赖」由 `git diff --stat Cargo.lock crates/*/Cargo.toml` 为空钉死。
- **零测试 churn**：checks.rs 单测 fixture 化（E10），无需新增/修改测试；不新增 bin/脚本（无 file-size 阈值风险）。
- **不触碰**：migrations/、aero-ai、aero-server 生产代码、v1 `snaplink_commercial/`、B5-1 的 0239、B5-2 的 probe/relay-mock 命名、B5-4 的 audit-provision-check 命名、`skills/clean-architecture.md`。
- 提交前门禁：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（零新增警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· §5 的 AC1-AC5 全绿 · no-touch 守卫（`git diff --stat` 仅 `crates/aero-eng/src/checks.rs` + `scripts/dependency-check.sh` 两个文件，外加在途的 connector 成员态）。

## 5. Acceptance checks（direction 原样保留，逐条 testable；过期括号以实跑更正）

> direction acceptance 五条逐条保留；其中 "(currently 1)" 与 "aero-audit-connector→(none)" 两条括号**已被实跑证伪**（E7/E8），按现状更正为 3 violations 与 →{aero-common, aero-auth}。全部机器断言 = 下面五条命令/断言，无人工判定项。

### AC1 — `cargo run -p aero-cli -- gate deps-native` exits 0 with 0 violations
- 实跑断言：`cargo run --quiet -p aero-cli -- gate deps-native` → exit 0，stdout JSON `"checked": 19, "violations": 0`，`details` 数组为空（或缺省）。
- 基线对照（已取证）：修复前 = 3 violations（`aero-audit-connector` ×2 + `aero-cli` ×1，E7）——**非 direction 所述的 1**；`grep -c "unknown crate"` 输出 = 0。
- 防 vacuous：`details` 中不得含 `aero-cli` / `aero-audit-connector` 字样；`checked` 必须为 19（成员数不变，防止「漏检」伪装成「转绿」）。

### AC2 — `scripts/dependency-check.sh` passes with edges aero-cli→aero-eng and aero-audit-connector→{aero-common, aero-auth} checked
- 实跑断言：`bash scripts/dependency-check.sh` → exit 0，`结果: 0 依赖违规`；输出含两行 `✓ OK` 对应新条目（`grep -E "check_deps \"aero-(cli|audit-connector)\""` 源码命中 + 运行输出无 ❌）。
- **edge 更正**：direction 原文 "aero-audit-connector→(none)" 过期——connector 真实内部依赖 = aero-common + aero-auth（E8，源码 use 取证），空名单写法会当场失败；断言以 `check_deps "aero-audit-connector" "aero-common,aero-auth"` 为规范。
- 等价门：`cargo run --quiet -p aero-cli -- gate deps` → exit 0。

### AC3 — `gate b5` deps leg green
- 现状核证：`scripts/test-integration.sh` 当前**无 deps leg**（E12，全文件 grep 零命中）；B5-2 §R5 step 6（`gate deps-native` 与 `gate deps` 断言 exit 0）是计划落点，属 sibling direction。
- 本 direction 的 testable 读法：**deps leg 落地后其两条断言所依赖的门即绿**——即 AC1 + AC2 双绿（leg 本身由 B5-2 建，本 direction 不建、不加条目，防 scope 膨胀）。若集成时 B5-2 条目已落地：跑 `cargo run --quiet -p aero-cli -- gate b5` 时对应 leg 不得 FAIL（以 leg 内 `gate deps-native`/`gate deps` 的 exit 0 断言为准，无需跑全 harness 1800s）。

### AC4 — no new `cargo clippy --workspace --all-targets` warnings
- 实跑断言：`cargo clippy --workspace --all-targets 2>&1 | grep -c "^warning"` 相对基线（修复前同命令计数）**不增加**——改动为 1 个 const 数组 + 1 个 shell 脚本，无新代码路径，预期 0 新增。
- 附带：`cargo check --workspace` 干净、`cargo test --workspace --lib` 全绿（checks.rs 单测 fixture 化不受影响，E10）。

### AC5 — connector still builds with zero added dependencies
- 实跑断言：`cargo check -p aero-audit-connector --all-targets` → exit 0（**已实跑验证**：2026-08-08 `Finished dev profile`，含全部 3 个 drill bin 的 `--bins` 目标；drill bin 本体需 DB，不做运行冒烟）。
- 零新增依赖断言：`git diff --stat Cargo.lock crates/aero-audit-connector/Cargo.toml crates/aero-cli/Cargo.toml` → **空**（本 direction 不 touch 任何依赖声明；connector 的 17 个外部依赖 + aero-common/aero-auth 内部依赖维持现状）。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| 原生依赖审计门（AC1/AC4 主体） | `crates/aero-eng/src/checks.rs`（ALLOWED_DEPS +2 行） | `cargo run -p aero-cli -- gate deps-native`（JSON 断言）+ 既有 fixture 单测（零 churn，E10） |
| shell 依赖方向面（AC2） | `scripts/dependency-check.sh`（check_deps +2 行） | `bash scripts/dependency-check.sh` / `cargo run -p aero-cli -- gate deps` |
| CI 门（AC3 前置） | B5-2 §R5 step 6 命名条目（sibling，未落地） | `gate b5`（本 direction 只保证其断言前置门绿） |
| 零依赖面（AC5） | — | `cargo check -p aero-audit-connector --all-targets` + `git diff --stat` 空断言 |

## 7. Risks / 决策点

- **direction 前提两处过期，已实跑更正**：① violations 计数 1 → **3**（connector 因 aero-common/aero-auth 各撞一次 unknown-crate 分支，E7）；② connector「零内部依赖」→ **aero-common + aero-auth**（源码 use 取证，E8）。修复条目随之从 `&[]`/`""` 改为 `["aero-common", "aero-auth"]`/`"aero-common,aero-auth"`——**这是本 direction 与 B5-2 §R4 文本的唯一分歧点**，集成时以本 spec 为准（B5-2 §R4 的 `&[]` 若照抄落地会当场红）。
- **实现时白名单须与实际依赖逐一相等**：connector 是未提交在途 crate（E5），若实现前其 `Cargo.toml` 增删内部依赖，以 `gate deps-native` 的 violations 明细反推补全白名单（机制 = 每内部 dep 一条违规，明细即完整依赖清单，天然自证）。
- **`gate b5` deps leg 的归属**：test-integration.sh 无 leg 是现状（E12）；本 direction 若顺手加 leg 即侵入 B5-2 §R5 的命名条目区（多 agent 共享文件纪律，AGENTS.md §4.1）——故 AC3 以「leg 的两条断言所依赖的门绿」为 testable 读法，leg 本体留给 sibling。
- **dependency-check.sh 存量漂移不修**：aero-im-call 白名单缺 aero-bus、aero-live-webrtc 缺 aero-live-hls 等与 ALLOWED_DEPS 的差异是既有债（脚本现状 exit 0 掩盖），碰了即扩大 scope——本 direction 只加自己的两行。
- **文档漂移不修**：`skills/clean-architecture.md` 无两 crate 条目且整体已漂移（E13），B5-2 §R4 明示不碰；如需同步属独立 doc 切片。
- **effort 1 的确定性**：改动 = 1 常量数组 + 1 shell 脚本共 4 行；无新依赖、无新 bin、无新测试、无 CI 文件改动；风险集中在「照抄 §R4 的 `&[]`」——本 spec §R1/R2 已给出修正后的规范条目。

## 8. Sequencing

1. **checks.rs**：`ALLOWED_DEPS` +2 行（R1）→ `cargo run --quiet -p aero-cli -- gate deps-native` 实跑断言 `"violations": 0`（AC1）。
2. **dependency-check.sh**：+2 行（R2）→ `bash scripts/dependency-check.sh` + `gate deps` 实跑断言 exit 0（AC2）。
3. **门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（零新增警告，AC4）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· AC5 双断言（`cargo check -p aero-audit-connector --all-targets` + `git diff --stat` 空）· no-touch 守卫（`git diff --stat` 仅 checks.rs + dependency-check.sh，外加在途 connector 成员态）。
