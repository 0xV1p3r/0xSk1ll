# Running a fleet

A fleet is every machine that shares one skills repo. It has three parts:

- The **server** is a Docker container. It hosts the canonical bare repo, stores assignment and machine status, and tells machines to pull after each push. It is the only git remote ([ADR 0002](adr/0002-server-is-the-git-remote.md)).
- The **daemon** runs on each machine. It watches the repo and the harnesses, applies the draft and assignment, and is the only process that talks to the server. When the server is unreachable, it keeps working from the last assignment it received.
- The **CLI and GUI** talk to the local daemon over a Unix socket.

The daemon and fleet commands need Unix.

## Deploy the server

```sh
docker login ghcr.io                                      # the image is private for now
export SK1LL_SERVER_ADMIN_TOKEN=$(openssl rand -hex 32)   # or put it in deploy/.env
docker compose -f deploy/docker-compose.server.yml up -d
```

Compose pulls `ghcr.io/0xv1p3r/sk1ll-server` at the release in `SK1LL_SERVER_VERSION`, which defaults to the version the file shipped with. Add `--build` to build the image from this checkout instead.

The server listens on port 7878 and keeps `repo.git` and `server.db` in the `server-data` volume. It serves plain HTTP. The daemon accepts `https://` URLs, so put a TLS proxy in front when machines connect over an untrusted network. `GET /healthz` reports whether it is up. `sk1ll-server --healthcheck` asks it from inside the container and is the image's healthcheck. The image is Alpine with a static binary and git, about 25 MB. It has no ssh client, so the server can't check `ssh` import sources for updates.

| Variable | Default | Meaning |
|---|---|---|
| `SK1LL_SERVER_ADMIN_TOKEN` | none | Bootstrap token that can create join codes and manage the fleet |
| `SK1LL_SERVER_DATA` | `/data` | Directory for `repo.git` and `server.db` |
| `SK1LL_SERVER_BIND` | `0.0.0.0:7878` | Listen address |
| `SK1LL_SERVER_OFFLINE_AFTER` | `2m` | A machine with no connection is Offline after this long |
| `SK1LL_SERVER_JOIN_CODE_TTL` | `24h` | How long a join code works |
| `SK1LL_SERVER_IMPORT_POLL` | `1h` | How often to check imports for newer upstream commits |
| `SK1LL_SERVER_IMPORT_CHECKS` | `on` | `off` stops the server from checking imports at all |
| `SK1LL_SERVER_IMPORT_HOSTS` | unset: any host | Comma-separated hosts the import check may contact, e.g. `github.com,gitlab.com`. Set but empty, it stops the server from starting |
| `SK1LL_SERVER_PING` | `30s` | Ping interval for daemon connections |
| `SK1LL_SERVER_SEND_TIMEOUT` | `10s` | A daemon that takes longer to receive one frame is disconnected |
| `SK1LL_SERVER_PUSH_IDLE` | `30s` | Maximum idle time inside a git request |
| `SK1LL_SERVER_PUSH_MAX` | `10m` | Deadline for one whole git exchange |

Durations take `s`, `m`, `h` or `d`. A bare number is seconds. Other variables set to an empty value count as unset.

### Import checks

The server checks each import's upstream for a newer commit with `git ls-remote`, using only the `https`, `http`, `ssh` and `git` transports. The sources come from the pushed manifest, so any machine that can push decides where the server connects. Two settings limit that:

- `SK1LL_SERVER_IMPORT_CHECKS=off` turns server-side checks off.
- `SK1LL_SERVER_IMPORT_HOSTS` lists the only hosts the check may contact. A host must match an entry exactly, ignoring case. Entries are host names or IP addresses (IPv6 in brackets), with no port, user or wildcard. Write internationalized names either way in the list; in sources they must be in punycode (`xn--...`). The host is read from `scheme://[user@]host[:port]/...` and scp-like `[user@]host:path` sources (`github:owner/repo` means `github.com`). A source whose host can't be read unambiguously isn't checked, and neither is one with a percent escape before its path or an ssh user name other than letters, digits, `.`, `_` and `-`. The list can't be empty: leave the variable unset to allow any host.

While the list is set, the check restricts the host name in the source, not where the connection ends up: DNS for an allowed name, or a proxy set in the server's environment, can still lead elsewhere (see [known limitations](known-limitations.md#import-checks-reach-whatever-the-server-can-reach)). Within that, nothing in the check moves it to another host name:

