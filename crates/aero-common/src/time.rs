//! Time helpers — single source of truth for "now" so tests can freeze time.

use time::OffsetDateTime;

#[must_use]
pub fn now_utc() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}
