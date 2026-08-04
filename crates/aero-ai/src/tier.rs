//! Query-difficulty model-tier routing (ROADMAP 第六版 · 方向一·1).
//!
//! The single configured `ANTHROPIC_MODEL` pays the same rate for a trivial fact
//! lookup as for complex cross-document synthesis. This classifies an answer query
//! into a difficulty tier (cheap heuristic — no extra model call) so the answer
//! path can route easy queries to a cheaper model and hard ones to a stronger one.
//!
//! **Opt-in, default-OFF**: a tier maps to a model only if its env var is set
//! (`AERO_AI_MODEL_EASY` / `AERO_AI_MODEL_HARD`); unset ⇒ `None` ⇒ the answer path
//! uses the client's default model, so behaviour is byte-identical until an
//! operator opts a tier in. Typical deployments are 60–70% easy queries.

/// Difficulty tier for an answer query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiTier {
    /// Short, well-grounded factual lookup → cheap model.
    Easy,
    /// The default — no strong signal either way → the client's default model.
    Medium,
    /// Long / multi-clause / reasoning-marked / ungrounded → strong model.
    Hard,
}

/// Markers that signal cross-document reasoning / synthesis (→ Hard).
const REASONING_MARKERS: &[&str] = &[
    "compare",
    "对比",
    "分析",
    "analyze",
    "why",
    "为什么",
    "explain",
    "解释",
    "综合",
    "trade-off",
    "权衡",
    "pros and cons",
    "step by step",
    "逐步",
    "推理",
    "summarize across",
];

/// Classify an answer query by difficulty from the query text + how many retrieval
/// hits grounded it. Pure + cheap (no model call), so it never adds latency/cost.
/// Char-count based so it works for both space-delimited (EN) and CJK queries
/// (where word count is meaningless — no spaces).
///
/// - **Hard**: long (`>160` chars), OR contains a reasoning marker, OR ungrounded
///   (`retrieval_hits == 0` ⇒ the model must reason unaided).
/// - **Easy**: short (`<=40` chars) AND well-grounded (`>=2` hits) AND no marker.
/// - **Medium**: everything else (the default tier ⇒ the client's default model).
#[must_use]
pub fn classify_tier(query: &str, retrieval_hits: usize) -> AiTier {
    let q = query.trim();
    let chars = q.chars().count();
    let lower = q.to_lowercase();
    let reasoning = REASONING_MARKERS.iter().any(|m| lower.contains(m));

    if chars > 160 || reasoning || retrieval_hits == 0 {
        AiTier::Hard
    } else if chars <= 40 && retrieval_hits >= 2 {
        AiTier::Easy
    } else {
        AiTier::Medium
    }
}

/// Resolve the model name to use for a tier, from env. `None` ⇒ use the client's
/// default model (the opt-in default-OFF contract). An empty env value is treated
/// as unset.
#[must_use]
pub fn tier_model(tier: AiTier) -> Option<String> {
    let var = match tier {
        AiTier::Easy => "AERO_AI_MODEL_EASY",
        AiTier::Hard => "AERO_AI_MODEL_HARD",
        AiTier::Medium => return None,
    };
    std::env::var(var)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_grounded_factual_is_easy() {
        assert_eq!(classify_tier("部署在哪台机器", 3), AiTier::Easy);
        assert_eq!(classify_tier("who owns auth", 4), AiTier::Easy);
    }

    #[test]
    fn ungrounded_is_hard_even_if_short() {
        // No retrieval grounding ⇒ the model reasons unaided ⇒ Hard.
        assert_eq!(classify_tier("status?", 0), AiTier::Hard);
    }

    #[test]
    fn reasoning_marker_is_hard() {
        assert_eq!(classify_tier("对比 A 和 B 的取舍", 5), AiTier::Hard);
        assert_eq!(classify_tier("why did the deploy fail", 5), AiTier::Hard);
    }

    #[test]
    fn long_query_is_hard() {
        let long = "请综合这周所有频道里关于发布、回滚、监控告警和值班的讨论，给出一份完整的复盘与改进建议清单";
        assert_eq!(classify_tier(long, 8), AiTier::Hard);
    }

    #[test]
    fn medium_when_no_strong_signal() {
        // Mid-length (41–160 chars), grounded, no marker → not short enough for
        // Easy, not long/marked/ungrounded for Hard.
        let mid = "我们这次线上发布用到的灰度放量策略和回滚预案具体是怎么安排的，分别涉及哪些下游服务以及对应的值班同学和告警阈值";
        assert!(
            mid.chars().count() > 40 && mid.chars().count() <= 160,
            "fixture length"
        );
        assert_eq!(classify_tier(mid, 3), AiTier::Medium);
    }

    #[test]
    fn tier_model_is_opt_in_default_off() {
        // Medium is always None; Easy/Hard are None unless their env is set.
        assert_eq!(tier_model(AiTier::Medium), None);
        // Don't mutate process env in a parallel test suite; just assert the
        // default-off shape for an unset var name via a tier we know is env-gated.
        // (Env-set behaviour is exercised by the answer-path integration, not here.)
    }
}
