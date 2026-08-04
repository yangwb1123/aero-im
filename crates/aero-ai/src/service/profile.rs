//! Persistent cross-room AI user-profile extraction + use (持久跨房 AI 用户画像).
//!
//! This is the OPT-IN, GDPR-safe entrypoint that turns a participant's cross-room
//! message history into a durable [`aero_storage::AiProfile`] the answer /
//! recommendation paths can read to personalise a reply.
//!
//! ## Privacy posture (隐私第一) — four hard gates
//!
//! 1. **Opt-in, default OFF.** Every public method here short-circuits to a no-op
//!    unless [`cross_room_profile_enabled`] returns `true`, i.e. the operator set
//!    `AERO_AI_CROSS_ROOM_PROFILE=1`. A fresh deploy therefore never extracts,
//!    stores, or reads a profile — the table stays empty and the personalised
//!    paths behave exactly as before.
//! 2. **GDPR-erasable.** The store is keyed by `participant_id` and is wired into
//!    `ParticipantRepo::delete_participant`'s explicit erasure DELETE list, so a
//!    right-to-erasure request removes the profile (the FK cascade does not fire
//!    on a tombstone — see the storage module docs).
//! 3. **Tenant-isolated.** Profiles are keyed and read by both participant and
//!    workspace. A hint extracted from one tenant can never enter another
//!    tenant's prompt; workspace deletion cascades its derived profile rows.
//! 4. **Transparent.** The extracted fields are plain readable data
//!    (`topics` array, `preferences` object, a short `summary`), never an opaque
//!    vector, so the subject can be shown exactly what is stored.
//!
//! ## Quality (honestly labelled)
//!
//! With an Anthropic key the extraction asks the model for a structured profile.
//! WITHOUT a key it degrades to a deterministic keyword heuristic — quality is
//! **staging-gated** (good enough to exercise the path, not a production-grade
//! profile). The use path is CONSERVATIVE: it only prepends a short profile hint
//! to the system prompt; it never changes retrieval or what the model is allowed
//! to see.

use std::collections::BTreeMap;

use aero_common::{Message, ParticipantId, WorkspaceId};
use serde_json::Value;

use crate::anthropic::ChatMsg;
use crate::error::Result;
use crate::service::tools::render_transcript;
use crate::service::AiService;

/// Env flag that opts a deployment INTO persistent cross-room AI profiling.
///
/// Default OFF: when unset (or not a truthy `1`/`true`), every profile path is a
/// no-op. This is the master privacy gate for the whole feature.
pub const CROSS_ROOM_PROFILE_ENV: &str = "AERO_AI_CROSS_ROOM_PROFILE";

/// Whether persistent cross-room AI profiling is opted-in for this deployment.
///
/// Reads [`CROSS_ROOM_PROFILE_ENV`]; truthy values are `1` / `true`
/// (case-insensitive). Anything else — including unset — is OFF. Pure wrt its
/// argument so the gate is unit-testable without touching the process env.
#[must_use]
pub fn flag_is_enabled(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some(v) if v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Process-env reading of [`flag_is_enabled`] — `true` iff the operator opted in
/// via `AERO_AI_CROSS_ROOM_PROFILE`. Default OFF.
#[must_use]
pub fn cross_room_profile_enabled() -> bool {
    flag_is_enabled(std::env::var(CROSS_ROOM_PROFILE_ENV).ok().as_deref())
}

/// A structured profile extracted from a participant's messages.
///
/// Mirrors the storage shape but in typed form so extraction is unit-testable
/// without a database. `topics` is a small ordered set of recurring keywords;
/// `preferences` free-form key/value hints; `summary` a short digest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractedProfile {
    pub topics: Vec<String>,
    pub preferences: BTreeMap<String, String>,
    pub summary: String,
}

impl ExtractedProfile {
    /// `true` when nothing meaningful was extracted (no topics, no preferences,
    /// blank summary). An empty extraction is never persisted — opt-in does not
    /// mean "write an empty row".
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.topics.is_empty() && self.preferences.is_empty() && self.summary.trim().is_empty()
    }

    /// `topics` as a JSON array value (storage shape).
    #[must_use]
    pub fn topics_json(&self) -> Value {
        Value::Array(self.topics.iter().cloned().map(Value::String).collect())
    }

    /// `preferences` as a JSON object value (storage shape).
    #[must_use]
    pub fn preferences_json(&self) -> Value {
        Value::Object(
            self.preferences
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        )
    }
}

/// Max messages folded into a single extraction pass — bounds the prompt size and
/// the heuristic's work.
const EXTRACT_MAX_MESSAGES: i64 = 80;
/// Max recurring topics kept (most frequent first).
const MAX_TOPICS: usize = 8;
/// Minimum times a keyword must recur to count as a "topic" (heuristic path).
const MIN_TOPIC_FREQ: usize = 3;

