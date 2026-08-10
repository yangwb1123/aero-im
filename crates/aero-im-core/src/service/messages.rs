//! Message operations — send, edit, delete, moderate-delete.
//!
//! Extracted from `service.rs` as part of REFACTOR_PLAN.md Step 1.
//! Uses `pub(crate)` fields/methods on `ImService` — these are module-internal
//! and should NOT be considered a public API.

use crate::service::orig::{per_tenant_metrics_enabled, spam_content_hash, WORKSPACE_NONE};
use crate::{moderation_text, validate_blocks, ImService, ModerationVerdict};
use aero_common::{
    metrics::{self, names},
    recall_window_expired, Block, Error, Message, MessageId, ParticipantId, Result, RoomId,
    RoomKind, WorkspaceId,
};
use aero_storage::{message::NewMessage, MessageIdempotency};
use tracing::{instrument, warn, Instrument};

/// Pure recall (撤回) permission decision: the author, or a room owner/admin,
/// may recall a message. `room_role` is the actor's role in the message's room
/// (`None` = not a member — unreachable past the room-access guard, kept for a
/// total function). Kept pure so the full permission matrix is table-driven and
/// unit-testable without a database; the storage transaction re-checks the same
/// rule under row locks (this is never authority on its own).
pub(crate) fn recall_authorized(
    actor: ParticipantId,
    sender: ParticipantId,
    room_role: Option<&str>,
) -> std::result::Result<(), aero_common::Error> {
    if actor == sender || matches!(room_role, Some("owner" | "admin")) {
        Ok(())
    } else {
        Err(aero_common::Error::Forbidden(
            "only author or room admin may recall".into(),
        ))
    }
}

/// Default recall window (`AERO_RECALL_WINDOW_SECS` fallback): 24 hours.
pub(crate) const RECALL_WINDOW_DEFAULT_SECS: i64 = 86_400;

/// Parse an `AERO_RECALL_WINDOW_SECS`-style raw value: `0` → unlimited
/// (`time::Duration::ZERO`); unset / garbage / negative / overflow → the
/// 86400s default (repo `env_parse(...).unwrap_or(default)` convention — a
/// hot-path knob must not brick startup on a typo). Pure so it is
/// unit-testable without process-global env mutation (parallel-unsafe).
pub(crate) fn parse_recall_window(raw: Option<&str>) -> time::Duration {
    match raw.and_then(|v| v.trim().parse::<i64>().ok()) {
        Some(secs) if secs >= 0 => time::Duration::seconds(secs),
        _ => time::Duration::seconds(RECALL_WINDOW_DEFAULT_SECS),
    }
}

/// Thin env wrapper — read ONCE at [`ImService::new`](crate::ImService::new)
/// so tests inject exact windows via `with_recall_window` instead of mutating
/// the process-global env (parallel-unsafe).
pub(crate) fn recall_window_from_env() -> time::Duration {
    parse_recall_window(std::env::var("AERO_RECALL_WINDOW_SECS").ok().as_deref())
}

#[derive(Debug, Clone)]
pub struct SendMessageOutcome {
    pub message: Message,
    pub deduplicated: bool,
}

impl ImService {
    /// Apply the full synchronous message policy before an external producer
    /// enters its own transaction-owned insert path. Commit-time room, post
    /// policy, information-barrier and attachment checks are still repeated by
    /// storage, so this preflight cannot become stale authority.
    pub async fn assert_external_message_send_preflight(
        &self,
        sender: ParticipantId,
        room: RoomId,
        blocks: &[Block],
    ) -> Result<()> {
        self.assert_room_access(sender, room).await?;
        self.assert_can_post(sender, room).await?;
        validate_blocks(blocks)?;
        if let ModerationVerdict::Block(reason) = self.moderator.check(blocks) {
            return Err(Error::Invalid(reason));
        }
        if let Some(guard) = self.spam_guard.as_ref() {
            let content_hash = spam_content_hash(blocks);
            if let crate::SpamDecision::Throttle(reason) = guard
                .record(sender, room, content_hash, std::time::Instant::now())
                .await
            {
                tracing::warn!(%sender, %room, ?reason, "external message spam guard throttled");
                return Err(Error::RateLimited);
            }
        }
        self.enforce_pii_and_auto_mod(sender, room, blocks).await
    }

