use std::sync::{Arc, Mutex};

use aero_common::{Block, Message, MessageId, ParticipantId, RoomId, WorkspaceId};
use aero_storage::SearchHit;
use serde_json::Value;
use time::OffsetDateTime;

use crate::agent::AgentTool;
use crate::anthropic::ToolDef;
use crate::service::{AiService, ChannelRec, Expert, PersonRec, Sentiment, SentimentScore};

pub(crate) fn parse_moderation_verdict(raw: &str) -> Option<String> {
    raw.trim().strip_prefix("BLOCK").map(|rest| {
        let reason = rest.trim_start_matches([':', '：', ' ']).trim();
        if reason.is_empty() { "内容违规".to_owned() } else { reason.to_owned() }
    })
}

// ---------- helpers ----------

/// Width of each per-retriever candidate pool fed into the RRF fuser —
/// deliberately wider than the final top-k (≤20) so lexical agreement deep in
/// the vector list can still promote a hit into the context window.
pub(crate) const RETRIEVAL_POOL: i64 = 40;

pub(crate) const SUMMARIZE_SYSTEM_PROMPT: &str = "\
你是一个对话摘要助手,服务于即时通讯系统。请用中文以无序列表(每条 1-2 行)输出 3-5 条要点摘要,\
按以下结构组织:\n\
- 要点:对话的关键话题或事实\n\
- 决定:已经达成的共识或决策(若无可省略)\n\
- 行动项:谁在何时之前需要做什么(若无可省略)\n\
\n\
约束:不要复述原文,不要超过 200 字,不要使用 Markdown 标题,只输出无序列表项(以 `-` 开头)。";

pub(crate) const RECAP_SYSTEM_PROMPT: &str = "\
你是一个通话/会议复盘助手,服务于即时通讯系统。请阅读给定的通话字幕记录,用中文以无序列表\
(每条 1-2 行)输出简短复盘,按以下结构组织:\n\
- 摘要:本次通话讨论的关键话题或结论\n\
- 决定:已经达成的共识或决策(若无可省略)\n\
- 行动项:谁在何时之前需要做什么(若无可省略)\n\
\n\
约束:不要复述原文,不要超过 200 字,不要使用 Markdown 标题,只输出无序列表项(以 `-` 开头)。";

pub(crate) const TRANSLATE_SYSTEM_PROMPT: &str = "\
你是一个实时字幕翻译引擎。把用户提供的口语化文本翻译成目标语言,保持简洁口语风格。\
只输出译文本身,不要添加任何解释、注释、标点修饰或引号。";

pub(crate) const MODERATE_SYSTEM_PROMPT: &str = "\
你是一个内容安全审核器,服务于企业协作 IM。判断给定文本是否包含应被拦截的内容\
(暴力威胁、仇恨与歧视、露骨色情、违法交易、严重骚扰)。保持克制:仅在明确违规时拦截。\n\
只输出一行:安全则输出 `SAFE`;应拦截则输出 `BLOCK: <简短中文理由>`。不要输出其它任何内容。";

pub(crate) const ANSWER_SYSTEM_PROMPT: &str = "\
你是一个基于检索增强生成(RAG)的问答助手。请严格基于提供的聊天上下文回答用户问题,\
用中文回答。回答规则:\n\
- 若上下文足以回答,给出简洁明确的答案。\n\
- 若上下文不足,直说\"上下文不足以回答\"并说明缺什么。\n\
- 不要编造未在上下文中出现的事实。\n\
- 引用证据时使用上下文中给出的消息 ID。";

pub(crate) const THREAD_TITLE_SYSTEM_PROMPT: &str = "\
你是一个话题串标题生成器,服务于即时通讯系统。请根据给定的根消息与回复,\
生成一个能概括该话题主旨的简短标题。\n\
约束:只输出标题本身(5-10 个词),不要加引号、不要加标点结尾、不要解释、不要换行。\
标题语言与原文一致。";

