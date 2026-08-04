use super::*;

mod blob_quota;
#[path = "debug_tests.rs"]
mod debug_tests;
mod dm_blob;
mod installation_idempotency;
mod issuer_rotation;
mod machine_safety;
mod shared;
mod workspace_gc;

use shared::*;
