# Build context is the repo root (see upsidedown/fly.toml `build.context`),
# so the whole Cargo workspace is available. Only blit-upsidedown and its
# external dependencies compile — no other workspace crate is a dependency.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p blit-upsidedown

FROM debian:bookworm-slim
# ca-certificates: ACME (Let's Encrypt) and outbound TLS need the root store.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/upsidedown /usr/local/bin/upsidedown
ENTRYPOINT ["upsidedown"]
