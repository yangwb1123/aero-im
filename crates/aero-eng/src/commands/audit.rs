//! `audit-provision-check` — verify audit relay provisioning (psql-backed).

use crate::outcome::Outcome;
use crate::register_command;

register_command!(
    AuditProvisionCheck_,
    "audit-provision-check",
    "Verify audit relay provisioning (psql-backed): fail-closed grant check + outbox status distribution; --priority = moderation-priority drill (probe + 500-backlog drill; TRUNCATEs audit_governance_outbox — throwaway DB only; refuses on a non-empty outbox unless AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1); --relay = B5-2 relay state-machine probe (Healthy requires all 9 probe PASS lines)",
    |_ctx, args| {
        let url = std::env::var("DATABASE_URL")
            .or_else(|_| std::env::var("AERO__DATABASE__URL"));
        match url {
            Ok(url) => match args.get(2).map(String::as_str) {
                None => crate::audit_provision::run(&url).await,
                Some("--priority") => crate::audit_provision::run_priority(&url).await,
                Some("--relay") => crate::relay_runtime::run_relay(&url).await,
                // Loud usage error for any unknown flag: silently running the
                // base check on a typo'd --priority would skip the drill
                // (silent-wrong-result beats loud error).
                Some(_) => Outcome::error("usage: audit-provision-check [--priority|--relay]"),
            },
            Err(_) => Outcome::error(
                "audit-provision-check: no database URL — set DATABASE_URL or AERO__DATABASE__URL",
            ),
        }
    }
);
