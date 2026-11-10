# Baut can-server und die CLI als kleines Laufzeit-Image.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p can-server -p can-cli

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 can
COPY --from=build /src/target/release/can-server /src/target/release/can /usr/local/bin/
USER can
WORKDIR /home/can
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD ["/usr/local/bin/can-server", "--health", "http://127.0.0.1:8080/healthz"]
ENTRYPOINT ["/usr/local/bin/can-server"]
CMD ["/etc/can/can-server.toml"]
