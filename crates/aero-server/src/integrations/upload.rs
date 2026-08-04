use super::*;

const MAX_UPLOAD_NAME_BYTES: usize = 255;
const MAX_UPLOAD_MIME_BYTES: usize = 255;
const MAX_UPLOAD_SUBJECT_BYTES: usize = 2_048;
const ROOM_TARGET_HEADER: &str = "x-aero-room-id";
const SUBJECT_TARGET_HEADER: &str = "x-aero-snaplink-subject";

fn upload_target(headers: &HeaderMap) -> Result<IntegrationTarget, AeroError> {
    let room_id = exact_target_header(headers, ROOM_TARGET_HEADER, 64)?;
    let subject = exact_target_header(headers, SUBJECT_TARGET_HEADER, MAX_UPLOAD_SUBJECT_BYTES)?;
    match (room_id, subject) {
        (Some(room), None) => RoomId::from_str(room.trim())
            .map(IntegrationTarget::Room)
            .map_err(|_| AeroError::Invalid("invalid integration room target header".into())),
        (None, Some(subject)) => Ok(IntegrationTarget::SnaplinkUser(subject)),
        _ => Err(AeroError::Invalid(
            "exactly one integration upload target header is required".into(),
        )),
    }
}

fn exact_target_header(
    headers: &HeaderMap,
    name: &'static str,
    max_bytes: usize,
) -> Result<Option<String>, AeroError> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(AeroError::Invalid(
            "integration upload target headers must be single-valued".into(),
        ));
    }
    let value = value
        .to_str()
        .map_err(|_| AeroError::Invalid("integration upload target header is invalid".into()))?;
    if value.is_empty()
        || !value.is_ascii()
        || value != value.trim()
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
    {
        return Err(AeroError::Invalid(
            "integration upload target header is invalid".into(),
        ));
    }
    Ok(Some(value.to_owned()))
}

