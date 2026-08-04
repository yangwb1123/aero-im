-- Transaction and raw-SQL fences for bot governance.
--
-- Application writes now lock workspace access, bot ownership, subscription
-- identity and DLQ state in one transaction. These triggers/FKs make the same
-- immutable tenant edges non-bypassable for direct SQL and future call sites.

-- Existing denormalized delivery rows are repaired before the composite
-- containment keys are installed.
UPDATE bot_subscription_deliveries AS delivery
   SET bot_id = subscription.bot_id,
       event_type = subscription.event_type
  FROM bot_event_subscriptions AS subscription
 WHERE subscription.id = delivery.subscription_id
   AND (
       delivery.bot_id IS DISTINCT FROM subscription.bot_id
       OR delivery.event_type IS DISTINCT FROM subscription.event_type
   );

UPDATE bot_subscription_delivery_outbox AS delivery
   SET bot_id = subscription.bot_id,
       event_type = subscription.event_type
  FROM bot_event_subscriptions AS subscription
 WHERE subscription.id = delivery.subscription_id
   AND (
       delivery.bot_id IS DISTINCT FROM subscription.bot_id
       OR delivery.event_type IS DISTINCT FROM subscription.event_type
   );

-- A workspace bot always carries its immutable workspace projection in every
-- subscription, including WS-only subscriptions created before this fence.
UPDATE bot_event_subscriptions AS subscription
   SET filters = jsonb_set(
       COALESCE(subscription.filters, '{}'::jsonb),
       '{workspace_id}',
       to_jsonb(aero_uuid_to_ulid(bot.workspace_id)),
       true
   )
  FROM bots AS bot
 WHERE bot.id = subscription.bot_id
   AND bot.workspace_id IS NOT NULL
   AND NULLIF(subscription.filters ->> 'workspace_id', '') IS NULL;

-- A workspace-scoped bot cannot be converted into a system bot as a side
-- effect of tenant deletion. Delete the bot registry row (and its subscriptions)
-- with the workspace instead, preserving immutable identity for surviving rows.
ALTER TABLE bots
    DROP CONSTRAINT IF EXISTS bots_workspace_id_fkey;
ALTER TABLE bots
    ADD CONSTRAINT bots_workspace_id_fkey
    FOREIGN KEY (workspace_id)
    REFERENCES workspaces (id)
    ON DELETE CASCADE;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'bot_event_subscriptions_id_bot_unique'
           AND conrelid = 'bot_event_subscriptions'::regclass
    ) THEN
        ALTER TABLE bot_event_subscriptions
            ADD CONSTRAINT bot_event_subscriptions_id_bot_unique
            UNIQUE (id, bot_id);
    END IF;
END
$$;

-- Replace the single-column subscription FKs with composite containment FKs.
-- ON DELETE CASCADE remains the lifecycle contract.
ALTER TABLE bot_subscription_deliveries
    DROP CONSTRAINT IF EXISTS bot_subscription_deliveries_subscription_id_fkey;
ALTER TABLE bot_subscription_delivery_outbox
    DROP CONSTRAINT IF EXISTS bot_subscription_delivery_outbox_subscription_id_fkey;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'bot_subscription_deliveries_subscription_bot_fkey'
           AND conrelid = 'bot_subscription_deliveries'::regclass
    ) THEN
        ALTER TABLE bot_subscription_deliveries
            ADD CONSTRAINT bot_subscription_deliveries_subscription_bot_fkey
            FOREIGN KEY (subscription_id, bot_id)
            REFERENCES bot_event_subscriptions (id, bot_id)
            ON DELETE CASCADE;
    END IF;
