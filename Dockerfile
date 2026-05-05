FROM rust:1.88-slim AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY mock_psp/Cargo.toml mock_psp/Cargo.toml
RUN mkdir -p src mock_psp/src \
    && echo 'fn main(){}' > src/main.rs \
    && echo 'fn main(){}' > mock_psp/src/main.rs \
    && cargo build --release -p dodo-payments 2>/dev/null || true \
    && rm -rf src mock_psp/src
COPY src/ src/
COPY mock_psp/src/ mock_psp/src/
COPY migrations/ migrations/
COPY .sqlx/ .sqlx/
ENV SQLX_OFFLINE=true
RUN touch src/main.rs && cargo build --release -p dodo-payments
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/dodo-payments /usr/local/bin/dodo-payments
COPY --from=builder /app/migrations /migrations
CMD ["dodo-payments"]
