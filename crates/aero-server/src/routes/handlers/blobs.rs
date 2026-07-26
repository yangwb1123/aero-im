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
    ALLOWED_MIME_PREFIXES.iter().any(|p| mime.starts_with(p))
        || ALLOWED_MIME_EXACT.contains(&mime)
}

async fn blob_upload(
    State(s): State<AppState>,
    auth: AuthUser,
    mut mp: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    // Tenant fairness (ROADMAP3 方向五): blobs are owner-scoped (no room in the
    // URL), so the charge resolves through the uploader's workspace membership.
    // Checked before the multipart body is read, shedding the bytes early.
    crate::ws_rate::check_ws_rate_participant(&s, auth.participant_id).await?;
    while let Some(field) = mp.next_field().await.map_err(|e| AeroError::Invalid(e.to_string()))? {
        if field.name() != Some("file") {
            continue;
        }
        let name = field.file_name().unwrap_or("untitled").to_owned();
        let mime = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_owned();
        let bytes = field.bytes().await.map_err(|e| AeroError::Invalid(e.to_string()))?;
        if bytes.len() > MAX_BLOB_BYTES {
            return Err(AeroError::Invalid(format!("blob too large: {} bytes", bytes.len())).into());
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
                        participant = %auth.participant_id,
                        signature = %sig,
                        "blob upload rejected: malware detected by clamd",
                    );
                    return Err(AeroError::Invalid(format!(
                        "file rejected by virus scan ({sig})"
                    ))
                    .into());
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
            .find_by_owner_sha256(auth.participant_id, &sha256_hex)
            .await
            .map_err(AeroError::from)?
        {
            return Ok(Json(serde_json::json!({
                "id": existing.id,
                "name": existing.name,
                "mime": existing.mime,
                "size": existing.size,
                "kind": existing.kind,
            })));
        }

        let blob = s
            .blobs
            .create(NewBlob {
                owner_id: auth.participant_id,
                kind,
                name: name.clone(),
                mime: mime.clone(),
                size,
                sha256: Some(sha256_hex),
                storage_key: format!("pending:{}", uuid::Uuid::new_v4()),
            })
            .await
            .map_err(AeroError::from)?;
        let key = s
            .blob_store
            .put(blob.id, bytes)
            .await
            .map_err(|e| AeroError::Internal(anyhow::anyhow!("blob put: {e}")))?;
        let _ = key; // The store knows the key from the blob id.
        return Ok(Json(serde_json::json!({
            "id": blob.id,
            "name": blob.name,
            "mime": blob.mime,
            "size": blob.size,
            "kind": blob.kind,
        })));
    }
    Err(AeroError::Invalid("multipart missing 'file' field".into()).into())
}

async fn blob_download(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<axum::response::Response> {
    let id = BlobId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("blob id: {e}")))?;
    // Tenant fairness (ROADMAP3 方向五): charge the download (a PG meta read +
    // a full blob-store read) against the caller's workspace budget up front.
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
    let bytes: Bytes = s
        .blob_store
        .get(id)
        .await
        .map_err(|e| AeroError::Internal(anyhow::anyhow!("blob get: {e}")))?;
    let mut resp = bytes.into_response();
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_str(&meta.mime).unwrap_or_else(|_| {
            header::HeaderValue::from_static("application/octet-stream")
        }),
    );
    // Anti-XSS: force attachment disposition for non-visual content types.
    // Images, video, audio, and PDF may render inline (expected UX); everything
    // else (HTML, SVG, Office docs, executables) is forced to download so a
    // malicious blob cannot execute scripts in the browser (ROADMAP 方向三).
    // The nosniff header prevents MIME-type confusion attacks regardless.
    let inline_safe = meta.mime.starts_with("image/")
        || meta.mime.starts_with("video/")
        || meta.mime.starts_with("audio/")
        || meta.mime == "application/pdf";
    let disposition = if inline_safe {
        format!("inline; filename=\"{}\"", meta.name)
    } else {
        format!("attachment; filename=\"{}\"", meta.name)
    };
    headers.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&disposition)
            .unwrap_or_else(|_| {
                if inline_safe {
                    header::HeaderValue::from_static("inline")
                } else {
                    header::HeaderValue::from_static("attachment")
                }
            }),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    Ok(resp)
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

