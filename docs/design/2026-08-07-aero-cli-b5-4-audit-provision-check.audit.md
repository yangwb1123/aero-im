# Audit — AC1-AC4 acceptance mapping：coverage gaps（drills 落地前）

- **Audited artifact**: `docs/design/2026-08-07-aero-cli-b5-4-audit-provision-check.design.md`（§6 Testable acceptance mapping）
- **Method**: 逐条对源码复核（config.rs / oidc.rs / jsonwebtoken vendored / test-integration.sh / Cargo.toml）+ 两条实证实验（http.server buffering、固定端口占用）
- **Date**: 2026-08-07。行号为复核锚点，会漂移——**文件/符号**为准。
- **Verdict**: mapping 骨架成立，但 **13 处必须修**（4 处是设计缺陷而非 drill 缺口：G4 iat/jti 假绿、G5 whitelist 违反自证不变量、G6 D2「完整镜像」不实、G14 自检不锁条目成员资格），**3 处建议**，exit-6 可测性（G16）**验证通过**。

---

## 1. AC1 负向断言与边界/边缘缺口

### G1（必须）— 无 drill 断言 token 不泄漏到输出
矩阵全部是正向 grep（`connector`/`boot gate`/`audit:event:write`/`unreachable`/`HTTPS`），零负向断言。要求：**每个失败 drill（①-⑥、⑥b、③ 各变体、④）与编译产物穿透复跑**，均断言 `! grep -Fq "$TOKEN" <stdout+stderr 合并>`。要点：
- aero-eng 路径会 `eprintln!` 脚本 stderr（`Outcome::error/warning` 消息回显）——同一断言覆盖二进制泄漏面；
- token 内嵌唯一随机 nonce（如 payload 塞 UUID），防 `grep -F` 巧合命中；
- 脚本内 `set -x` 一票否决（会印 env）——脚本头注释明示。

### G2（必须）— 无 P2 drill（python3 缺失）
新增：以 **PATH 剔除 python3 所在目录**（`PDIR=$(dirname "$(command -v python3)")` 后从 PATH 逐段剔除，勿假设 /usr/bin）运行真仓脚本 → `exit 1` + grep `python3`。P2 先于 P3-P5 触发，确定性成立。

### G3（必须）— nbf/exp 边界 drill；禁造竞态用例
实证（jsonwebtoken 9.3.1 `validation.rs:272-282`，connector `leeway=60` @ oidc.rs:293）：
- connector 拒绝条件 = `exp < now-60` / `nbf > now+60`——**connector 接受 exp∈[now-60,now]、nbf∈[now,now+60]**；
- 设计 P6 的 `exp <= now → 4`、`nbf > now → 4` 是**无 leeway 的严格版**（比 boot 更红，fail-closed 方向正确，但 D4「镜像」措辞不实——应改注「strict-no-leeway 近似，刻意严于 connector」）。

确定性边界 drill 集（mint 时间 T0，检查时间 T1 ≥ T0）：
| 用例 | 期望 | 确定性 |
|---|---|---|
| `exp = T0`（==now） | 4 | 恒成立（T1 ≥ T0 ⇒ exp ≤ now） |
| `nbf = T0`（==now） | 0（该步） | 恒成立（nbf ≤ now） |
| `nbf = T0+3600` | 4 | 恒成立 |
| `exp = T0+3600` | 0（并入 ⑤） | 恒成立 |

**禁造** `exp = T0+1` / `nbf = T0+1`（检查可能在 ±1s 内完成 → 竞态）。

### G4（必须，设计缺陷）— code-4 假绿：iat-future / jti / identity-component 未镜像
`validate_client_credentials_token` 额外拒绝（脚本全未镜像）：
- `iat > now + 60` → Err（oidc.rs:453，显式检查）；脚本只查 iat 存在 → **iat=now+3600 的 token 脚本报 0、connector 拒绝 = 假绿**（正是 D2 声称关闭的 FM6 类）；
- `jti` 空/超 1024/含控制字符 → Err（oidc.rs:455-459）；脚本不查 → 空 jti 假绿；
- `sub`/`client_id` 组件合法性（非空/trim/无控制字符，oidc.rs:462-469）→ 脚本只查 `sub == client_id`。

修：`iat > now` → 4（与无 leeway exp/nbf 自洽）；`jti` 非空无控制字符 → 4；sub/client_id 组件校验 → 4。新增 drill：`iat=now+3600` → 4、`jti=""` → 4。（`alg ∉ {RS256, EdDSA}` → 4 为可选结构检查，不越 D9 签名 seam。）

