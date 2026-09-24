# lean-lane node image: lean-lane-node (ENTRYPOINT) + spammer (/usr/local/bin/spammer).
#
#   ./scripts/build-image.sh                 # tags lean-lane:<git-sha> and lean-lane:local
#   docker build -t lean-lane:local .        # same, by hand
#
# The binaries link only libc/libm/libgcc_s (no OpenSSL, no C deps at runtime),
# so the runtime stage is distroless/cc. A static busybox is shipped solely for
# the HEALTHCHECK (`lean-healthcheck`: arc_getHead over JSON-RPC).

# Keep in step with rust-toolchain.toml (build-image.sh passes it from there).
ARG RUST_VERSION=1.93.0

###########
# Builder
###########
FROM rust:${RUST_VERSION}-bookworm AS builder
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates/ crates/
# Cache mounts: registry, git checkouts (reth is a git dependency) and target
# survive between builds; the binaries are copied out of the cached target.
RUN --mount=type=cache,id=lean-lane-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=lean-lane-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=lean-lane-target,target=/src/target \
    cargo build --release --locked -p lean-lane-node -p spammer \
 && mkdir -p /out /data \
 && cp target/release/lean-lane-node target/release/spammer /out/

# Static (musl) busybox: wget + grep + sh for the healthcheck, nothing else.
FROM busybox:1.36-musl AS busybox

###########
# Runtime
###########
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=busybox /bin/busybox /usr/local/bin/busybox
COPY --chmod=0755 docker/lean-healthcheck /usr/local/bin/lean-healthcheck
COPY --from=builder /out/lean-lane-node /out/spammer /usr/local/bin/
# /data owned by the nonroot user (65532) so a fresh named volume is writable.
COPY --from=builder --chown=65532:65532 /data /data

USER nonroot
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8560
# The node shuts down cleanly on SIGINT (ctrl_c); make `docker stop` send it.
STOPSIGNAL SIGINT
HEALTHCHECK --interval=5s --timeout=4s --start-period=60s --retries=30 \
  CMD ["/usr/local/bin/lean-healthcheck"]

ENTRYPOINT ["lean-lane-node"]
CMD ["run", "--datadir", "/data", "--bind", "0.0.0.0", "--port", "8560"]
