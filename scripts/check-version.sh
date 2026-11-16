#!/usr/bin/env bash
# Checks that every place naming the release version agrees, and with a tag
# (v1.2.3) if one is given: scripts/check-version.sh [tag]
set -euo pipefail
cd "$(dirname "$0")/.."

cargo=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
tauri=$(python3 -c 'import json; print(json.load(open("crates/sk1ll-gui/src-tauri/tauri.conf.json"))["version"])')
ui=$(python3 -c 'import json; print(json.load(open("crates/sk1ll-gui/ui/package.json"))["version"])')
compose=$(sed -n 's/.*SK1LL_SERVER_VERSION:-\([^}]*\)}.*/\1/p' deploy/docker-compose.server.yml)

status=0
for pair in "tauri.conf.json:$tauri" "ui/package.json:$ui" "docker-compose.server.yml:$compose"; do
  if [[ "${pair#*:}" != "$cargo" ]]; then
    echo "version mismatch: Cargo.toml has $cargo, ${pair%%:*} has ${pair#*:}" >&2
    status=1
  fi
done
if [[ $# -gt 0 && "$1" != "v$cargo" ]]; then
  echo "tag $1 doesn't match version $cargo" >&2
  status=1
fi
if ! grep -q "^## \[$cargo\]" CHANGELOG.md; then
  echo "CHANGELOG.md has no section for $cargo" >&2
  status=1
fi
[[ $status -eq 0 ]] && echo "version $cargo"
exit $status
