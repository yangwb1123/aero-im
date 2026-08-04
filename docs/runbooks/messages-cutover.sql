-- ===========================================================================
-- messages-cutover.sql — THE IRREVERSIBLE CUTOVER for partitioning `messages`.
--
--  ███  DOCS ARTIFACT — NOT A MIGRATION.  ███
--  • This file MUST NEVER be placed in migrations/ or run by the deploy chain.
--  • It is DESTRUCTIVE (rewrites 8 foreign keys + swaps the largest table) and
--    must be executed BY HAND, by a DBA, inside an APPROVED MAINTENANCE WINDOW,
--    with a TESTED BACKUP restore and the application's writes paused/gated.
--  • It is Step C of docs/runbooks/messages-partitioning.md. Steps A–B (the
--    additive shadow table + the throttled, days-ahead backfill loop) are
--    PREREQUISITES and must already be complete (rows_copied = 0) before you run
--    this. See migrations/0148_messages_partition_shadow.sql.
--
--  This exact script was END-TO-END VERIFIED on a throwaway database
--  (a brand-new pgvector:pg17 database, NOT the shared aero DB), most recently
--  on 2026-07-28 against the current chain through
--  `0176_rolling_upgrade_fences.sql` (runbook §5d:
--  delivery_ordinal/shadow/write-compatibility/partition maintenance),
--  previously on 2026-07-16 against the
--  then-current 158-migration chain (runbook §5b: version drift), and originally
--  on 2026-06-19 against the 148-migration chain (runbook §5a): full chain
--  replay → seed → backfill → THIS script → parity / 8-FK integrity /
--  FTS+keyset+reply_to+version query checks all PASS. Production execution still
--  requires its own window + backup; "verified" means the SQL is correct as of
--  the migration chain noted above, not that the window is optional — and not
--  that a LATER schema change to `messages` can't reintroduce the same class of
--  gap. Migration 0174's `delivery_ordinal` mirror and this final sync/parity
--  revision were included in the latest rehearsal (runbook §5c).
-- ===========================================================================
--
-- ─── DESIGN DECISION: how the 8 inbound FKs are repointed ───────────────────
--
-- The new parent `messages_partitioned` has the composite PK `(id, created_at)`
-- that PG's partition-key rule forces (the partition key must be in every
-- UNIQUE/PK). A foreign key may ONLY target a UNIQUE/PK constraint, and it must
-- reference the FULL key. So a child FK on `(message_id)` alone can no longer be
-- satisfied — it must become a composite FK on `(message_id, <message-created_at>)`.
--
-- Two strategies exist (runbook §2 Step C):
--
--   (A) COMPOSITE FK — chosen here for all 7 inbound child FKs.
--       Add a redundant `msg_created_at TIMESTAMPTZ` column to each child,
--       backfill it from the parent, mark NOT NULL, and recreate the FK as
--       (message_id, msg_created_at) → messages(id, created_at) ON DELETE CASCADE.
--       PRO: keeps referential integrity AND the ON DELETE CASCADE semantics the
--            app depends on (a message delete still cascades to reactions, pins,
--            receipts, notifications, bundles, bookmarks, block_interactions).
--       CON: one extra 8-byte TIMESTAMPTZ payload per child (plus row/index
--            alignment overhead) + a one-time backfill of it.
--            Cheap relative to the messages rewrite; the children are far smaller.
--            Because deployed application versions insert only `message_id`, a
--            small permanent BEFORE trigger must resolve `msg_created_at` from
--            the canonical parent. This also makes the maintenance-window cutover
--            backward-compatible with already-deployed writers.
--       NOTE: `msg_created_at` is the *referenced message's* created_at, which is
--            DISTINCT from each child's own created_at/read_at (the child-row time).
--            We deliberately use a NEW column name to avoid that semantic clash.
--
--   (B) NO ENFORCED FK + trigger (rejected as the default).
--       Drop the FK, enforce existence/cascade with AFTER triggers instead.
--       PRO: no schema change on children.
--       CON: cascade + existence are now app/trigger responsibilities; a missed
--            trigger silently loses referential integrity — exactly the failure
--            the runbook §7 warns about. Only choose (B) for a child so hot that
--            the extra column's write cost is unacceptable; none here qualify.
--
-- The self-reference `messages.reply_to → messages(id)` is special: it lives
-- INSIDE the table being swapped. On the partitioned table we re-add it as a
-- composite self-FK referencing (id, created_at). Because a reply and the message
-- it answers may live in DIFFERENT monthly partitions, we cannot assume the
-- parent's created_at; we carry it in a new `reply_to_created_at` column. A
-- reply's target is older-or-equal, so this is always backfillable. (If you would
-- rather not pay this on the hottest table, option (B) — drop the self-FK and
-- rely on the app's existing reply-target validation — is the documented
-- alternative; the FK is advisory here, not load-bearing for cascade.)
--
-- ─── PRECONDITIONS (assert before the window) ───────────────────────────────
--   1. `SELECT * FROM backfill_messages_partition(20000, :hwm)` returns
--      rows_copied = 0 (caught up), driven days ahead per runbook Step B.
--   2. `SELECT ensure_messages_partitions(12);` has been run so every month the
--      live tail can land in has a non-DEFAULT partition.
--   3. A tested backup restore exists. on-call + DBA present. writes gated.
--   4. Migration 0174's current backfill function carries MLS, version, and
--      delivery_ordinal. The FINAL in-window sync below still reconciles every
--      non-generated column so rows copied by an older function or edited
--      between the pre-window pass and this lock cannot retain stale values.
--   5. `version` (migration 0157, added AFTER 0148 shipped the shadow table) is
--      carried by the backfill function as of migration 0158 — but the FINAL
--      sync below ALSO reconciles it (same reasoning as the MLS columns): any
--      row backfilled before 0158 shipped, or edited between the pre-window
--      backfill and this window, must not have its version silently reset to
--      the shadow's column DEFAULT (1), or the client's remembered
--      `expected_version` would permanently mismatch post-cutover.
--   6. `delivery_ordinal` (migration 0174) is NOT NULL/positive on both live and
--      shadow tables; `room_delivery_sequences` is at least each room's live
--      MAX(delivery_ordinal). The in-transaction final sync deliberately repairs
--      any stale pre-existing shadow value before the ordinal parity gate.
-- ===========================================================================

