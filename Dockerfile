FROM rust:1.95-slim-bookworm AS builder

WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY client/Cargo.toml client/
COPY server/Cargo.toml server/
COPY crates/syncengine-protocol/Cargo.toml crates/syncengine-protocol/
COPY crates/syncengine-storage/Cargo.toml crates/syncengine-storage/
COPY crates/syncengine-engine/Cargo.toml crates/syncengine-engine/
COPY crates/syncengine-transport/Cargo.toml crates/syncengine-transport/

RUN mkdir -p client/src server/src \
    crates/syncengine-protocol/src crates/syncengine-storage/src \
    crates/syncengine-engine/src crates/syncengine-transport/src \
 && echo 'fn main() {}' > client/src/main.rs \
 && echo '' > client/src/lib.rs \
 && echo '' > crates/syncengine-protocol/src/lib.rs \
 && echo '' > crates/syncengine-storage/src/lib.rs \
 && echo '' > crates/syncengine-engine/src/lib.rs \
 && echo '' > crates/syncengine-transport/src/lib.rs \
 && echo 'fn main() {}' > server/src/main.rs \
 && echo '' > server/src/lib.rs \
 && cargo build --release -p syncengine-server

COPY . .

RUN find client server crates -type f -exec touch {} + \
 && cargo build --release -p syncengine-server

FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 syncengine

COPY --from=builder /src/target/release/syncengine-server /usr/local/bin/syncengine-server

USER syncengine
EXPOSE 8443
ENV RUST_LOG=info

HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=5 \
  CMD curl -fsS http://127.0.0.1:8443/healthz || exit 1

ENTRYPOINT ["syncengine-server"]
CMD ["serve", "--bind", "0.0.0.0:8443"]
