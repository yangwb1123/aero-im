//! AI writing assistant — rewrite a draft (adjust tone / make concise / expand).
//!
//! The "help me write" / Copilot-compose capability: hand the backend a draft
//! plus a target *style* and get back a rewritten version. This deliberately
//! reuses the exact same text-in / text-out seam that powers on-demand message
//! translation ([`AiBackend::translate`](crate::state::AiBackend::translate)) —
//! the style directive plays the role of the "target language" and the draft is
//! the source text. No new backend trait method, no storage, no migration.
//!
//! Degrades identically to `translate` / `summarize` / `ask`:
//! - With an LLM key configured the backend rewrites the draft.
//! - With a backend but **no** key, the backend echoes the source unchanged, so
//!   the route still returns a well-formed `200` (`rewritten == original`).
//! - With no AI service wired at all (`AppState.ai == None`) it returns
//!   `502 Upstream`, mirroring the other AI routes.

use aero_auth::AuthUser;
use aero_common::Error as AeroError;
use axum::{extract::State, routing::post, Json, Router};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the rewrite route, folded into the gateway router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/ai/rewrite", post(rewrite))
}

#[derive(Deserialize)]
struct RewriteReq {
    /// The draft to rewrite. Trimmed; an empty/blank draft is rejected (400).
    text: String,
    /// Optional target style. Absent/blank ⇒ [`RewriteStyle::DEFAULT`]; an
    /// unrecognized value is rejected (400).
    #[serde(default)]
    style: Option<String>,
}

/// Supported rewrite styles. The instruction prompt is derived purely from this
/// enum, so style validation and prompt construction are unit-tested offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RewriteStyle {
    /// Professional / formal tone (the default).
    Professional,
    /// Relaxed, conversational tone.
    Casual,
    /// Shorten — trim redundancy while keeping the meaning.
    Concise,
    /// Expand — add detail and structure.
    Expand,
    /// Warm, approachable tone.
    Friendly,
    /// Fix grammar / spelling / punctuation only, preserving tone.
    Fix,
}

impl RewriteStyle {
    /// Style applied when the request omits `style` (or sends a blank value).
    const DEFAULT: Self = Self::Professional;

    /// Parse a requested style. `None`/blank ⇒ [`Self::DEFAULT`]; an unknown
    /// non-blank value is rejected so a client typo surfaces as a `400` rather
    /// than being silently coerced. Matching is case-insensitive and trimmed.
    ///
    /// # Errors
    /// [`AeroError::Invalid`] when `raw` is a non-blank, unrecognized style.
    fn parse(raw: Option<&str>) -> Result<Self, AeroError> {
        let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(Self::DEFAULT);
        };
        match s.to_ascii_lowercase().as_str() {
            "professional" => Ok(Self::Professional),
            "casual" => Ok(Self::Casual),
            "concise" => Ok(Self::Concise),
            "expand" => Ok(Self::Expand),
            "friendly" => Ok(Self::Friendly),
            "fix" => Ok(Self::Fix),
            other => Err(AeroError::Invalid(format!("unknown rewrite style: {other}"))),
        }
    }

    /// Canonical wire label echoed back in the response.
    fn label(self) -> &'static str {
        match self {
            Self::Professional => "professional",
            Self::Casual => "casual",
            Self::Concise => "concise",
            Self::Expand => "expand",
            Self::Friendly => "friendly",
            Self::Fix => "fix",
        }
    }

    /// Instruction handed to the text-generation backend as the "target form".
    ///
    /// Phrased so it works through the translation seam: it names the desired
    /// output form, pins the output to the original language, and forbids any
    /// commentary so the reply is the rewritten draft and nothing else.
    fn directive(self) -> &'static str {
        match self {
            Self::Professional => {
                "把下面这段文字改写得更专业、正式、得体,保持原意和原语言,只输出改写后的文本本身"
            }
            Self::Casual => {
                "把下面这段文字改写得更轻松、口语化、自然,保持原意和原语言,只输出改写后的文本本身"
            }
            Self::Concise => {
                "在保持原意和原语言的前提下,把下面这段文字改写得更简洁精炼,删除冗余,只输出改写后的文本本身"
            }
            Self::Expand => {
                "在保持原意和原语言的前提下,把下面这段文字扩写得更详细、充分、有条理,只输出改写后的文本本身"
            }
            Self::Friendly => {
                "把下面这段文字改写得更友好、亲切、有温度,保持原意和原语言,只输出改写后的文本本身"
            }
            Self::Fix => {
                "修正下面这段文字的语法、拼写和标点错误,保持原意、原语言和原有语气,只输出修正后的文本本身"
            }
        }
    }
}

