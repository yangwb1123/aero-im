//! `LiveService` — orchestrates live-stream interactivity (danmaku, gifts,
//! viewer presence, lifecycle) on top of the storage repos and the NATS bus.
//!
//! Mirrors [`ImService`](aero_im_core::ImService)'s shape but fans out on
//! `live.stream.{id}` to *stream watchers* (tracked in the Hub) rather than to
//! room members — a public stream can be watched by anyone.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};

use aero_bus::EventBus;
use aero_common::metrics::{self, names};
use aero_common::{
    gift_by_id, Block, Error, GiftLeaderRow, ParticipantId, Result, StreamChatLine, StreamEvent,
    StreamGiftLine, StreamProtocol, StreamStatus,
};
use aero_im_core::{
    AllowAllModerator, KeywordModerator, LocalSeqProvider, ModerationVerdict, Moderator,
    SeqProvider,
};
use aero_storage::{LiveRepo, ParticipantRepo, StreamRepo, SubscriptionRepo};
use aero_storage::{StreamGoLiveOutboxRepo, StreamGoLiveOutboxRow};
use anyhow::{anyhow, Context as _};
use tracing::{instrument, warn};
use ulid::Ulid;
use url::Url;

/// Byte cap on a single danmaku line.
const MAX_CHAT_BYTES: usize = 500;
/// Upper bound on a single gift send (a combo, not a single tap).
const MAX_GIFT_QTY: u32 = 9999;

/// Public ingest URLs rendered by `POST /api/streams`.
///
/// RTMP and SRT listen on their own media ports, independently from the HTTP
/// gateway used by WHIP. A wildcard listen IP is not a connectable destination,
/// so it is replaced with an explicitly configured media advertise host or the
/// host from the public HTTP origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveIngestUrls {
    rtmp: String,
    whip: String,
    srt: String,
}

impl LiveIngestUrls {
    /// Resolve public endpoints from the actual media listener addresses.
    ///
    /// `advertise_host` is the explicitly configured `AERO_INGEST_HOST`, when
    /// present. It only replaces wildcard listener IPs; a concrete listener IP
    /// remains the advertised address.
    #[must_use]
    pub fn new(
        public_base_url: &str,
        advertise_host: Option<&str>,
        rtmp_listen: SocketAddr,
        srt_listen: SocketAddr,
    ) -> Self {
        let public_host = public_host(public_base_url);
        let advertise_host = advertise_host.and_then(usable_host);
        let rtmp_authority = public_authority(
            rtmp_listen,
            advertise_host.as_deref(),
            public_host.as_deref(),
        );
        let srt_authority = public_authority(
            srt_listen,
            advertise_host.as_deref(),
            public_host.as_deref(),
        );

        Self {
            rtmp: format!("rtmp://{rtmp_authority}"),
            whip: public_base_url.trim_end_matches('/').to_owned(),
            srt: format!("srt://{srt_authority}"),
        }
    }

    /// Render the protocol-specific URL containing the stream's secret key.
    #[must_use]
    pub fn for_stream(&self, protocol: StreamProtocol, stream_key: &str) -> String {
        match protocol {
            StreamProtocol::Rtmp => format!("{}/live/{stream_key}", self.rtmp),
            StreamProtocol::Whip => format!("{}/whip/{stream_key}", self.whip),
            StreamProtocol::Srt => format!("{}?streamid={stream_key}", self.srt),
        }
    }
}

fn public_host(public_base_url: &str) -> Option<String> {
    Url::parse(public_base_url)
        .ok()
        .and_then(|url| url.host_str().and_then(usable_host))
}

fn usable_host(raw: &str) -> Option<String> {
    let host = raw.trim().trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return None;
    }
    if host
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_unspecified())
    {
        return None;
    }
    Some(host.to_owned())
}

fn public_authority(
    listen: SocketAddr,
    advertise_host: Option<&str>,
    public_host: Option<&str>,
) -> String {
    let host = if listen.ip().is_unspecified() {
        advertise_host.or(public_host).map_or_else(
            || match listen.ip() {
                IpAddr::V4(_) => Ipv4Addr::LOCALHOST.to_string(),
                IpAddr::V6(_) => Ipv6Addr::LOCALHOST.to_string(),
            },
            str::to_owned,
        )
    } else {
        listen.ip().to_string()
    };
    format_authority(&host, listen.port())
}

