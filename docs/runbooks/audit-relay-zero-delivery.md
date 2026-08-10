# Audit relay — fail-closed 零投递检测与恢复 runbook

> 适用：`aero-audit-connector` relay（`AERO_AUDIT_*` env，B5-2 机器令牌验证 seam）。
> 设计出处：`docs/design/2026-08-08-aero-auth-b5-2-machine-token-verification-seam.design.md` §5.1（哨兵串与指标定义以该节为准）。
> 背景：验证器要求 RFC 9068 at+jwt（typ/alg 白名单 + 强制签名 + 8 required claims + sub==client_id + future-iat/jti 约束 + scopes）。**主流 IdP 的默认 client-credentials token 形状全部 fail-closed**——Azure AD v2/Auth0 发 `typ:"JWT"`、Keycloak 发 `typ:"Bearer"`、Azure AD cc token 无 jti、Azure AD/Keycloak/Auth0/Okta 发 `azp` 而非 `client_id`、Keycloak service-account 的 sub 是 client UUID。这些 token 被拒后行进 **Transient 有界 requeue、零投递、永不 Dead**——这是**预期状态**（契约变严的直接后果），不是故障；在 IdP 配成发 RFC 9068 at+jwt 之前不要「修」验证器。本 runbook 让该状态无需轮询 DB 即可探测与定位。

## 1. 哨兵串（`last_error` 精确串，grep 前缀即够）

| 场景 | 精确串 | 状态 |
|---|---|---|
| 验证器拒（Transient，卡重试面） | `transient audit delivery failure: audit token validation failed: ` + 下列 seam 文案之一 | status 0/1，attempts ≥ 1 |
| — typ 非 at+jwt | `…token rejected: token is not an RFC 9068 access token` | 同上 |
| — sub≠client_id（令牌内自指检查） | `…token rejected: token is not a client_credentials machine identity` | 同上 |
| — future-iat（时钟漂移） | `…token rejected: token issued-at time is in the future` | 同上 |
| — jti 缺失/非法 | `…token rejected: token id is invalid` | 同上 |
| — 缺 claims（serde missing-field，如 client_id） | `…token rejected: missing field \`client_id\`` | 同上 |
| — 未知 kid / JWKS 未达 | `…no signing key for kid Some("…")` | 同上 |
| — 坏格式 / 非白名单算法 | `…malformed token: …` / `…unsupported signing algorithm` | 同上 |
| scope-deficient → Unprovisioned dead | `audit token lacks a required application scope (unprovisioned)` | status 3（attempt 1），**不复苏**（真配给故障） |
| token-endpoint 403 → Unprovisioned dead | `audit token endpoint rejected the client credentials (HTTP 403)` | status 3（attempt 1），**复苏 SQL 只匹配这条**（§5） |
| sink 403（既有，不改） | `audit sink rejected the service identity (HTTP 403)` | status 3（attempt 1） |
| T-11 钉（不改文案） | `audit connector HTTP transport failed: …` | status 0/1（t11-drill 断言子串） |

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

- [ ] header `typ` == `at+jwt`（或 `application/at+jwt`，大小写不敏感）；
- [ ] header `alg` ∈ {RS256, EdDSA}；`kid` 存在且命中 JWKS（多 key 时必须）；
- [ ] claims 齐 8 项：`iss`/`aud`/`exp`/`nbf`/`iat`/`jti`（非空、无控制字符、≤1024）/`sub`/`client_id`；
- [ ] `sub == client_id`（RFC 9068 合规 IdP 恒满足——这是令牌内自指检查，与任何 env 无关）；
- [ ] `scope` 含 `audit:event:write`（或缺则按 `Unprovisioned` dead——见 §5 复苏边界）；
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
| status 0/1 行 attempts ≥ 1，last_error 以 `transient audit delivery failure: audit token validation failed: ` 开头 | fail-closed 契约生效（预期，直到 IdP 配成 at+jwt） |
| status=2 增长，`aero_audit_delivery_outcomes_total{outcome="delivered"}` 增长 | 契约满足，可上线 |
| status=3 + `%token endpoint rejected the client credentials (HTTP 403)%` | 403 误杀（临时限流）→ §5 复苏 SQL |
| status=3 + `%unprovisioned%` | 真配给故障 → 修 IdP scope，**不复苏** |

## 3. 指标与告警（`/metrics`，bearer 门控同网关）

4 条 series（定义见设计 §5.1；名称在 `aero-audit-connector/src/metrics.rs` crate 内 const）：