/// `POST /api/ai/rewrite` — rewrite `text` in the requested `style`.
///
/// Rejects an empty/blank draft (400) and an unknown style (400). Authenticated
/// but not room-scoped — the draft is supplied by the caller, not read from a
/// room. Returns the original alongside the rewrite so the client can diff/undo.
///
/// # Errors
/// - [`AeroError::Invalid`] (400) for an empty draft or unknown style.
/// - [`AeroError::Upstream`] (502) when no AI service is wired, or when the
///   backend call fails.
async fn rewrite(
    State(s): State<AppState>,
    _auth: AuthUser,
    Json(req): Json<RewriteReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let text = req.text.trim();
    if text.is_empty() {
        return Err(AeroError::Invalid("text must not be empty".into()).into());
    }
    let style = RewriteStyle::parse(req.style.as_deref())?;

    // Same backend seam as caption/message translation; `None` (no AI wired at
    // all) is a 502, matching the other AI routes. With a backend but no key,
    // `translate` echoes the source, so `rewritten == original` and we still 200.
    let ai = s
        .ai
        .as_ref()
        .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let rewritten = ai
        .translate(text, style.directive())
        .await
        .map_err(AeroError::Upstream)?;

    Ok(Json(serde_json::json!({
        "original": text,
        "rewritten": rewritten,
        "style": style.label(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [RewriteStyle; 6] = [
        RewriteStyle::Professional,
        RewriteStyle::Casual,
        RewriteStyle::Concise,
        RewriteStyle::Expand,
        RewriteStyle::Friendly,
        RewriteStyle::Fix,
    ];

    #[test]
    fn parse_defaults_to_professional_when_absent_or_blank() {
        assert_eq!(RewriteStyle::parse(None).unwrap(), RewriteStyle::DEFAULT);
        assert_eq!(RewriteStyle::parse(Some("")).unwrap(), RewriteStyle::DEFAULT);
        assert_eq!(RewriteStyle::parse(Some("   ")).unwrap(), RewriteStyle::DEFAULT);
        assert_eq!(RewriteStyle::DEFAULT, RewriteStyle::Professional);
    }

    #[test]
    fn parse_is_case_insensitive_and_trimmed() {
        assert_eq!(RewriteStyle::parse(Some("Casual")).unwrap(), RewriteStyle::Casual);
        assert_eq!(RewriteStyle::parse(Some(" CONCISE ")).unwrap(), RewriteStyle::Concise);
        assert_eq!(RewriteStyle::parse(Some("expand")).unwrap(), RewriteStyle::Expand);
        assert_eq!(RewriteStyle::parse(Some("friendly")).unwrap(), RewriteStyle::Friendly);
        assert_eq!(RewriteStyle::parse(Some("fix")).unwrap(), RewriteStyle::Fix);
    }

    #[test]
    fn parse_rejects_unknown_style() {
        let err = RewriteStyle::parse(Some("shakespeare")).unwrap_err();
        assert!(matches!(err, AeroError::Invalid(_)), "got {err:?}");
    }

    #[test]
    fn label_roundtrips_through_parse() {
        for style in ALL {
            assert_eq!(RewriteStyle::parse(Some(style.label())).unwrap(), style);
        }
    }

    #[test]
    fn every_style_has_a_distinct_nonempty_directive() {
        let mut seen = std::collections::HashSet::new();
        for style in ALL {
            let d = style.directive();
            assert!(!d.trim().is_empty(), "{} has empty directive", style.label());
            assert!(seen.insert(d), "{} shares a directive", style.label());
        }
    }
}
