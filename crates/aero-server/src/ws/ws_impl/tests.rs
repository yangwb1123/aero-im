//! `send_markdown` frame serde tests, moved verbatim from `ws_impl`.
//! Loaded via `#[cfg(test)] mod send_markdown_tests;` so the module name is
//! preserved (`ws::ws_impl::send_markdown_tests`). This file IS the module
//! body, so the inner items are reproduced exactly (indentation included).

    use super::ClientFrame;
    use aero_common::{markdown::parse_markdown_to_blocks, Block, ParticipantId, SpanStyle};

    /// The new `send_markdown` frame deserializes with the documented field set
    /// (markdown text + optional reply_to / expires_after_secs) and is distinct
    /// from `send_message`. Pure serde round-trip — no I/O, no `AppState`.
    #[test]
    fn send_markdown_frame_deserializes() {
        // `RoomId` is serde-transparent over `Ulid`, so the wire value is a ULID
        // string (matching the other ws frame tests), not a UUID.
        let frame: ClientFrame = serde_json::from_str(
            r#"{
                "type":"send_markdown",
                "room_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "markdown":"hello **world**",
                "expires_after_secs":60
            }"#,
        )
        .expect("parse send_markdown frame");
        match frame {
            ClientFrame::SendMarkdown { markdown, reply_to, expires_after_secs, .. } => {
                assert_eq!(markdown, "hello **world**");
                assert!(reply_to.is_none(), "reply_to defaults to None when absent");
                assert_eq!(expires_after_secs, Some(60));
            }
            _ => panic!("expected SendMarkdown variant"),
        }
    }

    /// The edge transform the `SendMarkdown` arm performs before the shared
    /// dispatch: the markdown body parses into exactly the `Vec<Block>` the
    /// structured `SendMessage` path would have carried. Verifies the bold span
    /// survives so the rich-text actually reaches `send_message`.
    #[test]
    fn send_markdown_body_parses_to_expected_blocks() {
        let blocks = parse_markdown_to_blocks("hello **world**");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Text { content, spans } => {
                assert_eq!(content, "hello world");
                assert_eq!(spans.len(), 1);
                assert_eq!((spans[0].start, spans[0].end), (6, 11));
                assert!(matches!(spans[0].style, SpanStyle::Bold));
            }
            other => panic!("expected text block, got {other:?}"),
        }
    }

    /// @mention follow-up contract: a leading `@name` parses to a nil-id
    /// `Block::Mention` marker (display-name → ParticipantId resolution is a
    /// deferred follow-up), and any trailing text still lands verbatim so the
    /// message body is never lost.
    #[test]
    fn send_markdown_mention_lands_as_nil_id_marker() {
        let blocks = parse_markdown_to_blocks("@alice ping");
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            Block::Mention { participant } => {
                assert_eq!(
                    *participant,
                    ParticipantId::nil(),
                    "mention id is nil pending resolution"
                );
            }
            other => panic!("expected mention block, got {other:?}"),
        }
        assert!(matches!(&blocks[1], Block::Text { .. }), "trailing text preserved");
    }
