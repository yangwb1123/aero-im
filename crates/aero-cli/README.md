# aero-cli

Lightweight binary package for the engineering CLI. It builds `aero-eng` from
the shared command registry without linking the database, media, or gateway
stack, making repository checks and shell completion fast to install and run.

The full database-capable `aero-cli` binary is built by the `aero-server`
package; both use the canonical implementations from the `aero-eng` library.

```bash
cargo test -p aero-cli
cargo run -p aero-cli --bin aero-eng -- gate list
```