| series | 类型 | label 值域 |
|---|---|---|
| `aero_audit_outbox_transient_requeue` | gauge | 无（30s 采样） |
| `aero_audit_outbox_dead` | gauge | 无（30s 采样，同一条 SQL） |
| `aero_audit_token_rejections_total` | counter | `reason` ∈ {`scope`,`claims`,`unknown_key`,`malformed`,`unsupported_alg`,`other`} |
| `aero_audit_delivery_outcomes_total` | counter | `outcome` ∈ {`delivered`,`transient`,`permanent`,`forbidden`,`unprovisioned`} |

```promql
# FAIL-CLOSED 零投递签名（唯一必须立刻响应的规则）：正常瞬态 backoff ≤300s 会自行排空，
# requeue>0 且 delivered=0 持续 = 结构性拒绝（真实 IdP 默认形状）→ 按 §2 探针核对 IdP，勿改验证器
rate(aero_audit_delivery_outcomes_total{outcome="delivered"}[30m]) == 0
  and aero_audit_outbox_transient_requeue > 0        # → CRITICAL，for: 30m
aero_audit_outbox_dead > 0                            # → WARN（403 误杀 / scope-deficient 真配给故障）
increase(aero_audit_token_rejections_total{reason="unknown_key"}[15m]) > 0   # → JWKS URI/可达性/kid 问题
increase(aero_audit_token_rejections_total{reason="claims"}[15m]) > 0       # → IdP token 形状（typ/jti/client_id/iat…）
```

```bash
# 手动查（AERO_METRICS_TOKEN 仅在启用时需带）
curl -fsS -H "Authorization: Bearer $AERO_METRICS_TOKEN" http://localhost:3030/metrics | grep aero_audit_
```

## 4. 部署后 watch 窗口（24–72h）

- 三 grep（每班一次）：`last_error` LIKE `%audit token validation failed%` / `%unprovisioned%` / `%token endpoint rejected%`——分别对应验证器拒（卡 Transient）、scope 配给故障（dead）、403 误杀（dead，可复苏）；
- 四条告警规则持续生效；CRITICAL 规则即「部署告警」——此前设计文档言过其实处已由 §5.1 指标面取代；
- `deliver`/requeue 的 `warn!(reason=…)` 日志与 `last_error` 列同文，grep 日志与 grep 列二选一即可。

## 5. false-Dead 复苏 SQL（403 误杀回滚；仅限 %token endpoint rejected% 行）

```sql
UPDATE audit_governance_outbox
   SET status = 0, attempts = 0, last_error = NULL,
       available_at = clock_timestamp(),
       claim_token = NULL, lease_expires_at = NULL   -- 必须同清，过 audit_governance_claim_state CHECK
 WHERE status = 3
   AND last_error LIKE '%audit token endpoint rejected the client credentials (HTTP 403)%';
```

- **只**复苏 token-endpoint-403 死行（临时限流误判，如 IdP rate-limit）；scope-deficient 死行（`%unprovisioned%`）是真配给故障——修 IdP scope，不重投；
- dead 行永久排除于 `claim_due`（fake.rs:207 语义），无 ops-requeue 面——本 SQL 是唯一恢复路径；
- 二进制回退同样适用：connector 非 durable consumer，outbox 行即 durable 位置，租约（默认 30s）过期后旧二进制自动重新 claim；`Idempotency-Key: event_id` 保证 sink 侧去重；
- 401→Transient 路径不需要任何人工干预（secret 轮换自愈）。

## 6. 故障速查

| 症状 | 判定 | 动作 |
|---|---|---|
| `transient_requeue` > 0 且 `delivered` rate = 0（CRITICAL） | 真实 IdP 默认形状 fail-closed（预期状态） | §2 探针核对 IdP 签发；配 IdP 发 RFC 9068 at+jwt；**勿改验证器/勿放宽契约** |
| `reason="unknown_key"` 增长 | JWKS URI 不可达 / kid 不匹配 | 查 `AERO_AUDIT_JWKS_URI` 可达性；比对 token header kid ∈ JWKS kid 集 |
| `reason="claims"` 增长 | IdP token 形状（typ/jti/client_id/iat/exp…） | 按 §2 核对清单逐项排查 |
| `dead` > 0 且 `%token endpoint rejected%` | 403 误杀（临时限流） | §5 复苏 SQL |
| `dead` > 0 且 `%unprovisioned%` | 真配给故障（scope 未授） | 修 IdP scope，不复苏 |
| `dead` > 0 且 `%HTTP 403%`（sink 串） | sink 拒服务身份（T-11 既有语义） | 修 sink 侧身份/配给，不复苏 |
