//! B5-4 `audit-provision-check` — psql-backed fail-closed provisioning gate.
//!
//! Verifies the audit relay's scope provisioning contract straight from the
//! database, which is the enforcement source of truth per the 0235 DDL
//! (`snaplink_commercial_runtime.enabled`). The relay-health predicate reads
//! the DB switch and enabled bindings — deliberately **not** the in-process
//! `ready()`. The v1 outbox buckets derive pending/claimed/delivered from
//! `delivered_at` / `claim_token` (mirroring `pending_count()`). When the
//! B5-1 0239 status-machine table exists, the four status buckets 0/1/2/3
//! are reported with dead rows as a terminal fail-closed state — never
//! folded into delivered.
//!
//! Design: `docs/design/2026-08-07-aero-cli-b5-4-audit-provision-check-psql.design.md`.

use crate::outcome::Outcome;
use aero_common::model::audit::OutboxStatus;
use std::fmt::Write as _;
use std::io::Write;

/// Per-query timeout; psql hanging on a network black hole must fail the
/// check (never hang the gate).
const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The B5-1 0239 table-name candidates, in probe order: the T-11 drill /
/// harness-pinned name first, then the aero-bus B5-1 design name. The single
/// point to extend if B5-1 lands a third name.
const G0239_CANDIDATES: [&str; 2] = ["audit_governance_outbox", "audit_outbox"];

// -- SQL contract (each query runs as its own psql -At -v ON_ERROR_STOP=1) --

/// Q0 — relay-health predicate: DB `runtime.enabled` switch + enabled
/// bindings count. Deliberately not `ready()` (entitlement projection
/// semantics, not a provisioning criterion).
const Q0_SQL: &str = "SELECT runtime.enabled, (SELECT count(*) FROM snaplink_commercial_bindings WHERE enabled) FROM snaplink_commercial_runtime runtime WHERE runtime.singleton";

/// Q1 — v1 derived buckets for the audit destination (pending/claimed/
/// delivered). Undelivered = `delivered_at IS NULL` (matches the repo's own
/// `pending_count()`); `destination='audit'` filter — usage is an independent
/// relay path.
const Q1_SQL: &str = "SELECT count(*) FILTER (WHERE delivered_at IS NULL AND claim_token IS NULL), count(*) FILTER (WHERE claim_token IS NOT NULL), count(*) FILTER (WHERE delivered_at IS NOT NULL) FROM snaplink_delivery_outbox WHERE destination = 'audit'";

/// Q2 — 0239 table-name candidate probe (drift single point).
const Q2_SQL: &str =
    "SELECT to_regclass('audit_governance_outbox')::text, to_regclass('audit_outbox')::text";

/// Q3 — 0239 four status buckets (0=pending, 1=claimed, 2=delivered,
/// 3=dead). `{table}` is only ever one of the fixed [`G0239_CANDIDATES`]
/// literals (resolved by [`parse_probe_line`]); never user input.
const Q3_SQL: &str = "SELECT status, count(*) FROM {table} GROUP BY status ORDER BY status";

/// Q4 — oldest pending row age in seconds (empty result = no pending rows).
/// `pub` (B5-4): the aero-server sampler's `oldest_pending_secs` query is a
/// text mirror of this — the parity is load-bearing (CLI/sampler oracle
/// agreement) and pinned by the server-side unit test.
pub const Q4_SQL: &str = "SELECT extract(epoch FROM (clock_timestamp() - min(available_at)))::bigint FROM {table} WHERE status = 0";

/// Q5 — dead-row detail (only run when dead > 0), bounded to 5 rows.
const Q5_SQL: &str = "SELECT event_id::text, left(coalesce(last_error, ''), 120) FROM {table} WHERE status = 3 ORDER BY available_at LIMIT 5";

/// P — priority/class column probe (B5-3 CLI surface; mirrors the drill
/// bin's runtime capability gate, aero-audit-priority-drill.rs:105-130).
/// Two fixed literals; the runner is a single `psql -At -c` — no
/// parameter-binding channel, no user input reaches the SQL. Absent table →
/// `f|f` (`information_schema` reports no rows for a missing table).
const P_SQL: &str = r"SELECT EXISTS (
    SELECT 1 FROM information_schema.columns
     WHERE table_name = 'audit_governance_outbox' AND column_name = 'priority'
), EXISTS (
    SELECT 1 FROM information_schema.columns
     WHERE table_name = 'audit_governance_outbox' AND column_name = 'class'
)";

