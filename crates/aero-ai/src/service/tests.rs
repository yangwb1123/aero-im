use super::*;
use aero_common::{Block, ParticipantId};
use time::OffsetDateTime;

#[test]
fn extract_text_accepts_utf8_within_cap() {
    let md = "# Title\n\nsome **markdown** body, csv-ish: a,b,c\n中文也行\n";
    assert_eq!(extract_text(md.as_bytes(), 64 * 1024).as_deref(), Some(md));
}

#[test]
fn extract_text_rejects_empty_oversized_and_binary() {
    // Empty.
    assert_eq!(extract_text(b"", 1024), None);
    // Over the cap.
    assert_eq!(extract_text(&vec![b'a'; 2048], 1024), None);
    // Invalid UTF-8 (a lone continuation byte) ⇒ not text.
    assert_eq!(extract_text(&[0xff, 0xfe, 0x00, 0x01], 1024), None);
    // Valid UTF-8 but full of NUL control bytes ⇒ treated as binary.
    assert_eq!(extract_text(&[0u8; 64], 1024), None);
}

#[test]
fn extract_text_allows_whitespace_controls() {
    // Tabs / newlines / carriage returns are normal in text and don't trip the
    // binary heuristic.
    let s = "line1\r\n\tindented\nline3\n";
    assert_eq!(extract_text(s.as_bytes(), 1024).as_deref(), Some(s));
}

#[tokio::test]
async fn moderate_without_anthropic_makes_no_paid_call_and_no_usage() {
    // No Anthropic key → moderation makes no paid call, returns no usage
    // (so the worker charges zero), and never touches the DB: the lazy pool
    // is never connected because both paths early-return (方向四).
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://aero:aero_dev_pw@localhost:5432/aero")
        .expect("connect_lazy never fails on a well-formed URL");
    let svc = AiService::new(
        None, // no Anthropic key
        std::sync::Arc::new(crate::embed::HashEmbedder::new()),
        std::sync::Arc::new(crate::transcribe::StubTranscriber),
        AiJobRepo::new(pool.clone()),
        MessageRepo::new(pool.clone()),
        RoomRepo::new(pool),
        None,
    );

    let (verdict, usage) = svc.moderate_with_usage("anything at all").await.unwrap();
    assert_eq!(verdict, None, "no key → conservative allow");
    assert!(usage.is_none(), "no key → no billed usage");
    // Empty text short-circuits before any client lookup too.
    assert_eq!(svc.moderate("   ").await.unwrap(), None);
}

#[test]
fn moderation_verdict_parsing() {
    assert_eq!(parse_moderation_verdict("SAFE"), None);
    assert_eq!(parse_moderation_verdict("  safe\n"), None); // not "BLOCK" → allow
    assert_eq!(
        parse_moderation_verdict("BLOCK: 暴力威胁"),
        Some("暴力威胁".to_owned())
    );
    assert_eq!(
        parse_moderation_verdict("BLOCK：色情内容"), // fullwidth colon
        Some("色情内容".to_owned())
    );
    assert_eq!(
        parse_moderation_verdict("BLOCK"),
        Some("内容违规".to_owned())
    );
}

fn mk_msg(text: &str) -> Message {
    Message {
        id: MessageId::new(),
        room_id: RoomId::new(),
        sender_id: ParticipantId::new(),
        blocks: vec![Block::text(text)],
        reply_to: None,
        metadata: serde_json::Value::Null,
        created_at: OffsetDateTime::now_utc(),
        edited_at: None,
        deleted_at: None,
        recalled_at: None,
        recalled_by: None,
        expires_at: None,
        version: 1,
    }
}

#[test]
fn heuristic_summary_uses_last_five() {
    let mut msgs = Vec::new();
    for i in 0..7 {
        msgs.push(mk_msg(&format!("line {i}")));
    }
    let s = heuristic_summary(&msgs);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 5);
    assert!(lines[0].contains("line 2"));
    assert!(lines[4].contains("line 6"));
    for l in &lines {
        assert!(l.starts_with("- "), "bullet form, got {l}");
    }
}

