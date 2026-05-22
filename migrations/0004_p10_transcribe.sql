-- P10: extend ai_jobs.kind CHECK to include 'transcribe' (voice → text via Whisper or stub).

ALTER TABLE ai_jobs DROP CONSTRAINT IF EXISTS ai_jobs_kind_check;
ALTER TABLE ai_jobs ADD CONSTRAINT ai_jobs_kind_check
    CHECK (kind IN ('embed', 'summarize', 'moderate', 'answer', 'transcribe'));
