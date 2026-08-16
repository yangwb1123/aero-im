# Audit relay — fail-closed 零投递检测与恢复 runbook

> 适用：`aero-audit-connector` relay（`AERO_AUDIT_*` env，B5-2 机器令牌验证 seam）+ B5-4 运行时
> fail-closed 环（settle 心跳保鲜门 + scope 自动反馈 + outbox 采样 gauge）。
> 设计出处：`docs/design/2026-08-08-aero-auth-b5-2-machine-token-verification-seam.design.md`、
> `docs/design/2026-08-09-aero-audit-connector-b5-4-settle-heartbeat-rejection-point.design.md`、
> `docs/design/2026-08-08-aero-audit-connector-b5-4-fail-closed-operational.design.md`。
> 本 runbook 只描述**已落地**的验证面与指标面：relay 自身验证器是 RS256 白名单 + claims 门
> （iss/aud/sub/scope/typed-gate/时间窗），**不是** RFC 9068 strict validator——「主流 IdP 默认
> client-credentials token 形状全部 fail-closed」是历史叙事（aero-auth 消费侧的事），relay 侧
> 的 fail-closed 行为以本文件 §1 哨兵串为准。

## 0. B5-4 运行时环（先读这段）

relay 的 settle（acknowledge）面现在由**心跳保鲜门**守卫，且 scope 缺失**立即 dead**：

- **保鲜门**：`audit_relay_provisioning`（0248，singleton 行）存在且 `verified_at` 新鲜
  （`now − verified_at ≤ AERO_AUDIT_PROVISION_FRESHNESS_SECS`，默认 300s，范围 [60, 86400]）
  时 settle 才放行；行缺失/过期 → settle 返回 false、行保持 status 1 claimed、租约过期后由
  `claim_due` 自动重领重试（**永不双 settle**）。保鲜是**tick 驱动**的（server 60s 固定 tick +
  启动一次 bootstrap），与 settle 流量无关——静默期不会饿死门；最坏 ~60s 落后于 300s 窗口。
- **scope 自动反馈**：client-credentials token 的 claims 不含 `audit:event:write`（`scope` /
  `scopes` 两种形状都查）→ **立即 dead（Forbidden 类 T-11）**，attempts=1、`delivered_at` NULL、
  `last_error` = `audit:event:write scope missing from the client credentials token (T-11)`——
  配给故障以可见的 terminal 呈现，不再藏成 Transient 无限重试。复苏见 §5。
- **可观测**：`/metrics` 上新 sampler 系列（§3.2）让「relay 缺席/黑洞 → status 0/1 行悄悄滞留」
  变成可告警信号；任何采样失败 fail-closed（`sampler_up 0` + 旧值保留，绝不伪造零快照）。

## 1. 哨兵串（`last_error` 精确串，grep 前缀即够）

| 场景 | 精确串 | 状态 |
|---|---|---|
| 验证器拒（Transient，卡重试面） | `transient audit delivery failure: audit token claim validation failed: ` + 下列 seam 文案之一 | status 0/1，attempts ≥ 1 |
| — 形状非法（空/控制字符/超长） | `…token has an invalid shape` | 同上 |
| — typed-gate（缺 sub/client_id） | `…token claims missing required fields (sub/client_id)` | 同上 |
| — iss 不符/缺失 | `…token iss does not match the configured issuer` / `…token has no iss claim` | 同上 |
| — aud 不符 | `…token aud does not contain the configured audience` | 同上 |
| — sub 不符 | `…token sub does not match the configured identity` / `…token has no sub claim` | 同上 |
| — 时间窗（exp/nbf/leeway） | `…token has expired` / `…token is not yet valid` / `…token exp is not a number` / `…token nbf is not a number` | 同上 |
| — 签名面 malformed / alg | `…token rejected: …malformed…` / `…unsupported signing algorithm…` | permanent 类（JWKS-on） |
| — 未知 kid / JWKS 未达 | `…no signing key for kid Some("…")` / `audit jwks unavailable: …`（Transient） | 见 design §D6 |
| **scope 缺失 → ScopeRejected dead（B5-4 新增）** | `audit:event:write scope missing from the client credentials token (T-11)` | status 3（attempt 1），复苏 SQL 匹配 `%T-11%`（§5） |
| token-endpoint 403 | `audit token endpoint rejected the client credentials (HTTP 403)` | status 3（attempt 1），**复苏 SQL 只匹配这条**（§5） |
| sink 403（既有，不改文案） | `audit sink rejected the service identity (HTTP 403)` | status 3（attempt 1） |
| **保鲜门拒（settle 面，B5-4 新增）** | 日志 `audit relay heartbeat is stale or absent; settle rejected fail-closed…` / `…freshness check failed…` | status 1（租约重领），**非 dead** |
| T-11 钉（不改文案） | `audit connector HTTP transport failed: …` | status 0/1（t11-drill 断言子串） |

