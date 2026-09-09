# Build with: podman build -t loom:local .
FROM docker.io/oven/bun:1.3.13 AS bun
FROM docker.io/library/rust:1.97.0-bookworm AS build
COPY --from=bun /usr/local/bin/bun /usr/local/bin/bun
WORKDIR /opt/loom
ENV CARGO_BUILD_JOBS=4
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY examples ./examples
COPY loom-wit ./loom-wit
COPY loom-rustc/vendor-config.toml ./loom-rustc/vendor-config.toml
RUN --mount=type=cache,id=loom-host-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=loom-host-target,target=/opt/loom/target \
    cargo build --locked --release -p loomd -p loom-cli && \
    mkdir -p /out && cp target/release/loomd target/release/loom /out/
COPY loom-ui ./loom-ui
RUN cd loom-ui && bun install --frozen-lockfile && bun run build

# Compiler sidecars are intentionally included: definitions build on this machine.
FROM docker.io/library/rust:1.97.0-bookworm
COPY --from=bun /usr/local/bin/bun /usr/local/bin/bun
RUN apt-get update && apt-get install -y --no-install-recommends binaryen bubblewrap ca-certificates pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-wasip1 wasm32-wasip2 && cargo install --locked cargo-component --version 0.21.1 --jobs 4
WORKDIR /opt/loom
COPY --from=build /out/loomd /usr/local/bin/loomd
COPY --from=build /out/loom /usr/local/bin/loom
COPY --from=build /opt/loom/loom-ui/build ./loom-ui/build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY examples ./examples
COPY loom-wit ./loom-wit
COPY loom-checker ./loom-checker
COPY loom-guest-ts ./loom-guest-ts
COPY loom-rustc ./loom-rustc
RUN cd loom-checker && bun install --frozen-lockfile
RUN cd loom-guest-ts && bun install --frozen-lockfile
RUN useradd --create-home --uid 10001 loom && mkdir -p /data /opt/loom/.loom-build && chown -R loom:loom /data /opt/loom/.loom-build
ENV LOOM_ROOT=/opt/loom LOOM_BUILD_DIR=/data/builds CARGO_HOME=/data/cargo CARGO_BUILD_JOBS=4
USER loom
EXPOSE 8787
VOLUME ["/data"]
ENTRYPOINT ["loomd"]
CMD ["--db", "/data/loom.sqlite", "--bind", "0.0.0.0:8787"]
