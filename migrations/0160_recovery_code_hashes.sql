-- 0160_recovery_code_hashes.sql — never retain plaintext 2FA recovery codes.
--
-- Migration 0122 originally stored the one-time code itself. Convert existing
-- rows in place to a normalized SHA-256 digest, then remove the plaintext
-- column. Existing printed codes remain usable because verification applies the
-- same trim + uppercase normalization before hashing.

ALTER TABLE recovery_codes
    ADD COLUMN IF NOT EXISTS code_hash TEXT;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = 'recovery_codes'
           AND column_name = 'code'
    ) THEN
        UPDATE recovery_codes
           SET code_hash = encode(digest(upper(btrim(code)), 'sha256'), 'hex')
         WHERE code_hash IS NULL;
    END IF;
END
$$;

-- A partially-applied migration must fail closed: an unhashable row cannot
-- authenticate and should not prevent the column from becoming NOT NULL.
DELETE FROM recovery_codes WHERE code_hash IS NULL;

ALTER TABLE recovery_codes
    ALTER COLUMN code_hash SET NOT NULL;

ALTER TABLE recovery_codes
    DROP COLUMN IF EXISTS code;

CREATE UNIQUE INDEX IF NOT EXISTS idx_recovery_codes_owner_hash
    ON recovery_codes (participant_id, code_hash);
