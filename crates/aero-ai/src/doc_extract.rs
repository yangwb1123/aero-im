//! Dependency-light text extraction for office documents (OOXML) and, best-effort,
//! PDF — so the agent's `read_attachment` tool (方向三 file RAG) can read the
//! *content* of `.docx`/`.xlsx`/`.pptx`/`.pdf` attachments, not just plain text.
//!
//! ## Why hand-rolled
//!
//! An OOXML file is a ZIP of XML parts; a PDF is objects with (often
//! DEFLATE-compressed) content streams. Both only need DEFLATE — which is already
//! in the dependency tree via `flate2` (pure-Rust `miniz_oxide` backend) — plus a
//! small, **defensive** container parser. We deliberately avoid a full XML parser
//! (so there is no XML-external-entity / billion-laughs surface) and a full PDF
//! library: text is pulled by tag-stripping (OOXML) / operator-scanning (PDF).
//!
//! ## Safety posture (input is attacker-controlled upload bytes)
//!
//! - **No panics on malformed input**: every slice is a checked `.get(..)`; a bad
//!   offset/length yields `None`, never an index panic (which would be a `DoS`).
//! - **Decompression bound**: inflate is capped (`Read::take`) so a zip-bomb /
//!   over-large content stream can't exhaust memory — the compressed input is
//!   itself already capped by the caller (256 KiB), and we additionally cap the
//!   *decompressed* total.
//! - **No entity expansion / no external fetch**: the XML text walker only copies
//!   character data and decodes the five predefined entities + numeric refs.
//!
//! All extractors are pure (`&[u8] -> Option<String>`) and unit-tested.

use std::io::Read as _;

/// Cap on total decompressed bytes we will materialize from one document, so a
/// highly-compressible bomb can't blow up memory. 8 MiB is plenty for the text of
/// any reasonable document while staying bounded.
pub const MAX_DECOMPRESSED_BYTES: usize = 8 * 1024 * 1024;

/// Sniff the leading magic bytes and route to the right extractor; returns the
/// document's text, or `None` when it isn't a recognized/extractable document
/// (the caller then falls back to plain-text decoding). Pure.
#[must_use]
pub fn extract_document_text(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        extract_office_text(bytes)
    } else if bytes.starts_with(b"%PDF-") {
        extract_pdf_text(bytes)
    } else {
        None
    }
}

// ----------------------------------------------------------- OOXML (ZIP of XML)

/// Read a little-endian u16 at `off`, bounds-checked.
fn le_u16(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

/// Read a little-endian u32 at `off`, bounds-checked.
fn le_u32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// One ZIP central-directory entry we care about: where its local data is and how
/// it's stored.
struct ZipEntry {
    name: String,
    method: u16,
    compressed_size: usize,
    local_header_offset: usize,
}

/// Locate the End-Of-Central-Directory record and return `(cd_offset, entry_count)`.
/// Scans backward for the signature so a trailing ZIP comment doesn't defeat it.
fn find_eocd(b: &[u8]) -> Option<(usize, usize)> {
    const SIG: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];
    if b.len() < 22 {
        return None;
    }
    let max_back = b.len().saturating_sub(22 + 0xFFFF);
    let mut i = b.len() - 22;
    loop {
        if b.get(i..i + 4) == Some(&SIG) {
            let cd_offset = le_u32(b, i + 16)? as usize;
            let count = le_u16(b, i + 10)? as usize;
            return Some((cd_offset, count));
        }
        if i == 0 || i <= max_back {
            return None;
        }
        i -= 1;
    }
}

