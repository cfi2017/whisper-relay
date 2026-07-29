# Whisper Relay

Whisper Relay is a Rust project for full-meeting and live transcription with local audio capture and remote GPU-backed Whisper inference.

## Components

- `whisper-relay-client`: Linux/PipeWire terminal client. It opens a TUI source picker, records selected streams into one meeting, sends the finalized WAV over WebSocket, and writes the transcript to Markdown.
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

For full-meeting transcription:

```sh
cargo run -p whisper-relay-client -- --list-sources
cargo run -p whisper-relay-client -- --output transcript.md
cargo run -p whisper-relay-client -- --source <pipewire-node-id> --output transcript.md
```

Select the application playback stream for the people you hear and your microphone for your own voice, then press `r` to start recording. Press `r` again to stop, save the meeting WAV, upload it once, and transcribe the complete recording. `q`, Esc, and Ctrl-C also stop and transcribe an active recording cleanly.

The TUI stays open while recording and while the server transcribes. Use Space to enable or disable the highlighted stream and `a` to toggle auto-enabling newly discovered streams. When streams appear, disappear, or are toggled, the recorder creates a new local segment; it merges all segments into one WAV before upload, so the backend never sees transcription chunks. By default the recording is saved next to the Markdown output as `transcript-YYYYMMDD-HHMMSS.wav`.

The previous low-latency behavior remains available with `--capture-mode live` or `capture_mode = "live"`. It sends finalized WAV chunks every `chunk_seconds`.

## Client Config

The client reads TOML config from `--config`, `WHISPER_RELAY_CONFIG`, or `~/.config/whisper-relay/client.toml`.
CLI flags and environment variables override config-file values.
Use `--list-sources` to print current PipeWire node IDs and identity keys.

```toml
server_url = "wss://whisper.example.com/v1/sessions/ws"
output = "~/Documents/meetings/transcript.md"
capture_mode = "meeting"
# Structured transcript events with absolute timestamps.
events_output = "~/Documents/meetings/transcript.events.jsonl"
# Optional fixed path. When omitted, a timestamped WAV is written beside output.
# recording_output = "~/Documents/meetings/meeting.wav"
oidc_issuer = "https://issuer.example.com"
oidc_client_id = "whisper-relay-device-client"
token_cache = "~/.cache/whisper-relay/oidc-token.json"
diarization = "prefer"
# Optional ISO-639-1 language for this recording, for example de, en, fr, or it.
language = "de"
chunk_seconds = 15
auto_enable_new_streams = false
audio_rescan_seconds = 2
source = ["42", "84"]
```

Supported `diarization` values are `prefer`, `require`, and `disable`.
Set `language` per invocation with `--language de` or `WHISPER_RELAY_LANGUAGE=de`. The session value overrides the server-wide default and is forwarded through LiteLLM to the transcription backend. Omit it to let the model detect the language.
Configured `source` entries may be current PipeWire node IDs, node names, descriptions, or identity keys printed by `--list-sources`. When a selected stream disappears, the client keeps the capture session alive and reconnects matching streams when they reappear. Set `auto_enable_new_streams = true` to adopt newly discovered streams while capture is running.
OIDC device-login tokens are cached by default at `$XDG_CACHE_HOME/whisper-relay/oidc-token.json` or `~/.cache/whisper-relay/oidc-token.json`. Use `token_cache` or `WHISPER_RELAY_TOKEN_CACHE` to choose a different path, or set `disable_token_cache = true` / `WHISPER_RELAY_DISABLE_TOKEN_CACHE=true` to force a fresh login.

## Home Manager

The flake exposes `homeManagerModules.default` and `homeManagerModules.whisper-relay-client`.

```nix
{
  inputs.whisper-relay.url = "github:cfi2017/whisper-relay";

  outputs = { whisper-relay, ... }: {
    homeConfigurations.me = home-manager.lib.homeManagerConfiguration {
      modules = [
        whisper-relay.homeManagerModules.default
        {
          programs.whisper-relay = {
            enable = true;
            settings = {
              server_url = "wss://whisper.example.com/v1/sessions/ws";
              output = "~/Documents/meetings/transcript.md";
              capture_mode = "meeting";
              oidc_issuer = "https://issuer.example.com";
              oidc_client_id = "whisper-relay-device-client";
              token_cache = "~/.cache/whisper-relay/oidc-token.json";
              diarization = "prefer";
              language = "de";
              auto_enable_new_streams = true;
              audio_rescan_seconds = 2;
            };
          };
        }
      ];
    };
  };
}
```

