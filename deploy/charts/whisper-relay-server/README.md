# whisper-relay-server Helm chart

This chart deploys the Whisper Relay server, its ClusterIP service, optional generated secrets, and an optional Gateway API `HTTPRoute`.

Set `config.logFilter=whisper_relay_server=debug,tower_http=debug` when troubleshooting WebSocket uploads. The default is server and HTTP `info` logging.

## Install

```sh
helm upgrade --install whisper-relay-server deploy/charts/whisper-relay-server \
  --namespace whisper-relay \
  --create-namespace \
  --set existingSecrets.oidc.name=whisper-relay-oidc \
  --set config.transcriptionBaseUrl=http://litellm.litellm.svc.cluster.local:4000
```

For a LiteLLM API key stored in an existing Secret, configure its exact name and key:

```yaml
config:
  transcriptionApiKeySecret:
    name: litellm-api-key
    key: api-key
    optional: false
```

Secret-backed environment variables are read when the pod starts. Restart the deployment after changing the Secret. At startup, the server logs `transcription_auth=true` when the key is available, without logging its value.

## Diarization backend

To route speaker-label requests to a separate OpenAI-compatible backend while keeping plain ASR on the default transcription backend:

```sh
helm upgrade --install whisper-relay-server deploy/charts/whisper-relay-server \
  --namespace whisper-relay \
  --set config.backendDiarization=true \
  --set config.transcriptionBaseUrl=http://litellm.litellm.svc.cluster.local:4000 \
  --set config.transcriptionModel=whisper \
  --set config.diarizationBaseUrl=http://litellm.litellm.svc.cluster.local:4000 \
  --set config.diarizationModel=whisper-diarized \
  --set config.diarizationResponseFormat=verbose_json
```

The reference MOSS backend returns `verbose_json` with `segments[].speaker` and uses the upstream vLLM image directly.
When both model routes use LiteLLM, the diarization client inherits the transcription API key. Set `config.diarizationApiKey` only for a backend with separate credentials.

## Gateway API

Enable an `HTTPRoute` when your cluster already has a `Gateway`:

```sh
helm upgrade --install whisper-relay-server deploy/charts/whisper-relay-server \
  --namespace whisper-relay \
  --set gateway.enabled=true \
  --set gateway.parentRefs[0].name=public \
  --set gateway.parentRefs[0].namespace=ingress \
  --set gateway.parentRefs[0].sectionName=https \
  --set gateway.hostnames[0]=whisper.example.com
```

The route forwards all paths by default, including `/healthz` and `/v1/sessions/ws`.

Full-meeting uploads default to 512 MiB WebSocket message and frame limits and a one-hour backend timeout. Adjust `config.maxAudioMiB` and `config.transcriptionTimeoutSeconds` for longer recordings, and apply matching limits to the Gateway implementation in front of the chart.

Plain WAV meetings use silence-aware server-side chunking by default. `config.targetChunkSeconds` controls normal packing, `config.maxChunkSeconds` controls forced cuts with one second of overlap, and `config.asrConcurrency` limits parallel backend requests. Smart chunking is intentionally bypassed for diarized requests so speaker identities are not reset independently in every chunk.

Set `config.language` to a default language code such as `de` to prevent per-chunk language drift. A client session can override it with `--language`; omit both values for model language detection. `config.prompt` can contain participant names, product names, and technical vocabulary when the selected transcription backend supports the OpenAI `prompt` field.