> `%unprovisioned%` 哨兵（`audit token lacks a required application scope (unprovisioned)`）是
> 已被否决的 strict-validator 叙事——crates 中不存在该串，勿再 grep；scope 配给故障的现状见
> 上一行的 ScopeRejected 串（B5-4 auto-feedback 落地后）。

## 2. 预部署 staging 探针（上线前必跑，真实 IdP；AGENTS.md §4.5「待联调」）

```bash
# 探针 0：取真实 token（staging IdP；scope 按 relay 配置）
TOKEN=$(curl -fsS -u "$AERO_AUDIT_CLIENT_ID:$AERO_AUDIT_CLIENT_SECRET" \
  -d 'grant_type=client_credentials' -d 'scope=audit:event:write' \
  "$AERO_AUDIT_TOKEN_ENDPOINT" | jq -r .access_token)

# 探针 1：离线解码 header/claims（base64url → JSON）
python3 - "$TOKEN" <<'PY'
import base64, json, sys
h, p, _ = sys.argv[1].split('.')
pad = lambda s: s + '=' * (-len(s) % 4)
print("HEADER", json.dumps(json.loads(base64.urlsafe_b64decode(pad(h)))))
print("CLAIMS", json.dumps(json.loads(base64.urlsafe_b64decode(pad(p)))))
PY

# 探针 2：kid 命中——JWKS 有 keyed 条目时 token header 必须带 kid 且 ∈ JWKS kid 集
curl -fsS "$AERO_AUDIT_JWKS_URI" | jq -r '.keys[].kid'

# 探针 3：时钟同步——|now − iat| 与 exp 余量须 < 60s（LEEWAY 60；future-iat 即拒）
date -u +%s
```

**核对清单**（每项不达标 = 预期 fail-closed，先配 IdP 再上线）：

- [ ] header `alg` == `RS256`（JWKS-on 白名单；JWKS-off 跳过签名面）；
- [ ] `kid` 存在且命中 JWKS（JWKS-on 且多 key 时必须）；
- [ ] claims 有 `iss`（匹配 `AERO_AUDIT_ISSUER`）/`aud`（含 `AERO_AUDIT_AUDIENCE`）/`sub`（匹配 `AERO_AUDIT_EXPECTED_SUB`）/`client_id`（typed-gate 必填）；
- [ ] `scope` 含 `audit:event:write`（或缺则按 ScopeRejected dead——见 §5 复苏边界）；
- [ ] 时钟：`|now − iat| < 60s` 且 `exp` 余量 > 60s。

```bash
# 探针 4：实跑 relay 一轮后 grep last_error（等 ≥1 个 poll_interval + 首轮 backoff；勿只离线解码）
psql "$DATABASE_URL" <<'SQL'
SELECT status, count(*) FROM audit_governance_outbox GROUP BY status ORDER BY status;
SELECT event_id, status, attempts, left(last_error, 200) AS last_error
  FROM audit_governance_outbox
 WHERE last_error IS NOT NULL
 ORDER BY created_at DESC LIMIT 20;
SQL
```

**探针 4 判读**：

| 观察 | 判读 |
|---|---|
| status 0/1 行 attempts ≥ 1，last_error 以 `transient audit delivery failure: audit token claim validation failed: ` 开头 | fail-closed 契约生效（预期，直到 IdP 配好 claims） |
| status=2 增长，`aero_audit_delivery_outcomes_total{outcome="delivered"}` 增长 | 契约满足，可上线 |
| status=3 + `%token endpoint rejected the client credentials (HTTP 403)%` | 403 误杀（临时限流）→ §5 复苏 SQL |
| status=3 + `%audit:event:write scope missing from the client credentials token (T-11)%` | scope 配给故障 → 修 IdP scope 授权，再 §5 复苏 SQL 重投 |
| status=1 持续滞留 + 日志 `heartbeat is stale or absent` | 保鲜门拒（§4 FM-Q）→ 查 `audit_relay_provisioning` 行/DB 连通性 |

## 3. 指标与告警（`/metrics`，bearer 门控同网关）

### 3.1 relay 计数（connector crate 内 const，进程内 counter，`sum()` 聚合）

| series | 类型 | label 值域 |
|---|---|---|
| `aero_audit_outbox_transient_requeue` | counter | 无 |
| `aero_audit_outbox_dead` | counter | 无 |
| `aero_audit_token_rejections_total` | counter | `reason` ∈ {`scope`,`claims`,`unknown_key`,`malformed`,`unsupported_alg`,`other`} |
| `aero_audit_delivery_outcomes_total` | counter | `outcome` ∈ {`delivered`,`transient`,`permanent`,`forbidden`,`unprovisioned`} |