/// D8′ destructive-gate count (design: priority-drill-destructive-gate) — the
/// wrapper's fast-fail probe: a non-empty outbox would be silently
/// TRUNCATE'd by the drill. Fixed literal (third hardcoded site for the
/// table name, same pattern as `P_SQL`); no user input reaches the SQL. The
/// authoritative gate is the drill's own in-transaction LOCK+COUNT+TRUNCATE
/// (aero-audit-priority-drill.rs) — this probe only avoids a cold `cargo
/// run` compile before discovering the refusal.
const COUNT_SQL: &str = "SELECT COUNT(*)::bigint FROM audit_governance_outbox";

/// v1 outbox counts derived from `snaplink_delivery_outbox` (0235 shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V1OutboxCounts {
    pub pending: i64,
    pub claimed: i64,
    pub delivered: i64,
}

/// 0239 status-machine buckets (B5-1), present only when a candidate table
/// was probed. Dead rows are a separate terminal bucket: dead is **never**
/// folded into delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct G0239Counts {
    /// Resolved candidate name (fixed literal from [`G0239_CANDIDATES`]).
    pub table: Option<&'static str>,
    pub pending: i64,
    pub claimed: i64,
    pub delivered: i64,
    pub dead: i64,
    /// Oldest pending row age in seconds (None = no pending rows).
    pub oldest_pending_secs: Option<i64>,
    /// Dead-row detail (`event_id`, `last_error`), ≤ 5 rows.
    pub dead_rows: Vec<(String, String)>,
}

/// One consistent snapshot of the provisioning state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditSnapshot {
    pub relay_enabled: bool,
    pub enabled_bindings: i64,
    pub v1: V1OutboxCounts,
    pub g0239: Option<G0239Counts>,
    /// 0239 `priority` column landed (B5-3 CLI surface verdict line).
    pub priority_landed: bool,
    /// 0239 `class` column landed (B5-3 CLI surface verdict line).
    pub class_landed: bool,
}

/// Three-state verdict matrix (dead priority first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Fail-closed: exit 1. Carries the actionable reason.
    FailClosed(String),
    /// Relay disabled with zero undelivered rows: consistent, exit 0.
    Consistent,
    /// Relay enabled (bindings > 0) and no dead rows: healthy, exit 0.
    Healthy,
}

/// 3-state verdict matrix, dead priority:
/// 1. any dead row (relay on or off) → fail-closed — dead is terminal;
/// 2. relay unhealthy ∧ any undelivered row (v1 pending+claimed, or 0239
///    pending+claimed) → fail-closed — no grant may have been issued;
/// 3. relay unhealthy ∧ zero undelivered → consistent;
/// 4. relay healthy ∧ dead=0 → healthy (pending backlog is normal).
pub fn verdict(s: &AuditSnapshot) -> Verdict {
    if let Some(g) = &s.g0239 {
        if g.dead > 0 {
            return Verdict::FailClosed(format!(
                "{} dead row(s): the audit:event:write grant was refused (403/provisioning); dead is never counted delivered",
                g.dead
            ));
        }
    }
    let undelivered =
        s.v1.pending + s.v1.claimed + s.g0239.as_ref().map_or(0, |g| g.pending + g.claimed);
    if !(s.relay_enabled && s.enabled_bindings > 0) {
        if undelivered > 0 {
            return Verdict::FailClosed(format!(
                "relay disabled (bindings={}) with {undelivered} undelivered audit row(s); no audit:event:write grant issued",
                s.enabled_bindings
            ));
        }
        return Verdict::Consistent;
    }
    Verdict::Healthy
}

