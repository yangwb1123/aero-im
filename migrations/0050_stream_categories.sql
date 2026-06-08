-- 0050 Stream categories & discovery (Twitch-style browse-by-category/tag).
--
-- Categorize live streams and browse them by category or tag. Purely additive:
-- the existing `streams` table is NOT touched. Two disjoint association tables
-- key off a stream's uuid id:
--   * `stream_category_assignments` — at most ONE category per stream (the
--     `stream_id` PRIMARY KEY enforces this; assign upserts).
--   * `stream_tags` — many free-form tags per stream (composite PK dedupes).
-- `stream_categories` is the small, slug-addressable catalog of categories.
--
-- Idempotent: `IF NOT EXISTS` tables/indexes and `ON CONFLICT (slug) DO NOTHING`
-- seeds, so re-running is a no-op. Discovery joins the assignments to `streams`
-- and filters on the live predicate (`streams.status = 'live'`).
CREATE TABLE IF NOT EXISTS stream_categories (
  id uuid PRIMARY KEY,
  slug text UNIQUE NOT NULL,
  name text NOT NULL,
  sort int NOT NULL DEFAULT 0
);

-- Seed a handful of default categories. Deterministic ids so a re-run (or a
-- second node) lands on the same rows; the unique `slug` is the conflict key.
INSERT INTO stream_categories (id, slug, name, sort) VALUES
  ('00000000-0000-0000-0000-000000000501'::uuid, 'gaming',    'Gaming',        10),
  ('00000000-0000-0000-0000-000000000502'::uuid, 'music',     'Music',         20),
  ('00000000-0000-0000-0000-000000000503'::uuid, 'coding',    'Coding',        30),
  ('00000000-0000-0000-0000-000000000504'::uuid, 'talk',      'Just Chatting', 40),
  ('00000000-0000-0000-0000-000000000505'::uuid, 'education', 'Education',     50)
ON CONFLICT (slug) DO NOTHING;

-- At most one category per stream: `stream_id` is the PRIMARY KEY, so assigning
-- again upserts (`ON CONFLICT (stream_id) DO UPDATE`).
CREATE TABLE IF NOT EXISTS stream_category_assignments (
  stream_id uuid PRIMARY KEY,
  category_id uuid NOT NULL
);
CREATE INDEX IF NOT EXISTS stream_category_assignments_category_idx
  ON stream_category_assignments (category_id);

-- Free-form per-stream tags; composite PK dedupes a (stream, tag) pair.
CREATE TABLE IF NOT EXISTS stream_tags (
  stream_id uuid NOT NULL,
  tag text NOT NULL,
  PRIMARY KEY (stream_id, tag)
);