pub(crate) const PROFILE_SYSTEM_PROMPT: &str = "\
你是一个用户画像抽取器,服务于企业协作 IM 的 AI 助手。请阅读某位用户跨多个频道的发言,\
抽取一个简短、克制、不含敏感个人隐私(不要推断健康、政治、宗教、性取向等)的画像,仅用于\
让助手更贴合其表达习惯与关注点。\n\
只输出三行,严格按以下格式(不要多余文字):\n\
TOPICS: 逗号分隔的 3-8 个其经常讨论的主题关键词\n\
PREFERENCES: 逗号分隔的 key=value 偏好(如 tone=concise, language=zh),无则留空\n\
SUMMARY: 一句话中文概括(不超过 40 字)";

impl AiService {
    /// OPT-IN extraction entrypoint (持久跨房 AI 用户画像).
    ///
    /// Reads up to [`EXTRACT_MAX_MESSAGES`] of `participant`'s recent cross-room
    /// messages within `workspace`, extracts a [`ExtractedProfile`] (LLM when an
    /// Anthropic key is configured, else a deterministic heuristic), and upserts
    /// it into the profile store.
    ///
    /// **No-op unless opted in.** Returns `Ok(None)` — touching neither the LLM
    /// nor the store — when any gate is closed:
    ///   * `AERO_AI_CROSS_ROOM_PROFILE` is not set (the master gate), OR
    ///   * no profile store is wired, OR
    ///   * the participant has no extractable messages, OR
    ///   * extraction yielded nothing meaningful (an empty profile is not stored).
    ///
    /// On success returns `Ok(Some(profile))` and the row is persisted.
    ///
    /// # Errors
    /// Propagates storage failures (message read / upsert) and, when a key is
    /// configured, an Anthropic failure. The no-key heuristic path is infallible.
    pub async fn extract_and_store_profile(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
    ) -> Result<Option<aero_storage::AiProfile>> {
        // Gate 1 (master): opt-in only.
        if !cross_room_profile_enabled() {
            return Ok(None);
        }
        // Gate 2: store must be wired.
        let Some(store) = self.ai_profiles.as_ref() else {
            return Ok(None);
        };

        // Read the participant's own recent cross-room messages over the SAME
        // membership/workspace boundary the workspace RAG uses — so extraction can
        // never see anything the participant could not.
        let mut recent = self
            .messages
            .recent_workspace(participant, workspace, EXTRACT_MAX_MESSAGES)
            .await?;
        recent.reverse(); // chronological for the LLM / heuristic
                          // Keep only the participant's OWN authored lines — a profile of THEM, not
                          // of everyone they talk to.
        recent.retain(|m| m.sender_id == participant);
        if recent.is_empty() {
            return Ok(None);
        }

        let extracted = self.extract_profile_from(workspace, &recent).await?;
        if extracted.is_empty() {
            return Ok(None);
        }

        let stored = store
            .upsert(
                participant,
                workspace,
                &extracted.topics_json(),
                &extracted.preferences_json(),
                &extracted.summary,
            )
            .await?;
        Ok(Some(stored))
    }

    /// Extract a profile from a chronological slice of the participant's own
    /// messages. LLM path when an Anthropic key is configured; deterministic
    /// heuristic otherwise. Internal to the opt-in entrypoint.
    async fn extract_profile_from(
        &self,
        workspace: WorkspaceId,
        messages: &[Message],
    ) -> Result<ExtractedProfile> {
        let transcript = render_transcript(messages);
        if transcript.trim().is_empty() {
            return Ok(ExtractedProfile::default());
        }
        if let Some(client) = &self.anthropic {
            let user = format!(
                "请阅读以下某用户的跨频道发言,并按系统指令抽取画像。\n\n发言记录:\n{transcript}"
            );
            let (verdict, _) = self
                .complete_accounted(
                    client,
                    crate::usage::UsageContext::new(Some(workspace.to_uuid())),
                    "anthropic_profile_extract",
                    "profile_extract",
                    crate::metrics::CostModel::default().summarize_micros,
                    PROFILE_SYSTEM_PROMPT,
                    &[ChatMsg::user(user)],
                    300,
                )
                .await?;
            // A malformed model line degrades to the heuristic rather than erroring.
            let parsed = parse_profile_verdict(&verdict);
            if !parsed.is_empty() {
                return Ok(parsed);
            }
        }
        Ok(heuristic_profile(messages))
    }