### G5（必须，设计缺陷）— whitelist 边缘必须 exit 1：删掉 D5 白名单
证据（config.rs:53-64）：connector stray 扫描**无任何白名单**，endpoint 缺席时数**所有** `AERO_AUDIT_*`；且扫描只在 endpoint 缺席分支执行——endpoint 在场时扫描根本不跑。因此：
- D5 白名单的理由（「check 自己的 env 把自己判成 config error」）**不成立**——token 只在 endpoint 在场时被消费，彼时无 stray 扫描，不存在自判；
- 白名单实际效果 = env {token, 无 endpoint} 报 3（"not provisioned"），而 server 同 env 会 boot Err → **违反设计自证核心不变量**（「凡 `RelayConfig::from_env` 会 Err 的 env 面，脚本必须报 1」）；
- 附带：白名单使 C5 违规（token 被全局 export）从「被检出（1）」退化为「被掩盖（3）」。

修：**P4 = 任一 `AERO_AUDIT_*`（含 `AERO_AUDIT_PROVISION_CHECK_TOKEN`、`AERO_AUDIT_ALLOW_INSECURE_LOOPBACK`）而无 endpoint → exit 1**；P5（exit 3）仅当零个 `AERO_AUDIT_*` 变量。新增 drill：token + 无 endpoint → 1（grep `config`）；同规则覆盖 allow-insecure + 无 endpoint。

### G6（必须，D2 过声称）— 「完整 boot 配置错镜像」不实：7 个必填 identity/secret 变量未镜像
endpoint 在场时 connector 还必填（脚本零覆盖）：`AERO_AUDIT_RESOURCE`/`CLIENT_ID`/`CLIENT_SECRET`（≥32B 非控制字符，config.rs:175-184）`/EXPECTED_ISS`/`EXPECTED_AUD`/`EXPECTED_SUB`/`SOURCE_SYSTEM`（config.rs:84-96），另有 5 个 duration/range 解析 + 2 个不变量（lease/drain）。**缺 `AERO_AUDIT_CLIENT_ID` 且其余全绿 → 脚本 0、boot Err = 假绿**。
修：
1. 至少镜像 7 个必填存在性 + secret ≥32（python3 `os.environ`，~15 行）；
2. duration range + lease/drain 不变量镜像标 [PROPOSED] 残留（或一并做——纯整数比较 + 两条算术不变量，~20 行）；
3. **AC1-⑤ 全绿 env 必须含全部 7 个且值合规**（boot-faithful），无论脚本镜像到哪一层；
4. 新增 drill ⑥c：endpoint 在场、`AERO_AUDIT_CLIENT_ID` 缺席 → 1。

---

## 2. fake-tree drills（exit 2 / exit 6）

### G7（必须）— drill ① 短路：workspace.dependencies grep 臂从未被执行
设计 ① = 「crates/ 无 connector + Cargo.toml 无成员项」——目录检查先失败，members/deps 两个 grep 臂**永远跑不到**。重构为 fake-tree builder 四臂：
| 臂 | tree 构造 | 期望 |
|---|---|---|
| ①a | `crates/aero-audit-connector/` 缺席 | 2（目录臂） |
| ①b | 目录在 + members 段含条目 + **deps 段缺条目** | 2（**deps grep 臂**） |
| ①c | 目录在 + members 段缺条目 + deps 段含条目 | 2（members grep 臂） |
| ①d | 两段均含 | 通过（复用为 exit-6 绿树） |

要点：fake Cargo.toml 必须同时含 `[workspace]` members 段与 `[workspace.dependencies]` 段（真仓 Cargo.toml:29/:77）；脚本 grep 必须**段作用域**（awk 按 section 头切），全局 grep 无法区分——引号路径串在两段都出现。

### G8（必须）— 模式隔离要 4 格矩阵；exit-6 树需要活 listener
设计只显式覆盖 2 格（markerless 树：`--gate`=6 / 非 `--gate`=0）；「清单落位后本仓直跑 → 0」与 drill ⑤ 的 0 是隐式覆盖第 3、4 格，且依赖落地后状态。补：
- **marker 在场 fake tree**（fake `scripts/test-integration.sh` 含 `B5_CONTRACT_TEST_LIST=(placeholder)`）→ `--gate`=0 且非 `--gate`=0——4 格自洽、与真仓清单状态解耦；
- 「P3-P7 全绿 env 配齐」措辞修正：**P7 dial 的是活 socket，不是 env**——exit-6 drill 必须起 http.server（复用 ⑤ 的 harness 进程）。

---

## 3. drill ⑤ 抖动面（已实证）

### G9（必须）— `python3 -m http.server` 块缓冲吞端口行
实证：`python3 -m http.server 0 --bind 127.0.0.1` 输出重定向后被杀 → **零输出**（管道块缓冲未 flush）；`python3 -u` → 端口行立即出现（`Serving HTTP on 127.0.0.1 port 36819 ...`）。修：**必须 `python3 -u`**，从输出行解析 `port (\d+)`。

