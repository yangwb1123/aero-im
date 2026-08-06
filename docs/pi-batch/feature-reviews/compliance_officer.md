Compliance re-review complete — written to `docs/pi-batch/feature-reviews/compliance_officer-r2.md` (round-1 file preserved; `prompts/README.md` absent from the repo, so I applied `/home/u1/ai-batch-runner/prompts/README.md` and re-derived every load-bearing claim from current source at the 0238 revision).

## What I verified in-tree (not taken from prior reports)

- **Recall tx** (`authorization.rs`): snapshot → placeholder → GC enqueue → 120-char audit digest → outbox, single tx; role re-check under `FOR UPDATE`; final `WHERE recalled_at IS NULL AND deleted_at IS NULL` fence.
- **GC vs history defect (NEW, Medium — security F1, independently confirmed)**: `enqueue_unreferenced_blobs_in_tx` (`crud.rs:306–340`) and drain-time `has_live_references` (`blob.rs:328`) scan **`messages` only** — `message_edits` (written in the same recall tx with the original `blob_id`s) is invisible to both. Recalled attachment bytes are destroyed within ~60 s while the member-visible `GET /api/messages/:id/history` still serves the snapshot pointing at them. The feature's "snapshot is evidence, not a loss" promise is broken for attachments.
- **Deferred erasure gap (round-1 F2, re-verified open)**: `participant.rs:644` sweep predicate (`searchable_text <> '' OR embedding IS NOT NULL`) skips recalled rows — recalled-under-hold content survives the post-hold completion sweep forever; zero `recalled` references in `participant.rs`.
- **`/changes` clamp (NEW, Low — DB F2, confirmed)**: `query.rs:123–134` single-page 200-row clamp → bounded non-delivery of recalls to offline members in high-churn rooms.
- **Migration timeout (NEW, Low/scale-conditional — DB F1, confirmed)**: `db.rs:31` `statement_timeout=10s` via `after_connect`; boot runs `migrate()` on that pool (`persistence.rs:33`) — 0238's `CREATE INDEX` can abort the chain on prod-sized tables.
- **Positive controls re-verified**: permission matrix + TOCTOU (9/9 + 28/28 suites), tenant-guard ordering, same-tx audit, minimization on live surfaces, `recalled_by` FK safety under tombstone stance, webhook `Recalled` mapping, `aero_messages_recalled_total` metric.

## Output structure (per role prompt)

1. **Scope statement** — jurisdiction, data classification, framework applicability all marked **unknown** (GDPR Art. 17(3)(e) comment = design intent, not applicability); recall characterized as placeholder-not-erasure.
2. **Control matrix** — 16 rows (requirement / status / repo evidence / process evidence / gap / owner / validation).
3. **Findings** — 8 severity-sorted (F1 Medium new, F2 Medium re-verified, F3–F5 Low new/re-verified, F6–F8 carried) + informationals (existence oracle, `recalled_by` stance, DS-4 deploy discipline, no-browser-E2E).
4. **Audit-readiness** — 5 required documents; next decisions led by P0 F1 fix, F2 predicate extension, product/legal calls on history visibility and recall window, and the jurisdiction declaration.

No claims of certification made; this is advisory engineering assessment only.
