//! Reciprocal-Rank-Fusion rerank over the RAG candidate lists (方向三).
//!
//! Bare cosine top-k lets semantically-near-but-wrong hits squeeze out the
//! limited context window. [`fuse_rankings`] blends the vector candidate list
//! with a lexical (Postgres FTS) candidate list using RRF — a rank-only fusion
//! that needs no score calibration between the two retrievers — so a message
//! that BOTH retrievers surface outranks one that only one of them likes.
//!
//! Pure: no I/O, no async. The retrieval halves live in
//! [`MessageRepo`](aero_storage::MessageRepo) (`search_vector*` /
//! `fts_candidates*`); [`crate::AiService`] wires them together.

use std::collections::HashMap;

use aero_common::MessageId;
use aero_storage::SearchHit;

/// RRF smoothing constant. The canonical value from Cormack et al. (2009):
/// large enough that a single first-place vote cannot drown out broad mid-list
/// agreement between the two retrievers.
const RRF_K: f64 = 60.0;

/// Fuse a vector-ranked and an FTS-ranked candidate list with Reciprocal Rank
/// Fusion and return the top `k` hits.
///
/// Each message scores `Σ 1/(60 + rank)` over the lists it appears in (`rank`
/// is its 1-based position in that list). Ties — e.g. two messages holding the
/// same single-list position — break by recency: ids are time-sortable ULIDs,
/// so the larger (newer) id wins. Returned hits carry the fused RRF score
/// (replacing the retriever-specific cosine/`ts_rank` values, which are not
/// comparable across lists).
///
/// Degradation is built in: an empty `fts_ranked` reproduces the vector order
/// exactly (RRF is monotone over a single list), and vice versa, so callers can
/// pass an empty list when one retriever fails rather than failing the ask.
#[must_use]
pub fn fuse_rankings(
    vector_ranked: &[SearchHit],
    fts_ranked: &[SearchHit],
    k: usize,
) -> Vec<SearchHit> {
    let mut scores: HashMap<MessageId, f64> = HashMap::new();
    // First sighting of each message keeps its row; the lists may carry the
    // same message and the row content is identical either way.
    let mut rows: HashMap<MessageId, &SearchHit> = HashMap::new();

    for list in [vector_ranked, fts_ranked] {
        for (i, hit) in list.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let rank = (i + 1) as f64;
            *scores.entry(hit.message.id).or_insert(0.0) += 1.0 / (RRF_K + rank);
            rows.entry(hit.message.id).or_insert(hit);
        }
    }

    let mut fused: Vec<(f64, MessageId)> =
        scores.into_iter().map(|(id, score)| (score, id)).collect();
    // Highest fused score first; on exact ties the newer ULID wins.
    fused.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    fused.truncate(k);

    fused
        .into_iter()
        .map(|(score, id)| {
            let row = rows[&id];
            #[allow(clippy::cast_possible_truncation)]
            SearchHit {
                message: row.message.clone(),
                score: score as f32,
                headline: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{fuse_rankings, RRF_K};
    use aero_common::{Block, Message, MessageId, ParticipantId, RoomId};
    use aero_storage::SearchHit;
    use ulid::Ulid;

    /// A hit whose message id has a controlled timestamp component, so recency
    /// (ULID order) is deterministic in tests: larger `ts` == newer message.
    fn hit(ts: u64, score: f32) -> SearchHit {
        SearchHit {
            message: Message {
                id: MessageId::from_ulid(Ulid::from_parts(ts, 0)),
                room_id: RoomId::new(),
                sender_id: ParticipantId::new(),
                blocks: vec![Block::text(format!("msg {ts}"))],
                reply_to: None,
                metadata: serde_json::Value::Null,
                created_at: time::OffsetDateTime::UNIX_EPOCH,
                edited_at: None,
                deleted_at: None,
                expires_at: None,
                version: 1,
            },
            score,
            headline: None,
        }
    }

    fn ids(hits: &[SearchHit]) -> Vec<MessageId> {
        hits.iter().map(|h| h.message.id).collect()
    }

    /// Empty FTS list — the degrade path when Postgres FTS errors — must
    /// reproduce the vector order exactly (then truncate to k).
    #[test]
    fn empty_fts_degrades_to_vector_order() {
        let vector = vec![hit(30, 0.9), hit(10, 0.8), hit(20, 0.7), hit(40, 0.6)];
        let fused = fuse_rankings(&vector, &[], 3);
        assert_eq!(
            ids(&fused),
            ids(&vector[..3]),
            "vector order preserved, truncated to k"
        );
    }

    /// Symmetric degrade: empty vector list (e.g. nothing embedded yet) yields
    /// the FTS order.
    #[test]
    fn empty_vector_degrades_to_fts_order() {
        let fts = vec![hit(5, 0.4), hit(7, 0.3)];
        let fused = fuse_rankings(&[], &fts, 10);
        assert_eq!(ids(&fused), ids(&fts));
    }

    #[test]
    fn both_empty_yields_empty() {
        assert!(fuse_rankings(&[], &[], 5).is_empty());
    }

    /// A message present in BOTH lists must outrank messages that only one
    /// retriever surfaced — the whole point of the fusion.
    #[test]
    fn overlap_outranks_single_list_membership() {
        let shared = hit(100, 0.5);
        // `shared` is only rank 2 in each list, but two votes beat the single
        // first-place vote of either list leader: 2/62 > 1/61.
        let vector = vec![hit(200, 0.9), shared.clone(), hit(300, 0.1)];
        let fts = vec![hit(400, 0.8), shared.clone(), hit(500, 0.2)];

        let fused = fuse_rankings(&vector, &fts, 10);
        assert_eq!(
            fused[0].message.id, shared.message.id,
            "double-listed hit wins"
        );
        assert_eq!(fused.len(), 5, "union of distinct candidates");

        let expected = 2.0 / (RRF_K + 2.0);
        assert!(
            (f64::from(fused[0].score) - expected).abs() < 1e-6,
            "fused score is the RRF sum, got {}",
            fused[0].score
        );
    }

    /// Disjoint lists interleave by RRF score: equal ranks across the two lists
    /// tie, and ties break by recency (newer ULID first).
    #[test]
    fn disjoint_lists_tiebreak_by_recency() {
        // vector rank 1 (ts=10) ties fts rank 1 (ts=20); same at rank 2.
        let vector = vec![hit(10, 0.9), hit(40, 0.8)];
        let fts = vec![hit(20, 0.7), hit(30, 0.6)];

        let fused = fuse_rankings(&vector, &fts, 10);
        let got: Vec<u64> = fused
            .iter()
            .map(|h| h.message.id.as_ulid().timestamp_ms())
            .collect();
        // Rank-1 pair first (newer 20 before 10), then rank-2 pair (40 before 30).
        assert_eq!(got, vec![20, 10, 40, 30]);
    }

    #[test]
    fn k_truncates_the_fused_list() {
        let vector = vec![hit(1, 0.9), hit(2, 0.8), hit(3, 0.7)];
        let fts = vec![hit(4, 0.6), hit(5, 0.5)];
        assert_eq!(fuse_rankings(&vector, &fts, 2).len(), 2);
        assert_eq!(fuse_rankings(&vector, &fts, 0).len(), 0);
        // k larger than the union returns everything, no padding.
        assert_eq!(fuse_rankings(&vector, &fts, 50).len(), 5);
    }

    /// The same message listed by both retrievers appears ONCE in the output.
    #[test]
    fn overlapping_hit_is_deduplicated() {
        let shared = hit(9, 0.5);
        let fused = fuse_rankings(&[shared.clone()], &[shared.clone()], 10);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].message.id, shared.message.id);
    }
}
