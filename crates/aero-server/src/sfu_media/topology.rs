//! SDP media layout parsing and revisioned SFU topology snapshots.

use std::collections::HashSet;

use aero_common::{
    CallId, ParticipantId, SfuMediaKind, SfuPublishedTrack, SfuPublisherDescription,
};
use aero_live_webrtc::canonical_mid;

const MAX_MEDIA_SECTIONS: usize = 64;
const MAX_MID_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

impl Direction {
    fn sends(self) -> bool {
        matches!(self, Self::SendRecv | Self::SendOnly)
    }

    fn receives(self) -> bool {
        matches!(self, Self::SendRecv | Self::RecvOnly)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SfuReceiveSlot {
    pub(super) mid: String,
    pub(super) media_kind: SfuMediaKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SfuOfferLayout {
    pub(super) published: Vec<SfuPublishedTrack>,
    pub(super) receive_slots: Vec<SfuReceiveSlot>,
}

struct PendingMedia {
    media_kind: SfuMediaKind,
    mid: Option<String>,
    direction: Direction,
}

/// Parse the audio/video m-sections relevant to SFU routing.
///
/// SDP syntax/codec validity remains str0m's responsibility. This pass only
/// extracts the bounded MID, media-kind and direction topology used to validate
/// explicit subscriber routes.
pub(super) fn parse_offer_layout(sdp: &str) -> Result<SfuOfferLayout, String> {
    fn finish(
        pending: Option<PendingMedia>,
        seen: &mut HashSet<String>,
        published: &mut Vec<SfuPublishedTrack>,
        receive_slots: &mut Vec<SfuReceiveSlot>,
    ) -> Result<(), String> {
        let Some(media) = pending else {
            return Ok(());
        };
        let Some(raw_mid) = media.mid else {
            return Err("audio/video m-section has no MID".into());
        };
        let raw_mid = raw_mid.trim();
        if raw_mid.is_empty() || raw_mid.len() > MAX_MID_BYTES {
            return Err("media MID is empty or exceeds 64 bytes".into());
        }
        let mid = canonical_mid(raw_mid);
        if !seen.insert(mid.clone()) {
            return Err("duplicate media MID".into());
        }
        if media.direction.sends() {
            published.push(SfuPublishedTrack {
                mid: mid.clone(),
                media_kind: media.media_kind,
            });
        }
        if media.direction.receives() {
            receive_slots.push(SfuReceiveSlot {
                mid,
                media_kind: media.media_kind,
            });
        }
        Ok(())
    }

    let mut pending = None;
    let mut seen = HashSet::new();
    let mut published = Vec::new();
    let mut receive_slots = Vec::new();
    let mut media_sections = 0usize;

    for line in sdp.lines().map(str::trim) {
        if let Some(mline) = line.strip_prefix("m=") {
            finish(
                pending.take(),
                &mut seen,
                &mut published,
                &mut receive_slots,
            )?;
            let media_kind = if mline.starts_with("audio ") {
                Some(SfuMediaKind::Audio)
            } else if mline.starts_with("video ") {
                Some(SfuMediaKind::Video)
            } else {
                None
            };
            pending = media_kind.map(|media_kind| PendingMedia {
                media_kind,
                mid: None,
                direction: Direction::SendRecv,
            });
            if pending.is_some() {
                media_sections += 1;
                if media_sections > MAX_MEDIA_SECTIONS {
                    return Err("SDP exceeds 64 audio/video media sections".into());
                }
            }
            continue;
        }
        let Some(media) = pending.as_mut() else {
            continue;
        };
        if let Some(mid) = line.strip_prefix("a=mid:") {
            media.mid = Some(mid.to_owned());
        } else {
            media.direction = match line {
                "a=sendrecv" => Direction::SendRecv,
                "a=sendonly" => Direction::SendOnly,
                "a=recvonly" => Direction::RecvOnly,
                "a=inactive" => Direction::Inactive,
                _ => media.direction,
            };
        }
    }
    finish(pending, &mut seen, &mut published, &mut receive_slots)?;
    if published.is_empty() && receive_slots.is_empty() {
        return Err("SDP offer has no active audio/video media".into());
    }
    Ok(SfuOfferLayout {
        published,
        receive_slots,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SfuTopologySnapshot {
    pub call_id: CallId,
    pub revision: u64,
    pub publishers: Vec<SfuPublisherDescription>,
}

impl SfuTopologySnapshot {
    #[must_use]
    pub fn required_recv_slots(&self, subscriber: ParticipantId) -> usize {
        self.publishers
            .iter()
            .filter(|publisher| publisher.participant != subscriber)
            .map(|publisher| publisher.tracks.len())
            .sum()
    }
}

pub(super) fn sorted_publishers(
    publishers: impl Iterator<Item = SfuPublisherDescription>,
) -> Vec<SfuPublisherDescription> {
    let mut publishers: Vec<_> = publishers.collect();
    publishers.sort_by_key(|publisher| publisher.participant);
    for publisher in &mut publishers {
        publisher
            .tracks
            .sort_by(|left, right| left.mid.cmp(&right.mid));
    }
    publishers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_send_and_receive_directions_without_treating_recvonly_as_published() {
        let sdp = "v=0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:local-audio\r\n\
a=sendonly\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:remote-video\r\n\
a=recvonly\r\n";
        let layout = parse_offer_layout(sdp).unwrap();
        assert_eq!(layout.published[0].mid, "local_audio");
        assert_eq!(layout.published[0].media_kind, SfuMediaKind::Audio);
        assert_eq!(layout.receive_slots[0].mid, "remote_video");
        assert_eq!(layout.receive_slots[0].media_kind, SfuMediaKind::Video);
    }

    #[test]
    fn rejects_duplicate_canonical_mids() {
        let sdp = "v=0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:track-main\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:track_main\r\n";
        assert!(parse_offer_layout(sdp).is_err());
    }
}
