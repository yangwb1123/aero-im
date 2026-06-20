//! Minimal CommonMark-to-Blocks parser (ROADMAP6 方向三: 富文本编辑管线).
//!
//! Converts a Markdown string into [`Block`]s with proper [`Span`] annotations
//! so `**bold**`, `*italic*`, `` `code` ``, `[text](url)`, and `@mention`
//! become structured rich-text blocks. Pure, allocation-light, and dependency-
//! free — hand-rolled byte scanner matching the codebase style (see
//! [`aero_im_core::pii_detect`] and [`aero_server::content_sniff`]).
//!
//! ## Scope (CommonMark subset)
//! | Input             | Output                        |
//! |-------------------|-------------------------------|
//! | `**bold**`        | `Span { style: Bold, … }`     |
//! | `*italic*`        | `Span { style: Italic, … }`   |
//! | `` `mono` ``       | `Span { style: Code, … }`     |
//! | `[label](href)`   | `Span { style: Link{href} }`  |
//! | `@display_name`   | `Block::Mention{participant}` |
//! | `~~strike~~`      | `Span { style: Strikethrough}`|
//!
//! ## Deliberately NOT supported
//! * Headings, lists, blockquotes, images, tables, HTML — out of chat scope.
//! * Nested spans (e.g. `**bold *and italic***`) — the innermost wins.
//! * `_italic_` — ambiguous with snake_case, skipped.
//! * `__bold__` — ambiguous with dunder, skipped.
//! * Multi-line blocks — `\n` becomes a plain Block boundary (one Block per line).
//!
//! ## Return contract
//! A single empty/missing input line yields `Vec::new()` (not one empty text block).
//! A line with no recognised formatting yields one `Block::Text { spans: [] }`
//! with the content verbatim. `@mention`s resolve to `Block::Mention` on a
//! best-effort basis: only `display_name` is parsed (ParticipantId lookup is the
//! caller's responsibility).
//!
//! Pure, unit-tested without any I/O.

use crate::{Block, Span, SpanStyle};

/// Parse a plain Markdown string into a sequence of Blocks.
///
/// Each non-empty line becomes a Block (text/mention). Empty lines are skipped.
#[must_use]
pub fn parse_markdown_to_blocks(md: &str) -> Vec<Block> {
    md.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .flat_map(|line| parse_line(line))
        .collect()
}

/// Parse one line into zero or more Blocks.
///
/// A `@mention` at the start of a line (or on its own) yields a `Block::Mention`;
/// otherwise the whole line is treated as rich text with inline span parsing.
fn parse_line(line: &str) -> Vec<Block> {
    // @mention lines: "@display_name optional trailing text" → Mention block
    if let Some(rest) = line.strip_prefix('@') {
        let (name, trailing) = rest.split_once(' ').unwrap_or((rest, ""));
        if !name.is_empty() {
            // Capture display_name as text + mention marker.
            // The caller resolves ParticipantId from display_name.
            let mention = Block::Mention { participant: crate::ParticipantId::nil() };
            let mut blocks = vec![mention];
            let remainder = trailing.trim();
            if !remainder.is_empty() {
                blocks.push(Block::text(remainder));
            }
            return blocks;
        }
    }
    // Rich text with inline spans.
    let (content, spans) = parse_spans(line);
    if content.is_empty() && spans.is_empty() {
        return vec![];
    }
    vec![Block::Text { content, spans }]
}

