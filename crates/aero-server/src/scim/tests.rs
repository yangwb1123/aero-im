//! Unit tests for the `scim` module (loaded via `#[cfg(test)] mod tests;`).
//!
//! `use super::*;` pulls in the schema types and shared helpers from the module
//! root; the group-only items being tested are imported from `super::groups`.

use super::groups::{
    member_filter_value, member_values_from, parse_group_patch_ops, slugify_handle, GroupPatch,
    GroupPatchAction, MAX_GROUP_HANDLE_LEN,
};
use super::users::{map_user_write_error, validate_identity_issuer};
use super::*;

#[test]
fn scim_patch_operations_are_bounded() {
    assert!(validate_patch_operation_count(MAX_SCIM_PATCH_OPERATIONS).is_ok());
    assert!(matches!(
        validate_patch_operation_count(MAX_SCIM_PATCH_OPERATIONS + 1),
        Err(AeroError::Invalid(_))
    ));
}

#[test]
fn scim_token_quota_maps_to_explicit_conflict() {
    match map_token_write_error(ScimTokenWriteError::QuotaExceeded) {
        AeroError::Conflict(message) => {
            assert!(message.contains("SCIM token quota exceeded"));
            assert!(message.contains(&MAX_SCIM_TOKENS_PER_WORKSPACE.to_string()));
        }
        error => panic!("quota must map to conflict, got {error:?}"),
    }
}

#[test]
fn scim_token_authz_errors_keep_forbidden_and_not_found_statuses() {
    assert_eq!(
        map_token_write_error(ScimTokenWriteError::Governance(AeroError::Forbidden(
            "workspace admin required".into(),
        )))
        .status_code(),
        403
    );
    assert_eq!(
        map_token_write_error(ScimTokenWriteError::Governance(AeroError::NotFound(
            "workspace".into(),
        )))
        .status_code(),
        404
    );
    assert_eq!(
        map_token_write_error(ScimTokenWriteError::TokenNotFound).status_code(),
        404
    );
}

#[test]
fn scim_identity_lifecycle_conflicts_map_to_fixed_http_conflicts() {
    for error in [
        aero_storage::ScimUserWriteError::IdentityConflict,
        aero_storage::ScimUserWriteError::IdentityTombstoned,
        aero_storage::ScimUserWriteError::IdentitySubjectImmutable,
    ] {
        let mapped = map_user_write_error(error);
        assert_eq!(mapped.status_code(), 409);
        assert!(
            !mapped.to_string().contains("issuer") && !mapped.to_string().contains("subject"),
            "route errors must not expose external identity keys"
        );
    }
}

#[test]
fn scim_identity_issuer_is_exact_and_bounded() {
    assert!(validate_identity_issuer(&"x".repeat(2 * 1024)).is_ok());
    for invalid in [
        "x".repeat(2 * 1024 + 1),
        " padded".to_owned(),
        "line\nbreak".to_owned(),
    ] {
        assert_eq!(
            validate_identity_issuer(&invalid)
                .expect_err("invalid issuer configuration")
                .status_code(),
            500
        );
    }
}

// ---------- parse_filter ----------

