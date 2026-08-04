// ----- Blobs (multipart upload, byte download) -----

const MAX_BLOB_BYTES: usize = 32 * 1024 * 1024; // 32 MiB

/// MIME-type prefix allowlist for uploads. Covers image, video, and audio.
const ALLOWED_MIME_PREFIXES: &[&str] = &["image/", "video/", "audio/"];
/// Exact MIME types allowed beyond the prefix allowlist above.
const ALLOWED_MIME_EXACT: &[&str] = &[
    "application/pdf",
    "text/plain",
    "text/csv",
    "application/zip",
    "application/x-zip-compressed",
    "application/octet-stream", // browser default for binary files without an extension
];

fn is_allowed_mime(mime: &str) -> bool {
    ALLOWED_MIME_PREFIXES.iter().any(|p| mime.starts_with(p)) || ALLOWED_MIME_EXACT.contains(&mime)
}

async fn blob_upload(
    State(s): State<AppState>,
    auth: AuthUser,
    mp: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    // Compatibility endpoint: there is no authenticated tenant in the URL, so
    // bytes go to default storage and the response explicitly declines a
    // residency guarantee. These unscoped objects are personal/read-only
    // compatibility data and cannot be attached to a new message; message
    // clients must use `POST /api/rooms/:id/blobs`.
    crate::ws_rate::check_ws_rate_participant(&s, auth.participant_id).await?;
    persist_blob_upload(
        &s,
        auth.participant_id,
        mp,
        None,
        aero_storage::DEFAULT_STORAGE_REGION,
        false,
    )
    .await
}

/// Room-scoped upload. The room access choke point supplies a trusted workspace
/// context; clients cannot choose another tenant or backend in multipart data.
async fn room_blob_upload(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    mp: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    let room = aero_common::RoomId::from_str(&room_str)
        .map_err(|error| AeroError::Invalid(format!("room id: {error}")))?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    crate::ws_rate::check_ws_rate_room(&s, room).await?;
    let workspace = s
        .rooms
        .room_workspace(room)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("room".into()))?;
    let configured_region = s
        .workspaces
        .region_code(workspace)
        .await
        .map_err(AeroError::from)?;
    let storage_region = s
        .region_router
        .canonical_region_code(configured_region.as_deref())
        .map_err(|error| {
            AeroError::Internal(anyhow::anyhow!(
                "workspace {workspace} has unusable storage region: {error}"
            ))
        })?;
    persist_blob_upload(
        &s,
        auth.participant_id,
        mp,
        Some(workspace),
        &storage_region,
        true,
    )
    .await
}

