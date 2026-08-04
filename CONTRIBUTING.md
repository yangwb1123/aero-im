# Contributing to Aero IM

## Development Setup

### Prerequisites

- Rust 1.80+ (`rustup toolchain install 1.80.0`)
- Docker & Docker Compose (for PostgreSQL/Redis/NATS)
- OpenSSL (for JWT key generation)

### Quick Start

```bash
# 1. Start infrastructure services
make up

# 2. Generate JWT keys and config
make jwt-keys env

# 3. Apply database migrations
make migrate

# 4. Run tests
make test

# 5. Start the server
make run
```

Or use the all-in-one command:

```bash
make dev
```

## Project Structure

```
crates/
├── aero-common/      Shared types, config, telemetry (leaf crate)
├── aero-eng/         Engineering CLI framework (standalone)
├── aero-cli/         Lightweight CLI binary (7 commands, 14 MB)
├── aero-bus/         NATS JetStream event bus
├── aero-storage/     PostgreSQL + Redis repositories
├── aero-auth/        JWT/OIDC authentication
├── aero-signaling/   WebRTC signaling types
├── aero-im-core/     IM business logic (messages, rooms)
├── aero-im-call/     Call orchestration
├── aero-ai/          AI gateway (Anthropic, Voyage, embeddings)
├── aero-push/        FCM/APNs push gateway
├── aero-live-core/   Live streaming abstractions
├── aero-live-rtmp/   RTMP ingest
├── aero-live-hls/    HLS segmenter
├── aero-live-whip/   WHIP/WHEP (WebRTC ingest/egress)
├── aero-live-webrtc/ SFU (selective forwarding)
├── aero-live-srt/    SRT ingest
└── aero-server/      Axum HTTP/WS gateway + full CLI (100 MB)
```

## Engineering Checks

Run all checks before submitting a PR:

```bash
# Full CI pipeline
make ci

# Or individual checks:
make check        # cargo check + test + clippy
make gate         # filesize + truth + web + deps gates
make doctor       # environment diagnostics
```

The `aero-eng` binary provides these checks as commands:

```bash
cargo run -p aero-cli -- check          # parallel cargo check + test + clippy
cargo run -p aero-cli -- gate all       # all engineering gates
cargo run -p aero-cli -- doctor         # environment diagnostics
```

## Testing

```bash
# Core tests (no database required)
cargo test --workspace --lib

# Test the engineering CLI framework
cargo test -p aero-eng

# Integration tests (require PostgreSQL)
bash scripts/test-integration.sh
```

## Code Style

- Follow clippy: `cargo clippy --workspace --all-targets`
- Format: `cargo fmt --all`
- No `unsafe` code (enforced by workspace lints)
- Prefer `?` over `.unwrap()` / `.expect()` in production code
- New features need tests

## Architecture Rules

Crate dependencies follow a strict bottom-up hierarchy:

```
aero-common (leaf, no internal deps)
  ├── aero-bus, aero-storage, aero-signaling, aero-live-core
  │     ├── aero-auth, aero-im-core, aero-im-call, aero-ai
  │     ├── aero-live-rtmp, aero-live-hls
  │     │     └── aero-live-whip, aero-live-webrtc, aero-live-srt
  │     └── aero-eng (standalone)
  └── aero-push
        └── aero-server (root, depends on everything)
```

Verify with: `cargo run -p aero-cli -- gate deps-native`

## Engineering CLI

Two binaries are available:

| Binary | Size | Commands | Build |
|---|---|---|---|
| `aero-eng` | 14 MB | 8 (engineering only) | `cargo build -p aero-cli` |
| `aero-cli` | 100 MB | 15 (full) | `cargo build -p aero-server --bin aero-cli` |

## Documentation

- `docs/engineering-cli.md` — Engineering CLI reference
- `crates/aero-eng/README.md` — CLI framework API
- `skills/` — Development procedure guides
