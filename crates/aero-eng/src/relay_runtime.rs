//! Relay runtime-health checks: the Q7 stuck-lease signal and the unified
//! `audit-provision-check --relay` probe gate.
//!
//! This module is intentionally split from `audit_provision` so the existing
//! psql-backed command remains below the repository's file-size warning
//! threshold.  It owns only pure probe-contract helpers and the process
//! orchestration; the database snapshot still comes from the base check.

use crate::outcome::Outcome;
use std::process::Stdio;

/// Lease upper bound cloned from `aero-audit-connector::relay`.
///
/// `aero-eng` must not depend on the connector crate (the dependency audit
/// intentionally keeps this command independent), so the value is pinned by
/// the lock-step unit test and the connector remains the normative source.
pub const MAX_LEASE_SECONDS: i64 = 86_400;

/// Q7 — oldest claimed-row age in seconds.  The table placeholder is replaced
/// only with a name selected from `G0239_CANDIDATES`; user input never reaches
/// this statement.
pub const Q7_SQL: &str = "SELECT extract(epoch FROM (clock_timestamp() - min(available_at)))::bigint FROM {table} WHERE status = 1 AND claim_token IS NOT NULL";

/// Parse the Q7 single-value psql result.  No rows are represented by an empty
/// result and mean there is no claimed row to age.
pub fn parse_claimed_age(out: &str) -> Result<Option<i64>, String> {
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<i64>()
        .map(Some)
        .map_err(|error| format!("invalid oldest-claimed age {out:?}: {error}"))
}

/// Render the Q7 report line in the same shape as Q4's pending-age line.
pub fn claimed_age_line(secs: Option<i64>) -> String {
    match secs {
        Some(secs) => format!("audit-provision-check: oldest-claimed-age: {secs}s"),
        None => "audit-provision-check: oldest-claimed-age: n/a".to_owned(),
    }
}

/// The required named scenarios emitted by the DB-free B5-2 probe.
pub const RELAY_PROBE_SCENARIOS: [&str; 9] = [
    "happy_path",
    "forbidden_403",
    "permanent_422",
    "permanent_409",
    "receipt_mismatch",
    "transient_500",
    "transient_timeout",
    "lease_invariant",
    "fencing_stale_token",
];

/// Validate the captured stdout contract of the relay probe.
///
/// The exit code is deliberately checked separately from the output: a
/// process that exits zero without the nine named lines must not open the
/// provisioning gate.  Unknown PASS names are tolerated so the probe can add
/// scenarios without requiring a synchronized CLI release.
pub fn validate_probe_output(exit_ok: bool, stdout: &str) -> Result<(), String> {
    if !exit_ok {
        return Err("relay probe exited non-zero".to_owned());
    }
    for name in RELAY_PROBE_SCENARIOS {
        let expected = format!("probe: {name}: PASS");
        if !stdout.lines().any(|line| line.trim_end() == expected) {
            return Err(format!("missing probe PASS line: {expected}"));
        }
    }
    if stdout
        .lines()
        .any(|line| line.starts_with("probe: ") && line.contains(": FAIL"))
    {
        return Err("relay probe reported a FAIL line".to_owned());
    }
    Ok(())
}

/// `--relay`: run the base provisioning check first, then require the
/// captured nine-scenario probe output before reporting a successful gate.
pub async fn run_relay(db_url: &str) -> Outcome {
    // Dead rows, an unhealthy relay with undelivered rows, and stuck leases
    // must short-circuit before the probe is spawned.
    let base = crate::audit_provision::run(db_url).await;
    if base.is_error() {
        return base;
    }

    let mut command = if let Ok(bin) = std::env::var("AERO_RELAY_PROBE_BIN") {
        tokio::process::Command::new(bin)
    } else {
        let mut command = tokio::process::Command::new("cargo");
        command.args([
            "run",
            "--quiet",
            "-p",
            "aero-audit-connector",
            "--bin",
            "aero-audit-relay-probe",
        ]);
        command
    };
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Outcome::error(format!("cannot launch relay probe: {error}")),
    };

    // The probe emits only a handful of short lines, so waiting before
    // draining the pipes stays well below the OS pipe buffer and lets the
    // timeout branch kill/reap the process deterministically.
    let status = match tokio::time::timeout(std::time::Duration::from_secs(120), child.wait()).await
    {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => return Outcome::error(format!("relay probe wait failed: {error}")),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Outcome::error("relay probe timed out after 120s");
        }
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let mut bytes = Vec::new();
        if tokio::io::AsyncReadExt::read_to_end(&mut pipe, &mut bytes)
            .await
            .is_ok()
        {
            stdout = String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    if let Some(mut pipe) = child.stderr.take() {
        let mut bytes = Vec::new();
        if tokio::io::AsyncReadExt::read_to_end(&mut pipe, &mut bytes)
            .await
            .is_ok()
        {
            stderr = String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    print!("{stdout}");
    eprint!("{stderr}");

    match status.code() {
        Some(0) => match validate_probe_output(true, &stdout) {
            Ok(()) => {
                println!("audit-provision-check: relay-probe: PASS (9/9)");
                Outcome::ok("")
            }
            Err(reason) => {
                println!("audit-provision-check: relay-probe: FAIL — {reason}");
                Outcome::error(format!("relay probe: {reason}"))
            }
        },
        Some(1) => Outcome::error("relay probe: one or more scenarios FAILED"),
        Some(2) => Outcome::error("relay probe usage error — cannot verify B5-2 state machine"),
        _ => Outcome::error(format!("relay probe exited abnormally: {status}")),
    }
}

/// Connection parameters parsed from a postgres URL.  Kept here so the
/// psql-backed command can re-export the historical public API without
/// growing past its size budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnParams {
    pub user: String,
    pub password: String,
    pub host: String,
    pub port: String,
    pub db: String,
}

/// Parse a postgres connection URL.  Passwords are returned for `PGPASSWORD`
/// injection and are never placed in a process argument list.
pub fn parse_db_url(url: &str) -> Result<ConnParams, String> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or_else(|| "database URL must start with postgres://".to_owned())?;
    let rest = rest.split('?').next().unwrap_or(rest);
    let (auth, host_and_db) = match rest.rfind('@') {
        Some(index) => (&rest[..index], &rest[index + 1..]),
        None => ("", rest),
    };
    let (user, password) = match auth.split_once(':') {
        Some((user, password)) => (user.to_owned(), password.to_owned()),
        None => (auth.to_owned(), String::new()),
    };
    let (host_port, db) = match host_and_db.split_once('/') {
        Some((host, db)) => (host, db.to_owned()),
        None => (host_and_db, String::new()),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !port.is_empty() => {
            (host.to_owned(), port.to_owned())
        }
        _ => (
            if host_port.is_empty() {
                "localhost".to_owned()
            } else {
                host_port.to_owned()
            },
            "5432".to_owned(),
        ),
    };
    if user.is_empty() {
        return Err("database URL must include a user".to_owned());
    }
    if db.is_empty() {
        return Err("database URL must include a database name".to_owned());
    }
    Ok(ConnParams {
        user,
        password,
        host,
        port,
        db,
    })
}
