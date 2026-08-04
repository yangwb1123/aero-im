# str0m 0.19 Rust 1.80 compatibility closure

This directory is the source-only dependency closure used by Aero IM's
`aero-live-whip` and `aero-live-webrtc` crates. It keeps the str0m API at
0.19 while preserving the workspace MSRV of Rust 1.80.

Only each crate's build manifest, original manifest, `src/` tree, and available
license files are retained. Upstream examples, integration tests, benchmarks,
documentation, and lockfiles are intentionally omitted. `crc` is not vendored;
the workspace lockfile pins its compatible 3.3 release.

## Upstream provenance

| Crate | Version | Registry VCS revision |
| --- | --- | --- |
| str0m | 0.19.0 | `c4eb63fd3d894d5906d7cab5ea4b97cfca189b1d` |
| str0m-proto | 0.5.0 | `00841ee461dfd1b68f7a5376c03b5d1cb30ffdc1` |
| str0m-rust-crypto | 0.4.0 | `00841ee461dfd1b68f7a5376c03b5d1cb30ffdc1` |
| dimpl | 0.6.1 | `37bb0fa83f4167420729de5ea71c61852f82e9ed` |
| is | 0.9.0 | `c4eb63fd3d894d5906d7cab5ea4b97cfca189b1d` |
| sctp-proto | 0.9.0 | `888891d85f4b42624491399fcc267aaed26fd922` |
| rcgen | 0.14.7 | `ee434c51053db0d4781e1b290ce9bae63fb8050b` |

## Local compatibility patches

- Backport edition-2024 syntax and crate metadata to edition 2021 / Rust 1.80
  in the str0m 0.19 dependency closure.
- Keep sctp-proto's SNAP-capable 0.9 API while constraining `crc` to 3.3.0,
  whose implementation remains compatible with Rust 1.80.
- Generate the ephemeral DTLS certificate with p256/PKCS#8 and SHA-256 from
  RustCrypto, and explicitly install dimpl's RustCrypto provider.
- Disable rcgen's built-in native crypto providers for this path. rcgen is used
  only as an X.509 encoder around the local RustCrypto signing key.

The scope of the native-free statement is the str0m DTLS/SRTP dependency
subtree. It does not cover unrelated TLS clients used elsewhere by the two Aero
media crates or the workspace.

## Updating

Start from the exact crates.io releases above, reapply the small manifest and
source patches, then run both media crates' all-target checks/tests, the exact
Rust 1.80 workspace check with `--locked`, and inspect `cargo tree -p str0m` for
native crypto/build dependencies. Keep this directory excluded from workspace
membership: it is consumed only through the two explicit path dependencies.