#[test]
fn heuristic_summary_empty_when_no_messages() {
    assert_eq!(heuristic_summary(&[]), "");
}

#[test]
fn heuristic_text_digest_takes_first_five_nonblank_lines() {
    let text = "line 0\n\n  line 1  \nline 2\nline 3\nline 4\nline 5\nline 6";
    let d = heuristic_text_digest(text);
    let lines: Vec<&str> = d.lines().collect();
    assert_eq!(lines.len(), 5, "capped at 5 lines");
    assert!(lines[0].starts_with("- "), "bullet form, got {}", lines[0]);
    assert!(lines[0].contains("line 0"));
    assert!(
        lines[1].contains("line 1"),
        "blank line skipped, whitespace trimmed"
    );
    assert!(lines[4].contains("line 4"));
}

#[test]
fn heuristic_text_digest_empty_when_blank() {
    assert_eq!(heuristic_text_digest("   \n\n  "), "");
}

fn hit(sender: ParticipantId, score: f32) -> SearchHit {
    let mut m = mk_msg("topic message");
    m.sender_id = sender;
    SearchHit {
        message: m,
        score,
        headline: None,
    }
}

#[test]
fn rank_experts_sums_scores_and_ranks_desc() {
    let alice = ParticipantId::new();
    let bob = ParticipantId::new();
    // Bob authored two weak hits (0.3 + 0.3 = 0.6); Alice one strong (0.5).
    let hits = vec![hit(bob, 0.3), hit(alice, 0.5), hit(bob, 0.3)];
    let ranked = rank_experts(&hits, 10);
    assert_eq!(ranked.len(), 2);
    // Bob's summed 0.6 beats Alice's 0.5.
    assert_eq!(ranked[0].participant, bob);
    assert!((ranked[0].score - 0.6).abs() < 1e-6);
    assert_eq!(ranked[0].citations.len(), 2, "both of bob's hits cited");
    assert_eq!(ranked[1].participant, alice);
}

#[test]
fn rank_experts_caps_citations_and_truncates_to_k() {
    let alice = ParticipantId::new();
    // 5 hits from one author — citations cap at MAX_EXPERT_CITATIONS.
    let hits: Vec<SearchHit> = (0..5).map(|_| hit(alice, 0.2)).collect();
    let ranked = rank_experts(&hits, 1);
    assert_eq!(ranked.len(), 1, "truncated to k=1");
    assert_eq!(
        ranked[0].citations.len(),
        MAX_EXPERT_CITATIONS,
        "citations capped"
    );
}

#[test]
fn rank_experts_empty_is_empty() {
    assert!(rank_experts(&[], 5).is_empty());
}

#[test]
fn rank_experts_is_deterministic_on_ties() {
    let a = ParticipantId::new();
    let b = ParticipantId::new();
    // Equal scores → order is broken by participant id (ascending), stable.
    let hits = vec![hit(a, 0.4), hit(b, 0.4)];
    let r1 = rank_experts(&hits, 10);
    let r2 = rank_experts(&hits, 10);
    assert_eq!(r1[0].participant, r2[0].participant);
    let lo = a.min(b);
    assert_eq!(r1[0].participant, lo, "lower id ranks first on a tie");
}

