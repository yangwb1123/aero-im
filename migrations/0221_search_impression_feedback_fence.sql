-- Server-issued proof for advanced-search click feedback.
--
-- Historical search_click_events remain valid and nullable impression_id keeps
-- a rolling old server binary operational. Every impression-backed write is
-- nevertheless fenced by identity/query/result/rank triggers, and new ranks and
-- queries receive NOT VALID checks (enforced for new rows without scanning old
-- history).

CREATE OR REPLACE FUNCTION aero_search_result_snapshot_valid(
    requested_results uuid[]
) RETURNS boolean
LANGUAGE sql
IMMUTABLE
PARALLEL SAFE
AS $$
    SELECT requested_results IS NOT NULL
       AND cardinality(requested_results) BETWEEN 0 AND 100
       AND array_position(requested_results, NULL) IS NULL
       AND cardinality(requested_results) = (
           SELECT COUNT(DISTINCT result_id)
             FROM unnest(requested_results) AS listed(result_id)
       )
$$;

CREATE TABLE IF NOT EXISTS search_impressions (
    id                UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    participant_id    UUID        NOT NULL
                                  REFERENCES participants(id) ON DELETE CASCADE,
    workspace_id      UUID        NOT NULL
                                  REFERENCES workspaces(id) ON DELETE CASCADE,
    query_text        TEXT        NOT NULL,
    result_ids        UUID[]      NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT statement_timestamp(),
    expires_at        TIMESTAMPTZ NOT NULL
                                  DEFAULT statement_timestamp() + INTERVAL '15 minutes',
    clicked_result_id UUID,
    clicked_at        TIMESTAMPTZ,
    CONSTRAINT search_impressions_query_normalized_chk CHECK (
        char_length(query_text) BETWEEN 1 AND 2000
        AND query_text = btrim(
            regexp_replace(query_text, '[[:space:]]+', ' ', 'g')
        )
    ),
    CONSTRAINT search_impressions_result_snapshot_chk CHECK (
        aero_search_result_snapshot_valid(result_ids)
    ),
    CONSTRAINT search_impressions_ttl_chk CHECK (
        expires_at > created_at
        AND expires_at <= created_at + INTERVAL '15 minutes'
    ),
    CONSTRAINT search_impressions_click_state_chk CHECK (
        (clicked_result_id IS NULL AND clicked_at IS NULL)
        OR (clicked_result_id IS NOT NULL AND clicked_at IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS search_impressions_owner_expiry_idx
    ON search_impressions (participant_id, expires_at);

CREATE INDEX IF NOT EXISTS search_impressions_unconsumed_expiry_idx
    ON search_impressions (expires_at)
    WHERE clicked_result_id IS NULL;

CREATE INDEX IF NOT EXISTS search_impressions_workspace_created_idx
    ON search_impressions (workspace_id, created_at);

ALTER TABLE search_click_events
    ADD COLUMN IF NOT EXISTS impression_id UUID;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'search_click_events'::regclass
           AND conname = 'search_click_events_impression_id_fkey'
    ) THEN
        ALTER TABLE search_click_events
            ADD CONSTRAINT search_click_events_impression_id_fkey
            FOREIGN KEY (impression_id)
            REFERENCES search_impressions(id)
            ON DELETE SET NULL;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'search_click_events'::regclass
           AND conname = 'search_click_events_rank_bounds_chk'
    ) THEN
        ALTER TABLE search_click_events
            ADD CONSTRAINT search_click_events_rank_bounds_chk
            CHECK (result_rank BETWEEN 0 AND 99) NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'search_click_events'::regclass
           AND conname = 'search_click_events_query_normalized_chk'
    ) THEN
        ALTER TABLE search_click_events
            ADD CONSTRAINT search_click_events_query_normalized_chk
            CHECK (
                char_length(query_text) <= 2000
                -- A rolling pre-0221 node records only parsed free-text terms,
                -- which are legitimately empty for an operators-only query.
                -- Proof-backed writers persist the complete normalized query
                -- and must never use the compatibility empty representation.
                AND (
                    impression_id IS NULL
                    OR char_length(query_text) >= 1
                )
                AND query_text = btrim(
                    regexp_replace(query_text, '[[:space:]]+', ' ', 'g')
                )
            ) NOT VALID;
    END IF;
END
$$;

CREATE UNIQUE INDEX IF NOT EXISTS search_click_events_impression_uidx
    ON search_click_events (impression_id)
    WHERE impression_id IS NOT NULL;