\set ON_ERROR_STOP on

-- One transaction: if anything raises, the whole thing ROLLs BACK and the live
-- `messages` is untouched (it is only RENAMEd at the very end, inside this txn).
BEGIN;

-- Block all writers (and the final-sync race) for the duration. This is the
-- short exclusive window; the long backfill already happened lock-free.
LOCK TABLE messages IN ACCESS EXCLUSIVE MODE;

-- ---------------------------------------------------------------------------
-- 1) FINAL INCREMENTAL CATCH-UP + FINAL SYNC under the lock.
--
-- (a) Drain any rows the pre-window loop missed since its last batch. We call
--     the shipped function in a bounded loop; it is idempotent. (Under the lock
--     no new rows arrive, so this converges in one or two iterations.)
-- (b) Then a full mutable-column sync. ON CONFLICT (id, created_at) DO UPDATE so
--     rows copied by an older backfill function, or changed after their first
--     copy, get their MLS/version/delivery-ordinal and latest
--     edited/deleted/searchable state reconciled to the live row.
-- ---------------------------------------------------------------------------
DO $catchup$
DECLARE
    hwm      uuid := '00000000-0000-0000-0000-000000000000';
    copied   bigint;
BEGIN
    LOOP
        SELECT rows_copied, last_id
          INTO copied, hwm
          FROM backfill_messages_partition(20000, hwm);
        EXIT WHEN copied = 0;
    END LOOP;
END
$catchup$;