END
$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'bot_delivery_outbox_subscription_bot_fkey'
           AND conrelid = 'bot_subscription_delivery_outbox'::regclass
    ) THEN
        ALTER TABLE bot_subscription_delivery_outbox
            ADD CONSTRAINT bot_delivery_outbox_subscription_bot_fkey
            FOREIGN KEY (subscription_id, bot_id)
            REFERENCES bot_event_subscriptions (id, bot_id)
            ON DELETE CASCADE;
    END IF;
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'bot_delivery_outbox_room_fkey'
           AND conrelid = 'bot_subscription_delivery_outbox'::regclass
    ) THEN
        -- Legacy queues may contain already-terminal synthetic room ids. NOT
        -- VALID preserves those rows while enforcing the FK for every new write.
        ALTER TABLE bot_subscription_delivery_outbox
            ADD CONSTRAINT bot_delivery_outbox_room_fkey
            FOREIGN KEY (room_id)
            REFERENCES rooms (id)
            ON DELETE CASCADE
            NOT VALID;
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION aero_bot_identity_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.owner_id IS DISTINCT FROM OLD.owner_id
       OR NEW.workspace_id IS DISTINCT FROM OLD.workspace_id THEN
        RAISE EXCEPTION 'bot owner/workspace identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'bots_owner_workspace_identity_immutable';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS bots_owner_workspace_identity_immutable_trg ON bots;
CREATE TRIGGER bots_owner_workspace_identity_immutable_trg
    BEFORE UPDATE OF owner_id, workspace_id
    ON bots
    FOR EACH ROW
    EXECUTE FUNCTION aero_bot_identity_immutable();

CREATE OR REPLACE FUNCTION aero_bot_subscription_validate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    bot_workspace uuid;
    canonical_workspace uuid;
    room_scope text;
    workspace_scope text;
BEGIN
    IF TG_OP = 'UPDATE'
       AND (
           NEW.bot_id IS DISTINCT FROM OLD.bot_id
           OR NULLIF(NEW.filters ->> 'room_id', '') IS DISTINCT FROM
              NULLIF(OLD.filters ->> 'room_id', '')
           OR NULLIF(NEW.filters ->> 'workspace_id', '') IS DISTINCT FROM
              NULLIF(OLD.filters ->> 'workspace_id', '')
       ) THEN
        RAISE EXCEPTION 'bot subscription bot/scope identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'bot_subscription_identity_immutable';
    END IF;

    SELECT workspace_id
      INTO bot_workspace
      FROM bots
     WHERE id = NEW.bot_id;
    IF NOT FOUND THEN
        -- The ordinary FK reports a missing bot.
        RETURN NEW;
    END IF;

    room_scope := NULLIF(NEW.filters ->> 'room_id', '');
    workspace_scope := NULLIF(NEW.filters ->> 'workspace_id', '');

    IF room_scope IS NOT NULL THEN
        SELECT room.workspace_id
          INTO canonical_workspace
          FROM rooms AS room
         WHERE aero_uuid_to_ulid(room.id) = room_scope;
        IF NOT FOUND
           OR workspace_scope IS DISTINCT FROM
              aero_uuid_to_ulid(canonical_workspace) THEN
            RAISE EXCEPTION 'bot subscription room/workspace scope mismatch'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'bot_subscription_room_workspace_containment';
        END IF;
    ELSIF workspace_scope IS NOT NULL THEN
        SELECT workspace.id
          INTO canonical_workspace
          FROM workspaces AS workspace
         WHERE aero_uuid_to_ulid(workspace.id) = workspace_scope;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'bot subscription workspace scope is unknown'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'bot_subscription_workspace_containment';
        END IF;
    ELSE
        canonical_workspace := NULL;
    END IF;

    IF bot_workspace IS NOT NULL
       AND (
           canonical_workspace IS DISTINCT FROM bot_workspace
           OR workspace_scope IS DISTINCT FROM aero_uuid_to_ulid(bot_workspace)
       ) THEN
        RAISE EXCEPTION 'workspace bot subscription escaped its tenant'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'bot_subscription_bot_workspace_containment';
    END IF;

    IF NEW.webhook_url IS NOT NULL AND canonical_workspace IS NULL THEN
        RAISE EXCEPTION 'external bot subscription requires a workspace scope'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'bot_subscription_external_workspace_containment';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS bot_subscription_validate_insert_trg
    ON bot_event_subscriptions;
