-- Linearizable prediction staking and settlement.
--
-- The application now uses the prediction row as the serialization point for
-- stake/lock/resolve/cancel. The admission trigger below is also required during
-- a rolling upgrade: an older binary checks `status = 'open'` before debiting
-- points but did not lock the prediction until its stake INSERT. Locking and
-- rechecking here means that old transaction either inserts before settlement
-- (and is included), or observes the terminal state and aborts its entire
-- transaction, including the earlier ledger debit.

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'predictions'::regclass
           AND conname = 'predictions_expiry_after_create_chk'
    ) THEN
        ALTER TABLE predictions
            ADD CONSTRAINT predictions_expiry_after_create_chk
            CHECK (expires_at IS NULL OR expires_at > created_at)
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'prediction_stakes'::regclass
           AND conname = 'prediction_stakes_outcome_fkey'
    ) THEN
        -- Existing stakes were application-validated. NOT VALID avoids making
        -- rollout depend on historical raw-SQL rows while enforcing containment
        -- for every new stake immediately.
        ALTER TABLE prediction_stakes
            ADD CONSTRAINT prediction_stakes_outcome_fkey
            FOREIGN KEY (prediction_id, outcome_idx)
            REFERENCES prediction_outcomes (prediction_id, idx)
            ON DELETE CASCADE
            NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conrelid = 'prediction_stakes'::regclass
           AND conname = 'prediction_stakes_payout_nonnegative_chk'
    ) THEN
        ALTER TABLE prediction_stakes
            ADD CONSTRAINT prediction_stakes_payout_nonnegative_chk
            CHECK (payout IS NULL OR payout >= 0)
            NOT VALID;
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION prediction_stake_admission_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    parent_status text;
    parent_expires_at timestamptz;
    parent_creator uuid;
    history_reason text;
BEGIN
    -- This lock is the rolling-upgrade fence. It deliberately precedes outcome
    -- validation and the INSERT's FK checks, matching the new application lock
    -- order (prediction -> points ledger -> stake).
    SELECT prediction.status,
           prediction.expires_at,
           prediction.creator_id
      INTO parent_status, parent_expires_at, parent_creator
      FROM predictions AS prediction
     WHERE prediction.id = NEW.prediction_id
       FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'prediction stake requires an existing prediction'
            USING ERRCODE = '23503',
                  CONSTRAINT = 'prediction_stakes_prediction_id_fkey';
    END IF;

    IF parent_status <> 'open' THEN
        RAISE EXCEPTION 'prediction is not open for staking'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'prediction_stake_open_state_chk';
    END IF;

    IF parent_expires_at IS NOT NULL
       AND parent_expires_at <= clock_timestamp() THEN
        RAISE EXCEPTION 'prediction has expired'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'prediction_stake_not_expired_chk';
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM prediction_outcomes AS outcome
         WHERE outcome.prediction_id = NEW.prediction_id
           AND outcome.idx = NEW.outcome_idx
    ) THEN
        RAISE EXCEPTION 'prediction stake outcome does not belong to prediction'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'prediction_stake_outcome_chk';
    END IF;

    -- Old binaries do not append the debit to the reconstructable points
    -- journal. Use a stable per-stake reason so the new application writer and
    -- this rolling-upgrade backstop converge to exactly one history row.
    history_reason := 'prediction_stake:' || NEW.id::text;
    INSERT INTO points_earn_history
        (id, viewer_id, creator_id, delta, reason)
    SELECT gen_random_uuid(),
           NEW.viewer_id,
           parent_creator,
           -NEW.points,
           history_reason
     WHERE NOT EXISTS (
        SELECT 1
          FROM points_earn_history AS history
         WHERE history.viewer_id = NEW.viewer_id
           AND history.creator_id = parent_creator
           AND history.reason = history_reason
    );

    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS prediction_stake_admission_guard
    ON prediction_stakes;
CREATE TRIGGER prediction_stake_admission_guard
    BEFORE INSERT
    ON prediction_stakes
    FOR EACH ROW
    EXECUTE FUNCTION prediction_stake_admission_guard();

CREATE OR REPLACE FUNCTION prediction_stake_identity_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.prediction_id IS DISTINCT FROM OLD.prediction_id
       OR NEW.outcome_idx IS DISTINCT FROM OLD.outcome_idx
       OR NEW.viewer_id IS DISTINCT FROM OLD.viewer_id
       OR NEW.points IS DISTINCT FROM OLD.points
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION 'prediction stake identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'prediction_stake_identity_immutable_chk';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS prediction_stake_identity_guard
    ON prediction_stakes;
