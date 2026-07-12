Good. I now have a complete picture of the codebase reality. Here is my thorough analysis of your document.

---

## 交叉验证报告：准确性 + 增量价值

### 验证摘要

| 方向 | 文档宣称 | 代码库实际 | 准确性 |
|------|---------|-----------|--------|
| ① 连接池隔离 | 单一 `PgPool`，无 timeout，health 共享 | ✅ 单一 `pg` 主池 + ✅ 已有 `pg_read` replica 池 + ✅ **已有** `statement_timeout=10s` via `after_connect` + ✅ `test_before_acquire` + ⚠️ `/health` 探活仍在主池 (`s.participants.pool()`) | ⚠️ **需修正**：低估了既有改进 |
| ② 多租户 RLS | 零 RLS，零列加密 | ✅ 完全准确 | ✅ 准确 |
| ③ 幂等键/Saga | 零 Idempotency-Key 头，零 saga | ✅ 准确 | ✅ 准确 |
| ④ WS 协议优化 | JSON-only，无 deflate，无批处理 | ✅ 完全准确 | ✅ 准确 |
| ⑤ 缓存一致性 | `participant_cache` 仅本地失效 | ✅ `participant_cache.invalidate` 仅本地 + 🔴 **`room_member_cache.invalidate` 有定义但在任何业务代码中零调用者**（比文档严重） | ✅ 准确（低估了严重性） |

### 方向一（连接池隔离）——关键修正

文档说「无池分区、无 label、无优先级」和「`select 1` 如果因为池满而超时」——但代码库实际已经比文档描述的更先进：

**已存在但文档忽视的**：
```rust
// crates/aero-storage/src/db.rs 第 6-41 行 — 已有
const STATEMENT_TIMEOUT_SQL: &str = "SET statement_timeout = '10000'";
// after_connect 在每连接建立时设 10s 超时
// min_connections(2), test_before_acquire(true), acquire_timeout(30s)

// crates/aero-server/src/state.rs 第 245-247 行 — 已有
pub pg_read: PgPool,  // 只读 replica 池，路由到 search/analytics/ai_usage
```

**文档仍有效但需修正后的论证**：
1. 主池 `pg` 仍是所有写入 + 大多数读取的单点（AI worker，sweepers，bots 全用它）
2. `/health` 探活 (`routes/health.rs:28-34`) 使用 `s.participants.pool()`——这是主池，不是独立 health 池
3. 无 `application_name`——pg_stat_activity 无法区分连接用途
4. 无 maintenance 池——sweeper 和 worker 与 HTTP handler 共享连接槽

### 方向五（缓存一致性）——低估了严重性

文档说 `participant_cache.invalidate` 只清本地。这是正确的。但有一个**更重要**的发现文档没有强调：

```rust
// room_member_cache.rs 第 111 行 — invalidate 方法已定义
pub fn invalidate(&self, room: &RoomId) { self.map.remove(room); }

// 但全代码库零业务路径调用它！
// bus.rs 第 84/129 行 — 只调 get_or_fetch，永远不 invalida
// 所有 add_member/remove_member 路径（dm.rs sso.rs scim/ webhooks/ guests/）— 无一调 invalidate
```

这意味着**成员增删后 60s TTL 内所有实例都返回旧成员列表**。如果 `assert_room_access` 信任缓存，已踢出的成员可在 60s 内继续操作。这比单纯的「跨实例不一致」更危险——它是**同一实例内也不一致**。

### 对既有分析的交叉验证

你声明的「零系统性架构级论证」基本成立，但有几个既有分析中确实覆盖了部分线索：

- **既有分析提及过** `pg_read`（2026-07-10-post-scan-five-architectural-edge-case-directions 等 ROADMAP 方向四）——但确实停留在「加一个 read replica」，没有做 workload 级 pool 分区
- **既有分析提及过** `statement_timeout`（2026-07-10-production-scale-security-lifecycle-gaps.md）——但文档说它们「停留在加一个 timeout 参数」，代码库已落地这一条（10s timeout），印证了既有分析的推进

### 三个文档未覆盖的交叉发现

1. **`room_member_cache.invalidate` 零调用者**（见上）——方向五的严重性翻倍
2. **Health probe 复用主池**（`routes/health.rs:28` `s.participants.pool()`）= 如果主池满，health check 也挂——这是文档未强调的「级联故障」场景具体案例
3. **`pg_read` 仅被 3 个模块消费**（search.rs, ai_usage.rs, analytics.rs）——大量只读查询（notification fan-out, room member list fetch, presence check）仍走主池

---

### 综合评价

这是一份**高质量的架构分析**，五个方向中的四个（②③④⑤）是真正的系统性盲点，代码证据扎实。方向①需要补充代码库已有改进的上下文，但核心论点（按 workload 隔离的连接池架构是缺失的）仍然成立。

最突出的三个「行动项级」发现（按文档分类）：

| 优先级 | 方向 | 具体发现 | 修复激进程度 |
|--------|------|---------|------------|
| P0 | ⑤ | `room_member_cache.invalidate` 定义但零调用 | 1 行代码插入每个 add_member/remove_member 路径 |
| P0 | ① | `/health` 使用主池，主池满则 health 假阴性 | 创建 `PgPool::new_with_options` 一个连接、1s 超时的 health-only 池 |
| P1 | ① | 无 `application_name` — 无法在 PG 侧归因连接用途 | `after_connect` 中加 `SET application_name = 'aero-primary'` |

需要我将这份文档保存为文件，并补充上述修正吗？