## Server Helm Chart

The server chart lives at `deploy/charts/whisper-relay-server` and supports optional Gateway API `HTTPRoute` creation.

```sh
helm lint deploy/charts/whisper-relay-server
helm template whisper-relay-server deploy/charts/whisper-relay-server \
  --set gateway.enabled=true \
  --set 'gateway.hostnames[0]=whisper.example.com'
```

## Releases

Commits and PR titles should use Conventional Commits, for example `feat(client): add stream picker` or `fix(server): validate jwt audience`.

Pushes to `main` run semantic-release. When a release is cut, the release workflow publishes:

- `ghcr.io/cfi2017/whisper-relay-server:<version>`
- `ghcr.io/cfi2017/whisper-relay-server:latest`
- `oci://ghcr.io/cfi2017/charts/whisper-relay-server:<version>`

Server image tags are multi-architecture manifests for `linux/amd64` and `linux/arm64`.

Semantic-release updates the shared Cargo workspace version, `Cargo.lock`, chart `version`/`appVersion`, and `CHANGELOG.md` before creating the release tag. Nix client and server packages read that workspace version. Released charts leave `image.tag` empty, so Helm deploys the immutable server image matching the chart's `appVersion`; users can still override `image.tag` explicitly. The chart is published only after the corresponding GHCR image succeeds.

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

## Diarization

Speaker labels require both sides to opt in. Set the client config to `diarization = "prefer"` or `diarization = "require"` and enable the upstream MOSS vLLM backend with:

```yaml
config:
  backendDiarization: true
  diarizationBaseUrl: "http://moss-diarized-asr.vllm.svc.cluster.local:8000"
  diarizationModel: "moss-diarized"
  diarizationResponseFormat: "verbose_json"
```

or set `WHISPER_RELAY_BACKEND_DIARIZATION=true`, `WHISPER_RELAY_DIARIZATION_BASE_URL`, `WHISPER_RELAY_DIARIZATION_MODEL`, and `WHISPER_RELAY_DIARIZATION_RESPONSE_FORMAT=verbose_json` on the server. The relay then sends diarized requests directly to MOSS and plain requests to `transcriptionBaseUrl` through LiteLLM. Do not add MOSS to LiteLLM: LiteLLM's hosted-vLLM transcription adapter rejects the `verbose_json` response format required for speaker segments.

The diarization client inherits `WHISPER_RELAY_TRANSCRIPTION_API_KEY` when `WHISPER_RELAY_DIARIZATION_API_KEY` is unset. Set the latter only when the diarization backend uses different credentials.

The diarized backend must expose `/v1/audio/transcriptions` and return JSON containing segment `speaker` fields. Reference wiring lives in:

- `deploy/reference/vllm-qwen3-asr.yaml`: vLLM Qwen3-ASR deployment.
- `deploy/reference/vllm-moss-diarized.yaml`: upstream vLLM deployment for multilingual MOSS transcription and diarization.
- `deploy/reference/litellm-config.yaml`: LiteLLM model entries for plain general and Swiss German transcription.

## Current V1 Boundaries

- The transport is WebSocket for both control and audio.
- Audio is persisted only by the client; the server holds the uploaded meeting in memory while forwarding it to the transcription backend.
- Diarization is requested from the transcription backend when enabled. If the backend does not return diarized segments, the client writes transcript lines with `Unknown`.
- Full-meeting mode records 16 kHz mono PCM and uploads one logical WAV after recording stops. Protocol v2 transports it in 64 KiB WebSocket frames, which the server buffers until `AudioEnd`; `config.maxAudioMiB` limits the aggregate recording size. This is roughly 115 MiB per hour.
- The server applies adaptive energy VAD to plain WAV meetings, packs nearby speech into approximately 25-second chunks, uses overlap only for forced cuts, transcribes with bounded concurrency, and restores timestamps to the original meeting timeline. Diarized requests remain whole-file until cross-chunk speaker clustering is available.
- The client appends every structured transcript event to `events_output` (default: the Markdown output path with `.events.jsonl`) for auditing and downstream processing.
- PipeWire capture is implemented through `pw-dump` and `gst-launch-1.0`. Live mode remains available for lower latency, but full-meeting mode is the default while capture and model quality are being validated.
