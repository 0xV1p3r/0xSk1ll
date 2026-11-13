# Desktop GUI

`sk1ll-gui` is a Tauri v2 app for Unix desktops. It talks only to the local daemon over its socket, so the daemon must be running. Without a fleet, it manages a standalone repo. It supports light, dark and system appearance.

## Screens

The header shows this machine's id, its headline status and whether the daemon and server are connected. If the daemon stops, the app shows a reconnecting screen with start instructions. Revoked machines and machines waiting for assignment get a banner. Fleet data refreshes on daemon events and every 15 seconds. Standalone machines hide Fleet and the assignment log. Their assignment belongs to this machine and is saved in `assignment.json`, with no log or undo.

| Screen | What you can do |
|---|---|
| Fleet | See every machine and its status |
| Assignment | Toggle items per harness, grouped by machine, and set the defaults for new skills and rules. Explicit and inherited values look different. |
| Draft | Pick items, read unified diffs, edit the prefilled commit message, push, restore, and pull when Blocked |
| Conflicts | Adopt or overwrite harness edits, and adopt or discard recovered copies |
| Imports | Paste a link to add skills (it previews every skill the repo holds) and update imports. Both land in the draft. |
| Log | Browse assignment changes, newest first, and undo any entry |
| Settings | List harnesses, join or rejoin, create one-time join codes, remove machines (typed confirmation) and see connection details |

In standalone mode, commits stay local. Push and Pull use the repo's own `origin`; without an origin those actions are unavailable. If there is no repo yet, an Init card creates it. The repo comes from `SK1LL_REPO`, the nearest `sk1ll.toml` above the daemon's working directory, or `~/skills`.

Items that aren't in the applied baseline carry an `Unpushed` or `Draft only` badge, and you can only assign them on this machine.

Assignment toggles update the screen before the daemon replies and roll back if it reports an error. Draft actions record the selected items and their content fingerprints, so if a selected item changes before you confirm, the app asks you to review it again. The GUI never retries a mutation on its own.

## Run it

Install Rust, pnpm and the Tauri v2 prerequisites. On Linux that includes GTK3 and WebKitGTK 4.1. Then start the daemon and the app:

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

## Layout

- `crates/sk1ll-gui/src-tauri` is the `sk1ll-gui` workspace member. It holds thin commands that forward to the daemon client and one task that forwards the daemon's server-sent events and reconnects. The Unix gating lives here.
- `crates/sk1ll-gui/ui` is React, TypeScript, Vite, Tailwind and shadcn/ui, built with pnpm. `ui/src/types.ts` is the hand-written copy of the daemon's serde types.
