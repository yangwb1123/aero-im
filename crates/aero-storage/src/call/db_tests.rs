use super::*;

const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

async fn participant(p: &PgPool, tag: &str) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(id.to_uuid())
        .bind(format!("call-{tag}-{id}"))
        .execute(p)
        .await
        .expect("insert participant");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'member')",
    )
    .bind(uuid::Uuid::parse_str(DEFAULT_WS).expect("uuid"))
    .bind(id.to_uuid())
    .execute(p)
    .await
    .expect("insert workspace member");
    id
}

async fn add_room_member(p: &PgPool, room: RoomId, participant: ParticipantId) {
    sqlx::query(
        "INSERT INTO room_members (room_id, participant_id, role)
         VALUES ($1, $2, 'member')",
    )
    .bind(room.to_uuid())
    .bind(participant.to_uuid())
    .execute(p)
    .await
    .expect("insert room member");
}

async fn room(p: &PgPool, creator: ParticipantId) -> RoomId {
    let id = RoomId::new();
    sqlx::query(
        "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
         VALUES ($1, 'group', $2, $3, now(), $4)",
    )
    .bind(id.to_uuid())
    .bind(format!("call-room-{id}"))
    .bind(creator.to_uuid())
    .bind(uuid::Uuid::parse_str(DEFAULT_WS).expect("uuid"))
    .execute(p)
    .await
    .expect("insert room");
    add_room_member(p, id, creator).await;
    id
}