-- Full-column reconciliation (covers MLS + version + delivery ordinal and any
-- edit/delete since backfill).
-- search_tsv is generated on the target, so it is intentionally NOT projected.
INSERT INTO messages_partitioned (
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
ON CONFLICT (id, created_at) DO UPDATE SET
    blocks          = EXCLUDED.blocks,
    metadata        = EXCLUDED.metadata,
    embedding       = EXCLUDED.embedding,
    edited_at       = EXCLUDED.edited_at,
    deleted_at      = EXCLUDED.deleted_at,
    searchable_text = EXCLUDED.searchable_text,
    mls_group_id    = EXCLUDED.mls_group_id,
    mls_epoch       = EXCLUDED.mls_epoch,
    mls_payload     = EXCLUDED.mls_payload,
    expires_at      = EXCLUDED.expires_at,
    version         = EXCLUDED.version,
    delivery_ordinal = EXCLUDED.delivery_ordinal,
    reply_to        = EXCLUDED.reply_to;

-- Hard parity + delivery-order gate: abort if the shadow is not row-for-row
-- complete or certifies a different room prefix than the live table.
DO $parity$
DECLARE
    src              bigint;
    dst              bigint;
    ordinal_mismatch bigint;
BEGIN
    SELECT count(*) INTO src FROM messages;
    SELECT count(*) INTO dst FROM messages_partitioned;
    IF src <> dst THEN
        RAISE EXCEPTION 'CUTOVER ABORT: row parity mismatch messages=% messages_partitioned=%', src, dst;
    END IF;
    SELECT count(*)
      INTO ordinal_mismatch
      FROM messages AS live
      JOIN messages_partitioned AS shadow
        ON shadow.id = live.id AND shadow.created_at = live.created_at
     WHERE shadow.delivery_ordinal IS DISTINCT FROM live.delivery_ordinal;
    IF ordinal_mismatch <> 0 THEN
        RAISE EXCEPTION 'CUTOVER ABORT: delivery_ordinal mismatches=%', ordinal_mismatch;
    END IF;
END
$parity$;

-- ---------------------------------------------------------------------------
-- 2a) FREE THE CANONICAL INDEX NAMES.
--
-- ⚠️ Index/relation names are SCHEMA-GLOBAL in PostgreSQL (verified: creating a
-- second index with an existing name raises `relation "<name>" already exists`).
-- The OLD `messages` table still owns every canonical index name
-- (messages_search_tsv_gin, messages_embedding_hnsw, …). So we MUST rename those
-- aside BEFORE building the shadow's indexes under the same names. We suffix them
-- `__old`; they ride along when `messages` becomes `messages_old` and are dropped
-- with it in Step D. (idx_messages_* spelled out too — same global-name rule.)
-- ---------------------------------------------------------------------------
ALTER INDEX idx_messages_expires_at        RENAME TO idx_messages_expires_at__old;
ALTER INDEX idx_messages_room_mutated      RENAME TO idx_messages_room_mutated__old;
ALTER INDEX messages_blocks_gin            RENAME TO messages_blocks_gin__old;
ALTER INDEX messages_embedding_hnsw        RENAME TO messages_embedding_hnsw__old;
ALTER INDEX messages_mls_group_idx         RENAME TO messages_mls_group_idx__old;
ALTER INDEX messages_reply_to_idx          RENAME TO messages_reply_to_idx__old;
ALTER INDEX messages_room_active_id        RENAME TO messages_room_active_id__old;
ALTER INDEX messages_room_created_idx      RENAME TO messages_room_created_idx__old;
ALTER INDEX messages_room_delivery_ordinal_key RENAME TO messages_room_delivery_ordinal_key__old;
ALTER INDEX messages_searchable_trgm       RENAME TO messages_searchable_trgm__old;
ALTER INDEX messages_search_tsv_gin        RENAME TO messages_search_tsv_gin__old;
ALTER INDEX messages_sender_id_active_idx  RENAME TO messages_sender_id_active_idx__old;
-- The old PK index `messages_pkey` is also a global name; free it for the new
-- partitioned PK index (which 0148 created as `messages_partitioned_pkey`, so no
-- collision there — but rename the OLD pkey aside for cleanliness / so a future
-- re-add of a plain messages PK does not clash).
ALTER INDEX messages_pkey                  RENAME TO messages_pkey__old;

-- ---------------------------------------------------------------------------
-- 2b) BUILD ALL INDEXES on the shadow parent (cascades to every partition).
--
-- These are the EXACT current `messages` index definitions (verified by
-- pg_get_indexdef on a fully-migrated DB through migration 0148). Built ONCE
-- here, after the bulk backfill, rather than maintained per inserted row.
-- The HNSW build is the long pole — size maintenance_work_mem accordingly.
-- (We are inside the txn / under the lock, so plain CREATE INDEX, not
-- CONCURRENTLY, which cannot run in a transaction anyway.)
-- ---------------------------------------------------------------------------
CREATE INDEX idx_messages_expires_at        ON messages_partitioned USING btree (expires_at) WHERE (expires_at IS NOT NULL);
CREATE INDEX idx_messages_room_mutated      ON messages_partitioned USING btree (room_id, GREATEST(edited_at, deleted_at));
CREATE INDEX messages_blocks_gin            ON messages_partitioned USING gin (blocks);
CREATE INDEX messages_embedding_hnsw        ON messages_partitioned USING hnsw (embedding vector_cosine_ops) WITH (m='16', ef_construction='128') WHERE (deleted_at IS NULL);
CREATE INDEX messages_mls_group_idx         ON messages_partitioned USING btree (mls_group_id) WHERE (mls_group_id IS NOT NULL);
CREATE INDEX messages_reply_to_idx          ON messages_partitioned USING btree (reply_to) WHERE (reply_to IS NOT NULL);
CREATE INDEX messages_room_active_id        ON messages_partitioned USING btree (room_id, id) WHERE (deleted_at IS NULL);
CREATE INDEX messages_room_created_idx      ON messages_partitioned USING btree (room_id, created_at DESC);
-- Migration 0174 already built this UNIQUE partition-compatible parent index;
-- free the canonical name above, then give the shadow index that name. The
-- partition key is required by PostgreSQL; room_delivery_sequences + the
-- BEFORE INSERT trigger provide cross-partition ordinal allocation.
ALTER INDEX messages_partitioned_room_delivery_ordinal_key
    RENAME TO messages_room_delivery_ordinal_key;