pub(crate) async fn persist_blob_upload(
    s: &AppState,
    participant: ParticipantId,
    mut mp: Multipart,
    workspace_id: Option<WorkspaceId>,
    storage_region: &str,
    residency_scoped: bool,
) -> ApiResult<Json<serde_json::Value>> {
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| AeroError::Invalid(e.to_string()))?
    {
        if field.name() != Some("file") {
            continue;
        }
        let name = field.file_name().unwrap_or("untitled").to_owned();
        let mime = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_owned();
        let bytes = field
            .bytes()
            .await
            .map_err(|e| AeroError::Invalid(e.to_string()))?;
        if bytes.len() > MAX_BLOB_BYTES {
            return Err(
                AeroError::Invalid(format!("blob too large: {} bytes", bytes.len())).into(),
            );
        }
        if !is_allowed_mime(&mime) {
            return Err(AeroError::Invalid(format!("unsupported file type: {mime}")).into());
        }
        // Defence-in-depth: the MIME above is client-claimed, so sniff the actual
        // bytes — reject executables / HTML / SVG payloads disguised as an allowed
        // type (stored malware / stored-XSS), and binary types whose content does
        // not match their declared family.
        if !crate::content_sniff::is_consistent(&mime, &bytes) {
            return Err(AeroError::Invalid(format!(
                "file content does not match its declared type ({mime}), or is a disallowed executable/markup payload"
            ))
            .into());
        }
        // Real anti-virus scan (defence-in-depth beyond the magic-byte sniffer):
        // a malicious binary carrying a benign header passes `is_consistent`, so —
        // when a `clamd` endpoint is configured (`AERO_CLAMAV_HOST`) — stream the
        // bytes to ClamAV via INSTREAM.
        //
        //   * `Infected` → reject the upload (400) with the signature name.
        //   * `Error` (clamd unreachable / timeout / daemon error) → policy:
        //     fail-OPEN by default (warn + metric, allow through, so a flaky
        //     daemon doesn't block every upload); `AERO_CLAMAV_FAIL_CLOSED` flips
        //     it to fail-CLOSED (reject with 502).
        //   * `Clean` → proceed.
        if let Some(scanner) = crate::av_scan::ClamdScanner::global() {
            match scanner.scan(&bytes).await {
                crate::av_scan::ScanVerdict::Clean => {
                    crate::av_scan::record_scan("clean");
                }
                crate::av_scan::ScanVerdict::Infected(sig) => {
                    crate::av_scan::record_scan("infected");
                    tracing::warn!(
                        participant = %participant,
                        signature = %sig,
                        "blob upload rejected: malware detected by clamd",
                    );
                    return Err(
                        AeroError::Invalid(format!("file rejected by virus scan ({sig})")).into(),
                    );
                }
                crate::av_scan::ScanVerdict::Error(diag) => {
                    crate::av_scan::record_scan("error");
                    if crate::av_scan::fail_closed() {
                        tracing::warn!(error = %diag, "blob upload rejected: clamd unavailable (fail-closed)");
                        return Err(AeroError::Upstream(format!(
                            "virus scanner unavailable: {diag}"
                        ))
                        .into());
                    }
                    tracing::warn!(error = %diag, "clamd scan failed; allowing upload (fail-open)");
                }
            }
        }
        let kind = guess_file_kind(&mime);
        let size = bytes.len() as u64;
        let sha256_hex = hex::encode(sha2::Sha256::digest(&bytes));

        // Content dedup (owner-scoped): if this participant already uploaded
        // identical bytes, return the existing blob without re-writing storage.
        if let Some(existing) = s
            .blobs
            .find_by_owner_sha256_in_scope(participant, &sha256_hex, workspace_id, storage_region)
            .await
            .map_err(AeroError::from)?
        {
            return Ok(Json(serde_json::json!({
                "id": existing.id,
                "name": existing.name,
                "mime": existing.mime,
                "size": existing.size,
                "kind": existing.kind,
                "workspace_id": existing.workspace_id,
                "storage_region": existing.storage_region.as_deref().unwrap_or(
                    aero_storage::DEFAULT_STORAGE_REGION
                ),
                "residency_scoped": residency_scoped,
                "message_attachment_eligible": residency_scoped,
            })));
        }

        let reservation = s
            .blobs
            .reserve_in_scope(
                NewBlob {
                    owner_id: participant,
                    kind,
                    name: name.clone(),
                    mime: mime.clone(),
                    size,
                    sha256: Some(sha256_hex),
                    storage_key: format!("pending:{}", uuid::Uuid::new_v4()),
                },
                workspace_id,
                Some(storage_region),
            )
            .await
            .map_err(AeroError::from)?;
        let key = match s.blob_store.put(reservation.id, bytes).await {
            Ok(key) => key,
            Err(error) => {
                abort_blob_reservation(&s, reservation.id).await;
                return Err(AeroError::Internal(anyhow::anyhow!("blob put: {error}")).into());
            }
        };
        let blob = match s.blobs.finalize(reservation.id, &key).await {
            Ok(Some(blob)) => blob,
            Ok(None) => {
                abort_blob_reservation(&s, reservation.id).await;
                return Err(AeroError::Internal(anyhow::anyhow!(
                    "blob reservation disappeared before finalize"
                ))
                .into());
            }
            Err(error) => {
                abort_blob_reservation(&s, reservation.id).await;
                return Err(AeroError::from(error).into());
            }
        };
        return Ok(Json(serde_json::json!({
            "id": blob.id,
            "name": blob.name,
            "mime": blob.mime,
            "size": blob.size,
            "kind": blob.kind,
            "workspace_id": blob.workspace_id,
            "storage_region": blob.storage_region.as_deref().unwrap_or(
                aero_storage::DEFAULT_STORAGE_REGION
            ),
            "residency_scoped": residency_scoped,
            "message_attachment_eligible": residency_scoped,
        })));
    }
    Err(AeroError::Invalid("multipart missing 'file' field".into()).into())
}

