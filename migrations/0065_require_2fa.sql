-- 0065 Workspace-wide two-factor enforcement.
--
-- An admin can mandate 2FA for a workspace. Enforcement is at the room-data
-- choke point (`ImService::assert_room_access`, mirroring the deactivation gate):
-- a member of a `require_2fa` workspace who has not activated TOTP is locked out
-- of that workspace's room data until they enroll. The `/api/me/2fa/*` enroll
-- routes are NOT room-gated, so enrollment stays reachable. Idempotent.
ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS require_2fa boolean NOT NULL DEFAULT false;
