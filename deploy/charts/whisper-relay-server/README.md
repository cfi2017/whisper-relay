# whisper-relay-server Helm chart

This chart deploys the Whisper Relay server, its ClusterIP service, optional generated secrets, and an optional Gateway API `HTTPRoute`.

## Install

```sh
helm upgrade --install whisper-relay-server deploy/charts/whisper-relay-server \
  --namespace whisper-relay \
  --create-namespace \
  --set existingSecrets.oidc.name=whisper-relay-oidc \
  --set config.transcriptionBaseUrl=http://litellm.litellm.svc.cluster.local:4000
```

## Diarization backend

To route speaker-label requests to a separate OpenAI-compatible backend while keeping plain ASR on the default transcription backend:

```sh
helm upgrade --install whisper-relay-server deploy/charts/whisper-relay-server \
  --namespace whisper-relay \
  --set config.backendDiarization=true \
  --set config.transcriptionBaseUrl=http://litellm.litellm.svc.cluster.local:4000 \
  --set config.transcriptionModel=whisper \
  --set config.diarizationBaseUrl=http://litellm.litellm.svc.cluster.local:4000 \
  --set config.diarizationModel=whisper-diarized
```

The diarized backend must return JSON with `segments[].speaker`.

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
