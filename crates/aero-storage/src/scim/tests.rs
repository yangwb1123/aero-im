use super::*;
use aero_common::ScimTokenId;

#[test]
fn hash_token_is_stable_hex_sha256() {
    // Known SHA-256 of the empty string and "abc" (FIPS 180-4 examples).
    assert_eq!(
        hash_token(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hash_token("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    // Hex, 64 chars, deterministic.
    let h = hash_token("a-scim-secret");
    assert_eq!(h.len(), 64);
    assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(h, hash_token("a-scim-secret"));
    assert_ne!(h, hash_token("a-scim-secre")); // sensitive to input
}

#[test]
fn generate_token_is_prefixed_high_entropy_and_hashable() {
    let a = generate_token();
    let b = generate_token();
    assert!(a.starts_with("scim_"));
    assert_eq!(a.len(), "scim_".len() + 64, "256-bit hex secret");
    assert_ne!(a, b, "two mints differ");
    // The generated secret hashes to a stable 64-char hex digest.
    assert_eq!(hash_token(&a).len(), 64);
}

#[test]
fn token_inventory_projection_never_serializes_credential_material() {
    let row = ScimTokenRecord {
        id: ScimTokenId::new(),
        workspace_id: WorkspaceId::new(),
        label: Some("Okta production".into()),
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        revoked_at: Some(time::OffsetDateTime::UNIX_EPOCH),
    };
    let json = serde_json::to_value(row).unwrap();
    assert!(json.get("id").is_some());
    assert!(json.get("label").is_some());
    assert_eq!(
        json.get("created_at").and_then(serde_json::Value::as_str),
        Some("1970-01-01T00:00:00Z")
    );
    assert_eq!(
        json.get("revoked_at").and_then(serde_json::Value::as_str),
        Some("1970-01-01T00:00:00Z")
    );
    assert!(json.get("token").is_none());
    assert!(json.get("token_hash").is_none());
    assert!(json.get("secret").is_none());
}

#[test]
fn clamp_count_defaults_and_bounds() {
    assert_eq!(clamp_count(None), MAX_PAGE);
    assert_eq!(clamp_count(Some(0)), MAX_PAGE);
    assert_eq!(clamp_count(Some(-5)), MAX_PAGE);
    assert_eq!(clamp_count(Some(1)), 1);
    assert_eq!(clamp_count(Some(50)), 50);
    assert_eq!(clamp_count(Some(10_000)), MAX_PAGE);
}

#[test]
fn start_offset_is_one_based_to_zero_based() {
    assert_eq!(start_offset(None), 0);
    assert_eq!(start_offset(Some(0)), 0); // invalid, clamp to first page
    assert_eq!(start_offset(Some(-3)), 0);
    assert_eq!(start_offset(Some(1)), 0); // SCIM startIndex is 1-based
    assert_eq!(start_offset(Some(2)), 1);
    assert_eq!(start_offset(Some(51)), 50);
}