    /// Send a message into a room: access → moderator → spam → PII → persist →
    /// notify → AI embed/moderate → metrics.
    #[instrument(skip(self, blocks), fields(?sender, ?room))]
    pub async fn send_message(
        &self,
        sender: ParticipantId,
        room: RoomId,
        blocks: Vec<Block>,
        reply_to: Option<MessageId>,
        expires_at: Option<time::OffsetDateTime>,
    ) -> Result<Message> {
        Ok(self
            .send_message_inner(sender, room, blocks, reply_to, expires_at, None)
            .await?
            .message)
    }

    /// Sender-confirmed message send using a stable client UUID. A retry with
    /// the same sender/key/payload returns the canonical message without
    /// publishing or dispatching any side effect again.
    // Internal service method with a fixed signature; grouping params into a
    // struct would churn the callers for no behavioral gain.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message_idempotent(
        &self,
        sender: ParticipantId,
        room: RoomId,
        blocks: Vec<Block>,
        reply_to: Option<MessageId>,
        expires_at: Option<time::OffsetDateTime>,
        client_message_id: uuid::Uuid,
        request_hash: [u8; 32],
    ) -> Result<SendMessageOutcome> {
        self.send_message_inner(
            sender,
            room,
            blocks,
            reply_to,
            expires_at,
            Some((client_message_id, request_hash)),
        )
        .await
    }

    async fn send_message_inner(
        &self,
        sender: ParticipantId,
        room: RoomId,
        blocks: Vec<Block>,
        reply_to: Option<MessageId>,
        expires_at: Option<time::OffsetDateTime>,
        idempotency: Option<(uuid::Uuid, [u8; 32])>,
    ) -> Result<SendMessageOutcome> {
        let started = std::time::Instant::now();
        // Keep this before the idempotency lookup: a revoked/deactivated account
        // must not recover the canonical message through a replay key.
        self.assert_room_access(sender, room).await?;
        if let Some((client_message_id, request_hash)) = idempotency {
            if let Some(message) = self
                .messages
                .find_by_client_message_id(sender, client_message_id, &request_hash)
                .await?
            {
                // The canonical message may have committed immediately before a
                // process crash prevented its fast publish. Nudge its retained
                // outbox row now; failure is harmless because the background
                // relay owns the durable retry.
                let repo = aero_storage::EventOutboxRepo::new(self.messages.pool.clone());
                if let Ok(Some(row)) = repo.pending_for_message(message.id).await {
                    if let Err(error) = self.dispatch_event_outbox_id(row.id).await {
                        warn!(?error, message_id = %message.id, "fast outbox retry failed");
                    }
                }
                self.kick_message_side_effects(message.id).await;
                return Ok(SendMessageOutcome {
                    message,
                    deduplicated: true,
                });
            }
        }
        if let Some(parent) = reply_to {
            let parent_matches_room = self
                .messages
                .reply_parent_exists_in_room(parent, room)
                .await?;
            validate_reply_parent_scope(parent, room, parent_matches_room)?;
        }
        self.assert_can_post(sender, room).await?;
        validate_blocks(&blocks)?;
        if let ModerationVerdict::Block(reason) = self.moderator.check(&blocks) {
            return Err(Error::Invalid(reason));
        }

        // Behavioral spam/flood guard
        if let Some(guard) = self.spam_guard.as_ref() {
            let content_hash = spam_content_hash(&blocks);
            if let crate::SpamDecision::Throttle(reason) = guard
                .record(sender, room, content_hash, std::time::Instant::now())
                .await
            {
                tracing::warn!(%sender, %room, ?reason, "spam guard throttled");
                return Err(Error::RateLimited);
            }
        }

        self.enforce_pii_and_auto_mod(sender, room, &blocks).await?;

        let new = NewMessage {
            room_id: room,
            sender_id: sender,
            blocks,
            reply_to,
            metadata: serde_json::Value::Null,
            expires_at,
        };
        // A committed message must always have a committed event. Fan-out and
        // notification workers resolve current membership at delivery time, so
        // a transient member-list read cannot block this durable write.
        let traceparent = aero_common::telemetry::current_traceparent();
        let inserted = self
            .messages
            .insert_outboxed(
                new,
                idempotency.map(|(client_message_id, request_hash)| {
                    MessageIdempotency::new(client_message_id, request_hash)
                }),
                Vec::new(),
                traceparent.as_deref(),
            )
            .await?;
        let deduplicated = inserted.deduplicated();
        let outbox_id = inserted.outbox_id;
        let message = inserted.into_message();
        // Fast path: make the event visible before returning when NATS is healthy.
        // Crucially, a publish error no longer turns a committed send into an
        // ambiguous failure; the durable relay will retry the same event id/seq.
        if let Err(error) = self.dispatch_event_outbox_id(outbox_id).await {
            warn!(?error, %outbox_id, message_id = %message.id, "fast outbox dispatch failed");
        }
        self.kick_message_side_effects(message.id).await;
        if deduplicated {
            return Ok(SendMessageOutcome {
                message,
                deduplicated: true,
            });
        }

        // Metrics
        let room_type = match self.rooms.room_kind(room).await {
            Ok(Some(RoomKind::Direct)) => "direct",
            Ok(Some(RoomKind::Group)) => "group",
            Ok(Some(RoomKind::Channel)) => "channel",
            _ => "unknown",
        };
        metrics::inc_counter_labeled(names::MESSAGES_SENT_TOTAL, 1, &[("room_type", room_type)]);
        if per_tenant_metrics_enabled() {
            let ws_label = self
                .rooms
                .room_workspace(room)
                .await
                .ok()
                .flatten()
                .map_or_else(|| WORKSPACE_NONE.to_string(), |w| w.to_string());
            metrics::inc_counter_labeled(
                names::MESSAGES_SENT_TOTAL,
                1,
                &[("workspace", ws_label.as_str())],
            );
        }
        metrics::observe_histogram_labeled(
            names::MESSAGE_PROCESSING_DURATION_SECONDS,
            started.elapsed().as_secs_f64(),
            &[("op", "send")],
        );
        Ok(SendMessageOutcome {
            message,
            deduplicated: false,
        })
    }

    /// Apply the configured synchronous governance policies that need tenant
    /// context. Send and edit both call this before their durable write so an
    /// edit cannot turn previously clean content into PII or an auto-mod match.
    async fn enforce_pii_and_auto_mod(
        &self,
        actor: ParticipantId,
        room: RoomId,
        blocks: &[Block],
    ) -> Result<()> {
        let text = moderation_text(blocks);
        if let Some(detector) = self.pii_detector.as_ref() {
            let kinds = detector.scan(&text);
            if !kinds.is_empty() {
                let tags = kinds.iter().map(|k| k.tag()).collect::<Vec<_>>().join(", ");
                tracing::warn!(%actor, %room, pii = %tags, "PII guard blocked");
                return Err(Error::Invalid(format!(
                    "message blocked: it appears to contain sensitive personal information ({tags})"
                )));
            }
        }

        if let Some(rule_repo) = self.auto_mod_rules.as_ref() {
            if let Some(workspace) = self.rooms.room_workspace(room).await? {
                // Rule lookup is part of the configured governance decision.
                // Fail closed on storage errors so a policy cannot disappear
                // silently while the subsequent message write still succeeds.
                let rules = rule_repo.list_for_enforcement(workspace).await?;
                if !rules.is_empty() {
                    let lowercase_text = text.to_lowercase();
                    if rules
                        .iter()
                        .any(|rule| rule.matches_lowercase(&lowercase_text))
                    {
                        return Err(Error::Invalid("blocked by auto-mod rule".into()));
                    }
                }
            }
        }
        Ok(())
    }

    /// Edit a message. Only the sender may edit; soft-deleted messages refuse.
    ///
    /// `expected_version` is the optimistic-lock check (migration 0157): pass
    /// the version the client last saw a message at (from a prior `GET`/list/
    /// search response's `Message.version`) and a concurrent edit that already
    /// bumped the version fails with `Error::Conflict` (409) instead of
    /// silently overwriting it. `None` (a caller that hasn't been updated to
    /// send one, or doesn't have a prior read to base it on) falls back to the
    /// pre-migration behavior: the current version is read and used as-is,
    /// which only protects against edits racing within the same instant and
    /// does not detect a genuine read-then-overwrite race.
    #[instrument(skip(self, blocks), fields(?actor, ?id))]
    pub async fn edit_message(
        &self,
        actor: ParticipantId,
        id: MessageId,
        blocks: Vec<Block>,
        expected_version: Option<i32>,
    ) -> Result<Message> {
        let started = std::time::Instant::now();
        let existing = self.editable_message(actor, id).await?;
        validate_blocks(&blocks)?;
        if let ModerationVerdict::Block(reason) = self.moderator.check(&blocks) {
            return Err(Error::Invalid(reason));
        }
        self.enforce_pii_and_auto_mod(actor, existing.room_id, &blocks)
            .await?;

        // Optimistic-lock version (migration 0157): use the client-supplied
        // expectation when present (the real lost-update guard); otherwise fall
        // back to reading the current version, matching pre-versioning behavior.
        let version =
            match expected_version {
                Some(v) => v,
                None => self.messages.get_version(id).await?.ok_or_else(|| {
                    Error::NotFound(format!("message {id} disappeared before edit"))
                })?,
            };

        let traceparent = aero_common::telemetry::current_traceparent();
        let edited = self
            .messages
            .edit_outboxed_authorized(
                id,
                actor,
                blocks,
                version,
                self.message_edits.is_some(),
                traceparent.as_deref(),
            )
            .await?
            .ok_or_else(|| Error::Conflict("version mismatch or edit raced with delete".into()))?;
        let updated = edited.message;
        if let Err(error) = self.dispatch_event_outbox_id(edited.outbox_id).await {
            warn!(
                ?error,
                outbox_id = %edited.outbox_id,
                message_id = %id,
                "fast edited-event outbox dispatch failed"
            );
        }
        self.kick_message_side_effects(updated.id).await;
        metrics::inc_counter(names::MESSAGES_EDITED_TOTAL, 1);
        metrics::observe_histogram_labeled(
            names::MESSAGE_PROCESSING_DURATION_SECONDS,
            started.elapsed().as_secs_f64(),
            &[("op", "edit")],
        );
        Ok(updated)
    }

    /// Resolve and authorize an edit before an edge spends tenant rate/slow-mode
    /// capacity. The eventual [`Self::edit_message`] call repeats these checks so
    /// a concurrent membership, account, or announcement-policy change cannot
    /// turn this preflight result into authority.
    pub async fn assert_message_edit_preflight(
        &self,
        actor: ParticipantId,
        id: MessageId,
    ) -> Result<RoomId> {
        Ok(self.editable_message(actor, id).await?.room_id)
    }

    async fn editable_message(&self, actor: ParticipantId, id: MessageId) -> Result<Message> {
        let existing = self
            .messages
            .get(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("message {id}")))?;
        // Resolve the message's tenant/room first, then apply the shared access
        // guard before author checks. This prevents a global message id from
        // becoming an IDOR/oracle across workspaces.
        self.assert_room_access(actor, existing.room_id).await?;
        if existing.deleted_at.is_some() {
            return Err(Error::Conflict("message is deleted".into()));
        }
        // A recalled message is terminal for user content mutations: its body is
        // the system placeholder and must not be overwritten back into view.
        if existing.recalled_at.is_some() {
            return Err(Error::Conflict("message is recalled".into()));
        }
        if existing.sender_id != actor {
            return Err(Error::Forbidden("only sender may edit".into()));
        }
        // Editing mutates the channel's current visible content, so it must
        // respect the same announcement-channel policy as a fresh post. Without
        // this recheck, a member could publish arbitrary new text by editing a
        // message created before the room became admins-only.
        self.assert_can_post(actor, existing.room_id).await?;
        Ok(existing)
    }

    /// Soft-delete a message. Only the sender may delete.
    #[instrument(skip(self), fields(?actor, ?id))]
    pub async fn delete_message(&self, actor: ParticipantId, id: MessageId) -> Result<()> {
        let started = std::time::Instant::now();
        let existing = self
            .messages
            .get(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("message {id}")))?;
        self.assert_room_access(actor, existing.room_id).await?;
        if existing.deleted_at.is_some() {
            return Ok(());
        }
        if existing.sender_id != actor {
            return Err(Error::Forbidden("only sender may delete in P2".into()));
        }
        let traceparent = aero_common::telemetry::current_traceparent();
        let deleted = self
            .messages
            .soft_delete_outboxed_authorized(id, actor, traceparent.as_deref())
            .await?;
        if let Some(deleted) = deleted {
            if let Err(error) = self.dispatch_event_outbox_id(deleted.outbox_id).await {
                warn!(
                    ?error,
                    outbox_id = %deleted.outbox_id,
                    message_id = %id,
                    "fast deleted-event outbox dispatch failed"
                );
            }
        }
        metrics::inc_counter(names::MESSAGES_DELETED_TOTAL, 1);
        metrics::observe_histogram_labeled(
            names::MESSAGE_PROCESSING_DURATION_SECONDS,
            started.elapsed().as_secs_f64(),
            &[("op", "delete")],
        );
        Ok(())
    }

    /// Resolve and authorize a recall before an edge spends tenant rate
    /// capacity. Mirrors [`Self::assert_message_edit_preflight`]: resolves the
    /// message's room through the shared access guard so the rate gate can
    /// charge the right workspace (and non-members can never drain a victim's
    /// budget by spamming message ids), and carries the recall role gate
    /// (author or room owner/admin — edit's preflight carries its sender gate
    /// the same way). The role check MUST run here, before the edge charges
    /// the workspace budget: a doomed recall attempt by a plain member fails
    /// with `Forbidden` from this preflight and never consumes shared rate
    /// capacity (gate S1 — the rate gate itself must not be a workspace-wide
    /// `DoS` amplifier). The author recall-window check runs here for the same
    /// reason: a window-expired attempt is doomed and must not burn the budget
    /// either. The eventual [`Self::recall_message`] call repeats every check so
    /// a concurrent membership, account, or state change cannot turn this
    /// preflight result into authority.
    pub async fn assert_message_recall_preflight(
        &self,
        actor: ParticipantId,
        id: MessageId,
    ) -> Result<RoomId> {
        let existing = self
            .messages
            .get(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("message {id}")))?;
        // Resolve the message's tenant/room first, then apply the shared access
        // guard before any state check. This prevents a global message id from
        // becoming an IDOR/oracle across workspaces.
        self.assert_room_access(actor, existing.room_id).await?;
        if existing.deleted_at.is_some() {
            return Err(Error::Conflict("message is deleted".into()));
        }
        if existing.recalled_at.is_some() {
            return Err(Error::Conflict("message is already recalled".into()));
        }
        // Author, or room owner/admin — BEFORE any edge charges rate capacity.
        // Same stable error `recall_message` returns (the commit-time storage
        // path re-checks this rule under row locks; this read only produces the
        // early 403 without spending budget).
        let role = aero_storage::RoomRoleRepo::new(self.messages.pool.clone())
            .role_of(existing.room_id, actor)
            .await?;
        recall_authorized(actor, existing.sender_id, role.as_deref())?;
        // Recall window (撤回时间窗): author-only — room owner/admin recall is
        // the moderation path and is exempt. Runs AFTER the role gate so the
        // window state is never revealed to non-privileged actors, and before
        // the rate charge (gate S1). The storage transaction re-checks this
        // against the row-locked snapshot; this read only produces the early
        // 409. Emits the rejection counter here — the single choke point shared
        // by REST and WS (the tx-fence boundary-race fraction is not counted).
        if existing.sender_id == actor
            && recall_window_expired(
                existing.created_at,
                aero_common::time::now_utc(),
                self.recall_window,
            )
        {
            metrics::inc_counter(names::MESSAGES_RECALL_EXPIRED_TOTAL, 1);
            return Err(Error::Conflict("recall window expired".into()));
        }
        Ok(existing.room_id)
    }

    /// Recall (撤回) a message: the sender — or a room owner/admin — replaces
    /// its content with the system placeholder while the row, room history and
    /// audit trail stay intact, then broadcasts a `Recalled` room event so every
    /// client renders the placeholder.
    ///
    /// Stable failure paths, in evaluation order (a caller outside the room must
    /// never learn anything about the message's state — no existence oracle):
    /// 1. unknown message → `NotFound`;
    /// 2. no room access (non-member / cross-workspace) → `Forbidden`;
    /// 3. already deleted → `Conflict("message is deleted")`;
    /// 4. already recalled → `Conflict("message is already recalled")`;
    /// 5. member (not author, not admin/owner) → `Forbidden`;
    /// 6. author outside the recall window (`AERO_RECALL_WINDOW_SECS`) →
    ///    `Conflict("recall window expired")` — room owner/admin recall
    ///    (moderation path) is exempt.
    /// The commit-time storage path re-checks access, role, state and the
    /// window under row locks, so this preflight is UX only, never authority.
    #[instrument(skip(self), fields(?actor, ?id))]
    pub async fn recall_message(&self, actor: ParticipantId, id: MessageId) -> Result<Message> {
        let started = std::time::Instant::now();
        let existing = self
            .messages
            .get(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("message {id}")))?;
        // Resolve the message's tenant/room first, then apply the shared access
        // guard before any state check. This prevents a global message id from
        // becoming an IDOR/oracle across workspaces.
        self.assert_room_access(actor, existing.room_id).await?;
        if existing.deleted_at.is_some() {
            return Err(Error::Conflict("message is deleted".into()));
        }
        if existing.recalled_at.is_some() {
            return Err(Error::Conflict("message is already recalled".into()));
        }
        // Author, or room owner/admin. The role is re-checked inside the storage
        // transaction under locks; this read only produces the stable early 403.
        let role = aero_storage::RoomRoleRepo::new(self.messages.pool.clone())
            .role_of(existing.room_id, actor)
            .await?;
        recall_authorized(actor, existing.sender_id, role.as_deref())?;
        // Author-only recall window — same rule as the preflight; room
        // owner/admin recall is exempt. Re-checked inside the storage
        // transaction under the row lock (authority); this read only produces
        // the stable early 409 for direct callers.
        if existing.sender_id == actor
            && recall_window_expired(
                existing.created_at,
                aero_common::time::now_utc(),
                self.recall_window,
            )
        {
            return Err(Error::Conflict("recall window expired".into()));
        }

        let traceparent = aero_common::telemetry::current_traceparent();
        let recalled = self
            .messages
            .recall_outboxed_authorized(id, actor, self.recall_window, traceparent.as_deref())
            .await?
            .ok_or_else(|| Error::Conflict("message recall raced with another mutation".into()))?;
        if let Err(error) = self.dispatch_event_outbox_id(recalled.outbox_id).await {
            warn!(
                ?error,
                outbox_id = %recalled.outbox_id,
                message_id = %id,
                "fast recalled-event outbox dispatch failed"
            );
        }
        metrics::inc_counter(names::MESSAGES_RECALLED_TOTAL, 1);
        metrics::observe_histogram_labeled(
            names::MESSAGE_PROCESSING_DURATION_SECONDS,
            started.elapsed().as_secs_f64(),
            &[("op", "recall")],
        );
        Ok(recalled.message)
    }

    /// System action: soft-delete a message flagged by AI moderation.
    #[instrument(skip(self), fields(?message_id, reason))]
    pub async fn moderate_delete(
        &self,
        message_id: MessageId,
        workspace: Option<WorkspaceId>,
        reason: &str,
        digest: &str,
    ) -> Result<()> {
        // R-D1 (B5-1, parity with the aero-ai worker's
        // `moderation_delete_workspace`): refuse `None` — the audit action is
        // derived from the workspace, so deleting with `None` would commit the
        // soft delete + `Deleted` broadcast with ZERO `audit_events` rows (the
        // 0239 governance enqueue never fires) — an invisible, un-audited
        // removal and the only route around the otherwise fail-closed binding
        // RAISE. The caller keeps the message visible instead.
        let workspace = workspace.ok_or_else(|| {
            Error::Invalid(
                "moderate_delete requires a workspace; refusing un-audited delete (R-D1)"
                    .to_owned(),
            )
        })?;
        let existing = self
            .messages
            .get(message_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("message {message_id}")))?;
        if existing.deleted_at.is_some() {
            return Ok(());
        }
        let detail = serde_json::json!({
            "room_id": existing.room_id, "reason": reason, "digest": digest,
        });
        let traceparent = aero_common::telemetry::current_traceparent();
        let deleted = self
            .messages
            .soft_delete_outboxed_system(
                message_id,
                Some(workspace),
                None,
                Some("message.moderated"),
                detail,
                ParticipantId::nil(),
                traceparent.as_deref(),
            )
            .await?;
        warn!(%message_id, reason, "message removed by AI moderation");
        if let Some(deleted) = deleted {
            if let Err(error) = self.dispatch_event_outbox_id(deleted.outbox_id).await {
                warn!(
                    ?error,
                    outbox_id = %deleted.outbox_id,
                    %message_id,
                    "fast moderation-event outbox dispatch failed"
                );
            }
        }
        Ok(())
    }

    async fn kick_message_side_effects(&self, message_id: MessageId) {
        let service = self.clone();
        let dispatch = tokio::spawn(
            async move {
                if let Err(error) = service.dispatch_message_side_effects_for(message_id).await {
                    warn!(?error, %message_id, "fast message side-effect dispatch failed");
                }
            }
            .in_current_span(),
        );
        #[cfg(test)]
        if let Err(error) = dispatch.await {
            warn!(?error, %message_id, "message side-effect dispatcher panicked");
        }
        #[cfg(not(test))]
        drop(dispatch);
    }
}

