//! Tests for [`super`], split out to keep the parent under the 1200-line HARD limit.

use super::*;
use crate::test_util::MockBus;

#[test]
fn room_subject_is_well_formed() {
    let room = RoomId::new();
    let subject = ImService::room_subject(room);
    assert!(subject.starts_with("im.room."));
    assert!(subject.ends_with(&room.to_string()));
}

#[test]
fn notify_delivery_id_is_deterministic_per_message_and_kind() {
    let m1 = aero_common::MessageId::new();
    let m2 = aero_common::MessageId::new();

    // Same message + same batch kind => identical id (so a redelivery of the
    // same batch derives the same delivery_id and ON CONFLICT can de-dup).
    assert_eq!(
        notify_delivery_id(m1, NotifyBatchKind::Mention),
        notify_delivery_id(m1, NotifyBatchKind::Mention),
        "deterministic for (message, kind)"
    );
    assert_eq!(
        notify_delivery_id(m1, NotifyBatchKind::Reply),
        notify_delivery_id(m1, NotifyBatchKind::Reply),
    );

    // Mention vs reply of the SAME message must differ — a recipient
    // legitimately on both batches must not be collapsed.
    assert_ne!(
        notify_delivery_id(m1, NotifyBatchKind::Mention),
        notify_delivery_id(m1, NotifyBatchKind::Reply),
        "mention batch and reply batch get distinct ids"
    );

    // Different messages get different ids (same kind).
    assert_ne!(
        notify_delivery_id(m1, NotifyBatchKind::Mention),
        notify_delivery_id(m2, NotifyBatchKind::Mention),
        "distinct messages => distinct ids"
    );

    // Derived ids are non-nil v5 UUIDs.
    let id = notify_delivery_id(m1, NotifyBatchKind::Mention);
    assert_ne!(id, uuid::Uuid::nil());
    assert_eq!(id.get_version(), Some(uuid::Version::Sha1));
}

#[test]
fn mentioned_participants_dedups_in_order_and_ignores_non_mentions() {
    let a = ParticipantId::new();
    let b = ParticipantId::new();
    let blocks = vec![
        Block::text("hey"),
        Block::Mention { participant: a },
        Block::text("and"),
        Block::Mention { participant: b },
        // duplicate mention of `a` is collapsed
        Block::Mention { participant: a },
    ];
    let got = mentioned_participants(&blocks);
    assert_eq!(got, vec![a, b], "distinct mentions, first-appearance order");

    // No mentions => empty.
    assert!(mentioned_participants(&[Block::text("plain")]).is_empty());
}

#[test]
fn group_handle_tokens_extracts_lowercases_and_dedups() {
    let blocks = vec![
        Block::text("hey @Eng and @ops, ping @eng again"),
        Block::text("also @on-call_team! and a bare @ and email a@b.com"),
    ];
    let got = group_handle_tokens(&blocks);
    // first-appearance order, lowercased, deduped; "eng" not repeated.
    assert_eq!(got, vec!["eng", "ops", "on-call_team", "b"]);
    // The bare "@ " yields no token; non-text blocks are ignored.
    assert!(group_handle_tokens(&[Block::text("no handles here")]).is_empty());
    assert!(group_handle_tokens(&[Block::Mention {
        participant: ParticipantId::new()
    }])
    .is_empty());
}

#[test]
fn broadcast_tokens_recognized_case_insensitively() {
    // group_handle_tokens lowercases, so is_broadcast_token sees lowercase.
    for t in ["channel", "everyone", "all", "here"] {
        assert!(is_broadcast_token(t), "{t} is a broadcast mention");
    }
    for t in ["eng", "ops", "channels", "everybody", ""] {
        assert!(!is_broadcast_token(t), "{t} is NOT a broadcast mention");
    }
    // End-to-end through the token extractor: "@channel" in text is detected.
    let toks = group_handle_tokens(&[Block::text("hey @Channel ship it")]);
    assert!(
        toks.iter().any(|t| is_broadcast_token(t)),
        "@Channel detected"
    );
}

#[test]
fn here_is_distinct_from_channel_class_broadcasts() {
    // `@here` is its own (online-only) flavour, NOT a "notify everyone" token.
    assert!(is_here_token("here"));
    assert!(
        !is_all_broadcast_token("here"),
        "@here must NOT fan out to all"
    );

    // `@channel` / `@everyone` / `@all` stay full-fan-out and are NOT `@here`.
    for t in ["channel", "everyone", "all"] {
        assert!(is_all_broadcast_token(t), "{t} fans out to everyone");
        assert!(!is_here_token(t), "{t} is not @here");
    }

    // Every token in both classes is still a broadcast (skipped by group
    // resolution); a non-broadcast handle is neither.
    for t in ["here", "channel", "everyone", "all"] {
        assert!(is_broadcast_token(t));
    }
    for t in ["eng", "ops", "here_team", ""] {
        assert!(!is_here_token(t), "{t} is not @here");
        assert!(!is_all_broadcast_token(t));
    }
}

