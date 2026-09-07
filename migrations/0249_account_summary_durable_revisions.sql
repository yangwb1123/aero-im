-- Aero IM source-owned migration: durable account-summary revision primitives.
--
-- This file is intentionally not in aero-id/migrations. Aero IM owns this
-- database. It installs replay-safe durable state and controlled seed tooling;
-- the strict endpoint contract must remain disabled until the source can bind
-- its opaque signed account IDs to mutations (or implements the approved
-- canonical-state fingerprint path).

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE IF NOT EXISTS account_summary_revisions (
    account_id TEXT PRIMARY KEY,
    revision BIGINT NOT NULL CHECK (revision > 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS account_summary_revision_control (
    control_id BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (control_id),
    state TEXT NOT NULL CHECK (state IN ('active', 'paused')),
    epoch BIGINT NOT NULL CHECK (epoch > 0),
    active_run_id UUID,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO account_summary_revision_control(control_id, state, epoch)
VALUES (TRUE, 'active', 1)
ON CONFLICT (control_id) DO NOTHING;

CREATE TABLE IF NOT EXISTS account_summary_revision_seed_runs (
    run_id UUID PRIMARY KEY,
    manifest_sha256 TEXT NOT NULL CHECK (manifest_sha256 ~ '^[0-9a-f]{64}$'),
    state TEXT NOT NULL CHECK (state IN ('running', 'completed', 'aborted')),
    requested_by TEXT NOT NULL CHECK (length(trim(requested_by)) BETWEEN 1 AND 256),
    source_region TEXT NOT NULL CHECK (length(trim(source_region)) BETWEEN 1 AND 128),
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    rows_total BIGINT NOT NULL DEFAULT 0 CHECK (rows_total >= 0),
    manifest_locked BOOLEAN NOT NULL DEFAULT FALSE,
    rows_applied BIGINT NOT NULL DEFAULT 0 CHECK (rows_applied >= 0),
    last_error TEXT NOT NULL DEFAULT '',
    UNIQUE (manifest_sha256)
);

CREATE TABLE IF NOT EXISTS account_summary_revision_seed_manifest (
    run_id UUID NOT NULL REFERENCES account_summary_revision_seed_runs(run_id) ON DELETE CASCADE,
    account_id TEXT NOT NULL CHECK (length(trim(account_id)) BETWEEN 1 AND 512 AND account_id !~ '[,\r\n]'),
    max_existing_revision BIGINT NOT NULL CHECK (max_existing_revision >= 0),
    seed_revision BIGINT NOT NULL CHECK (seed_revision > 0),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'applied')),
    attempts BIGINT NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    applied_at TIMESTAMPTZ,
    PRIMARY KEY (run_id, account_id)
);

CREATE INDEX IF NOT EXISTS account_summary_revision_seed_claim
    ON account_summary_revision_seed_manifest(run_id, state, account_id);

-- The loader may insert rows only before validation. Once rows_total is set,
-- identity and seed inputs cannot be changed by a retrying worker or operator.
CREATE OR REPLACE FUNCTION guard_account_summary_revision_seed_manifest()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    locked_run BIGINT;
    manifest_locked BOOLEAN;
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'account summary revision seed manifest is immutable';
    END IF;
    IF TG_OP = 'INSERT' THEN
        SELECT rows_total, manifest_locked INTO locked_run, manifest_locked
          FROM account_summary_revision_seed_runs
         WHERE run_id = NEW.run_id FOR SHARE;
        IF locked_run IS NULL OR manifest_locked OR locked_run > 0 THEN
            RAISE EXCEPTION 'seed manifest is closed for loading';
        END IF;
        RETURN NEW;
    END IF;
    IF OLD.run_id <> NEW.run_id OR OLD.account_id <> NEW.account_id OR
       OLD.max_existing_revision <> NEW.max_existing_revision OR OLD.seed_revision <> NEW.seed_revision THEN
        RAISE EXCEPTION 'seed manifest identity is immutable';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS account_summary_revision_seed_manifest_guard
    ON account_summary_revision_seed_manifest;
CREATE TRIGGER account_summary_revision_seed_manifest_guard
BEFORE INSERT OR UPDATE OR DELETE ON account_summary_revision_seed_manifest
FOR EACH ROW EXECUTE FUNCTION guard_account_summary_revision_seed_manifest();

-- An append-only operational audit stream. The seed run row is mutable state;
-- this table is the durable record of who paused, seeded, resumed, or aborted.
CREATE TABLE IF NOT EXISTS account_summary_revision_seed_audit (
    audit_id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id UUID NOT NULL REFERENCES account_summary_revision_seed_runs(run_id),
    action TEXT NOT NULL CHECK (action IN ('registered', 'paused', 'batch_applied', 'completed', 'resumed', 'aborted')),
    actor TEXT NOT NULL CHECK (length(trim(actor)) BETWEEN 1 AND 256),
    details JSONB NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(details) = 'object'),
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE OR REPLACE FUNCTION account_summary_revision_seed_audit(
    p_run_id UUID, p_action TEXT, p_actor TEXT, p_details JSONB DEFAULT '{}'::jsonb
) RETURNS VOID LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO account_summary_revision_seed_audit(run_id, action, actor, details)
    VALUES (p_run_id, p_action, p_actor, COALESCE(p_details, '{}'::jsonb));
END;
$$;

CREATE OR REPLACE FUNCTION reject_account_summary_revision_seed_audit_change()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'account summary revision seed audit is append-only';
END;
$$;

DROP TRIGGER IF EXISTS account_summary_revision_seed_audit_append_only
    ON account_summary_revision_seed_audit;
CREATE TRIGGER account_summary_revision_seed_audit_append_only
BEFORE UPDATE OR DELETE ON account_summary_revision_seed_audit
FOR EACH ROW EXECUTE FUNCTION reject_account_summary_revision_seed_audit_change();

CREATE OR REPLACE FUNCTION account_summary_revision_seed_value(p_max BIGINT)
RETURNS BIGINT LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN
    IF p_max < 0 OR p_max = 9223372036854775807 THEN
        RAISE EXCEPTION 'account summary revision seed would overflow int64';
    END IF;
    RETURN p_max + 1;
END;
$$;

-- The allocator is deliberately not wired to ParticipantId-based triggers.
-- `account_id` is an opaque identifier owned by the signed target contract;
-- Aero IM cannot infer it from its local participant UUID. Until an
-- owner-approved mapping or the design-approved canonical-state fingerprint
-- path is implemented, source mutation advancement remains a rollout blocker.
-- Keeping this exact-key allocator available preserves durable seed/control
-- state without silently inventing an identity mapping.
CREATE OR REPLACE FUNCTION next_account_summary_revision(p_account_id TEXT)
RETURNS BIGINT LANGUAGE plpgsql AS $$
DECLARE
    current_state TEXT;
    allocated BIGINT;
BEGIN
    SELECT state INTO current_state
      FROM account_summary_revision_control
     WHERE control_id = TRUE
     FOR SHARE;
    IF current_state IS DISTINCT FROM 'active' THEN
        RAISE EXCEPTION 'account summary revisions are paused';
    END IF;
    IF length(trim(p_account_id)) = 0 OR p_account_id <> trim(p_account_id)
       OR p_account_id ~ '[,\r\n]' THEN
        RAISE EXCEPTION 'account id is invalid';
    END IF;

    INSERT INTO account_summary_revisions(account_id, revision)
    VALUES (p_account_id, 1)
    ON CONFLICT (account_id) DO UPDATE
       SET revision = account_summary_revisions.revision + 1,
           updated_at = now()
     WHERE account_summary_revisions.revision < 9223372036854775807
    RETURNING revision INTO allocated;
    IF allocated IS NULL THEN
        RAISE EXCEPTION 'account summary revision exhausted for account %', p_account_id;
    END IF;
    RETURN allocated;
END;
$$;

-- Register is idempotent by manifest hash. It never starts a second run while
-- another run owns the control row, and it computes seed_revision in the
-- database so an overflow cannot be hidden by a client integer conversion.
CREATE OR REPLACE FUNCTION register_account_summary_revision_seed(
    p_run_id UUID, p_manifest_sha256 TEXT, p_requested_by TEXT, p_source_region TEXT
) RETURNS VOID LANGUAGE plpgsql AS $$
DECLARE
    existing_run UUID;
    existing_state TEXT;
BEGIN
    PERFORM pg_advisory_xact_lock(hashtextextended('aero-im:account-summary-revision-seed', 0));
    SELECT run_id, state INTO existing_run, existing_state
      FROM account_summary_revision_seed_runs
     WHERE manifest_sha256 = lower(trim(p_manifest_sha256));
    IF existing_run IS NOT NULL THEN
        IF existing_run <> p_run_id THEN
            RAISE EXCEPTION 'manifest already belongs to seed run %', existing_run;
        END IF;
        -- A retry after a client timeout, process restart, or failover is a
        -- no-op. In particular, it must not increment epoch or re-pause.
        RETURN;
    END IF;
    SELECT active_run_id INTO existing_run FROM account_summary_revision_control
     WHERE control_id = TRUE FOR UPDATE;
    IF existing_run IS NOT NULL AND existing_run <> p_run_id THEN
        RAISE EXCEPTION 'another account summary revision seed is active: %', existing_run;
    END IF;
    INSERT INTO account_summary_revision_seed_runs(run_id, manifest_sha256, state, requested_by, source_region)
    VALUES (p_run_id, lower(trim(p_manifest_sha256)), 'running', trim(p_requested_by), trim(p_source_region));
    UPDATE account_summary_revision_control
       SET state = 'paused', active_run_id = p_run_id, epoch = account_summary_revision_seed_value(epoch), updated_at = now()
     WHERE control_id = TRUE;
    PERFORM account_summary_revision_seed_audit(p_run_id, 'registered', p_requested_by,
        jsonb_build_object('manifest_sha256', lower(trim(p_manifest_sha256))));
    PERFORM account_summary_revision_seed_audit(p_run_id, 'paused', p_requested_by, '{}'::jsonb);
END;
$$;

-- Validate the loaded manifest before any allocator row is changed. The
-- manifest hash is computed over a canonical, sorted export by the external
-- coordinator; this check makes a truncated or mixed export fail closed.
CREATE OR REPLACE FUNCTION validate_account_summary_revision_seed(
    p_run_id UUID, p_expected_rows BIGINT
) RETURNS VOID LANGUAGE plpgsql AS $$
DECLARE
    actual_rows BIGINT;
    bad_rows BIGINT;
    expected_hash TEXT;
    actual_hash TEXT;
BEGIN
    PERFORM pg_advisory_xact_lock(hashtextextended('aero-im:account-summary-revision-seed', 0));
    SELECT manifest_sha256 INTO expected_hash FROM account_summary_revision_seed_runs WHERE run_id = p_run_id;
    SELECT encode(digest(COALESCE(string_agg(account_id || ',' || max_existing_revision::text,
        E'\n' ORDER BY account_id), ''), 'sha256'), 'hex') INTO actual_hash
      FROM account_summary_revision_seed_manifest WHERE run_id = p_run_id;
    IF expected_hash IS NULL OR actual_hash <> expected_hash THEN
        RAISE EXCEPTION 'seed manifest hash mismatch';
    END IF;
    SELECT count(*) INTO actual_rows FROM account_summary_revision_seed_manifest WHERE run_id = p_run_id;
    IF actual_rows <> p_expected_rows THEN
        RAISE EXCEPTION 'seed manifest row count mismatch: got %, want %', actual_rows, p_expected_rows;
    END IF;
    SELECT count(*) INTO bad_rows FROM account_summary_revision_seed_manifest
     WHERE run_id = p_run_id
       AND (seed_revision <> account_summary_revision_seed_value(max_existing_revision));
    IF bad_rows <> 0 THEN
        RAISE EXCEPTION 'seed manifest contains % invalid seed revisions', bad_rows;
    END IF;
    UPDATE account_summary_revision_seed_runs SET rows_total = actual_rows, manifest_locked = TRUE
     WHERE run_id = p_run_id AND state = 'running';
    IF NOT FOUND THEN
        RAISE EXCEPTION 'seed run is not running';
    END IF;
END;
$$;

-- Apply one bounded batch. The caller must commit each call. If the process or
-- primary fails before commit, both the source upserts and manifest state roll
-- back; rerunning the same call is safe because GREATEST never lowers a row.
CREATE OR REPLACE FUNCTION apply_account_summary_revision_seed_batch(
    p_run_id UUID, p_batch_size INTEGER, p_actor TEXT
) RETURNS BIGINT LANGUAGE plpgsql AS $$
DECLARE
    item RECORD;
    applied BIGINT := 0;
    run_state TEXT;
    manifest_locked BOOLEAN;
BEGIN
    IF p_batch_size < 1 OR p_batch_size > 10000 THEN
        RAISE EXCEPTION 'seed batch size must be between 1 and 10000';
    END IF;
    PERFORM pg_advisory_xact_lock(hashtextextended('aero-im:account-summary-revision-seed', 0));
    SELECT state, account_summary_revision_seed_runs.manifest_locked
      INTO run_state, manifest_locked
      FROM account_summary_revision_seed_runs WHERE run_id = p_run_id FOR UPDATE;
    IF run_state IS DISTINCT FROM 'running' THEN
        RAISE EXCEPTION 'seed run is not running: %', COALESCE(run_state, 'missing');
    END IF;
    IF NOT manifest_locked THEN
        RAISE EXCEPTION 'seed manifest has not been validated';
    END IF;
    PERFORM 1 FROM account_summary_revision_control
      WHERE control_id = TRUE AND state = 'paused' AND active_run_id = p_run_id
      FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'seed run is not the paused control owner';
    END IF;

    FOR item IN
        SELECT * FROM account_summary_revision_seed_manifest
         WHERE run_id = p_run_id AND state = 'pending'
         ORDER BY account_id
         LIMIT p_batch_size FOR UPDATE SKIP LOCKED
    LOOP
        INSERT INTO account_summary_revisions(account_id, revision, updated_at)
        VALUES (item.account_id, item.seed_revision, now())
        ON CONFLICT (account_id) DO UPDATE
              SET revision = GREATEST(account_summary_revisions.revision, EXCLUDED.revision),
                  updated_at = CASE WHEN account_summary_revisions.revision < EXCLUDED.revision
                                    THEN now() ELSE account_summary_revisions.updated_at END;
        UPDATE account_summary_revision_seed_manifest
           SET state = 'applied', attempts = attempts + 1, applied_at = now()
         WHERE run_id = p_run_id AND account_id = item.account_id AND state = 'pending';
        applied := applied + 1;
    END LOOP;
    UPDATE account_summary_revision_seed_runs
       SET rows_applied = rows_applied + applied
     WHERE run_id = p_run_id;
    IF applied > 0 THEN
        PERFORM account_summary_revision_seed_audit(p_run_id, 'batch_applied', p_actor,
            jsonb_build_object('rows', applied));
    END IF;
    RETURN applied;
END;
$$;

-- Completion does not resume traffic. This is intentional: the operator must
-- perform the canary and first-post-promotion checks before calling resume.
CREATE OR REPLACE FUNCTION complete_account_summary_revision_seed(
    p_run_id UUID, p_actor TEXT
) RETURNS VOID LANGUAGE plpgsql AS $$
DECLARE pending_count BIGINT;
BEGIN
    PERFORM pg_advisory_xact_lock(hashtextextended('aero-im:account-summary-revision-seed', 0));
    SELECT count(*) INTO pending_count FROM account_summary_revision_seed_manifest
     WHERE run_id = p_run_id AND state = 'pending';
    IF pending_count <> 0 THEN
        RAISE EXCEPTION 'seed run has % pending rows', pending_count;
    END IF;
    UPDATE account_summary_revision_seed_runs
       SET state = 'completed', completed_at = now()
     WHERE run_id = p_run_id AND state = 'running';
    IF NOT FOUND THEN
        RAISE EXCEPTION 'seed run is not running';
    END IF;
    PERFORM account_summary_revision_seed_audit(p_run_id, 'completed', p_actor,
        jsonb_build_object('rows', (SELECT rows_total FROM account_summary_revision_seed_runs WHERE run_id = p_run_id)));
END;
$$;

CREATE OR REPLACE FUNCTION resume_account_summary_revision_seed(
    p_run_id UUID, p_actor TEXT, p_canary_verified BOOLEAN
) RETURNS BIGINT LANGUAGE plpgsql AS $$
DECLARE new_epoch BIGINT;
BEGIN
    IF NOT p_canary_verified THEN
        RAISE EXCEPTION 'canary verification is required before resume';
    END IF;
    PERFORM pg_advisory_xact_lock(hashtextextended('aero-im:account-summary-revision-seed', 0));
    IF NOT EXISTS (SELECT 1 FROM account_summary_revision_seed_runs WHERE run_id = p_run_id AND state = 'completed') THEN
        RAISE EXCEPTION 'seed run is not complete';
    END IF;
    UPDATE account_summary_revision_control
       SET state = 'active', active_run_id = NULL, epoch = account_summary_revision_seed_value(epoch), updated_at = now()
     WHERE control_id = TRUE AND state = 'paused' AND active_run_id = p_run_id
     RETURNING epoch INTO new_epoch;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'seed run does not own the paused control row';
    END IF;
    PERFORM account_summary_revision_seed_audit(p_run_id, 'resumed', p_actor,
        jsonb_build_object('epoch', new_epoch));
    RETURN new_epoch;
END;
$$;

CREATE OR REPLACE FUNCTION abort_account_summary_revision_seed(
    p_run_id UUID, p_actor TEXT, p_reason TEXT
) RETURNS VOID LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(hashtextextended('aero-im:account-summary-revision-seed', 0));
    UPDATE account_summary_revision_seed_runs SET state = 'aborted', last_error = left(trim(p_reason), 4096)
     WHERE run_id = p_run_id AND state = 'running';
    IF NOT FOUND THEN
        RAISE EXCEPTION 'seed run is not running';
    END IF;
    -- Deliberately keep the control row paused. An aborted run may have
    -- committed some batches, so automatically reopening traffic could expose
    -- an incomplete migration. Recovery must restore/reconcile the source and
    -- perform a separately reviewed control-row promotion.
    PERFORM account_summary_revision_seed_audit(p_run_id, 'aborted', p_actor,
        jsonb_build_object('reason', left(trim(p_reason), 4096)));
END;
$$;

REVOKE ALL ON account_summary_revision_seed_audit FROM PUBLIC;
REVOKE ALL ON FUNCTION account_summary_revision_seed_audit(UUID, TEXT, TEXT, JSONB) FROM PUBLIC;
REVOKE ALL ON FUNCTION reject_account_summary_revision_seed_audit_change() FROM PUBLIC;
REVOKE ALL ON FUNCTION next_account_summary_revision(TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION register_account_summary_revision_seed(UUID, TEXT, TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION validate_account_summary_revision_seed(UUID, BIGINT) FROM PUBLIC;
REVOKE ALL ON FUNCTION apply_account_summary_revision_seed_batch(UUID, INTEGER, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION complete_account_summary_revision_seed(UUID, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION resume_account_summary_revision_seed(UUID, TEXT, BOOLEAN) FROM PUBLIC;
REVOKE ALL ON FUNCTION abort_account_summary_revision_seed(UUID, TEXT, TEXT) FROM PUBLIC;