#[test]
fn rank_channels_orders_by_activity_desc_and_normalizes() {
    let busy = RoomId::new();
    let quiet = RoomId::new();
    let dead = RoomId::new();
    let cands = vec![
        (quiet, "quiet".to_owned(), 2_i64),
        (busy, "busy".to_owned(), 10_i64),
        (dead, "dead".to_owned(), 0_i64),
    ];
    let ranked = rank_channels(&cands, 10);
    assert_eq!(ranked.len(), 3);
    // Busiest first; score normalized to 1.0.
    assert_eq!(ranked[0].room, busy);
    assert!((ranked[0].score - 1.0).abs() < 1e-6);
    assert_eq!(ranked[1].room, quiet);
    assert!((ranked[1].score - 0.2).abs() < 1e-6);
    // Zero-activity ranks last with score 0 and the "not joined" reason.
    assert_eq!(ranked[2].room, dead);
    assert!((ranked[2].score - 0.0).abs() < 1e-6);
    assert!(ranked[2].reason.contains("未加入"));
    assert!(
        ranked[0].reason.contains("10"),
        "active reason cites the count"
    );
}

#[test]
fn rank_channels_truncates_to_k_and_breaks_ties_by_id() {
    let a = RoomId::new();
    let b = RoomId::new();
    // Equal activity → order broken by room id (ascending), stable.
    let cands = vec![(a, "a".to_owned(), 5_i64), (b, "b".to_owned(), 5_i64)];
    let r1 = rank_channels(&cands, 10);
    let r2 = rank_channels(&cands, 10);
    assert_eq!(r1[0].room, r2[0].room, "stable across runs");
    assert_eq!(r1[0].room, a.min(b), "lower id ranks first on a tie");
    // Truncation to k.
    assert_eq!(rank_channels(&cands, 1).len(), 1);
}

#[test]
fn rank_channels_unnamed_gets_fallback_name() {
    let room = RoomId::new();
    let ranked = rank_channels(&[(room, "   ".to_owned(), 1)], 5);
    assert_eq!(ranked[0].name, UNNAMED_CHANNEL);
}

#[test]
fn rank_channels_empty_is_empty() {
    assert!(rank_channels(&[], 5).is_empty());
}

#[test]
fn rank_people_orders_by_shared_overlap_desc() {
    let close = ParticipantId::new();
    let acquaintance = ParticipantId::new();
    let cands = vec![(acquaintance, 1_i64), (close, 4_i64)];
    let ranked = rank_people(&cands, 10);
    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0].participant, close);
    assert!((ranked[0].score - 1.0).abs() < 1e-6);
    assert!(
        ranked[0].reason.contains('4'),
        "reason cites the overlap count"
    );
    assert_eq!(ranked[1].participant, acquaintance);
    assert!((ranked[1].score - 0.25).abs() < 1e-6);
}

#[test]
fn rank_people_breaks_ties_by_id_and_truncates() {
    let a = ParticipantId::new();
    let b = ParticipantId::new();
    let cands = vec![(a, 3_i64), (b, 3_i64)];
    let r1 = rank_people(&cands, 10);
    let r2 = rank_people(&cands, 10);
    assert_eq!(r1[0].participant, r2[0].participant, "stable across runs");
    assert_eq!(r1[0].participant, a.min(b), "lower id ranks first on a tie");
    assert_eq!(rank_people(&cands, 1).len(), 1, "truncated to k");
}

#[test]
fn rank_people_empty_is_empty() {
    assert!(rank_people(&[], 5).is_empty());
}

#[test]
fn render_transcript_skips_empty_messages() {
    let mut empty = mk_msg("");
    empty.blocks = Vec::new();
    let msgs = vec![empty, mk_msg("hello")];
    let t = render_transcript(&msgs);
    assert_eq!(t.lines().count(), 1);
    assert!(t.contains("hello"));
}

#[test]
fn truncate_for_summary_keeps_under_limit() {
    let s = "a".repeat(50);
    assert_eq!(truncate_for_summary(&s, 120), s);
    let long = "汉".repeat(200);
    let t = truncate_for_summary(&long, 120);
    assert!(t.ends_with("…"));
    assert_eq!(t.chars().count(), 121);
}

// ---- FEATURE #8: thread auto-titling heuristic ----

#[test]
fn heuristic_title_takes_first_eight_words() {
    let root = "we should ship the new billing flow before the next release window";
    let title = heuristic_title(root);
    assert_eq!(title, "we should ship the new billing flow before");
    assert_eq!(title.split_whitespace().count(), HEURISTIC_TITLE_WORDS);
}

