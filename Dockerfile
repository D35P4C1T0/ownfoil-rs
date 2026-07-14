FROM rust:1.89-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY ownfoil-rs ./ownfoil-rs
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates curl gosu \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/ownfoil-rs /usr/local/bin/ownfoil-rs
COPY docker/entrypoint.sh /usr/local/bin/ownfoil-entrypoint
RUN chmod 0755 /usr/local/bin/ownfoil-entrypoint \
    && mkdir -p /app/config /app/data /games
ENV OWNFOIL_SETTINGS=/app/config/settings.yaml
EXPOSE 8465
VOLUME ["/app/config", "/app/data", "/games"]
STOPSIGNAL SIGINT
ENTRYPOINT ["ownfoil-entrypoint"]
CMD ["ownfoil-rs"]
