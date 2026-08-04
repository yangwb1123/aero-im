-- Workspace-scoped bots are service identities, not merely registry rows.
--
-- Migration 0195 made the access rule explicit: Bot/Agent identities are exempt
-- from human-only mandatory TOTP, but they still require an active participant
-- plus a retained, non-guest workspace membership. Older `/api/bots` writes
-- persisted `bots.workspace_id` without that membership, so the bot could not be
-- installed into a room after the target-eligibility fence was tightened.
--
-- Repair every live workspace bot. Preserve any stronger existing role, while
-- converting legacy guest-shaped rows into ordinary members. New writes create
-- this edge atomically with the participant, token hash, and bot registry row.
INSERT INTO workspace_members
    (workspace_id, participant_id, role, joined_at, is_guest)
SELECT bot.workspace_id,
       bot.id,
       'member',
       bot.created_at,
       false
  FROM bots AS bot
  JOIN participants AS participant
    ON participant.id = bot.id
   AND participant.deleted_at IS NULL
 WHERE bot.workspace_id IS NOT NULL
ON CONFLICT (workspace_id, participant_id) DO UPDATE
       SET role = CASE
                      WHEN workspace_members.role = 'guest'
                           OR workspace_members.is_guest
                      THEN 'member'
                      ELSE workspace_members.role
                  END,
           is_guest = false
 WHERE workspace_members.role = 'guest'
    OR workspace_members.is_guest;
