//! Dedicated Aero ID account-summary endpoint.
//!
//! Split out of the integration module because this surface is authorized by
//! the signed target assertion (see `crate::account_summary_binding`), never
//! by the unsigned legacy headers. Every lookup below runs only after the
//! verifier and the one-time replay gate have passed.

use std::collections::{BTreeMap, BTreeSet};

use aero_common::{Error as AeroError, Notification, WorkspaceId};
use aero_storage::{AuditRepo, ParticipantRepo, SsoRepo, WorkspaceRepo};
use axum::{
    extract::{RawQuery, State},
    http::HeaderMap,
    Json,
};
use serde::Serialize;

use super::{env_value, AppState, IntegrationApiResult, MachinePrincipal};
use crate::account_summary_binding::{AccountSummaryTargetVerifier, ACCOUNT_SUMMARY_SCOPE};

pub(super) const REQUIRED_ACCOUNT_SUMMARY_SCOPE: &str = ACCOUNT_SUMMARY_SCOPE;
pub(super) const ACCOUNT_ID_HEADER: &str = "x-aero-account-id";
pub(super) const CANONICAL_UID_HEADER: &str = "x-aero-canonical-uid";
pub(super) const TENANT_ID_HEADER: &str = "x-aero-tenant-id";
pub(super) const REGION_HEADER: &str = "x-aero-region";
pub(super) const MAX_ACCOUNT_SUMMARY_QUERY_BYTES: usize = 8 * 1024;
pub(super) const MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES: usize = 512;
pub(super) const MAX_ACCOUNT_SUMMARY_REGION_BYTES: usize = 128;
pub(super) const MAX_ACCOUNT_SUMMARY_DATASETS: usize = 4;
pub(super) const ACCOUNT_SUMMARY_BINDING_UNAVAILABLE: &str =
    "account summary target binding is unavailable";
pub(super) const ACCOUNT_NOTIFICATION_LIMIT: i64 = 50;

pub(super) fn account_notification_limit_usize() -> usize {
    usize::try_from(ACCOUNT_NOTIFICATION_LIMIT).expect("notification limit fits usize")
}

