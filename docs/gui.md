# Desktop GUI

`sk1ll-gui` is a Tauri v2 desktop app. Release builds ship for x86_64 Linux and Apple silicon macOS. The macOS build runs in CI and has had no manual testing. It talks only to the local daemon over its socket, so the daemon must be running. Without a fleet, it manages a standalone repo. It supports light, dark and system appearance.

## First run

1. **Start the daemon.** If the daemon isn't running, the app offers to start it. With a systemd user manager, it installs `deploy/sk1ll-daemon.service` with `ExecStart` pointing at the `sk1ll` it found, then enables and starts it. On macOS it installs and starts the per-user launchd agent in your GUI login session. An existing systemd unit is used as it is. On macOS, starting the daemon after moving the app updates the path in a generated plist. Without a user service manager, it runs `sk1ll daemon` in the background until you log out. The background process logs to `~/.cache/sk1ll/daemon.log` on Linux or `~/Library/Caches/sk1ll/daemon.log` on macOS. See [Run the daemon](fleet.md#run-the-daemon) for service logs and commands. It looks for `sk1ll` next to the app, then on `PATH`, then in `~/.local/bin` and `~/.cargo/bin`. With a custom `SK1LL_DAEMON_SOCKET`, it only shows the commands.
2. **Choose how to use this machine.** While the machine has no repo, a setup wizard replaces the app:
   - *Only this machine* creates a standalone repo.
   - *Join a fleet* takes the server URL and a join code from another machine.
   - *Join as the first Machine* takes the URL and admin token of a [deployed Server](fleet.md#deploy-the-server). The daemon uses the token once to create a join code, joins with it, and doesn't store the token.
3. **Choose harnesses.** The wizard lists the detected harnesses and saves the ones you turn off as `enabled = false` in `machine.toml`. This works only before the repo exists, so nothing has been synced or registered with the server yet. Afterwards, edit `machine.toml`.
4. **Pick a starting assignment.** A machine that joins without copying another machine's assignment starts with everything off. The app then offers to turn on everything, including the defaults for items added later, or to open the assignment matrix.

`sk1ll setup` asks the same questions on the command line. It goes through the daemon if one is running, otherwise it works directly, and at the end it offers to install and start the service. Answers come from standard input, so it can be scripted.

After the wizard, the Library shows a getting-started checklist: add a skill or rule, adopt unmanaged skills, commit the draft and, on a fleet, add another machine. It goes away when every step is done or you dismiss it. Choose **Show getting started** in the command palette to bring it back, including on Machines set up before the checklist existed.

## Screens

The header shows this machine's id, its headline status and whether the daemon and server are connected. If the daemon stops, the app shows a reconnecting screen with start instructions. Revoked machines and machines waiting for assignment get a banner. Fleet data refreshes on daemon events and every 15 seconds. Standalone machines hide Fleet and the assignment log. Their assignment belongs to this machine and is saved in `assignment.json`, with no log or undo.

| Screen | What you can do |
|---|---|
| Fleet | See every Machine and its status. Choose a Machine in the command palette to open its details. |
| Library | Browse Personal Skills, Imports, Rules and Unmanaged skills. Create Skills and Rules, adopt Unmanaged skills, and compare variants. |
| Assignment | Toggle items per harness, grouped by machine, and set the defaults for new skills and rules. Explicit and inherited values look different. |
| Draft | Pick items, read unified diffs, edit the prefilled commit message, push, restore, and pull when Blocked |
| Conflicts | Adopt or overwrite harness edits, and adopt or discard recovered copies |
| Imports | Paste a link to add skills (it previews every skill the repo holds) and update imports. Both land in the draft. |
| Log | Browse assignment changes, newest first, and undo any entry |
| Settings | List harnesses, join or rejoin (with a join code, or the admin token for a new fleet), create single-use join codes, and copy connection and version details with **Copy diagnostics** |

In standalone mode, commits stay local. Push and Pull use the repo's own `origin`; without an origin those actions are unavailable. If there is no repo yet, the setup wizard creates it. The repo comes from `SK1LL_REPO`, the nearest `sk1ll.toml` above the daemon's working directory, or the default repo location, `~/.local/share/sk1ll/repo` on Linux and `~/Library/Application Support/sk1ll/repo` on macOS.

Items that aren't in the applied baseline carry an `Unpushed` or `Draft only` badge, and you can only assign them on this machine.

Assignment toggles update the screen before the daemon replies and roll back if it reports an error. Draft actions record the selected items and their content fingerprints, so if a selected item changes before you confirm, the app asks you to review it again. The GUI never retries a mutation on its own.

In the Library, a deletion affects this Machine's Draft immediately. Other Machines pick up the deletion after you commit and push it and they sync.

In a Fleet, create a join code from Fleet, Settings or the getting-started checklist. Copy the code or the ready-to-paste `sk1ll join <server-url> <code>` command. Before creating it, choose what the new Machine may manage: **Whole Fleet** (create join codes, change any Machine's Assignment, undo any log entry, remove any Machine) or **Only itself** (its own Assignment, log entries for itself, and removing itself). See [Scopes](fleet.md#scopes). The code works once, and the dialog shows exactly when it expires (24 hours by default, set by the Server's [`SK1LL_SERVER_JOIN_CODE_TTL`](fleet.md#machine-status)). The Fleet screen marks Self-only Machines and shows each Machine's scope in its details. A Self-only Machine can't create join codes and only offers Remove for itself.

The Imports screen marks Imports the Server didn't check under its [import check settings](fleet.md#import-checks) as *Not checked by Server*, with the reason. **Check now** checks them from this Machine.

When an upgrade leaves the app and Daemon on different local API versions, the connection screen offers **Restart daemon** for a running systemd user service on the default socket. It shows terminal instructions for other setups.

## Run it

Install Rust, pnpm and the Tauri v2 prerequisites. On Linux that includes GTK3 and WebKitGTK 4.1. On macOS install the Xcode Command Line Tools with `xcode-select --install`. The app uses the system WebKit. Then start the daemon (or let the app start it) and the app:

```sh
systemctl --user start sk1ll-daemon    # or: sk1ll daemon
cd crates/sk1ll-gui/ui
pnpm install
pnpm tauri dev
```

Set `SK1LL_DAEMON_SOCKET=/absolute/path/to/daemon.sock` to use a different daemon. Set `SK1LL_GUI_TRACE=1` to log command names and outcomes, without payloads.

To try it against a throwaway fleet, use `scripts/dev-fleet.sh --gui` (see [Development](development.md#isolated-dev-fleet)).

## Build

```sh
cd crates/sk1ll-gui/ui
pnpm build                      # the app embeds ui/dist, so build this first
pnpm tauri build --no-bundle    # produces target/release/sk1ll-gui
```

The release `.deb` adds `src-tauri/tauri.release.conf.json`, which bundles the `sk1ll` CLI next to the app. Copy a release `sk1ll` to `src-tauri/binaries/sk1ll-<target triple>` first:

```sh
cargo build --release -p sk1ll-cli
mkdir -p ../src-tauri/binaries
cp ../../../target/release/sk1ll ../src-tauri/binaries/sk1ll-$(rustc --print host-tuple)
pnpm tauri build --config src-tauri/tauri.release.conf.json   # target/release/bundle/deb/
```

## Layout

- `crates/sk1ll-gui/src-tauri` is the `sk1ll-gui` workspace member. It holds thin commands that forward to the daemon client and one task that forwards the daemon's server-sent events and reconnects. The Unix gating lives here.
- `crates/sk1ll-gui/ui` is React, TypeScript, Vite, Tailwind and shadcn/ui, built with pnpm. `ui/src/types.ts` is the hand-written copy of the daemon's serde types.
