use std::sync::Arc;

use aero_common::{Block, RoomKind};
use aero_storage::{AutoModRuleRepo, MessageRepo, ParticipantRepo};

use super::{new_participant, pool, service};

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn configured_auto_mod_rule_is_enforced_without_an_environment_gate() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let alice = new_participant(&participants, "auto-mod-enforced").await;
    let setup = service(pool.clone());
    let room = setup
        .create_room(alice.id, RoomKind::Group, Some("auto-mod".into()))
        .await
        .unwrap();
    let rules = AutoModRuleRepo::new(pool.clone());
    rules
        .create(
            aero_common::WorkspaceId::from_uuid(uuid::Uuid::nil()),
            "forbidden phrase",
            "contains",
            "block",
            alice.id,
        )
        .await
        .unwrap();
    rules
        .create(
            aero_common::WorkspaceId::from_uuid(uuid::Uuid::nil()),
            "forbidden.example",
            "contains",
            "block",
            alice.id,
        )
        .await
        .unwrap();

    let svc = service(pool).with_auto_mod_rules(rules);
    let error = svc
        .send_message(
            alice.id,
            room.id,
            vec![Block::text("contains FORBIDDEN PHRASE here")],
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, aero_common::Error::Invalid(_)));
    let structured_error = svc
        .send_message(
            alice.id,
            room.id,
            vec![Block::Card {
                schema: "generic".into(),
                payload: serde_json::json!({
                    "title": "clean title",
                    "body": {"nested": "FORBIDDEN PHRASE"}
                }),
            }],
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(structured_error, aero_common::Error::Invalid(_)));
    let link_error = svc
        .send_message(
            alice.id,
            room.id,
            vec![Block::Button {
                action_id: "safe-action".into(),
                label: "clean label".into(),
                style: None,
                url: Some("https://forbidden.example/path".into()),
            }],
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(link_error, aero_common::Error::Invalid(_)));
    svc.send_message(
        alice.id,
        room.id,
        vec![Block::text("clean message")],
        None,
        None,
    )
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn edit_rechecks_keyword_pii_and_workspace_auto_mod_before_writing() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let alice = new_participant(&participants, "edit-policy").await;
    let setup = service(pool.clone());
    let room = setup
        .create_room(alice.id, RoomKind::Group, Some("edit-policy".into()))
        .await
        .unwrap();

    let auto_pattern = format!("auto-mod-edit-{}", aero_common::MessageId::new());
    let rules = AutoModRuleRepo::new(pool.clone());
    rules
        .create(
            aero_common::WorkspaceId::from_uuid(uuid::Uuid::nil()),
            &auto_pattern,
            "contains",
            "block",
            alice.id,
        )
        .await
        .unwrap();
    let svc = service(pool.clone())
        .with_moderator(Arc::new(crate::KeywordModerator::new(vec![
            "edit-keyword-block".into(),
        ])))
        .with_pii_detector(Arc::new(crate::PiiDetector::new(
            crate::PiiConfig::default(),
        )))
        .with_auto_mod_rules(rules);
    let original = svc
        .send_message(
            alice.id,
            room.id,
            vec![Block::text("clean original")],
            None,
            None,
        )
        .await
        .unwrap();

    let keyword_error = svc
        .edit_message(
            alice.id,
            original.id,
            vec![Block::Card {
                schema: "generic".into(),
                payload: serde_json::json!({
                    "title": "clean",
                    "body": {"nested": "edit-keyword-block"}
                }),
            }],
            Some(original.version),
        )
        .await
        .unwrap_err();
    assert!(matches!(keyword_error, aero_common::Error::Invalid(_)));

    let pii_error = svc
        .edit_message(
            alice.id,
            original.id,
            vec![Block::ToolCall {
                tool: "lookup".into(),
                args: serde_json::json!({"query": "clean"}),
                result: Some(serde_json::json!({
                    "nested": {"card": "4111 1111 1111 1111"}
                })),
            }],
            Some(original.version),
        )
        .await
        .unwrap_err();
    assert!(matches!(pii_error, aero_common::Error::Invalid(_)));

    let auto_mod_error = svc
        .edit_message(
            alice.id,
            original.id,
            vec![Block::Select {
                action_id: "blocked-select".into(),
                placeholder: Some(auto_pattern),
                options: vec![aero_common::SelectOption {
                    value: "clean-machine-value".into(),
                    label: "clean label".into(),
                }],
            }],
            Some(original.version),
        )
        .await
        .unwrap_err();
    assert!(matches!(auto_mod_error, aero_common::Error::Invalid(_)));

    let unchanged = MessageRepo::new(pool)
        .get(original.id)
        .await
        .unwrap()
        .expect("original remains");
    assert_eq!(unchanged.version, original.version);
    assert_eq!(unchanged.searchable_text(), "clean original");
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn edit_rechecks_current_announcement_post_policy() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let owner = new_participant(&participants, "edit-announcement-owner").await;
    let member = new_participant(&participants, "edit-announcement-member").await;
    let svc = service(pool.clone());
    let room = svc
        .create_room(
            owner.id,
            RoomKind::Channel,
            Some("edit-announcement".into()),
        )
        .await
        .unwrap();
    svc.add_member(owner.id, room.id, member.id).await.unwrap();

    let original = svc
        .send_message(
            member.id,
            room.id,
            vec![Block::text("posted while open")],
            None,
            None,
        )
        .await
        .unwrap();
    svc.set_room_post_policy(owner.id, room.id, "admins")
        .await
        .unwrap();

    let error = svc
        .edit_message(
            member.id,
            original.id,
            vec![Block::text("announcement policy bypass")],
            Some(original.version),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, aero_common::Error::Forbidden(_)));

    let unchanged = MessageRepo::new(pool)
        .get(original.id)
        .await
        .unwrap()
        .expect("original remains");
    assert_eq!(unchanged.version, original.version);
    assert_eq!(unchanged.searchable_text(), "posted while open");
}