pub(super) async fn upload_blob(
    State(state): State<AppState>,
    Path(installation_raw): Path<String>,
    headers: HeaderMap,
    multipart: Multipart,
) -> IntegrationApiResult<Response> {
    let installation_id = parse_installation(&installation_raw)?;
    let principal = authenticate_machine(&headers).await?;
    let idempotency_key = required_idempotency_key(&headers)?;
    let target = upload_target(&headers)?;
    let repo = IntegrationRepo::new(state.pg.clone());
    // Reject invalid credentials *and targets* before buffering, sniffing,
    // hashing, or virus-scanning a 32 MiB body. This preflight is read-only and
    // never creates a DM. The global HTTP limiter runs outside this router and
    // the post-body claim/commit repeat all mutable authority checks.
    repo.preauthorize_target(
        installation_id,
        &principal.issuer,
        &principal.client_id,
        &target,
    )
    .await?;
    // Keep at most a small, configurable number of complete upload bodies in
    // this process. The permit is intentionally held through storage/commit,
    // so accepted bytes cannot pile up behind a slower virus scanner or vault.
    let _upload_permit = integration_upload_gate().acquire().await?;
    let upload = read_integration_upload(multipart).await?;
    let request_hash = blob_request_hash(&target, &upload)?;
    let probe = IntegrationBlobProbe {
        installation_id,
        issuer: principal.issuer.clone(),
        client_id: principal.client_id.clone(),
        idempotency_key,
        request_hash,
        target: target.clone(),
    };
    let mut backoff = ClaimBackoff::new();
    let (lease_token, prepared) = loop {
        match repo.claim_blob(&probe).await? {
            IntegrationBlobClaim::Replay(outcome) => return Ok(blob_response(&outcome)),
            IntegrationBlobClaim::Pending => {
                let Some(delay) = backoff.next_delay() else {
                    return Ok(request_pending_response());
                };
                sleep(delay).await;
            }
            IntegrationBlobClaim::Acquired {
                lease_token,
                target,
            } => break (lease_token, target),
        }
    };

    let mut resolved_for_cleanup = None;
    let mut reservation_to_abort = None;
    let result: Result<IntegrationBlobOutcome, AeroError> = async {
        crate::ws_rate::check_ws_rate(&state, prepared.installation.workspace_id).await?;
        let resolved = repo.materialize_target(prepared).await?;
        resolved_for_cleanup = Some(resolved.clone());
        state
            .im
            .assert_message_send_preflight(resolved.installation.bot_id, resolved.room_id)
            .await?;
        let configured_region = state
            .workspaces
            .region_code(resolved.installation.workspace_id)
            .await?;
        let storage_region = state
            .region_router
            .canonical_region_code(configured_region.as_deref())
            .map_err(|error| {
                AeroError::Internal(anyhow::anyhow!(
                    "workspace {} has unusable storage region: {error}",
                    resolved.installation.workspace_id
                ))
            })?;
        let existing = state
            .blobs
            .find_by_owner_sha256_in_scope(
                resolved.installation.bot_id,
                &upload.sha256,
                Some(resolved.installation.workspace_id),
                &storage_region,
            )
            .await?;
        let (blob_id, storage_key) = if let Some(existing) = existing {
            (existing.id, None)
        } else {
            let reservation = state
                .blobs
                .reserve_in_scope(
                    NewBlob {
                        owner_id: resolved.installation.bot_id,
                        kind: upload.kind.clone(),
                        name: upload.name.clone(),
                        mime: upload.mime.clone(),
                        size: upload.bytes.len() as u64,
                        sha256: Some(upload.sha256.clone()),
                        storage_key: format!("pending:{}", Uuid::new_v4()),
                    },
                    Some(resolved.installation.workspace_id),
                    Some(&storage_region),
                )
                .await?;
            reservation_to_abort = Some(reservation.id);
            let quota_reservation = IntegrationBlobQuotaReservation {
                installation_id,
                issuer: principal.issuer.clone(),
                client_id: principal.client_id.clone(),
                idempotency_key,
                request_hash,
                target: resolved.target.clone(),
                room_id: resolved.room_id,
                recipient: resolved.recipient,
                lease_token,
                blob_id: reservation.id,
                content_sha256: upload.sha256.clone(),
            };
            let storage_key = put_after_quota_reservation(
                state.blob_store.as_ref(),
                reservation.id,
                upload.bytes,
                async { repo.reserve_blob_upload_quota(quota_reservation).await },
            )
            .await?;
            (reservation.id, Some(storage_key))
        };
        let commit = IntegrationBlobCommit {
            installation_id,
            issuer: principal.issuer,
            client_id: principal.client_id,
            idempotency_key,
            request_hash,
            target: resolved.target.clone(),
            room_id: resolved.room_id,
            recipient: resolved.recipient,
            lease_token,
            blob_id,
            content_sha256: upload.sha256,
            storage_key,
        };
        match repo.commit_blob(commit).await {
            Ok(outcome) => Ok(outcome),
            Err(commit_error) => match repo.resolve_blob_commit(&probe, lease_token).await {
                Ok(IntegrationBlobCommitResolution::Completed(outcome)) => Ok(*outcome),
                Ok(IntegrationBlobCommitResolution::Released) => Err(commit_error),
                Ok(IntegrationBlobCommitResolution::Lost) | Err(_) => {
                    // The object may be canonical or owned by a replacement
                    // lease. Preserve it; stale-reservation GC is the only safe
                    // recovery path when commit outcome cannot be proven.
                    reservation_to_abort = None;
                    Err(commit_error)
                }
            },
        }
    }
    .await;

    match result {
        Ok(outcome) => {
            if let Some(reservation) = reservation_to_abort {
                if reservation != outcome.blob.id {
                    crate::routes::routes::abort_blob_reservation(&state, reservation).await;
                }
            }
            Ok(blob_response(&outcome))
        }
        Err(error) => {
            if let Some(reservation) = reservation_to_abort {
                crate::routes::routes::abort_blob_reservation(&state, reservation).await;
            }
            cleanup_failed_request(
                &repo,
                resolved_for_cleanup.as_ref(),
                installation_id,
                idempotency_key,
                lease_token,
                true,
            )
            .await;
            Err(error.into())
        }
    }
}

/// Keep the external object-store side effect structurally downstream of the
/// durable, fenced installation-quota reservation. A rejected reservation must
/// never reach `BlobStore::put`.
async fn put_after_quota_reservation<F>(
    blob_store: &dyn aero_storage::BlobStore,
    blob_id: aero_common::BlobId,
    bytes: Bytes,
    quota_reservation: F,
) -> Result<String, AeroError>
where
    F: std::future::Future<Output = Result<(), AeroError>>,
{
    quota_reservation.await?;
    blob_store
        .put(blob_id, bytes)
        .await
        .map_err(|error| AeroError::Internal(anyhow::anyhow!("blob put: {error}")))
}

struct PreparedIntegrationUpload {
    name: String,
    mime: String,
    kind: FileKind,
    bytes: Bytes,
    sha256: String,
}

