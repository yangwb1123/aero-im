//! Report-only PII backfill scan over historical messages (post-第五版 follow-up).
//!
//! The send-path PII guard ([`aero_im_core::PiiDetector`]) only inspects *new*
//! messages: anything that landed before the guard was enabled — or while it was
//! off — already flowed into `searchable_text`, the FTS index, AI embeddings, and
//! exports with no retrospective check. The original P3 analysis flagged that gap
//! ("已落库的历史 PII 无回溯").
//!
//! This module closes the *visibility* half of that gap with a **strictly
//! read-only, report-only** retrospective sweep. It keyset-pages over every
//! historical message (oldest-first by ULID id, via the existing read-only
//! [`MessageRepo::changes_since`]), runs each message's searchable text through
//! the same [`PiiDetector`], and reports suspected hits so an administrator can
//! review and decide. It **never** issues an `UPDATE`/`DELETE`, never rewrites
//! `searchable_text` or `embedding`, and never touches user history — automated
//! mutation of a user's own message would be destructive and is deliberately out
//! of scope.
//!
//! ## Report outlets
//!
//! Findings are surfaced two ways, both migration-free and PII-safe:
//!
//! * **Metric** `aero_pii_backfill_findings_total{kind}` — a counter per PII class
//!   ([`record_finding`]), so a dashboard/alert can track the suspected backlog.
//! * **Structured log** at `warn` — the `message_id` + the matched kind tags
//!   **only**. The matched *text* is never logged (that would re-leak the very PII
//!   we are flagging into the log sink). The `message_id` is enough for an admin to
//!   locate the message; it is not itself personal data.
//!
//! Persisting findings to a table was considered and deliberately skipped: a
//! participant-keyed findings table would itself become PII that GDPR erasure must
//! sweep, for no gain over metric+log here. Admins who want a durable report can
//! scrape the counter and collect the logs.
//!
//! ## Operation
//!
//! Off by default. The background sweep is gated on `AERO_PII_BACKFILL_SCAN` and
//! additionally requires the PII guard config itself (`AERO_PII_GUARD`) so the scan
//! uses exactly the classes the deployment cares about. It is batched and rate
//! limited (a sleep between pages) so it never floods the DB, and it is
//! [`CancellationToken`]-aware so graceful shutdown stops it promptly.

use std::time::Duration;

use aero_common::MessageId;
use aero_im_core::{PiiDetector, PiiKind};
use aero_storage::MessageRepo;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// Metric: count of suspected historical-PII findings, labeled by `kind`
/// (`ssn`/`credit_card`/`email`/`phone`). One increment per (message, kind) hit.
///
/// Server-local (not an [`aero_common::metrics::names`] constant) because the
/// backfill scan is a gateway-side operational task, mirroring the other
/// server-local counters in [`crate::metrics`].
pub const PII_BACKFILL_FINDINGS_TOTAL: &str = "aero_pii_backfill_findings_total";

/// How many messages to pull per keyset page. [`MessageRepo::changes_since`] caps
/// the limit at 200 internally; we ask for that ceiling so each round-trip does
/// the most work.
const PAGE_SIZE: i64 = 200;

/// Default pause between pages, in milliseconds. Keeps the sweep from saturating
/// the DB on a large history; overridable via `AERO_PII_BACKFILL_SCAN_SLEEP_MS`.
const DEFAULT_SLEEP_MS: u64 = 250;

/// One message's worth of input for the pure scan: its id and searchable text.
/// The scan never needs the blocks/metadata, so callers project to just this.
#[derive(Debug, Clone)]
pub struct ScanItem {
    pub id: MessageId,
    pub text: String,
}

/// A single suspected finding: which message, and which PII classes it matched.
/// Carries no PII text — only the id and the matched kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub message_id: MessageId,
    pub kinds: Vec<PiiKind>,
}

/// Pure scan of a batch of messages: returns one [`Finding`] per message that the
/// detector flags (messages with no PII are omitted). No I/O, no logging, no
/// metrics — so the matching logic is unit-testable in isolation. The caller is
/// responsible for reporting (metric + log) the returned findings.
#[must_use]
pub fn scan_batch(detector: &PiiDetector, items: &[ScanItem]) -> Vec<Finding> {
    items
        .iter()
        .filter_map(|item| {
            let kinds = detector.scan(&item.text);
            if kinds.is_empty() {
                None
            } else {
                Some(Finding {
                    message_id: item.id,
                    kinds,
                })
            }
        })
        .collect()
}

/// Report one finding: bump the per-kind metric and emit a PII-free `warn` log.
/// Splitting this out keeps [`scan_batch`] pure and the reporting side effect at a
/// single, auditable call site.
pub fn record_finding(finding: &Finding) {
    for kind in &finding.kinds {
        aero_common::metrics::inc_counter_labeled(
            PII_BACKFILL_FINDINGS_TOTAL,
            1,
            &[("kind", kind.tag())],
        );
    }
    // Tags only — never the matched text (that would re-leak the PII into logs).
    let tags: Vec<&str> = finding.kinds.iter().map(|k| k.tag()).collect();
    tracing::warn!(
        message_id = %finding.message_id,
        kinds = ?tags,
        "pii_backfill: historical message matched PII detector (report-only, not modified)"
    );
}

