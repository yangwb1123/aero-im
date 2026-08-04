-- Separate the issuer that authenticates an integration's machine token from
-- the trusted human OIDC issuer used to resolve `snaplink_user` targets.
--
-- Existing installations historically used one issuer for both purposes, so
-- backfill from `issuer` preserves their exact behaviour during a rolling
-- upgrade. New writes must provide both namespaces explicitly.

ALTER TABLE integration_installations
    ADD COLUMN IF NOT EXISTS user_identity_issuer TEXT;

UPDATE integration_installations
   SET user_identity_issuer = issuer
 WHERE user_identity_issuer IS NULL;

ALTER TABLE integration_installations
    ALTER COLUMN user_identity_issuer SET NOT NULL;

-- A 0232 binary may still serve administrator traffic while 0233 rolls out.
-- Its INSERT omits the new column, so preserve the legacy one-issuer behaviour
-- only for that old-shaped INSERT. Updates never re-couple the two namespaces.
CREATE OR REPLACE FUNCTION aero_default_integration_user_identity_issuer()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.user_identity_issuer IS NULL THEN
        NEW.user_identity_issuer := NEW.issuer;
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS integration_user_identity_issuer_default
    ON integration_installations;
CREATE TRIGGER integration_user_identity_issuer_default
    BEFORE INSERT ON integration_installations
    FOR EACH ROW
    EXECUTE FUNCTION aero_default_integration_user_identity_issuer();

ALTER TABLE integration_installations
    DROP CONSTRAINT IF EXISTS integration_installations_user_identity_issuer_valid;

ALTER TABLE integration_installations
    ADD CONSTRAINT integration_installations_user_identity_issuer_valid CHECK (
        octet_length(user_identity_issuer) BETWEEN 1 AND 2048
        AND user_identity_issuer = btrim(user_identity_issuer)
        AND user_identity_issuer !~ '[[:cntrl:]]'
    );
