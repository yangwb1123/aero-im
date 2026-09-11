# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e

# Build both the gateway and the migration CLI from the same locked workspace
# snapshot.  Keeping the migration binary in the image lets Compose run the
# schema step as a one-shot service before the gateway starts.
ARG RUST_BUILDER_IMAGE=rust:1.80-bookworm@sha256:d22d8938f0403ee31c118b5bf2162b883313dd7f387f859d9f2accd7c884c385
ARG RUNTIME_BASE_IMAGE=debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241
FROM ${RUST_BUILDER_IMAGE} AS builder

# `rust-toolchain.toml` intentionally tracks stable for local development.
# Override that floating channel inside the release image so the advertised
# MSRV builder cannot silently download a newer compiler.
ENV RUSTUP_TOOLCHAIN=1.80.0 \
    CARGO_INCREMENTAL=0

WORKDIR /workspace
RUN rustc --version | grep -E '^rustc 1\.80\.0 '

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY migrations ./migrations
COPY vendor ./vendor
COPY web ./web
COPY config.example.toml ./config.example.toml

RUN --mount=type=cache,id=aero-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=aero-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=aero-cargo-target,target=/workspace/target,sharing=locked \
    cargo build --locked --release \
        --bin aero-server \
        --bin aero-cli \
        --bin aero-audit-relay-probe \
    && mkdir -p /out \
    && install -m 0755 /workspace/target/release/aero-server /out/aero-server \
    && install -m 0755 /workspace/target/release/aero-cli /out/aero-cli \
    && install -m 0755 /workspace/target/release/aero-audit-relay-probe /out/aero-audit-relay-probe

FROM ${RUNTIME_BASE_IMAGE} AS runtime

RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates curl util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /app/web /var/lib/aero/blobs /var/lib/aero/hls \
    && useradd --system --uid 10001 --home-dir /app --no-create-home aero

WORKDIR /app
COPY --from=builder /out/aero-server /usr/local/bin/aero-server
COPY --from=builder /out/aero-cli /usr/local/bin/aero-cli
COPY --from=builder /out/aero-audit-relay-probe /usr/local/bin/aero-audit-relay-probe
COPY --from=builder /workspace/web /app/web
COPY docker/entrypoint.sh /usr/local/bin/aero-entrypoint
# The example file supplies the required config shape; Compose environment
# values override its localhost defaults. JWT PEM values must be supplied by
# the deployment (the placeholders intentionally fail server startup).
COPY --from=builder /workspace/config.example.toml /app/config.toml

ENV AERO__SERVER__HOST=0.0.0.0 \
    AERO__SERVER__PORT=8080 \
    AERO__SERVER__WEB_DIR=/app/web/dist \
    AERO__SERVER__BLOB_DIR=/var/lib/aero/blobs \
    AERO__SERVER__HLS_DIR=/var/lib/aero/hls \
    AERO_RELAY_PROBE_BIN=/usr/local/bin/aero-audit-relay-probe \
    RUST_LOG=info,aero=debug

VOLUME ["/var/lib/aero/blobs", "/var/lib/aero/hls"]
EXPOSE 8080/tcp 1935/tcp 1936/udp

RUN chmod 0755 /usr/local/bin/aero-entrypoint
ENTRYPOINT ["/usr/local/bin/aero-entrypoint"]
CMD ["/usr/local/bin/aero-server"]

# Keep the migration job as an explicit target so deployment tooling can build
# the one-shot image without accidentally changing the gateway's default CMD.
FROM runtime AS migrate

CMD ["/usr/local/bin/aero-cli", "migrate"]