CREATE INDEX messages_searchable_trgm       ON messages_partitioned USING gin (searchable_text gin_trgm_ops) WHERE (deleted_at IS NULL);
CREATE INDEX messages_search_tsv_gin        ON messages_partitioned USING gin (search_tsv) WHERE (deleted_at IS NULL);
CREATE INDEX messages_sender_id_active_idx  ON messages_partitioned USING btree (sender_id) WHERE (deleted_at IS NULL);

-- ---------------------------------------------------------------------------
-- 3) OUTBOUND FKs on the new parent (room_id, sender_id) — re-declared.
--    0148 deliberately did NOT copy these onto the staging table. Add them now
--    so the partitioned table enforces the same outbound integrity as the old.
-- ---------------------------------------------------------------------------
ALTER TABLE messages_partitioned
    DROP CONSTRAINT IF EXISTS messages_delivery_ordinal_positive;

ALTER TABLE messages_partitioned
    ALTER COLUMN delivery_ordinal SET NOT NULL,
    ADD CONSTRAINT messages_delivery_ordinal_positive
        CHECK (delivery_ordinal > 0);

ALTER TABLE messages_partitioned
    ADD CONSTRAINT messages_room_id_fkey
        FOREIGN KEY (room_id)   REFERENCES rooms(id)        ON DELETE CASCADE,
    ADD CONSTRAINT messages_sender_id_fkey
        FOREIGN KEY (sender_id) REFERENCES participants(id);

-- ---------------------------------------------------------------------------
-- 4) SELF-REFERENCE: messages.reply_to → messages(id, created_at).
--    Carry the *target message's* created_at in a new reply_to_created_at column,
--    then declare the composite self-FK. A reply's target is older-or-equal, so
--    this is always backfillable. NULL reply_to ⇒ NULL reply_to_created_at.
--    `MATCH FULL` accepts that all-NULL "no target" key but rejects either
--    partially-NULL shape, closing the ordinary MATCH SIMPLE bypass.
--    (Alternative (B): skip this column + FK and
--    rely on app-side reply-target validation; the self-FK is advisory.)
-- ---------------------------------------------------------------------------
ALTER TABLE messages_partitioned ADD COLUMN reply_to_created_at TIMESTAMPTZ;

UPDATE messages_partitioned child
   SET reply_to_created_at = parent.created_at
  FROM messages_partitioned parent
 WHERE child.reply_to = parent.id
   AND child.reply_to IS NOT NULL;

ALTER TABLE messages_partitioned
    ADD CONSTRAINT messages_reply_to_fkey
        FOREIGN KEY (reply_to, reply_to_created_at)
        REFERENCES messages_partitioned (id, created_at)
        MATCH FULL;

