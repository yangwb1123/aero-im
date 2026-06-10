//! `LiveService` — orchestrates live-stream interactivity (danmaku, gifts,
//! viewer presence, lifecycle) on top of the storage repos and the NATS bus.
//!
//! Mirrors [`ImService`](aero_im_core::ImService)'s shape but fans out on
//! `live.stream.{id}` to *stream watchers* (tracked in the Hub) rather than to
//! room members — a public stream can be watched by anyone.

use std::sync::Arc;

use aero_bus::EventBus;
use aero_common::{
    gift_by_id, Block, Error, GiftLeaderRow, ParticipantId, Result, StreamChatLine, StreamEvent,
    StreamGiftLine, StreamStatus,
};
use aero_im_core::{
    AllowAllModerator, KeywordModerator, LocalSeqProvider, ModerationVerdict, Moderator,
    SeqProvider,
};
use aero_storage::{LiveRepo, ParticipantRepo, StreamRepo};
use tracing::{instrument, warn};
use ulid::Ulid;

/// Byte cap on a single danmaku line.
const MAX_CHAT_BYTES: usize = 500;
/// Upper bound on a single gift send (a combo, not a single tap).
const MAX_GIFT_QTY: u32 = 9999;

#[derive(Clone)]
pub struct LiveService {
    streams: StreamRepo,
    live: LiveRepo,
    participants: ParticipantRepo,
    bus: Arc<dyn EventBus>,
    moderator: Arc<dyn Moderator>,
    /// Per-subject event-seq source for publish-time `"seq"` stamping (ROADMAP
    /// 第三版 方向一), mirroring `ImService`. Defaults to the process-local
    /// provider; `bin/aero-server.rs` wires the Redis-backed
    /// [`aero_storage::SeqStore`] via [`with_seq`](Self::with_seq). Per-stream
    /// monotonic; gaps are legal (dedup + relative order only).
    seq: Arc<dyn SeqProvider>,
}

impl LiveService {
    /// Build a service, selecting a moderator from `AERO_BLOCKED_WORDS` (same
    /// policy as [`ImService`]) so danmaku is screened with the chat rules.
    pub fn new(
        streams: StreamRepo,
        live: LiveRepo,
        participants: ParticipantRepo,
        bus: Arc<dyn EventBus>,
    ) -> Self {
        let moderator: Arc<dyn Moderator> = match KeywordModerator::from_env() {
            Some(m) => Arc::new(m),
            None => Arc::new(AllowAllModerator),
        };
        Self {
            streams,
            live,
            participants,
            bus,
            moderator,
            seq: Arc::new(LocalSeqProvider::new()),
        }
    }

    /// Inject a custom event-seq provider (cluster-correct Redis `INCR` in the
    /// server binary). Additive builder mirroring `ImService::with_seq`.
    #[must_use]
    pub fn with_seq(mut self, seq: Arc<dyn SeqProvider>) -> Self {
        self.seq = seq;
        self
    }

    /// NATS subject used for per-stream broadcast.
    #[must_use]
    pub fn live_subject(stream_id: Ulid) -> String {
        format!("live.stream.{stream_id}")
    }

    async fn publish(&self, event: &StreamEvent) {
        let subject = Self::live_subject(event.stream_id());
        // Publish-time seq stamp (ROADMAP 第三版 方向一): minted before the
        // bytes hit NATS so a redelivery carries the SAME seq (the client dedup
        // key). `None` degrades to an unstamped event — never blocks fan-out.
        let seq = self.seq.next_seq(&subject).await;
        match aero_bus::stamped_event_bytes(event, seq) {
            Ok(bytes) => {
                if let Err(err) = self.bus.publish(&subject, bytes.into()).await {
                    warn!(?err, %subject, "publish StreamEvent failed");
                }
            }
            Err(err) => warn!(?err, "serialize StreamEvent failed"),
        }
    }

    /// Announce a go-live transition on the stream's subject (used by the WHIP
    /// ingest path on the idle/ended→live edge). Funnels through
    /// [`publish`](Self::publish) so the event is seq-stamped like every other
    /// `StreamEvent`. Best-effort: failures are logged, never surfaced.
    pub async fn publish_go_live(&self, stream_id: Ulid) {
        self.publish(&StreamEvent::Status { stream_id, status: StreamStatus::Live }).await;
    }

