# aero-auth

Authentication primitives and service orchestration for Aero IM: Argon2id
password hashing, RS256 JWT sessions, PAT and bot-token fallback, OIDC/JWKS,
TOTP, password policy, login throttling, and the Axum `AuthUser` extractor.

Workspace and room authorization is deliberately enforced by higher layers;
this crate establishes and validates caller identity.

```bash
cargo test -p aero-auth --lib
```