/// Walk the central directory, returning the entries whose names pass `want`.
fn central_directory(b: &[u8], want: impl Fn(&str) -> bool) -> Vec<ZipEntry> {
    const SIG: [u8; 4] = [0x50, 0x4B, 0x01, 0x02];
    let mut out = Vec::new();
    let Some((cd_off, count)) = find_eocd(b) else {
        return out;
    };
    let mut off = cd_off;
    for _ in 0..count {
        if b.get(off..off + 4) != Some(&SIG) {
            break;
        }
        let (
            Some(method),
            Some(comp),
            Some(name_len),
            Some(extra_len),
            Some(comment_len),
            Some(lho),
        ) = (
            le_u16(b, off + 10),
            le_u32(b, off + 20),
            le_u16(b, off + 28),
            le_u16(b, off + 30),
            le_u16(b, off + 32),
            le_u32(b, off + 42),
        )
        else {
            break;
        };
        let name_start = off + 46;
        let name_end = name_start + name_len as usize;
        let name = b
            .get(name_start..name_end)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();
        if want(&name) {
            out.push(ZipEntry {
                name,
                method,
                compressed_size: comp as usize,
                local_header_offset: lho as usize,
            });
        }
        // Advance to the next central-directory header.
        off = name_end + extra_len as usize + comment_len as usize;
    }
    out
}

/// Resolve a central-directory entry's compressed data slice via its local header
/// (the local header's own name/extra lengths give the exact data offset).
fn entry_data<'a>(b: &'a [u8], e: &ZipEntry) -> Option<&'a [u8]> {
    const SIG: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];
    let lho = e.local_header_offset;
    if b.get(lho..lho + 4) != Some(&SIG) {
        return None;
    }
    let name_len = le_u16(b, lho + 26)? as usize;
    let extra_len = le_u16(b, lho + 28)? as usize;
    let data_start = lho + 30 + name_len + extra_len;
    b.get(data_start..data_start + e.compressed_size)
}

/// Inflate (or copy, for stored entries) one entry's bytes, capped at `cap`.
fn inflate_entry(data: &[u8], method: u16, cap: usize) -> Option<Vec<u8>> {
    match method {
        0 => Some(data.get(..data.len().min(cap))?.to_vec()), // stored
        8 => {
            // ZIP uses raw DEFLATE (no zlib header).
            let mut out = Vec::new();
            flate2::read::DeflateDecoder::new(data)
                .take(cap as u64)
                .read_to_end(&mut out)
                .ok()?;
            Some(out)
        }
        _ => None, // unsupported method (bzip2/lzma/…) — skip this part
    }
}

/// True for the OOXML parts that carry user-visible text across docx/xlsx/pptx.
/// (OOXML part names are spec-defined and always lowercase `.xml`, so the
/// case-sensitive suffix checks are correct here.)
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn is_text_part(name: &str) -> bool {
    name == "word/document.xml"
        || name.starts_with("word/header") && name.ends_with(".xml")
        || name.starts_with("word/footer") && name.ends_with(".xml")
        || name == "xl/sharedStrings.xml"
        || name.starts_with("ppt/slides/slide") && name.ends_with(".xml")
        || name.starts_with("ppt/notesSlides/notesSlide") && name.ends_with(".xml")
}

