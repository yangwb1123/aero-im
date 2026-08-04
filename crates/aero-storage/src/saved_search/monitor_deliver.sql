WITH eligible AS MATERIALIZED (
  SELECT input.notification_id, input.delivery_id,
         message.id AS message_id, message.room_id,
         message.sender_id
    FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[])
         AS input(notification_id, message_id, delivery_id)
    JOIN messages message ON message.id = input.message_id
    JOIN rooms room ON room.id = message.room_id
    JOIN workspaces workspace ON workspace.id = room.workspace_id
    JOIN room_members room_member
      ON room_member.room_id = message.room_id
     AND room_member.participant_id = $4
    JOIN workspace_members workspace_member
      ON workspace_member.workspace_id = room.workspace_id
     AND workspace_member.participant_id = $4
    JOIN participants viewer
      ON viewer.id = $4
     AND viewer.deleted_at IS NULL
    LEFT JOIN workspace_deactivations deactivated
      ON deactivated.workspace_id = room.workspace_id
     AND deactivated.participant_id = $4
    LEFT JOIN totp_secrets totp ON totp.participant_id = $4
   WHERE room.workspace_id = $8
     AND deactivated.participant_id IS NULL
     AND (
       viewer.kind <> 'human'
       OR NOT workspace.require_2fa
       OR COALESCE(totp.activated, false)
     )
     AND message.deleted_at IS NULL
     AND (message.expires_at IS NULL OR message.expires_at > CURRENT_TIMESTAMP)
),
inserted_notifications AS (
  INSERT INTO notifications
     (id, participant_id, room_id, message_id, kind, actor_id,
      created_at, delivery_id, importance_score)
  SELECT notification_id, $4, room_id, message_id, $5, sender_id,
         $6, delivery_id, $7
    FROM eligible
  ON CONFLICT (delivery_id, participant_id)
      WHERE delivery_id IS NOT NULL
      DO NOTHING
  RETURNING 1
),
recorded_deliveries AS (
  INSERT INTO saved_search_monitor_deliveries
     (saved_search_id, message_id, participant_id, delivery_id, delivered_at)
  SELECT $9, message_id, $4, delivery_id, $6
    FROM eligible
  ON CONFLICT (saved_search_id, message_id, participant_id)
      DO NOTHING
  RETURNING 1
)
SELECT (SELECT COUNT(*) FROM inserted_notifications),
       (SELECT COUNT(*) FROM recorded_deliveries)
