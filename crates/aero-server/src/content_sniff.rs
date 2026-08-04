//! Magic-byte content sniffing for uploaded blobs (defence-in-depth).
//!
//! The upload MIME is whatever the client claims; [`crate::routes`]'s
//! `is_allowed_mime` only prefix-allowlists that *string*. So an attacker can
//! upload an executable or an HTML/SVG payload labelled `image/png` — stored
//! malware / stored-XSS that the blob-download endpoint would then serve to other
//! tenants. This module inspects the actual leading bytes:
//!
//! - any **executable** / object code / shebang script, or **markup**
//!   (HTML/SVG/XML/PHP), is rejected outright regardless of the claimed type
//!   (none of those is ever a legitimate upload here — and SVG is a stored-XSS
//!   vector when later served as `image/svg+xml`);
//! - a file claimed as a specific binary media / PDF / ZIP type must carry a
//!   matching signature. An *unrecognised* signature is allowed through, so we
//!   never false-reject a format we simply don't fingerprint — the executable /
//!   markup guard above is the real protection.
//!
//! Pure functions; unit-tested without any I/O.

/// Coarse content family inferred from a file's leading magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sniffed {
    Image,
    Video,
    Audio,
    /// ISO-BMFF (`ftyp`) or generic RIFF container shared by image/video/audio —
    /// consistent with any of those claims.
    MediaContainer,
    Pdf,
    Zip,
    /// Executable / object code / shebang script — never allowed.
    Executable,
    /// HTML / SVG / XML / PHP markup — a stored-XSS vector.
    Markup,
    /// No recognised signature (plain text, CSV, or a format we don't fingerprint).
    Unknown,
}

/// Infer the [`Sniffed`] family from a blob's leading bytes.
#[must_use]
pub fn sniff(b: &[u8]) -> Sniffed {
    // Executables / object code / scripts first (highest risk).
    if b.starts_with(b"MZ")                          // DOS/PE (.exe, .dll)
        || b.starts_with(&[0x7F, b'E', b'L', b'F'])  // ELF
        || b.starts_with(&[0xFE, 0xED, 0xFA, 0xCE])  // Mach-O 32 BE
        || b.starts_with(&[0xFE, 0xED, 0xFA, 0xCF])  // Mach-O 64 BE
        || b.starts_with(&[0xCE, 0xFA, 0xED, 0xFE])  // Mach-O 32 LE
        || b.starts_with(&[0xCF, 0xFA, 0xED, 0xFE])  // Mach-O 64 LE
        || b.starts_with(&[0xCA, 0xFE, 0xBA, 0xBE])  // Mach-O universal / Java class
        || b.starts_with(b"#!")
    // shebang script
    {
        return Sniffed::Executable;
    }
    if is_markup(b) {
        return Sniffed::Markup;
    }
    // Image signatures.
    if b.starts_with(&[0x89, b'P', b'N', b'G'])           // PNG
        || b.starts_with(&[0xFF, 0xD8, 0xFF])             // JPEG
        || b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")
        || b.starts_with(b"BM")                           // BMP
        || b.starts_with(&[0x49, 0x49, 0x2A, 0x00])       // TIFF (LE)
        || b.starts_with(&[0x4D, 0x4D, 0x00, 0x2A])
    // TIFF (BE)
    {
        return Sniffed::Image;
    }
    // RIFF container — distinguish by the form type at bytes 8..12.
    if b.len() >= 12 && b.starts_with(b"RIFF") {
        return match &b[8..12] {
            b"WEBP" => Sniffed::Image,
            b"WAVE" => Sniffed::Audio,
            b"AVI " => Sniffed::Video,
            _ => Sniffed::MediaContainer,
        };
    }
    // ISO-BMFF `ftyp` box (mp4/mov/m4a/heic/avif — image OR video OR audio).
    if b.len() >= 12 && &b[4..8] == b"ftyp" {
        return Sniffed::MediaContainer;
    }
    // Other audio.
    if b.starts_with(b"ID3")
        || b.starts_with(&[0xFF, 0xFB])
        || b.starts_with(&[0xFF, 0xF3])
        || b.starts_with(&[0xFF, 0xF2])
    {
        return Sniffed::Audio; // MP3
    }
    if b.starts_with(b"OggS") || b.starts_with(b"fLaC") {
        return Sniffed::Audio;
    }
    // Matroska / WebM.
    if b.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Sniffed::Video;
    }
    // Documents / archives.
    if b.starts_with(b"%PDF") {
        return Sniffed::Pdf;
    }
    if b.starts_with(&[0x50, 0x4B, 0x03, 0x04])
        || b.starts_with(&[0x50, 0x4B, 0x05, 0x06])
        || b.starts_with(&[0x50, 0x4B, 0x07, 0x08])
    {
        return Sniffed::Zip;
    }
    Sniffed::Unknown
}

/// Detect leading HTML/SVG/XML/PHP markup (after an optional UTF-8 BOM + ASCII
/// whitespace) — the stored-XSS surface.
fn is_markup(bytes: &[u8]) -> bool {
    let mut b = bytes;
    if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        b = &b[3..]; // UTF-8 BOM
    }
    let start = b
        .iter()
        .position(|c| !c.is_ascii_whitespace())
        .unwrap_or(b.len());
    let b = &b[start..];
    if b.first() != Some(&b'<') {
        return false;
    }
    let head: Vec<u8> = b.iter().take(16).map(u8::to_ascii_lowercase).collect();
    head.starts_with(b"<!doctype")
        || head.starts_with(b"<html")
        || head.starts_with(b"<svg")
        || head.starts_with(b"<?xml")
        || head.starts_with(b"<?php")
        || head.starts_with(b"<script")
        || head.starts_with(b"<!--")
}

