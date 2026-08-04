-- ===========================================================================
-- messages-cutover-rollback.sql — reverse the partitioned messages cutover.
--
--  ███  DOCS ARTIFACT — NOT A MIGRATION.  ███
--  • Run only after `messages-cutover.sql` committed and before
--    `DROP TABLE messages_old`.
--  • Application writes MUST be paused for the complete transaction.
--  • Use a tested backup and an approved DBA/on-call maintenance window.
--  • The transaction first replays every post-cutover insert/update/delete into
--    the retained heap, then restores all FKs, columns, indexes, and table names.
-- ===========================================================================

\set ON_ERROR_STOP on

BEGIN;

LOCK TABLE messages, messages_old IN ACCESS EXCLUSIVE MODE;

-- Fail closed unless this is exactly the reversible post-cutover shape.
DO $preconditions$
DECLARE
    duplicate_ids BIGINT;
    timestamp_drift BIGINT;
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_class AS relation
         WHERE relation.oid = 'messages'::regclass
           AND relation.relkind = 'p'
    ) OR NOT EXISTS (
        SELECT 1
          FROM pg_class AS relation
         WHERE relation.oid = 'messages_old'::regclass
           AND relation.relkind = 'r'
    ) THEN
        RAISE EXCEPTION
            'ROLLBACK ABORT: expected partitioned messages and heap messages_old';
    END IF;

    SELECT count(*)
      INTO duplicate_ids
      FROM (
          SELECT id
            FROM messages
           GROUP BY id
          HAVING count(*) > 1
      ) AS duplicates;
    IF duplicate_ids <> 0 THEN
        RAISE EXCEPTION
            'ROLLBACK ABORT: partitioned messages contains % duplicate id(s)',
            duplicate_ids;
    END IF;

    SELECT count(*)
      INTO timestamp_drift
      FROM messages AS current_message
      JOIN messages_old AS retained_message USING (id)
     WHERE current_message.created_at
           IS DISTINCT FROM retained_message.created_at;
    IF timestamp_drift <> 0 THEN
        RAISE EXCEPTION
            'ROLLBACK ABORT: % retained id(s) changed created_at',
            timestamp_drift;
    END IF;
END
$preconditions$;

-- Replay inserts and updates. `search_tsv` is generated and
-- `reply_to_created_at` exists only on the partitioned shape.
INSERT INTO messages_old (
    id, room_id, sender_id, blocks, reply_to, metadata, embedding,
    created_at, edited_at, deleted_at, searchable_text,
    mls_group_id, mls_epoch, mls_payload, expires_at, version,
    delivery_ordinal
)
SELECT
    id, room_id, sender_id, blocks, reply_to, metadata, embedding,
    created_at, edited_at, deleted_at, searchable_text,
    mls_group_id, mls_epoch, mls_payload, expires_at, version,
    delivery_ordinal
  FROM messages
ON CONFLICT (id) DO UPDATE SET
    room_id          = EXCLUDED.room_id,
    sender_id        = EXCLUDED.sender_id,
    blocks           = EXCLUDED.blocks,
    reply_to         = EXCLUDED.reply_to,
    metadata         = EXCLUDED.metadata,
    embedding        = EXCLUDED.embedding,
    edited_at        = EXCLUDED.edited_at,
    deleted_at       = EXCLUDED.deleted_at,
    searchable_text  = EXCLUDED.searchable_text,
    mls_group_id     = EXCLUDED.mls_group_id,
    mls_epoch        = EXCLUDED.mls_epoch,
    mls_payload      = EXCLUDED.mls_payload,
    expires_at       = EXCLUDED.expires_at,
    version          = EXCLUDED.version,
    delivery_ordinal = EXCLUDED.delivery_ordinal;

-- Replay hard deletes. The application normally soft-deletes, but retention and
-- ephemeral-message cleanup can remove rows during a soak.
DELETE FROM messages_old AS retained_message
 WHERE NOT EXISTS (
     SELECT 1
       FROM messages AS current_message
      WHERE current_message.id = retained_message.id
        AND current_message.created_at = retained_message.created_at
 );

-- Prove the retained heap now represents the same application-visible rows.
DO $replay_parity$
DECLARE
    differing_rows BIGINT;
