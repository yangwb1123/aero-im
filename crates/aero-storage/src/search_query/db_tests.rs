use super::{parse_search_query, AdvancedSearchRepo};
use crate::{MessageRepo, NewMessage, WorkspaceRepo};
use aero_common::{Block, MessageId, ParticipantId, RoomId, WorkspaceId};
use sqlx::PgPool;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

// A throwaway participant so message/membership FKs are satisfiable.
async fn participant(p: &PgPool) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(id.to_uuid())
        .bind(format!("adv-search-actor-{id}"))
        .execute(p)
        .await
        .expect("insert participant");
    id
}

async fn workspace(p: &PgPool, owner: ParticipantId) -> WorkspaceId {
    WorkspaceRepo::new(p.clone())
        .create(
            format!("Advanced search {owner}"),
            format!("advanced-search-{owner}"),
            owner,
        )
        .await
        .expect("create workspace")
        .id
}

async fn enroll(p: &PgPool, workspace: WorkspaceId, participant: ParticipantId) {
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'member')",
    )
    .bind(workspace.to_uuid())
    .bind(participant.to_uuid())
    .execute(p)
    .await
    .expect("enroll participant");
}

// A throwaway room in a test-owned workspace.
async fn room(p: &PgPool, workspace: WorkspaceId, creator: ParticipantId) -> RoomId {
    let id = RoomId::new();
    sqlx::query(
        "INSERT INTO rooms (id, kind, name, created_by, workspace_id) VALUES ($1,'group',$2,$3,$4)",
    )
    .bind(id.to_uuid())
    .bind(format!("adv-search-room-{id}"))
    .bind(creator.to_uuid())
    .bind(workspace.to_uuid())
    .execute(p)
    .await
    .expect("insert room");
    id
}

async fn join(p: &PgPool, room: RoomId, who: ParticipantId) {
    sqlx::query("INSERT INTO room_members (room_id, participant_id, role) VALUES ($1,$2,'member')")
        .bind(room.to_uuid())
        .bind(who.to_uuid())
        .execute(p)
        .await
        .expect("insert membership");
}

async fn remove_workspace(p: &PgPool, workspace: WorkspaceId) {
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(p)
        .await
        .expect("delete workspace");
}

