# 0172–0176 migration-first rolling upgrade

This migration set changes tenant-sensitive blob and AI-profile storage and adds
Canvas operation idempotency. Apply it in this order:

1. Rebuild `aero-cli` from the exact source being deployed. Migrations are
   compile-time embedded, so a previously built binary cannot see the new SQL.
2. Keep `AERO_AI_CROSS_ROOM_PROFILE` disabled on the old fleet and run the
   attachment inventory below before migration.
3. Apply the complete migration chain through
   `0176_rolling_upgrade_fences.sql` before deploying application code.
4. If migration `0172` or `0176` reports SQLSTATE `23514`, remediate the
   attachment references listed below and rerun the chain. Each migration is
   transactional; its failed version is not recorded or partially installed.
5. Deploy the tenant-aware application binary and drain all pre-0173 pods.
6. Enable `AERO_AI_CROSS_ROOM_PROFILE` only after no pre-0173 application pod is
   serving traffic.

The rolling window has these deliberate behaviours:

- A pre-0175 Canvas writer may omit `client_op_id`. The database fills it from
  that operation's already unique `id`; tenant-aware writers keep their supplied
  retry key and preserve `(canvas_id, author_id, client_op_id)` idempotency.
- A pre-0172 message writer is still subject to the database attachment trigger.
  Every File or Voice reference that resolves to an existing blob must match the
  target room's immutable workspace; legacy unscoped blobs and cross-workspace
  blobs are rejected. An old opaque id with no metadata remains non-downloadable
  (tenant-aware service code rejects it before insert). Download authorization
  repeats the workspace equality as defence in depth. Before installing the
  trigger, `0172` locks and audits both `messages` and
  `messages_partitioned`. Any resolvable historical mismatch aborts the
  migration instead of silently preserving polluted authorization state. The
  trigger is installed on both tables so an online partition cutover cannot
  promote an unguarded shadow table.
- A pre-0173 AI-profile reader cannot supply a workspace predicate. The legacy
  `participant_ai_profiles` name is therefore a read-empty, write-rejecting
  compatibility view. Such a pod loses optional personalization (fail closed)
  and its old single-key upsert fails; it never receives an arbitrary tenant's
  profile. New binaries use `participant_ai_profiles_scoped`. Participant
  tombstoning deletes scoped profiles in a database trigger, so GDPR erasure
  remains safe when an old pod handles the request. The shape migration also
  deletes profiles whose participants were already tombstoned before the
  trigger existed, and `0176` idempotently reasserts that historical cleanup. An old
  `ON CONFLICT (participant_id)` write fails visibly at PostgreSQL rather than
  modifying tenant data; treat that database error as the signal that a legacy
  pod still has the opt-in feature enabled.

The compatibility fence is committed in the same transaction as each breaking
shape: blob audit/trigger in `0172`, AI scoped rename/view/erasure trigger in
`0173`, and Canvas fallback trigger before the `NOT NULL` in `0175`. `0176`
idempotently reasserts all three contracts for defence in depth. Once this
migration set is released, do not edit those SQL files or repair checksums in
`_sqlx_migrations`; ship any later correction as a new migration.

## Attachment audit and remediation

Run this session-local conversion helper and read-only inventory before the
migration. Using `to_jsonb(blob)` makes the same query work both before and after
`0172` adds `blobs.workspace_id`. It reports only references that resolve to a
blob; opaque legacy ids cannot grant a download and are intentionally not
rewritten.

```sql
CREATE OR REPLACE FUNCTION pg_temp.aero_uuid_to_ulid(input_uuid UUID)
RETURNS TEXT
LANGUAGE plpgsql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $$
DECLARE
    raw_bits BIT(130);
    alphabet CONSTANT TEXT := '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
    result TEXT := '';
    position INTEGER;
    digit INTEGER;
BEGIN
    raw_bits := B'00'
        || (('x' || replace(input_uuid::text, '-', ''))::BIT(128));
    FOR position IN 0..25 LOOP
        digit := substring(raw_bits FROM position * 5 + 1 FOR 5)::INTEGER;
        result := result || substr(alphabet, digit + 1, 1);
    END LOOP;
    RETURN result;
END;
$$;

WITH message_sources AS (
    SELECT 'messages' AS source, id, room_id, blocks
      FROM messages
    UNION ALL
    SELECT 'messages_partitioned', id, room_id, blocks
      FROM messages_partitioned
),
attachment_refs AS (
    SELECT source,
           message.id AS message_id,
           room.workspace_id AS room_workspace_id,
           upper(element ->> 'blob_id') AS public_blob_id
      FROM message_sources AS message
      JOIN rooms AS room ON room.id = message.room_id
      CROSS JOIN LATERAL jsonb_array_elements(
          CASE
              WHEN jsonb_typeof(message.blocks) = 'array'
                  THEN message.blocks
              ELSE '[]'::jsonb
          END
      ) AS element
     WHERE element ->> 'type' IN ('file', 'voice')
)
SELECT attachment.source,
       attachment.message_id,
       blob.id AS blob_id,
       attachment.room_workspace_id,
       (to_jsonb(blob) ->> 'workspace_id')::uuid AS blob_workspace_id
  FROM attachment_refs AS attachment
  JOIN blobs AS blob
    ON pg_temp.aero_uuid_to_ulid(blob.id) = attachment.public_blob_id
 WHERE (to_jsonb(blob) ->> 'workspace_id')::uuid
       IS DISTINCT FROM attachment.room_workspace_id
 ORDER BY attachment.source, attachment.message_id;
```

If a deployment predates `0172`, blobs have no authenticated workspace history,
so every resolving historical File/Voice reference is conservatively reported
as unscoped. This is intentional: inferring ownership from the referencing
message would legitimize the exact cross-tenant pollution being prevented.

For every returned message, preserve the original row in the incident/audit
store, then quarantine/remove only the mismatched File/Voice block through the
normal audited message-edit path. After migration, re-upload/import the object
through the authenticated target-workspace reservation path and restore the
attachment as a new audited edit. Do not guess or reassign a blob's immutable
`workspace_id`: that would transfer an object across tenants and can invalidate
other legitimate references. If the source is the partition shadow, repeat the
same correction there or restart its backfill from the cleaned canonical row.
Rerun the inventory until it is empty, then rerun the complete migration chain.

The standard pre-cutover schema contains `messages_partitioned`. If it has
already been promoted and that relation no longer exists, omit only that
`UNION ALL` branch; migrations themselves detect the relation dynamically.

Useful post-migration checks:

```sql
SELECT relkind
  FROM pg_class
 WHERE oid = 'participant_ai_profiles'::regclass; -- v (view)

SELECT to_regclass('participant_ai_profiles_scoped'); -- non-null

SELECT tgname
  FROM pg_trigger
 WHERE tgname IN (
   'canvas_ops_fill_legacy_client_id',
   'messages_enforce_blob_workspace_scope',
   'participants_erase_scoped_ai_profiles'
 )
   AND NOT tgisinternal;

SELECT tgrelid::regclass AS guarded_relation, tgname
  FROM pg_trigger
 WHERE tgname = 'messages_enforce_blob_workspace_scope'
   AND NOT tgisinternal
 ORDER BY guarded_relation::text;
```