/// Extract text from an OOXML (docx/xlsx/pptx) ZIP. Concatenates the text of the
/// relevant parts in name order. `None` if it isn't a readable OOXML package or no
/// text was found.
#[must_use]
pub fn extract_office_text(bytes: &[u8]) -> Option<String> {
    let mut entries = central_directory(bytes, is_text_part);
    if entries.is_empty() {
        return None;
    }
    // Deterministic order (e.g. slide1, slide2, …) for stable output.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut budget = MAX_DECOMPRESSED_BYTES;
    let mut text = String::new();
    for e in &entries {
        if budget == 0 {
            break;
        }
        let Some(data) = entry_data(bytes, e) else {
            continue;
        };
        let Some(xml) = inflate_entry(data, e.method, budget) else {
            continue;
        };
        budget = budget.saturating_sub(xml.len());
        let part = xml_to_text(&xml);
        if !part.trim().is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(part.trim_end());
        }
    }
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Pull human text out of an OOXML XML part WITHOUT a real XML parser (so there is
/// no entity-expansion / external-entity surface). Character data inside any
/// element whose *local* name is `t` (docx `w:t`, xlsx `t`, pptx `a:t`) is the
/// visible text; paragraph/break/tab elements (`p`/`br`/`cr`/`tab`) become
/// whitespace so words and lines don't run together. The five predefined entities
/// and numeric refs are decoded.
fn xml_to_text(xml: &[u8]) -> String {
    let s = String::from_utf8_lossy(xml);
    let bytes = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut in_text = false; // inside a <*:t> element
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Skip comments / CDATA-aware-enough / processing instructions wholesale
            // by finding the matching '>'. (CDATA inside <*:t> is vanishingly rare in
            // OOXML; treating it as markup just drops it, which is safe.)
            let Some(rel) = s[i..].find('>') else { break };
            let tag = &s[i + 1..i + rel];
            i += rel + 1;
            // Determine tag kind + local name.
            let is_close = tag.starts_with('/');
            // A self-closing tag (`<w:t/>`) ends with '/' and has no content; it must
            // not toggle `in_text` on (else sibling chardata is over-captured).
            let is_self_closing = tag.ends_with('/');
            let raw = tag.trim_start_matches('/');
            // local name = after optional "prefix:", up to whitespace or '/'.
            let local = raw
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or("")
                .rsplit(':')
                .next()
                .unwrap_or("");
            match local {
                // open → collect; close → stop; self-closing `<w:t/>` → no-op (empty).
                "t" if !is_self_closing => in_text = !is_close,
                "t" => {}
                "p" | "br" | "cr" => {
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                "tab" => out.push('\t'),
                _ => {}
            }
        } else {
            // Character data: collect only when inside a text element.
            let Some(rel) = s[i..].find('<') else {
                if in_text {
                    push_decoded(&mut out, &s[i..]);
                }
                break;
            };
            if in_text {
                push_decoded(&mut out, &s[i..i + rel]);
            }
            i += rel;
        }
    }
    out
}

/// Append `frag` to `out`, decoding the five predefined XML entities and numeric
/// character references. Unknown entities are passed through verbatim.
fn push_decoded(out: &mut String, frag: &str) {
    let mut rest = frag;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        if let Some(semi) = after.find(';').filter(|&semi| semi <= 10) {
            let ent = &after[1..semi];
            let decoded = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => decode_numeric(ent),
            };
            if let Some(c) = decoded {
                out.push(c);
                rest = &after[semi + 1..];
                continue;
            }
        }
        // Not a recognizable entity — emit the '&' literally and move on.
        out.push('&');
        rest = &after[1..];
    }
    out.push_str(rest);
}

/// Decode a numeric character reference body (`#123` or `#x1F`) to a char.
fn decode_numeric(ent: &str) -> Option<char> {
    let num = ent.strip_prefix('#')?;
    let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        num.parse::<u32>().ok()?
    };
    char::from_u32(code)
}

// --------------------------------------------------------------- PDF (best effort)

/// Best-effort PDF text extraction (no font-aware library): inflate `FlateDecode`
/// content streams and pull literal-string operands. Handles the common
/// Word/LibreOffice-exported case; **misses** encrypted PDFs, CID/Type0
/// custom-encoded fonts (would yield glyph codes), and object/xref streams. To
/// avoid feeding the model garbage, the result is rejected (→ `None`) when it
/// looks mostly non-textual. `None` therefore means "couldn't confidently extract".
#[must_use]
pub fn extract_pdf_text(bytes: &[u8]) -> Option<String> {
    let mut budget = MAX_DECOMPRESSED_BYTES;
    let mut text = String::new();
    let mut search_from = 0;
    while let Some(rel) = find_subslice(&bytes[search_from..], b"stream") {
        let stream_kw = search_from + rel;
        // Data starts after "stream" + an EOL (CRLF or LF).
        let mut data_start = stream_kw + 6;
        if bytes.get(data_start) == Some(&b'\r') {
            data_start += 1;
        }
        if bytes.get(data_start) == Some(&b'\n') {
            data_start += 1;
        }
        let Some(end_rel) = find_subslice(&bytes[data_start..], b"endstream") else {
            break;
        };
        let stream_data = &bytes[data_start..data_start + end_rel];
        search_from = data_start + end_rel + 9;

        // Was this stream FlateDecode? Look in the dict just before the keyword.
        let dict_start = stream_kw.saturating_sub(200);
        let dict = &bytes[dict_start..stream_kw];
        let content = if find_subslice(dict, b"FlateDecode").is_some() {
            // PDF FlateDecode is zlib-wrapped (has the 2-byte header).
            let mut out = Vec::new();
            if flate2::read::ZlibDecoder::new(stream_data)
                .take(budget as u64)
                .read_to_end(&mut out)
                .is_ok()
                && !out.is_empty()
            {
                out
            } else {
                continue;
            }
        } else if find_subslice(dict, b"/Filter").is_none() {
            stream_data.to_vec() // unfiltered content stream
        } else {
            continue; // some other filter we don't handle
        };
        budget = budget.saturating_sub(content.len());
        extract_pdf_strings(&content, &mut text);
        if budget == 0 {
            break;
        }
    }
    let text = collapse_ws(&text);
    // Quality gate: require the extraction to look like real text.
    if text.len() < 2 || !looks_textual(&text) {
        return None;
    }
    Some(text)
}