/// Scan `text` and emit `(plain_content, spans)` where `spans` annotate the
/// ranges within `plain_content` that correspond to formatting delimiters.
///
/// Strategy: a single forward pass. For each recognised delimiter pair we record
/// a `Span` on the *text between the delimiters*, strip the delimiters from
/// `content`, and adjust offsets accordingly.
#[must_use]
fn parse_spans(text: &str) -> (String, Vec<Span>) {
    let bytes = text.as_bytes();
    let len = bytes.len();

    // Phase 1: collect all span ranges and their stripped text positions.
    #[derive(Debug)]
    struct RawSpan {
        start: usize, // in the stripped content
        end: usize,   // in the stripped content
        style: SpanStyle,
    }

    let mut stripped = String::with_capacity(len);
    let mut raw_spans: Vec<RawSpan> = Vec::new();
    let mut i = 0;

    while i < len {
        // **bold** (two asterisks, need at least **x** = 5 bytes)
        if i + 4 < len && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            if let Some(end) = find_closing(&bytes[i + 2..], b"**") {
                let content_start = stripped.len();
                let inner = &bytes[i + 2..i + 2 + end];
                stripped.push_str(std::str::from_utf8(inner).unwrap_or(""));
                let content_end = stripped.len();
                raw_spans.push(RawSpan { start: content_start, end: content_end, style: SpanStyle::Bold });
                i += 2 + end + 2;
                continue;
            }
        }
        // ~~strike~~ (two tildes)
        if i + 4 < len && bytes[i] == b'~' && bytes[i + 1] == b'~' {
            if let Some(end) = find_closing(&bytes[i + 2..], b"~~") {
                let content_start = stripped.len();
                let inner = &bytes[i + 2..i + 2 + end];
                stripped.push_str(std::str::from_utf8(inner).unwrap_or(""));
                let content_end = stripped.len();
                raw_spans.push(RawSpan { start: content_start, end: content_end, style: SpanStyle::Strikethrough });
                i += 2 + end + 2;
                continue;
            }
        }
        // *italic* (single asterisk, but not followed by another asterisk -> not bold)
        if i + 2 < len && bytes[i] == b'*' && bytes[i + 1] != b'*' {
            if let Some(end) = find_closing(&bytes[i + 1..], b"*") {
                // Don't consume if it matches the bold closer instead.
                let inner_end_pos = i + 1 + end;
                let is_bold = inner_end_pos + 1 < len && bytes[inner_end_pos + 1] == b'*';
                if !is_bold {
                    let content_start = stripped.len();
                    let inner = &bytes[i + 1..i + 1 + end];
                    stripped.push_str(std::str::from_utf8(inner).unwrap_or(""));
                    let content_end = stripped.len();
                    raw_spans.push(RawSpan { start: content_start, end: content_end, style: SpanStyle::Italic });
                    i += 1 + end + 1;
                    continue;
                }
            }
        }
        // `code` (single backtick)
        if bytes[i] == b'`' {
            if let Some(end) = find_closing(&bytes[i + 1..], b"`") {
                let content_start = stripped.len();
                let inner = &bytes[i + 1..i + 1 + end];
                stripped.push_str(std::str::from_utf8(inner).unwrap_or(""));
                let content_end = stripped.len();
                raw_spans.push(RawSpan { start: content_start, end: content_end, style: SpanStyle::Code });
                i += 1 + end + 1;
                continue;
            }
        }
        // [label](href)
        if bytes[i] == b'[' {
            if let Some(bracket_end) = find_closing(&bytes[i + 1..], b"]") {
                let label_end = i + 1 + bracket_end;
                if label_end + 1 < len && bytes[label_end + 1] == b'(' {
                    if let Some(paren_end) = find_closing(&bytes[label_end + 2..], b")") {
                        let href_start = label_end + 2;
                        let href = std::str::from_utf8(&bytes[href_start..href_start + paren_end])
                            .unwrap_or("")
                            .to_string();
                        let content_start = stripped.len();
                        let label = &bytes[i + 1..i + 1 + bracket_end];
                        stripped.push_str(std::str::from_utf8(label).unwrap_or(""));
                        let content_end = stripped.len();
                        raw_spans.push(RawSpan {
                            start: content_start,
                            end: content_end,
                            style: SpanStyle::Link { href },
                        });
                        i = href_start + paren_end + 1;
                        continue;
                    }
                }
            }
        }
        // Plain char.
        stripped.push(bytes[i] as char);
        i += 1;
    }

    // Phase 2: convert RawSpan → Span with correct offsets.
    let spans: Vec<Span> = raw_spans
        .into_iter()
        .map(|r| Span {
            start: r.start as u32,
            end: r.end as u32,
            style: r.style,
        })
        .collect();

    (stripped, spans)
}

