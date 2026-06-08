-- 0062_activity_feed.sql — general-purpose per-participant activity feed.
--
-- A durable, per-participant feed of NON-message events. The existing
-- notification inbox (0019) is message+room-scoped (both columns NOT NULL) and so
-- cannot carry a creator-feed entry like "a creator you follow went live"; this
-- table fills that gap with a deliberately general shape (kind + optional
-- actor/subject + human summary), reusable for future non-message events.
--
-- `participant_id` is the RECIPIENT. `kind` is a short discriminator (e.g.
-- 'stream_live'). `actor_id` is the entity that triggered the entry (e.g. the
-- creator who went live); `subject_id` is the entity it is about (e.g. the stream
-- id). Both are nullable, opaque uuids with NO foreign key — an entry should
-- outlive the rows it references (a stream may be pruned while its feed entry is
-- kept), mirroring how `stream_clips` (0057) keeps `stream_id` a plain column.
-- `read_at` is NULL until the recipient marks it read. Purely additive: a NEW
-- `activity_feed` table; no existing table is reshaped, and the whole script is
-- idempotent (safe to re-run).
CREATE TABLE IF NOT EXISTS activity_feed (
    id             UUID        PRIMARY KEY,
    participant_id UUID        NOT NULL,
    kind           TEXT        NOT NULL,
    actor_id       UUID,
    subject_id     UUID,
    summary        TEXT        NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    read_at        TIMESTAMPTZ
);

-- Per-recipient feed listing, newest first. ULID-backed ids sort by time, so
-- `(participant_id, id DESC)` serves both the list (keyset `id < before`) and the
-- unread scan without a separate created_at index.
CREATE INDEX IF NOT EXISTS activity_feed_participant_idx
    ON activity_feed (participant_id, id DESC);