fn validate_reply_parent_scope(
    parent: MessageId,
    room: RoomId,
    parent_matches_room: bool,
) -> Result<()> {
    if parent_matches_room {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "reply_to {parent} does not reference a message in room {room}"
        )))
    }
}

#[cfg(test)]
mod reply_scope_tests {
    use super::*;

    #[test]
    fn reply_parent_scope_fails_closed() {
        let parent = MessageId::new();
        let room = RoomId::new();
        assert!(validate_reply_parent_scope(parent, room, true).is_ok());
        assert!(matches!(
            validate_reply_parent_scope(parent, room, false),
            Err(Error::Invalid(message))
                if message.contains(&parent.to_string()) && message.contains(&room.to_string())
        ));
    }
}

#[cfg(test)]
mod recall_permission_tests {
    use super::*;

    /// The full recall permission matrix, table-driven: actor kind × room role
    /// → allow / stable 403. Author always allowed; owner/admin allowed; plain
    /// member (non-author) forbidden; stranger (None role) forbidden. State
    /// transitions (already-recalled / deleted) are rejected in
    /// [`ImService::recall_message`] before this is consulted, so the pure
    /// function only decides the permission axis.
    #[test]
    fn recall_permission_matrix() {
        let author = ParticipantId::new();
        let admin = ParticipantId::new();
        let owner = ParticipantId::new();
        let member = ParticipantId::new();
        let stranger = ParticipantId::new();

        let cases: Vec<(&str, ParticipantId, Option<&str>, bool)> = vec![
            ("author", author, None, true), // author needs no role
            ("author-as-member", author, Some("member"), true),
            ("admin", admin, Some("admin"), true),
            ("owner", owner, Some("owner"), true),
            ("member-non-author", member, Some("member"), false),
            ("stranger", stranger, None, false), // no membership edge
        ];
        for (label, actor, role, allowed) in cases {
            match recall_authorized(actor, author, role) {
                Ok(()) => assert!(allowed, "{label}: expected Forbidden"),
                Err(Error::Forbidden(msg)) => {
                    assert!(!allowed, "{label}: expected allow");
                    assert_eq!(msg, "only author or room admin may recall");
                }
                Err(other) => panic!("{label}: unexpected error {other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod recall_window_tests {
    use super::*;

    #[test]
    fn parse_recall_window_defaults_and_unlimited() {
        // Unset → 86400s default.
        assert_eq!(parse_recall_window(None), time::Duration::seconds(86_400));
        // 0 = unlimited.
        assert_eq!(parse_recall_window(Some("0")), time::Duration::ZERO);
        // Valid values, trimmed.
        assert_eq!(parse_recall_window(Some("60")), time::Duration::seconds(60));
        assert_eq!(
            parse_recall_window(Some(" 60 ")),
            time::Duration::seconds(60)
        );
        // Garbage / negative / empty / float / overflow → default fallback.
        for raw in ["abc", "-5", "1.5", "", "99999999999999999999999"] {
            assert_eq!(
                parse_recall_window(Some(raw)),
                time::Duration::seconds(86_400),
                "raw={raw:?} must fall back to the default"
            );
        }
    }

    #[test]
    fn recall_window_boundary_is_inclusive_and_zero_is_unlimited() {
        let window = time::Duration::seconds(86_400);
        // Single captured `now` for both sides: t = window exactly → allowed
        // (inclusive boundary).
        let now = time::OffsetDateTime::now_utc();
        assert!(!recall_window_expired(now - window, now, window));
        // t = window + 1s → expired.
        assert!(recall_window_expired(
            now - window - time::Duration::seconds(1),
            now,
            window
        ));
        // window ZERO = unlimited at any age.
        assert!(!recall_window_expired(
            now - time::Duration::days(365),
            now,
            time::Duration::ZERO
        ));
        // Future created_at (clock skew) → not expired.
        assert!(!recall_window_expired(
            now + time::Duration::seconds(60),
            now,
            window
        ));
        // Just sent → not expired.
        assert!(!recall_window_expired(now, now, window));
    }
}