- It doesn't follow HTTP redirects, so a moved repository fails the check instead of leading to another host.
- `git ls-remote` runs outside any repository, without the server's git configuration (`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`) and with only `PATH`, `HOME`, `LANG`, `LC_ALL`, `TMPDIR`, `SSH_AUTH_SOCK`, the proxy variables (`https_proxy`, `no_proxy` and the like) and the CA variables (`SSL_CERT_FILE`, `SSL_CERT_DIR`, `GIT_SSL_CAINFO`, `GIT_SSL_CAPATH`) from its environment. `url.*.insteadOf` rules, credential helpers and `http.*` settings in the server's git config don't apply.
- ssh runs as `ssh -F /dev/null -o BatchMode=yes`, so no ssh config file applies: no `Host` aliases, `HostName`, `Port`, `IdentityFile`, `ProxyJump` or `ProxyCommand`. The default keys (`~/.ssh/id_*`), the agent and `~/.ssh/known_hosts` still work, and a host missing from `known_hosts` fails the check.

If your checks relied on that configuration, write sources with the real host name and port, list that host, and use a default key or the agent; for https, set a proxy or CA with the variables above. Without the list, the check uses the server's git and ssh configuration as before.

The server skips imports these settings exclude and reports each with the reason. The GUI's Imports screen shows them as *Not checked by Server*, and **Check now** checks every import from that machine instead. `sk1ll status` shows only local import state, so it isn't affected. Neither setting restricts other outbound traffic: a firewall is still the way to keep the server out of its own network.

## Join a machine

The first machine joins with the admin token, which creates a single-use join code and spends it right away. The token isn't stored. In `sk1ll setup`, choose *Join as the first machine*. In the GUI's setup wizard, choose *Start a new fleet*.

```sh
SK1LL_SERVER_ADMIN_TOKEN=... sk1ll join http://server:7878   # or --admin-token <token>
```

To create a code by hand instead:

```sh
curl -fsS -X POST -H "Authorization: Bearer $SK1LL_SERVER_ADMIN_TOKEN" \
  -d '{"scope": "self"}' http://server:7878/api/join-codes \
  | jq -er 'if .scope == "self" then .code else error("the server did not grant self scope") end'
```

The response includes the `code`, its `scope` and `expires_at` in Unix seconds. An empty request body creates a `fleet` code.

On a joined machine with a running daemon, run `sk1ll fleet invite` to create a single-use join code. It prints the code, a ready-to-paste join command for the new machine, when the code expires, and its scope. You can also create a code from the GUI's Fleet or Settings screen.

```sh
sk1ll fleet invite                 # the new machine may manage the whole fleet
sk1ll fleet invite --scope self    # the new machine may manage only itself
```

### Scopes

A join code decides what the machine that joins with it may manage ([ADR 0005](adr/0005-join-code-scopes.md)):

| Scope | May |
|---|---|
| `fleet` (default) | Create join codes, change any machine's assignment, undo any log entry, remove any machine |
| `self` | Change its own assignment, undo log entries for itself, remove itself |

Every machine can read the fleet view, the log and every assignment. The admin token can do everything. A `self` machine that tries more gets `403` with the reason, and since it can't create join codes, it can't create a `fleet` code either. Scopes cover fleet administration only: a `self` machine can still push content to the baseline.

Every write checks the caller's token and scope again inside its transaction. Removing a machine waits for a push in progress. A push queued behind a removal is refused with `401`. The server takes pushes only to the Baseline branch, selected from HEAD at startup, and ignores replacement objects. If HEAD is detached, point it to the branch with `git -C <data>/repo.git symbolic-ref HEAD refs/heads/<branch>` and restart the server. `sk1ll fleet` marks `self` Machines as *manages only itself*. A Machine removed and joined again gets the scope of its new code.

On the new machine:

```sh
sk1ll join http://server:7878 <code>                  # clones the repo into ~/.local/share/sk1ll/repo
sk1ll join http://server:7878 <code> --copy-from laptop   # start from laptop's assignment
```

A new machine starts with everything off unless you pass `--copy-from`. `--id` sets the machine id, which otherwise comes from `machine.toml` or the hostname. The daemon stores the machine token in `daemon.json` with mode 0600. On Linux the file is under `$XDG_CONFIG_HOME/sk1ll`, which defaults to `~/.config/sk1ll`. On macOS it is under `~/Library/Application Support/sk1ll`. The server can revoke each machine’s token.

## Copy an Assignment after joining

Run `sk1ll fleet copy-assignment laptop workstation` to preview the changes and confirm them. Use `--yes` in scripts. The command needs a running, joined daemon. A Machine with `self` scope can copy to its own Assignment.