-- ---------------------------------------------------------------------------
-- 5) THE 7 INBOUND CHILD FKs — repoint each to messages_partitioned(id, created_at).
--
--    Pattern per child (strategy A, see header):
--      a. ALTER TABLE child DROP CONSTRAINT <child>_message_id_fkey;     -- old (id)-only FK
--      b. ALTER TABLE child ADD COLUMN msg_created_at TIMESTAMPTZ;       -- redundant key part
--      c. UPDATE child SET msg_created_at = (the referenced message's created_at);
--      d. ALTER TABLE child ALTER COLUMN msg_created_at SET NOT NULL;    -- it is part of a PK-grade key
--      e. ALTER TABLE child ADD CONSTRAINT … FOREIGN KEY (message_id, msg_created_at)
--             REFERENCES messages_partitioned (id, created_at) ON DELETE CASCADE;
--
--    The backfill in (c) reads the NEW parent (already row-complete + parity-gated
--    above), so every child row resolves. ON DELETE CASCADE is preserved on all 7.
--    Exact constraint names below come from pg_get_constraintdef on the live
--    catalog (all are the PG-generated `<table>_message_id_fkey`).
-- ---------------------------------------------------------------------------

-- 5.1 reactions  (PK: message_id, participant_id, emoji)  — CASCADE
ALTER TABLE reactions DROP CONSTRAINT reactions_message_id_fkey;
ALTER TABLE reactions ADD COLUMN msg_created_at TIMESTAMPTZ;
UPDATE reactions c SET msg_created_at = m.created_at
  FROM messages_partitioned m WHERE m.id = c.message_id;
ALTER TABLE reactions ALTER COLUMN msg_created_at SET NOT NULL;
ALTER TABLE reactions ADD CONSTRAINT reactions_message_id_fkey
    FOREIGN KEY (message_id, msg_created_at)
    REFERENCES messages_partitioned (id, created_at) ON DELETE CASCADE;

-- 5.2 notifications  (PK: id)  — CASCADE
ALTER TABLE notifications DROP CONSTRAINT notifications_message_id_fkey;
ALTER TABLE notifications ADD COLUMN msg_created_at TIMESTAMPTZ;
UPDATE notifications c SET msg_created_at = m.created_at
  FROM messages_partitioned m WHERE m.id = c.message_id;
ALTER TABLE notifications ALTER COLUMN msg_created_at SET NOT NULL;
ALTER TABLE notifications ADD CONSTRAINT notifications_message_id_fkey
    FOREIGN KEY (message_id, msg_created_at)
    REFERENCES messages_partitioned (id, created_at) ON DELETE CASCADE;

-- 5.3 pins  (PK: room_id, message_id)  — CASCADE
ALTER TABLE pins DROP CONSTRAINT pins_message_id_fkey;
ALTER TABLE pins ADD COLUMN msg_created_at TIMESTAMPTZ;
UPDATE pins c SET msg_created_at = m.created_at
  FROM messages_partitioned m WHERE m.id = c.message_id;
ALTER TABLE pins ALTER COLUMN msg_created_at SET NOT NULL;
ALTER TABLE pins ADD CONSTRAINT pins_message_id_fkey
    FOREIGN KEY (message_id, msg_created_at)
    REFERENCES messages_partitioned (id, created_at) ON DELETE CASCADE;

-- 5.4 bookmarks  (PK: participant_id, message_id)  — CASCADE
ALTER TABLE bookmarks DROP CONSTRAINT bookmarks_message_id_fkey;
ALTER TABLE bookmarks ADD COLUMN msg_created_at TIMESTAMPTZ;
UPDATE bookmarks c SET msg_created_at = m.created_at
  FROM messages_partitioned m WHERE m.id = c.message_id;
ALTER TABLE bookmarks ALTER COLUMN msg_created_at SET NOT NULL;
ALTER TABLE bookmarks ADD CONSTRAINT bookmarks_message_id_fkey
    FOREIGN KEY (message_id, msg_created_at)
    REFERENCES messages_partitioned (id, created_at) ON DELETE CASCADE;

-- 5.5 message_receipts  (PK: message_id, participant_id)  — CASCADE
ALTER TABLE message_receipts DROP CONSTRAINT message_receipts_message_id_fkey;
ALTER TABLE message_receipts ADD COLUMN msg_created_at TIMESTAMPTZ;
UPDATE message_receipts c SET msg_created_at = m.created_at
  FROM messages_partitioned m WHERE m.id = c.message_id;
ALTER TABLE message_receipts ALTER COLUMN msg_created_at SET NOT NULL;
ALTER TABLE message_receipts ADD CONSTRAINT message_receipts_message_id_fkey
    FOREIGN KEY (message_id, msg_created_at)
    REFERENCES messages_partitioned (id, created_at) ON DELETE CASCADE;

-- 5.6 block_interactions  (PK: id)  — CASCADE
ALTER TABLE block_interactions DROP CONSTRAINT block_interactions_message_id_fkey;
ALTER TABLE block_interactions ADD COLUMN msg_created_at TIMESTAMPTZ;
UPDATE block_interactions c SET msg_created_at = m.created_at
  FROM messages_partitioned m WHERE m.id = c.message_id;
ALTER TABLE block_interactions ALTER COLUMN msg_created_at SET NOT NULL;
ALTER TABLE block_interactions ADD CONSTRAINT block_interactions_message_id_fkey
    FOREIGN KEY (message_id, msg_created_at)
    REFERENCES messages_partitioned (id, created_at) ON DELETE CASCADE;

-- 5.7 notification_bundles  (PK: id)  — CASCADE
ALTER TABLE notification_bundles DROP CONSTRAINT notification_bundles_message_id_fkey;
ALTER TABLE notification_bundles ADD COLUMN msg_created_at TIMESTAMPTZ;
UPDATE notification_bundles c SET msg_created_at = m.created_at
  FROM messages_partitioned m WHERE m.id = c.message_id;
ALTER TABLE notification_bundles ALTER COLUMN msg_created_at SET NOT NULL;
ALTER TABLE notification_bundles ADD CONSTRAINT notification_bundles_message_id_fkey
    FOREIGN KEY (message_id, msg_created_at)
    REFERENCES messages_partitioned (id, created_at) ON DELETE CASCADE;

-- ---------------------------------------------------------------------------
-- 6) THE SWAP. After this rename the partitioned table IS `messages`; every
--    query, FK, and the app see the canonical name. The old heap is retained as
--    messages_old for the soak period (Step D drops it after a verified backup).
-- ---------------------------------------------------------------------------
-- The canonical index names were already freed in §2a (old indexes suffixed
-- `__old`) and the shadow's indexes built under the canonical names in §2b, so
-- this rename is just the two table renames — no index juggling is needed here.
-- After this, \d messages shows the canonical index names on the partitioned
-- parent and the `__old` ones ride along on messages_old (dropped in Step D).
ALTER TABLE messages              RENAME TO messages_old;
ALTER TABLE messages_partitioned  RENAME TO messages;