BEGIN
    WITH current_rows AS (
        SELECT id,
               to_jsonb(current_message)
                   - 'search_tsv'
                   - 'reply_to_created_at' AS document
          FROM messages AS current_message
    ),
    retained_rows AS (
        SELECT id,
               to_jsonb(retained_message) - 'search_tsv' AS document
          FROM messages_old AS retained_message
    )
    SELECT count(*)
      INTO differing_rows
      FROM current_rows
      FULL JOIN retained_rows USING (id)
     WHERE current_rows.id IS NULL
        OR retained_rows.id IS NULL
        OR current_rows.document IS DISTINCT FROM retained_rows.document;

    IF differing_rows <> 0 THEN
        RAISE EXCEPTION
            'ROLLBACK ABORT: replay left % differing message row(s)',
            differing_rows;
    END IF;
END
$replay_parity$;

-- Remove the write-compatibility triggers owned by the partitioned shape.
DROP TRIGGER messages_fill_reply_created_at ON messages;
DROP FUNCTION fill_message_reply_created_at();

DROP TRIGGER reactions_fill_msg_created_at            ON reactions;
DROP TRIGGER notifications_fill_msg_created_at        ON notifications;
DROP TRIGGER pins_fill_msg_created_at                 ON pins;
DROP TRIGGER bookmarks_fill_msg_created_at            ON bookmarks;
DROP TRIGGER message_receipts_fill_msg_created_at     ON message_receipts;
DROP TRIGGER block_interactions_fill_msg_created_at   ON block_interactions;
DROP TRIGGER notification_bundles_fill_msg_created_at ON notification_bundles;
DROP FUNCTION fill_message_child_created_at();

-- Restore all seven child relations to their original message-id-only FK shape.
ALTER TABLE reactions
    DROP CONSTRAINT reactions_message_id_fkey,
    ADD CONSTRAINT reactions_message_id_fkey
        FOREIGN KEY (message_id) REFERENCES messages_old(id) ON DELETE CASCADE,
    DROP COLUMN msg_created_at;

ALTER TABLE notifications
    DROP CONSTRAINT notifications_message_id_fkey,
    ADD CONSTRAINT notifications_message_id_fkey
        FOREIGN KEY (message_id) REFERENCES messages_old(id) ON DELETE CASCADE,
    DROP COLUMN msg_created_at;

ALTER TABLE pins
    DROP CONSTRAINT pins_message_id_fkey,
    ADD CONSTRAINT pins_message_id_fkey
        FOREIGN KEY (message_id) REFERENCES messages_old(id) ON DELETE CASCADE,
    DROP COLUMN msg_created_at;

ALTER TABLE bookmarks
    DROP CONSTRAINT bookmarks_message_id_fkey,
    ADD CONSTRAINT bookmarks_message_id_fkey
        FOREIGN KEY (message_id) REFERENCES messages_old(id) ON DELETE CASCADE,
    DROP COLUMN msg_created_at;

ALTER TABLE message_receipts
    DROP CONSTRAINT message_receipts_message_id_fkey,
    ADD CONSTRAINT message_receipts_message_id_fkey
        FOREIGN KEY (message_id) REFERENCES messages_old(id) ON DELETE CASCADE,
    DROP COLUMN msg_created_at;

ALTER TABLE block_interactions
    DROP CONSTRAINT block_interactions_message_id_fkey,
    ADD CONSTRAINT block_interactions_message_id_fkey
        FOREIGN KEY (message_id) REFERENCES messages_old(id) ON DELETE CASCADE,
    DROP COLUMN msg_created_at;

ALTER TABLE notification_bundles
    DROP CONSTRAINT notification_bundles_message_id_fkey,
    ADD CONSTRAINT notification_bundles_message_id_fkey
        FOREIGN KEY (message_id) REFERENCES messages_old(id) ON DELETE CASCADE,
    DROP COLUMN msg_created_at;

-- Free every canonical index name held by the partitioned parent.
ALTER INDEX idx_messages_expires_at
    RENAME TO idx_messages_expires_at__failed;
ALTER INDEX idx_messages_room_mutated
    RENAME TO idx_messages_room_mutated__failed;
ALTER INDEX messages_blocks_gin
    RENAME TO messages_blocks_gin__failed;
ALTER INDEX messages_embedding_hnsw
    RENAME TO messages_embedding_hnsw__failed;
ALTER INDEX messages_mls_group_idx
    RENAME TO messages_mls_group_idx__failed;
ALTER INDEX messages_reply_to_idx
    RENAME TO messages_reply_to_idx__failed;
ALTER INDEX messages_room_active_id
    RENAME TO messages_room_active_id__failed;
ALTER INDEX messages_room_created_idx
    RENAME TO messages_room_created_idx__failed;