CREATE OR REPLACE FUNCTION search_impression_write_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    clicked_rank integer;
    consumed_at timestamptz;
    issued_at timestamptz;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF NEW.id IS DISTINCT FROM OLD.id
           OR NEW.participant_id IS DISTINCT FROM OLD.participant_id
           OR NEW.workspace_id IS DISTINCT FROM OLD.workspace_id
           OR NEW.query_text IS DISTINCT FROM OLD.query_text
           OR NEW.result_ids IS DISTINCT FROM OLD.result_ids
           OR NEW.created_at IS DISTINCT FROM OLD.created_at
           OR NEW.expires_at IS DISTINCT FROM OLD.expires_at THEN
            RAISE EXCEPTION 'search impression identity and proof are immutable'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'search_impressions_identity_immutable_chk';
        END IF;

        IF OLD.clicked_result_id IS NOT NULL
           AND (
               NEW.clicked_result_id IS DISTINCT FROM OLD.clicked_result_id
               OR NEW.clicked_at IS DISTINCT FROM OLD.clicked_at
           ) THEN
            RAISE EXCEPTION 'consumed search impression is immutable'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'search_impressions_consumed_immutable_chk';
        END IF;

        IF OLD.clicked_result_id IS NULL
           AND NEW.clicked_result_id IS NULL
           AND NEW.clicked_at IS DISTINCT FROM OLD.clicked_at THEN
            RAISE EXCEPTION 'search impression click state is inconsistent'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'search_impressions_click_state_chk';
        END IF;
    ELSE
        issued_at := clock_timestamp();
        IF NEW.created_at > issued_at
           OR NEW.expires_at > issued_at + INTERVAL '15 minutes' THEN
            RAISE EXCEPTION 'search impression cannot be issued in the future'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'search_impressions_issue_time_chk';
        END IF;

        IF NEW.clicked_result_id IS NOT NULL OR NEW.clicked_at IS NOT NULL THEN
            RAISE EXCEPTION 'new search impression must be unconsumed'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'search_impressions_initial_state_chk';
        END IF;
    END IF;

    IF NOT aero_participant_has_effective_workspace_access(
        NEW.workspace_id,
        NEW.participant_id
    ) THEN
        RAISE EXCEPTION 'search impression participant lacks workspace access'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'search_impressions_effective_access_chk';
    END IF;

    IF TG_OP = 'INSERT' THEN
        IF EXISTS (
            SELECT 1
              FROM unnest(NEW.result_ids) AS listed(result_id)
              LEFT JOIN messages message ON message.id = listed.result_id
              LEFT JOIN rooms room ON room.id = message.room_id
              LEFT JOIN room_members membership
                ON membership.room_id = message.room_id
               AND membership.participant_id = NEW.participant_id
             WHERE message.id IS NULL
                OR room.workspace_id IS DISTINCT FROM NEW.workspace_id
                OR message.deleted_at IS NOT NULL
                OR (
                    message.expires_at IS NOT NULL
                    AND message.expires_at <= clock_timestamp()
                )
                OR membership.participant_id IS NULL
        ) THEN
            RAISE EXCEPTION 'search impression contains an inaccessible result'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'search_impressions_result_scope_chk';
        END IF;
    ELSIF OLD.clicked_result_id IS NULL
          AND NEW.clicked_result_id IS NOT NULL
          AND NOT EXISTS (
              SELECT 1
                FROM messages message
                JOIN rooms room ON room.id = message.room_id
                JOIN room_members membership
                  ON membership.room_id = message.room_id
                 AND membership.participant_id = NEW.participant_id
               WHERE message.id = NEW.clicked_result_id
                 AND room.workspace_id = NEW.workspace_id
                 AND message.deleted_at IS NULL
                 AND (
                     message.expires_at IS NULL
                     OR message.expires_at > clock_timestamp()
                 )
          ) THEN
        -- A result page is a historical snapshot. Other entries may age out
        -- before the user clicks; only the selected result needs to remain
        -- live and accessible at consumption time.
        RAISE EXCEPTION 'selected search result is inaccessible'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'search_impressions_result_scope_chk';
    END IF;

    IF NEW.clicked_result_id IS NOT NULL THEN
        IF TG_OP = 'UPDATE' AND OLD.clicked_result_id IS NULL THEN
            -- The database, not a raw-SQL caller, owns the consume timestamp.
            -- Enforce expiry exactly once at the unconsumed -> consumed
            -- transition. A matching analytics row may then be materialized
            -- from this immutable proof even if that insert crosses the TTL
            -- boundary.
            consumed_at := clock_timestamp();
            IF consumed_at >= NEW.expires_at THEN
                RAISE EXCEPTION 'search impression click is outside its proof'
                    USING ERRCODE = '23514',
                          CONSTRAINT = 'search_impressions_click_proof_chk';
            END IF;
            NEW.clicked_at := consumed_at;
        END IF;

        clicked_rank := array_position(NEW.result_ids, NEW.clicked_result_id);
        IF clicked_rank IS NULL
           OR clicked_rank NOT BETWEEN 1 AND 100
           OR NEW.clicked_at < NEW.created_at
           OR NEW.clicked_at > NEW.expires_at THEN
            RAISE EXCEPTION 'search impression click is outside its proof'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'search_impressions_click_proof_chk';
        END IF;
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS search_impression_write_fence
    ON search_impressions;