pub(crate) const SENTIMENT_SYSTEM_PROMPT: &str = "\
你是一个消息情感与毒性分析器,服务于企业协作 IM。请评估给定文本的情感倾向、\
毒性(敌意/辱骂/攻击)程度,以及简短语气标签。这只是描述性分析,不拦截任何内容。\n\
只输出一行,使用竖线分隔三个字段:`情感|毒性|语气`。\n\
- 情感:取值之一 negative / neutral / positive\n\
- 毒性:0 到 1 之间的小数,越高越具敌意\n\
- 语气:一个简短英文或中文词(如 angry、friendly、neutral)\n\
示例:`negative|0.80|angry`。不要输出其它任何内容。";

/// Render messages as a plain chronological transcript for the LLM.
///
/// Format: `[ts] sender_id: text`. Empty messages (no searchable content)
/// are skipped to keep the context window tight.
pub(crate) fn render_transcript(messages: &[Message]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for m in messages {
        let text = m.searchable_text();
        if text.is_empty() {
            continue;
        }
        // `write!` to a String is infallible; the `Result` it returns is the
        // error of the inner formatter (always Ok for String).
        let _ = writeln!(out, "[{}] {}: {}", m.created_at, m.sender_id, text);
    }
    out
}

/// Render search hits as a numbered context block with explicit IDs the LLM
/// can cite. Keeps the score so the model can prefer higher-confidence rows.
/// System prompt for the agentic answer loop: instruct the model to gather context
/// via the tool before answering, and to cite the ids it relied on.
pub(crate) const AGENT_SYSTEM_PROMPT: &str = "\
你是聊天室里的智能助手。回答前,请使用 `search_messages` 工具检索本房间的历史消息以获取依据;\
必要时可用不同关键词多次检索。若某条消息带有文本附件且与问题相关,可用 `read_attachment` 读取其内容。\
只依据检索到的上下文作答,引用所依据消息的 id;若信息不足,如实说明。";



/// A room-scoped retrieval tool for the agentic answer loop. The model calls it
/// with a `query`; it runs the same hybrid (vector + FTS) retrieval as the one-shot
/// path and records every surfaced message id so the caller can build citations.
/// Scoped to a single `room` the asker already belongs to — no privilege widening.
pub(crate) struct SearchMessagesTool {
    pub(crate) svc: Arc<AiService>,
    pub(crate) room: RoomId,
    pub(crate) seen: Mutex<Vec<MessageId>>,
}

#[async_trait::async_trait]
impl crate::agent::AgentTool for SearchMessagesTool {
    fn definition(&self) -> crate::anthropic::ToolDef {
        crate::anthropic::ToolDef {
            name: "search_messages".into(),
            description: "检索本房间历史消息中与查询最相关的若干条(含消息 id 与相关度)。\
回答前调用,可用不同关键词多次调用以补全依据。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "要检索的内容 / 关键词" }
                },
                "required": ["query"]
            }),
        }
    }

    async fn run(&self, input: &Value) -> String {
        let q = input.get("query").and_then(serde_json::Value::as_str).unwrap_or("").trim();
        if q.is_empty() {
            return "error: 'query' 不能为空".to_string();
        }
        match self.svc.retrieve_room(self.room, q, 6).await {
            Ok(hits) => {
                if let Ok(mut seen) = self.seen.lock() {
                    seen.extend(hits.iter().map(|h| h.message.id));
                }
                let ctx = render_context(&hits);
                if ctx.trim().is_empty() {
                    "（未检索到相关消息。）".to_string()
                } else {
                    ctx
                }
            }
            Err(e) => format!("error: 检索失败: {e}"),
        }
    }
}

/// Max attachment bytes the agent will pull into context (256 KiB).
pub(crate) const MAX_ATTACHMENT_BYTES: usize = 256 * 1024;

/// Decode an attachment's bytes to text. First tries structured-document
/// extraction (OOXML docx/xlsx/pptx + best-effort PDF, via [`crate::doc_extract`],
/// detected by magic bytes); otherwise treats the bytes as plain UTF-8 text,
/// returning `None` for empty / over-`cap` / non-UTF-8 / "binary" (more than ~1%
/// non-whitespace control chars) inputs. Pure — no MIME needed.
pub(crate) fn extract_text(bytes: &[u8], cap: usize) -> Option<String> {
    if bytes.is_empty() || bytes.len() > cap {
        return None;
    }
    // docx/xlsx/pptx/pdf → extract their text content (dependency-free; see
    // doc_extract). A recognized-but-unextractable doc falls through to the
    // plain-text attempt, which will reject it as binary.
    if let Some(doc) = crate::doc_extract::extract_document_text(bytes) {
        return Some(doc);
    }
    let s = std::str::from_utf8(bytes).ok()?;
    let total = s.chars().count().max(1);
    let control = s
        .chars()
        .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        .count();
    if control * 100 > total {
        return None;
    }
    Some(s.to_string())
}