-- ---------------------------------------------------------------------------
-- 7) WRITE-COMPATIBILITY TRIGGERS.
--
-- Existing and current application binaries intentionally know only the stable
-- `message_id` API on child tables. Populate the redundant composite-key half
-- in the database, or every post-cutover reaction/read/notification/pin/etc.
-- INSERT would fail `23502`. The trigger also overrides a caller-supplied stale
-- timestamp with the actual parent key before the composite FK is checked.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION fill_message_child_created_at()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $child_time$
DECLARE
    resolved_created_at timestamptz;
BEGIN
    SELECT m.created_at
      INTO resolved_created_at
      FROM public.messages AS m
     WHERE m.id = NEW.message_id
     ORDER BY m.created_at
     LIMIT 1;

    IF resolved_created_at IS NULL THEN
        RAISE EXCEPTION USING
            ERRCODE = '23503',
            MESSAGE = format('message %s does not exist', NEW.message_id);
    END IF;
    NEW.msg_created_at := resolved_created_at;
    RETURN NEW;
END
$child_time$;

CREATE TRIGGER reactions_fill_msg_created_at
    BEFORE INSERT OR UPDATE OF message_id ON reactions
    FOR EACH ROW EXECUTE FUNCTION fill_message_child_created_at();
CREATE TRIGGER notifications_fill_msg_created_at
    BEFORE INSERT OR UPDATE OF message_id ON notifications
    FOR EACH ROW EXECUTE FUNCTION fill_message_child_created_at();
CREATE TRIGGER pins_fill_msg_created_at
    BEFORE INSERT OR UPDATE OF message_id ON pins
    FOR EACH ROW EXECUTE FUNCTION fill_message_child_created_at();
CREATE TRIGGER bookmarks_fill_msg_created_at
    BEFORE INSERT OR UPDATE OF message_id ON bookmarks
    FOR EACH ROW EXECUTE FUNCTION fill_message_child_created_at();
CREATE TRIGGER message_receipts_fill_msg_created_at
    BEFORE INSERT OR UPDATE OF message_id ON message_receipts
    FOR EACH ROW EXECUTE FUNCTION fill_message_child_created_at();
CREATE TRIGGER block_interactions_fill_msg_created_at
    BEFORE INSERT OR UPDATE OF message_id ON block_interactions
    FOR EACH ROW EXECUTE FUNCTION fill_message_child_created_at();
CREATE TRIGGER notification_bundles_fill_msg_created_at
    BEFORE INSERT OR UPDATE OF message_id ON notification_bundles
    FOR EACH ROW EXECUTE FUNCTION fill_message_child_created_at();

-- `reply_to_created_at` is the same redundant-key pattern on the partitioned
-- parent itself. Old and current writers provide only `reply_to`, so populate
-- it on both INSERT and edits that change the target.
CREATE OR REPLACE FUNCTION fill_message_reply_created_at()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $reply_time$
DECLARE
    resolved_created_at timestamptz;
BEGIN
    IF NEW.reply_to IS NULL THEN
        NEW.reply_to_created_at := NULL;
        RETURN NEW;
    END IF;

    SELECT m.created_at
      INTO resolved_created_at
      FROM public.messages AS m
     WHERE m.id = NEW.reply_to
     ORDER BY m.created_at
     LIMIT 1;
    IF resolved_created_at IS NULL THEN
        RAISE EXCEPTION USING
            ERRCODE = '23503',
            MESSAGE = format('reply target %s does not exist', NEW.reply_to);
    END IF;
    NEW.reply_to_created_at := resolved_created_at;
    RETURN NEW;