CREATE TRIGGER search_impression_write_fence
    BEFORE INSERT OR UPDATE ON search_impressions
    FOR EACH ROW
    EXECUTE FUNCTION search_impression_write_fence();

CREATE OR REPLACE FUNCTION search_click_impression_fence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    proof search_impressions%ROWTYPE;
    expected_rank integer;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF OLD.impression_id IS NOT NULL THEN
            -- Retention of an impression intentionally preserves analytics by
            -- letting its FK clear only impression_id. No other proof-backed
            -- field may change.
            IF NEW.impression_id IS NULL
               AND NEW.id IS NOT DISTINCT FROM OLD.id
               AND NEW.participant_id IS NOT DISTINCT FROM OLD.participant_id
               AND NEW.workspace_id IS NOT DISTINCT FROM OLD.workspace_id
               AND NEW.query_text IS NOT DISTINCT FROM OLD.query_text
               AND NEW.result_id IS NOT DISTINCT FROM OLD.result_id
               AND NEW.result_rank IS NOT DISTINCT FROM OLD.result_rank
               AND NEW.clicked_at IS NOT DISTINCT FROM OLD.clicked_at THEN
                RETURN NEW;
            END IF;

            RAISE EXCEPTION 'impression-backed search click is immutable'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'search_click_events_proof_immutable_chk';
        END IF;

        -- Rolling old nodes still INSERT rows without an impression id. They
        -- never need to UPDATE analytics history, so keep that compatibility
        -- ingress append-only instead of leaving a raw-SQL mutation bypass.
        RAISE EXCEPTION 'legacy search click is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'search_click_events_legacy_immutable_chk';
    END IF;

    -- NULL is the bounded compatibility ingress for pre-0221 history and old
    -- nodes during a rolling deploy. It necessarily cannot carry server proof;
    -- rank/query CHECK constraints still apply. The 0221 request path always
    -- supplies proof.
    IF NEW.impression_id IS NULL THEN
        RETURN NEW;
    END IF;

    SELECT *
      INTO proof
      FROM search_impressions
     WHERE id = NEW.impression_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'search click impression does not exist'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'search_click_events_impression_proof_chk';
    END IF;

    expected_rank := array_position(proof.result_ids, NEW.result_id);
    IF expected_rank IS NULL
       OR NEW.participant_id IS DISTINCT FROM proof.participant_id
       OR NEW.workspace_id IS DISTINCT FROM proof.workspace_id
       OR NEW.query_text IS DISTINCT FROM proof.query_text
       OR NEW.result_rank IS DISTINCT FROM expected_rank - 1
       OR proof.clicked_result_id IS DISTINCT FROM NEW.result_id
       OR proof.clicked_at IS DISTINCT FROM NEW.clicked_at
       OR NEW.clicked_at < proof.created_at
       OR NEW.clicked_at > proof.expires_at THEN
        RAISE EXCEPTION 'search click does not match impression proof'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'search_click_events_impression_proof_chk';
    END IF;

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS search_click_impression_fence
    ON search_click_events;
CREATE TRIGGER search_click_impression_fence
    BEFORE INSERT OR UPDATE ON search_click_events
    FOR EACH ROW
    EXECUTE FUNCTION search_click_impression_fence();

COMMENT ON TABLE search_impressions IS
    '15-minute server proof retained only to the configured analytics cutoff, binding one participant/workspace/query to an ordered advanced-search result snapshot';

COMMENT ON COLUMN search_click_events.impression_id IS
    'Server-issued proof for 0221+ clicks; NULL preserves historical and rolling-upgrade rows';