#[test]
fn heuristic_title_keeps_short_root_intact() {
    assert_eq!(heuristic_title("quick question"), "quick question");
}

#[test]
fn heuristic_title_collapses_newlines_and_trims() {
    assert_eq!(heuristic_title("  hello\nworld  "), "hello world");
}

#[test]
fn heuristic_title_empty_when_blank() {
    assert_eq!(heuristic_title("   \n  "), "");
}

#[test]
fn heuristic_title_truncates_unbroken_cjk() {
    // No whitespace boundaries — char-truncated rather than returned whole.
    let cjk = "这是一个非常长的没有空格的中文标题需要被截断处理以免标题过长影响显示".to_owned();
    let title = heuristic_title(&cjk);
    assert!(
        title.ends_with('…'),
        "long unbroken title truncated, got {title}"
    );
    assert!(title.chars().count() <= 25);
}

#[test]
fn clean_title_strips_quotes_and_trailing_punct() {
    assert_eq!(
        clean_title("\"Billing flow redesign.\""),
        "Billing flow redesign"
    );
    assert_eq!(clean_title("「发布计划讨论。」"), "发布计划讨论");
    assert_eq!(clean_title("  Roadmap sync!  "), "Roadmap sync");
}

#[test]
fn clean_title_takes_first_nonblank_line() {
    assert_eq!(
        clean_title("\n\nHere is a title\nignored second line"),
        "Here is a title"
    );
}

// ---- FEATURE #9: sentiment / toxicity heuristic truth table ----

#[test]
fn heuristic_sentiment_insult_is_negative_high_toxicity() {
    let s = heuristic_sentiment("you are an idiot and a loser");
    assert_eq!(s.sentiment, Sentiment::Negative);
    assert!(
        s.toxicity >= 0.8,
        "insult => high toxicity, got {}",
        s.toxicity
    );
    assert_eq!(s.tone, "angry");
}

#[test]
fn heuristic_sentiment_cjk_insult_is_negative() {
    let s = heuristic_sentiment("你就是个废物");
    assert_eq!(s.sentiment, Sentiment::Negative);
    assert!(s.toxicity >= 0.8);
    assert_eq!(s.tone, "angry");
}

#[test]
fn heuristic_sentiment_all_caps_is_angry() {
    let s = heuristic_sentiment("STOP DOING THAT RIGHT NOW");
    assert_eq!(s.sentiment, Sentiment::Negative);
    assert!(
        s.toxicity >= 0.5 && s.toxicity < 0.85,
        "shout < keyword, got {}",
        s.toxicity
    );
    assert_eq!(s.tone, "angry");
}

#[test]
fn heuristic_sentiment_short_caps_not_shouting() {
    // "OK" / "YES" are too short to be flagged as a deliberate shout.
    let s = heuristic_sentiment("OK");
    assert_eq!(s.sentiment, Sentiment::Neutral);
}

#[test]
fn heuristic_sentiment_positive_words() {
    let s = heuristic_sentiment("thanks so much, great job on this!");
    assert_eq!(s.sentiment, Sentiment::Positive);
    assert!(s.toxicity < 0.1);
    assert_eq!(s.tone, "friendly");
}

#[test]
fn heuristic_sentiment_cjk_positive() {
    let s = heuristic_sentiment("太好了,谢谢你");
    assert_eq!(s.sentiment, Sentiment::Positive);
    assert_eq!(s.tone, "friendly");
}

#[test]
fn heuristic_sentiment_neutral_default() {
    let s = heuristic_sentiment("the meeting is at 3pm in room 2");
    assert_eq!(s.sentiment, Sentiment::Neutral);
    assert!(s.toxicity <= 0.05);
    assert_eq!(s.tone, "neutral");
}

