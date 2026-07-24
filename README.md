# Whisper Relay

Whisper Relay is a greenfield Rust project for live meeting transcription with local audio capture and remote GPU-backed Whisper inference.

## Components

- `whisper-relay-client`: Linux/PipeWire terminal client. It opens a TUI source picker by default, starts a GStreamer capture pipeline, sends short Ogg/Opus chunks over WebSocket, and appends transcripts to Markdown.
- `whisper-relay-server`: Kubernetes-ready Rust server. It validates OIDC bearer tokens, accepts WebSocket audio sessions, calls an OpenAI-compatible transcription endpoint, and returns normalized transcript events.
- `whisper-relay-protocol`: Shared JSON protocol types.

## Local Development

```sh
nix develop
cargo test
cargo run -p whisper-relay-server -- \
  --insecure-no-auth \
  --transcription-base-url http://127.0.0.1:8000 \
  --transcription-model whisper
```

In another shell:

```sh
cargo run -p whisper-relay-client -- \
  --insecure-no-auth \
  --server-url ws://127.0.0.1:8080/v1/sessions/ws \
  --audio-file sample.ogg \
  --output transcript.md
```

For live capture:

```sh
cargo run -p whisper-relay-client -- --list-sources
cargo run -p whisper-relay-client -- --output transcript.md
cargo run -p whisper-relay-client -- --source <pipewire-node-id> --output transcript.md
```

## Server Helm Chart

The server chart lives at `deploy/charts/whisper-relay-server` and supports optional Gateway API `HTTPRoute` creation.

```sh
helm lint deploy/charts/whisper-relay-server
helm template whisper-relay-server deploy/charts/whisper-relay-server \
  --set gateway.enabled=true \
  --set gateway.hostnames[0]=whisper.example.com
```

## Authentication

Production mode expects a generic OIDC issuer and audience on the server:

```sh
WHISPER_RELAY_OIDC_ISSUER=https://issuer.example.com
WHISPER_RELAY_OIDC_AUDIENCE=whisper-relay
```

The client can either run device-code login:

```sh
WHISPER_RELAY_OIDC_ISSUER=https://issuer.example.com
WHISPER_RELAY_OIDC_CLIENT_ID=whisper-relay-device-client
```

or use an existing access token with `WHISPER_RELAY_TOKEN`.

## Current V1 Boundaries

- The transport is WebSocket for both control and audio.
- Audio is not persisted by the server.
- Diarization is requested from the transcription backend when enabled. If the backend does not return diarized segments, the client writes transcript lines with `Unknown`.
- PipeWire capture is implemented through `pw-dump` and `gst-launch-1.0`; live capture writes temporary local Ogg/Opus chunks, sends each completed chunk, and deletes it.
