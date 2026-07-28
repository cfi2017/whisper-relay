#!/usr/bin/env bash
set -euo pipefail

cargo_version="$(awk '
  /^\[workspace.package\]$/ { in_package = 1; next }
  /^\[/ { in_package = 0 }
  in_package && /^version = / { gsub(/[" ]/, "", $3); print $3; exit }
' Cargo.toml)"
chart_version="$(awk '/^version:/ { print $2; exit }' deploy/charts/whisper-relay-server/Chart.yaml)"
app_version="$(awk '/^appVersion:/ { gsub(/"/, "", $2); print $2; exit }' deploy/charts/whisper-relay-server/Chart.yaml)"
image_tag="$(awk '
  /^image:$/ { in_image = 1; next }
  /^[^ ]/ { in_image = 0 }
  in_image && /^  tag:/ { gsub(/"/, "", $2); print $2; exit }
' deploy/charts/whisper-relay-server/values.yaml)"

if [[ -z "$cargo_version" || "$cargo_version" != "$chart_version" || "$cargo_version" != "$app_version" ]]; then
  echo "Cargo ($cargo_version), chart ($chart_version), and appVersion ($app_version) must match" >&2
  exit 1
fi

if [[ -n "$image_tag" ]]; then
  echo "The default image tag must be empty so the chart uses appVersion; got $image_tag" >&2
  exit 1
fi

echo "release versions are aligned at $cargo_version"