/// The first [`Block::File`] attachment on a message (id, name, byte size), in
/// block order, or `None` when the message carries no file. Pure. Used by the
/// indexing worker to locate the document whose body text it folds into
/// `searchable_text` (方向三-2). `Voice` blocks are excluded — their transcript
/// is already indexed via [`Block::searchable_text`].
#[must_use]
pub(crate) fn first_extractable_file(blocks: &[Block]) -> Option<(aero_common::BlobId, String, u64)> {
    blocks.iter().find_map(|b| match b {
        Block::File { blob_id, name, size, .. } => Some((*blob_id, name.clone(), *size)),
        _ => None,
    })
}

/// Fold an attachment's extracted body `doc_text` into a message's `base`
/// searchable text, returning the combined string to index + embed (方向三-2
/// file content search). Pure + unit-testable: no DB / blob involved.
///
/// The document body is appended after the message's own text (separated by a
/// blank line) so a message that says "see the attached spec" plus a `spec.pdf`
/// becomes findable by the spec's *contents*. Whitespace-only `doc_text` is
/// dropped (nothing to add); when `base` is empty the document text stands
/// alone (a file-only message becomes content-searchable rather than blank).
#[must_use]
pub(crate) fn fold_searchable_with_document(base: &str, doc_text: &str) -> String {
    let doc = doc_text.trim();
    if doc.is_empty() {
        return base.to_string();
    }
    if base.trim().is_empty() {
        return doc.to_string();
    }
    format!("{base}\n\n{doc}")
}

/// Agentic tool: read a text attachment on a message in this room. Scoped to
/// `room` and authz-checked (the message must belong to it) so the agent can read
/// nothing the asker couldn't. Binary / oversized / non-text attachments return an
/// explanation rather than bytes.
pub(crate) struct ReadAttachmentTool {
    pub(crate) svc: Arc<AiService>,
    pub(crate) room: RoomId,
}

#[async_trait::async_trait]

impl crate::agent::AgentTool for ReadAttachmentTool {
    fn definition(&self) -> crate::anthropic::ToolDef {
        crate::anthropic::ToolDef {
            name: "read_attachment".into(),
            description: "读取本房间某条消息的附件内容:纯文本/markdown/csv/json/代码,\
以及 Office 文档(docx/xlsx/pptx)与 PDF 的正文文本。输入消息 id;若该消息无文件附件、\
或附件为不可提取的二进制 / 过大则返回说明。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message_id": { "type": "string", "description": "含附件的消息 id" }
                },
                "required": ["message_id"]
            }),
        }
    }

    async fn run(&self, input: &Value) -> String {
        let Some(store) = self.svc.blob_store.as_ref() else {
            return "error: 附件读取未启用".to_string();
        };
        let raw = input.get("message_id").and_then(serde_json::Value::as_str).unwrap_or("").trim();
        let Ok(mid) = raw.parse::<MessageId>() else {
            return "error: 无效的 message_id".to_string();
        };
        // Authz: resolve the message and require it to belong to THIS room.
        let msg = match self.svc.messages.get(mid).await {
            Ok(Some(m)) if m.room_id == self.room => m,
            Ok(Some(_)) => return "error: 该消息不属于当前房间".to_string(),
            Ok(None) => return "error: 未找到该消息".to_string(),
            Err(e) => return format!("error: 查询失败: {e}"),
        };
        let file = msg.blocks.iter().find_map(|b| match b {
            aero_common::Block::File { blob_id, name, size, .. } => {
                Some((*blob_id, name.clone(), *size))
            }
            _ => None,
        });
        let Some((blob_id, name, size)) = file else {
            return "（该消息没有文件附件。）".to_string();
        };
        if size > MAX_ATTACHMENT_BYTES as u64 {
            return format!("（附件 '{name}' 过大,未读取。）");
        }
        let bytes = match store.get(blob_id).await {
            Ok(b) => b,
            Err(e) => return format!("error: 读取附件失败: {e}"),
        };
        match extract_text(&bytes, MAX_ATTACHMENT_BYTES) {
            Some(text) => format!("附件 '{name}' 内容:\n{text}"),
            None => format!("（附件 '{name}' 不是可读文本(二进制或过大)。）"),
        }
    }
}