/// Pull literal `(...)` string operands out of a content stream, appending a space
/// after each text-show and a newline on positioning ops, so words/lines separate.
fn extract_pdf_strings(content: &[u8], out: &mut String) {
    let mut i = 0;
    while i < content.len() {
        match content[i] {
            b'(' => {
                i += 1;
                let mut depth = 1;
                while i < content.len() && depth > 0 {
                    match content[i] {
                        b'\\' => {
                            // Escape: handle \n \r \t \( \) \\ and octal \ddd.
                            if let Some(&c) = content.get(i + 1) {
                                match c {
                                    b'n' => out.push('\n'),
                                    b'r' => {}
                                    b't' => out.push('\t'),
                                    b'(' => out.push('('),
                                    b')' => out.push(')'),
                                    b'\\' => out.push('\\'),
                                    b'0'..=b'7' => {
                                        // up to 3 octal digits
                                        let mut val = 0u32;
                                        let mut k = 0;
                                        while k < 3 {
                                            match content.get(i + 1 + k) {
                                                Some(d @ b'0'..=b'7') => {
                                                    val = val * 8 + u32::from(d - b'0');
                                                    k += 1;
                                                }
                                                _ => break,
                                            }
                                        }
                                        if let Some(ch) = char::from_u32(val) {
                                            out.push(ch);
                                        }
                                        i += 1 + k;
                                        continue;
                                    }
                                    _ => out.push(c as char),
                                }
                                i += 2;
                                continue;
                            }
                            i += 1;
                        }
                        b'(' => {
                            depth += 1;
                            out.push('(');
                            i += 1;
                        }
                        b')' => {
                            depth -= 1;
                            if depth > 0 {
                                out.push(')');
                            }
                            i += 1;
                        }
                        c => {
                            out.push(c as char);
                            i += 1;
                        }
                    }
                }
                out.push(' ');
            }
            // Positioning operators that imply a new line of text.
            b'T' if matches!(content.get(i + 1), Some(b'*')) => {
                out.push('\n');
                i += 2;
            }
            _ => i += 1,
        }
    }
}

/// Heuristic: does `s` look like extracted prose rather than glyph-code noise?
/// True when a strong majority of chars are printable ASCII/whitespace or common
/// letters. Guards against emitting gibberish for font-encoded PDFs.
fn looks_textual(s: &str) -> bool {
    let total = s.chars().count().max(1);
    let good = s
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || c.is_ascii_punctuation())
        .count();
    good * 100 / total >= 80
}

/// Collapse runs of blank lines / trailing whitespace for tidy output.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_nl = false;
    for line in s.split('\n') {
        let t = line.trim_end();
        if t.is_empty() {
            if !prev_nl && !out.is_empty() {
                out.push('\n');
                prev_nl = true;
            }
            continue;
        }
        out.push_str(t);
        out.push('\n');
        prev_nl = false;
    }
    out.trim().to_string()
}

/// First index of `needle` in `hay` (small, allocation-free).
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