#[test]
fn parse_filter_handles_username_eq() {
    let (attr, value) = parse_filter(r#"userName eq "alice@example.com""#).unwrap();
    assert_eq!(attr, "userName");
    assert_eq!(value, "alice@example.com");
}

#[test]
fn parse_filter_is_case_insensitive_on_operator() {
    let (attr, value) = parse_filter(r#"userName EQ "bob""#).unwrap();
    assert_eq!(attr, "userName");
    assert_eq!(value, "bob");
}

#[test]
fn parse_filter_unescapes_quotes_and_backslashes() {
    let (_, value) = parse_filter(r#"userName eq "a\"b\\c""#).unwrap();
    assert_eq!(value, r#"a"b\c"#);
}

#[test]
fn parse_filter_rejects_unsupported() {
    // Non-eq operators.
    assert!(parse_filter(r#"userName co "ali""#).is_none());
    assert!(parse_filter("userName pr").is_none());
    // Compound filters.
    assert!(parse_filter(r#"userName eq "a" and active eq "true""#).is_none());
    // Missing / malformed value.
    assert!(parse_filter("userName eq alice").is_none());
    assert!(parse_filter(r#"userName eq "unterminated"#).is_none());
    assert!(parse_filter("").is_none());
    assert!(parse_filter("eq").is_none());
}

#[test]
fn parse_filter_handles_empty_value() {
    let (attr, value) = parse_filter(r#"userName eq """#).unwrap();
    assert_eq!(attr, "userName");
    assert_eq!(value, "");
}

// ---------- ScimUser (de)serialization round-trip ----------

#[test]
fn scim_user_roundtrips_camelcase() {
    let user = ScimUser {
        schemas: vec![SCHEMA_USER.to_owned()],
        id: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned()),
        external_id: Some("ext-7".to_owned()),
        user_name: "carol@example.com".to_owned(),
        name: Some(ScimName {
            given_name: Some("Carol".to_owned()),
            family_name: Some("Danvers".to_owned()),
            formatted: Some("Carol Danvers".to_owned()),
        }),
        emails: vec![ScimEmail {
            value: "carol@example.com".to_owned(),
            primary: true,
            kind: Some("work".to_owned()),
        }],
        active: true,
        meta: Some(ScimMeta {
            resource_type: "User".to_owned(),
            created: None,
            last_modified: None,
            location: Some("/scim/v2/Users/x".to_owned()),
        }),
    };
    let json = serde_json::to_value(&user).unwrap();
    // The renamed camelCase fields must be on the wire exactly.
    assert_eq!(json["userName"], "carol@example.com");
    assert_eq!(json["externalId"], "ext-7");
    assert_eq!(json["name"]["givenName"], "Carol");
    assert_eq!(json["name"]["familyName"], "Danvers");
    assert_eq!(json["emails"][0]["type"], "work");
    assert_eq!(json["meta"]["resourceType"], "User");
    // Round-trip back to the struct.
    let back: ScimUser = serde_json::from_value(json).unwrap();
    assert_eq!(back, user);
}

#[test]
fn scim_user_deserializes_minimal_okta_payload() {
    // What Okta sends on create: schemas + userName + active, nothing else.
    let raw = serde_json::json!({
        "schemas": [SCHEMA_USER],
        "userName": "dave@example.com",
        "active": true
    });
    let user: ScimUser = serde_json::from_value(raw).unwrap();
    assert_eq!(user.user_name, "dave@example.com");
    assert!(user.active);
    assert!(user.name.is_none());
    assert!(user.emails.is_empty());
    assert!(user.id.is_none());
}

#[test]
fn scim_user_active_defaults_to_true_when_absent() {
    let raw = serde_json::json!({ "schemas": [SCHEMA_USER], "userName": "e" });
    let user: ScimUser = serde_json::from_value(raw).unwrap();
    assert!(user.active, "absent active defaults to true");
}

// ---------- ListResponse round-trip ----------

#[test]
fn list_response_roundtrips_camelcase() {
    let list = ScimListResponse::new(vec!["a".to_owned(), "b".to_owned()], 5, 1);
    let json = serde_json::to_value(&list).unwrap();
    assert_eq!(json["schemas"][0], SCHEMA_LIST);
    assert_eq!(json["totalResults"], 5);
    assert_eq!(json["startIndex"], 1);
    assert_eq!(json["itemsPerPage"], 2);
    assert_eq!(json["Resources"][1], "b");
    let back: ScimListResponse<String> = serde_json::from_value(json).unwrap();
    assert_eq!(back, list);
}

#[test]
fn scim_error_status_is_a_string() {
    let json = serde_json::to_value(ScimError::new(409, "userName exists")).unwrap();
    assert_eq!(json["status"], "409", "RFC 7644 carries status as a string");
    assert_eq!(json["detail"], "userName exists");
    assert_eq!(json["schemas"][0], SCHEMA_ERROR);
}

// ---------- display_name_for ----------

#[test]
fn display_name_prefers_formatted_then_parts_then_username() {
    let mut u = ScimUser {
        schemas: vec![],
        id: None,
        external_id: None,
        user_name: "fallback@x.com".to_owned(),
        name: None,
        emails: vec![],
        active: true,
        meta: None,
    };
    // No name → userName.
    assert_eq!(display_name_for(&u), "fallback@x.com");
    // given+family.
    u.name = Some(ScimName {
        given_name: Some("Grace".to_owned()),
        family_name: Some("Hopper".to_owned()),
        formatted: None,
    });
    assert_eq!(display_name_for(&u), "Grace Hopper");
    // formatted wins.
    u.name = Some(ScimName {
        given_name: Some("Grace".to_owned()),
        family_name: Some("Hopper".to_owned()),
        formatted: Some("Rear Admiral Grace Hopper".to_owned()),
    });
    assert_eq!(display_name_for(&u), "Rear Admiral Grace Hopper");
}

// ---------- ScimGroup (de)serialization ----------

#[test]
fn scim_group_roundtrips_camelcase() {
    let group = ScimGroup {
        schemas: vec![SCHEMA_GROUP.to_owned()],
        id: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned()),
        display_name: "Design Team".to_owned(),
        members: vec![
            ScimGroupMember {
                value: "01BX5ZZKBKACTAV9WEVGEMMVRZ".to_owned(),
                display: Some("Alice".to_owned()),
            },
            ScimGroupMember {
                value: "01BX5ZZKBKACTAV9WEVGEMMVS0".to_owned(),
                display: None,
            },
        ],
        meta: Some(ScimMeta {
            resource_type: "Group".to_owned(),
            created: None,
            last_modified: None,
            location: Some("/scim/v2/Groups/x".to_owned()),
        }),
    };
    let json = serde_json::to_value(&group).unwrap();
    assert_eq!(json["schemas"][0], SCHEMA_GROUP);
    assert_eq!(json["displayName"], "Design Team");
    assert_eq!(json["members"][0]["value"], "01BX5ZZKBKACTAV9WEVGEMMVRZ");
    assert_eq!(json["members"][0]["display"], "Alice");
    // A member with no `display` omits the key (skip_serializing_if).
    assert!(json["members"][1].get("display").is_none());
    assert_eq!(json["meta"]["resourceType"], "Group");
    let back: ScimGroup = serde_json::from_value(json).unwrap();
    assert_eq!(back, group);
}

#[test]
fn scim_group_deserializes_minimal_idp_create() {
    // What an IdP sends to create a group: schemas + displayName + members.
    let raw = serde_json::json!({
        "schemas": [SCHEMA_GROUP],
        "displayName": "Engineers",
        "members": [{ "value": "01BX5ZZKBKACTAV9WEVGEMMVRZ" }]
    });
    let g: ScimGroup = serde_json::from_value(raw).unwrap();
    assert_eq!(g.display_name, "Engineers");
    assert_eq!(g.members.len(), 1);
    assert_eq!(g.members[0].value, "01BX5ZZKBKACTAV9WEVGEMMVRZ");
    assert!(g.members[0].display.is_none());
    assert!(g.id.is_none());
}

// ---------- slugify_handle ----------

#[test]
fn slugify_handle_makes_a_valid_mention_handle() {
    assert_eq!(slugify_handle("Design Team"), "design-team");
    assert_eq!(slugify_handle("  Backend  Eng  "), "backend-eng");
    assert_eq!(slugify_handle("Devs & Ops!"), "devs-ops");
    assert_eq!(slugify_handle("under_score-ok"), "under_score-ok");
    // Non-ascii / emoji are dropped; a fully-degenerate name falls back.
    assert_eq!(slugify_handle("✨"), "group");
    assert_eq!(slugify_handle(""), "group");
    assert_eq!(slugify_handle("---"), "group");
    // Result is always a valid `[a-z0-9_-]`, 1..=32 handle.
    for input in [
        "A Very Very Very Long Group Display Name That Exceeds",
        "déjà vu",
        "  ",
    ] {
        let h = slugify_handle(input);
        assert!(
            !h.is_empty() && h.chars().count() <= MAX_GROUP_HANDLE_LEN,
            "{input:?} -> {h:?}"
        );
        assert!(
            h.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'),
            "{input:?} -> {h:?} has illegal chars"
        );
        assert!(
            !h.ends_with('-') && !h.starts_with('-'),
            "{input:?} -> {h:?} has edge dash"
        );
    }
}

// ---------- parse_group_patch_ops ----------

fn group_ops(raw: serde_json::Value) -> Vec<GroupPatchAction> {
    let patch: GroupPatch = serde_json::from_value(raw).unwrap();
    parse_group_patch_ops(&patch.operations)
}

#[test]
fn group_patch_parses_member_add() {
    // The Azure AD / Okta shape: add to `members` with an array value.
    let actions = group_ops(serde_json::json!({
        "Operations": [{
            "op": "add",
            "path": "members",
            "value": [{ "value": "01BX5ZZKBKACTAV9WEVGEMMVRZ" }]
        }]
    }));
    assert_eq!(
        actions,
        vec![GroupPatchAction::AddMember(
            "01BX5ZZKBKACTAV9WEVGEMMVRZ".to_owned()
        )]
    );
}

#[test]
fn group_patch_parses_member_remove_plain_and_filtered() {
    // Plain `remove members` with a value array.
    let plain = group_ops(serde_json::json!({
        "Operations": [{
            "op": "remove",
            "path": "members",
            "value": [{ "value": "01AAA" }, { "value": "01BBB" }]
        }]
    }));
    assert_eq!(
        plain,
        vec![
            GroupPatchAction::RemoveMember("01AAA".to_owned()),
            GroupPatchAction::RemoveMember("01BBB".to_owned()),
        ]
    );
    // Targeted `members[value eq "<id>"]` remove (the Okta deprovision shape).
    let filtered = group_ops(serde_json::json!({
        "Operations": [{ "op": "remove", "path": r#"members[value eq "01CCC"]"# }]
    }));
    assert_eq!(
        filtered,
        vec![GroupPatchAction::RemoveMember("01CCC".to_owned())]
    );
}

#[test]
fn group_patch_parses_displayname_replace() {
    let by_path = group_ops(serde_json::json!({
        "Operations": [{ "op": "replace", "path": "displayName", "value": "Renamed" }]
    }));
    assert_eq!(
        by_path,
        vec![GroupPatchAction::SetDisplayName("Renamed".to_owned())]
    );
    // Path-less replace carrying the whole resource (Azure sometimes does this).
    let pathless = group_ops(serde_json::json!({
        "Operations": [{
            "op": "replace",
            "value": { "displayName": "WholeReplace", "members": [{ "value": "01DDD" }] }
        }]
    }));
    assert_eq!(
        pathless,
        vec![
            GroupPatchAction::SetDisplayName("WholeReplace".to_owned()),
            GroupPatchAction::AddMember("01DDD".to_owned()),
        ]
    );
}

#[test]
fn group_patch_skips_unknown_ops_and_paths() {
    let actions = group_ops(serde_json::json!({
        "Operations": [
            { "op": "replace", "path": "externalId", "value": "x" },
            { "op": "frobnicate", "path": "members", "value": [{ "value": "01EEE" }] },
            { "op": "add", "path": "members", "value": [{ "value": "01FFF" }] }
        ]
    }));
    // Only the well-formed member add survives.
    assert_eq!(
        actions,
        vec![GroupPatchAction::AddMember("01FFF".to_owned())]
    );
}

#[test]
fn member_filter_value_extracts_quoted_id() {
    assert_eq!(
        member_filter_value(r#"members[value eq "01GGG"]"#),
        Some("01GGG".to_owned())
    );
    // A non-`value` attribute or malformed filter yields None.
    assert!(member_filter_value(r#"members[display eq "x"]"#).is_none());
    assert!(member_filter_value("members[]").is_none());
    assert!(member_filter_value("members").is_none());
}

#[test]
fn member_values_from_handles_array_object_and_string() {
    // Array of member objects.
    assert_eq!(
        member_values_from(&serde_json::json!([{ "value": "a" }, { "value": "b" }])),
        vec!["a".to_owned(), "b".to_owned()]
    );
    // A single member object.
    assert_eq!(
        member_values_from(&serde_json::json!({ "value": "c" })),
        vec!["c".to_owned()]
    );
    // A bare string.
    assert_eq!(
        member_values_from(&serde_json::json!("d")),
        vec!["d".to_owned()]
    );
}