    /// CONSERVATIVE personalisation read (持久跨房 AI 用户画像 使用点).
    ///
    /// Returns a short system-prompt PREFIX describing the participant — to be
    /// prepended to an answer's system prompt so the model can tailor tone /
    /// focus. Returns `None` (no personalisation) when any gate is closed:
    /// the opt-in flag is off, no store is wired, or the participant has no
    /// profile. NEVER changes retrieval or what the model may see — it only adds
    /// a hint, so behaviour is unchanged by default.
    ///
    /// # Errors
    /// Propagates a storage read failure; a missing profile is `Ok(None)`.
    pub async fn profile_personalization_prefix(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
    ) -> Result<Option<String>> {
        if !cross_room_profile_enabled() {
            return Ok(None);
        }
        let Some(store) = self.ai_profiles.as_ref() else {
            return Ok(None);
        };
        // Populate on first read: the extraction (write) path has no other trigger,
        // so without this the opt-in profile table stays empty and personalization
        // never engages despite the flag being on. Best-effort — `extract_and_store`
        // re-checks the same opt-in/store gates, returns `None` cheaply when the
        // participant has no own messages (no LLM call), and a failure/empty result
        // just yields no prefix this turn (never fails the answer).
        let profile = match store.get(participant, workspace).await? {
            Some(p) => Some(p),
            None => self
                .extract_and_store_profile(participant, workspace)
                .await
                .unwrap_or(None),
        };
        let Some(profile) = profile else {
            return Ok(None);
        };
        Ok(render_personalization_prefix(&profile))
    }
}

/// Build a short personalisation hint from a stored profile, or `None` when the
/// profile carries nothing usable. Pure, so it is unit-tested offline.
#[must_use]
pub(crate) fn render_personalization_prefix(profile: &aero_storage::AiProfile) -> Option<String> {
    let topics: Vec<String> = profile
        .topics
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let summary = profile.summary.trim();
    if topics.is_empty() && summary.is_empty() {
        return None;
    }
    let mut hint = String::from("（用户画像,仅供参考以贴合其关注点,不要据此编造事实）");
    if !summary.is_empty() {
        hint.push_str("\n概况: ");
        hint.push_str(summary);
    }
    if !topics.is_empty() {
        hint.push_str("\n常关注: ");
        hint.push_str(&topics.join("、"));
    }
    Some(hint)
}

/// Parse the LLM profile verdict (3 lines: `TOPICS:` / `PREFERENCES:` /
/// `SUMMARY:`). Tolerant of ordering, case, and extra whitespace; unknown lines
/// are ignored. Returns a (possibly empty) [`ExtractedProfile`].
#[must_use]
pub(crate) fn parse_profile_verdict(raw: &str) -> ExtractedProfile {
    let mut out = ExtractedProfile::default();
    for line in raw.lines().map(str::trim) {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let rest = rest.trim();
        match key.trim().to_ascii_uppercase().as_str() {
            "TOPICS" => {
                out.topics = split_terms(rest).into_iter().take(MAX_TOPICS).collect();
            }
            "PREFERENCES" => {
                for term in split_terms(rest) {
                    if let Some((k, v)) = term.split_once('=') {
                        let (k, v) = (k.trim(), v.trim());
                        if !k.is_empty() && !v.is_empty() {
                            out.preferences.insert(k.to_owned(), v.to_owned());
                        }
                    }
                }
            }
            "SUMMARY" => out.summary = rest.to_owned(),
            _ => {}
        }
    }
    out
}

