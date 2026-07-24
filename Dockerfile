FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p whisper-relay-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/whisper-relay-server /usr/local/bin/whisper-relay-server
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/whisper-relay-server"]

