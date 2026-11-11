# Running a fleet

A fleet is every machine that shares one skills repo. It has three parts:

- The **server** is a Docker container. It hosts the canonical bare repo, stores assignment and machine status, and tells machines to pull after each push. It is the only git remote ([ADR 0002](adr/0002-server-is-the-git-remote.md)).
- The **daemon** runs on each machine. It watches the repo and the harnesses, applies the draft and assignment, and is the only process that talks to the server. When the server is unreachable, it keeps working from the last assignment it received.
- The **CLI and GUI** talk to the local daemon over a Unix socket.

The daemon and fleet commands need Unix.

## Deploy the server

```sh
export SK1LL_SERVER_ADMIN_TOKEN=$(openssl rand -hex 32)   # or put it in deploy/.env
docker compose -f deploy/docker-compose.server.yml up -d
```

The server listens on port 7878 and keeps `repo.git` and `server.db` in the `server-data` volume. It serves plain HTTP. The daemon accepts `https://` URLs, so put a TLS proxy in front when machines connect over an untrusted network. `GET /healthz` reports whether it is up.

| Variable | Default | Meaning |
|---|---|---|
| `SK1LL_SERVER_ADMIN_TOKEN` | none | Bootstrap token that can create join codes and manage the fleet |
| `SK1LL_SERVER_DATA` | `/data` | Directory for `repo.git` and `server.db` |
| `SK1LL_SERVER_BIND` | `0.0.0.0:7878` | Listen address |
| `SK1LL_SERVER_OFFLINE_AFTER` | `2m` | A machine with no connection is Offline after this long |
| `SK1LL_SERVER_IMPORT_POLL` | `1h` | How often to check imports for newer upstream commits |
| `SK1LL_SERVER_PING` | `30s` | Ping interval for daemon connections |
| `SK1LL_SERVER_SEND_TIMEOUT` | `10s` | A daemon that takes longer to receive one frame is disconnected |
| `SK1LL_SERVER_PUSH_IDLE` | `30s` | Maximum idle time inside a git request |
| `SK1LL_SERVER_PUSH_MAX` | `10m` | Deadline for one whole git exchange |

Durations take `s`, `m`, `h` or `d`. A bare number is seconds.

## Join a machine

Create a single-use join code with the admin token:

```sh
curl -X POST -H "Authorization: Bearer $SK1LL_SERVER_ADMIN_TOKEN" \
  http://server:7878/api/join-codes
```

Once one machine has joined, any joined machine can create more codes, from the GUI's Settings screen or with its own token.

On the new machine:

```sh
sk1ll join http://server:7878 <code>                  # clones the repo into ~/skills
sk1ll join http://server:7878 <code> --copy-from laptop   # start from laptop's assignment
```

A new machine starts with everything off unless you pass `--copy-from`. `--id` sets the machine id, which otherwise comes from `machine.toml` or the hostname. The machine token is stored only in `~/.config/sk1ll/daemon.json` with mode 0600, and the server can revoke it per machine.

## Run the daemon

On Linux, install the systemd user unit:

```sh
cp deploy/sk1ll-daemon.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now sk1ll-daemon
loginctl enable-linger "$USER"     # optional: keep it running while logged out
journalctl --user -u sk1ll-daemon  # logs; set RUST_LOG=debug for more
```

The unit runs `~/.cargo/bin/sk1ll daemon`. Edit `ExecStart` if you installed `sk1ll` elsewhere. On other systems, run `sk1ll daemon` under your service manager.

While it runs, the daemon holds the repo's engine lock. CLI commands that change the repo go through the daemon, and the daemon refuses to start while another sk1ll process holds the lock. Running `sk1ll join` again after the machine was removed also goes through the daemon, which then restarts itself with the new credentials.

The local API socket is `$XDG_RUNTIME_DIR/sk1ll/daemon.sock`. Its directory must belong to the user and have mode 0700, or the daemon won't start.

| Variable | Default | Meaning |
|---|---|---|
| `SK1LL_DAEMON_STATE` | `~/.config/sk1ll/daemon.json` | Machine id, server URL, token and repo path |
| `SK1LL_DAEMON_SOCKET` | `$XDG_RUNTIME_DIR/sk1ll/daemon.sock` | Local API socket |
| `SK1LL_MACHINE_CONFIG` | `~/.config/sk1ll/machine.toml` | Harness configuration |
| `SK1LL_ASSIGNMENT` | `~/.cache/sk1ll/assignment.json` | Assignment cache |
| `SK1LL_PULL_INTERVAL` | `5m` | Fallback pull interval while connected |
| `SK1LL_DEBOUNCE` | `300ms` | Quiet time before reacting to file changes |
| `SK1LL_DEBOUNCE_MAX` | `2s` | Longest a stream of file changes can postpone a sync |
| `SK1LL_RECONCILE_INTERVAL` | `60s` | Local re-sync of the harnesses, even while offline |
| `SK1LL_AUTH_WARN_AFTER` | `24h` | After the server rejects the token this long, status reports the machine as probably removed |
| `SK1LL_RECONNECT_MAX` | `60s` | Longest reconnect backoff |

## Machine status

`sk1ll fleet` shows every machine with one headline status. A machine gets the most severe one that applies, in this order:

| Status | Meaning |
|---|---|
| Offline | The server can't reach the machine. Shows a last-seen time. |
| Blocked | A new baseline clashes with the machine's draft, so it stays on the old baseline. |
| Conflict | A harness holds content that sk1ll won't overwrite on its own. |
| Behind | The machine hasn't picked up the latest baseline yet. |
| Drafting | The machine has a draft. |
| In sync | Latest baseline, no draft, no conflicts. |

To clear Blocked, commit or restore the clashing items, then run `sk1ll pull`.
