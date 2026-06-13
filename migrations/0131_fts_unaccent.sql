-- Accent-insensitive full-text search (ROADMAP5 方向三): fold diacritics so a
-- query for 'cafe'/'resume'/'naive' matches 'café'/'résumé'/'naïve' (and vice
-- versa). Stacks on top of the 'english' stemmer from 0128.
--
-- `unaccent` itself is NOT IMMUTABLE (it depends on a mutable dictionary), so it
-- can't sit directly in a generated column / index. The standard fix is an
-- IMMUTABLE wrapper that pins the dictionary by name — safe because we never
-- change the shipped `unaccent` dictionary. Both the stored tsvector and the
-- query side (message.rs) call f_unaccent in lock-step.
--
-- ⚠️ OPERATIONAL: like 0128 this DROPs + re-ADDs the STORED generated `search_tsv`
-- column — a ONE-TIME FULL TABLE REWRITE of `messages` + GIN rebuild. Schedule in
-- a maintenance window on a large deployment.
CREATE EXTENSION IF NOT EXISTS unaccent;

CREATE OR REPLACE FUNCTION f_unaccent(text) RETURNS text
    AS $$ SELECT public.unaccent('public.unaccent', $1) $$
    LANGUAGE sql IMMUTABLE PARALLEL SAFE STRICT;

DROP INDEX IF EXISTS messages_search_tsv_gin;
ALTER TABLE messages DROP COLUMN IF EXISTS search_tsv;
ALTER TABLE messages
    ADD COLUMN search_tsv tsvector
    GENERATED ALWAYS AS (to_tsvector('english', f_unaccent(COALESCE(searchable_text, '')))) STORED;
CREATE INDEX IF NOT EXISTS messages_search_tsv_gin ON messages USING gin (search_tsv);