/// Greppable `audit-provision-check:` report lines (the harness leg grep
/// surface).
pub fn format_report(s: &AuditSnapshot, v: &Verdict) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "audit-provision-check: relay: enabled={} bindings={}",
        s.relay_enabled, s.enabled_bindings
    );
    let _ = writeln!(
        out,
        "audit-provision-check: v1-outbox: pending={} claimed={} delivered={}",
        s.v1.pending, s.v1.claimed, s.v1.delivered
    );
    match &s.g0239 {
        Some(g) => {
            let _ = writeln!(
                out,
                "audit-provision-check: outbox-0239: table={} pending={} claimed={} delivered={} dead={}",
                g.table.unwrap_or("?"),
                g.pending,
                g.claimed,
                g.delivered,
                g.dead
            );
            match g.oldest_pending_secs {
                Some(secs) => {
                    let _ = writeln!(out, "audit-provision-check: oldest-pending-age: {secs}s");
                }
                None => {
                    let _ = writeln!(out, "audit-provision-check: oldest-pending-age: n/a");
                }
            }
            for (event_id, last_error) in &g.dead_rows {
                let _ = writeln!(out, "audit-provision-check: dead: {event_id} {last_error}");
            }
        }
        None => {
            let _ = writeln!(out, "audit-provision-check: outbox-0239: not migrated");
        }
    }
    let _ = writeln!(
        out,
        "audit-provision-check: priority: {}",
        if s.priority_landed {
            "landed"
        } else {
            "absent"
        }
    );
    let _ = writeln!(
        out,
        "audit-provision-check: class: {}",
        if s.class_landed { "landed" } else { "absent" }
    );
    let verdict_line = match v {
        Verdict::FailClosed(reason) => format!("verdict: fail-closed — {reason}"),
        Verdict::Consistent => {
            "verdict: consistent — relay disabled, zero undelivered audit rows".to_string()
        }
        Verdict::Healthy => {
            format!(
                "verdict: healthy — relay enabled ({} bindings)",
                s.enabled_bindings
            )
        }
    };
    let _ = writeln!(out, "audit-provision-check: {verdict_line}");
    out
}

/// Parse a psql `-At` boolean cell ("t"/"f").
pub fn parse_psql_bool_line(line: &str) -> Result<bool, String> {
    match line.trim() {
        "t" | "true" | "T" | "TRUE" => Ok(true),
        "f" | "false" | "F" | "FALSE" => Ok(false),
        other => Err(format!("expected psql boolean cell, got {other:?}")),
    }
}

/// Parse the two-cell psql boolean line ("t|f") → (`priority_landed`,
/// `class_landed`). Reuses [`parse_psql_bool_line`]; exactly two cells.
pub fn parse_priority_probe(line: &str) -> Result<(bool, bool), String> {
    let mut parts = line.trim().split('|');
    let priority = parts
        .next()
        .ok_or_else(|| format!("empty priority probe line: {line:?}"))
        .and_then(parse_psql_bool_line)?;
    let class = parts
        .next()
        .ok_or_else(|| format!("priority probe line missing class cell: {line:?}"))
        .and_then(parse_psql_bool_line)?;
    if parts.next().is_some() {
        return Err(format!(
            "priority probe line has more than two cells: {line:?}"
        ));
    }
    Ok((priority, class))
}

/// Parse a psql `-At` single-value count (`COUNT(*)::bigint` never yields
/// empty output on success — "0" minimum; empty/garbage output means psql
/// failed and must fail the check, never pass it).
pub fn parse_outbox_count(out: &str) -> Result<i64, String> {
    out.trim()
        .parse::<i64>()
        .map_err(|e| format!("invalid outbox count {out:?}: {e}"))
}

/// D8′ gate predicate: a non-empty outbox blocks the drill unless the opt-in
/// env is exactly `"1"` (fail-closed env semantics — `"true"`/`"yes"`/absent
/// all refuse). Pure, unit-testable.
pub fn priority_drill_blocked(count: i64, allow_truncate: bool) -> bool {
    count > 0 && !allow_truncate
}

/// Parse the Q0 relay line `t|2` → (enabled, enabled bindings).
pub fn parse_relay_line(line: &str) -> Result<(bool, i64), String> {
    let mut parts = line.trim().split('|');
    let enabled = parts
        .next()
        .ok_or_else(|| format!("empty relay line: {line:?}"))
        .and_then(parse_psql_bool_line)?;
    let bindings = parts
        .next()
        .ok_or_else(|| format!("relay line missing bindings: {line:?}"))?
        .trim()
        .parse::<i64>()
        .map_err(|e| format!("invalid bindings count in {line:?}: {e}"))?;
    Ok((enabled, bindings))
}