/// Compensate a failed upload/finalize transition.
///
/// If object deletion succeeds, discard the unpublished metadata immediately.
/// If the backend is unavailable, leave the reservation in place and enqueue it
/// so the periodic delete-then-ack worker retries without exposing it to reads.
pub(crate) async fn abort_blob_reservation(s: &AppState, id: BlobId) {
    match s.blob_store.delete(id).await {
        Ok(()) => {
            if let Err(error) = s.blobs.discard_reservation(id).await {
                tracing::warn!(%id, %error, "failed to discard aborted blob reservation");
            }
        }
        Err(error) => {
            tracing::warn!(%id, %error, "failed to delete aborted blob object; queued for retry");
            if let Err(queue_error) = aero_storage::BlobGcRepo::new(s.pg.clone())
                .enqueue_one(id)
                .await
            {
                tracing::warn!(%id, %queue_error, "failed to enqueue aborted blob reservation");
            }
        }
    }
}

async fn blob_download(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    request: axum::extract::Request,
) -> ApiResult<axum::response::Response> {
    let id = BlobId::from_str(&id_str).map_err(|e| AeroError::Invalid(format!("blob id: {e}")))?;
    // Tenant fairness (ROADMAP3 方向五): charge the authorized download against
    // the caller's workspace budget before metadata or object-store work.
    crate::ws_rate::check_ws_rate_participant(&s, auth.participant_id).await?;
    let meta = s
        .blobs
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("blob".into()))?;
    // IDOR guard: a logged-in user may only download a blob they uploaded or one
    // referenced by a message in a room they belong to. Without this any holder
    // of a blob id could read any attachment.
    if !s
        .blobs
        .is_accessible_by(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?
    {
        return Err(AeroError::Forbidden("blob".into()).into());
    }
    // Conditional/range evaluation intentionally happens only after the IDOR
    // guard above. A caller cannot use ETag or size responses to probe a blob
    // they are not authorized to read.
    let send_body = request.method() != axum::http::Method::HEAD;
    authorized_blob_response(
        s.blob_store.as_ref(),
        id,
        &meta,
        request.headers(),
        send_body,
    )
    .await
}

// Blob authorization is dynamic (room membership, deletion, expiry). Clients may
// cache bytes but must revalidate every use so a 403 after access removal cannot
// be bypassed by an immutable browser-cache hit.
const BLOB_CACHE_CONTROL: &str = "private, no-cache, max-age=0, must-revalidate";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DownloadRange {
    start: u64,
    end: u64,
}

impl DownloadRange {
    const fn len(self) -> u64 {
        self.end - self.start + 1
    }
}

async fn authorized_blob_response(
    store: &dyn aero_storage::BlobStore,
    id: BlobId,
    meta: &aero_common::Blob,
    request_headers: &axum::http::HeaderMap,
    send_body: bool,
) -> ApiResult<axum::response::Response> {
    let etag = strong_etag(meta.sha256.as_deref());
    if etag
        .as_ref()
        .is_some_and(|tag| if_none_match_matches(request_headers, tag))
    {
        let mut response = axum::response::Response::new(axum::body::Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        apply_common_blob_headers(response.headers_mut(), etag.as_ref());
        return Ok(response);
    }

    // RFC 9110 defines Range for GET. Axum dispatches HEAD through GET handlers,
    // so answer HEAD from metadata and never open the blob backend.
    if !send_body {
        let mut response = axum::response::Response::new(axum::body::Body::empty());
        apply_common_blob_headers(response.headers_mut(), etag.as_ref());
        apply_blob_entity_headers(response.headers_mut(), meta);
        response.headers_mut().insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&meta.size.to_string())
                .expect("numeric content-length is a valid header"),
        );
        return Ok(response);
    }

    let parsed_range = if if_range_allows_range(request_headers, etag.as_ref()) {
        parse_download_range(request_headers, meta.size)
    } else {
        Ok(None)
    };
    let Ok(range) = parsed_range else {
        let mut response = axum::response::Response::new(axum::body::Body::empty());
        *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
        apply_common_blob_headers(response.headers_mut(), etag.as_ref());
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes */{}", meta.size))
                .expect("numeric content-range is a valid header"),
        );
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
        return Ok(response);
    };
    let storage_range = range.map(|range| {
        aero_storage::blob_store::BlobRange::new(range.start, range.end)
            .expect("HTTP parser only produces ordered ranges")
    });
    let stream = store
        .get_stream(id, storage_range)
        .await
        .map_err(|e| AeroError::Internal(anyhow::anyhow!("blob get: {e}")))?;
    let mut response = axum::response::Response::new(axum::body::Body::from_stream(stream));
    *response.status_mut() = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    let headers = response.headers_mut();
    apply_common_blob_headers(headers, etag.as_ref());
    apply_blob_entity_headers(headers, meta);
    if let Some(range) = range {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!(
                "bytes {}-{}/{}",
                range.start, range.end, meta.size
            ))
            .expect("numeric content-range is a valid header"),
        );
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&range.len().to_string())
                .expect("numeric content-length is a valid header"),
        );
    } else {
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&meta.size.to_string())
                .expect("numeric content-length is a valid header"),
        );
    }
    Ok(response)
}