#[derive(Debug)]
pub(super) struct AccountSummaryRequest {
    /// Opaque Aero ID account identifier. This is deliberately not parsed as
    /// or converted to an Aero `ParticipantId`.
    pub(super) account_id: String,
    pub(super) region: Option<String>,
    pub(super) datasets: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AuthorizedAccountSummaryTarget {
    pub(super) account_id: String,
    pub(super) canonical_uid: String,
    pub(super) tenant_id: String,
    pub(super) region: String,
    pub(super) datasets: BTreeSet<String>,
    pub(super) jti: String,
    pub(super) exp: u64,
}

#[derive(Serialize)]
pub(super) struct AccountSummaryResponse {
    source_region: String,
    version: i64,
    generated_at: String,
    complete: bool,
    datasets: BTreeMap<String, serde_json::Value>,
    sources: Vec<serde_json::Value>,
    memberships: Vec<serde_json::Value>,
}

/// Verify the owner-approved assertion before any source identity or
/// projection lookup. Unsigned headers are checked only as exact consistency
/// fields after the signed target has been verified.
pub(super) async fn authorize_account_summary_target(
    binding: Option<&AccountSummaryTargetVerifier>,
    principal: &MachinePrincipal,
    request: &AccountSummaryRequest,
    source_region: &str,
    headers: &HeaderMap,
) -> Result<AuthorizedAccountSummaryTarget, AeroError> {
    let binding =
        binding.ok_or_else(|| AeroError::Upstream(ACCOUNT_SUMMARY_BINDING_UNAVAILABLE.into()))?;
    let expected_datasets = if request.datasets.is_empty() {
        all_account_summary_dataset_names()
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        request.datasets.clone()
    };
    let verified = binding
        .verify_request(
            &principal.client_id,
            &request.account_id,
            request.region.as_deref(),
            &expected_datasets,
            headers,
        )
        .await?;
    let target = AuthorizedAccountSummaryTarget {
        account_id: verified.account_id.clone(),
        canonical_uid: verified.canonical_uid.clone(),
        tenant_id: verified.tenant_id.clone(),
        region: verified.region.clone(),
        datasets: verified.datasets.clone(),
        jti: verified.jti.clone(),
        exp: verified.exp,
    };
    validate_bound_account_summary_target(request, &target, source_region)?;
    validate_account_summary_legacy_headers(headers, &target)?;
    // Replay consumption is the last gate. No source identity, participant,
    // workspace, notification, projection, or audit lookup may precede it.
    binding.consume_replay(&verified).await?;
    Ok(target)
}

pub(super) fn validate_bound_account_summary_target(
    request: &AccountSummaryRequest,
    target: &AuthorizedAccountSummaryTarget,
    source_region: &str,
) -> Result<(), AeroError> {
    let expected_datasets = if request.datasets.is_empty() {
        all_account_summary_dataset_names()
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        request.datasets.clone()
    };
    let query_region_matches = request
        .region
        .as_deref()
        .is_some_and(|region| region == target.region);
    if target.account_id != request.account_id
        || target.datasets != expected_datasets
        || target.region != source_region
        || !query_region_matches
        || !valid_account_summary_identifier(
            &target.canonical_uid,
            MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES,
        )
        || !valid_account_summary_identifier(
            &target.tenant_id,
            MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES,
        )
    {
        return Err(AeroError::Forbidden(
            "account summary target binding mismatch".into(),
        ));
    }
    Ok(())
}

pub(super) async fn account_summary(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> IntegrationApiResult<Json<AccountSummaryResponse>> {
    let principal = super::authenticate_account_summary_machine(&headers).await?;
    let request = parse_account_summary_query(raw_query.as_deref())?;
    // The signed target/replay gate precedes every identity/projection lookup.
    // Unsigned headers are consistency-only and cannot authorize a target.
    let source_region = env_value("AERO__ACCOUNT_SOURCE__REGION")
        .ok_or_else(|| AeroError::Upstream(ACCOUNT_SUMMARY_BINDING_UNAVAILABLE.into()))?;
    let target = authorize_account_summary_target(
        state.account_summary_binding.as_deref(),
        &principal,
        &request,
        &source_region,
        &headers,
    )
    .await?;
    let canonical_uid = target.canonical_uid.as_str();
    let human_issuer = env_value("AERO__OIDC__ISSUER").ok_or_else(|| {
        AeroError::Invalid("Snaplink human identity issuer is not configured".into())
    })?;
    let participant_id = SsoRepo::new(state.pg.clone())
        .find_participant(&human_issuer, canonical_uid)
        .await?;
    let now = time::OffsetDateTime::now_utc();
    let generated_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let version = i64::try_from(now.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX);
    let requested = requested_account_datasets(&target.datasets);
    let mut datasets = BTreeMap::new();
    let mut sources = Vec::new();
    let mut memberships = Vec::new();

    if let Some(participant_id) = participant_id {
        let participant = ParticipantRepo::new(state.pg.clone())
            .get(participant_id)
            .await?
            .ok_or_else(|| AeroError::NotFound("participant".into()))?;
        let workspaces = WorkspaceRepo::new(state.pg.clone())
            .list_for_participant(participant_id)
            .await?;
        if requested.contains("aero-im.profile") {
            datasets.insert(
                "aero-im.profile".into(),
                serde_json::json!({
                    "display_name": participant.display_name,
                    "avatar_url": participant.avatar_url,
                    "status": "active",
                }),
            );
        }
        if requested.contains("aero-im.workspaces") {
            datasets.insert(
                "aero-im.workspaces".into(),
                serde_json::json!({
                    "items": workspaces.iter().map(|workspace| serde_json::json!({
                        "id": workspace.id, "name": workspace.name, "slug": workspace.slug,
                    })).collect::<Vec<_>>()
                }),
            );
        }
        if requested.contains("aero-im.activity_summary") {
            datasets.insert(
                "aero-im.activity_summary".into(),
                serde_json::json!({"workspace_count": workspaces.len()}),
            );
        }
        if requested.contains("aero-im.notifications") {
            let notifications = state
                .notifications
                .list(
                    participant_id,
                    None,
                    false,
                    Some(ACCOUNT_NOTIFICATION_LIMIT + 1),
                )
                .await?;
            let unread_count = state.notifications.unread_count(participant_id).await?;
            datasets.insert(
                "aero-im.notifications".into(),
                notification_dataset(notifications, unread_count, "active"),
            );
        }
        sources.push(serde_json::json!({
            "source_account_id": target.account_id,
            "scope_type": "account",
            "scope_id": target.account_id,
            "status": "active",
            "data": {},
        }));
        memberships.extend(workspaces.iter().map(|workspace| {
            serde_json::json!({
                "scope_type": "workspace",
                "scope_id": workspace.id,
                "source_member_id": participant_id,
                "role": "member",
                "status": "active",
                "data": {},
            })
        }));
        AuditRepo::new(state.pg.clone())
            .append(
                WorkspaceId::nil(),
                None,
                "integration.account_summary.read",
                Some(&participant_id.to_string()),
                serde_json::json!({"client_id": principal.client_id, "datasets": requested}),
            )
            .await?;
    } else {
        for dataset in requested {
            let value = if dataset == "aero-im.notifications" {
                notification_dataset(Vec::new(), 0, "not_found")
            } else {
                serde_json::json!({"status": "not_found"})
            };
            datasets.insert(dataset.to_owned(), value);
        }
    }

    Ok(Json(AccountSummaryResponse {
        source_region,
        version,
        generated_at,
        complete: true,
        datasets,
        sources,
        memberships,
    }))
}

fn valid_account_summary_query_encoding(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

fn valid_account_summary_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value == value.trim()
        && !value.contains('\u{FFFD}')
        && value
            .chars()
            .all(|character| !character.is_control() && !character.is_whitespace())
}

pub(super) fn parse_account_summary_query(
    raw: Option<&str>,
) -> Result<AccountSummaryRequest, AeroError> {
    let raw = raw.unwrap_or_default();
    if raw.len() > MAX_ACCOUNT_SUMMARY_QUERY_BYTES {
        return Err(AeroError::Invalid(
            "account summary query is too large".into(),
        ));
    }
    if !valid_account_summary_query_encoding(raw)
        || raw.is_empty()
        || raw.starts_with('&')
        || raw.ends_with('&')
        || raw.contains("&&")
    {
        return Err(AeroError::Invalid(
            "account summary query is malformed".into(),
        ));
    }
    let mut account_id = None;
    let mut region = None;
    let mut last_dataset: Option<String> = None;
    let mut datasets = BTreeSet::new();
    for (name, value) in form_urlencoded::parse(raw.as_bytes()) {
        match name.as_ref() {
            "account_id" => {
                if account_id.is_some() {
                    return Err(AeroError::Invalid(
                        "account_id must appear exactly once".into(),
                    ));
                }
                let value = value.into_owned();
                if !valid_account_summary_identifier(&value, MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES) {
                    return Err(AeroError::Invalid("account_id is invalid".into()));
                }
                account_id = Some(value);
            }
            "region" => {
                if region.is_some() {
                    return Err(AeroError::Invalid("region must appear at most once".into()));
                }
                let value = value.into_owned();
                if !valid_account_summary_identifier(&value, MAX_ACCOUNT_SUMMARY_REGION_BYTES) {
                    return Err(AeroError::Invalid("region is invalid".into()));
                }
                region = Some(value);
            }
            "dataset" => {
                let value = value.into_owned();
                if !matches!(
                    value.as_str(),
                    "aero-im.profile"
                        | "aero-im.workspaces"
                        | "aero-im.activity_summary"
                        | "aero-im.notifications"
                ) {
                    return Err(AeroError::Invalid(
                        "unsupported account summary dataset".into(),
                    ));
                }
                if last_dataset
                    .as_deref()
                    .is_some_and(|last| last.as_bytes() >= value.as_bytes())
                {
                    return Err(AeroError::Invalid(
                        "dataset parameters must be in canonical order".into(),
                    ));
                }
                last_dataset = Some(value.clone());
                if !datasets.insert(value) {
                    return Err(AeroError::Invalid("dataset must appear only once".into()));
                }
            }
            _ => {
                return Err(AeroError::Invalid(
                    "unknown account summary query parameter".into(),
                ));
            }
        }
    }
    if datasets.len() > MAX_ACCOUNT_SUMMARY_DATASETS {
        return Err(AeroError::Invalid(
            "too many account summary datasets".into(),
        ));
    }
    Ok(AccountSummaryRequest {
        account_id: account_id
            .ok_or_else(|| AeroError::Invalid("account_id is required".into()))?,
        region: Some(region.ok_or_else(|| AeroError::Invalid("region is required".into()))?),
        datasets,
    })
}

fn all_account_summary_dataset_names() -> [&'static str; 4] {
    [
        "aero-im.profile",
        "aero-im.workspaces",
        "aero-im.activity_summary",
        "aero-im.notifications",
    ]
}

pub(super) fn requested_account_datasets(values: &BTreeSet<String>) -> BTreeSet<&str> {
    if values.is_empty() {
        return BTreeSet::from(all_account_summary_dataset_names());
    }
    values.iter().map(String::as_str).collect()
}

pub(super) fn notification_dataset(
    mut notifications: Vec<Notification>,
    unread_count: u64,
    source_account_status: &str,
) -> serde_json::Value {
    let limit = account_notification_limit_usize();
    let truncated = notifications.len() > limit;
    notifications.truncate(limit);
    let items = notifications
        .iter()
        .map(|notification| {
            serde_json::json!({
                "notification_id": notification.id,
                "room_id": notification.room_id,
                "message_id": notification.message_id,
                "kind": notification.kind,
                "actor_id": notification.actor_id,
                "created_at": notification.created_at,
                "read_at": notification.read_at,
                "aggregate_count": notification.aggregate_count,
                "importance_score": notification.importance_score,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "items": items,
        "unread_count": unread_count,
        "truncated": truncated,
        "source_account_status": source_account_status,
    })
}

fn required_account_header<'a>(
    headers: &'a HeaderMap,
    name: &str,
    max_bytes: usize,
) -> Result<&'a str, AeroError> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .ok_or_else(|| AeroError::Invalid(format!("{name} is required")))?;
    if values.next().is_some() {
        return Err(AeroError::Invalid(format!(
            "exactly one {name} header is required"
        )));
    }
    let value = value
        .to_str()
        .map_err(|_| AeroError::Invalid(format!("{name} must be valid ASCII")))?;
    if !valid_account_summary_identifier(value, max_bytes) {
        return Err(AeroError::Invalid(format!("{name} is invalid")));
    }
    Ok(value)
}

/// Requires the signed target's identity headers as exact wire consistency
/// fields. They are never accepted as authorization without the assertion.
pub(super) fn validate_account_summary_legacy_headers(
    headers: &HeaderMap,
    target: &AuthorizedAccountSummaryTarget,
) -> Result<(), AeroError> {
    for (name, expected, max_bytes) in [
        (
            ACCOUNT_ID_HEADER,
            target.account_id.as_str(),
            MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES,
        ),
        (
            CANONICAL_UID_HEADER,
            target.canonical_uid.as_str(),
            MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES,
        ),
        (
            TENANT_ID_HEADER,
            target.tenant_id.as_str(),
            MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES,
        ),
        (
            REGION_HEADER,
            target.region.as_str(),
            MAX_ACCOUNT_SUMMARY_REGION_BYTES,
        ),
    ] {
        if required_account_header(headers, name, max_bytes)? != expected {
            return Err(AeroError::Invalid(
                "account summary target binding header mismatch".into(),
            ));
        }
    }
    // X-Aero-Actor-UID is deliberately not authorization material: the target
    // assertion has no actor claim, so this diagnostic value is ignored.
    Ok(())
}