END
$reply_time$;

CREATE TRIGGER messages_fill_reply_created_at
    BEFORE INSERT OR UPDATE OF reply_to, reply_to_created_at ON messages
    FOR EACH ROW EXECUTE FUNCTION fill_message_reply_created_at();

-- The additive preparation function targets the shadow relation by name. After
-- the swap that name no longer exists, so replace it with a relation-aware
-- implementation. It also remains correct during a pre-decommission rollback:
-- use canonical `messages` when it is partitioned, otherwise maintain the
-- parked `messages_partitioned` parent. Existing children deliberately retain
-- their `messages_partitioned_YYYYMM` names across the parent rename; continuing
-- that prefix avoids creating overlapping bounds under a second name.
CREATE OR REPLACE FUNCTION ensure_messages_partitions(
    ahead INT DEFAULT 3
) RETURNS void
LANGUAGE plpgsql
AS $partition_maintenance$
DECLARE
    parent_name TEXT;
    month_start DATE := date_trunc('month', now() AT TIME ZONE 'UTC')::date;
    lower_bound DATE;
    partition_name TEXT;
BEGIN
    IF EXISTS (
        SELECT 1
          FROM pg_class AS relation
          JOIN pg_namespace AS namespace
            ON namespace.oid = relation.relnamespace
         WHERE namespace.nspname = 'public'
           AND relation.relname = 'messages'
           AND relation.relkind = 'p'
    ) THEN
        parent_name := 'messages';
    ELSIF EXISTS (
        SELECT 1
          FROM pg_class AS relation
          JOIN pg_namespace AS namespace
            ON namespace.oid = relation.relnamespace
         WHERE namespace.nspname = 'public'
           AND relation.relname = 'messages_partitioned'
           AND relation.relkind = 'p'
    ) THEN
        parent_name := 'messages_partitioned';
    ELSE
        RETURN;
    END IF;

    FOR lower_bound IN
        SELECT generate_series(
            month_start - interval '1 month',
            month_start + (ahead || ' months')::interval,
            interval '1 month'
        )::date
    LOOP
        partition_name :=
            'messages_partitioned_' || to_char(lower_bound, 'YYYYMM');
        IF to_regclass(format('public.%I', partition_name)) IS NULL THEN
            EXECUTE format(
                'CREATE TABLE public.%I PARTITION OF public.%I '
                || 'FOR VALUES FROM (%L) TO (%L)',
                partition_name,
                parent_name,
                lower_bound,
                (lower_bound + interval '1 month')::date
            );
        END IF;
    END LOOP;
END
$partition_maintenance$;

COMMIT;