async fn read_integration_upload(
    mut multipart: Multipart,
) -> Result<PreparedIntegrationUpload, AeroError> {
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| AeroError::Invalid(error.to_string()))?
    {
        if field.name() != Some("file") {
            continue;
        }
        let name = field.file_name().unwrap_or("untitled").to_owned();
        let mime = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_owned();
        validate_upload_metadata(&name, &mime)?;
        let bytes = field
            .bytes()
            .await
            .map_err(|error| AeroError::Invalid(error.to_string()))?;
        if bytes.len() > MAX_INTEGRATION_BLOB_BYTES {
            return Err(AeroError::Invalid(format!(
                "blob too large: {} bytes",
                bytes.len()
            )));
        }
        if !is_allowed_integration_mime(&mime) {
            return Err(AeroError::Invalid(
                "unsupported integration upload media type".into(),
            ));
        }
        if !crate::content_sniff::is_consistent(&mime, &bytes) {
            return Err(AeroError::Invalid(
                "upload content does not match its declared media type or is not allowed".into(),
            ));
        }
        if let Some(scanner) = crate::av_scan::ClamdScanner::global() {
            match scanner.scan(&bytes).await {
                crate::av_scan::ScanVerdict::Clean => crate::av_scan::record_scan("clean"),
                crate::av_scan::ScanVerdict::Infected(signature) => {
                    crate::av_scan::record_scan("infected");
                    return Err(AeroError::Invalid(format!(
                        "file rejected by virus scan ({signature})"
                    )));
                }
                crate::av_scan::ScanVerdict::Error(diagnostic) => {
                    crate::av_scan::record_scan("error");
                    if crate::av_scan::fail_closed() {
                        return Err(AeroError::Upstream(format!(
                            "virus scanner unavailable: {diagnostic}"
                        )));
                    }
                    tracing::warn!(error = %diagnostic, "integration clamd scan failed open");
                }
            }
        }
        let sha256 = hex::encode(Sha256::digest(&bytes));
        return Ok(PreparedIntegrationUpload {
            name,
            kind: integration_file_kind(&mime),
            mime,
            bytes,
            sha256,
        });
    }
    Err(AeroError::Invalid("multipart missing 'file' field".into()))
}

fn validate_upload_metadata(name: &str, mime: &str) -> Result<(), AeroError> {
    for (value, max) in [(name, MAX_UPLOAD_NAME_BYTES), (mime, MAX_UPLOAD_MIME_BYTES)] {
        if value.is_empty()
            || value != value.trim()
            || value.len() > max
            || value.chars().any(char::is_control)
        {
            return Err(AeroError::Invalid(
                "upload metadata must be trimmed, non-empty, control-free, and within size limits"
                    .into(),
            ));
        }
    }
    Ok(())
}

fn blob_request_hash(
    target: &IntegrationTarget,
    upload: &PreparedIntegrationUpload,
) -> Result<[u8; 32], AeroError> {
    let encoded = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "target_kind": target.kind(),
        "target_key": target.key(),
        "name": upload.name,
        "mime": upload.mime,
        "sha256": upload.sha256,
    }))?;
    Ok(Sha256::digest(encoded).into())
}

fn is_allowed_integration_mime(mime: &str) -> bool {
    mime.starts_with("image/")
        || mime.starts_with("video/")
        || mime.starts_with("audio/")
        || matches!(
            mime,
            "application/pdf"
                | "text/plain"
                | "text/csv"
                | "application/zip"
                | "application/x-zip-compressed"
                | "application/octet-stream"
        )
}

fn integration_file_kind(mime: &str) -> FileKind {
    if mime.starts_with("image/") {
        FileKind::Image
    } else if mime.starts_with("video/") {
        FileKind::Video
    } else if mime.starts_with("audio/") {
        FileKind::Audio
    } else if mime == "application/pdf" || mime.starts_with("text/") {
        FileKind::Document
    } else {
        FileKind::Other
    }
}