pub(crate) fn render_context(hits: &[SearchHit]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        let text = h.message.searchable_text();
        if text.is_empty() {
            continue;
        }
        let _ = writeln!(
            out,
            "[{}] id={} score={:.3}\n{}\n",
            i + 1,
            h.message.id,
            h.score,
            text
        );
    }
    out
}

/// Maximum citation message ids kept per ranked expert.
pub(crate) const MAX_EXPERT_CITATIONS: usize = 3;

/// Aggregate topic-relevant search hits into a ranked expert list.
///
/// Pure (no I/O), so the ranking/aggregation is unit-tested offline. Each hit's
/// author accrues that hit's `score`; the author's running total is their
/// relevance, and their highest-scoring message ids (up to [`MAX_EXPERT_CITATIONS`])
/// are kept as citations. The result is sorted by total score descending, ties
/// broken deterministically by the participant's id (so the order is stable across
/// runs), and truncated to `k`. An empty hit list yields an empty result — never a
/// panic — so the "no embeddings / nothing matched" path degrades to an empty list.
#[must_use]
pub(crate) fn rank_experts(hits: &[SearchHit], k: usize) -> Vec<Expert> {
    use std::collections::HashMap;

    // author -> (summed score, all (score, message id) pairs)
    let mut agg: HashMap<ParticipantId, (f32, Vec<(f32, MessageId)>)> = HashMap::new();
    for h in hits {
        let entry = agg.entry(h.message.sender_id).or_insert((0.0, Vec::new()));
        entry.0 += h.score;
        entry.1.push((h.score, h.message.id));
    }

    let mut experts: Vec<Expert> = agg
        .into_iter()
        .map(|(participant, (score, mut scored))| {
            // Keep this author's strongest-scoring message ids as citations.
            // Sort by score desc, ties broken by id so the citation set is stable.
            scored.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.1.cmp(&b.1))
            });
            let citations = scored
                .into_iter()
                .take(MAX_EXPERT_CITATIONS)
                .map(|(_, id)| id)
                .collect();
            Expert { participant, score, citations }
        })
        .collect();

    // Strongest first; ties broken by participant id for a deterministic order.
    experts.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.participant.cmp(&b.participant))
    });
    experts.truncate(k);
    experts
}

/// Fallback display name for a channel with no `name` set.
pub(crate) const UNNAMED_CHANNEL: &str = "(未命名频道)";

/// Rank channel candidates by recent activity into "channels to join" recs.
///
/// Pure (no I/O), so the ranking is unit-tested offline. Each candidate
/// `(room, name, activity)` is scored by its `activity` count (recent message
/// volume): the busiest candidate's count anchors `score = 1.0` and the rest are
/// scaled linearly against it (so scores live in `[0, 1]` and are comparable across
/// requests), with a zero-activity field still ranked (score `0`) below any active
/// channel. Sorted by activity descending, ties broken by room id for a stable
/// order, then truncated to `k`. An empty candidate list yields an empty result —
/// never a panic — so the "no candidates" path degrades cleanly.
#[must_use]
pub(crate) fn rank_channels(candidates: &[(RoomId, String, i64)], k: usize) -> Vec<ChannelRec> {
    if candidates.is_empty() {
        return Vec::new();
    }
    // Normalize against the busiest candidate so scores are comparable.
    let max_activity = candidates.iter().map(|(_, _, a)| *a).max().unwrap_or(0).max(0);

    let mut ranked: Vec<(i64, ChannelRec)> = candidates
        .iter()
        .map(|(room, name, activity)| {
            let activity = (*activity).max(0);
            #[allow(clippy::cast_precision_loss)]
            let score = if max_activity > 0 {
                activity as f32 / max_activity as f32
            } else {
                0.0
            };
            let display = if name.trim().is_empty() {
                UNNAMED_CHANNEL.to_owned()
            } else {
                name.clone()
            };
            let reason = if activity > 0 {
                format!("近期活跃,最近有 {activity} 条消息")
            } else {
                "工作区公开频道,你还未加入".to_owned()
            };
            (activity, ChannelRec { room: *room, name: display, score, reason })
        })
        .collect();

    // Busiest first; ties broken by room id for a deterministic order.
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.room.cmp(&b.1.room)));
    ranked.truncate(k);
    ranked.into_iter().map(|(_, rec)| rec).collect()
}