/// An unanswered call surfaces its callees (for the "missed call" notice);
/// once answered, it no longer does.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn call_session_missed_detection() {
    let p = pool();
    let repo = CallRepo::new(p.clone());
    let caller = participant(&p, "caller").await;
    let target = participant(&p, "callee").await;
    let r = room(&p, caller).await;
    add_room_member(&p, r, target).await;
    let call_id = CallId::new();
    repo.start(
        call_id,
        r,
        caller,
        CallKind::Audio,
        CallMode::P2p,
        &[target],
    )
    .await
    .expect("start call");

    // Never answered → returns (initiator, [callee]).
    let un = repo
        .unanswered_callees(call_id)
        .await
        .unwrap()
        .expect("unanswered");
    assert_eq!(un.0, caller, "initiator");
    assert_eq!(un.1, vec![target], "callees");

    // Answered → no longer a missed call.
    repo.mark_answered(call_id).await.unwrap();
    assert!(
        repo.unanswered_callees(call_id).await.unwrap().is_none(),
        "answered call is not missed"
    );

    // Cleanup (call_participants cascade on call_sessions delete).
    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(call_id.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(r.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
        .bind(vec![caller.to_uuid(), target.to_uuid()])
        .execute(&p)
        .await
        .ok();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn call_session_active_transitions_are_compare_and_set() {
    let p = pool();
    let repo = CallRepo::new(p.clone());
    let caller = participant(&p, "cas-caller").await;
    let member = participant(&p, "cas-member").await;
    let late = participant(&p, "cas-late").await;
    let r = room(&p, caller).await;
    add_room_member(&p, r, member).await;
    add_room_member(&p, r, late).await;
    let call_id = CallId::new();
    repo.start(call_id, r, caller, CallKind::Video, CallMode::Sfu, &[])
        .await
        .expect("start call");

    assert!(repo
        .join_participant_if_active(call_id, member)
        .await
        .unwrap());
    assert!(repo.is_participant(call_id, member).await.unwrap());
    repo.leave_participant(call_id, member).await.unwrap();
    assert!(!repo.is_participant(call_id, member).await.unwrap());
    assert!(
        repo.join_participant_if_active(call_id, member)
            .await
            .unwrap(),
        "a live leg can rejoin"
    );

    assert!(repo.end_if_active(call_id, "done").await.unwrap());
    assert!(
        !repo.end_if_active(call_id, "duplicate").await.unwrap(),
        "only one End wins"
    );
    assert!(
        !repo
            .join_participant_if_active(call_id, late)
            .await
            .unwrap(),
        "an ended call cannot acquire a new leg"
    );
    assert!(
        !repo.mark_answered_if_active(call_id).await.unwrap(),
        "an ended call cannot be answered"
    );

    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(call_id.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(r.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
        .bind(vec![caller.to_uuid(), member.to_uuid(), late.to_uuid()])
        .execute(&p)
        .await
        .ok();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn group_call_initiator_join_is_idempotent_and_keeps_caller_role() {
    let p = pool();
    let repo = CallRepo::new(p.clone());
    let caller = participant(&p, "group-initiator").await;
    let member = participant(&p, "group-member").await;
    let r = room(&p, caller).await;
    add_room_member(&p, r, member).await;
    let call_id = CallId::new();

    repo.start_group_authorized(call_id, r, caller, CallKind::Video)
        .await
        .expect("start group call");
    repo.join_participant_authorized(call_id, caller, r, CallKind::Video)
        .await
        .expect("initiator reconnect must not be reinserted as member");
    repo.join_participant_authorized(call_id, member, r, CallKind::Video)
        .await
        .expect("ordinary member joins");

    let legs = sqlx::query_as::<_, (uuid::Uuid, String)>(
        "SELECT participant_id, role
           FROM call_participants
          WHERE call_id = $1
          ORDER BY participant_id",
    )
    .bind(call_id.to_uuid())
    .fetch_all(&p)
    .await
    .expect("load call legs");
    assert_eq!(legs.len(), 2);
    assert!(legs
        .iter()
        .any(|(id, role)| *id == caller.to_uuid() && role == "caller"));
    assert!(legs
        .iter()
        .any(|(id, role)| *id == member.to_uuid() && role == "member"));

    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(call_id.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(r.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
        .bind(vec![caller.to_uuid(), member.to_uuid()])
        .execute(&p)
        .await
        .ok();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn legacy_v3_initiator_member_upsert_is_conflict_safe() {
    let p = pool();
    let repo = CallRepo::new(p.clone());
    let caller = participant(&p, "legacy-v3-caller").await;
    let r = room(&p, caller).await;
    let call_id = CallId::new();

    repo.start_group_authorized(call_id, r, caller, CallKind::Video)
        .await
        .expect("start canonical group call");

    let legacy_upsert = r"INSERT INTO call_participants
                               (call_id, participant_id, role, joined_at, left_at)
                           SELECT id, $2, 'member', NOW(), NULL
                             FROM call_sessions
                            WHERE id = $1 AND ended_at IS NULL
                      ON CONFLICT (call_id, participant_id)
                      DO UPDATE SET joined_at = EXCLUDED.joined_at,
                                    left_at = NULL
                            WHERE call_participants.left_at IS NOT NULL";

    sqlx::query(legacy_upsert)
        .bind(call_id.to_uuid())
        .bind(caller.to_uuid())
        .execute(&p)
        .await
        .expect("active v3 initiator reconnect conflict is a safe no-op");

    repo.leave_participant(call_id, caller)
        .await
        .expect("close the first caller incarnation");
    sqlx::query(legacy_upsert)
        .bind(call_id.to_uuid())
        .bind(caller.to_uuid())
        .execute(&p)
        .await
        .expect("v3 may reactivate its existing canonical caller row");

    let caller_leg = sqlx::query_as::<_, (String, Option<time::OffsetDateTime>, i64)>(
        "SELECT role, left_at, leg_generation
           FROM call_participants
          WHERE call_id = $1 AND participant_id = $2",
    )
    .bind(call_id.to_uuid())
    .bind(caller.to_uuid())
    .fetch_one(&p)
    .await
    .expect("load canonical caller leg");
    assert_eq!(caller_leg.0, "caller");
    assert!(caller_leg.1.is_none());
    assert_eq!(caller_leg.2, 1, "v3 does not mint a v4 leg generation");

    let mut reconnect = p.begin().await.expect("begin legacy reconnect");
    sqlx::query(legacy_upsert)
        .bind(call_id.to_uuid())
        .bind(caller.to_uuid())
        .execute(&mut *reconnect)
        .await
        .expect("legacy conflict attempt obtains the caller-row lock");
    let delete_pool = p.clone();
    let mut delete = tokio::spawn(async move {
        sqlx::query(
            "DELETE FROM call_participants
              WHERE call_id = $1 AND participant_id = $2",
        )
        .bind(call_id.to_uuid())
        .bind(caller.to_uuid())
        .execute(&delete_pool)
        .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut delete)
            .await
            .is_err(),
        "FOR KEY SHARE must keep the conflict target alive through the statement transaction"
    );
    reconnect.commit().await.expect("commit legacy reconnect");
    tokio::time::timeout(std::time::Duration::from_secs(2), delete)
        .await
        .expect("concurrent delete unblocks")
        .expect("delete task does not panic")
        .expect("delete caller row");

    let forged = sqlx::query(legacy_upsert)
        .bind(call_id.to_uuid())
        .bind(caller.to_uuid())
        .execute(&p)
        .await
        .expect_err("member role cannot create a missing canonical caller leg");
    assert_eq!(
        forged
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("call_participant_caller_identity")
    );

    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(call_id.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(r.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM participants WHERE id = $1")
        .bind(caller.to_uuid())
        .execute(&p)
        .await
        .ok();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn call_leg_generation_fences_stale_leave_across_reconnects() {
    let p = pool();
    let repo = CallRepo::new(p.clone());
    let caller = participant(&p, "generation-caller").await;
    let member = participant(&p, "generation-member").await;
    let r = room(&p, caller).await;
    add_room_member(&p, r, member).await;
    let call_id = CallId::new();

    repo.start_group_authorized(call_id, r, caller, CallKind::Video)
        .await
        .expect("start group call");
    let (_, generation_one) = repo
        .join_participant_authorized_generation(call_id, member, r, CallKind::Video)
        .await
        .expect("first join");
    assert_eq!(generation_one, 1);
    let (_, generation_two) = repo
        .join_participant_authorized_generation(call_id, member, r, CallKind::Video)
        .await
        .expect("active reconnect");
    assert_eq!(generation_two, 2);
    assert_eq!(
        repo.current_participant_generation(call_id, member)
            .await
            .unwrap(),
        Some(generation_two)
    );
    assert!(repo
        .participant_generation_matches(call_id, member, generation_two, true)
        .await
        .unwrap());
    assert!(!repo
        .participant_generation_matches(call_id, member, generation_two, false)
        .await
        .unwrap());
    assert!(!repo
        .leave_participant_if_generation(call_id, member, generation_one)
        .await
        .unwrap());
    assert!(repo
        .leave_participant_if_generation(call_id, member, generation_two)
        .await
        .unwrap());
    assert!(repo
        .current_participant_generation(call_id, member)
        .await
        .unwrap()
        .is_none());
    assert!(repo
        .participant_generation_matches(call_id, member, generation_two, false)
        .await
        .unwrap());

    let (_, generation_three) = repo
        .join_participant_authorized_generation(call_id, member, r, CallKind::Video)
        .await
        .expect("rejoin after leave");
    assert_eq!(generation_three, 3);
    assert!(repo
        .participant_generation_matches(call_id, member, generation_three, true)
        .await
        .unwrap());
    assert!(!repo
        .participant_generation_matches(call_id, member, generation_three, false)
        .await
        .unwrap());
    assert!(matches!(
        repo.leave_authorized_generation(call_id, member, r, generation_two)
            .await,
        Err(Error::Conflict(_))
    ));
    repo.leave_authorized_generation(call_id, member, r, generation_three)
        .await
        .expect("current authorized generation may leave");
    assert!(repo
        .participant_generation_matches(call_id, member, generation_three, false)
        .await
        .unwrap());

    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(call_id.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(r.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
        .bind(vec![caller.to_uuid(), member.to_uuid()])
        .execute(&p)
        .await
        .ok();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn call_authorization_and_room_containment_are_commit_time_invariants() {
    let p = pool();
    let repo = CallRepo::new(p.clone());
    let caller = participant(&p, "authorized-caller").await;
    let target = participant(&p, "authorized-callee").await;
    let outsider = participant(&p, "authorized-outsider").await;
    let r = room(&p, caller).await;
    add_room_member(&p, r, target).await;

    let call_id = CallId::new();
    let (_, callees) = repo
        .start_authorized(call_id, r, caller, CallKind::Audio, CallMode::P2p)
        .await
        .expect("authorized call start");
    assert_eq!(callees, vec![target]);

    repo.answer_authorized(call_id, target, caller, r)
        .await
        .expect("active callee may answer caller");
    assert!(matches!(
        repo.authorize_active(call_id, outsider, r, None, None)
            .await,
        Err(Error::Forbidden(_))
    ));

    let raw_cross_room = sqlx::query(
        "INSERT INTO call_participants
             (call_id, participant_id, role, joined_at)
         VALUES ($1, $2, 'member', NOW())",
    )
    .bind(call_id.to_uuid())
    .bind(outsider.to_uuid())
    .execute(&p)
    .await
    .expect_err("raw cross-room leg must be rejected");
    assert_eq!(
        raw_cross_room
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("call_participant_room_containment")
    );

    sqlx::query(
        "DELETE FROM room_members
          WHERE room_id = $1 AND participant_id = $2",
    )
    .bind(r.to_uuid())
    .bind(target.to_uuid())
    .execute(&p)
    .await
    .expect("revoke room membership");
    assert!(
        !repo.is_participant(call_id, target).await.unwrap(),
        "room leave closes the durable call leg in the same transaction"
    );
    assert!(matches!(
        repo.authorize_active(call_id, target, r, None, None).await,
        Err(Error::Forbidden(_))
    ));

    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(call_id.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(r.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
        .bind(vec![caller.to_uuid(), target.to_uuid(), outsider.to_uuid()])
        .execute(&p)
        .await
        .ok();
}