/// Parse the Q2 probe line into a known candidate table name. The first
/// non-empty cell wins; unknown names resolve to `None` (treated as not
/// migrated — fail-closed direction never fabricates a table).
pub fn parse_probe_line(line: &str) -> Option<&'static str> {
    line.split('|')
        .map(str::trim)
        .find(|cell| !cell.is_empty())
        .and_then(|cell| G0239_CANDIDATES.iter().copied().find(|c| *c == cell))
}

/// Parse Q3-style `status|count` lines into the four buckets; missing
/// statuses stay 0. Unknown statuses are ignored (0239 CHECKs 0..3) — the
/// status vocabulary resolves through the leaf enum's fail-open `from_i32`
/// (a scanning tool tolerates future statuses; the wire serde path rejects
/// them).
pub fn parse_buckets(out: &str) -> [i64; 4] {
    let mut buckets = [0i64; 4];
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split('|');
        if let (Some(status), Some(count)) = (parts.next(), parts.next()) {
            if let (Ok(status), Ok(count)) =
                (status.trim().parse::<i32>(), count.trim().parse::<i64>())
            {
                if let Some(status) = OutboxStatus::from_i32(status) {
                    // `Some` guarantees 0..3 — enum→usize cast is in-range
                    // (and clippy-clean, unlike `as i32 as usize`).
                    buckets[status as usize] = count;
                }
            }
        }
    }
    buckets
}

/// Parse the Q1 v1 line `pending|claimed|delivered` (one row, three cells).
pub fn parse_v1_line(line: &str) -> Result<V1OutboxCounts, String> {
    let mut parts = line.trim().split('|');
    let pending = parts
        .next()
        .ok_or_else(|| format!("empty v1 outbox line: {line:?}"))?
        .trim()
        .parse::<i64>()
        .map_err(|e| format!("invalid v1 pending count in {line:?}: {e}"))?;
    let claimed = parts
        .next()
        .ok_or_else(|| format!("v1 line missing claimed count: {line:?}"))?
        .trim()
        .parse::<i64>()
        .map_err(|e| format!("invalid v1 claimed count in {line:?}: {e}"))?;
    let delivered = parts
        .next()
        .ok_or_else(|| format!("v1 line missing delivered count: {line:?}"))?
        .trim()
        .parse::<i64>()
        .map_err(|e| format!("invalid v1 delivered count in {line:?}: {e}"))?;
    Ok(V1OutboxCounts {
        pending,
        claimed,
        delivered,
    })
}

/// Parse the Q5 dead-detail lines `event_id|last_error` (error text may itself
/// contain `|`, hence `splitn`).
pub fn parse_dead_rows(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut parts = line.splitn(2, '|');
            match (parts.next(), parts.next()) {
                (Some(event_id), Some(last_error)) => {
                    Some((event_id.trim().to_string(), last_error.trim().to_string()))
                }
                _ => None,
            }
        })
        .collect()
}

/// Connection parameters parsed from a `postgres://user:pass@host:port/db`
/// URL (harness-compatible parsing; password travels via `PGPASSWORD`, never
/// argv).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnParams {
    pub user: String,
    pub password: String,
    pub host: String,
    pub port: String,
    pub db: String,
}

/// Parse a postgres connection URL. Host defaults to localhost, port to
/// 5432; query strings are stripped; user and database are required.
pub fn parse_db_url(url: &str) -> Result<ConnParams, String> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or_else(|| "database URL must start with postgres://".to_string())?;
    let rest = rest.split('?').next().unwrap_or(rest);
    let (auth, host_and_db) = match rest.rfind('@') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => ("", rest),
    };
    let (user, password) = match auth.split_once(':') {
        Some((u, p)) => (u.to_string(), p.to_string()),
        None => (auth.to_string(), String::new()),
    };
    let (host_port, db) = match host_and_db.split_once('/') {
        Some((h, d)) => (h, d.to_string()),
        None => (host_and_db, String::new()),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() => (h.to_string(), p.to_string()),
        _ => (
            if host_port.is_empty() {
                "localhost".to_string()
            } else {
                host_port.to_string()
            },
            "5432".to_string(),
        ),
    };
    if user.is_empty() {
        return Err("database URL must include a user".to_string());
    }
    if db.is_empty() {
        return Err("database URL must include a database name".to_string());
    }
    Ok(ConnParams {
        user,
        password,
        host,
        port,
        db,
    })
}

