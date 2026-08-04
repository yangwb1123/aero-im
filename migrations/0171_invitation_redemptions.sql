-- Durable invitation-redemption identity.
--
-- The primary key makes accepting the same invitation as the same participant
-- idempotent beyond an HTTP retry window.  `InvitationRepo::accept_by_token_hash`
-- writes this row, the workspace membership, and the invitation use count in one
-- transaction while holding the invitation row lock.

-- Supports the composite foreign key below, which makes the duplicated
-- `workspace_id` an enforced tenant scope rather than trusting application code.
CREATE UNIQUE INDEX IF NOT EXISTS invitations_id_workspace_unique_idx
    ON invitations (id, workspace_id);

CREATE TABLE IF NOT EXISTS invitation_redemptions (
    invitation_id     UUID        NOT NULL,
    participant_id    UUID        NOT NULL
        REFERENCES participants(id) ON DELETE CASCADE,
    workspace_id      UUID        NOT NULL
        REFERENCES workspaces(id) ON DELETE CASCADE,
    accepted_role     TEXT        NOT NULL
        CHECK (accepted_role IN ('owner', 'admin', 'member', 'guest')),
    membership_created BOOLEAN     NOT NULL,
    redeemed_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (invitation_id, participant_id),
    CONSTRAINT invitation_redemptions_invitation_workspace_fk
        FOREIGN KEY (invitation_id, workspace_id)
        REFERENCES invitations(id, workspace_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS invitation_redemptions_workspace_participant_idx
    ON invitation_redemptions (workspace_id, participant_id);