fn apply_common_blob_headers(headers: &mut axum::http::HeaderMap, etag: Option<&HeaderValue>) {
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(BLOB_CACHE_CONTROL),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Some(etag) = etag {
        headers.insert(header::ETAG, etag.clone());
    }
}

fn apply_blob_entity_headers(headers: &mut axum::http::HeaderMap, meta: &aero_common::Blob) {
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_str(&meta.mime)
            .unwrap_or_else(|_| header::HeaderValue::from_static("application/octet-stream")),
    );
    // Anti-XSS: force attachment disposition for non-visual content types.
    // Images, video, audio, and PDF may render inline (expected UX); everything
    // else (HTML, SVG, Office docs, executables) is forced to download so a
    // malicious blob cannot execute scripts in the browser (ROADMAP 方向三).
    // The nosniff header prevents MIME-type confusion attacks regardless.
    let inline_safe = is_inline_safe_mime(&meta.mime);
    let disposition = content_disposition(inline_safe, &meta.name);
    headers.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&disposition).unwrap_or_else(|_| {
            if inline_safe {
                header::HeaderValue::from_static("inline")
            } else {
                header::HeaderValue::from_static("attachment")
            }
        }),
    );
}

fn is_inline_safe_mime(mime: &str) -> bool {
    (mime.starts_with("image/") && mime != "image/svg+xml")
        || mime.starts_with("video/")
        || mime.starts_with("audio/")
        || mime == "application/pdf"
}

fn content_disposition(inline_safe: bool, filename: &str) -> String {
    let mode = if inline_safe { "inline" } else { "attachment" };
    let mut safe = filename
        .chars()
        .map(|character| match character {
            '\r' | '\n' | '"' | '\\' | '/' => '_',
            character if character.is_control() => '_',
            character => character,
        })
        .collect::<String>();
    if safe.trim().is_empty() {
        safe = "download".into();
    }
    format!("{mode}; filename=\"{safe}\"")
}

fn strong_etag(sha256: Option<&str>) -> Option<HeaderValue> {
    let sha256 = sha256?;
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    HeaderValue::from_str(&format!("\"{sha256}\"")).ok()
}

fn if_none_match_matches(headers: &axum::http::HeaderMap, etag: &HeaderValue) -> bool {
    let Ok(etag) = etag.to_str() else {
        return false;
    };
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
        })
}

fn if_range_allows_range(headers: &axum::http::HeaderMap, etag: Option<&HeaderValue>) -> bool {
    let mut values = headers.get_all(header::IF_RANGE).iter();
    let Some(if_range) = values.next() else {
        return true;
    };
    if values.next().is_some() {
        return false;
    }
    // Without Last-Modified metadata only the strong entity-tag form is
    // supported. Weak tags and HTTP dates deliberately fall back to a complete
    // 200 response.
    let (Ok(if_range), Some(etag)) = (if_range.to_str(), etag) else {
        return false;
    };
    !if_range.starts_with("W/") && etag.to_str().is_ok_and(|etag| if_range == etag)
}

fn parse_download_range(
    headers: &axum::http::HeaderMap,
    size: u64,
) -> Result<Option<DownloadRange>, ()> {
    let mut values = headers.get_all(header::RANGE).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    parse_single_byte_range(value.to_str().map_err(|_| ())?, size).map(Some)
}

