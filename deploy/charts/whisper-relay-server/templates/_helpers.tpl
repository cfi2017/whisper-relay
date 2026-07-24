{{- define "whisper-relay-server.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "whisper-relay-server.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "whisper-relay-server.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" -}}
{{- end -}}

{{- define "whisper-relay-server.labels" -}}
helm.sh/chart: {{ include "whisper-relay-server.chart" . }}
{{ include "whisper-relay-server.selectorLabels" . }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "whisper-relay-server.selectorLabels" -}}
app.kubernetes.io/name: {{ include "whisper-relay-server.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "whisper-relay-server.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "whisper-relay-server.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "whisper-relay-server.oidcSecretName" -}}
{{- if .Values.existingSecrets.oidc.name -}}
{{- .Values.existingSecrets.oidc.name -}}
{{- else -}}
{{- printf "%s-oidc" (include "whisper-relay-server.fullname" .) -}}
{{- end -}}
{{- end -}}

{{- define "whisper-relay-server.transcriptionSecretName" -}}
{{- if .Values.config.transcriptionApiKeySecret.name -}}
{{- .Values.config.transcriptionApiKeySecret.name -}}
{{- else -}}
{{- printf "%s-transcription" (include "whisper-relay-server.fullname" .) -}}
{{- end -}}
{{- end -}}