/// Rank people candidates by shared-room overlap into "people to follow" recs.
///
/// Pure (no I/O), so the ranking is unit-tested offline. Each candidate
/// `(participant, shared)` is scored by `shared` (count of rooms co-occupied with
/// the caller), normalized against the strongest candidate so scores live in
/// `[0, 1]`. Sorted by shared count descending, ties broken by participant id for a
/// stable order, then truncated to `k`. An empty candidate list yields an empty
/// result. Callers are expected to have already removed the caller themselves and
/// anyone they already follow before ranking.
#[must_use]
pub(crate) fn rank_people(candidates: &[(ParticipantId, i64)], k: usize) -> Vec<PersonRec> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let max_shared = candidates.iter().map(|(_, s)| *s).max().unwrap_or(0).max(0);

    let mut ranked: Vec<(i64, PersonRec)> = candidates
        .iter()
        .map(|(participant, shared)| {
            let shared = (*shared).max(0);
            #[allow(clippy::cast_precision_loss)]
            let score = if max_shared > 0 {
                shared as f32 / max_shared as f32
            } else {
                0.0
            };
            let reason = if shared > 0 {
                format!("你们共同加入了 {shared} 个频道")
            } else {
                "同工作区成员".to_owned()
            };
            (shared, PersonRec { participant: *participant, score, reason })
        })
        .collect();

    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.participant.cmp(&b.1.participant)));
    ranked.truncate(k);
    ranked.into_iter().map(|(_, rec)| rec).collect()
}

/// Last-5-lines fallback summary used when Anthropic is disabled.
pub(crate) fn heuristic_summary(messages: &[Message]) -> String {
    let tail: Vec<&Message> = messages.iter().rev().take(5).collect();
    let mut lines = Vec::with_capacity(tail.len());
    for m in tail.iter().rev() {
        let text = m.searchable_text();
        if text.is_empty() {
            continue;
        }
        // Single-line, trimmed; bullet-style to mirror the LLM output shape.
        let one_line = text.replace('\n', " ");
        lines.push(format!("- {}", truncate_for_summary(&one_line, 120)));
    }
    if lines.is_empty() {
        return String::new();
    }
    lines.join("\n")
}

/// First-lines digest fallback used by [`AiService::summarize_text`] when
/// Anthropic is disabled. Mirrors [`heuristic_summary`]'s shape: the first up-to-5
/// non-blank lines of the text, each collapsed to one line, truncated, and
/// rendered as a `-` bullet. Empty input yields an empty string.
pub(crate) fn heuristic_text_digest(text: &str) -> String {
    let mut lines = Vec::with_capacity(5);
    for raw in text.lines() {
        let one_line = raw.trim();
        if one_line.is_empty() {
            continue;
        }
        lines.push(format!("- {}", truncate_for_summary(one_line, 120)));
        if lines.len() == 5 {
            break;
        }
    }
    lines.join("\n")
}

pub(crate) fn truncate_for_summary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// Max whitespace-delimited words kept in the heuristic thread title.
pub(crate) const HEURISTIC_TITLE_WORDS: usize = 8;

/// Heuristic thread title used when Anthropic is disabled — the first up-to-8
/// whitespace-delimited words of the (single-lined) root message text, trimmed.
///
/// Degrades safely: empty/blank root text yields an empty string. CJK text often
/// has no spaces, so when the first line is a single unbroken word (no internal
/// whitespace) it is character-truncated to keep the title bounded.
pub(crate) fn heuristic_title(root_text: &str) -> String {
    let one_line = root_text.replace('\n', " ");
    let trimmed = one_line.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let words: Vec<&str> = trimmed.split_whitespace().take(HEURISTIC_TITLE_WORDS).collect();
    if words.len() <= 1 {
        // No word boundaries (e.g. CJK) — char-truncate so the title stays bounded.
        return truncate_for_summary(trimmed, 24);
    }
    let title = words.join(" ");
    // Even with several words, keep an upper bound on length for the UI chip.
    truncate_for_summary(&title, 80)
}