### 3.2 B5-4 outbox 采样 gauge（server sampler，`AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS` 节流）

| series | 类型 / 聚合 | 语义 |
|---|---|---|
| `aero_audit_outbox_status{status="enqueued"\|"claimed"}` | gauge / `max()` | Tier-1（30s）精确计数（index-served） |
| `aero_audit_outbox_status{status="delivered"\|"dead"}` | gauge / `max()` | Tier-2 慢采样写入；**dead 是 7 天窗口内**（P-2 recency bound，CLI 无窗口） |
| `aero_audit_outbox_dead_rows` | gauge / `max()` | **任何** status=3 行存在的 0/1 标志（Tier-1 O(1) EXISTS，告警用这条） |
| `aero_audit_outbox_oldest_pending_secs` | gauge / `max()` | 最老 status=0 行年龄（Q4 镜像；无行则系列缺席） |
| `aero_audit_outbox_oldest_claimed_secs` | gauge / `max()` | 最老 status=1 行年龄——**保鲜门拒的受害者在这里可见**（FM-Q） |
| `aero_audit_outbox_sampler_up` | gauge / `min()` | 0 = 上次采样失败（旧值保留，fail-closed） |
| `aero_audit_outbox_sample_errors_total` | counter / `sum()` | 采样失败计数（Tier-1 + Tier-2） |
| `aero_audit_outbox_table_size_bytes` | gauge / `max()` | 表体积（增长先于 statement_timeout 暴露） |

```promql
# FAIL-CLOSED 零投递签名（唯一必须立刻响应的规则）：正常瞬态 backoff ≤300s 会自行排空，
# requeue>0 且 delivered=0 持续 = 结构性拒绝（claims/scope 配给）→ 按 §2 探针核对 IdP，勿改验证器
rate(aero_audit_delivery_outcomes_total{outcome="delivered"}[30m]) == 0
  and aero_audit_outbox_transient_requeue > 0        # → CRITICAL，for: 30m
# B5-4 家族（多实例 max/min 语义见 HELP 文本）：
aero_audit_outbox_dead_rows > 0                      # → WARN（scope 配给 / 403 误杀 / sink 拒身份）
aero_audit_outbox_sampler_up == 0                    # → WARN（采样面失败，旧值不可信）
max(aero_audit_outbox_oldest_claimed_secs) > 600     # → WARN（保鲜门拒持续 >10min，§4 FM-Q）
increase(aero_audit_token_rejections_total{reason="scope"}[15m]) > 0   # → WARN（B5-4：将立即 dead，见 §5）
increase(aero_audit_token_rejections_total{reason="unknown_key"}[15m]) > 0   # → JWKS URI/可达性/kid 问题
increase(aero_audit_token_rejections_total{reason="claims"}[15m]) > 0       # → IdP token 形状（iss/aud/sub/exp…）
```

```bash
# 手动查（AERO_METRICS_TOKEN 仅在启用时需带）
curl -fsS -H "Authorization: Bearer $AERO_METRICS_TOKEN" http://localhost:3030/metrics | grep aero_audit_
```

> 环境：`AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS` 三态——**缺省=300s；`0`=禁用（一条 boot
> info）；非法值（垃圾/负数/溢出）=fail-closed 禁用 + 一条 boot error**（拼错不会把最贵采样
> 频率拉高 5×）。`AERO_AUDIT_PROVISION_FRESHNESS_SECS` 默认 300s，范围 [60, 86400]，越界即
> boot 失败（与整个 `AERO_AUDIT_*` 家族一致，见 §4.3 AGENTS）。

## 4. 部署后 watch 窗口（24–72h）

- 三 grep（每班一次）：`last_error` LIKE `%audit token claim validation failed%`（卡 Transient）/
  `%audit:event:write scope missing from the client credentials token (T-11)%`（scope 配给故障，dead）/
  `%token endpoint rejected%`（403 误杀，dead，可复苏）；
- 采样 gauge 四看：`oldest_claimed_secs` 增长 = 保鲜门拒持续（FM-Q）；`sampler_up 0` = 采样面坏；
  `{status="dead"}` / `dead_rows` > 0 = terminal 出现；`oldest_pending_secs` 增长 = relay 缺席/黑洞
  （FM11，行滞留 status 0）；
- `deliver`/requeue 的 `warn!(reason=…)` 日志与 `last_error` 列同文，grep 日志与 grep 列二选一即可。

