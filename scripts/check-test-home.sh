#!/bin/sh
# Runs the Rust test suite with HOME pointed at an empty directory and the
# XDG variables unset, then fails if any test wrote there: every test must
# keep to the temporary directories it is given, never the real home.
# Usage: scripts/check-test-home.sh [cargo test arguments...]
set -eu
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
sentinel=$(mktemp -d "${TMPDIR:-/tmp}/sk1ll-home.XXXXXX")
trap 'rm -rf "$sentinel"' EXIT
unset XDG_CONFIG_HOME XDG_CACHE_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_RUNTIME_DIR \
  SK1LL_REPO SK1LL_STATE_DIR SK1LL_CACHE_DIR SK1LL_DAEMON_SOCKET SK1LL_DAEMON_STATE \
  SK1LL_MACHINE_CONFIG SK1LL_ASSIGNMENT
HOME="$sentinel" cargo test --workspace "$@"
written=$(find "$sentinel" -mindepth 1 | head -n 20)
if [ -n "$written" ]; then
  echo "tests wrote under HOME ($sentinel):" >&2
  echo "$written" >&2
  exit 1
fi
echo "no test wrote under HOME"