#[test]
fn heuristic_sentiment_lone_exclamation_is_excited() {
    let s = heuristic_sentiment("the build passed!");
    assert_eq!(s.sentiment, Sentiment::Neutral);
    assert_eq!(s.tone, "excited");
}

#[test]
fn heuristic_sentiment_blank_is_neutral() {
    let s = heuristic_sentiment("   ");
    assert_eq!(s.sentiment, Sentiment::Neutral);
    assert!(
        s.toxicity.abs() < 1e-6,
        "blank text has zero toxicity, got {}",
        s.toxicity
    );
    assert_eq!(s.tone, "neutral");
}

#[test]
fn parse_sentiment_verdict_well_formed() {
    let s = parse_sentiment_verdict("negative|0.82|angry").expect("parses");
    assert_eq!(s.sentiment, Sentiment::Negative);
    assert!((s.toxicity - 0.82).abs() < 1e-6);
    assert_eq!(s.tone, "angry");
}

#[test]
fn parse_sentiment_verdict_clamps_and_tolerates_whitespace() {
    let s = parse_sentiment_verdict("  positive | 1.5 | friendly  \n").expect("parses");
    assert_eq!(s.sentiment, Sentiment::Positive);
    assert!((s.toxicity - 1.0).abs() < 1e-6, "toxicity clamped to 1.0");
    assert_eq!(s.tone, "friendly");
}

#[test]
fn parse_sentiment_verdict_rejects_malformed() {
    assert!(parse_sentiment_verdict("not a verdict").is_none());
    assert!(parse_sentiment_verdict("negative|not-a-number|angry").is_none());
    assert!(parse_sentiment_verdict("").is_none());
}

#[test]
fn parse_sentiment_verdict_empty_tone_falls_back_to_label() {
    let s = parse_sentiment_verdict("positive|0.0|").expect("parses");
    assert_eq!(
        s.tone, "positive",
        "empty tone defaults to the sentiment label"
    );
}

#[test]
fn sentiment_serializes_lowercase() {
    let s = SentimentScore {
        sentiment: Sentiment::Negative,
        toxicity: 0.5,
        tone: "angry".into(),
    };
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["sentiment"], "negative");
    assert_eq!(v["tone"], "angry");
}

// ---------- 方向三-2: file content folded into the search/embed text ----------

/// `fold_searchable_with_document` appends a document's body after the message's
/// own text, drops empty doc text, and lets a file-only message be carried by the
/// document body alone.
#[test]
fn fold_searchable_with_document_combines_appends_and_handles_edges() {
    // Both present → message text first, doc body after, blank-line separated.
    let folded = fold_searchable_with_document("see attached spec", "Revenue grew 12% in Q3");
    assert!(
        folded.starts_with("see attached spec"),
        "msg text leads: {folded:?}"
    );
    assert!(
        folded.contains("Revenue grew 12% in Q3"),
        "doc body folded in: {folded:?}"
    );

    // Empty / whitespace-only doc text → unchanged base (nothing to add).
    assert_eq!(fold_searchable_with_document("hello", "   \n\t"), "hello");
    assert_eq!(fold_searchable_with_document("hello", ""), "hello");

    // File-only message (base is just the file name / blank) → doc body stands alone.
    assert_eq!(
        fold_searchable_with_document("   ", "Quarterly report body"),
        "Quarterly report body"
    );
}

/// `first_extractable_file` returns the first `File` block (skipping non-file
/// blocks and `Voice`), and `None` when the message has no attachment.
#[test]
fn first_extractable_file_picks_first_file_block() {
    let blob = aero_common::BlobId::new();
    let blocks = vec![
        Block::text("intro"),
        Block::Voice {
            blob_id: aero_common::BlobId::new(),
            duration_ms: 100,
            transcript: None,
        },
        Block::File {
            blob_id: blob,
            kind: aero_common::FileKind::Document,
            name: "report.pdf".into(),
            size: 4096,
        },
    ];
    let (id, name, size) = first_extractable_file(&blocks).expect("a file block");
    assert_eq!(id, blob);
    assert_eq!(name, "report.pdf");
    assert_eq!(size, 4096);

    // No file → None (text + voice only).
    let none = vec![Block::text("just chatting")];
    assert!(first_extractable_file(&none).is_none());
}

