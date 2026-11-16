FROM rust:1-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /workspace
COPY --from=builder /build/target/release/cs /usr/local/bin/cs
ENTRYPOINT ["cs"]
