//! Lock-order and tenant-resolution helpers for bot governance.

use std::str::FromStr;

use aero_common::{Error, ParticipantId, RoomId, WorkspaceId};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::BotRepo;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BotTenant {
    pub(super) owner: ParticipantId,
    pub(super) workspace: Option<WorkspaceId>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SubscriptionScope {
    pub(super) room: Option<RoomId>,
    pub(super) workspace: Option<WorkspaceId>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DeliveryTenant {
    pub(super) subscription: Uuid,
    pub(super) room: RoomId,
    pub(super) workspace: WorkspaceId,
}

pub(super) fn mint_bot_token() -> String {
    format!("{}{}", BotRepo::TOKEN_PREFIX, Uuid::new_v4())
}

pub(super) fn bot_not_found() -> Error {
    Error::NotFound("bot".into())
}

pub(super) fn subscription_not_found() -> Error {
    Error::NotFound("bot subscription".into())
}

pub(super) fn delivery_not_found() -> Error {
    Error::NotFound("bot delivery".into())
}

pub(super) fn parse_subscription_scope(
    filters: &serde_json::Value,
) -> Result<SubscriptionScope, Error> {
    let filters = filters
        .as_object()
        .ok_or_else(|| Error::Invalid("filters must be a JSON object".into()))?;
    let room = filters
        .get("room_id")
        .map(|value| {
            let raw = value
                .as_str()
                .ok_or_else(|| Error::Invalid("filters.room_id must be a string".into()))?;
            RoomId::from_str(raw)
                .map_err(|error| Error::Invalid(format!("filters.room_id: {error}")))
        })
        .transpose()?;
    let workspace = filters
        .get("workspace_id")
        .map(|value| {
            let raw = value
                .as_str()
                .ok_or_else(|| Error::Invalid("filters.workspace_id must be a string".into()))?;
            WorkspaceId::from_str(raw)
                .map_err(|error| Error::Invalid(format!("filters.workspace_id: {error}")))
        })
        .transpose()?;
    Ok(SubscriptionScope { room, workspace })
}

pub(super) async fn resolve_owned_bot(
    tx: &mut Transaction<'_, Postgres>,
    bot: ParticipantId,
    actor: ParticipantId,
) -> Result<BotTenant, Error> {
    let resolved = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
        "SELECT owner_id, workspace_id FROM bots WHERE id = $1",
    )
    .bind(bot.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(bot_not_found)?;
    if resolved.0 != actor.to_uuid() {
        return Err(bot_not_found());
    }
    Ok(BotTenant {
        owner: ParticipantId::from_uuid(resolved.0),
        workspace: resolved.1.map(WorkspaceId::from_uuid),
    })
}

pub(super) async fn authorize_owned_bot(
    tx: &mut Transaction<'_, Postgres>,
    bot: ParticipantId,
    actor: ParticipantId,
    exclusive: bool,
) -> Result<BotTenant, Error> {
    let tenant = resolve_owned_bot(tx, bot, actor).await?;
    if let Some(workspace) = tenant.workspace {
        lock_effective_workspace_member(tx, workspace, actor, false).await?;
    } else {
        lock_active_participant(tx, actor).await?;
    }
    lock_expected_bot(tx, bot, tenant, exclusive).await?;
    Ok(tenant)
}

pub(super) async fn lock_expected_bot(
    tx: &mut Transaction<'_, Postgres>,
    bot: ParticipantId,
    expected: BotTenant,
    exclusive: bool,
) -> Result<(), Error> {
    let locked = if exclusive {
        sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
            "SELECT owner_id, workspace_id FROM bots WHERE id = $1 FOR UPDATE",
        )
        .bind(bot.to_uuid())
        .fetch_optional(&mut **tx)
        .await?
    } else {
        sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
            "SELECT owner_id, workspace_id FROM bots WHERE id = $1 FOR SHARE",
        )
        .bind(bot.to_uuid())
        .fetch_optional(&mut **tx)
        .await?
    };
    if locked
        != Some((
            expected.owner.to_uuid(),
            expected.workspace.map(|workspace| workspace.to_uuid()),
        ))
    {
        return Err(bot_not_found());
    }
    Ok(())
}

