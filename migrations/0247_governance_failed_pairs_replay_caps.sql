-- Migration 0247: replay caps + dead state for the fail-open DLQ
-- (design-gate F-4, auth B5-1 slice).
--
-- The 0244 DLQ had no replay_attempts cap, no dead state, no retention:
-- under a persistent failure, `replay_all` re-enqueued a NEW row per failed
-- replay and the table doubled every run. This adds:
--   * replay_attempts  INTEGER NOT NULL DEFAULT 0 — bounded replay attempts
--   * status           TEXT    NOT NULL DEFAULT 'pending' — 'pending' | 'dead'
--     (MAX_ATTEMPTS reached or an operator marked it terminal)
--
-- Idempotent: IF NOT EXISTS, so a partial apply self-heals on re-run.
ALTER TABLE audit_governance_failed_pairs
    ADD COLUMN IF NOT EXISTS replay_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE audit_governance_failed_pairs
    ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'pending';

-- Replay eligibility is 'pending' only.
CREATE INDEX IF NOT EXISTS failed_pairs_pending_idx
    ON audit_governance_failed_pairs (id)
    WHERE status = 'pending' AND replayed_at IS NULL;