fn format_authority(host: &str, port: u16) -> String {
    if host.parse::<Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

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
        // Stamp seq + the current span's trace context (ROADMAP5 方向二) onto the
        // envelope — sibling keys serde ignores on typed decode — so a watcher's
        // fan-out trace continues the producer's across the NATS boundary.
        let traceparent = aero_common::telemetry::current_traceparent();
        let bytes = match serde_json::to_value(event) {
            Ok(mut value) => {
                aero_bus::stamp_seq(&mut value, seq);
                aero_bus::stamp_traceparent(&mut value, traceparent.as_deref());
                match serde_json::to_vec(&value) {
                    Ok(b) => b,
                    Err(err) => {
                        warn!(?err, "serialize StreamEvent failed");
                        return;
                    }
                }
            }
            Err(err) => {
                warn!(?err, "serialize StreamEvent failed");
                return;
            }
        };
        if let Err(err) = self.bus.publish(&subject, bytes.into()).await {
            warn!(?err, %subject, "publish StreamEvent failed");
        }
    }

    /// Publish one claimed transactional lifecycle row with stable wire bytes.
    ///
    /// Unlike interactive live events, errors are surfaced to the `PostgreSQL`
    /// relay so it can re-park the claim. The per-subject sequence is persisted
    /// before publication and therefore reused by ambiguous retries.
    pub(crate) async fn publish_outboxed_go_live(
        &self,
        repo: &StreamGoLiveOutboxRepo,
        row: &StreamGoLiveOutboxRow,
    ) -> anyhow::Result<()> {
        let seq = match row.seq {
            Some(seq) => Some(seq),
            None => match self.seq.next_seq(&row.subject).await {
                Some(candidate) => Some(
                    repo.assign_seq_if_absent(row.id, row.claim_token, candidate)
                        .await
                        .context("persist stream.live sequence")?
                        .ok_or_else(|| {
                            anyhow!("stream.live claim lost before sequence assignment")
                        })?,
                ),
                None => None,
            },
        };
        let bytes = crate::stream_live_outbox::bus_payload(row, seq)
            .context("serialize stream.live outbox payload")?;
        if let Err(error) = self
            .bus
            .publish_idempotent(&row.subject, bytes.into(), &row.event_id.to_string())
            .await
        {
            metrics::inc_counter(names::NATS_PUBLISH_ERRORS_TOTAL, 1);
            return Err(anyhow!(error).context("publish stream.live outbox to NATS"));
        }
        Ok(())
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
            .map_or_else(|| who.to_string(), |p| p.display_name))
    }

    // ---------------------------------------------------------- danmaku

    /// Post a danmaku line: validate, moderate, persist, broadcast.
    ///
    /// `is_subscriber` is resolved by the HTTP/WS layer (it has the
    /// [`SubscriptionRepo`](aero_storage::SubscriptionRepo) and the stream owner)
    /// and recorded on the line so clients render a subscriber badge (migration
    /// 0082). It is computed at the edge rather than here to keep [`LiveService`]'s
    /// dependency set unchanged.
    #[instrument(skip(self, body), fields(%stream_id, %sender))]
    pub async fn post_chat(
        &self,
        sender: ParticipantId,
        stream_id: Ulid,
        body: String,
        is_subscriber: bool,
    ) -> Result<StreamChatLine> {
        let body = body.trim();
        if body.is_empty() {
            return Err(Error::Invalid("empty chat".into()));
        }
        if body.len() > MAX_CHAT_BYTES {
            return Err(Error::Invalid(format!(
                "chat exceeds {MAX_CHAT_BYTES} bytes"
            )));
        }
        self.require_open(stream_id).await?;
        if let ModerationVerdict::Block(reason) = self.moderator.check(&[Block::text(body)]) {
            return Err(Error::Invalid(reason));
        }
        let sender_name = self.sender_name(sender).await?;
        let (id, created_at) = self
            .live
            .insert_chat(stream_id, sender, body, is_subscriber)
            .await?;
        let line = StreamChatLine {
            id,
            stream_id,
            sender_id: sender,
            sender_name,
            body: body.to_owned(),
            is_subscriber,
            created_at,
        };
        self.publish(&StreamEvent::Chat(line.clone())).await;
        Ok(line)
    }

    pub async fn recent_chat(&self, stream_id: Ulid, limit: i64) -> Result<Vec<StreamChatLine>> {
        Ok(self
            .live
            .recent_chat(stream_id, limit.clamp(1, 200))
            .await?)
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
        Ok(self
            .live
            .recent_chat_since(stream_id, since, limit.clamp(1, 200))
            .await?)
    }

    // ---------------------------------------------------------- gifts

    /// Send a gift: validate against the catalog + quantity bounds, persist,
    /// broadcast. The coin total is `qty * unit price`.
    #[instrument(skip(self), fields(%stream_id, %sender, gift_id))]
    /// Record a gift and (on a fresh send) broadcast + advance goal bars. The
    /// optional `idempotency_key` makes a retried send a no-op: it returns the
    /// original gift line and `false` so the caller skips its own side-effects
    /// (e.g. hype train). A fresh send returns `true`.
    pub async fn send_gift(
        &self,
        sender: ParticipantId,
        stream_id: Ulid,
        gift_id: &str,
        qty: u32,
        idempotency_key: Option<&str>,
    ) -> Result<(StreamGiftLine, bool)> {
        let gift = gift_by_id(gift_id)
            .ok_or_else(|| Error::Invalid(format!("unknown gift: {gift_id}")))?;
        if qty == 0 || qty > MAX_GIFT_QTY {
            return Err(Error::Invalid(format!("qty must be 1..={MAX_GIFT_QTY}")));
        }
        self.require_open(stream_id).await?;
        let coins = u64::from(gift.coins) * u64::from(qty);
        let sender_name = self.sender_name(sender).await?;
        let (id, created_at, inserted) = self
            .live
            .insert_gift(stream_id, sender, gift_id, qty, coins, idempotency_key)
            .await?;
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
        // On a dedup hit (an idempotency-key retry) the original send already
        // broadcast and scored, so skip both — and signal `false` so the caller
        // skips its own side-effects too. A fresh send broadcasts and feeds goals.
        if inserted {
            self.publish(&StreamEvent::Gift(line.clone())).await;
            // Feed any active `gifts`-metric goal bars on this stream (creator goal
            // bars, migration 0090). Best-effort: a goal-update hiccup never fails the gift.
            self.feed_gift_goals(stream_id, i64::from(qty)).await;
        }
        Ok((line, inserted))
    }

    /// Advance a stream's active `gifts`-metric goal bars by `delta` gift units after
    /// a gift is recorded, broadcasting [`StreamEvent::GoalProgress`] per advanced
    /// goal and [`StreamEvent::GoalReached`] on the threshold crossing. Best-effort:
    /// any storage or broadcast hiccup is swallowed so the gift is never failed by
    /// goal bookkeeping. Uses the pool [`LiveService`] already holds.
    async fn feed_gift_goals(&self, stream_id: Ulid, delta: i64) {
        if delta <= 0 {
            return;
        }
        let repo = aero_storage::GoalRepo::new(self.participants.pool().clone());
        let goals = match repo.list_active(stream_id).await {
            Ok(g) => g,
            Err(err) => {
                warn!(?err, %stream_id, "list_active goals failed; skipping goal feed");
                return;
            }
        };
        for goal in goals {
            if goal.metric_type != "gifts" {
                continue;
            }
            match repo.add_progress(goal.id, delta).await {
                Ok(Some((current, just_reached))) => {
                    self.publish(&StreamEvent::GoalProgress {
                        stream_id,
                        goal_id: goal.id,
                        current,
                        target: goal.target,
                    })
                    .await;
                    if just_reached {
                        self.publish(&StreamEvent::GoalReached {
                            stream_id,
                            goal_id: goal.id,
                        })
                        .await;
                    }
                }
                // Goal went inactive between the list and the bump, or a storage
                // error: skip it (best-effort), never failing the gift.
                Ok(None) => {}
                Err(err) => warn!(?err, goal_id = %goal.id, "goal add_progress failed"),
            }
        }
    }

    pub async fn recent_gifts(&self, stream_id: Ulid, limit: i64) -> Result<Vec<StreamGiftLine>> {
        Ok(self
            .live
            .recent_gifts(stream_id, limit.clamp(1, 100))
            .await?)
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
        Ok(self
            .live
            .recent_gifts_since(stream_id, since, limit.clamp(1, 100))
            .await?)
    }

    pub async fn leaderboard(&self, stream_id: Ulid, limit: i64) -> Result<Vec<GiftLeaderRow>> {
        Ok(self.live.leaderboard(stream_id, limit.clamp(1, 50)).await?)
    }

    // ------------------------------------------------- presence + lifecycle

    /// Broadcast an updated viewer count (best-effort, fire-and-forget).
    pub async fn publish_viewers(&self, stream_id: Ulid, count: u32) {
        self.publish(&StreamEvent::Viewers { stream_id, count })
            .await;
    }

    /// Broadcast an arbitrary [`StreamEvent`] on its stream's subject (seq-stamped
    /// like every other event). Best-effort, fire-and-forget — the public seam for
    /// feature modules (hype train, raids) to fan an event out to watchers without
    /// reaching into the private publish path.
    pub async fn broadcast(&self, event: &StreamEvent) {
        self.publish(event).await;
    }

    /// Resolve whether `sender` should be badged as a subscriber on a chat line in
    /// `stream_id`: they have an active creator subscription to the stream's owner
    /// (migration 0082). Best-effort — a missing stream or any storage hiccup
    /// degrades to `false` (no badge) rather than blocking the post, and a streamer
    /// is never "their own subscriber". Resolved here (the service holds the
    /// [`StreamRepo`] + a pool) so both the REST and WS chat paths share one rule.
    pub async fn subscriber_flag(&self, stream_id: Ulid, sender: ParticipantId) -> bool {
        let Ok(Some(stream)) = self.streams.get(stream_id).await else {
            return false;
        };
        if stream.owner_id == sender {
            return false;
        }
        SubscriptionRepo::new(self.participants.pool().clone())
            .is_subscribed(stream.owner_id, sender)
            .await
            .unwrap_or(false)
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
        self.publish(&StreamEvent::Status {
            stream_id,
            status: StreamStatus::Ended,
        })
        .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(value: &str) -> SocketAddr {
        value.parse().expect("test socket address")
    }

    #[test]
    fn media_urls_use_listener_ports_not_http_port() {
        let urls = LiveIngestUrls::new(
            "https://live.example.test:8443",
            None,
            addr("0.0.0.0:1935"),
            addr("0.0.0.0:1936"),
        );

        assert_eq!(
            urls.for_stream(StreamProtocol::Rtmp, "secret"),
            "rtmp://live.example.test:1935/live/secret"
        );
        assert_eq!(
            urls.for_stream(StreamProtocol::Whip, "secret"),
            "https://live.example.test:8443/whip/secret"
        );
        assert_eq!(
            urls.for_stream(StreamProtocol::Srt, "secret"),
            "srt://live.example.test:1936?streamid=secret"
        );
    }

    #[test]
    fn explicit_ingest_host_replaces_wildcard_listeners() {
        let urls = LiveIngestUrls::new(
            "https://live.example.test",
            Some("203.0.113.42"),
            addr("0.0.0.0:11935"),
            addr("0.0.0.0:11936"),
        );

        assert_eq!(
            urls.for_stream(StreamProtocol::Rtmp, "key"),
            "rtmp://203.0.113.42:11935/live/key"
        );
        assert_eq!(
            urls.for_stream(StreamProtocol::Srt, "key"),
            "srt://203.0.113.42:11936?streamid=key"
        );
    }

    #[test]
    fn concrete_listener_hosts_are_preserved() {
        let urls = LiveIngestUrls::new(
            "https://live.example.test",
            Some("203.0.113.42"),
            addr("192.0.2.10:1935"),
            addr("192.0.2.11:1936"),
        );

        assert_eq!(
            urls.for_stream(StreamProtocol::Rtmp, "key"),
            "rtmp://192.0.2.10:1935/live/key"
        );
        assert_eq!(
            urls.for_stream(StreamProtocol::Srt, "key"),
            "srt://192.0.2.11:1936?streamid=key"
        );
    }

    #[test]
    fn ipv6_hosts_are_bracketed_with_media_ports() {
        let urls = LiveIngestUrls::new(
            "https://[2001:db8::42]:8443",
            None,
            addr("[::]:1935"),
            addr("[::]:1936"),
        );

        assert_eq!(
            urls.for_stream(StreamProtocol::Rtmp, "key"),
            "rtmp://[2001:db8::42]:1935/live/key"
        );
        assert_eq!(
            urls.for_stream(StreamProtocol::Srt, "key"),
            "srt://[2001:db8::42]:1936?streamid=key"
        );
    }

    #[test]
    fn unusable_public_hosts_fall_back_to_matching_loopback_family() {
        let ipv4 = LiveIngestUrls::new(
            "http://0.0.0.0:3030",
            Some("0.0.0.0"),
            addr("0.0.0.0:1935"),
            addr("0.0.0.0:1936"),
        );
        let ipv6 = LiveIngestUrls::new("not a URL", None, addr("[::]:1935"), addr("[::]:1936"));

        assert_eq!(
            ipv4.for_stream(StreamProtocol::Rtmp, "key"),
            "rtmp://127.0.0.1:1935/live/key"
        );
        assert_eq!(
            ipv6.for_stream(StreamProtocol::Srt, "key"),
            "srt://[::1]:1936?streamid=key"
        );
    }
}
