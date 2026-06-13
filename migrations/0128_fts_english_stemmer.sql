-- Full-text search: switch the message search config from 'simple' (exact
-- tokens, no stemming) to 'english' so a query for 'deploy' matches 'deploying',
-- 'run' matches 'running', and English stopwords fold (ROADMAP5 方向三: lexical
-- recall — the scan flagged "'deploying' 匹配不上 'deploy'").
--
-- ⚠️ OPERATIONAL: `search_tsv` is a STORED generated column, so its config can
-- only change by dropping + re-adding it — a ONE-TIME FULL TABLE REWRITE of
-- `messages` plus a GIN index rebuild. Instant on a fresh DB; on a large existing
-- deployment, schedule this in a maintenance window. The query side (message.rs)
-- moves to 'english' in lock-step so the tsquery stems the same way as the tsvector.
-- (`unaccent` is intentionally NOT added: it is not IMMUTABLE and so cannot sit in
-- a generated column / index without a wrapper — a separate change.)
DROP INDEX IF EXISTS messages_search_tsv_gin;
ALTER TABLE messages DROP COLUMN IF EXISTS search_tsv;
ALTER TABLE messages
    ADD COLUMN search_tsv tsvector
    GENERATED ALWAYS AS (to_tsvector('english', COALESCE(searchable_text, ''))) STORED;
CREATE INDEX IF NOT EXISTS messages_search_tsv_gin ON messages USING gin (search_tsv);
