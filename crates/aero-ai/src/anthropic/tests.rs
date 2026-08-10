use super::*;

#[test]
fn request_body_shape_matches_anthropic_contract() {
    let msgs = vec![
        ChatMsg::user("hello"),
        ChatMsg::assistant("hi"),
        ChatMsg::user("how are you?"),
    ];
    let body = RequestBody {
        model: "claude-sonnet-4-6",
        max_tokens: 256,
        system: SystemBlock::cached("be terse"),
        messages: &msgs,
    };
    let json = serde_json::to_value(&body).unwrap();
    assert_eq!(json["model"], "claude-sonnet-4-6");
    assert_eq!(json["max_tokens"], 256);
    // `system` is now the cache-tagged content-block array form (required to
    // carry `cache_control`), not a bare string. The text is preserved.
    let sys = json["system"]
        .as_array()
        .expect("system must be a block array");
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0]["type"], "text");
    assert_eq!(sys[0]["text"], "be terse");
    let arr = json["messages"].as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["role"], "user");
    assert_eq!(arr[0]["content"], "hello");
    assert_eq!(arr[1]["role"], "assistant");
    assert_eq!(arr[2]["content"], "how are you?");
}

#[test]
fn request_body_system_block_carries_prompt_cache_control() {
    // P3-4: the stable system prefix must be tagged for prompt caching so a
    // reused prefix bills at the cache-read rate. Assert the serialized body's
    // system block carries `cache_control: {type:"ephemeral"}`.
    let msgs = vec![ChatMsg::user("q")];
    let body = RequestBody {
        model: "claude-sonnet-4-6",
        max_tokens: 64,
        system: SystemBlock::cached("a stable, reusable system prompt"),
        messages: &msgs,
    };
    let json = serde_json::to_value(&body).unwrap();
    let block = &json["system"][0];
    assert_eq!(block["cache_control"]["type"], "ephemeral");
    assert_eq!(block["text"], "a stable, reusable system prompt");
}

#[test]
fn stream_request_body_system_block_carries_cache_control() {
    // The streaming path uses the owned block variant — it must cache too.
    let body = StreamRequestBody {
        model: "claude-sonnet-4-6".into(),
        max_tokens: 64,
        system: OwnedSystemBlock::cached("stable system".into()),
        messages: vec![ChatMsg::user("q")],
        stream: true,
    };
    let json = serde_json::to_value(&body).unwrap();
    assert_eq!(json["stream"], true);
    let block = &json["system"][0];
    assert_eq!(block["type"], "text");
    assert_eq!(block["text"], "stable system");
    assert_eq!(block["cache_control"]["type"], "ephemeral");
}

#[test]
fn tools_cache_control_tags_only_the_last_tool() {
    // The tools prefix is cached by tagging the LAST tool definition; earlier
    // tools must stay untagged (one breakpoint covers the whole prefix).
    let tools = vec![
        ToolDef {
            name: "search".into(),
            description: "search the room".into(),
            input_schema: serde_json::json!({ "type": "object" }),
        },
        ToolDef {
            name: "clock".into(),
            description: "current time".into(),
            input_schema: serde_json::json!({ "type": "object" }),
        },
    ];
    let values = tools_with_cache_control(&tools);
    assert_eq!(values.len(), 2);
    // First tool: no cache_control.
    assert!(values[0].get("cache_control").is_none());
    assert_eq!(values[0]["name"], "search");
    // Last tool: cache_control present, original fields preserved.
    assert_eq!(values[1]["cache_control"]["type"], "ephemeral");
    assert_eq!(values[1]["name"], "clock");
    assert_eq!(values[1]["input_schema"]["type"], "object");
}

#[test]
fn tools_cache_control_empty_slice_is_noop() {
    assert!(tools_with_cache_control(&[]).is_empty());
}

#[test]
fn chat_msg_roles() {
    assert_eq!(ChatMsg::user("x").role, "user");
    assert_eq!(ChatMsg::assistant("x").role, "assistant");
}

