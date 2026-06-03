-- 0024_unfurl_cache.sql — link-unfurl (Open Graph preview) cache
--
-- When a message contains URL(s), a background listener fetches each URL's Open
-- Graph metadata and attaches a `link_preview` Card to the message. This table
-- caches the resolved preview per URL so a hot link is fetched/parsed at most
-- once per TTL window (~24h, enforced in the query layer via a fetched_at check)
-- instead of on every message that mentions it. Purely additive — no existing
-- table is touched. Idempotent so re-running the migration is safe.

CREATE TABLE IF NOT EXISTS unfurl_cache (
    -- SHA-256 hex of the URL: a fixed-width key so the PK isn't an unbounded URL.
    url_hash   TEXT        PRIMARY KEY,
    -- The original URL, kept alongside the hash for debuggability.
    url        TEXT        NOT NULL,
    -- The resolved LinkPreview ({url,title?,description?,image?,site_name?}).
    preview    JSONB       NOT NULL,
    -- When this preview was fetched; drives TTL/freshness on read.
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Lets a periodic eviction sweep (and freshness checks) range over age cheaply.
CREATE INDEX IF NOT EXISTS unfurl_cache_fetched_at_idx
    ON unfurl_cache (fetched_at);
