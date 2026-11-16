#!/usr/bin/env bash
# Packs release binaries with the docs and the systemd unit into
# dist/sk1ll-<version>-<arch>-linux.tar.gz:
#   scripts/package-release.sh <arch> <binary>...
set -euo pipefail
cd "$(dirname "$0")/.."

arch=$1
shift
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
name="sk1ll-$version-$arch-linux"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/$name" dist
for binary in "$@"; do
  install -m 0755 "$binary" "$stage/$name/"
done
cp LICENSE README.md CHANGELOG.md deploy/sk1ll-daemon.service "$stage/$name/"
tar -C "$stage" -czf "dist/$name.tar.gz" "$name"
echo "dist/$name.tar.gz"