#[test]
fn response_parses_concatenated_text_blocks() {
    let raw = r#"{
        "content": [
            {"type":"text","text":"Hello "},
            {"type":"tool_use","id":"t1","name":"x","input":{}},
            {"type":"text","text":"world"}
        ]
    }"#;
    let parsed: ResponseBody = serde_json::from_str(raw).unwrap();
    let joined: String = parsed
        .content
        .into_iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text),
            ContentBlock::ToolUse { .. } | ContentBlock::Other => None,
        })
        .collect::<String>();
    assert_eq!(joined, "Hello world");
}

#[test]
fn parse_agent_turn_splits_text_and_tool_calls() {
    // A real tool-use response: interleaved text + two tool_use blocks.
    let raw = r#"{
        "content": [
            {"type":"text","text":"let me check"},
            {"type":"tool_use","id":"tu_1","name":"search","input":{"q":"deploys"}},
            {"type":"tool_use","id":"tu_2","name":"clock","input":{}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 7}
    }"#;
    let turn = parse_agent_turn(raw).unwrap();
    assert_eq!(turn.text, "let me check");
    assert!(!turn.is_final(), "tool calls present ⇒ not final");
    assert_eq!(turn.tool_uses.len(), 2);
    assert_eq!(turn.tool_uses[0].id, "tu_1");
    assert_eq!(turn.tool_uses[0].name, "search");
    assert_eq!(turn.tool_uses[0].input["q"], "deploys");
    assert_eq!(turn.tool_uses[1].name, "clock");
    assert_eq!(turn.usage.input_tokens, 10);
    assert_eq!(turn.usage.output_tokens, 7);
}

#[test]
fn parse_agent_turn_text_only_is_final() {
    let raw = r#"{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}"#;
    let turn = parse_agent_turn(raw).unwrap();
    assert!(turn.is_final());
    assert_eq!(turn.text, "done");
    assert!(turn.tool_uses.is_empty());
}

#[test]
fn tool_def_serializes_to_anthropic_shape() {
    let def = ToolDef {
        name: "search_messages".into(),
        description: "Search the room".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        }),
    };
    let json = serde_json::to_value(&def).unwrap();
    assert_eq!(json["name"], "search_messages");
    assert_eq!(json["description"], "Search the room");
    assert_eq!(json["input_schema"]["type"], "object");
    assert_eq!(json["input_schema"]["required"][0], "query");
}

#[test]
fn response_parses_usage_from_anthropic_shape() {
    // Mirrors a real Anthropic Messages API response: a top-level `usage`
    // object alongside `content`, including the cache fields we ignore.
    let raw = r#"{
        "id": "msg_01XYZ",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{"type":"text","text":"42"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 1234,
            "output_tokens": 56,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        }
    }"#;
    let parsed: ResponseBody = serde_json::from_str(raw).unwrap();
    let usage = parsed.usage.expect("usage must be present");
    assert_eq!(usage.input_tokens, 1234);
    assert_eq!(usage.output_tokens, 56);
}