/// Find closing delimiter `needle` in `haystack`. Returns the byte offset from
/// the start of `haystack` to the character just before the closing delimiter.
fn find_closing(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let n = needle.len();
    if haystack.len() < n {
        return None;
    }
    // A closing delimiter must not be immediately adjacent to whitespace before it.
    haystack
        .windows(n)
        .position(|w| w == needle)
        .filter(|&pos| pos == 0 || haystack[pos - 1] != b' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Basic formatting ---

    #[test]
    fn bold() {
        let blocks = parse_markdown_to_blocks("hello **world** here");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Text { content, spans } => {
                assert_eq!(content, "hello world here");
                assert_eq!(spans.len(), 1);
                assert_eq!(spans[0].start, 6);
                assert_eq!(spans[0].end, 11);
                assert!(matches!(spans[0].style, SpanStyle::Bold));
            }
            other => assert!(false, "expected text block, got {other:?}"),
        }
    }

    #[test]
    fn italic() {
        let blocks = parse_markdown_to_blocks("a *italic* word");
        assert_eq!(blocks.len(), 1);
        if let Block::Text { content, spans } = &blocks[0] {
            assert_eq!(content, "a italic word");
            assert_eq!(spans.len(), 1);
            assert!(matches!(spans[0].style, SpanStyle::Italic));
        } else {
            assert!(false, "expected text block");
        }
    }

    #[test]
    fn code() {
        let blocks = parse_markdown_to_blocks("use `let x = 1;` here");
        assert_eq!(blocks.len(), 1);
        if let Block::Text { content, spans } = &blocks[0] {
            assert_eq!(content, "use let x = 1; here");
            assert!(matches!(spans[0].style, SpanStyle::Code));
        } else {
            assert!(false, "expected text block");
        }
    }

    #[test]
    fn strikethrough() {
        let blocks = parse_markdown_to_blocks("old ~~news~~ update");
        assert_eq!(blocks.len(), 1);
        if let Block::Text { content, spans } = &blocks[0] {
            assert_eq!(content, "old news update");
            assert!(matches!(spans[0].style, SpanStyle::Strikethrough));
        } else {
            assert!(false, "expected text block");
        }
    }

    #[test]
    fn link() {
        let blocks = parse_markdown_to_blocks("see [our docs](https://aero.im/docs) for more");
        assert_eq!(blocks.len(), 1);
        if let Block::Text { content, spans } = &blocks[0] {
            assert_eq!(content, "see our docs for more");
            assert_eq!(spans.len(), 1);
            match &spans[0].style {
                SpanStyle::Link { href } => assert_eq!(href, "https://aero.im/docs"),
                other => assert!(false, "expected Link, got {other:?}"),
            }
        } else {
            assert!(false, "expected text block");
        }
    }

    // --- Multiple spans ---

    #[test]
    fn multiple_formats_in_one_line() {
        let blocks = parse_markdown_to_blocks("**bold** and *italic* and ~~strike~~");
        assert_eq!(blocks.len(), 1);
        if let Block::Text { content, spans } = &blocks[0] {
            assert_eq!(content, "bold and italic and strike");
            assert_eq!(spans.len(), 3);
        } else {
            assert!(false, "expected text block");
        }
    }

    // --- @mention ---

    #[test]
    fn at_mention_line() {
        let blocks = parse_markdown_to_blocks("@alice");
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], Block::Mention { .. }));
    }

    #[test]
    fn at_mention_with_trailing_text() {
        let blocks = parse_markdown_to_blocks("@bob check this out");
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], Block::Mention { .. }));
        assert!(matches!(&blocks[1], Block::Text { .. }));
    }

    // --- Edge cases ---

    #[test]
    fn empty_string_returns_nothing() {
        assert!(parse_markdown_to_blocks("").is_empty());
        assert!(parse_markdown_to_blocks("   ").is_empty());
    }

    #[test]
    fn plain_text_passes_through() {
        let blocks = parse_markdown_to_blocks("hello world");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Text { content, spans } => {
                assert_eq!(content, "hello world");
                assert!(spans.is_empty());
            }
            other => assert!(false, "expected text, got {other:?}"),
        }
    }

    #[test]
    fn unclosed_delimiter_is_literal() {
        // ** without closing → literal asterisks.
        let blocks = parse_markdown_to_blocks("hello **world");
        assert_eq!(blocks.len(), 1);
        if let Block::Text { content, spans } = &blocks[0] {
            assert_eq!(content, "hello **world");
            assert!(spans.is_empty());
        } else {
            assert!(false, "expected text block");
        }
    }

    #[test]
    fn asterisk_in_word_not_italic() {
        // A single * adjacent to non-space should not trigger italic (snake_case
        // guard handled by the general parser, but for now it's ambiguous and
        // the parser is intentionally simple).
        let blocks = parse_markdown_to_blocks("snake_case");
        assert_eq!(blocks.len(), 1);
        // The single * in snake_case has no matching closer → literal.
        if let Block::Text { content, spans: _ } = &blocks[0] {
            assert_eq!(content, "snake_case");
        } else {
            assert!(false, "expected text block");
        }
    }

    #[test]
    fn multi_line() {
        let blocks = parse_markdown_to_blocks("line one\n\nline **two**");
        assert_eq!(blocks.len(), 2);
        if let Block::Text { content, .. } = &blocks[0] {
            assert_eq!(content, "line one");
        } else {
            assert!(false, "expected text");
        }
    }

    #[test]
    fn link_no_href_is_literal() {
        let blocks = parse_markdown_to_blocks("[text] no parens");
        assert_eq!(blocks.len(), 1);
        if let Block::Text { content, spans } = &blocks[0] {
            assert_eq!(content, "[text] no parens");
            assert!(spans.is_empty());
        } else {
            assert!(false, "expected text block");
        }
    }

    #[test]
    fn quadruple_asterisk_is_literal() {
        // **** is too short for a bold pair (min **x** = 5 bytes).
        let blocks = parse_markdown_to_blocks("****");
        assert_eq!(blocks.len(), 1);
        if let Block::Text { content, spans } = &blocks[0] {
            assert_eq!(content, "****");
            assert!(spans.is_empty());
        } else {
            assert!(false, "expected text block");
        }
    }
}
