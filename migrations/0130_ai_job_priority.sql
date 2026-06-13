-- Priority lane for the ai_jobs work queue (ROADMAP5 方向一).
--
-- The claim was strict FIFO (`ORDER BY scheduled_at ASC`). Every 5 minutes the
-- backfill loop enqueues up to 200 `embed` jobs; under FIFO those sit AHEAD of
-- latency-sensitive `moderate` (gates message visibility) and `answer` (a user is
-- waiting) jobs enqueued moments later — so a burst of best-effort backfill stalls
-- user-visible AI. `priority` (lower = more urgent) gives moderation/answer their
-- own lane ahead of summarize/embed; the claim orders by (priority, scheduled_at)
-- so FIFO still breaks ties within a lane.
--
-- DEFAULT 100 keeps any existing/queued row at the lowest (backfill) lane until a
-- new enqueue stamps the kind-derived priority — see `AiJobRepo::priority_for`.
ALTER TABLE ai_jobs
    ADD COLUMN IF NOT EXISTS priority SMALLINT NOT NULL DEFAULT 100;

-- Claim filters status='queued' AND scheduled_at<=NOW(), then orders by the new
-- key — a partial index on exactly that keeps the claim a cheap index scan.
CREATE INDEX IF NOT EXISTS ai_jobs_claim_priority_idx
    ON ai_jobs (priority, scheduled_at)
    WHERE status = 'queued';
