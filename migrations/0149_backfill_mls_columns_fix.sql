-- Fix: messages-partition backfill omitted the 3 MLS columns.
--
-- `backfill_messages_partition` (migration 0148) lists source columns explicitly
-- so the generated `search_tsv` is recomputed rather than copied — but the list
-- left out the three MLS-E2E columns added to `messages` in migration 0003
-- (`mls_group_id`, `mls_epoch`, `mls_payload`). The shadow table itself has them
-- (it was `CREATE TABLE … LIKE messages INCLUDING …`), so the pre-window
-- incremental backfill silently dropped the MLS ciphertext payload of every
-- encrypted message. The cutover's in-window full-column final sync masked the
-- end result, but the incremental copy was lossy on its own and any verification
-- between backfill and cutover would have shown NULL MLS columns.
--
-- This `CREATE OR REPLACE` (additive, no schema change, no chain break) restores
-- the three columns to both sides of the INSERT … SELECT. Re-running the backfill
-- after this migration repairs already-copied rows: it is `ON CONFLICT DO NOTHING`,
-- so it won't overwrite, but a one-time `UPDATE messages_partitioned p SET
-- mls_* = m.mls_* FROM messages m WHERE p.id = m.id AND p.mls_group_id IS NULL
-- AND m.mls_group_id IS NOT NULL` (documented in the runbook) backfills any rows
-- copied by the old function. Found by the throwaway-DB cutover verification.

CREATE OR REPLACE FUNCTION backfill_messages_partition(
    batch_size INT  DEFAULT 5000,            -- rows per call (throttle knob)
    from_id    UUID DEFAULT '00000000-0000-0000-0000-000000000000'
) RETURNS TABLE (rows_copied BIGINT, last_id UUID)
LANGUAGE plpgsql
AS $fn$
BEGIN
    RETURN QUERY
    WITH batch AS (
        SELECT m.*
          FROM messages m
         WHERE m.id > from_id
         ORDER BY m.id
         LIMIT batch_size
    ),
    ins AS (
        -- Explicit column list so the generated `search_tsv` is recomputed on
        -- insert rather than fed from the source. Every stored column is carried
        -- over — including the three MLS-E2E columns (mig 0003) the prior version
        -- omitted.
        INSERT INTO messages_partitioned (
            id, room_id, sender_id, blocks, reply_to, metadata, embedding,
            created_at, edited_at, deleted_at, searchable_text, expires_at,
            mls_group_id, mls_epoch, mls_payload
        )
        SELECT
            id, room_id, sender_id, blocks, reply_to, metadata, embedding,
            created_at, edited_at, deleted_at, searchable_text, expires_at,
            mls_group_id, mls_epoch, mls_payload
          FROM batch
        ON CONFLICT DO NOTHING
        RETURNING id
    )
    SELECT
        (SELECT count(*) FROM ins)::BIGINT,
        COALESCE((SELECT id FROM batch ORDER BY id DESC LIMIT 1), from_id);
END
$fn$;

COMMENT ON FUNCTION backfill_messages_partition(INT, UUID) IS
    'ADDITIVE prep: incrementally copy messages → messages_partitioned in id-ordered '
    'batches (ON CONFLICT DO NOTHING), carrying ALL stored columns incl. the MLS-E2E '
    'columns (mig 0003); search_tsv is recomputed. Call in a throttled loop before the '
    'cutover window; returns (rows_copied, last_id). 0 rows_copied = caught up. Reads '
    'messages only — never writes/locks the live table. See docs/runbooks/messages-partitioning.md.';