    /// Ensure a stream exists and has not ended; returns its current status.
    async fn require_open(&self, stream_id: Ulid) -> Result<StreamStatus> {
        let stream = self
            .streams
            .get(stream_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("stream {stream_id}")))?;
        if matches!(stream.status, StreamStatus::Ended) {
            return Err(Error::Conflict("stream has ended".into()));
        }
        Ok(stream.status)
    }

    async fn sender_name(&self, who: ParticipantId) -> Result<String> {
        Ok(self
            .participants
            .get(who)
            .await?
            .map(|p| p.display_name)
            .unwrap_or_else(|| who.to_string()))
    }

    // ---------------------------------------------------------- danmaku

    /// Post a danmaku line: validate, moderate, persist, broadcast.
    #[instrument(skip(self, body), fields(%stream_id, %sender))]
    pub async fn post_chat(
        &self,
        sender: ParticipantId,
        stream_id: Ulid,
        body: String,
    ) -> Result<StreamChatLine> {
        let body = body.trim();
        if body.is_empty() {
            return Err(Error::Invalid("empty chat".into()));
        }
        if body.len() > MAX_CHAT_BYTES {
            return Err(Error::Invalid(format!("chat exceeds {MAX_CHAT_BYTES} bytes")));
        }
        self.require_open(stream_id).await?;
        if let ModerationVerdict::Block(reason) = self.moderator.check(&[Block::text(body)]) {
            return Err(Error::Invalid(reason));
        }
        let sender_name = self.sender_name(sender).await?;
        let (id, created_at) = self.live.insert_chat(stream_id, sender, body).await?;
        let line = StreamChatLine {
            id,
            stream_id,
            sender_id: sender,
            sender_name,
            body: body.to_owned(),
            created_at,
        };
        self.publish(&StreamEvent::Chat(line.clone())).await;
        Ok(line)
    }

    pub async fn recent_chat(&self, stream_id: Ulid, limit: i64) -> Result<Vec<StreamChatLine>> {
        Ok(self.live.recent_chat(stream_id, limit.clamp(1, 200)).await?)
    }

    /// Late-joiner catch-up: chat lines strictly newer than `since` (a forward
    /// cursor — the last id the client rendered), or the bounded recent tail when
    /// `since` is `None`. Lets a viewer who joins a fast-chat stream pull the lines
    /// they missed instead of being capped at the last [`Self::recent_chat`] tail.
    pub async fn recent_chat_since(
        &self,
        stream_id: Ulid,
        since: Option<Ulid>,
        limit: i64,
    ) -> Result<Vec<StreamChatLine>> {
        Ok(self.live.recent_chat_since(stream_id, since, limit.clamp(1, 200)).await?)
    }

    // ---------------------------------------------------------- gifts

    /// Send a gift: validate against the catalog + quantity bounds, persist,
    /// broadcast. The coin total is `qty * unit price`.
    #[instrument(skip(self), fields(%stream_id, %sender, gift_id))]
    pub async fn send_gift(
        &self,
        sender: ParticipantId,
        stream_id: Ulid,
        gift_id: &str,
        qty: u32,
    ) -> Result<StreamGiftLine> {
        let gift =
            gift_by_id(gift_id).ok_or_else(|| Error::Invalid(format!("unknown gift: {gift_id}")))?;
        if qty == 0 || qty > MAX_GIFT_QTY {
            return Err(Error::Invalid(format!("qty must be 1..={MAX_GIFT_QTY}")));
        }
        self.require_open(stream_id).await?;
        let coins = u64::from(gift.coins) * u64::from(qty);
        let sender_name = self.sender_name(sender).await?;
        let (id, created_at) = self.live.insert_gift(stream_id, sender, gift_id, qty, coins).await?;
        let line = StreamGiftLine {
            id,
            stream_id,
            sender_id: sender,
            sender_name,
            gift_id: gift.id,
            gift_name: gift.name,
            gift_icon: gift.icon,
            qty,
            coins,
            created_at,
        };
        self.publish(&StreamEvent::Gift(line.clone())).await;
        Ok(line)
    }

    pub async fn recent_gifts(&self, stream_id: Ulid, limit: i64) -> Result<Vec<StreamGiftLine>> {
        Ok(self.live.recent_gifts(stream_id, limit.clamp(1, 100)).await?)
    }

    /// Late-joiner catch-up for gifts: ledger entries strictly newer than `since`,
    /// or the bounded recent tail when `since` is `None`. Symmetric to
    /// [`Self::recent_chat_since`].
    pub async fn recent_gifts_since(
        &self,
        stream_id: Ulid,
        since: Option<Ulid>,
        limit: i64,
    ) -> Result<Vec<StreamGiftLine>> {
        Ok(self.live.recent_gifts_since(stream_id, since, limit.clamp(1, 100)).await?)
    }

    pub async fn leaderboard(&self, stream_id: Ulid, limit: i64) -> Result<Vec<GiftLeaderRow>> {
        Ok(self.live.leaderboard(stream_id, limit.clamp(1, 50)).await?)
    }

    // ------------------------------------------------- presence + lifecycle

    /// Broadcast an updated viewer count (best-effort, fire-and-forget).
    pub async fn publish_viewers(&self, stream_id: Ulid, count: u32) {
        self.publish(&StreamEvent::Viewers { stream_id, count }).await;
    }

    /// End a stream (owner only) and broadcast the lifecycle transition.
    #[instrument(skip(self), fields(%stream_id, %actor))]
    pub async fn end_stream(&self, actor: ParticipantId, stream_id: Ulid) -> Result<()> {
        let stream = self
            .streams
            .get(stream_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("stream {stream_id}")))?;
        if stream.owner_id != actor {
            return Err(Error::Forbidden("only the stream owner may end it".into()));
        }
        self.streams.mark_ended(stream_id).await?;
        self.publish(&StreamEvent::Status { stream_id, status: StreamStatus::Ended })
            .await;
        Ok(())
    }
}