/// How the check launches psql (mirrors the harness's `PSQL_MODE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PsqlMode {
    Local,
    Container,
}

/// psql launcher with local/container dual mode and auto fallback.
struct PsqlRunner {
    conn: ConnParams,
    container: String,
    mode: PsqlMode,
}

impl PsqlRunner {
    fn new(conn: ConnParams) -> Self {
        let container = std::env::var("AERO_POSTGRES_CONTAINER")
            .unwrap_or_else(|_| "aero-postgres".to_string());
        let mode = match std::env::var("AERO_PSQL_MODE").as_deref() {
            Ok("local") => PsqlMode::Local,
            Ok("container") => PsqlMode::Container,
            _ => {
                if psql_available() {
                    PsqlMode::Local
                } else {
                    PsqlMode::Container
                }
            }
        };
        Self {
            conn,
            container,
            mode,
        }
    }

    /// Run one SQL statement; returns trimmed stdout on success.
    async fn query(&self, sql: &str) -> Result<String, String> {
        let mut cmd = if self.mode == PsqlMode::Container {
            let mut c = tokio::process::Command::new("docker");
            c.args([
                "exec",
                "-i",
                "-e",
                &format!("PGPASSWORD={}", self.conn.password),
                &self.container,
                "psql",
            ]);
            c
        } else {
            let mut c = tokio::process::Command::new("psql");
            c.env("PGPASSWORD", &self.conn.password);
            c
        };
        cmd.args([
            "-At",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            &self.conn.host,
            "-p",
            &self.conn.port,
            "-U",
            &self.conn.user,
            "-d",
            &self.conn.db,
            "-c",
            sql,
        ]);
        let result = tokio::time::timeout(QUERY_TIMEOUT, cmd.output()).await;
        match result {
            Ok(Ok(output)) if output.status.success() => {
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            }
            Ok(Ok(output)) => Err(format!(
                "psql failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
                "cannot launch psql{}: {e} — set AERO_PSQL_MODE=local or AERO_POSTGRES_CONTAINER=<name>",
                if self.mode == PsqlMode::Container {
                    format!(" via docker exec {}", self.container)
                } else {
                    String::new()
                }
            )),
            Ok(Err(e)) => Err(format!(
                "cannot launch psql{}: {e}",
                if self.mode == PsqlMode::Container {
                    format!(" via docker exec {}", self.container)
                } else {
                    String::new()
                }
            )),
            Err(_) => Err(format!(
                "psql query timed out after {QUERY_TIMEOUT:?}: {sql}"
            )),
        }
    }
}

/// Probe both 0239 columns via the runner. Table absent → (false, false)
/// from `information_schema` (never an error — the absent verdict is the
/// phase-1-window signal, see [`run_priority`]).
async fn probe_priority_columns(runner: &PsqlRunner) -> Result<(bool, bool), String> {
    let out = runner.query(P_SQL).await?;
    parse_priority_probe(&out)
}