#[cfg(test)]
// Test-only: ZIP byte construction casts tiny lengths to u16/u32 (never truncates
// at these sizes), and some raw-string literals carry no internal quotes.
#[allow(clippy::cast_possible_truncation, clippy::needless_raw_string_hashes)]
mod tests {
    use super::*;
    use flate2::{write::DeflateEncoder, Compression};
    use std::io::Write as _;

    // ---- ZIP builder (test helper): assemble a minimal ZIP from named parts. ----

    fn deflate(data: &[u8]) -> Vec<u8> {
        let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// Build a ZIP. Each part is stored DEFLATE'd (method 8) so the inflate path is
    /// exercised. Only the fields our parser reads are populated.
    fn make_zip(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        let mut offsets = Vec::new();
        for (name, data) in parts {
            let comp = deflate(data);
            let lho = out.len() as u32;
            offsets.push(lho);
            // Local file header.
            out.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]);
            out.extend_from_slice(&[0x14, 0x00]); // version
            out.extend_from_slice(&[0x00, 0x00]); // flags
            out.extend_from_slice(&[0x08, 0x00]); // method = deflate
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // time/date
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // crc (unchecked)
            out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&[0x00, 0x00]); // extra len
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&comp);
        }
        let cd_offset = out.len() as u32;
        for ((name, data), lho) in parts.iter().zip(offsets) {
            let comp = deflate(data);
            central.extend_from_slice(&[0x50, 0x4B, 0x01, 0x02]);
            central.extend_from_slice(&[0x14, 0x00, 0x14, 0x00]); // versions
            central.extend_from_slice(&[0x00, 0x00]); // flags
            central.extend_from_slice(&[0x08, 0x00]); // method
            central.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // time/date
            central.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // crc
            central.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0x00, 0x00]); // extra
            central.extend_from_slice(&[0x00, 0x00]); // comment
            central.extend_from_slice(&[0x00, 0x00]); // disk
            central.extend_from_slice(&[0x00, 0x00]); // internal attrs
            central.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // external attrs
            central.extend_from_slice(&lho.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_size = central.len() as u32;
        let count = parts.len() as u16;
        out.extend_from_slice(&central);
        // EOCD.
        out.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06]);
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // disk numbers
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&[0x00, 0x00]); // comment len
        out
    }

    #[test]
    fn docx_body_text_is_extracted_with_runs_joined_and_paragraphs_split() {
        // A word can be split across runs ("Hel"+"lo"); paragraphs separate.
        let doc = br#"<?xml version="1.0"?><w:document><w:body>
            <w:p><w:r><w:t>Hel</w:t></w:r><w:r><w:t xml:space="preserve">lo</w:t></w:r></w:p>
            <w:p><w:r><w:t>second &amp; line</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let zip = make_zip(&[("word/document.xml", doc)]);
        let text = extract_office_text(&zip).expect("docx text");
        assert!(text.contains("Hello"), "split runs join: {text:?}");
        assert!(
            text.contains("second & line"),
            "entity decoded + run kept: {text:?}"
        );
        // Two paragraphs ⇒ a line break between them.
        assert!(
            text.contains("Hello\nsecond"),
            "paragraph break inserted: {text:?}"
        );
    }

    #[test]
    fn xlsx_shared_strings_and_pptx_slides_extract() {
        let xlsx = make_zip(&[(
            "xl/sharedStrings.xml",
            br#"<sst><si><t>Revenue</t></si><si><t>Q3 total</t></si></sst>"#,
        )]);
        let t = extract_office_text(&xlsx).expect("xlsx");
        assert!(t.contains("Revenue") && t.contains("Q3 total"), "{t:?}");

        let pptx = make_zip(&[(
            "ppt/slides/slide1.xml",
            br#"<p:sld><a:p><a:r><a:t>Slide One</a:t></a:r></a:p></p:sld>"#,
        )]);
        let t = extract_office_text(&pptx).expect("pptx");
        assert!(t.contains("Slide One"), "{t:?}");
    }

    #[test]
    fn office_multiple_slides_ordered_and_dispatch_by_magic() {
        let pptx = make_zip(&[
            ("ppt/slides/slide2.xml", br#"<a:t>Two</a:t>"#),
            ("ppt/slides/slide1.xml", br#"<a:t>One</a:t>"#),
            ("docProps/core.xml", br#"<title>ignored</title>"#),
        ]);
        // Routed via the magic-byte dispatcher, slides come out in name order.
        let t = extract_document_text(&pptx).expect("pptx via dispatch");
        let one = t.find("One").expect("has One");
        let two = t.find("Two").expect("has Two");
        assert!(one < two, "slide1 before slide2: {t:?}");
        assert!(!t.contains("ignored"), "non-text part skipped: {t:?}");
    }

    #[test]
    fn malformed_zip_returns_none_never_panics() {
        assert_eq!(extract_office_text(b"PK\x03\x04 garbage truncated"), None);
        assert_eq!(extract_office_text(b""), None);
        assert_eq!(extract_office_text(&[0x50, 0x4B, 0x05, 0x06]), None);
        // Truncated EOCD-ish tail.
        assert_eq!(extract_document_text(b"PK\x03\x04\x00\x00\x00"), None);
    }

    #[test]
    fn xml_to_text_decodes_entities_and_numeric_refs() {
        let xml = br#"<w:p><w:t>a&lt;b &#65;&#x42; &amp; c</w:t></w:p>"#;
        let t = xml_to_text(xml);
        assert!(t.contains("a<b AB & c"), "{t:?}");
    }

    #[test]
    fn xml_to_text_self_closing_text_element_does_not_overcapture() {
        // A self-closing <w:t/> is empty; it must NOT flip into text mode and slurp
        // the following non-text markup/whitespace as if it were inside <w:t>.
        let xml = br#"<w:p><w:t/></w:p><w:p><w:r><w:rPr/></w:r><w:t>real</w:t></w:p>"#;
        let t = xml_to_text(xml);
        assert!(t.contains("real"), "real text captured: {t:?}");
        // The <w:rPr/> run-properties element must not appear as captured text.
        assert!(!t.contains("rPr"), "no markup leaked into text: {t:?}");
    }

    #[test]
    fn pdf_extracts_uncompressed_text_operands() {
        // A minimal PDF-ish blob: an unfiltered content stream with Tj/TJ literals.
        let pdf = b"%PDF-1.4\n1 0 obj<<>>\nstream\nBT (Hello) Tj T* [(Wor)-10(ld)] TJ ET\nendstream endobj\n";
        let t = extract_pdf_text(pdf).expect("pdf text");
        assert!(t.contains("Hello"), "{t:?}");
        assert!(
            t.contains("Wor") && t.contains("ld"),
            "TJ array parts: {t:?}"
        );
    }

    #[test]
    fn pdf_extracts_flate_compressed_stream() {
        // zlib-wrap a content stream (PDF FlateDecode is zlib, not raw deflate).
        let content = b"BT (Compressed body) Tj ET";
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(content).unwrap();
        let z = enc.finish().unwrap();
        let mut pdf = b"%PDF-1.5\n2 0 obj<</Filter /FlateDecode>>\nstream\n".to_vec();
        pdf.extend_from_slice(&z);
        pdf.extend_from_slice(b"\nendstream endobj\n");
        let t = extract_pdf_text(&pdf).expect("flate pdf");
        assert!(t.contains("Compressed body"), "{t:?}");
    }

    #[test]
    fn pdf_rejects_gibberish_and_non_pdf() {
        // Non-PDF magic → dispatcher declines.
        assert_eq!(extract_document_text(b"not a document"), None);
        // A PDF whose only "text" is glyph-code noise fails the quality gate.
        let pdf = b"%PDF-1.4\nstream\nBT (\x01\x02\x03\x04\x05\x06\x07\x08) Tj ET\nendstream\n";
        assert_eq!(extract_pdf_text(pdf), None, "gibberish rejected");
    }

    #[test]
    fn octal_escapes_in_pdf_strings_decode() {
        // \101 == 'A', \102 == 'B'.
        let pdf = b"%PDF-1.4\nstream\nBT (\\101\\102C) Tj ET\nendstream\n";
        let t = extract_pdf_text(pdf).expect("octal pdf");
        assert!(t.contains("ABC"), "{t:?}");
    }
}
