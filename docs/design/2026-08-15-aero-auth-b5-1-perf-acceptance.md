# Performance hardening — B5-1 acceptance criteria (F-A / F-D / F-B / load leg)

> Companion to `2026-08-15-aero-auth-b5-1-in-tx-audit-outbox-landing.design.md` (status **Proposed**).
> Scope: turn the DB-architect findings F-A / F-D / F-B and the load/bench gap into **concrete acceptance metrics and threshold numbers**, with equivalence proofs and repo-grounded measurements. Every anchor was re-verified against the working tree on this run (`8f59850` + B5-1 landing in flight: 246 migrations, arbiter 246, `audit_governance/` landed).

**Two landed-code defects this spec pins (must be fixed by the landing batch):**

1. **F-A not applied**: `crates/aero-storage/src/audit_governance/outbox.rs:417-431` `aggregate_login_failure_buckets` still runs the un-indexable predicate (function-of-`created_at` WHERE) → full Seq Scan of the retained `login_failures` window every tick (default 30s) on the shared 16-conn pool. Migration 0243's header claim ("Serves the L1 bucket scan") is currently false; its only real consumer today is the retention sweep. Fix = §1.2 rewrite + §1.4 gate.
2. **Tick/window conflation**: `crates/aero-server/src/bin/boot/background.rs:337` passes the env tick cadence (`AERO__SERVER__LOGIN_FAILURE_L1_AGGREGATE_SECS`, default 30) as the **window** argument. Bucket width then tracks the env (30 ≠ the pinned `L1_WINDOW_SECONDS = 60`, `model/audit.rs:185`), and a later env change re-keys every v5 `event_id` (duplicate outbox rows for the same span — F-F). Fix = §1.3.

---

## 1. F-A — L1 bucket-scan rewrite (index-friendly closure + watermark)

### 1.1 Equivalence validation

Let `W = window_secs`, `c = epoch(created_at)`, `n = epoch(clock_timestamp())`, `b(c) = floor(c/W)`, `bucket_end(c) = (b(c)+1)·W`. The landed closure predicate is

```
C(c):  (b(c)+1)·W ≤ n − W
```

**Theorem 1 (tight equivalence).** `C(c) ⟺ c < B(n)` where `B(n) = floor((n−W)/W)·W`.

*Proof.* `C(c) ⟺ b(c)+1 ≤ (n−W)/W ⟺ b(c) ≤ (n−W)/W − 1`. Write `n−W = B·W + f`, `B = floor((n−W)/W)`, `f ∈ [0,W)`. Then `(n−W)/W − 1 = B + f/W − 1 ∈ [B−1, B)`, and since `b(c)` is an integer, `b(c) ≤ B−1 ⟺ b(c) < B ⟺ c < B·W = B(n)`. ∎

The closed set is a **time-axis prefix** `(−∞, B(n))` — monotone in `c`, hence range-scanable on a `created_at` btree.

**Theorem 2 (loose bound is a safe superset).** `L(c): c < n − W` satisfies `C ⟹ L` (because `c < bucket_end(c) ≤ n−W`), so `L ∧ C ≡ C`. `L` is index-sargable on 0243 (pure column comparison, RHS constant per execution). The loose bound over-scans **at most one bucket's rows** — those in `[B(n), n−W)`, all in bucket `B(n)`, all rejected by the `C` filter.

**Theorem 3 (watermark bound).** With a per-instance watermark `Wm = (start of the first bucket not yet aggregated) = (max closed-bucket start at the previous tick) + W`, the scan

```
created_at >= Wm AND created_at < clock_timestamp() − W AND C
```

is exact (`C` is the only row selector; the two range terms only bound the index walk) and per-tick cost = **O(rows in [Wm, n−W))** — i.e. O(rows-since-last-tick + ≤1 still-open bucket), independent of table size.

*Completeness.* Every bucket newly closed between tick `τ'` and `τ` has start `≥ (floor((τ'−2W)/W)+1)·W = max_closed_start(τ') + W = Wm(τ)` (closed at `τ'` ⟺ `bucket_end ≤ τ'−W` ⟺ index `≤ floor((τ'−2W)/W)`), so its rows (`c ≥ bucket_start ≥ Wm(τ)`) are inside the range; buckets closed at `τ'` were aggregated at `τ'` by induction. Holds for any tick cadence `T` (no `T ≤ W` requirement). Rows missed only when app-clock lag lands them in an **already-aggregated** bucket — the pre-existing F-C clock-skew bound, unchanged by the watermark (pin with a future-dated row test; merge-arm mitigation remains out of scope).