fn psql_available() -> bool {
    std::process::Command::new("psql")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Execute the full check against the given database URL and return the
/// command outcome. Non-verdict paths (URL malformed, psql unavailable, DB
/// unreachable, schema missing, timeout) always return `Outcome::error`
/// (exit 1 — never 0); the verdict maps fail-closed → error, consistent /
/// healthy → ok. The greppable report is printed to stdout by this function.
pub async fn run(db_url: &str) -> Outcome {
    let outcome_error = |e: String| Outcome::error(format!("audit-provision-check: {e}"));
    let conn = match parse_db_url(db_url) {
        Ok(c) => c,
        Err(e) => return outcome_error(e),
    };
    let runner = PsqlRunner::new(conn);

    // Q0 — relay-health predicate (DB switch + enabled bindings).
    let q0 = match runner.query(Q0_SQL).await {
        Ok(o) => o,
        Err(e) => return outcome_error(e),
    };
    let (relay_enabled, enabled_bindings) = match parse_relay_line(&q0) {
        Ok(pair) => pair,
        Err(e) => return outcome_error(e),
    };

    // Q1 — v1 derived buckets (audit destination only).
    let q1 = match runner.query(Q1_SQL).await {
        Ok(o) => o,
        Err(e) => return outcome_error(e),
    };
    let v1 = match parse_v1_line(&q1) {
        Ok(v) => v,
        Err(e) => return outcome_error(e),
    };

    // Q2 — 0239 candidate probe (drift single point).
    let q2 = match runner.query(Q2_SQL).await {
        Ok(o) => o,
        Err(e) => return outcome_error(e),
    };
    let g0239 = match parse_probe_line(&q2) {
        Some(table) => {
            // Q3 — four status buckets; Q4 — oldest pending age; Q5 — dead
            // detail (only when dead > 0). `{table}` is a fixed candidate
            // literal resolved above; no user input reaches the SQL.
            let q3 = match runner.query(&Q3_SQL.replace("{table}", table)).await {
                Ok(o) => o,
                Err(e) => return outcome_error(e),
            };
            let buckets = parse_buckets(&q3);
            let q4 = match runner.query(&Q4_SQL.replace("{table}", table)).await {
                Ok(o) => o,
                Err(e) => return outcome_error(e),
            };
            let oldest_pending_secs = if q4.is_empty() {
                None
            } else {
                match q4.parse::<i64>() {
                    Ok(secs) => Some(secs),
                    Err(e) => {
                        return outcome_error(format!("invalid oldest-pending age {q4:?}: {e}"));
                    }
                }
            };
            let dead_rows = if buckets[3] > 0 {
                match runner.query(&Q5_SQL.replace("{table}", table)).await {
                    Ok(o) => parse_dead_rows(&o),
                    Err(e) => return outcome_error(e),
                }
            } else {
                Vec::new()
            };
            Some(G0239Counts {
                table: Some(table),
                pending: buckets[0],
                claimed: buckets[1],
                delivered: buckets[2],
                dead: buckets[3],
                oldest_pending_secs,
                dead_rows,
            })
        }
        None => None,
    };

    // P — 0239 priority/class column probe (B5-3 CLI surface; additive
    // verdict lines, always reported — absent table yields `f|f`).
    let (priority_landed, class_landed) = match probe_priority_columns(&runner).await {
        Ok(pair) => pair,
        Err(e) => return outcome_error(e),
    };

    let snapshot = AuditSnapshot {
        relay_enabled,
        enabled_bindings,
        v1,
        g0239,
        priority_landed,
        class_landed,
    };
    let v = verdict(&snapshot);
    let report = format_report(&snapshot, &v);
    print!("{report}");
    let _ = std::io::stdout().flush();
    match v {
        Verdict::FailClosed(reason) => {
            Outcome::error(format!("audit-provision-check: fail-closed — {reason}"))
        }
        Verdict::Consistent | Verdict::Healthy => Outcome::ok(""),
    }
}

/// `--priority` mode (B5-3 CLI surface): base check (full report + verdict)
/// → priority/class column gate → direct-spawn of the 500-backlog+1-
/// moderation drill bin with exit-code passthrough. Missing columns are a
/// SKIP (exit 2, never FAIL — phase-1 window); a drill that runs but fails
/// its assertions is a red FAIL (exit 1) — the honest G6 signal that B5-3
/// claim ordering has not landed. The spawn is direct (`tokio::process::
/// Command`), never `run_cmd`: `run_cmd` flattens distinct exit codes to 1
/// and the SKIP=2 contract depends on the raw code.
pub async fn run_priority(db_url: &str) -> Outcome {
    // 1. Base check first: DB unreachable / psql missing / URL bad / timeout
    //    fail before the drill spawns — a drill on a broken DB would only
    //    produce a misleading FAIL.
    let base = run(db_url).await;
    if base.is_error() {
        return base;
    }

    // 2. Column gate (re-probe: run()'s snapshot is internal state).
    let conn = match parse_db_url(db_url) {
        Ok(c) => c,
        Err(e) => return Outcome::error(format!("audit-provision-check: {e}")),
    };
    let runner = PsqlRunner::new(conn);
    let (priority_landed, class_landed) = match probe_priority_columns(&runner).await {
        Ok(pair) => pair,
        Err(e) => return Outcome::error(format!("audit-provision-check: {e}")),
    };
    if !(priority_landed && class_landed) {
        let reason = "priority/class columns not landed";
        println!("audit-provision-check: priority-drill: SKIP ({reason})");
        return Outcome::warning(
            2,
            format!("audit-provision-check: priority-drill: SKIP ({reason})"),
        );
    }

    // 2.5 — D8′ destructive gate (design: priority-drill-destructive-gate;
    //      revision of D8 "warning only"): the drill TRUNCATEs the outbox —
    //      a non-empty outbox would be silently destroyed. Refuse (exit 1,
    //      error semantics) unless AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1.
    //      Status 0/1 rows already fail-closed in base `run()`; this gate's
    //      marginal protection is delivered (status 2) history rows on a
    //      shared DB. Fast-fail UX layer only — the authoritative gate is
    //      the drill's in-transaction LOCK+COUNT+TRUNCATE (the drill is
    //      directly invocable; the wrapper cannot gate that path).
    let count_raw = match runner.query(COUNT_SQL).await {
        Ok(o) => o,
        Err(e) => return Outcome::error(format!("audit-provision-check: {e}")),
    };
    let count = match parse_outbox_count(&count_raw) {
        Ok(c) => c,
        Err(e) => return Outcome::error(format!("audit-provision-check: {e}")),
    };
    let allow_truncate = std::env::var("AERO_PRIORITY_DRILL_ALLOW_TRUNCATE").as_deref() == Ok("1");
    if priority_drill_blocked(count, allow_truncate) {
        let mut msg = format!(
            "audit-provision-check: priority-drill: REFUSED — audit_governance_outbox has \
{count} row(s); the drill TRUNCATEs the table. Re-run with \
AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 (throwaway DB only)"
        );
        // The double-underscore figment spelling (the codebase's dominant
        // config convention) is silently ignored here — detect it and hint,
        // so a user who "set the flag" learns why it did not take. Fail-
        // closed either way.
        if std::env::var("AERO__PRIORITY__DRILL__ALLOW_TRUNCATE").is_ok() {
            msg.push_str(
                " (note: AERO__PRIORITY__DRILL__ALLOW_TRUNCATE with double underscores \
is ignored — single underscore only)",
            );
        }
        println!("{msg}");
        return Outcome::error(msg);
    }

    // 3. Drill spawn — direct spawn with exit-code passthrough (0 PASS /
    //    1 FAIL / 2 drill-internal SKIP), stdout/stderr inherited so the
    //    drill's `drill: …: PASS` lines pass the harness greps verbatim.
    println!(
        "audit-provision-check: priority-drill: WARNING — the drill TRUNCATEs \
audit_governance_outbox; run against a throwaway DB only"
    );
    let mut cmd = if let Ok(bin) = std::env::var("AERO_PRIORITY_DRILL_BIN") {
        tokio::process::Command::new(bin)
    } else {
        let mut c = tokio::process::Command::new("cargo");
        c.args([
            "run",
            "--quiet",
            "-p",
            "aero-audit-connector",
            "--bin",
            "aero-audit-priority-drill",
        ]);
        c
    };
    // The drill reads only DATABASE_URL (no AERO__DATABASE__URL fallback):
    // inject the resolved URL explicitly regardless of which env the caller
    // used.
    cmd.env("DATABASE_URL", db_url)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let hint = if std::env::var("AERO_PRIORITY_DRILL_BIN").is_ok() {
                " (AERO_PRIORITY_DRILL_BIN must name an executable path — if you meant \
to allow the TRUNCATE, set AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1)"
            } else {
                ""
            };
            return Outcome::error(format!("cannot launch priority drill: {e}{hint}"));
        }
    };
    match tokio::time::timeout(std::time::Duration::from_secs(120), child.wait()).await {
        Ok(Ok(status)) => match status.code() {
            Some(0) => Outcome::ok(""),
            Some(1) => Outcome::error("priority drill: one or more assertions FAILED"),
            Some(2) => Outcome::warning(2, "priority drill: SKIP (runtime capability gate)"),
            _ => Outcome::error(format!("priority drill exited abnormally: {status}")),
        },
        Ok(Err(e)) => Outcome::error(format!("priority drill wait failed: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Outcome::error("priority drill timed out after 120s")
        }
    }
}