fn parse_single_byte_range(value: &str, size: u64) -> Result<DownloadRange, ()> {
    if size == 0 {
        return Err(());
    }
    let (unit, spec) = value.trim().split_once('=').ok_or(())?;
    if !unit.eq_ignore_ascii_case("bytes") || spec.is_empty() || spec.contains(',') {
        return Err(());
    }
    if let Some(suffix) = spec.strip_prefix('-') {
        let suffix = suffix.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        let length = suffix.min(size);
        return Ok(DownloadRange {
            start: size - length,
            end: size - 1,
        });
    }

    let (start, end) = spec.split_once('-').ok_or(())?;
    if start.is_empty() {
        return Err(());
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= size {
        return Err(());
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        let end = end.parse::<u64>().map_err(|_| ())?;
        if end < start {
            return Err(());
        }
        end.min(size - 1)
    };
    Ok(DownloadRange { start, end })
}

fn guess_file_kind(mime: &str) -> FileKind {
    if mime.starts_with("image/") {
        FileKind::Image
    } else if mime.starts_with("video/") {
        FileKind::Video
    } else if mime.starts_with("audio/") {
        FileKind::Audio
    } else if mime == "application/pdf" || mime.starts_with("text/") || mime.contains("document") {
        FileKind::Document
    } else {
        FileKind::Other
    }
}

#[cfg(test)]
mod blob_download_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aero_common::{Blob, ParticipantId};
    use aero_storage::{BlobStore, BlobStoreError};
    use async_trait::async_trait;

    use super::*;

    struct CountingStore {
        reads: AtomicUsize,
        bytes: Bytes,
    }

    #[async_trait]
    impl BlobStore for CountingStore {
        async fn put(&self, _id: BlobId, _bytes: Bytes) -> Result<String, BlobStoreError> {
            Ok("fake".into())
        }

        async fn get(&self, _id: BlobId) -> Result<Bytes, BlobStoreError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(self.bytes.clone())
        }

        async fn delete(&self, _id: BlobId) -> Result<(), BlobStoreError> {
            Ok(())
        }

        fn key_for(&self, _id: BlobId) -> String {
            "fake".into()
        }
    }

    fn meta(name: &str) -> Blob {
        Blob {
            id: BlobId::new(),
            owner_id: ParticipantId::new(),
            workspace_id: None,
            storage_region: None,
            kind: FileKind::Document,
            name: name.into(),
            mime: "text/plain".into(),
            size: 10,
            sha256: Some("ab".repeat(32)),
            storage_key: "fake".into(),
            created_at: time::OffsetDateTime::now_utc(),
            finalized_at: Some(time::OffsetDateTime::now_utc()),
        }
    }

    #[test]
    fn parses_supported_single_byte_range_forms() {
        assert_eq!(
            parse_single_byte_range("bytes=2-5", 10),
            Ok(DownloadRange { start: 2, end: 5 })
        );
        assert_eq!(
            parse_single_byte_range("bytes=7-", 10),
            Ok(DownloadRange { start: 7, end: 9 })
        );
        assert_eq!(
            parse_single_byte_range("bytes=-3", 10),
            Ok(DownloadRange { start: 7, end: 9 })
        );
        assert_eq!(
            parse_single_byte_range("bytes=-99", 10),
            Ok(DownloadRange { start: 0, end: 9 })
        );
        assert_eq!(
            parse_single_byte_range("bytes=8-99", 10),
            Ok(DownloadRange { start: 8, end: 9 })
        );
    }

    #[test]
    fn rejects_unsatisfiable_multirange_and_malformed_ranges() {
        for value in [
            "bytes=10-11",
            "bytes=5-4",
            "bytes=0-1,4-5",
            "bytes=-0",
            "bytes=",
            "items=0-1",
            "bytes=abc-2",
            "bytes=1-2-3",
        ] {
            assert_eq!(
                parse_single_byte_range(value, 10),
                Err(()),
                "{value} must be rejected"
            );
        }
        assert_eq!(parse_single_byte_range("bytes=0-0", 0), Err(()));

        let mut duplicated = axum::http::HeaderMap::new();
        duplicated.append(header::RANGE, HeaderValue::from_static("bytes=0-1"));
        duplicated.append(header::RANGE, HeaderValue::from_static("bytes=2-3"));
        assert_eq!(parse_download_range(&duplicated, 10), Err(()));
    }

    #[test]
    fn strong_etag_and_if_none_match_handle_lists_and_weak_validators() {
        let tag = strong_etag(Some(&"ab".repeat(32))).unwrap();
        assert_eq!(
            tag,
            HeaderValue::from_str(&format!("\"{}\"", "ab".repeat(32))).unwrap()
        );
        assert!(strong_etag(Some("not-a-sha256")).is_none());

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_str(&format!("\"other\", W/\"{}\"", "ab".repeat(32))).unwrap(),
        );
        assert!(if_none_match_matches(&headers, &tag));
    }

    #[test]
    fn content_disposition_removes_filename_header_injection() {
        let disposition = content_disposition(false, "report\"\r\nX-Evil: yes\\../x.txt");
        assert_eq!(
            disposition,
            "attachment; filename=\"report___X-Evil: yes_.._x.txt\""
        );
        assert!(!disposition.contains('\r'));
        assert!(!disposition.contains('\n'));
    }

    #[test]
    fn active_svg_content_is_never_rendered_inline() {
        assert!(!is_inline_safe_mime("image/svg+xml"));
        assert!(is_inline_safe_mime("image/png"));
        assert_eq!(
            content_disposition(is_inline_safe_mime("image/svg+xml"), "diagram.svg"),
            "attachment; filename=\"diagram.svg\""
        );
    }

    #[tokio::test]
    async fn matching_etag_returns_304_without_reading_blob_store() {
        let meta = meta("report.txt");
        let store = CountingStore {
            reads: AtomicUsize::new(0),
            bytes: Bytes::from_static(b"0123456789"),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            strong_etag(meta.sha256.as_deref()).unwrap(),
        );

        let response = authorized_blob_response(&store, meta.id, &meta, &headers, true)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(store.reads.load(Ordering::Relaxed), 0);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            BLOB_CACHE_CONTROL
        );
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(
            response.headers()[header::ETAG],
            strong_etag(meta.sha256.as_deref()).unwrap()
        );
    }

    #[tokio::test]
    async fn range_response_is_streamed_with_protocol_headers() {
        let meta = meta("report.txt");
        let store = CountingStore {
            reads: AtomicUsize::new(0),
            bytes: Bytes::from_static(b"0123456789"),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=-3"));
        headers.insert(
            header::IF_RANGE,
            strong_etag(meta.sha256.as_deref()).unwrap(),
        );

        let response = authorized_blob_response(&store, meta.id, &meta, &headers, true)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 7-9/10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "3");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            BLOB_CACHE_CONTROL
        );
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"report.txt\""
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"789");
        assert_eq!(store.reads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn complete_response_is_streamed_with_length_and_etag() {
        let meta = meta("report.txt");
        let store = CountingStore {
            reads: AtomicUsize::new(0),
            bytes: Bytes::from_static(b"0123456789"),
        };

        let response =
            authorized_blob_response(&store, meta.id, &meta, &axum::http::HeaderMap::new(), true)
                .await
                .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(
            response.headers()[header::ETAG],
            strong_etag(meta.sha256.as_deref()).unwrap()
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"0123456789");
        assert_eq!(store.reads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn mismatched_if_range_falls_back_to_complete_200() {
        let meta = meta("report.txt");
        let store = CountingStore {
            reads: AtomicUsize::new(0),
            bytes: Bytes::from_static(b"0123456789"),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=2-4"));
        headers.insert(
            header::IF_RANGE,
            HeaderValue::from_static("\"different-sha256\""),
        );

        let response = authorized_blob_response(&store, meta.id, &meta, &headers, true)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
        assert!(!response.headers().contains_key(header::CONTENT_RANGE));
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"0123456789");
    }

    #[tokio::test]
    async fn head_returns_entity_headers_without_reading_blob_store() {
        let meta = meta("report.txt");
        let store = CountingStore {
            reads: AtomicUsize::new(0),
            bytes: Bytes::from_static(b"0123456789"),
        };
        let mut headers = axum::http::HeaderMap::new();
        // Range is intentionally ignored for HEAD.
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=2-4"));

        let response = authorized_blob_response(&store, meta.id, &meta, &headers, false)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(store.reads.load(Ordering::Relaxed), 0);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn invalid_range_returns_416_without_reading_blob_store() {
        let meta = meta("report.txt");
        let store = CountingStore {
            reads: AtomicUsize::new(0),
            bytes: Bytes::from_static(b"0123456789"),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-1,4-5"));

        let response = authorized_blob_response(&store, meta.id, &meta, &headers, true)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert_eq!(store.reads.load(Ordering::Relaxed), 0);
    }
}
