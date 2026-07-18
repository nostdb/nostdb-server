# syntax=docker/dockerfile:1
FROM rust:1.85-bookworm AS build
WORKDIR /source
COPY nostos-core ./nostos-core
COPY nostos-server ./nostos-server
COPY nostos-cli ./nostos-cli
RUN cargo build --locked --release --manifest-path nostos-server/Cargo.toml --bin nostosd \
    && cargo build --locked --release --manifest-path nostos-cli/Cargo.toml --bin nostos

FROM debian:bookworm-slim
RUN groupadd --system --gid 1700 nostosdb \
    && useradd --system --uid 1700 --gid nostosdb --home-dir /var/lib/nostosdb --shell /usr/sbin/nologin nostosdb \
    && install -d -o nostosdb -g nostosdb -m 0700 /var/lib/nostosdb \
    && install -d -o nostosdb -g nostosdb -m 0700 /etc/nostosdb
COPY --from=build /source/nostos-server/target/release/nostosd /usr/local/bin/nostosd
COPY --from=build /source/nostos-cli/target/release/nostos /usr/local/bin/nostos
USER nostosdb:nostosdb
VOLUME ["/etc/nostosdb", "/var/lib/nostosdb"]
EXPOSE 7878
ENTRYPOINT ["nostosd"]
CMD ["serve", "--config", "/etc/nostosdb/server.toml"]
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
  CMD ["nostos", "server", "ping", "--server", "nostos://127.0.0.1:7878", "--credential-file", "/var/lib/nostosdb/credentials/client.token"]