/// Split a comma/、-separated list into trimmed, non-empty terms.
fn split_terms(s: &str) -> Vec<String> {
    s.split([',', '，', '、'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Deterministic, no-LLM profile extraction (staging-gated quality). Counts
/// recurring lowercase word-tokens across the participant's lines and keeps the
/// most frequent as topics; synthesises a one-line summary. Preferences are left
/// empty on this path (no reliable heuristic). Pure → unit-tested offline.
#[must_use]
pub(crate) fn heuristic_profile(messages: &[Message]) -> ExtractedProfile {
    let mut freq: BTreeMap<String, usize> = BTreeMap::new();
    for m in messages {
        let text = m.searchable_text();
        for tok in text.split(|c: char| !c.is_alphanumeric()) {
            let tok = tok.trim().to_lowercase();
            // Skip very short tokens and pure numbers — they're noise as "topics".
            if tok.chars().count() < 4 || tok.chars().all(|c| c.is_numeric()) {
                continue;
            }
            *freq.entry(tok).or_insert(0) += 1;
        }
    }
    // Most frequent first; ties broken by the token for determinism.
    let mut ranked: Vec<(String, usize)> = freq
        .into_iter()
        .filter(|(_, n)| *n >= MIN_TOPIC_FREQ)
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let topics: Vec<String> = ranked
        .into_iter()
        .take(MAX_TOPICS)
        .map(|(t, _)| t)
        .collect();

    let summary = if topics.is_empty() {
        String::new()
    } else {
        format!("常讨论:{}", topics.join("、"))
    };
    ExtractedProfile {
        topics,
        preferences: BTreeMap::new(),
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{Block, Message, MessageId, ParticipantId, RoomId};
    use time::OffsetDateTime;

    fn msg(sender: ParticipantId, text: &str) -> Message {
        Message {
            id: MessageId::new(),
            room_id: RoomId::new(),
            sender_id: sender,
            blocks: vec![Block::text(text)],
            reply_to: None,
            metadata: serde_json::json!({}),
            created_at: OffsetDateTime::UNIX_EPOCH,
            edited_at: None,
            deleted_at: None,
            expires_at: None,
            version: 1,
        }
    }

    #[test]
    fn flag_default_off_and_truthy_values() {
        assert!(
            !flag_is_enabled(None),
            "unset is OFF (default privacy-safe)"
        );
        assert!(!flag_is_enabled(Some("0")));
        assert!(!flag_is_enabled(Some("false")));
        assert!(!flag_is_enabled(Some("")));
        assert!(flag_is_enabled(Some("1")));
        assert!(flag_is_enabled(Some("true")));
        assert!(flag_is_enabled(Some(" TRUE ")));
    }

    #[test]
    fn parse_verdict_extracts_all_fields() {
        let raw = "TOPICS: rust, postgres, oncall\nPREFERENCES: tone=concise, language=zh\nSUMMARY: 关注后端与值班";
        let p = parse_profile_verdict(raw);
        assert_eq!(p.topics, vec!["rust", "postgres", "oncall"]);
        assert_eq!(
            p.preferences.get("tone").map(String::as_str),
            Some("concise")
        );
        assert_eq!(
            p.preferences.get("language").map(String::as_str),
            Some("zh")
        );
        assert_eq!(p.summary, "关注后端与值班");
        assert!(!p.is_empty());
    }

    #[test]
    fn parse_verdict_tolerates_order_case_and_noise() {
        let raw = "summary: hi\njunk line\nTopics: a、b\npreferences:";
        let p = parse_profile_verdict(raw);
        assert_eq!(p.topics, vec!["a", "b"]);
        assert!(
            p.preferences.is_empty(),
            "empty PREFERENCES yields no entries"
        );
        assert_eq!(p.summary, "hi");
    }

    #[test]
    fn empty_extraction_is_empty() {
        assert!(ExtractedProfile::default().is_empty());
        assert!(parse_profile_verdict("garbage with no fields").is_empty());
    }

    #[test]
    fn heuristic_keeps_only_recurring_topics() {
        let s = ParticipantId::new();
        // "postgres" recurs >= MIN_TOPIC_FREQ times; "hello" appears once.
        let messages = vec![
            msg(s, "postgres tuning question"),
            msg(s, "postgres index bloat"),
            msg(s, "postgres vacuum hello"),
            msg(s, "another postgres thing"),
        ];
        let p = heuristic_profile(&messages);
        assert!(
            p.topics.contains(&"postgres".to_string()),
            "recurring topic kept"
        );
        assert!(
            !p.topics.contains(&"hello".to_string()),
            "one-off word dropped"
        );
        assert!(p.summary.contains("postgres"), "summary mentions the topic");
        assert!(
            p.preferences.is_empty(),
            "heuristic path sets no preferences"
        );
    }

    #[test]
    fn topics_and_preferences_json_shapes() {
        let mut p = ExtractedProfile::default();
        p.topics = vec!["x".into(), "y".into()];
        p.preferences.insert("tone".into(), "concise".into());
        assert_eq!(p.topics_json(), serde_json::json!(["x", "y"]));
        assert_eq!(
            p.preferences_json(),
            serde_json::json!({ "tone": "concise" })
        );
    }

    #[test]
    fn personalization_prefix_renders_or_none() {
        use aero_storage::AiProfile;
        // Empty profile → no hint (no personalisation).
        let empty = AiProfile {
            participant_id: ParticipantId::new(),
            workspace_id: WorkspaceId::new(),
            topics: serde_json::json!([]),
            preferences: serde_json::json!({}),
            summary: String::new(),
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert!(render_personalization_prefix(&empty).is_none());

        let full = AiProfile {
            participant_id: ParticipantId::new(),
            workspace_id: WorkspaceId::new(),
            topics: serde_json::json!(["rust", "pg"]),
            preferences: serde_json::json!({}),
            summary: "后端工程师".into(),
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let hint = render_personalization_prefix(&full).expect("hint");
        assert!(hint.contains("后端工程师"));
        assert!(hint.contains("rust"));
    }
}