*Index-scan caveat (documented, not a defect):* `clock_timestamp()` is volatile, so the executor cannot early-stop the scan at the upper bound — the effective index walk is `[Wm, +∞)`, i.e. rows since the last tick plus the last `W` seconds of rows (⊆ rows-since-last-tick when `T ≤ W`). The **parameter** lower bound `created_at >= $2` is what gives the planner a real selectivity estimate; with only volatile bounds it may choose Seq Scan. Per-tuple evaluation of the volatile qual is negligible vs. row fetch.

### 1.2 Normative rewritten query (replaces outbox.rs:420-431)

```sql
-- $1 = window_secs (must be L1_WINDOW_SECONDS = 60, not the tick env), $2 = watermark (epoch, 0 on first tick)
SELECT floor(extract(epoch FROM created_at) / $1)::bigint * $1 AS bucket_start,
       COUNT(*)::bigint AS n
  FROM login_failures
 WHERE created_at >= to_timestamp($2)                                -- watermark: O(rows-since-last-tick)
   AND created_at < clock_timestamp() - make_interval(secs => $1)    -- loose range bound: index scan on 0243
   AND (floor(extract(epoch FROM created_at) / $1)::bigint + 1) * $1 -- closure filter: exact row selector (Thm 1/2)
       <= extract(epoch FROM clock_timestamp()) - $1
 GROUP BY bucket_start
 ORDER BY bucket_start
```

Semantics identical to the landed query (same closed-bucket set, same per-bucket counts, same v5 ids); the range terms are provably (Thm 2/3) transparent. Plan: Index Range Scan on `login_failures_created_at_idx` (0243) + Hash Aggregate over the scanned range.

### 1.3 Watermark state + timer fixes (must land with the rewrite)

- **State**: in-memory `Option<i64>` in the L1 timer task (`background.rs`), `None` → first tick binds `$2 = 0` (one-time full retained-window backfill — the D11 self-heal; bounded by retention, never repeated). Advance after each tick: `Wm = max(bucket_start returned by the SELECT) + W`, **computed from the scan result, not from `inserted`** (multi-instance: instance B's dedup'd scan still advances its own watermark; `ON CONFLICT DO NOTHING` keeps rows exact).
- **No new table, no new migration**: correctness never depends on the watermark (re-scan + dedup is at-least-once); the arbiter stays 246.
- **Window pin**: `background.rs:337` must call `aggregate_login_failure_buckets(L1_WINDOW_SECONDS)` and keep the env purely as tick cadence. Also fix the comment at `outbox.rs:417` and migration 0243's header ("Serves the L1 bucket scan" becomes true only after §1.2; it already serves the retention sweep).
- **Tripwire metric (new, mandatory)**: histogram `aero_l1_auth_aggregate_duration_seconds` around the tick call (first-ever timing metric for this timer; the slice already adds 2 metrics — this is the F-A acceptance instrument). Warn log when a single tick > 2 s.

### 1.4 EXPLAIN-gated regression test (spec — land in `db_tests/l1_auth.rs`)

`l1_aggregate_scan_is_index_bounded` (`#[ignore = "requires live Postgres"]`):

