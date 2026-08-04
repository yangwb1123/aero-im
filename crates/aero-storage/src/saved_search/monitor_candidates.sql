SELECT message.id, message.room_id, message.sender_id, message.created_at
  FROM messages message
  JOIN rooms room ON room.id = message.room_id
  JOIN workspaces workspace ON workspace.id = room.workspace_id
  JOIN room_members room_member
    ON room_member.room_id = message.room_id
   AND room_member.participant_id = $1
  JOIN workspace_members workspace_member
    ON workspace_member.workspace_id = room.workspace_id
   AND workspace_member.participant_id = $1
  JOIN participants viewer
    ON viewer.id = $1
   AND viewer.deleted_at IS NULL
  LEFT JOIN workspace_deactivations deactivated
    ON deactivated.workspace_id = room.workspace_id
   AND deactivated.participant_id = $1
  LEFT JOIN totp_secrets totp ON totp.participant_id = $1
 WHERE room.workspace_id = $2
   AND deactivated.participant_id IS NULL
   AND (
     viewer.kind <> 'human'
     OR NOT workspace.require_2fa
     OR COALESCE(totp.activated, false)
   )
   AND message.deleted_at IS NULL
   AND (message.expires_at IS NULL OR message.expires_at > CURRENT_TIMESTAMP)
   AND ($3::uuid IS NULL OR message.sender_id = $3)
   AND ($4::uuid IS NULL OR message.room_id = $4)
   AND ($5::uuid IS NULL OR message.id < $5)
   AND ($6::uuid IS NULL OR message.id > $6)
   AND ($7::timestamptz IS NULL OR message.created_at >= $7)
   AND ($8::timestamptz IS NULL OR message.created_at <= $8)
   AND (
     $9 = ''
     OR message.search_tsv
          @@ websearch_to_tsquery('english', f_unaccent($9))
     OR message.searchable_text % $9
   )
   AND (
     message.created_at > $10
     OR (
       $11::uuid IS NOT NULL
       AND message.created_at = $10
       AND message.id > $11
     )
   )
   AND message.created_at <= $12
   AND NOT EXISTS (
     SELECT 1
       FROM saved_search_monitor_deliveries delivery
      WHERE delivery.saved_search_id = $13
        AND delivery.message_id = message.id
        AND delivery.participant_id = $1
   )
 ORDER BY message.created_at, message.id
 LIMIT $14
