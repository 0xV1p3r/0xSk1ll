#!/usr/bin/env bash
# An isolated real Server and two Daemons. No command runs with the real HOME.
set -euo pipefail
set +m
umask 077
project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_root"
# Keep build-tool caches available when the optional GUI gets an isolated HOME.
toolchain_cargo_home=${CARGO_HOME:-$HOME/.cargo}
toolchain_rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
cargo build -p sk1ll-server -p sk1ll-daemon
build_dir=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
fleet_dir=$(mktemp -d "${TMPDIR:-/tmp}/sk1ll-fleet.XXXXXXXX")
pids=()
sessions=()
session_alive() {
  # Zombies cannot run hooks or touch files; their parent will reap them.
  ps -eo sid=,stat= | awk -v sid="$1" '$1 == sid && $2 !~ /^Z/ { alive = 1 } END { exit !alive }'
}
cleanup() {
  trap - EXIT
  trap '' INT TERM
  # Git creates its own process groups. All of them still belong to the
  # Daemon/Server session, so signal sessions rather than only the launch group.
  local sid attempt alive
  for sid in "${sessions[@]}"; do pkill -TERM -s "$sid" 2>/dev/null || true; done
  for ((attempt = 0; attempt < 50; attempt++)); do
    alive=false
    for sid in "${sessions[@]}"; do
      if session_alive "$sid"; then alive=true; break; fi
    done
    if ! $alive; then break; fi
    sleep 0.1
  done
  for sid in "${sessions[@]}"; do pkill -KILL -s "$sid" 2>/dev/null || true; done
  for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
  rm -rf -- "$fleet_dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
machine_env() {
  local background=false
  if [[ ${1:-} == --background ]]; then background=true; shift; fi
  local machine=$1
  shift
  local machine_home="$fleet_dir/$machine"
  local isolated=(env -i PATH="$PATH" HOME="$machine_home" \
    XDG_CONFIG_HOME="$machine_home/config" XDG_CACHE_HOME="$machine_home/cache" \
    XDG_DATA_HOME="$machine_home/data" XDG_RUNTIME_DIR="$machine_home/run" \
    SK1LL_MACHINE_CONFIG="$machine_home/config/sk1ll/machine.toml" \
    SK1LL_DAEMON_SOCKET="$machine_home/run/sk1ll/daemon.sock" \
    GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 \
    RUST_LOG=info)
  if $background; then
    setsid "${isolated[@]}" "$@" &
    pids+=("$!")
    sessions+=("$!")
  else
    "${isolated[@]}" "$@"
  fi
}
for machine in server alpha beta gui; do
  mkdir -p "$fleet_dir/$machine"/{config/sk1ll,cache,data,run,.claude,.codex}
done
# Use absolute, isolated Harness paths as well as isolated HOME/XDG directories.
for machine in alpha beta; do
  cat > "$fleet_dir/$machine/config/sk1ll/machine.toml" <<CONFIG
id = "$machine"
[harnesses.claude]
skills = "$fleet_dir/$machine/.claude/skills"
instructions = "$fleet_dir/$machine/.claude/rules"
[harnesses.codex]
skills = "$fleet_dir/$machine/.codex/skills"
instructions = "$fleet_dir/$machine/.codex/AGENTS.md"
CONFIG
done
port=${SK1LL_DEV_PORT:-$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')}
server_url="http://127.0.0.1:$port"
admin_token=$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')
machine_env --background server env SK1LL_SERVER_DATA="$fleet_dir/server/data" SK1LL_SERVER_BIND="127.0.0.1:$port" \
  SK1LL_SERVER_ADMIN_TOKEN="$admin_token" "$build_dir/debug/sk1ll-server" > "$fleet_dir/server.log" 2>&1
wait_for() {
  local attempts=0
  until "$@"; do
    attempts=$((attempts + 1))
    if ((attempts >= 150)); then
      printf 'Timed out. Logs in %s\n' "$fleet_dir" >&2
      cat "$fleet_dir"/*.log >&2
      return 1
    fi
    sleep 0.2
  done
}
# Health reads may retry. Each mutation below executes once and fails visibly.
wait_for curl --silent --fail --output /dev/null --header "Authorization: Bearer $admin_token" "$server_url/api/fleet"
create_code() {
  curl --silent --show-error --fail-with-body --request POST --header "Authorization: Bearer $admin_token" \
    "$server_url/api/join-codes" | python3 -c 'import json,sys; print(json.load(sys.stdin)["code"])'
}
for machine in alpha beta; do
  join_code=$(create_code)
  machine_env "$machine" "$build_dir/debug/sk1ll-daemon" join "$server_url" "$join_code" \
    --id "$machine" --repo "$fleet_dir/$machine/skills"
  machine_env --background "$machine" "$build_dir/debug/sk1ll-daemon" > "$fleet_dir/$machine.log" 2>&1
  wait_for curl --silent --fail --output /dev/null --unix-socket "$fleet_dir/$machine/run/sk1ll/daemon.sock" http://localhost/status
done
alpha_socket="$fleet_dir/alpha/run/sk1ll/daemon.sock"
connected() {
  curl --silent --fail --unix-socket "$1" http://localhost/status | \
    python3 -c 'import json,sys; s=json.load(sys.stdin); sys.exit(0 if s["connection"]=="connected" and not s["awaiting_assignment"] else 1)'
}
wait_for connected "$alpha_socket"
mkdir -p "$fleet_dir/alpha/skills/personal/"{git-review,release-notes} "$fleet_dir/alpha/skills/instructions"
cat > "$fleet_dir/alpha/skills/personal/git-review/SKILL.md" <<'SKILL'
---
name: git-review
description: Review a change for correctness and missing tests
---
Read the diff and identify concrete bugs before suggesting style changes.
SKILL
cat > "$fleet_dir/alpha/skills/personal/release-notes/SKILL.md" <<'SKILL'
---
name: release-notes
description: Write concise release notes from a diff
---
Describe changes that matter to people using the app.
SKILL
printf 'Use focused commits with clear messages.\n' > "$fleet_dir/alpha/skills/instructions/git.md"
# Commit through the Daemon; it owns the repo lock and pushes on connection.
curl --silent --show-error --fail-with-body --unix-socket "$alpha_socket" --request POST \
  --header 'Content-Type: application/json' \
  --data '{"items":["skill:git-review","skill:release-notes","rule:git"],"message":"Seed development Fleet"}' \
  http://localhost/commit > "$fleet_dir/commit.json" || { cat "$fleet_dir/commit.json" >&2; exit 1; }
# Exercise real Assignment toggles. New Machines intentionally start all off.
for machine in alpha beta; do
  curl --silent --show-error --fail-with-body --unix-socket "$alpha_socket" --request PUT \
    --header 'Content-Type: application/json' \
    --data '{"changes":[{"harness":"claude","kind":"skill","name":"git-review","enabled":true},{"harness":"codex","kind":"rule","name":"git","enabled":true}]}' \
    "http://localhost/machines/$machine/assignment" > "$fleet_dir/assignment.json" || { cat "$fleet_dir/assignment.json" >&2; exit 1; }
done
printf '\nAdd a second pass for error handling.\n' >> "$fleet_dir/alpha/skills/personal/git-review/SKILL.md"
mkdir -p "$fleet_dir/alpha/skills/personal/local-only"
printf '%s\n' '---' 'name: local-only' 'description: A Skill private to this Machine until Commit' '---' 'Draft work.' \
  > "$fleet_dir/alpha/skills/personal/local-only/SKILL.md"
# Editors opened from the GUI get the real HOME and XDG dirs back so they keep the user's settings.
editor_env=()
for var in HOME XDG_CONFIG_HOME XDG_CACHE_HOME XDG_DATA_HOME XDG_STATE_HOME; do
  editor_env+=("SK1LL_EDITOR_$var=${!var:-}")
done
gui_env() {
  env "${editor_env[@]}" HOME="$fleet_dir/gui" XDG_CONFIG_HOME="$fleet_dir/gui/config" \
    XDG_CACHE_HOME="$fleet_dir/gui/cache" XDG_DATA_HOME="$fleet_dir/gui/data" \
    CARGO_HOME="$toolchain_cargo_home" RUSTUP_HOME="$toolchain_rustup_home" "$@"
}
# Printed and exported for the optional app command, never changes the real HOME.
export SK1LL_DAEMON_SOCKET="$alpha_socket"
printf '\nDevelopment Fleet ready\nServer: %s\nFiles and logs: %s\n\n' "$server_url" "$fleet_dir"
printf 'In another terminal:\n  cd %q\n  SK1LL_DAEMON_SOCKET=%q pnpm tauri dev\n\n' "$project_root/crates/sk1ll-gui/ui" "$alpha_socket"
printf 'Or pass --gui to this script to launch the GUI now. Ctrl-C removes the temporary Fleet.\n'
if [[ ${1:-} == --gui ]]; then
  (cd "$project_root/crates/sk1ll-gui/ui" && exec setsid env "${editor_env[@]}" HOME="$fleet_dir/gui" XDG_CONFIG_HOME="$fleet_dir/gui/config" \
    XDG_CACHE_HOME="$fleet_dir/gui/cache" XDG_DATA_HOME="$fleet_dir/gui/data" \
    CARGO_HOME="$toolchain_cargo_home" RUSTUP_HOME="$toolchain_rustup_home" pnpm tauri dev) &
  pids+=("$!")
  sessions+=("$!")
fi
# Tests may pass an explicit command, e.g. scripts/dev-fleet.sh -- target/debug/sk1ll-gui.
if [[ ${1:-} == -- ]]; then
  shift
  gui_env "$@"
else
  wait "${pids[0]}"
fi