CREATE TRIGGER prediction_stake_identity_guard
    BEFORE UPDATE OF id, prediction_id, outcome_idx, viewer_id, points, created_at
    ON prediction_stakes
    FOR EACH ROW
    EXECUTE FUNCTION prediction_stake_identity_guard();

CREATE OR REPLACE FUNCTION prediction_state_transition_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.status IS NOT DISTINCT FROM OLD.status THEN
        RETURN NEW;
    END IF;

    IF (OLD.status = 'open'
            AND NEW.status IN ('locked', 'resolved', 'cancelled'))
       OR (OLD.status = 'locked'
            AND NEW.status IN ('resolved', 'cancelled')) THEN
        RETURN NEW;
    END IF;

    RAISE EXCEPTION 'illegal prediction lifecycle transition: % -> %',
        OLD.status, NEW.status
        USING ERRCODE = '23514',
              CONSTRAINT = 'prediction_state_transition_chk';
END
$$;

DROP TRIGGER IF EXISTS prediction_state_transition_guard
    ON predictions;
CREATE TRIGGER prediction_state_transition_guard
    BEFORE UPDATE OF status
    ON predictions
    FOR EACH ROW
    EXECUTE FUNCTION prediction_state_transition_guard();

-- Backfill the missing negative audit entry for every historical stake. This
-- changes only the reconstructable journal; balances were already debited by the
-- original stake transactions.
INSERT INTO points_earn_history
    (id, viewer_id, creator_id, delta, reason)
SELECT gen_random_uuid(),
       stake.viewer_id,
       prediction.creator_id,
       -stake.points,
       'prediction_stake:' || stake.id::text
  FROM prediction_stakes AS stake
  JOIN predictions AS prediction
    ON prediction.id = stake.prediction_id
 WHERE NOT EXISTS (
    SELECT 1
      FROM points_earn_history AS history
     WHERE history.viewer_id = stake.viewer_id
       AND history.creator_id = prediction.creator_id
       AND history.reason = 'prediction_stake:' || stake.id::text
 );

-- A terminal prediction with payout IS NULL is the exact footprint of the old
-- race: settlement committed before that stake became visible. It is too late to
-- recompute an already-published pari-mutuel pool safely, so restore the late
-- stake's principal, stamp it as refunded, and append a stable correction entry.
-- The payout-null predicate plus the correction reason make this block idempotent.
DO $$
DECLARE
    late record;
    inserted_history integer;
    correction_reason text;
BEGIN
    FOR late IN
        SELECT stake.id,
               stake.viewer_id,
               stake.points,
               prediction.creator_id
          FROM prediction_stakes AS stake
          JOIN predictions AS prediction
            ON prediction.id = stake.prediction_id
         WHERE prediction.status IN ('resolved', 'cancelled')
           AND stake.payout IS NULL
         ORDER BY stake.id
         FOR UPDATE OF stake
    LOOP
        correction_reason :=
            'prediction_late_stake_refund:' || late.id::text;

        INSERT INTO points_earn_history
            (id, viewer_id, creator_id, delta, reason)
        SELECT gen_random_uuid(),
               late.viewer_id,
               late.creator_id,
               late.points,
               correction_reason
         WHERE NOT EXISTS (
            SELECT 1
              FROM points_earn_history AS history
             WHERE history.viewer_id = late.viewer_id
               AND history.creator_id = late.creator_id
               AND history.reason = correction_reason
        );
        GET DIAGNOSTICS inserted_history = ROW_COUNT;

        IF inserted_history = 1 THEN
            INSERT INTO points_ledger (viewer_id, creator_id, balance)
            VALUES (late.viewer_id, late.creator_id, late.points)
            ON CONFLICT (viewer_id, creator_id)
            DO UPDATE
                  SET balance = points_ledger.balance + EXCLUDED.balance;
        END IF;

        UPDATE prediction_stakes
           SET payout = points
         WHERE id = late.id
           AND payout IS NULL;
    END LOOP;
END
$$;

COMMENT ON TRIGGER prediction_stake_admission_guard ON prediction_stakes IS
    'Serializes stake admission with prediction lifecycle, rejects expired/closed parents, and journals old-binary debits during rolling upgrades.';