Matching Harness names are paired by default. To choose the pairs explicitly, repeat `--map SOURCE=TARGET` for each pair to copy. Replace copies the source defaults and explicit toggles, clearing target-only toggles in those Harnesses. Use `--merge` to keep the target defaults and fill only unset item toggles. As at join time, explicit toggles carry over even if the target has yet to see the item.

If either Assignment changes or either Machine rejoins after preview, preview again before applying. Each change appears in the Assignment log and can be undone there.

## Remove a machine

Run `sk1ll fleet` to find the machine id. Run `sk1ll fleet remove <machine>` and type that id at the confirmation prompt. In a script, pass `--yes` to confirm removal. For an id that starts with `-`, put `--` before it: `sk1ll fleet remove --yes -- -lab`. The daemon must be running. Removal revokes the machine's credential.

Use `sk1ll assign <harness> --all on|off` to set both defaults and every explicit toggle for a Harness, including its toggles for items the Machine has yet to pull.

Use `sk1ll fleet log` to see recent Assignment changes, newest first. Filter with `--machine`, `--harness`, `--author`, or `--item`. Use `--limit` to set the page size and `--before <entry-id>` to page back. To undo a change, run `sk1ll fleet undo <entry-id>`.

## Orphan toggles

Assignment uses Item IDs. Renames keep their toggles on every Machine. A push or Restore changes only the repo content. Toggles for absent IDs are inert.

Run `sk1ll fleet orphans` to list toggles whose Item ID appears in neither the Baseline nor any Machine's last report. `sk1ll fleet prune-orphans` clears the listed orphans in your scope and records each removal in the Assignment log. A `self` Machine can clear only its own toggles. A `fleet` Machine or the admin can clear across the Fleet, or pass `--machine <id>` to select one Machine.

The Server refuses to list or clear orphans while the Baseline or any reported inventory is incomplete. Fix the affected Item ID metadata and let the Machine report again. The Server also refuses pruning while git push state is degraded.

The server exposes `GET /api/assignment/orphans` and `POST /api/assignment/orphans/prune`. The optional prune body is `{"machine":"workstation"}`.

## Run the daemon

The daemon also runs without a fleet. When `daemon.json` is absent, it uses `SK1LL_REPO`, the nearest skills repo above its working directory, or `~/.local/share/sk1ll/repo`. Assignment is local in `~/.cache/sk1ll/assignment.json`. It watches the repo, assignment and harnesses, and reconciles them periodically. Commits stay local; Push and Pull use the repo's own `origin`. No origin means no Push or Pull.

If the repo does not exist, the daemon still starts and the GUI offers Init. Fleet operations and the assignment log are unavailable until joining. `sk1ll join` can go through a running standalone daemon and restart it with the fleet credentials. A standalone draft or local commits must be moved aside before joining. An untouched `sk1ll init` directory is kept in a `sk1ll-before-join-*` backup beside the repo when the prepared fleet clone is activated. The replacement clone is locked before it becomes live. Snapshot and assignment files are prepared first; `daemon.json` is the final activation marker. If activation fails, the original repo and its engine lock stay active, and any replacement repo is preserved as `unactivated-repo` inside the backup directory. Earlier snapshot and assignment files are retained in private `daemon-activation-*` directories beside those files. Rollback uses no-replace renames and refuses to overwrite concurrent content; if restoration is obstructed, the error names the retained backup and mutations are frozen until recovery and a daemon restart.

A refusal **after registration** consumes the single-use join code and reserves the machine id on the server. Credentials remain in a private `daemon-pending-join-*.json` file next to `daemon.json`, without activating joined mode. Correct the local problem, then repeat the original join command or POST `/join` request with the same server URL, machine id and repo. The daemon reuses those pending credentials, so the consumed code can be supplied again and no new code is needed. Keep that pending file private. If you abandon the registration, remove the reserved machine on the server before registering that id again. If the pending file is lost, remove the reserved machine and obtain a new join code.

On Linux, install the systemd user unit:

```sh
cp deploy/sk1ll-daemon.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now sk1ll-daemon
loginctl enable-linger "$USER"     # optional: keep it running while logged out
journalctl --user -u sk1ll-daemon  # logs; set RUST_LOG=debug for more
```

The unit runs `~/.local/bin/sk1ll daemon`, where the release archive installs it. Edit `ExecStart` for `~/.cargo/bin/sk1ll` after `cargo install`, or `/usr/bin/sk1ll` from the `.deb`. `sk1ll setup` and the app write the unit with the path of the `sk1ll` they found, so they need no edit.

