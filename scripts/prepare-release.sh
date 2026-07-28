#!/usr/bin/env bash
set -euo pipefail

version="${1:?release version is required}"

sed -i -E '0,/^version = "[^"]+"/s//version = "'"$version"'"/' Cargo.toml
sed -i -E 's/^version: .*/version: '"$version"'/' deploy/charts/whisper-relay-server/Chart.yaml
sed -i -E 's/^appVersion: .*/appVersion: "'"$version"'"/' deploy/charts/whisper-relay-server/Chart.yaml

cargo metadata --no-deps --format-version 1 >/dev/null
./scripts/check-versions.sh