/// `from:` narrows to one sender; `in:` excludes hits from other rooms; bare
/// free text matches across all the caller's rooms — all within the SQL
/// membership boundary.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn advanced_search_filters_by_from_in_and_free_text() {
    let p = pool();
    let repo = AdvancedSearchRepo::new(p.clone());
    let msgs = MessageRepo::new(p.clone());
    let me = participant(&p).await;
    let ws = workspace(&p, me).await;
    let other = participant(&p).await;
    enroll(&p, ws, other).await;

    // Two rooms the caller belongs to.
    let mine = room(&p, ws, me).await;
    let elsewhere = room(&p, ws, me).await;
    join(&p, mine, me).await;
    join(&p, mine, other).await;
    join(&p, elsewhere, me).await;

    // A distinctive token shared by every seeded message.
    let needle = format!("zqxbladetoken{}", ParticipantId::new());
    let m_me = msgs
        .insert(NewMessage {
            room_id: mine,
            sender_id: me,
            blocks: vec![Block::text(format!("alpha {needle} from me"))],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("insert m_me");
    let m_other = msgs
        .insert(NewMessage {
            room_id: mine,
            sender_id: other,
            blocks: vec![Block::text(format!("beta {needle} from other"))],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("insert m_other");
    let m_elsewhere = msgs
        .insert(NewMessage {
            room_id: elsewhere,
            sender_id: me,
            blocks: vec![Block::text(format!("gamma {needle} elsewhere"))],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("insert m_elsewhere");

    // Free text alone: matches across both of the caller's rooms.
    let q = parse_search_query(&needle);
    let hits = repo.search(me, ws, &q, 50).await.expect("free-text search");
    assert!(hits.iter().any(|h| h.message.id == m_me.id));
    assert!(hits.iter().any(|h| h.message.id == m_other.id));
    assert!(hits.iter().any(|h| h.message.id == m_elsewhere.id));

    // from:<other> restricts to that sender only.
    let q = parse_search_query(&format!("{needle} from:{other}"));
    let hits = repo.search(me, ws, &q, 50).await.expect("from: search");
    assert!(
        hits.iter().all(|h| h.message.sender_id == other),
        "every hit is from the requested sender"
    );
    assert!(hits.iter().any(|h| h.message.id == m_other.id));
    assert!(!hits.iter().any(|h| h.message.id == m_me.id));

    // in:<mine> excludes the message that lives in the other room.
    let q = parse_search_query(&format!("{needle} in:{mine}"));
    let hits = repo.search(me, ws, &q, 50).await.expect("in: search");
    assert!(
        hits.iter().all(|h| h.message.room_id == mine),
        "every hit is from the requested room"
    );
    assert!(!hits.iter().any(|h| h.message.id == m_elsewhere.id));

    // Cleanup so reruns stay self-contained (children before parents).
    for id in [m_me.id, m_other.id, m_elsewhere.id] {
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(id.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
    for r in [mine, elsewhere] {
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(r.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(r.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
    remove_workspace(&p, ws).await;
    for who in [me, other] {
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(who.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}

/// The advanced-search path stems and folds accents in LOCK-STEP with the
/// stored `search_tsv` (migrations 0128 + 0131): a query for the root/unaccented
/// form matches an inflected/diacritic'd message. Guards the regression where
/// this repo still used `websearch_to_tsquery('simple', …)` against the
/// `english`+`f_unaccent` column — which silently dropped stem/accent hits.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn advanced_search_stems_and_unaccents() {
    let p = pool();
    let repo = AdvancedSearchRepo::new(p.clone());
    let msgs = MessageRepo::new(p.clone());
    let me = participant(&p).await;
    let ws = workspace(&p, me).await;
    let r = room(&p, ws, me).await;
    join(&p, r, me).await;

    // Unique marker so concurrent rows can't satisfy the assertions for us.
    let marker = format!("zqftsmark{}", ParticipantId::new());
    let m = msgs
        .insert(NewMessage {
            room_id: r,
            sender_id: me,
            // 'deploying' (inflected) + 'café' (accented), neither appearing
            // literally in the queries below.
            blocks: vec![Block::text(format!("{marker} deploying to the café"))],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("insert m");

    // Root form 'deploy' (stem) — must match 'deploying'.
    let q = parse_search_query(&format!("{marker} deploy"));
    let hits = repo.search(me, ws, &q, 50).await.expect("stem search");
    assert!(
        hits.iter().any(|h| h.message.id == m.id),
        "advanced search stems 'deploy' → 'deploying'"
    );

    // Unaccented 'cafe' — must match 'café'.
    let q = parse_search_query(&format!("{marker} cafe"));
    let hits = repo.search(me, ws, &q, 50).await.expect("accent search");
    assert!(
        hits.iter().any(|h| h.message.id == m.id),
        "advanced search folds 'cafe' → 'café'"
    );

    // Cleanup.
    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(m.id.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM room_members WHERE room_id = $1")
        .bind(r.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(r.to_uuid())
        .execute(&p)
        .await
        .ok();
    remove_workspace(&p, ws).await;
    sqlx::query("DELETE FROM participants WHERE id = $1")
        .bind(me.to_uuid())
        .execute(&p)
        .await
        .ok();
}

/// `count` returns the full match total regardless of page size, and
/// `search_page` walks the whole result set in keyset pages with no overlaps
/// and no gaps — including across score ties (every seeded message shares the
/// same single needle token, so they rank near-identically, exercising the
/// `(score, id)` tiebreaker).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn count_and_keyset_pagination_cover_every_hit_once() {
    use super::SearchCursor;
    let p = pool();
    let repo = AdvancedSearchRepo::new(p.clone());
    let msgs = MessageRepo::new(p.clone());
    let me = participant(&p).await;
    let ws = workspace(&p, me).await;
    let r = room(&p, ws, me).await;
    join(&p, r, me).await;

    // Seed 25 messages all carrying one distinctive token.
    let needle = format!("zpagetoken{}", ParticipantId::new());
    let mut seeded = Vec::new();
    for i in 0..25 {
        let m = msgs
            .insert(NewMessage {
                room_id: r,
                sender_id: me,
                blocks: vec![Block::text(format!("{needle} item {i}"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert");
        seeded.push(m.id);
    }

    let q = parse_search_query(&needle);

    // count() is page-independent.
    let total = repo.count(me, ws, &q).await.expect("count");
    assert_eq!(total, 25, "count reflects all seeded matches");

    // Walk every page of size 10 via the returned cursor, collecting ids.
    let mut seen: Vec<MessageId> = Vec::new();
    let mut cursor: Option<SearchCursor> = None;
    let mut pages = 0;
    loop {
        let (hits, next) = repo
            .search_page(me, ws, &q, 10, cursor)
            .await
            .expect("page");
        pages += 1;
        assert!(pages <= 10, "pagination must terminate");
        for h in &hits {
            seen.push(h.message.id);
        }
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }

    // Every seeded id appears exactly once across all pages (no gaps/overlaps).
    assert_eq!(
        seen.len(),
        25,
        "every hit returned exactly once across pages"
    );
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        25,
        "no duplicate ids across pages (keyset tiebreak holds)"
    );
    for id in &seeded {
        assert!(seen.contains(id), "seeded id {id} appears in some page");
    }

    // A round-tripped cursor decodes back to the same boundary.
    let (_first, next) = repo
        .search_page(me, ws, &q, 10, None)
        .await
        .expect("first page");
    let c = next.expect("first page has a next cursor");
    assert_eq!(
        SearchCursor::decode(&c.encode()),
        Some(c),
        "cursor encode/decode round-trips"
    );

    // Cleanup.
    for id in &seeded {
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(id.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
    sqlx::query("DELETE FROM room_members WHERE room_id = $1")
        .bind(r.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(r.to_uuid())
        .execute(&p)
        .await
        .ok();
    remove_workspace(&p, ws).await;
    sqlx::query("DELETE FROM participants WHERE id = $1")
        .bind(me.to_uuid())
        .execute(&p)
        .await
        .ok();
}

/// `suggest_terms` returns trigram-near words from the caller's own recent
/// messages — so a typo'd query can surface a "did you mean" hint — and is
/// membership-scoped (never a word from a room the caller can't see).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn suggest_terms_offers_trigram_near_words_membership_scoped() {
    let p = pool();
    let repo = AdvancedSearchRepo::new(p.clone());
    let msgs = MessageRepo::new(p.clone());
    let me = participant(&p).await;
    let ws = workspace(&p, me).await;
    let stranger = participant(&p).await;
    enroll(&p, ws, stranger).await;
    let mine = room(&p, ws, me).await;
    let theirs = room(&p, ws, stranger).await;
    join(&p, mine, me).await;
    join(&p, theirs, stranger).await; // I am NOT a member of `theirs`.

    // A distinctive, unusual word in MY room.
    let marker = "zqdeploymentpipeline";
    let m = msgs
        .insert(NewMessage {
            room_id: mine,
            sender_id: me,
            blocks: vec![Block::text(format!("the {marker} is green"))],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("insert mine");
    // A different distinctive word in a room I can't see.
    let secret = "zqforbiddenkeyword";
    let m_secret = msgs
        .insert(NewMessage {
            room_id: theirs,
            sender_id: stranger,
            blocks: vec![Block::text(format!("a {secret} here"))],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("insert theirs");

    // A near-miss of my word suggests it.
    let sugg = repo
        .suggest_terms(me, ws, "zqdeploymentpipelin", 5)
        .await
        .expect("suggest");
    assert!(
        sugg.iter().any(|w| w == marker),
        "a trigram-near typo surfaces my word, got {sugg:?}"
    );

    // A near-miss of the forbidden word suggests NOTHING — I'm not in that room.
    let sugg = repo
        .suggest_terms(me, ws, "zqforbiddenkeywor", 5)
        .await
        .expect("suggest secret");
    assert!(
        !sugg.iter().any(|w| w == secret),
        "membership boundary: a word from a room I can't see is never suggested, got {sugg:?}"
    );

    // Cleanup.
    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(m.id.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(m_secret.id.to_uuid())
        .execute(&p)
        .await
        .ok();
    for rm in [mine, theirs] {
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(rm.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(rm.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
    remove_workspace(&p, ws).await;
    for who in [me, stranger] {
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(who.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}

/// `facets` groups the SAME membership-scoped match set by room and by sender,
/// with counts that sum to the total and lists ordered by count — the
/// drill-down breakdown. Membership-scoped like search/count.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn facets_break_down_matches_by_room_and_sender() {
    let p = pool();
    let repo = AdvancedSearchRepo::new(p.clone());
    let msgs = MessageRepo::new(p.clone());
    let me = participant(&p).await;
    let ws = workspace(&p, me).await;
    let other = participant(&p).await;
    enroll(&p, ws, other).await;
    let room_a = room(&p, ws, me).await;
    let room_b = room(&p, ws, me).await;
    join(&p, room_a, me).await;
    join(&p, room_a, other).await;
    join(&p, room_b, me).await;

    // needle distribution: room_a gets 3 (2 from me, 1 from other), room_b 1 (me).
    let needle = format!("zqfacettoken{}", ParticipantId::new());
    let plan = [(room_a, me), (room_a, me), (room_a, other), (room_b, me)];
    let mut ids = Vec::new();
    for (i, (rm, sender)) in plan.iter().enumerate() {
        let m = msgs
            .insert(NewMessage {
                room_id: *rm,
                sender_id: *sender,
                blocks: vec![Block::text(format!("{needle} n{i}"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert");
        ids.push(m.id);
    }

    let q = parse_search_query(&needle);
    let total = repo.count(me, ws, &q).await.expect("count");
    let facets = repo.facets(me, ws, &q, 10).await.expect("facets");

    // Room facet: room_a=3, room_b=1, ordered by count desc, summing to total.
    let room_sum: i64 = facets.rooms.iter().map(|f| f.count).sum();
    assert_eq!(room_sum, total, "room facet counts sum to total");
    assert_eq!(
        facets.rooms.first().map(|f| f.count),
        Some(3),
        "top room has 3 hits"
    );
    assert!(
        facets
            .rooms
            .iter()
            .any(|f| f.value == room_a.to_uuid().to_string() && f.count == 3),
        "room_a has 3 hits, got {:?}",
        facets.rooms
    );
    assert!(
        facets.rooms.windows(2).all(|w| w[0].count >= w[1].count),
        "rooms ordered desc"
    );

    // Sender facet: me=3, other=1.
    let sender_sum: i64 = facets.senders.iter().map(|f| f.count).sum();
    assert_eq!(sender_sum, total, "sender facet counts sum to total");
    assert!(
        facets
            .senders
            .iter()
            .any(|f| f.value == me.to_uuid().to_string() && f.count == 3),
        "sender me has 3 hits, got {:?}",
        facets.senders
    );

    // Cleanup.
    for id in &ids {
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(id.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
    for rm in [room_a, room_b] {
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(rm.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(rm.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
    remove_workspace(&p, ws).await;
    for who in [me, other] {
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(who.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