pub(super) async fn lock_active_participant(
    tx: &mut Transaction<'_, Postgres>,
    participant: ParticipantId,
) -> Result<(), Error> {
    let active = sqlx::query_scalar::<_, bool>(
        "SELECT deleted_at IS NULL FROM participants WHERE id = $1 FOR SHARE",
    )
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false);
    if !active {
        return Err(Error::Forbidden("active bot owner required".into()));
    }
    Ok(())
}

pub(super) async fn lock_effective_workspace_member(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
    opaque_absent_membership: bool,
) -> Result<(), Error> {
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
    if !exists {
        return Err(Error::NotFound("bot tenant".into()));
    }
    let membership = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM workspace_members
           WHERE workspace_id = $1
             AND participant_id = $2
           FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .is_some();
    if !membership {
        return Err(if opaque_absent_membership {
            Error::NotFound("bot tenant".into())
        } else {
            Error::Forbidden("effective workspace membership required".into())
        });
    }
    if !crate::workspace::members::effective_workspace_access_in_tx(tx, workspace, participant)
        .await?
    {
        return Err(Error::Forbidden(
            "effective workspace membership required".into(),
        ));
    }
    Ok(())
}

pub(super) async fn resolve_room_workspace(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
) -> Result<WorkspaceId, Error> {
    sqlx::query_scalar::<_, Uuid>("SELECT workspace_id FROM rooms WHERE id = $1")
        .bind(room.to_uuid())
        .fetch_optional(&mut **tx)
        .await?
        .map(WorkspaceId::from_uuid)
        .ok_or_else(subscription_not_found)
}

pub(super) async fn lock_room_member(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<(), Error> {
    let locked =
        sqlx::query_scalar::<_, Uuid>("SELECT workspace_id FROM rooms WHERE id = $1 FOR SHARE")
            .bind(room.to_uuid())
            .fetch_optional(&mut **tx)
            .await?;
    if locked != Some(workspace.to_uuid()) {
        return Err(subscription_not_found());
    }
    let member = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM room_members
           WHERE room_id = $1
             AND participant_id = $2
           FOR UPDATE",
    )
    .bind(room.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .is_some();
    if !member {
        return Err(Error::Forbidden(
            "effective room membership required".into(),
        ));
    }
    Ok(())
}

pub(super) async fn lock_subscription(
    tx: &mut Transaction<'_, Postgres>,
    subscription: Uuid,
    bot: ParticipantId,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM bot_event_subscriptions
           WHERE id = $1 AND bot_id = $2
           FOR UPDATE",
    )
    .bind(subscription)
    .bind(bot.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.is_some())
}

pub(super) async fn resolve_delivery_tenant(
    tx: &mut Transaction<'_, Postgres>,
    delivery: Uuid,
    bot: ParticipantId,
) -> Result<DeliveryTenant, Error> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
        r"SELECT delivery.subscription_id, delivery.room_id, room.workspace_id
            FROM bot_subscription_delivery_outbox delivery
            JOIN bot_event_subscriptions subscription
              ON subscription.id = delivery.subscription_id
             AND subscription.bot_id = delivery.bot_id
            JOIN rooms room
              ON room.id = delivery.room_id
           WHERE delivery.id = $1
             AND delivery.bot_id = $2",
    )
    .bind(delivery)
    .bind(bot.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(delivery_not_found)?;
    Ok(DeliveryTenant {
        subscription: row.0,
        room: RoomId::from_uuid(row.1),
        workspace: WorkspaceId::from_uuid(row.2),
    })
}

pub(super) async fn advisory_lock(
    tx: &mut Transaction<'_, Postgres>,
    key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