#[test]
fn mock_bus_records_publish() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let bus = Arc::new(MockBus::default());
    rt.block_on(async {
        bus.publish_json("im.test", &serde_json::json!({"k": "v"}))
            .await
            .unwrap();
    });
    let log = bus.published.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].0, "im.test");
}

#[test]
fn publish_event_helper_through_dyn() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let bus = Arc::new(MockBus::default());
    let dyn_bus: Arc<dyn super::BusSink> = bus.clone();
    rt.block_on(async {
        crate::service::events::publish_event(
            dyn_bus.as_ref(),
            "im.events.room.created",
            &serde_json::json!({"ok": true}),
        )
        .await
        .unwrap();
    });
    let log = bus.published.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].0, "im.events.room.created");
    assert!(std::str::from_utf8(&log[0].1)
        .unwrap()
        .contains("\"ok\":true"));
}

// ---- Pure tenancy-authorization decisions (DB-free, exhaustive) ----

const ALL_ROLES: [WorkspaceRole; 4] = [
    WorkspaceRole::Guest,
    WorkspaceRole::Member,
    WorkspaceRole::Admin,
    WorkspaceRole::Owner,
];

#[test]
fn can_create_channel_is_member_and_above() {
    assert!(!can_create_channel(WorkspaceRole::Guest));
    assert!(can_create_channel(WorkspaceRole::Member));
    assert!(can_create_channel(WorkspaceRole::Admin));
    assert!(can_create_channel(WorkspaceRole::Owner));
}

#[test]
fn can_create_channel_matches_at_least_member_for_all_roles() {
    for r in ALL_ROLES {
        assert_eq!(
            can_create_channel(r),
            r.at_least(WorkspaceRole::Member),
            "role {r:?}"
        );
    }
}

#[test]
fn can_access_room_requires_both_memberships() {
    // Full truth table over (workspace_member, room_member).
    assert!(!can_access_room(false, false));
    assert!(!can_access_room(true, false));
    assert!(!can_access_room(false, true));
    assert!(can_access_room(true, true));
}

#[test]
fn can_access_room_is_logical_and() {
    for ws in [false, true] {
        for room in [false, true] {
            assert_eq!(can_access_room(ws, room), ws && room, "({ws}, {room})");
        }
    }
}

#[test]
fn can_join_public_channel_requires_public_and_not_archived() {
    // Truth table over (is_private, is_archived): joinable only when public
    // AND not archived.
    assert!(can_join_public_channel(false, false));
    assert!(!can_join_public_channel(true, false));
    assert!(!can_join_public_channel(false, true));
    assert!(!can_join_public_channel(true, true));
}

#[test]
fn can_join_public_channel_is_neither_private_nor_archived() {
    for private in [false, true] {
        for archived in [false, true] {
            assert_eq!(
                can_join_public_channel(private, archived),
                !private && !archived,
                "({private}, {archived})"
            );
        }
    }
}

#[test]
fn env_truthy_accepts_one_and_case_insensitive_true_only() {
    for v in ["1", "true", "True", "TRUE", "tRuE"] {
        assert!(env_truthy(v), "{v:?} should be truthy");
    }
    for v in [
        "", "0", "false", "False", "yes", "on", "2", "truee", " true",
    ] {
        assert!(!env_truthy(v), "{v:?} should NOT be truthy");
    }
}

#[test]
fn post_allowed_everyone_is_always_true() {
    // The open default ignores admin/creator standing entirely.
    for is_admin in [false, true] {
        for is_creator in [false, true] {
            assert!(
                post_allowed("everyone", is_admin, is_creator),
                "everyone always permits (admin={is_admin}, creator={is_creator})"
            );
        }
    }
}

#[test]
fn post_allowed_admins_requires_admin_or_creator() {
    // 'admins' permits exactly the admin OR the creator; a plain member is denied.
    assert!(
        post_allowed("admins", true, false),
        "workspace admin may post"
    );
    assert!(post_allowed("admins", false, true), "room creator may post");
    assert!(post_allowed("admins", true, true), "admin+creator may post");
    assert!(
        !post_allowed("admins", false, false),
        "a plain member may not post in an announcements-only channel"
    );
}

#[test]
fn post_allowed_admins_is_logical_or_of_admin_and_creator() {
    for is_admin in [false, true] {
        for is_creator in [false, true] {
            assert_eq!(
                post_allowed("admins", is_admin, is_creator),
                is_admin || is_creator,
                "(admin={is_admin}, creator={is_creator})"
            );
        }
    }
}

#[test]
fn post_allowed_unknown_policy_falls_back_to_everyone() {
    // An unrecognized/typo'd policy fails OPEN (treated as everyone), never a
    // silent lockout — matches the read-side default in `RoomRepo::post_policy`.
    for is_admin in [false, true] {
        for is_creator in [false, true] {
            assert!(
                post_allowed("bogus", is_admin, is_creator),
                "unknown policy permits like everyone (admin={is_admin}, creator={is_creator})"
            );
            assert!(
                post_allowed("", is_admin, is_creator),
                "empty policy permits"
            );
        }
    }
}