1. **Seed** one `INSERT … SELECT generate_series` batch: 190,000 rows spread over the 180-day retention window (`created_at = now() − (i % 180) days − …`) + 10,000 rows in the last 5 minutes (recent burst). No `reset_governance_table` needed beyond the module pattern.
2. **Plan gate** (the regression's red condition):
   ```sql
   EXPLAIN (COSTS OFF) SELECT floor(extract(epoch FROM created_at)/60)::bigint*60 AS bucket_start, COUNT(*)::bigint
     FROM login_failures
    WHERE created_at >= to_timestamp($2) AND created_at < clock_timestamp() - make_interval(secs => 60)
      AND (floor(extract(epoch FROM created_at)/60)::bigint + 1)*60 <= extract(epoch FROM clock_timestamp()) - 60
    GROUP BY bucket_start;
   ```
   with `$2 = now − 120 s`. Assert, on the EXPLAIN text: contains `Index Scan using login_failures_created_at_idx` (or `Index Only Scan … login_failures_created_at_idx`); **does not contain** `Seq Scan on login_failures`. This is a genuine planner gate — no `enable_seqscan=off` (that would prove nothing). On the seeded table the parameter lower bound gives the planner real selectivity and the index wins deterministically; the gate reds if a future edit wraps `created_at` in a function again.
3. **Bounded per-tick cost gate**: `EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF)` on the same query — assert the top node's `actual rows ≤ 12,000` (10k recent burst + one closed bucket + slack) — i.e. the 190k old rows are **not** walked. This is the "bounded per-tick cost" acceptance: the old query on this table reports `actual rows ≈ 200,000` (Seq Scan) — the contrast is the regression.
4. **Retention-sweep gate** (0243's second consumer): `EXPLAIN DELETE FROM login_failures WHERE created_at < now() - interval '180 days'` on the seeded table → contains `Index Scan using login_failures_created_at_idx`, no Seq Scan.
5. **Watermark-path behavior**: after a first tick with `$2=0` (backfill, count = number of closed buckets) and a second tick with `$2 = backfill_max_bucket + 60`, assert the second scan's actual rows ≤ 1,000 (only the newly-closed bucket) and `inserted = 0` (idempotent).
6. **Skew-bound pin** (F-C): seed one row future-dated into an already-aggregated bucket → rerun → count unchanged (documents the skew bound; no merge arm).

### 1.5 Acceptance metrics and thresholds (F-A)

| # | Metric / gate | Acceptance value |
|---|---|---|
| A1 | EXPLAIN plan gate (§1.4.2) | Index/Index-Only Scan on 0243; **no Seq Scan** — CI-red on regression |
| A2 | Bounded per-tick cost (§1.4.3) | actual rows ≤ 12,000 on the 200k-row fixture (old query: 200,000) |
| A3 | Tick duration (new histogram) | p95 ≤ 500 ms over a 1 h run under 200 logins/s load; tripwire warn > 2 s/tick |
| A4 | Backfill bound | first tick after boot on the 200k-row fixture ≤ 30 s (one-time; retention-bounded) |
| A5 | Retention sweep plan | Index Scan on 0243 (before the migration: Seq Scan) |
| A6 | Correctness (unchanged semantics) | `login_failure_l1_aggregation_n_to_one` passes verbatim against the rewritten query (same closed-bucket set, same counts, same v5 ids) |

---

## 2. F-D — auth hot-path cost of the standalone pair tx

### 2.1 Measured inventory (all statements verified in code)

**Successful login with active TOTP (worst case) — pool round trips:**

| # | Step | Statements | RTs |
|---|---|---|---|
| 1 | `AuthService::login` → `login_inner` (`aero-auth/service.rs:386`) | `find_credentials_by_email` + `get` | 2 |
| 2 | 2FA gate (`handlers/auth.rs:150-199`) | `is_activated` + `get_secret` | 2 |
| 3 | `record_session` → `record_with_id` (`auth_session.rs:165`) | BEGIN + SELECT…FOR UPDATE + INSERT ON CONFLICT + COMMIT | 4 |
| 4 | **NEW `record_pair_standalone`** (`outbox.rs:312`) | BEGIN + audit INSERT (5 AFTER triggers: 0236/0239/0242/0245/0246, 0236 does a runtime-singleton SELECT + optional binding lookup) + outbox INSERT (PK + 2 partial btree entries + ~1 KB jsonb envelope) + COMMIT | **4** |
| 5 | `record_login_event` (`handlers/auth.rs:35`) | `is_known_ip` + `has_any` + `append` (+ `auth.login.new_ip` AuditRepo append when new IP) | 3–4 |
| | **Total** | | **≈ 15–17 (+4 vs baseline ≈ +30%)** |

**Refresh** (`aero-auth/service.rs:413`): `is_active` (1 RT) + **NEW pair (4 RTs)** = 5 vs 1 → **+400%**. Refresh tokens are reused (no rotation) — every client refresh mints a pair.

**Per-pair server-side cost**: BEGIN/COMMIT ≈ 0.05–0.3 ms each; audit INSERT with the 5-trigger surface ≈ 0.5–2 ms; outbox INSERT ≈ 0.3–0.8 ms → **pair ≈ 1.5–4 ms + 4 RTs** (0.4–1 ms each on localhost/cloud).

**Pool arithmetic**: 16 conns (`config.example.toml:15`), `acquire_timeout` 30 s, `statement_timeout` 10 s (`db.rs:16/31`). Per-login conn-hold ≈ 6–17 ms → pool saturation ≈ 900–1,600 logins/s theoretical. The login path is **CPU-bound before the pool**: Argon2id `Argon2::default()` verify (`password.rs:17`, m=19 MiB, t=2, p=1) ≈ 20–50 ms → ~20–50 logins/s/core. Below ≈ 150 logins/s the pair is noise on the pool; contention risk materializes only in multi-core bursts ≥ 300/s and in refresh churn (10k clients × refresh/5 min ≈ 33/s — noise). The acceptance therefore gates the **burst** case, not the mean.

### 2.2 Acceptance measurement (what exists vs what to add)

- **Exists**: `aero_http_request_duration_seconds` histogram labeled by matched **route** (`metrics.rs:480` — per-route p95/p99 for `/api/auth/login`, `/api/auth/refresh`); `aero_db_pool_in_use` + `aero_db_pool_size` (15 s sampler, `metrics_tasks.rs:17-41`).
- **Add (mandatory)**: histogram `aero_audit_pair_tx_seconds` around `record_pair_standalone`'s begin→commit (includes pool-acquire wait). This is the F-D-specific instrument and the mitigation trigger source.
- **Queue depth**: sqlx 0.8.6 `PgPool` exposes only `size()`/`num_idle()` (verified in the vendored `sqlx-core-0.8.6/src/pool/mod.rs:535/540` — **no `num_waiting`**). A direct queue-depth gauge is not implementable without a pool fork; the accepted proxy is (a) `aero_db_pool_in_use` saturation samples, (b) pair-tx latency (includes wait), (c) client-side p95 from the load leg. Do not block the landing on a queue-depth gauge.
- Note the 15 s sampler cadence under-samples bursts — treat it as a saturation **proxy**, never the primary gate (the load leg's client-side latency is primary).

### 2.3 Thresholds (16-conn pool, load-leg profile of §4)

| Level | Condition | Threshold |
|---|---|---|
| **GREEN** (acceptance, §4 pass criteria) | p95 login (client-side) | ≤ 300 ms at ≥ 200 logins/s sustained 120 s |
| | p99 login | ≤ 600 ms |
| | p95 refresh | ≤ 150 ms at ≥ 100 refreshes/s |
| | `aero_db_pool_in_use` | mean ≤ 12/16 over the run |
| | `aero_audit_pair_tx_seconds` p95 | ≤ 20 ms |
| | pair share of login p95 | < 25% (pair is not the dominant cost) |
| **T1 — trigger window-row merge** (documented 1:1 carve-out for `auth.login` only) | p95 login > 500 ms for ≥ 2 consecutive 60 s windows at ≥ 200 logins/s, **or** pair-tx p95 > 50 ms | mitigate |
| **T2 — trigger dedicated pool** (e.g. 4-conn pool for `record_pair_standalone`) | p95 login > 1 s, **or** `aero_db_pool_in_use` = 16 in ≥ 3 consecutive 15 s samples while pair-tx p95 > 100 ms | mitigate |
| Refresh-only trigger | p95 refresh > 300 ms sustained | apply T1/T2 to the refresh write point |

Mitigations are ops decisions; the acceptance requires the thresholds to be **monitorable** (metrics exist), not the mitigations to be pre-implemented. T1's parity carve-out must be flagged in the 1:1 acceptance (AC-2) as an authorized deviation if ever enabled.

---

## 3. F-B — claim-loop cost growth + terminal-row sweep

### 3.1 Is 0240 sufficient as the table grows? — Yes for claim cost; the caveats

Verified claim query (`aero-audit-connector/src/pg.rs:98-184`): arm A = top `limit−K` of `(priority DESC, available_at, created_at, event_id)` with `FOR UPDATE SKIP LOCKED` + `MATERIALIZED`; arm B = `K` rows of the `MIN(priority)` lane with `NOT EXISTS` arm-A exclusion.

- **Arm A**: 0240's partial index `(priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)` matches the ORDER BY exactly → LIMIT pushdown, no per-tick Sort, only LIMIT rows locked. Per-tick cost = O(limit + skipped prefix). Worst case (sink outage: every pending row parked with `available_at` in the future via `audit_backoff`, exponential 1 s→300 s, `relay.rs:61`) the executor walks the whole **partial** index per tick — O(pending), **not** O(table).
- **Arm B MIN(priority)**: served by the same partial index (leading priority column, backward scan); executor stops at the first due row. Worst case O(min-priority prefix) when that lane is entirely parked.
- **Terminal rows (status 2/3) are excluded from the partial index** → claim-loop cost is **independent of terminal-row growth**. F-B's sweep is an **operational** requirement (pg_dump/backup, autovacuum, bloat, dead-row ops review), **not** a claim-latency requirement.
- Pending-set bound: moderation ≤ 300/60 s (budget), auth pairs = login+refresh rate, L1 = 1 row/60 s — pending is bounded by producer rates, and the D-CAP floor (K = `min(max(1,b/20), b−1)`, 25 at batch 500) bounds low-lane starvation.

**Answer to "is 0240 sufficient as the table grows"**: yes — claim cost depends on the pending set, which is producer-bounded; terminal growth is invisible to it. The sweep is required at a **size threshold driven by operational cost**, and the arithmetic makes it urgent at auth scale: delivered rows accrue at the enqueue rate (at 30 logins/s ≈ 2.6 M rows/day at ~1.2–1.5 KB/row ≈ 3.5 GB/day). The sweep must therefore be a **routine bounded batch** (mirror `AiJobRepo::sweep_terminal_before`, `retention.rs:347`), not a fire-drill.

### 3.2 Terminal-row sweep spec + thresholds

- **Sweep DML** (bounded batch, 1,000 rows/tx, in the retention timer):
  - `DELETE FROM audit_governance_outbox WHERE status = 2 AND delivered_at < now() − interval '7 days' LIMIT 1000` (delivered rows are a pure cursor after the sink's POST ack — `settle` is last, `pg.rs:120-155`; the sink's Idempotency-Key = event_id makes re-delivery safe ⇒ loss-free; 7-day grace covers sink downtime + review).
  - `DELETE FROM audit_governance_outbox WHERE status = 3 AND created_at < now() − interval '90 days' LIMIT 1000` — dead rows carry no `delivered_at` (verified `mark_dead`, `pg.rs:263-281`); `created_at` = enqueue time (conservative), 90-day ops review window. Keep never-swept dead rows shorter-lived than DLQ unreplayed (below).
- **Required index (new migration 0247, arbiter 246→247 same commit)**: `CREATE INDEX IF NOT EXISTS audit_governance_terminal_sweep_idx ON audit_governance_outbox (status, delivered_at) WHERE status IN (2,3);` — serves the status-2 range; the status-3 slice is a small index prefix + `created_at` filter (dead rows are rare).
- **Alert thresholds** (sampler, 30 s cadence — reuse the AI-DLQ pattern at `metrics_tasks.rs:142`):
  - `pg_total_relation_size('audit_governance_outbox')` > **512 MB** (≈ 350–450 k rows) → **WARN** (sweep lagging / not landed); > **1 GB** → **ERROR**.
  - Dead rows (status 3) > **10 k** → WARN (poison producer).
- **DLQ (0244) policy**: `DELETE FROM audit_governance_failed_pairs WHERE replayed_at IS NOT NULL AND replayed_at < now() − interval '30 days'` (bounded batch); never-replayed rows are kept and alerted — extend the 30 s sampler to warn when `max(created_at)` of unreplayed rows > **7 days** (standing-alert semantics; keep the existing `aero_audit_governance_failed_pairs` gauge). DLQ is small by construction (fail-open compensation only) — seq-scan DELETE acceptable, no index needed.
- **Claim-loop regression gate** (new db_test, mirrors §1.4): seed 1,000,000 terminal rows (status 2, `delivered_at` spread over 90 days) + 10,000 pending rows → `EXPLAIN` the claim CTE → contains `Index Scan using audit_governance_due_prio_idx`, **no `Seq Scan`** on `audit_governance_outbox`, no top-level `Sort` node (LIMIT pushdown preserved). Per-tick claim time acceptance: p95 ≤ 50 ms at 10 k pending, poll 5 s.
- **Pre-existing sibling gap (flag, out of slice)**: v1 `snaplink_delivery_outbox` has no terminal sweep (verified) — carry into the follow-up.

---

## 4. Load/bench leg for the harness

New script `scripts/bench-auth-b5-1.sh` (scripts/ is the sanctioned home; NOT a `test-integration.sh` B5 segment — it needs a running server, and CI has no load suites). Runbook per AGENTS.md §4.3: fresh throwaway DB → `cargo build` → `aero-cli migrate` → server foreground/`setsid` on `:3030` with `AERO__SERVER__BLOB_DIR=/tmp/aero/blobs` etc. → leg → `pkill aero-server` → `DROP DATABASE`. Raise `AERO_AUTH_RATE_LIMIT_PER_SEC`/`AERO_RATE_LIMIT_PER_SEC` for the run (single-underscore env, §4.3).

**Phases** (all against one fresh DB; totals ~5 min):

1. **Seed**: 1,000 participants + credentials with a precomputed Argon2id PHC hash (reuse a fixture hash from `aero-auth` tests via direct SQL — one INSERT batch). TOTP inactive (worst-case 2FA leg optional: repeat burst with 100 activated).
2. **Warmup**: 30 s at 50 logins/s (verify pipeline; discard).
3. **Login burst + concurrent aggregator** (the F-D/F-A combined leg): 120 s at **200 logins/s** (24,000 attempts), 64-way concurrency (`xargs -P 64 curl -w '%{time_total}'`), **85% correct / 15% wrong passwords** (wrong-password path exercises `LoginFailureRepo::record` + `auth.login.failed` audit + the L1 aggregator under load). L1 timer running (`AERO__SERVER__LOGIN_FAILURE_L1_AGGREGATE_SECS=30`); per-tick duration captured from the new histogram + log lines.
4. **Refresh leg**: 60 s at 100 refreshes/s using issued refresh tokens.
5. **Post-run DB assertions** (pass criteria):
   - **Correctness**: `audit_governance_failed_pairs` = 0 rows; slice-token pairs 1:1 (`audit_events.action IN ('auth.login','auth.refresh')` LEFT JOIN outbox on `event_id` → 0 NULLs, 0 dupes); exactly 1 outbox row per closed L1 bucket (`payload->>'aggregated'='true'`, grouped by `window_start_epoch` → count = 1); aggregator rerun inserts 0; 0 HTTP 500/429.
   - **Performance**: p95 login ≤ **300 ms**, p99 ≤ **600 ms**; p95 refresh ≤ **150 ms**; throughput ≥ 200 logins/s sustained (measured, no dropped requests); `aero_db_pool_in_use` mean ≤ 12/16, no ≥ 3 consecutive 15 s samples at 16/16; `aero_audit_pair_tx_seconds` p95 ≤ **20 ms** and pair share < 25% of login p95; `aero_l1_auth_aggregate_duration_seconds` p95 ≤ **500 ms**.
   - **Bounded-cost gate on the big table** (F-A §1.4): after the burst, re-run the §1.4 seeded-fixture EXPLAIN gates — or, if the fixture is skipped in the bench, rely on the db_test (bench tables are small; the fixture is the authority).
6. **A/B note**: no disable knob exists for the pair (R6: unconditionally enqueued — no env gate by design), so baseline A/B is not available without a test knob; the acceptance uses absolute thresholds + pair-share instead. Do not add a disable knob to the product path.

**Trigger summary** (single table, for the landing's acceptance section): GREEN = login p95 ≤ 300 ms / refresh p95 ≤ 150 ms / pair-tx p95 ≤ 20 ms / pool mean ≤ 12/16 / L1 tick p95 ≤ 500 ms / 0 DLQ rows / parity 1:1 / rerun idempotent; **T1** (window-row merge) at login p95 > 500 ms × 2 consecutive windows or pair-tx p95 > 50 ms; **T2** (dedicated pool) at login p95 > 1 s or pool saturated × 3 samples with pair-tx p95 > 100 ms; **F-B alerts** at outbox > 512 MB (WARN) / > 1 GB (ERROR) / dead > 10 k (WARN) / DLQ unreplayed age > 7 d (ERROR); **sweep routine** always-on bounded batches (7 d delivered / 90 d dead / 30 d replayed DLQ).