#[test]
fn usage_deserializes_standalone_object() {
    let u: Usage = serde_json::from_str(r#"{"input_tokens":10,"output_tokens":20}"#).unwrap();
    assert_eq!(
        u,
        Usage {
            input_tokens: 10,
            output_tokens: 20
        }
    );
}

#[test]
fn response_without_usage_defaults_to_none() {
    // An older/malformed response missing `usage` must not fail to parse —
    // it degrades to None (callers treat that as zero usage).
    let raw = r#"{"content":[{"type":"text","text":"hi"}]}"#;
    let parsed: ResponseBody = serde_json::from_str(raw).unwrap();
    assert!(parsed.usage.is_none());
}

#[test]
fn usage_fields_default_to_zero_when_partial() {
    // Defensive: a usage object missing one field defaults it to zero rather
    // than failing (the `#[serde(default)]` on each field).
    let u: Usage = serde_json::from_str(r#"{"input_tokens":7}"#).unwrap();
    assert_eq!(
        u,
        Usage {
            input_tokens: 7,
            output_tokens: 0
        }
    );
}

#[test]
fn truncate_at_char_boundary() {
    let s = "中文测试";
    let t = truncate(s, 4);
    assert!(t.ends_with("..."));
    // Should not panic on non-ASCII boundary
    assert!(t.is_char_boundary(t.len()));
}

#[test]
fn sse_parser_extracts_text_delta() {
    let block = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}";
    assert_eq!(extract_text_delta(block), Some("Hello".into()));
}

#[test]
fn sse_parser_ignores_non_delta_events() {
    let ping = "event: ping\ndata: {}";
    assert_eq!(extract_text_delta(ping), None);

    let start = "event: message_start\ndata: {\"type\":\"message_start\"}";
    assert_eq!(extract_text_delta(start), None);
}

#[test]
fn sse_parser_ignores_non_text_delta_type() {
    let block = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\"}}";
    assert_eq!(extract_text_delta(block), None);
}

#[test]
fn sse_parser_buffers_across_chunks() {
    let mut p = SseParser::default();
    // Split an event across two byte deliveries.
    let part1 = b"event: content_block_delta\ndata: {\"type\":\"content_block_delt";
    let part2 = b"a\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n";
    p.push_bytes(&Bytes::from(part1.as_slice()));
    assert_eq!(p.next_chunk(), None, "event not yet complete");
    p.push_bytes(&Bytes::from(part2.as_slice()));
    assert_eq!(p.next_chunk(), Some("Hi".into()));
    assert_eq!(p.next_chunk(), None);
}

#[test]
fn sse_parser_queues_multiple_chunks() {
    let mut p = SseParser::default();
    let raw = concat!(
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"A\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"B\"}}\n\n",
    );
    p.push_bytes(&Bytes::from(raw.as_bytes()));
    assert_eq!(p.next_chunk(), Some("A".into()));
    assert_eq!(p.next_chunk(), Some("B".into()));
    assert_eq!(p.next_chunk(), None);
}

/// REQ-3 / AT-1 regression marker: a UTF-8 multi-byte char (`中` = `E4 B8
/// AD`) split across two network chunks must survive intact. This failed
/// on the pre-fix parser (both chunks dropped whole — see the recorded red
/// run) and must stay green forever.
#[test]
fn sse_parser_cjk_split_two_chunks() {
    let head = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"";
    let mid = "中文".as_bytes(); // E4 B8 AD | E6 96 87
    let tail = b"\"}}\n\n";

    // Split after the lead byte of `中`: chunk 1 ends `…"text":"` + `E4`.
    let mut p = SseParser::default();
    let mut c1 = head.to_vec();
    c1.extend_from_slice(&mid[..1]);
    p.push_bytes(&Bytes::from(c1));
    assert_eq!(p.next_chunk(), None, "event not yet complete");
    let mut c2 = mid[1..].to_vec();
    c2.extend_from_slice(tail);
    p.push_bytes(&Bytes::from(c2));
    assert_eq!(p.next_chunk(), Some("中文".into()));
    assert_eq!(p.next_chunk(), None);

    // Mirror split inside the second char (`文` = `E6 96 87`): chunk 1
    // holds a complete `中` plus the `E6` lead byte.
    let mut p = SseParser::default();
    let mut c1 = head.to_vec();
    c1.extend_from_slice(&mid[..4]);
    p.push_bytes(&Bytes::from(c1));
    assert_eq!(p.next_chunk(), None, "event not yet complete");
    let mut c2 = mid[4..].to_vec();
    c2.extend_from_slice(tail);
    p.push_bytes(&Bytes::from(c2));
    assert_eq!(p.next_chunk(), Some("中文".into()));
    assert_eq!(p.next_chunk(), None);
}

/// AT-1b: `中` split across three chunks (`E4` | `B8` | `AD…`) — the tail
/// must survive two consecutive truncated-sequence errors.
#[test]
fn sse_parser_cjk_split_three_chunks() {
    let head = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"";
    let mid = "中文".as_bytes();
    let tail = b"\"}}\n\n";

    let mut p = SseParser::default();
    let mut c1 = head.to_vec();
    c1.extend_from_slice(&mid[..1]); // …"text":" + E4
    p.push_bytes(&Bytes::from(c1));
    assert_eq!(p.next_chunk(), None);
    assert_eq!(p.tail, vec![0xE4], "tail retains the truncated lead byte");
    p.push_bytes(&Bytes::from_static(b"\xB8")); // continuation byte alone
    assert_eq!(p.next_chunk(), None);
    assert_eq!(
        p.tail,
        vec![0xE4, 0xB8],
        "tail grows to at most 2 bytes across two truncated pushes"
    );
    let mut c2 = mid[2..].to_vec(); // AD + 文 + rest of event
    c2.extend_from_slice(tail);
    p.push_bytes(&Bytes::from(c2));
    assert_eq!(p.next_chunk(), Some("中文".into()));
    assert_eq!(p.next_chunk(), None);
    assert!(p.tail.is_empty(), "tail drained once the char completes");
}

/// AT-2 property sweep: for a fixture with two `text_delta` events, two CJK
/// chars and a 4-byte emoji, split the byte stream at EVERY offset and
/// assert the parser yields exactly the full text as exactly the right
/// number of chunks. Covers all 1/2/3-byte intra-char cuts plus 2-chunk
/// cuts of 4-byte chars for free.
#[test]
fn sse_parser_property_every_split_offset() {
    let raw = concat!(
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"中文😀\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"文B\"}}\n\n",
    );
    let expected = "中文😀文B";
    let bytes = raw.as_bytes();
    for s in 1..bytes.len() - 1 {
        let mut p = SseParser::default();
        p.push_bytes(&Bytes::from(&bytes[..s]));
        p.push_bytes(&Bytes::from(&bytes[s..]));
        let mut out = String::new();
        let mut chunks = 0;
        while let Some(c) = p.next_chunk() {
            out.push_str(&c);
            chunks += 1;
        }
        assert_eq!(out, expected, "split at byte {s}: text lost or garbled");
        assert_eq!(chunks, 2, "split at byte {s}: event boundary missed");
        assert!(
            std::str::from_utf8(out.as_bytes()).is_ok(),
            "split at byte {s}: yielded chunk is not valid UTF-8"
        );
        assert!(p.tail.is_empty(), "split at byte {s}: tail not drained");
    }
}

/// AT-2: a 4-byte char (`😀` = `F0 9F 98 80`) pushed one byte per chunk —
/// exercises the bounded-memory invariant (tail must never exceed 3
/// bytes). The `tail` length assertions land with the fix.
#[test]
fn sse_parser_emoji_split_four_pushes() {
    let mut p = SseParser::default();
    p.push_bytes(&Bytes::from_static(
        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"\xF0",
    ));
    assert_eq!(p.next_chunk(), None);
    assert_eq!(p.tail.len(), 1, "tail capped: 1 byte after one push");
    p.push_bytes(&Bytes::from_static(b"\x9F"));
    assert_eq!(p.next_chunk(), None);
    assert_eq!(p.tail.len(), 2, "tail capped: 2 bytes after two pushes");
    p.push_bytes(&Bytes::from_static(b"\x98"));
    assert_eq!(p.next_chunk(), None);
    assert_eq!(p.tail.len(), 3, "tail capped: 3 bytes after three pushes");
    assert!(p.tail.len() <= 3, "bounded-memory invariant");
    p.push_bytes(&Bytes::from_static(b"\x80\"}}\n\n"));
    assert_eq!(p.next_chunk(), Some("😀".into()));
    assert_eq!(p.next_chunk(), None);
    assert!(p.tail.is_empty(), "tail drained once the char completes");
}

/// AT-2: an empty `bytes` push must be a no-op. (a) fresh parser; (b) an
/// empty push inserted between the two halves of a split char must leave
/// behavior identical to a parser that never saw it.
#[test]
fn sse_parser_empty_bytes_push() {
    // (a) Fresh parser: nothing appended, nothing queued, no loop.
    let mut p = SseParser::default();
    p.push_bytes(&Bytes::new());
    assert!(p.buffer.is_empty());
    assert_eq!(p.next_chunk(), None);

    // (b) Empty push between the chunks of a split `中`.
    let head = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"";
    let tail = b"\"}}\n\n";
    let mut c1 = head.to_vec();
    c1.extend_from_slice(&[0xE4]); // first byte of 中
    let mut c2 = vec![0xB8, 0xAD]; // rest of 中 + event tail
    c2.extend_from_slice(tail);

    let mut with_empty = SseParser::default();
    with_empty.push_bytes(&Bytes::from(c1.clone()));
    with_empty.push_bytes(&Bytes::new());
    assert_eq!(
        with_empty.tail,
        vec![0xE4],
        "empty push must not grow the tail"
    );
    with_empty.push_bytes(&Bytes::from(c2.clone()));

    let mut control = SseParser::default();
    control.push_bytes(&Bytes::from(c1));
    control.push_bytes(&Bytes::from(c2));

    let mut out_empty = Vec::new();
    while let Some(c) = with_empty.next_chunk() {
        out_empty.push(c);
    }
    let mut out_ctrl = Vec::new();
    while let Some(c) = control.next_chunk() {
        out_ctrl.push(c);
    }
    assert_eq!(out_empty, out_ctrl, "empty push changed behavior");
    assert_eq!(out_empty, vec!["中".to_string()]);
}

/// Minimal `tracing::Subscriber` that counts WARN-level events, so tests
/// can assert the corruption branch is never silent. No new deps.
#[derive(Clone, Default)]
struct WarnCounter {
    warns: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl WarnCounter {
    fn count(&self) -> usize {
        self.warns.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl tracing::Subscriber for WarnCounter {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().level() == &tracing::Level::WARN {
            self.warns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// AT-2: mid-stream corruption (`error_len() == Some(_)`) must warn (never
/// silent), retain the valid prefix, and leave the parser usable.
#[test]
fn sse_parser_corrupt_mid_stream_warns_and_retains_prefix() {
    // Case 1: 0xFF mid-chunk after a valid prefix. The prefix before the
    // invalid byte must be retained in the buffer; the rest is dropped
    // with a warn.
    let mut p = SseParser::default();
    let warns = WarnCounter::default();
    tracing::subscriber::with_default(warns.clone(), || {
        p.push_bytes(&Bytes::from_static(
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"A\xFFB\"}}\n\n",
        ));
    });
    assert_eq!(warns.count(), 1, "corruption must never be silent");
    assert!(
        p.buffer.contains("\"text\":\"A"),
        "valid prefix before the invalid byte must be retained"
    );
    assert!(
        p.tail.is_empty(),
        "corruption must not leave a retained tail"
    );
    // State stays consistent: a subsequent good chunk still parses.
    p.push_bytes(&Bytes::from_static(
        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"X\"}}\n\n",
    ));
    assert_eq!(p.next_chunk(), Some("X".into()));

    // Case 2: retained truncated tail then a non-continuation byte —
    // `Some(1)` at index 0: warn, drop, tail cleared, parser usable.
    let mut p = SseParser::default();
    let warns = WarnCounter::default();
    p.push_bytes(&Bytes::from_static(b"\xE4")); // truncated 中 lead byte
    tracing::subscriber::with_default(warns.clone(), || {
        p.push_bytes(&Bytes::from_static(b"A"));
    });
    assert_eq!(warns.count(), 1, "tail-then-non-continuation must warn");
    assert!(p.tail.is_empty(), "tail cleared after corruption");
    assert_eq!(p.next_chunk(), None);
    p.push_bytes(&Bytes::from_static(
        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Y\"}}\n\n",
    ));
    assert_eq!(p.next_chunk(), Some("Y".into()));
}