/// Run the report-only backfill scan to completion (or until cancelled).
///
/// Keyset-pages over every historical message oldest-first (read-only), scans each
/// page, and reports findings. Soft-deleted messages are skipped by
/// `changes_since`'s `deleted_at IS NULL` filter (their text is already cleared),
/// which is exactly right: there is nothing left to flag there. Returns the total
/// number of findings reported.
///
/// This is one-shot: it sweeps the *current* history once and exits, rather than
/// looping forever. New messages are covered by the send-path guard, so a
/// repeating sweep would only re-flag the same backlog; an operator who wants to
/// re-run schedules another invocation.
pub async fn run_backfill_scan(
    repo: &MessageRepo,
    detector: &PiiDetector,
    cancel: &CancellationToken,
) -> u64 {
    let sleep_ms = std::env::var("AERO_PII_BACKFILL_SCAN_SLEEP_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SLEEP_MS);

    // Start from the all-zero id so the first `id > since` page is the very oldest
    // batch; advance the cursor to the last id of each page (keyset pagination).
    let mut cursor = MessageId::nil();
    let mut scanned: u64 = 0;
    let mut total_findings: u64 = 0;

    tracing::info!("pii_backfill: starting report-only historical scan");
    loop {
        if cancel.is_cancelled() {
            tracing::info!(
                scanned,
                findings = total_findings,
                "pii_backfill: cancelled"
            );
            return total_findings;
        }

        let page = match repo.scan_after(None, cursor, PAGE_SIZE).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "pii_backfill: page query failed; stopping scan");
                return total_findings;
            }
        };
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |m| m.id);
        scanned += page.len() as u64;

        let items: Vec<ScanItem> = page
            .iter()
            .map(|m| ScanItem {
                id: m.id,
                text: m.searchable_text(),
            })
            .collect();
        let findings = scan_batch(detector, &items);
        for finding in &findings {
            record_finding(finding);
        }
        total_findings += findings.len() as u64;

        // A short page means we've reached the end of history.
        if page.len() < usize::try_from(PAGE_SIZE).expect("PAGE_SIZE is positive") {
            break;
        }

        // Rate limit between pages; bail early if cancelled mid-sleep.
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!(scanned, findings = total_findings, "pii_backfill: cancelled");
                return total_findings;
            }
            () = sleep(Duration::from_millis(sleep_ms)) => {}
        }
    }

    tracing::info!(
        scanned,
        findings = total_findings,
        "pii_backfill: report-only historical scan complete"
    );
    total_findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_im_core::PiiConfig;

    fn item(id: MessageId, text: &str) -> ScanItem {
        ScanItem {
            id,
            text: text.to_string(),
        }
    }

    fn detector_all() -> PiiDetector {
        PiiDetector::new(PiiConfig {
            ssn: true,
            credit_card: true,
            email: true,
            phone: true,
        })
    }

    #[test]
    fn scan_batch_flags_only_messages_with_pii() {
        let d = detector_all();
        let m_clean = MessageId::new();
        let m_ssn = MessageId::new();
        let m_card = MessageId::new();
        let items = vec![
            item(m_clean, "ship the release at 3pm in room 1402"),
            item(m_ssn, "my ssn is 123-45-6789"),
            item(m_card, "card 4111111111111111"),
        ];

        let findings = scan_batch(&d, &items);

        // Clean message is omitted; only the two PII-bearing ones are reported.
        assert_eq!(findings.len(), 2);
        let ssn = findings
            .iter()
            .find(|f| f.message_id == m_ssn)
            .expect("ssn finding");
        assert_eq!(ssn.kinds, vec![PiiKind::Ssn]);
        let card = findings
            .iter()
            .find(|f| f.message_id == m_card)
            .expect("card finding");
        assert_eq!(card.kinds, vec![PiiKind::CreditCard]);
        assert!(findings.iter().all(|f| f.message_id != m_clean));
    }

    #[test]
    fn scan_batch_reports_multiple_kinds_per_message() {
        let d = detector_all();
        let id = MessageId::new();
        let findings = scan_batch(
            &d,
            &[item(
                id,
                "ssn 123-45-6789 card 4111111111111111 mail a@b.com",
            )],
        );
        assert_eq!(findings.len(), 1);
        let kinds = &findings[0].kinds;
        assert!(kinds.contains(&PiiKind::Ssn));
        assert!(kinds.contains(&PiiKind::CreditCard));
        assert!(kinds.contains(&PiiKind::Email));
    }

    #[test]
    fn scan_batch_empty_input_yields_no_findings() {
        let d = detector_all();
        assert!(scan_batch(&d, &[]).is_empty());
    }

    #[test]
    fn scan_batch_all_clean_yields_no_findings() {
        let d = detector_all();
        let items = vec![
            item(MessageId::new(), "lunch at noon"),
            item(MessageId::new(), "deploy v1.2.3 is green"),
        ];
        assert!(scan_batch(&d, &items).is_empty());
    }

    #[test]
    fn scan_batch_respects_detector_config() {
        // A detector with only email enabled must ignore an SSN-bearing message.
        let email_only = PiiDetector::new(PiiConfig {
            ssn: false,
            credit_card: false,
            email: true,
            phone: false,
        });
        let id_email = MessageId::new();
        let items = vec![
            item(MessageId::new(), "ssn 123-45-6789"),
            item(id_email, "reach me at jane@example.com"),
        ];
        let findings = scan_batch(&email_only, &items);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].message_id, id_email);
        assert_eq!(findings[0].kinds, vec![PiiKind::Email]);
    }
}