On macOS, run `sk1ll setup` or start the daemon in the app while logged in to the desktop. Either creates `~/Library/LaunchAgents/app.v1p3r.sk1ll.daemon.plist` if needed, loads it into your GUI login session and starts it. If you move the app, starting the daemon again updates the binary path in a generated plist and reloads the agent. If you customized the plist, follow the startup error to update it by hand. The shipped agent starts at login, keeps the daemon running and logs to `~/Library/Logs/sk1ll/daemon.log`. If you install the plist by hand, create the log directory and set `ProgramArguments` to the absolute path of your `sk1ll` binary followed by `daemon`.

```sh
mkdir -p ~/Library/Logs/sk1ll
launchctl enable "gui/$(id -u)/app.v1p3r.sk1ll.daemon" &&
  (launchctl print "gui/$(id -u)/app.v1p3r.sk1ll.daemon" >/dev/null 2>&1 ||
    launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/app.v1p3r.sk1ll.daemon.plist) &&
  launchctl kickstart "gui/$(id -u)/app.v1p3r.sk1ll.daemon"
tail -n 50 ~/Library/Logs/sk1ll/daemon.log
```

The LaunchAgent has its own `PATH`. When setup or the app creates the plist, it looks up `git` on its current `PATH` and prepends Git's directory if the agent's default list lacks it. To change the agent's search path, edit `EnvironmentVariables` → `PATH` in the plist. Run `launchctl bootout "gui/$(id -u)/app.v1p3r.sk1ll.daemon"` if the agent is loaded, then run the start commands above to load the change.

While it runs, the daemon holds the repo's engine lock. CLI commands that change the repo go through the daemon, and the daemon refuses to start while another sk1ll process holds the lock. Running `sk1ll join` again after the machine was removed also goes through the daemon, which then restarts itself with the new credentials.

On Linux, the local API socket is `$XDG_RUNTIME_DIR/sk1ll/daemon.sock`, with a fallback to `~/.config/sk1ll/run/daemon.sock` when there is no runtime directory. On macOS it is `~/Library/Application Support/sk1ll/run/daemon.sock`. The socket has mode 0600 and its directory must belong to the user and have mode 0700. On macOS the socket path can be at most 103 bytes. If it is too long, set `SK1LL_DAEMON_SOCKET` to a shorter path in a private directory for the daemon and its clients.

The table lists Linux paths. On macOS, config and data use `~/Library/Application Support/sk1ll/` and cache uses `~/Library/Caches/sk1ll/`. This also applies to the repo, machine token and assignment paths above.

| Variable | Default | Meaning |
|---|---|---|
| `SK1LL_DAEMON_STATE` | Platform config directory’s `sk1ll/daemon.json` | Machine id, server URL, token and repo path |
| `SK1LL_DAEMON_SOCKET` | `$XDG_RUNTIME_DIR/sk1ll/daemon.sock` | Local API socket |
| `SK1LL_MACHINE_CONFIG` | `~/.config/sk1ll/machine.toml` | Harness configuration |
| `SK1LL_ASSIGNMENT` | `~/.cache/sk1ll/assignment.json` | Local assignment in standalone; server mirror when joined |
| `SK1LL_REPO` | nearest skills repo, else `~/.local/share/sk1ll/repo` | Standalone repo path; joined machines use `daemon.json` |
| `SK1LL_PULL_INTERVAL` | `5m` | Fallback pull interval while connected |
| `SK1LL_DEBOUNCE` | `300ms` | Quiet time before reacting to file changes |
| `SK1LL_DEBOUNCE_MAX` | `2s` | Longest a stream of file changes can postpone a sync |
| `SK1LL_RECONCILE_INTERVAL` | `60s` | Local re-sync of the harnesses, even while offline |
| `SK1LL_AUTH_WARN_AFTER` | `24h` | After the server rejects the token this long, status reports the machine as probably removed |
| `SK1LL_NOTIFICATIONS` | unset | Set to `0` to disable [desktop notifications](configuration.md#desktop-notifications) |
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

Join codes expire after `SK1LL_SERVER_JOIN_CODE_TTL` (default `24h`), using the server duration syntax. `sk1ll fleet invite` and the GUI show the exact time.

## Upgrade

Upgrade the server and every machine to the same release. The server and the daemons exchange a protocol version on every request. A daemon whose version the server doesn't speak is refused, and its status shows the reason as the last error. It keeps working offline from its cache until it is upgraded. The same check runs between the daemon and the CLI or app: after installing a new `sk1ll`, restart the daemon with `systemctl --user restart sk1ll-daemon`. Until then, commands that go through it say so.

`sk1ll --version`, `sk1ll-server --version`, the server's `/healthz` and the app's Settings → Diagnostics show the release and the commit it was built from. The server upgrades its database in place on start.
