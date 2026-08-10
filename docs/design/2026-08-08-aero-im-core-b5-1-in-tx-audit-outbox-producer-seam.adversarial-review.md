# Adversarial review — B5-1 in-tx audit + governance-outbox producer seam（对 Postgres 语义）

- **Subject**: `docs/design/2026-08-08-aero-im-core-b5-1-in-tx-audit-outbox-producer-seam.design.md`
- **Method**: 源码逐点核对（migrations 0235/0236/0239/0240/0241、`aero-storage/src/message/{authorization,events,idempotency,crud}.rs`、`room/governance.rs`、`audit_governance.rs`、`snaplink_commercial.rs`、`aero-ai/src/governance.rs`、`aero-im-core/src/db_tests/governance_drill_tests.rs`、`test-integration.sh`）+ 对活 PG（docker aero-postgres, pg17）的 `EXPLAIN` / 类型行为实证。
- **Verdict**: 4 问全部有结论。**1 个设计必须修正的缺陷**（priority-drill db_test 第二 claim 断言被 seam 行打破，设计「三 drill 行为不变」为假）+ **1 个热路径成本缺陷**（`envelope_in_tx` 的 occurred_at SELECT 对分区表全分区探测，DEFAULT 分区 seq scan，实证）+ 若干措辞/加固级发现。F4 枚举结论 = **空集**（无任何存量测试在 enforcement ON + 无 binding 下驱动 5 路径）。

---

## Q1 锁序 / 锁升级 / 索引命中 / `SELECT workspace_id FROM rooms` 竞态

### 1.1 结论：无新增死锁环；锁序与既有 delete 路径同构

- `envelope_in_tx` 的两个 SELECT（`audit_events.created_at`、`snaplink_commercial_bindings.source_system`）都是**裸 MVCC 读**（无 FOR SHARE/UPDATE）——不取行锁就不可能参与死锁环；PG 无锁升级（不存在 escalation 对象）。0236/0239 trigger 内的 `aero_snaplink_binding_for_workspace` 同为裸读（`STABLE`），无锁。
- 本设计**唯一的行锁增量** = audit INSERT 的 FK `FOR KEY SHARE`（`audit_events.workspace_id → workspaces`、`actor_id → participants`，0007/0146 继承）。该锁**今天已被 delete/recall 路径以相同尾部位置获取**（`soft_delete_outboxed_authorized`/`recall_outboxed_authorized` 在持 rooms FOR SHARE + messages FOR UPDATE 后写 audit 行）。生产上所有锁定 `participants` 行的写者（profile 更新、GDPR 删除）只持 participants 锁、不持 workspace/room 锁 → 单向等待，无环；deactivation 只锁 workspaces + workspace_members。新增路径（send/edit/room.*）的锁集 ⊆ delete 路径锁集 ∪ room governance 锁集（后者已持 workspace FOR UPDATE），顺序一致 → **无新死锁环**。
- `SELECT workspace_id FROM rooms`（insert_outboxed seam）：**竞态-free 成立**——同 tx 早前 `lock_effective_message_write_access`（authorization.rs:99-151 → `aero_effective_room_access` 0197 内第二个 SELECT）已对该 rooms 行取 `FOR SHARE`，行版本被钉到 commit；后置裸读必然看到被锁版本。且 `rooms.id` 是 PK，索引命中。更优做法：`lock_effective_sender_room_access`（idempotency.rs:170-178）现把 `LockedRoomWriteAccess{workspace}` 折叠成 bool 丢弃——改回传 workspace 即可免掉这条额外 SELECT（2 行重构），设计保留 SELECT 亦可接受。

### 1.2 🔴 缺陷（热路径成本）：occurred_at SELECT 对分区表全分区探测 + DEFAULT 分区 seq scan

`audit_events` 自 0146 起为 `PARTITION BY RANGE (created_at)`，PK = `(id, created_at)`。设计（及 half-B 先例 audit_governance.rs:522-527）的查询 `SELECT to_jsonb(created_at) #>> '{}' FROM audit_events WHERE id=$1` **不含分区键** → PG 无法 pruning。对活库实证：