### G10（必须）— 固定端口不可用
实证：本环境 `python3 -m http.server 8123` → `OSError: [Errno 98] Address already in use`。修：**port 0 + 解析打印端口**（同时消灭 TOCTOU 与碰撞）。

### G11（建议）— drill ④ 关闭端口确定性化
bind 一个 socket **保持不 listen**（不 close），dial 必得 connection refused——比 bind+close 的窗口期干净。

### G12（必须）— 启动竞态 vs 2s dial 超时
起服后先 wait-for-port（有界轮询 ~10s，python connect 探测），超时 kill + fail；`trap 'kill $SRV_PID' EXIT` 防断言失败孤儿进程拖 CI。

---

## 4. 37 槽清单自检与 A3 pin 原子性

### G13（注记，不改）— 「37」自指
implementation-gate.md:63 的「37/37」在 B5-1 行（「30 个忽略测试 CI 全绿（37/37）」）；仓外契约清单不可核对（proposal :13 明标）。自检（非空 / 恰 37 / 非-[PROPOSED] 可解析 / [PROPOSED] 尾置）钉的是**本设计自洽的 35+2 构成**——作为本地契约成立，[PROPOSED] 尾是诚实 seam，维持现状。

### G14（必须）— 自检不锁条目成员资格 → 原子性缺半环
「恰 37 + 尾置 [PROPOSED]」**无法检出**「audit-provision-check exit-code matrix 条目被删 + 补一个 [PROPOSED]」——计数仍 37、仍尾置。本 direction 的 drill 就是该条目，必须**显式成员断言**：`"audit-provision-check exit-code matrix" ∈ B5_CONTRACT_TEST_LIST`。反向（条目在、执行段缺）由 G15 的可解析规则兜底。step 4 三件套（清单替换 :265-268 + 条目 + B5 CI 接线）已同一步——补一句「单 commit」显式化。

### G15（必须）— 解析语义二义 + 放置决策
- 「每非-[PROPOSED] 项有可执行解析段」二义：文本存在 vs 运行通过。若运行通过，**当前阶段必红**——B5-1 槽（1-30）在 0239 文件缺席时 SKIP（test-integration.sh:226-242）。钉死：**解析 = `run_b5_slot` dispatcher 中每名有非空 case 臂（文本级）**，运行时 skip 是段自身职责；
- 放置：pin 在 `if [ -z "$SKIP_DB_CREATE" ]` 分支内（:197-276）——SKIP_DB_CREATE 下自检与无 DB drill 都不跑，须显式决策（建议把 drill + 自检 hoist 出分支或明示排除）；drill 段还必须**在 0239 文件门之外**（相邻 B5-1/A3 块均被门控），否则当前阶段不执行，「无 DB 不受影响」落空。

---

## 5. exit-6 可测性（G16，验证通过）

D8 成立，无需 test-only override：
- **当前态**：`B5_CONTRACT_TEST_LIST` 字面量全仓零命中（grep 实证，仅 docs 有）→ 脚本落地（step 1）后真仓 `--gate` 直出 6（设计 §5 step 1 已用它当 checkpoint）；
- **落地后**：fake-tree markerless 变体可用——P1 root 自解析（`BASH_SOURCE` 定位）⇒ 脚本拷入 `$TREE/scripts/` 即 ROOT=$TREE；P8 读 `$TREE/scripts/test-integration.sh`（fake 文件须存在）。拷贝即当前版，无漂移；
- 前提约束收敛到 G7/G8：fake 树需双段 Cargo.toml（P3）+ 活 listener（P7）。
- 可选补强：markerless 树 + 坏 token → 期望 4 而非 6（钉 P6 先于 P8 的短路序）。

---

## 6. 行动清单（按 landing order 并入设计 §5/§6）

| # | 变更 | 类型 |
|---|---|---|
| 1 | P4 删白名单（G5）+ 镜像 7 必填 identity/secret + secret≥32（G6）+ iat/jti/组件校验（G4） | 设计修改（脚本 §2.1） |
| 2 | D4 措辞改 strict-no-leeway 近似（G3）；D2 改「部分镜像 + [PROPOSED] 残留」（G6） | 设计文档 |
| 3 | AC1 矩阵增：token 缺席负向 grep（G1）、P2（G2）、边界四例（G3）、iat/jti（G4）、whitelist 边缘（G5）、⑥c（G6） | drill 矩阵 |
| 4 | fake-tree builder 四臂（G7）+ 4 格模式隔离（G8）+ -u/port0/wait-port/trap（G9-G12） | drill 实现 |
| 5 | 自检增成员断言（G14）+ `run_b5_slot` 文本级解析（G15）+ 放置决策 | test-integration.sh |
| 6 | step 4 显式单 commit（G14）；exit-6 段并入 G7/G8 约束（G16 已验证） | landing 序 |