/// The binary family a (prefix-allowlisted) claimed MIME belongs to, if its
/// content can be verified. `None` for text/csv/plain/octet-stream — no signature
/// is required of those.
fn claimed_family(mime: &str) -> Option<Sniffed> {
    if mime.starts_with("image/") {
        Some(Sniffed::Image)
    } else if mime.starts_with("video/") {
        Some(Sniffed::Video)
    } else if mime.starts_with("audio/") {
        Some(Sniffed::Audio)
    } else if mime == "application/pdf" {
        Some(Sniffed::Pdf)
    } else if mime == "application/zip" || mime == "application/x-zip-compressed" {
        Some(Sniffed::Zip)
    } else {
        None
    }
}

/// Whether a blob's actual `bytes` are acceptable for its (already prefix-
/// allowlisted) `claimed` MIME. See the module docs for the policy.
#[must_use]
pub fn is_consistent(claimed: &str, bytes: &[u8]) -> bool {
    let kind = sniff(bytes);
    // Executables and markup are never allowed, whatever the claim.
    if matches!(kind, Sniffed::Executable | Sniffed::Markup) {
        return false;
    }
    match claimed_family(claimed) {
        Some(Sniffed::Image) => {
            matches!(
                kind,
                Sniffed::Image | Sniffed::MediaContainer | Sniffed::Unknown
            )
        }
        Some(Sniffed::Video) => {
            matches!(
                kind,
                Sniffed::Video | Sniffed::MediaContainer | Sniffed::Unknown
            )
        }
        Some(Sniffed::Audio) => {
            matches!(
                kind,
                Sniffed::Audio | Sniffed::MediaContainer | Sniffed::Unknown
            )
        }
        Some(Sniffed::Pdf) => matches!(kind, Sniffed::Pdf | Sniffed::Unknown),
        Some(Sniffed::Zip) => matches!(kind, Sniffed::Zip | Sniffed::Unknown),
        // text / csv / plain / octet-stream carry no signature requirement (None);
        // the remaining `Sniffed` variants are never returned by `claimed_family`.
        None | Some(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{is_consistent, sniff, Sniffed};

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    const EXE: &[u8] = b"MZ\x90\x00\x03"; // PE header
    const HTML: &[u8] = b"<!DOCTYPE html><html><body>x</body></html>";
    const SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>";
    const PDF: &[u8] = b"%PDF-1.7\n%...";
    const ZIP: &[u8] = &[0x50, 0x4B, 0x03, 0x04, 0x14, 0x00];

    #[test]
    fn sniffs_basic_families() {
        assert_eq!(sniff(PNG), Sniffed::Image);
        assert_eq!(sniff(EXE), Sniffed::Executable);
        assert_eq!(sniff(HTML), Sniffed::Markup);
        assert_eq!(sniff(SVG), Sniffed::Markup);
        assert_eq!(sniff(PDF), Sniffed::Pdf);
        assert_eq!(sniff(ZIP), Sniffed::Zip);
        assert_eq!(sniff(b"\x00\x01\x02 random"), Sniffed::Unknown);
        assert_eq!(sniff(b"   \n  #!/bin/sh"), Sniffed::Unknown); // shebang only at very start
        assert_eq!(sniff(b"#!/bin/sh\necho hi"), Sniffed::Executable);
    }

    #[test]
    fn ftyp_and_riff_containers() {
        let mp4 = b"\x00\x00\x00\x18ftypmp42";
        assert_eq!(sniff(mp4), Sniffed::MediaContainer);
        let webp = b"RIFF\x00\x00\x00\x00WEBPVP8 ";
        assert_eq!(sniff(webp), Sniffed::Image);
        let wav = b"RIFF\x00\x00\x00\x00WAVEfmt ";
        assert_eq!(sniff(wav), Sniffed::Audio);
    }

    #[test]
    fn rejects_disguised_executables_and_markup() {
        // The headline threat: an exe or HTML/SVG labelled as an image.
        assert!(
            !is_consistent("image/png", EXE),
            "exe disguised as png rejected"
        );
        assert!(
            !is_consistent("image/png", HTML),
            "html disguised as png rejected"
        );
        assert!(
            !is_consistent("image/svg+xml", SVG),
            "svg (xss) rejected even when claimed"
        );
        assert!(
            !is_consistent("text/plain", EXE),
            "exe disguised as text rejected"
        );
        assert!(
            !is_consistent("application/octet-stream", HTML),
            "html as octet-stream rejected"
        );
    }

    #[test]
    fn rejects_cross_family_binary_mislabels() {
        assert!(
            !is_consistent("image/png", PDF),
            "pdf labelled png rejected"
        );
        assert!(
            !is_consistent("image/png", ZIP),
            "zip labelled png rejected"
        );
        assert!(
            !is_consistent("application/pdf", PNG),
            "png labelled pdf rejected"
        );
    }

    #[test]
    fn accepts_matching_and_unknown() {
        assert!(is_consistent("image/png", PNG));
        assert!(is_consistent("application/pdf", PDF));
        assert!(is_consistent("application/zip", ZIP));
        assert!(
            is_consistent("video/mp4", b"\x00\x00\x00\x18ftypisom"),
            "ftyp ok for video"
        );
        assert!(
            is_consistent("image/heic", b"\x00\x00\x00\x18ftypheic"),
            "ftyp ok for image"
        );
        // Unknown signature passes (avoid false-rejecting unfingerprinted formats).
        assert!(is_consistent("image/png", b"\x00\x01\x02 unrecognised"));
        // Text / csv carry no signature requirement.
        assert!(is_consistent("text/csv", b"a,b,c\n1,2,3"));
        assert!(is_consistent("text/plain", b"just some words"));
    }
}