/// Strip wrapping quotes / trailing terminal punctuation an LLM sometimes adds to
/// a one-line title, and collapse to a single trimmed line. Bounds the length.
pub(crate) fn clean_title(raw: &str) -> String {
    // Models occasionally add a leading note line; take the first non-blank line.
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").trim();
    let line = line
        .trim_matches(|c| c == '"' || c == '\'' || c == '“' || c == '”' || c == '「' || c == '」')
        .trim_end_matches(['.', '。', '!', '!', '?', '?'])
        .trim();
    truncate_for_summary(line, 80)
}

/// Insult / hostility keyword set for the no-LLM toxicity heuristic. Lowercased,
/// matched case-insensitively as substrings. Deliberately small and conservative —
/// this is a degrade path, not a real classifier.
const TOXIC_KEYWORDS: &[&str] = &[
    "idiot", "stupid", "shut up", "hate", "kill you", "moron", "loser", "trash",
    "dumb", "fool", "scum", "滚", "废物", "白痴", "去死", "蠢", "垃圾", "傻",
];

/// Positive-sentiment keyword set for the no-LLM heuristic. Lowercased, matched
/// case-insensitively as substrings.
const POSITIVE_KEYWORDS: &[&str] = &[
    "thank", "thanks", "great", "awesome", "love", "good job", "well done",
    "nice", "excellent", "appreciate", "congrats", "happy", "谢谢", "感谢",
    "太好了", "棒", "赞", "干得好", "厉害", "开心",
];

/// Deterministic keyword/punctuation sentiment+toxicity heuristic used when
/// Anthropic is disabled (degrade-safe path).
///
/// Rules (checked in priority order):
/// - An insult/hostility keyword OR ALL-CAPS shouting raises toxicity and reads
///   `negative` with an `angry` tone (a keyword hit pushes toxicity higher).
/// - Otherwise a positive keyword reads `positive` (tone `friendly`), toxicity low.
/// - Otherwise neutral with low toxicity; a lone `!` nudges tone to `excited`.
///
/// Pure (no I/O), so the truth-table is unit-tested offline. Blank input scores
/// neutral.
#[must_use]
pub(crate) fn heuristic_sentiment(text: &str) -> SentimentScore {
    let text = text.trim();
    if text.is_empty() {
        return SentimentScore::neutral();
    }
    let lower = text.to_lowercase();

    let has_toxic = TOXIC_KEYWORDS.iter().any(|kw| lower.contains(kw));
    let has_positive = POSITIVE_KEYWORDS.iter().any(|kw| lower.contains(kw));

    // ALL-CAPS shouting: there is at least one ASCII letter and every ASCII letter
    // is uppercase, with enough letters to be a deliberate shout (not "OK").
    let ascii_letters: Vec<char> = text.chars().filter(char::is_ascii_alphabetic).collect();
    let is_shouting =
        ascii_letters.len() >= 4 && ascii_letters.iter().all(char::is_ascii_uppercase);

    if has_toxic || is_shouting {
        // Keyword hostility is a stronger signal than mere shouting.
        let toxicity = if has_toxic { 0.85 } else { 0.6 };
        return SentimentScore {
            sentiment: Sentiment::Negative,
            toxicity,
            tone: "angry".to_owned(),
        };
    }

    if has_positive {
        return SentimentScore {
            sentiment: Sentiment::Positive,
            toxicity: 0.0,
            tone: "friendly".to_owned(),
        };
    }

    // Neutral baseline; a lone exclamation reads as excited (still neutral polarity,
    // still low toxicity).
    let tone = if text.contains('!') || text.contains('!') { "excited" } else { "neutral" };
    SentimentScore { sentiment: Sentiment::Neutral, toxicity: 0.05, tone: tone.to_owned() }
}



