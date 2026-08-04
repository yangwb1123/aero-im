-- Managed external-identity lifecycle.
--
-- Aero keeps message/file/history ownership on the immutable participant id,
-- while IdP identities are replaceable login bindings.  Erasure previously
-- deleted sso_identities outright, so a later login with the same (issuer,sub)
-- could JIT-provision a brand-new participant and silently bypass the erasure.
-- Keep a deliberately retained pseudonymous deny record for erased bindings
-- to make that transition explicit and administratively recoverable.  The
-- issuer/subject pair can still be personal data and therefore belongs in the
-- security-retention inventory; it is not ordinary profile data.

CREATE TABLE IF NOT EXISTS sso_identity_tombstones (
    issuer                TEXT        NOT NULL CHECK (btrim(issuer) <> ''),
    subject               TEXT        NOT NULL CHECK (btrim(subject) <> ''),
    former_participant_id UUID        NOT NULL,
    reason                TEXT        NOT NULL DEFAULT 'account_erased'
                                      CHECK (btrim(reason) <> ''),
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (issuer, subject)
);

CREATE INDEX IF NOT EXISTS sso_identity_tombstones_participant_idx
    ON sso_identity_tombstones (former_participant_id);

-- Historical databases may contain malformed identity keys from integrations
-- that predate strict OIDC/SCIM validation.  `NOT VALID` makes every new write
-- fail closed without making this migration impossible to deploy; operators can
-- audit/repair legacy rows and validate the constraints afterwards.
ALTER TABLE sso_identities
    ADD CONSTRAINT sso_identities_issuer_valid
        CHECK (btrim(issuer) <> '' AND issuer !~ '[[:cntrl:]]') NOT VALID,
    ADD CONSTRAINT sso_identities_subject_valid
        CHECK (btrim(subject) <> '' AND subject !~ '[[:cntrl:]]') NOT VALID;

ALTER TABLE sso_identity_tombstones
    ADD CONSTRAINT sso_identity_tombstones_issuer_valid
        CHECK (issuer !~ '[[:cntrl:]]') NOT VALID,
    ADD CONSTRAINT sso_identity_tombstones_subject_valid
        CHECK (subject !~ '[[:cntrl:]]') NOT VALID;

-- A live binding and an erasure tombstone for the same identity would make
-- login semantics ambiguous.  Repository transactions remove neither
-- implicitly; recovery must be an explicit audited administrative operation.
CREATE OR REPLACE FUNCTION aero_lock_external_identity(identity_issuer TEXT, identity_subject TEXT)
RETURNS void
LANGUAGE sql
AS $$
    SELECT pg_advisory_xact_lock(
        hashtextextended(
            jsonb_build_array('aero:sso', identity_issuer, identity_subject)::text,
            0
        )
    )
$$;

CREATE OR REPLACE FUNCTION aero_guard_live_sso_identity_tombstone()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM aero_lock_external_identity(NEW.issuer, NEW.subject);
    IF EXISTS (
        SELECT 1
          FROM sso_identity_tombstones tombstone
         WHERE tombstone.issuer = NEW.issuer
           AND tombstone.subject = NEW.subject
    ) THEN
        RAISE EXCEPTION 'external identity is tombstoned'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'sso_identity_not_tombstoned';
    END IF;
    RETURN NEW;
END
$$;

CREATE OR REPLACE FUNCTION aero_guard_tombstone_live_sso_identity()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM aero_lock_external_identity(NEW.issuer, NEW.subject);
    IF EXISTS (
        SELECT 1
          FROM sso_identities identity
         WHERE identity.issuer = NEW.issuer
           AND identity.subject = NEW.subject
    ) THEN
        RAISE EXCEPTION 'external identity is still live'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'sso_tombstone_has_no_live_identity';
    END IF;
    RETURN NEW;
END
$$;

CREATE OR REPLACE FUNCTION aero_lock_deleted_sso_identity()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM aero_lock_external_identity(OLD.issuer, OLD.subject);
    RETURN OLD;
END
$$;

DROP TRIGGER IF EXISTS sso_identity_tombstone_guard ON sso_identities;
CREATE TRIGGER sso_identity_tombstone_guard
    BEFORE INSERT OR UPDATE OF issuer, subject
    ON sso_identities
    FOR EACH ROW
    EXECUTE FUNCTION aero_guard_live_sso_identity_tombstone();

DROP TRIGGER IF EXISTS sso_identity_delete_lock ON sso_identities;
CREATE TRIGGER sso_identity_delete_lock
    BEFORE DELETE
    ON sso_identities
    FOR EACH ROW
    EXECUTE FUNCTION aero_lock_deleted_sso_identity();

DROP TRIGGER IF EXISTS sso_tombstone_live_identity_guard ON sso_identity_tombstones;
CREATE TRIGGER sso_tombstone_live_identity_guard
    BEFORE INSERT OR UPDATE OF issuer, subject
    ON sso_identity_tombstones
    FOR EACH ROW
    EXECUTE FUNCTION aero_guard_tombstone_live_sso_identity();