### FM-Q（保鲜门拒）速查

**症状**：行滞留 status 1（`oldest_claimed_secs` 增长），日志 `heartbeat is stale or absent`，
`audit_relay_provisioning` 行缺失或 `verified_at` 过旧。**诱因**：DB 停机/迁移回滚吞掉了
singleton 行、或 tick/boot 心搏持续失败。**恢复**（二选一）：

```sql
-- 1) 手动刷新（等价 operator 一次心跳；tick 之后会持续保鲜）
UPDATE audit_relay_provisioning SET verified_at = clock_timestamp(), updated_at = clock_timestamp();
-- 2) 或重启 relay（boot 的 one-shot bootstrap 臂会重建行）
```

**注意**：这是**设计内后果**（D1 override：投递面在保鲜失效时 fail-closed），不是 bug——门的存在
就是把「悄悄滞留」变成「可见 + 可恢复」。tick 驱动的心搏使静默期无法饿死门（最坏 ~60s 落后于
300s 窗口），missed-tick 会在下一 tick 自愈；`record_heartbeat` 是 UPSERT，可随时重入。

## 5. false-Dead 复苏 SQL（terminal 行回滚重投）

**scope 配给故障（B5-4 ScopeRejected）**——修好 IdP 授权后重投（token 形状曾经是病因）：

```sql
UPDATE audit_governance_outbox
   SET status = 0, attempts = 0, last_error = NULL,
       available_at = clock_timestamp(),
       claim_token = NULL, lease_expires_at = NULL   -- 必须同清，过 audit_governance_claim_state CHECK
 WHERE status = 3
   AND last_error LIKE '%audit:event:write scope missing from the client credentials token (T-11)%';
```

**token-endpoint 403 误杀**（临时限流，如 IdP rate-limit）：

```sql
UPDATE audit_governance_outbox
   SET status = 0, attempts = 0, last_error = NULL,
       available_at = clock_timestamp(),
       claim_token = NULL, lease_expires_at = NULL
 WHERE status = 3
   AND last_error LIKE '%audit token endpoint rejected the client credentials (HTTP 403)%';
```

- **只**复苏病因可定位的 dead 行：ScopeRejected = IdP scope 授权（修完再重投）；403 = 临时限流
  （误杀）；**sink 403**（`%audit sink rejected the service identity (HTTP 403)%`）是 sink 侧身份
  故障——修 sink，不重投；保鲜门拒（status 1）不需要本 SQL（租约自动重领）。
- dead 行永久排除于 `claim_due`（fake.rs:207 语义），无 ops-requeue 面——本 SQL 是唯一恢复路径；
- 二进制回退同样适用：connector 非 durable consumer，outbox 行即 durable 位置，租约（默认 30s）
  过期后旧二进制自动重新 claim；`Idempotency-Key: event_id` 保证 sink 侧去重；
- 401→Transient 路径不需要任何人工干预（secret 轮换自愈）。

## 6. 故障速查

| 症状 | 判定 | 动作 |
|---|---|---|
| `transient_requeue` > 0 且 `delivered` rate = 0（CRITICAL） | claims 面拒绝持续（iss/aud/sub/时间窗） | §2 探针核对 IdP 签发；配 IdP；**勿改验证器/勿放宽契约** |
| `reason="scope"` 增长 / dead 行 `%T-11%` | scope 配给故障（B5-4 auto-feedback 已 dead） | 修 IdP scope 授权 → §5 ScopeRejected 复苏 SQL |
| `oldest_claimed_secs` 增长 + `heartbeat is stale or absent` 日志 | 保鲜门拒（FM-Q） | §4 FM-Q：刷新 `verified_at` 或重启；查 DB 连通性 |
| `oldest_pending_secs` 增长 + 无 last_error | relay 缺席/黑洞（FM11） | 查 relay 配置/网络；行滞留 status 0 可见即可告警 |
| `sampler_up == 0` | 采样面失败（旧值保留，fail-closed） | 查 `aero_audit_outbox_sample_errors_total` 增长与 PG 连通性 |
| `reason="unknown_key"` 增长 | JWKS URI 不可达 / kid 不匹配 | 查 `AERO_AUDIT_JWKS_URI` 可达性；比对 token header kid ∈ JWKS kid 集 |
| `reason="claims"` 增长 | IdP token 形状（iss/aud/sub/exp…） | 按 §2 核对清单逐项排查 |
| dead > 0 且 `%token endpoint rejected%` | 403 误杀（临时限流） | §5 复苏 SQL |
| dead > 0 且 `%HTTP 403%`（sink 串） | sink 拒服务身份（T-11 既有语义） | 修 sink 侧身份/配给，不复苏 |