fn blob_response(outcome: &IntegrationBlobOutcome) -> Response {
    let status = if outcome.deduplicated {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let mut response = (
        status,
        Json(serde_json::json!({
            "id": outcome.blob.id,
            "name": outcome.blob.name,
            "mime": outcome.blob.mime,
            "size": outcome.blob.size,
            "kind": outcome.blob.kind,
            "workspace_id": outcome.blob.workspace_id,
            "storage_region": outcome.blob.storage_region.as_deref().unwrap_or(
                aero_storage::DEFAULT_STORAGE_REGION
            ),
            "residency_scoped": true,
            "message_attachment_eligible": true,
            "deduplicated": outcome.deduplicated,
        })),
    )
        .into_response();
    response.headers_mut().insert(
        "idempotency-replayed",
        HeaderValue::from_static(if outcome.deduplicated {
            "true"
        } else {
            "false"
        }),
    );
    response
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aero_storage::{BlobStore, BlobStoreError};
    use async_trait::async_trait;

    use super::*;

    struct PutCountingStore {
        puts: AtomicUsize,
    }

    #[async_trait]
    impl BlobStore for PutCountingStore {
        async fn put(
            &self,
            _id: aero_common::BlobId,
            _bytes: Bytes,
        ) -> Result<String, BlobStoreError> {
            self.puts.fetch_add(1, Ordering::Relaxed);
            Ok("fake".into())
        }

        async fn get(&self, _id: aero_common::BlobId) -> Result<Bytes, BlobStoreError> {
            Err(BlobStoreError::NotFound)
        }

        async fn delete(&self, _id: aero_common::BlobId) -> Result<(), BlobStoreError> {
            Ok(())
        }

        fn key_for(&self, _id: aero_common::BlobId) -> String {
            "fake".into()
        }
    }

    fn upload(name: &str, mime: &str, digest: &str) -> PreparedIntegrationUpload {
        PreparedIntegrationUpload {
            name: name.into(),
            mime: mime.into(),
            kind: FileKind::Document,
            bytes: Bytes::from_static(b"payload"),
            sha256: digest.into(),
        }
    }

    #[tokio::test]
    async fn quota_rejection_never_reaches_blob_store_put() {
        let store = PutCountingStore {
            puts: AtomicUsize::new(0),
        };
        let result = put_after_quota_reservation(
            &store,
            aero_common::BlobId::new(),
            Bytes::from_static(b"must-not-be-written"),
            async {
                Err(AeroError::Conflict(
                    "integration blob storage quota exceeded".into(),
                ))
            },
        )
        .await;

        assert!(matches!(result, Err(AeroError::Conflict(_))));
        assert_eq!(store.puts.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn upload_target_requires_exactly_one_selector() {
        let mut headers = HeaderMap::new();
        assert!(upload_target(&headers).is_err());
        headers.insert(
            ROOM_TARGET_HEADER,
            RoomId::new().to_string().parse().unwrap(),
        );
        assert!(matches!(
            upload_target(&headers),
            Ok(IntegrationTarget::Room(_))
        ));
        headers.insert(SUBJECT_TARGET_HEADER, HeaderValue::from_static("subject"));
        assert!(upload_target(&headers).is_err());
    }

    #[test]
    fn upload_target_headers_are_exact_bounded_and_single_valued() {
        let mut headers = HeaderMap::new();
        headers.insert(
            SUBJECT_TARGET_HEADER,
            HeaderValue::from_static("stable-subject"),
        );
        assert!(matches!(
            upload_target(&headers),
            Ok(IntegrationTarget::SnaplinkUser(subject)) if subject == "stable-subject"
        ));
        headers.append(
            SUBJECT_TARGET_HEADER,
            HeaderValue::from_static("other-subject"),
        );
        assert!(upload_target(&headers).is_err());

        let mut oversized = HeaderMap::new();
        oversized.insert(
            SUBJECT_TARGET_HEADER,
            HeaderValue::from_str(&"x".repeat(MAX_UPLOAD_SUBJECT_BYTES + 1)).unwrap(),
        );
        assert!(upload_target(&oversized).is_err());

        let mut non_ascii = HeaderMap::new();
        non_ascii.insert(
            SUBJECT_TARGET_HEADER,
            HeaderValue::from_bytes("subject-é".as_bytes()).unwrap(),
        );
        assert!(upload_target(&non_ascii).is_err());
    }

    #[test]
    fn upload_hash_binds_target_metadata_and_content() {
        let target = IntegrationTarget::Room(RoomId::new());
        let base = blob_request_hash(&target, &upload("a.txt", "text/plain", "aa")).unwrap();
        assert_eq!(
            base,
            blob_request_hash(&target, &upload("a.txt", "text/plain", "aa")).unwrap()
        );
        assert_ne!(
            base,
            blob_request_hash(&target, &upload("b.txt", "text/plain", "aa")).unwrap()
        );
        assert_ne!(
            base,
            blob_request_hash(&target, &upload("a.txt", "text/plain", "bb")).unwrap()
        );
    }

    #[test]
    fn upload_mime_and_kind_policy_match_attachment_surface() {
        assert!(is_allowed_integration_mime("application/pdf"));
        assert!(!is_allowed_integration_mime("text/html"));
        assert_eq!(integration_file_kind("audio/ogg"), FileKind::Audio);
        assert_eq!(integration_file_kind("text/plain"), FileKind::Document);
    }

    #[test]
    fn upload_metadata_is_exact_bounded_and_control_free() {
        assert!(validate_upload_metadata("report.pdf", "application/pdf").is_ok());
        for (name, mime) in [
            ("", "application/pdf"),
            (" report.pdf", "application/pdf"),
            ("report\r\nX-Evil: yes.pdf", "application/pdf"),
            ("report.pdf", " application/pdf"),
            ("report.pdf", "application/pdf\ntext/html"),
        ] {
            let error = validate_upload_metadata(name, mime).expect_err("metadata is invalid");
            assert!(!error.to_string().contains(mime));
        }
        assert!(validate_upload_metadata(
            &"x".repeat(MAX_UPLOAD_NAME_BYTES + 1),
            "application/pdf"
        )
        .is_err());
        assert!(
            validate_upload_metadata("report.pdf", &"x".repeat(MAX_UPLOAD_MIME_BYTES + 1)).is_err()
        );
    }
}
