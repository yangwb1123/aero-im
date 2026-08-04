# uppsala 0.4 Rust 1.80 compatibility patch

This is the source-only `uppsala` 0.4.0 package used by Aero IM's bounded SAML
XML parsing path. It comes from crates.io revision
`18517162a315232b577915e946fbe24dbb2f517b` and retains the upstream
BSD-2-Clause `LICENSE`.

Published versions 0.2.0 through 0.4.0 all use the same integer
`is_multiple_of` calls, which are unavailable on Rust 1.80. The complete 0.1.x
series compiles on Rust 1.80 but predates the namespace-aware DOM helpers used
by the SAML validation code; 0.2.0 still lacks those helpers. No published
release is both Rust-1.80-compatible and API-compatible with this SAML path.

The only source change is a behavior-equivalent mechanical rewrite of five
integer method calls across three expressions:

- divisibility by 4/100/400 uses remainder comparisons;
- odd hex length uses `len % 2 != 0`;
- non-quad base64 length uses `len % 4 != 0`.

The package's `src/`, manifests, and license are retained. Upstream examples,
integration tests, documentation, and lockfile are omitted; the package is
excluded from workspace membership and consumed through one explicit path
dependency.