// ---- minimal OOXML (docx) ZIP builder, mirroring doc_extract's test helper ----
// Lets us exercise the REAL extract_text → doc_extract path on document bytes,
// then the fold, without needing a live blob store.
#[allow(clippy::cast_possible_truncation)]
fn make_docx(body_xml: &[u8]) -> Vec<u8> {
    use flate2::{write::DeflateEncoder, Compression};
    use std::io::Write as _;
    let name = "word/document.xml";
    let deflate = |data: &[u8]| {
        let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    };
    let comp = deflate(body_xml);
    let mut out = Vec::new();
    let lho = out.len() as u32;
    out.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]);
    out.extend_from_slice(&[0x14, 0x00, 0x00, 0x00, 0x08, 0x00]);
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // time/crc
    out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
    out.extend_from_slice(&(body_xml.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&[0x00, 0x00]);
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&comp);
    let cd_offset = out.len() as u32;
    let mut central = Vec::new();
    central.extend_from_slice(&[0x50, 0x4B, 0x01, 0x02, 0x14, 0x00, 0x14, 0x00]);
    central.extend_from_slice(&[0x00, 0x00, 0x08, 0x00]);
    central.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // time/crc
    central.extend_from_slice(&(comp.len() as u32).to_le_bytes());
    central.extend_from_slice(&(body_xml.len() as u32).to_le_bytes());
    central.extend_from_slice(&(name.len() as u16).to_le_bytes());
    central.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // extra/comment/disk
    central.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // attrs
    central.extend_from_slice(&lho.to_le_bytes());
    central.extend_from_slice(name.as_bytes());
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06, 0x00, 0x00, 0x00, 0x00]);
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&[0x00, 0x00]);
    out
}

/// End-to-end of the *pure* indexing logic: real docx bytes → `extract_text`
/// (via `doc_extract`) → folded into a message's searchable text. This is exactly
/// what the worker persists into `searchable_text` + embeds, minus the blob read.
#[test]
fn docx_body_is_extracted_and_folded_into_message_searchable_text() {
    let docx = make_docx(
        br"<w:document><w:body><w:p><w:r><w:t>Migration runbook: rotate the TLS cert before Friday</w:t></w:r></w:p></w:body></w:document>",
    );
    // The same extractor the worker uses on the blob bytes.
    let doc_text = extract_text(&docx, MAX_ATTACHMENT_BYTES).expect("docx extracts");
    assert!(
        doc_text.contains("rotate the TLS cert"),
        "body text extracted: {doc_text:?}"
    );

    // A message that just says "see attached" with the file name as its own text.
    let base = "see attached\nrunbook.docx";
    let folded = fold_searchable_with_document(base, &doc_text);
    // The document's *contents* are now in the indexed text, not just the name.
    assert!(
        folded.contains("rotate the TLS cert"),
        "doc body now searchable: {folded:?}"
    );
    assert!(
        folded.contains("see attached"),
        "original message text retained: {folded:?}"
    );
}

/// A binary / unextractable attachment yields no extra text, so folding is a
/// no-op — the message's own text is indexed unchanged (fail-open behaviour, the
/// worker never crashes on a bad attachment).
#[test]
fn unextractable_attachment_leaves_searchable_text_unchanged() {
    // A short NUL-filled "binary" blob: extract_text returns None.
    assert!(extract_text(&[0u8; 32], MAX_ATTACHMENT_BYTES).is_none());
    // With no doc text, the fold returns the base verbatim.
    assert_eq!(
        fold_searchable_with_document("hello world", ""),
        "hello world"
    );
}
