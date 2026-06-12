-- Channel-points expiry: an optional timestamp after which a viewer's balance
-- with a creator is zeroed by the sweep task. NULL means the balance never expires.
ALTER TABLE points_ledger ADD COLUMN earn_expires_at TIMESTAMPTZ;
