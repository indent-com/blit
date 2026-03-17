FROM rust:alpine AS build
RUN apk add --no-cache musl-dev curl wasm-pack binaryen
RUN rustup target add wasm32-unknown-unknown
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY server server
COPY browser browser
COPY web web
RUN cd browser && wasm-pack build --target web --release --out-dir ../web
RUN cargo build --release -p blit-server

FROM alpine:latest
RUN apk add --no-cache \
    coreutils procps curl mpv fish bash \
    grep sed gawk findutils less \
    htop vim git jq wget tree file
COPY --from=build /src/target/release/blit-server /usr/local/bin/
ENV SHELL=/usr/bin/fish
EXPOSE 3264
ENTRYPOINT ["blit-server"]
