# aero-storage

Persistence layer for Aero IM. Feature-scoped repositories encapsulate
PostgreSQL/sqlx queries, while Redis-backed stores hold cluster presence,
viewer, call-roster, routing, and rate-limit state. Blob storage supports local
filesystem, S3-compatible, regional, and Aero Vault backends.

SQL and storage state machines stay in this crate; HTTP authorization and
business orchestration belong to the service and server layers.

```bash
cargo test -p aero-storage --lib
```

Database-gated tests require a freshly migrated disposable PostgreSQL database.