CREATE TRIGGER bot_subscription_validate_insert_trg
    BEFORE INSERT
    ON bot_event_subscriptions
    FOR EACH ROW
    EXECUTE FUNCTION aero_bot_subscription_validate();

DROP TRIGGER IF EXISTS bot_subscription_identity_immutable_trg
    ON bot_event_subscriptions;
CREATE TRIGGER bot_subscription_identity_immutable_trg
    BEFORE UPDATE OF bot_id, filters
    ON bot_event_subscriptions
    FOR EACH ROW
    EXECUTE FUNCTION aero_bot_subscription_validate();

CREATE OR REPLACE FUNCTION aero_bot_delivery_outbox_validate_insert()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    subscription_bot uuid;
    subscription_event text;
    room_scope text;
    workspace_scope text;
    delivery_workspace uuid;
BEGIN
    SELECT subscription.bot_id,
           subscription.event_type,
           NULLIF(subscription.filters ->> 'room_id', ''),
           NULLIF(subscription.filters ->> 'workspace_id', '')
      INTO subscription_bot,
           subscription_event,
           room_scope,
           workspace_scope
      FROM bot_event_subscriptions AS subscription
     WHERE subscription.id = NEW.subscription_id;
    IF NOT FOUND THEN
        RETURN NEW;
    END IF;
    IF NEW.bot_id IS DISTINCT FROM subscription_bot
       OR NEW.event_type IS DISTINCT FROM subscription_event THEN
        RAISE EXCEPTION 'bot delivery does not match its subscription'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'bot_delivery_outbox_subscription_containment';
    END IF;

    SELECT room.workspace_id
      INTO delivery_workspace
      FROM rooms AS room
     WHERE room.id = NEW.room_id;
    IF NOT FOUND
       OR workspace_scope IS DISTINCT FROM
          aero_uuid_to_ulid(delivery_workspace)
       OR (
           room_scope IS NOT NULL
           AND room_scope IS DISTINCT FROM aero_uuid_to_ulid(NEW.room_id)
       ) THEN
        RAISE EXCEPTION 'bot delivery room escaped its subscription scope'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'bot_delivery_outbox_room_containment';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS bot_delivery_outbox_validate_insert_trg
    ON bot_subscription_delivery_outbox;
CREATE TRIGGER bot_delivery_outbox_validate_insert_trg
    BEFORE INSERT
    ON bot_subscription_delivery_outbox
    FOR EACH ROW
    EXECUTE FUNCTION aero_bot_delivery_outbox_validate_insert();

CREATE OR REPLACE FUNCTION aero_bot_delivery_outbox_identity_immutable()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.subscription_id IS DISTINCT FROM OLD.subscription_id
       OR NEW.bot_id IS DISTINCT FROM OLD.bot_id
       OR NEW.event_id IS DISTINCT FROM OLD.event_id
       OR NEW.event_type IS DISTINCT FROM OLD.event_type
       OR NEW.room_id IS DISTINCT FROM OLD.room_id
       OR NEW.request_body IS DISTINCT FROM OLD.request_body THEN
        RAISE EXCEPTION 'bot delivery producer identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'bot_delivery_outbox_identity_immutable';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS bot_delivery_outbox_identity_immutable_trg
    ON bot_subscription_delivery_outbox;
CREATE TRIGGER bot_delivery_outbox_identity_immutable_trg
    BEFORE UPDATE OF
        subscription_id, bot_id, event_id, event_type, room_id, request_body
    ON bot_subscription_delivery_outbox
    FOR EACH ROW
    EXECUTE FUNCTION aero_bot_delivery_outbox_identity_immutable();

COMMENT ON FUNCTION aero_bot_identity_immutable() IS
    '0213 raw-SQL fence for immutable bot owner/workspace identity';
COMMENT ON FUNCTION aero_bot_subscription_validate() IS
    '0213 raw-SQL fence for bot subscription identity and canonical tenant scope';
COMMENT ON FUNCTION aero_bot_delivery_outbox_validate_insert() IS
    '0213 raw-SQL fence for subscription/bot/room containment of bot deliveries';
