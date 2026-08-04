//! Shared detection for database-enforced ownership conflicts.

use sqlx::{Postgres, Transaction};

/// Constraint label emitted by migration 0194's channel governance triggers.
pub const CHANNEL_EFFECTIVE_OWNER_CONSTRAINT: &str = "channel_effective_owner_required";

/// Constraint label emitted by migration 0200's workspace governance triggers.
pub const WORKSPACE_OWNER_CONSTRAINT: &str = "workspace_owner_required";

/// Whether `error` is the stable database refusal for an operation that would
/// leave a channel without an effective owner.
#[must_use]
pub fn is_channel_effective_owner_violation(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database)
            if database.constraint() == Some(CHANNEL_EFFECTIVE_OWNER_CONSTRAINT)
    )
}

/// Whether `error` is the stable database refusal for an operation that would
/// create or leave a workspace without a non-guest owner.
#[must_use]
pub fn is_workspace_owner_violation(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database)
            if database.constraint() == Some(WORKSPACE_OWNER_CONSTRAINT)
    )
}

/// Enter the global membership-governance lock order before a transaction
/// explicitly locks any workspace/channel/member row.
///
/// Migration 0200 also invokes this fence from `BEFORE STATEMENT` triggers, so
/// a standalone direct SQL mutation is covered automatically. A multi-statement
/// governance transaction must enter here first: a statement trigger cannot
/// retroactively precede an earlier `SELECT .. FOR UPDATE` on a child row.
pub(crate) async fn lock_membership_governance(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT aero_lock_all_membership_governance()")
        .execute(&mut **tx)
        .await?;
    Ok(())
}