```
EXPLAIN ... WHERE id = '...'                    → Append（全部 3 个分区各一次 Bitmap Index Scan on PK；
                                                    audit_events_default 是 Seq Scan）
EXPLAIN ... WHERE id = '...' AND created_at=$2  → Index Only Scan on audit_events_default_pkey（单分区）
```

即：每次 send/edit/delete 的 envelope 构造 = **O(#分区) 次 PK 探测 + 一次 DEFAULT 分区 seq scan**。默认分区内容 = 无日分区兜底行（维护滞后/法定保全滞留），可能很大；每日分区按 365 天留存 ≈ 365+ 次探测。这发生在全系统最热路径（每条消息）。

**修复（二选一，均零迁移）**：
1. `append_in_tx` 的 INSERT 加 `RETURNING created_at`（值本来就在 Rust 手里：`OffsetDateTime::now_utc()`），envelope 用 `SELECT to_jsonb($1::timestamptz) #>> '{}'` 纯表达式构造——**零表读**、零探测，拼写与 trigger 的 `jsonb_build_object('occurred_at', NEW.created_at)` 同会话同文本（`to_jsonb` 同源）。
2. 或查询加 `AND created_at = $2`（绑定同一值，微秒精度精确相等）→ 单分区 Index Only Scan（实证）。
   （不得用 Rust formatter 直接拼字符串——设计「绝不 Rust formatter」正确：PG 的 timestamptz→jsonb 文本是会话 TZ 依赖的，见 Q3。）

---

## Q2 Trigger 互操作 + F4 完整枚举

### 2.1 0239 token gate + ON CONFLICT 对 Rust 写行 = 等价（逐路径核对成立）

对 5 个新 token 的 audit 行，0239 trigger 必然早退（Gate 1 关 → RETURN；token 门 `NEW.action <> 'message.moderated'` → RETURN；Gate 2 仅 message.moderated 可达）→ **trigger 对 seam token 零产出**，seam 的 `ON CONFLICT (event_id) DO NOTHING` 今天不可能撞 trigger 行；两者按 token 集合互斥，无双写。形状等价由 half-B 断言集钉住（16 键、status 0/attempts 0/available_at 走 DDL 默认，与 trigger INSERT 列集一致）。两点注记：
- **刻意不对称确认**：seam 无条件入队（不看 runtime.enabled）——与 Gate 1 的不对称是设计明示；且 `"aero-im"` 回退仅在「无 binding」时可达，而该配置下 v1 0236 trigger 必然无行（OFF → Gate 1；ON → 在 audit INSERT 处 RAISE 早于 seam）→ **v1/v2 对同一事件永不同时携带不同 source_system**，回退自洽。
- **payload 未净化**：trigger 路 `payload` = `aero_snaplink_audit_payload(NEW.detail)`（净化 + 64KB 截断），seam 路 = detail 原样（half-B 钉 `back.payload == detail`）。当前 5 处 detail 均为固定形状小字面量，无敏感键——仅作契约注记，若未来 caller 塞敏感键需在 Rust 侧同构净化（有漂移风险，建议在 repo doc 明示此不对称）。

### 2.2 F4 完整枚举 = **空集**（并纠正 F4 措辞）

enforcement-ON 的全部站点（`rg snaplink_commercial_runtime` 限定）：
- `aero-storage/src/audit_governance.rs` 5 测试（parity/rust_produced/ddl_contract/non_moderation/duplicate/reconcile）——只驱动 moderate_finalize / soft_delete_audited / 直插 SQL，**均带 binding**；
- `aero-im-core/src/db_tests/governance_drill_tests.rs` 2 测试——`write_path_rows` 驱动 send + room.create，**带 binding**（`seed_governance_enforcement`）；
- `aero-storage/src/snaplink_commercial_db_tests.rs` —— `MessageRepo::insert`（裸路径，非 seam），带 binding；
- `aero-eng/src/audit_provision.rs` / aero-server runtime.rs —— 生产 repo/CLI，不驱动 5 路径。

逐路径结论（enforcement ON + 无 binding）：
| 路径 | 今天 | seam 后 | 存量测试 |
|---|---|---|---|
| send（insert_outboxed） | **已 fail-closed**（0235 metering 在 messages INSERT 即 RAISE） | 不变 | 无（今天就不可能通过，故不存在） |
| delete（soft_delete_outboxed_authorized） | **已 fail-closed**（0236 在既有 audit 行上 RAISE） | 不变 | 无 |
| edit | 成功（零 audit） | **新 fail-closed** | 无（enforcement-ON 站点无一驱动 edit） |
| room.created | 成功 | **新 fail-closed** | 无（唯一驱动者 = drill，带 binding） |
| room.member.add | 成功 | **新 fail-closed** | 无 |

→ **没有任何存量 db_test/fixture 在 enforcement ON + 无 binding 下驱动这 5 路径**；设计的「全量 -- --ignored 排查」是防御性空跑。同时 F4 表格措辞不准确：send/delete 并非「新失败」（今天已 fail-closed），真正新增 fail-closed 的只有 edit + room.created + room.member.add。

### 2.3 🔴 缺陷：设计漏掉一个真实的 ignored-suite 回归——priority-drill db_test

`drill_priority_claim_preempts_fifo_on_write_path_rows`（im-core governance_drill_tests.rs:480，主 integration 腿不 skip 它，`--test-threads=1` 共享主库运行）在 seam 落地后：
- `write_path_rows`（n=10）现在产生 **1 room.created + 10 message.send 的真实 priority-10 outbox 行**（外加 10 条 priority-100 的 moderated 行）；
- 测试再种 40 条 synthetic backlog（priority 10，available_at 在 200s 前）；
- 第一 claim（limit 10）：priority DESC → 恰好 10 条 admin → `claimed_ids == admin_set` **仍绿**；
- **第二 claim（limit 50）**：due 集 = 40 backlog + 11 seam 行 = 51 → 返回 50 行 → `rest.len() == 40` ✗、`rest_set == backlog_set` ✗ **红**。

即设计的 §2「harness 零改动」与 §5(e)/§4.7「三 drill 行为不变 / moderation-priority-drill PASS」**为假**（aero-cli 那条 harness drill `audit-provision-check --priority` 用独立 fresh DB 直种行，不受影响；受影响的是 im-core 这条 db_test）。**必须改 drill**（例如第二 claim 断言改为 `backlog_set ⊆ rest_set` + `rest.len() == min(50, 51)` 或 claim 前清掉 seam 行），并把它加进变更清单——这是对「零改动」承诺的实质修正。另一条 `drill_payload_contract_16_key_envelope_via_moderate_delete`（limit 1, priority DESC 取 moderated 行）不受影响。

---

## Q3 回滚原子性 + occurred_at 保真

### 3.1 无「有 outbox 无 audit」路径（逐 seam 核对）

- 五 seam 顺序恒为 ① 域变更 → ② `append_in_tx`（audit，捕获 AuditId）→ ③ `envelope_in_tx` + outbox append，同 tx；任一步 Err → 整体回滚。`insert_outboxed` 幂等重放早退（idempotency.rs:166-176）在 `pool.begin()` **之前** → 零写入；edit/delete 的 `Ok(None)`（版本冲突/已删/已 recall）、`add_member` 的 `Ok(false)`（已存在）都在任何写之前返回；send 的并发输家路径（claimed==false）整体 rollback 含 seam 行。0236/0239 trigger 产的行随 tx 同滚。**无逃逸路径**。
- 反向（有 audit 无 outbox）仅存在于设计外生产者：`soft_delete_audited`（crud.rs:369，测试专用——已核实全部调用点均在 `#[cfg(test)]`）、retention sweep（sweep.rs 零 audit 引用）、0241 reconciler 只回填 message.moderated——均与 5-token 契约无冲突。
- 加固建议：`audit_governance_outbox.event_id` **无 FK**（1:1 靠约定 + ON CONFLICT）。`envelope_in_tx` 的 occurred_at 读取应钉 `fetch_one`（RowNotFound → Err → fail-closed，half-B 已是此语义），勿用 `fetch_optional` 吞缺行——否则未来重排 seam 顺序时可能写出缺 audit 的 outbox 行。

### 3.2 `#>> '{}'` 保真：实证成立（微秒 + 偏移完整往返）

活库实证：`to_jsonb('2026-08-08 22:30:00.123456+00'::timestamptz) #>> '{}'` → `2026-08-08T22:30:00.123456+00:00`——微秒 6 位保留、UTC 偏移内嵌，下游 ISO 8601 解析可恢复精确瞬间；与 0239 trigger `jsonb_build_object('occurred_at', NEW.created_at)` 的 jsonb 文本**同会话同拼写**（同源 to_jsonb）。会话 TZ 依赖已实证（America/New_York 下同瞬间渲染为 `-04:00`）——两条路径共享该依赖，且 relay 原样转发、sink 按瞬间解析，跨会话拼写差异无正确性影响。`created_at NOT NULL` → 无 NULL 注入面。

---

## Q4 delete seam 条件化（action == message.deleted）——无双写

`soft_delete_locked_outboxed_in_tx`（events.rs:293-355）全部 3 个生产调用者枚举：
| 调用者 | action | 0239 trigger | seam |
|---|---|---|---|
| `soft_delete_outboxed_authorized`（authorization.rs:499，用户删除） | `"message.deleted"` | token 门早退 | **写 outbox 行**（唯一 seam 写者） |
| `soft_delete_outboxed_system` ← im-core `moderate_delete` :645、ai worker :401 | `"message.moderated"` | Gate 2 写行（ON + binding） | 跳过 |
| **`message_reports.rs:305`（报告审核移除——设计证据未枚举的第三生产者）** | `"message.moderated"`（actor = reviewer，participant） | Gate 2 写行 | 跳过 |

- **无任何调用者把 message.moderated 送进 seam 写分支** → 无双写。且结构兜底：即使条件写错，trigger 在 audit INSERT 时（早于 seam 的 outbox INSERT）先写，seam 的 `ON CONFLICT (event_id) DO NOTHING` no-op，trigger 行（admin/100、binding source_system）胜出——语义仍正确。设计证据 E3 应把 message_reports.rs:305 补进调用者清单。
- `soft_delete_audited`/`soft_delete_moderated`（crud.rs:369/:398）走独立 `soft_delete_in_tx`，测试专用（已核实），不触 seam → `non_moderation_action_passes_through_unmapped` 的 0-governance 钉不受影响，设计 R7 成立。

---

## 修正清单（按优先级）

1. **priority-drill db_test 第二 claim 断言**（governance_drill_tests.rs）必须随 seam 更新——设计 §2「harness 零改动」/§5(e)/§4.7 相应改口；这是唯一被遗漏的真实回归面。
2. **`envelope_in_tx` occurred_at 查询改单分区**（`AND created_at=$2` 或 `RETURNING created_at` + 纯表达式）——热路径 O(#分区)+DEFAULT seq scan → O(1)（实证）。
3. F4 表格措辞纠正：send/delete 已 fail-closed（非新增），新增面 = edit/room.created/room.member.add；且枚举结论 = 空集（附上 2.2 的表当「全量排查」的 concrete list）。
4. 加固：`envelope_in_tx` 钉 `fetch_one`（缺行 fail-closed）；repo doc 明示 seam 路 payload 不净化 vs trigger 路净化的不对称。
5. 补充设计证据：`message_reports.rs:305` 为 `soft_delete_locked_outboxed_in_tx` 第三生产调用者（message.moderated，participant actor）。
