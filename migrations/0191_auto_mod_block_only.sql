-- Auto-mod management was exposed even when enforcement was disabled, and the
-- schema advertised two actions that no send-time path implemented.  Preserve
-- existing rules by converting the reserved actions to the one deterministic
-- action the product supports, then prevent future silent no-ops at the DB
-- boundary as well as the HTTP boundary.
UPDATE auto_mod_rules
SET action = 'block'
WHERE action <> 'block';

ALTER TABLE auto_mod_rules
    DROP CONSTRAINT IF EXISTS auto_mod_rules_action_check;

ALTER TABLE auto_mod_rules
    ADD CONSTRAINT auto_mod_rules_action_check
    CHECK (action = 'block');