ALTER INDEX messages_room_delivery_ordinal_key
    RENAME TO messages_room_delivery_ordinal_key__failed;
ALTER INDEX messages_searchable_trgm
    RENAME TO messages_searchable_trgm__failed;
ALTER INDEX messages_search_tsv_gin
    RENAME TO messages_search_tsv_gin__failed;
ALTER INDEX messages_sender_id_active_idx
    RENAME TO messages_sender_id_active_idx__failed;

-- Restore every retained-heap index and its PK to the canonical name.
ALTER INDEX idx_messages_expires_at__old
    RENAME TO idx_messages_expires_at;
ALTER INDEX idx_messages_room_mutated__old
    RENAME TO idx_messages_room_mutated;
ALTER INDEX messages_blocks_gin__old
    RENAME TO messages_blocks_gin;
ALTER INDEX messages_embedding_hnsw__old
    RENAME TO messages_embedding_hnsw;
ALTER INDEX messages_mls_group_idx__old
    RENAME TO messages_mls_group_idx;
ALTER INDEX messages_reply_to_idx__old
    RENAME TO messages_reply_to_idx;
ALTER INDEX messages_room_active_id__old
    RENAME TO messages_room_active_id;
ALTER INDEX messages_room_created_idx__old
    RENAME TO messages_room_created_idx;
ALTER INDEX messages_room_delivery_ordinal_key__old
    RENAME TO messages_room_delivery_ordinal_key;
ALTER INDEX messages_searchable_trgm__old
    RENAME TO messages_searchable_trgm;
ALTER INDEX messages_search_tsv_gin__old
    RENAME TO messages_search_tsv_gin;
ALTER INDEX messages_sender_id_active_idx__old
    RENAME TO messages_sender_id_active_idx;
ALTER INDEX messages_pkey__old RENAME TO messages_pkey;

ALTER TABLE messages     RENAME TO messages_partitioned;
ALTER TABLE messages_old RENAME TO messages;

-- Catalog gates before commit: canonical heap, parked partitioned copy, all
-- eight original inbound FKs, and all canonical indexes must be restored.
DO $postconditions$
DECLARE
    inbound_fks BIGINT;
    misplaced_indexes BIGINT;
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_class
         WHERE oid = 'messages'::regclass
           AND relkind = 'r'
    ) OR NOT EXISTS (
        SELECT 1
          FROM pg_class
         WHERE oid = 'messages_partitioned'::regclass
           AND relkind = 'p'
    ) THEN
        RAISE EXCEPTION 'ROLLBACK ABORT: table names were not restored';
    END IF;

    SELECT count(*)
      INTO inbound_fks
      FROM pg_constraint
     WHERE confrelid = 'messages'::regclass
       AND contype = 'f'
       AND convalidated;
    IF inbound_fks <> 8 THEN
        RAISE EXCEPTION
            'ROLLBACK ABORT: expected 8 validated inbound FKs, found %',
            inbound_fks;
    END IF;

    SELECT count(*)
      INTO misplaced_indexes
      FROM (
          VALUES
              ('idx_messages_expires_at'),
              ('idx_messages_room_mutated'),
              ('messages_blocks_gin'),
              ('messages_embedding_hnsw'),
              ('messages_mls_group_idx'),
              ('messages_reply_to_idx'),
              ('messages_room_active_id'),
              ('messages_room_created_idx'),
              ('messages_room_delivery_ordinal_key'),
              ('messages_searchable_trgm'),
              ('messages_search_tsv_gin'),
              ('messages_sender_id_active_idx'),
              ('messages_pkey')
      ) AS expected(index_name)
      LEFT JOIN pg_class AS index_relation
        ON index_relation.oid =
           to_regclass(format('public.%I', expected.index_name))
      LEFT JOIN pg_index AS index_metadata
        ON index_metadata.indexrelid = index_relation.oid
     WHERE index_metadata.indrelid IS DISTINCT FROM 'messages'::regclass;
    IF misplaced_indexes <> 0 THEN
        RAISE EXCEPTION
            'ROLLBACK ABORT: % canonical index name(s) target the wrong table',
            misplaced_indexes;
    END IF;
END
$postconditions$;

COMMIT;

-- Post-commit application probes:
--   • insert/edit/reply/delete a throwaway message;
--   • insert one row in each of the seven child tables without msg_created_at;
--   • verify delivery ordinal allocation and blob workspace rejection;
--   • only then resume writes.
