# syntax=docker/dockerfile:1.7
FROM rust:1.95-bookworm AS build
ARG TARGETARCH
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git-${TARGETARCH},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=cargo-target-${TARGETARCH},target=/src/target,sharing=locked \
    cargo build --release -p whisper-relay-server \
    && cp target/release/whisper-relay-server /tmp/whisper-relay-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /tmp/whisper-relay-server /usr/local/bin/whisper-relay-server
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/whisper-relay-server"]