-- ===========================================================================
-- POST-COMMIT VERIFICATION (runbook §5) — run BEFORE dropping messages_old:
--
--   -- parity (account for window writes):
--   SELECT (SELECT count(*) FROM messages) AS new_ct,
--          (SELECT count(*) FROM messages_old) AS old_ct;
--
--   -- 8 FKs present + validated on the new parent:
--   SELECT conname, convalidated
--     FROM pg_constraint
--    WHERE confrelid = 'messages'::regclass AND contype='f' ORDER BY conname;
--
--   -- write-compatibility triggers must cover all seven children + reply_to:
--   SELECT tgrelid::regclass AS table_name, tgname
--     FROM pg_trigger
--    WHERE NOT tgisinternal
--      AND tgname IN (
--        'reactions_fill_msg_created_at',
--        'notifications_fill_msg_created_at',
--        'pins_fill_msg_created_at',
--        'bookmarks_fill_msg_created_at',
--        'message_receipts_fill_msg_created_at',
--        'block_interactions_fill_msg_created_at',
--        'notification_bundles_fill_msg_created_at',
--        'messages_fill_reply_created_at'
--      )
--    ORDER BY table_name::text, tgname;
--
--   -- reply FK must be MATCH FULL (`confmatchtype = 'f'`), and a direct
--   -- redundant-key update must be repaired by the trigger:
--   SELECT conname, confmatchtype
--     FROM pg_constraint
--    WHERE conrelid='messages'::regclass
--      AND conname='messages_reply_to_fkey';
--   UPDATE messages
--      SET reply_to_created_at = NULL
--    WHERE reply_to IS NOT NULL;
--   SELECT count(*) AS incomplete_reply_keys
--     FROM messages
--    WHERE (reply_to IS NULL) <> (reply_to_created_at IS NULL);
--
--   -- partition maintenance must target the renamed canonical parent:
--   SELECT count(*) AS partitions_before
--     FROM pg_inherits WHERE inhparent='messages'::regclass;
--   SELECT ensure_messages_partitions(12);
--   SELECT count(*) AS partitions_after
--     FROM pg_inherits WHERE inhparent='messages'::regclass;
--
--   -- referential integrity per child (must all be 0):
--   SELECT 'reactions'           AS child, count(*) FROM reactions          r LEFT JOIN messages m ON m.id=r.message_id WHERE m.id IS NULL
--   UNION ALL SELECT 'notifications',       count(*) FROM notifications     n LEFT JOIN messages m ON m.id=n.message_id WHERE m.id IS NULL
--   UNION ALL SELECT 'pins',                count(*) FROM pins              p LEFT JOIN messages m ON m.id=p.message_id WHERE m.id IS NULL
--   UNION ALL SELECT 'bookmarks',           count(*) FROM bookmarks         b LEFT JOIN messages m ON m.id=b.message_id WHERE m.id IS NULL
--   UNION ALL SELECT 'message_receipts',    count(*) FROM message_receipts  x LEFT JOIN messages m ON m.id=x.message_id WHERE m.id IS NULL
--   UNION ALL SELECT 'block_interactions',  count(*) FROM block_interactions y LEFT JOIN messages m ON m.id=y.message_id WHERE m.id IS NULL
--   UNION ALL SELECT 'notification_bundles',count(*) FROM notification_bundles z LEFT JOIN messages m ON m.id=z.message_id WHERE m.id IS NULL;
--
--   -- FTS, room keyset, reply_to still work + partition pruning:
--   EXPLAIN ANALYZE SELECT id FROM messages
--     WHERE search_tsv @@ to_tsquery('english','hello') AND deleted_at IS NULL;
--   EXPLAIN ANALYZE SELECT id FROM messages
--     WHERE room_id = :room AND created_at < now() ORDER BY created_at DESC LIMIT 50;
--   SELECT count(*) FROM messages WHERE reply_to IS NOT NULL;
--
--   -- delivery ordinal schema, uniqueness, trigger, and sequence coverage:
--   SELECT attnotnull
--     FROM pg_attribute
--    WHERE attrelid='messages'::regclass
--      AND attname='delivery_ordinal' AND NOT attisdropped;
--   SELECT count(*) AS invalid_ordinals
--     FROM messages WHERE delivery_ordinal <= 0 OR delivery_ordinal IS NULL;
--   SELECT count(*) AS duplicate_room_ordinals
--     FROM (
--       SELECT room_id, delivery_ordinal
--         FROM messages
--        GROUP BY room_id, delivery_ordinal HAVING count(*) > 1
--     ) duplicates;
--   SELECT count(*) AS sequence_coverage_gaps
--     FROM (
--       SELECT m.room_id, max(m.delivery_ordinal) AS max_ordinal,
--              s.last_ordinal
--         FROM messages m
--         LEFT JOIN room_delivery_sequences s ON s.room_id=m.room_id
--        GROUP BY m.room_id, s.last_ordinal
--       HAVING s.last_ordinal IS NULL
--           OR s.last_ordinal < max(m.delivery_ordinal)
--     ) gaps;
--   SELECT tgname, tgenabled
--     FROM pg_trigger
--    WHERE tgrelid='messages'::regclass
--      AND tgname='messages_assign_delivery_ordinal' AND NOT tgisinternal;
-- ===========================================================================

-- ===========================================================================
-- ROLLBACK
--
-- DURING the txn (before COMMIT): any error => automatic ROLLBACK; the original
--   `messages` is byte-for-byte intact (it was only RENAMEd at the very end). The
--   shadow table + its half-built FKs vanish with the rollback. Nothing to undo.
--   To retry, re-run from BEGIN after fixing the cause.
--
-- AFTER COMMIT but BEFORE `DROP TABLE messages_old` (something is wrong in prod):
--   Keep writes gated and execute the exact companion artifact
--   `docs/runbooks/messages-cutover-rollback.sql`. It replays every
--   post-cutover insert/update/delete into `messages_old`, proves row parity,
--   expands all seven FK reversals, restores every canonical index/PK name,
--   reverses the table swap, and runs catalog gates in one transaction.
--
-- ULTIMATE FALLBACK: restore the pre-window backup (loses writes since backup —
--   last resort; this is why writes are paused at cutover and a soak precedes the
--   DROP of messages_old). See runbook §6.
--
-- DECOMMISSION (Step D, after soak + verified backup of the new table):
--   DROP TABLE messages_old;     -- reclaim the doubled disk
--   -- then schedule ensure_messages_partitions() (or pg_partman) as a cron.
-- ===========================================================================
